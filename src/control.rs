//! The control API. FXServer's `server/voice.lua` posts here.
//!
//! No audio crosses this link - it carries routing decisions only.
//!
//! HTTP/1.1 is hand-rolled rather than pulled in as a dependency. We control
//! both ends, the requests are small JSON POSTs from a known client, and the
//! alternative is twenty crates and a larger container for a handful of
//! endpoints. If this ever needs to face anything but FXServer, replace it.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::router::{ClientId, Router};
use crate::servers::KeyIndex;
use crate::stream::SharedStreams;

pub type Shared = Arc<Mutex<Router>>;
/// Rebuilt on enrolment changes, so a revoked key stops working without a
/// restart.
pub type SharedKeys = Arc<Mutex<KeyIndex>>;

/// Consoles live on server 0.
///
/// Enrolled servers are numbered from 1 - the local store's next_id starts
/// there and so does the platform's autoincrement - so 0 can never collide
/// with a real world. Past that the router does not need to know a console is
/// not a radio: it is a member of talkgroups, it listens, and it can key.
pub const CONSOLE_SERVER: u32 = 0;

static NEXT_CONSOLE: AtomicU32 = AtomicU32::new(1);

pub async fn serve(
    addr: String,
    router: Shared,
    streams: SharedStreams,
    keys: SharedKeys,
    node_key: Option<String>,
    recorder: crate::recorder::SharedRecorder,
) -> Result<()> {
    let listener = TcpListener::bind(&addr).await?;
    println!("control api listening on {addr}");

    loop {
        let (socket, _) = listener.accept().await?;
        let router = router.clone();
        let streams = streams.clone();
        let keys = keys.clone();
        let node_key = node_key.clone();
        let recorder = recorder.clone();
        tokio::spawn(async move {
            if let Err(e) = handle(socket, router, streams, keys, node_key, recorder).await {
                eprintln!("control: {e}");
            }
        });
    }
}

async fn handle(
    mut socket: TcpStream,
    router: Shared,
    streams: SharedStreams,
    keys: SharedKeys,
    node_key: Option<String>,
    recorder: crate::recorder::SharedRecorder,
) -> Result<()> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];

    // Headers first.
    let head_end = loop {
        let n = socket.read(&mut chunk).await?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = find(&buf, b"\r\n\r\n") {
            break pos + 4;
        }
        if buf.len() > 64 * 1024 {
            return respond(&mut socket, 431, &json!({ "error": "headers too large" })).await;
        }
    };

    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let mut lines = head.lines();
    let request_line = lines.next().unwrap_or_default().to_string();

    let mut headers = HashMap::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }

    let want: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    let mut body = buf[head_end..].to_vec();
    while body.len() < want {
        let n = socket.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }

    let mut parts = request_line.split_whitespace();
    let _method = parts.next().unwrap_or("");
    // The LAST path segment is the message name, not the whole path.
    //
    // Message names are single words - route, unkey, resync - so anything to
    // the left of the final slash is a proxy's doing. Cloudflare Tunnel path
    // rules forward the prefix rather than stripping it (unlike Caddy's
    // handle_path), so a node behind one at /control/* would otherwise see
    // "control/route" and match nothing. Being indifferent to the prefix means
    // the node works behind any proxy layout without a config option for it.
    let path = parts
        .next()
        .unwrap_or("/")
        .split('?')
        .next()
        .unwrap_or("")
        .rsplit('/')
        .next()
        .unwrap_or("")
        .to_string();

    let payload: Value = if body.is_empty() {
        json!({})
    } else {
        serde_json::from_slice(&body).unwrap_or_else(|_| json!({}))
    };

    // AUTHENTICATE FIRST, and derive the server id from the key rather than
    // believing whatever the caller claims. A server does not get to say which
    // server it is - otherwise anything that can reach this port could post
    // routing for another world.
    let presented = headers
        .get("x-rfx-key")
        .cloned()
        .or_else(|| {
            payload
                .get("key")
                .and_then(|v| v.as_str())
                .map(String::from)
        })
        .unwrap_or_default();

    // The CONSOLE plane, authenticated with the node key rather than a server
    // key. A dispatch console is not a player on any world, so it cannot
    // present a server key - and it must never hold one, because a server key
    // is authority over that world's routing.
    //
    // The browser never sees this key either: the platform holds it and
    // proxies, handing the console back only a stream token.
    let presented_node = headers.get("x-node-key").cloned().unwrap_or_default();
    if !presented_node.is_empty() {
        let ok = node_key.as_deref().is_some_and(|k| {
            crate::servers::constant_time_eq(k.as_bytes(), presented_node.as_bytes())
        });
        if !ok {
            return respond(
                &mut socket,
                401,
                &json!({ "ok": false, "error": "bad node key" }),
            )
            .await;
        }
        let (code, reply) = dispatch_console(&path, &payload, &router, &streams, &recorder);
        return respond(&mut socket, code, &reply).await;
    }

    let server = keys.lock().ok().and_then(|k| k.resolve(&presented));

    let Some(server) = server else {
        return respond(
            &mut socket,
            401,
            &json!({ "ok": false, "error": "unknown or missing server key" }),
        )
        .await;
    };

    let (code, reply) = dispatch(&path, &payload, server, &router, &streams, &recorder);
    respond(&mut socket, code, &reply).await
}

/// Attach, re-subscribe, detach. Everything a position needs and nothing else.
fn dispatch_console(
    path: &str,
    body: &Value,
    router: &Shared,
    streams: &SharedStreams,
    recorder: &crate::recorder::SharedRecorder,
) -> (u16, Value) {
    let tgs: Vec<u32> = body
        .get("talkgroups")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_u64())
                .map(|x| x as u32)
                .collect()
        })
        .unwrap_or_default();

    // The recorder's index and one call's audio. Node-key authenticated like
    // the rest of the console plane - a browser never reaches this directly,
    // the platform proxies for it.
    //
    // Answered BEFORE the router and streams locks are taken. Neither needs
    // them, and both do blocking disk work: index() walks every retained day
    // directory and parses every sidecar, which on a month of recordings is
    // tens of thousands of files. Holding the global router mutex across that
    // blocks the per-packet destination() lookup in every Mumble tap, so one
    // dispatcher opening instant recall stopped live audio on every enrolled
    // world at once.
    if path == "rec.index" {
        let since = body.get("since").and_then(|v| v.as_u64()).unwrap_or(0);
        let limit = body
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(200)
            .min(1000) as usize;

        let calls = match recorder.lock() {
            Ok(g) => g
                .rec
                .as_ref()
                .map(|x| x.index(since, limit))
                .unwrap_or_default(),
            Err(_) => Vec::new(),
        };
        return (200, json!({ "ok": true, "calls": calls }));
    }

    if path == "rec.audio" {
        let id = body.get("id").and_then(|v| v.as_str()).unwrap_or_default();
        let pcm = match recorder.lock() {
            Ok(g) => g.rec.as_ref().and_then(|x| x.audio(id)),
            Err(_) => None,
        };
        return match pcm {
            // Base64 rather than a binary body: this rides the same small
            // hand-rolled HTTP as the rest of the control API, and a call
            // is a few hundred kilobytes at most.
            Some(bytes) => (
                200,
                json!({ "ok": true, "rate": crate::dsp::RATE, "pcm": b64(&bytes) }),
            ),
            None => (404, json!({ "ok": false, "error": "no such recording" })),
        };
    }

    let mut r = match router.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    let mut s = match streams.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };

    match path {
        // Patches, pushed rather than waited for.
        //
        // The node re-reads the patch list from the platform on its poll, which
        // is sixty seconds by default. That is fine for a scheduled patch and
        // completely wrong for a console one: a dispatcher presses PATCH
        // because something is happening NOW, and a minute of the two channels
        // not crossing - with the board already lit as though they do - is
        // indistinguishable from the feature being broken. It was reported as
        // exactly that.
        //
        // Replaced whole, like the polled path, because a patch that half
        // applied is a channel joined in one direction only.
        "patches.set" => match body.get("groups").and_then(|g| g.as_array()) {
            Some(list) => {
                let groups: Vec<Vec<u32>> = list
                    .iter()
                    .filter_map(|g| g.as_array())
                    .map(|g| {
                        g.iter()
                            .filter_map(|v| v.as_u64().map(|n| n as u32))
                            .collect()
                    })
                    .filter(|g: &Vec<u32>| g.len() > 1)
                    .collect();
                let n = groups.len();
                r.set_patches(groups);
                println!("patches replaced: {n} group(s)");
                (200, json!({ "ok": true, "groups": n }))
            }
            None => (400, json!({ "ok": false, "error": "groups required" })),
        },

        "console.attach" => {
            let id = NEXT_CONSOLE.fetch_add(1, Ordering::Relaxed);
            let client = ClientId::new(CONSOLE_SERVER, id);

            let token = match crate::servers::mint_key() {
                Ok(t) => t,
                Err(_) => return (500, json!({ "ok": false, "error": "no randomness" })),
            };

            // Quality 100: a console is wired into the network, not listening
            // over the air. Degrading it would simulate a radio path that does
            // not exist.
            for tg in &tgs {
                r.set_member(*tg, client, true, 100);
            }
            s.authorize(token.clone(), client);

            println!("console {id} attached to {} talkgroup(s)", tgs.len());
            (200, json!({ "ok": true, "client": id, "token": token }))
        }

        "console.update" => match u32_of(body, "client") {
            Some(id) => {
                let client = ClientId::new(CONSOLE_SERVER, id);
                // Forget then re-add rather than diffing. The set is small, and
                // a diff that drifts leaves a position hearing a talkgroup it
                // took off its board.
                //
                // MEMBERSHIP only. This was forget_client, which also releases
                // the key - so a console that re-subscribed while transmitting
                // revoked its own grant and everything it said afterwards was
                // dropped. Re-subscribing is a board change, not an unkey.
                r.forget_membership(client);
                for tg in &tgs {
                    r.set_member(*tg, client, true, 100);
                }
                (200, json!({ "ok": true, "talkgroups": tgs.len() }))
            }
            None => (400, json!({ "ok": false, "error": "client required" })),
        },

        "console.detach" => match u32_of(body, "client") {
            Some(id) => {
                let client = ClientId::new(CONSOLE_SERVER, id);
                r.forget_client(client);
                s.revoke_player(client);
                println!("console {id} detached");
                (200, json!({ "ok": true }))
            }
            None => (400, json!({ "ok": false, "error": "client required" })),
        },

        other => (
            404,
            json!({ "ok": false, "error": format!("unknown message {other}") }),
        ),
    }
}

/// Push a call event to every CONSOLE listening to a talkgroup.
///
/// Consoles only. Game clients learn about calls from their own server, which
/// already knows - sending it twice would mean two sources of truth for the
/// same fact, disagreeing whenever one is late.
fn announce_call(r: &Router, streams: &SharedStreams, tg: u32, keyed: bool, talker: &str) {
    // ACROSS THE PATCH GROUP, and announced on the route each console is
    // actually watching.
    //
    // A patch joins talkgroups, so audio keyed on one is heard on all of them -
    // that part worked. The INDICATION did not: this announced on one route
    // only, so a dispatcher monitoring the other side of a patch heard the
    // traffic while their module stayed dark, and a module that is silent about
    // a call it is passing is worse than one that shows nothing at all.
    //
    // Each console is told the route IT holds, not the route that was keyed.
    // A console never subscribed to the far side, and a call announced on a
    // talkgroup that is not on its board matches no module and is dropped.
    let group = r.joined(tg);

    let mut s = match streams.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };

    let mut told: Vec<ClientId> = Vec::new();
    for member in group {
        for c in r.listeners_of(member) {
            if c.server != CONSOLE_SERVER || told.contains(&c) {
                continue;
            }
            told.push(c);
            s.send_call(c, member, keyed, talker);
        }
    }
}

/// Base64. A dependency for eighteen lines would be the wrong trade.
fn b64(data: &[u8]) -> String {
    const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = u32::from(b[0]) << 16 | u32::from(b[1]) << 8 | u32::from(b[2]);
        out.push(A[(n >> 18 & 63) as usize] as char);
        out.push(A[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            A[(n >> 6 & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            A[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

fn u32_of(v: &Value, key: &str) -> Option<u32> {
    v.get(key).and_then(|x| x.as_u64()).map(|x| x as u32)
}

/// Player ids are unique only within one FXServer, so a client is always the
/// pair - and the server half comes from the AUTHENTICATED key, never from the
/// request body.
fn client_of(server: u32, v: &Value) -> Option<ClientId> {
    Some(ClientId::new(server, u32_of(v, "client")?))
}

fn dispatch(
    path: &str,
    body: &Value,
    server: u32,
    router: &Shared,
    streams: &SharedStreams,
    recorder: &crate::recorder::SharedRecorder,
) -> (u16, Value) {
    let mut r = match router.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };

    match path {
        // A frequency rather than a talkgroup. Same routing table - the id
        // space is disjoint, so they cannot collide - but different rules
        // about what keying means.
        "conv.open" => match u32_of(body, "tg") {
            Some(id) => {
                // Airband. The FXServer says so, because it is the thing
                // holding the codeplug that knows this channel is AM.
                let am = body.get("am").and_then(|v| v.as_bool()).unwrap_or(false);
                let fresh = r.open_conventional(id, am);
                if fresh {
                    println!("CONV OPEN     {id}{}", if am { " AM" } else { "" });
                }
                (200, json!({ "ok": true, "fresh": fresh }))
            }
            None => (400, json!({ "ok": false, "error": "tg required" })),
        },

        "tg.open" => match u32_of(body, "tg") {
            Some(tg) => {
                let encrypted = body
                    .get("encrypted")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let fresh = r.open(tg, encrypted);
                if fresh {
                    println!(
                        "route open   tg {tg}{}",
                        if encrypted { " (encrypted)" } else { "" }
                    );
                }
                (200, json!({ "ok": true, "fresh": fresh }))
            }
            None => (400, json!({ "ok": false, "error": "tg required" })),
        },

        "tg.close" => match u32_of(body, "tg") {
            Some(tg) => {
                if r.close(tg) {
                    println!("route close  tg {tg}");
                }
                (200, json!({ "ok": true }))
            }
            None => (400, json!({ "ok": false, "error": "tg required" })),
        },

        // Many member updates in one message.
        //
        // Link quality now moves as fast as people walk - the game models it
        // from a coverage grid rather than assuming full quieting - so a busy
        // server produced a control request per radio per step. Three hundred
        // people walking is six hundred HTTP requests a second, each carrying
        // about forty bytes of actual news, and the cost is almost entirely in
        // the requests rather than the work.
        //
        // Coalesced on the sending side and applied here in one pass. The
        // resource already batches its state reports the same way and for the
        // same reason.
        "tg.members" => match body.get("members").and_then(|m| m.as_array()) {
            Some(list) => {
                let mut applied = 0usize;
                for m in list {
                    let (Some(tg), Some(client)) = (u32_of(m, "tg"), client_of(server, m)) else {
                        continue;
                    };
                    let listen = m.get("listen").and_then(|v| v.as_bool()).unwrap_or(false);
                    let quality = u32_of(m, "quality").unwrap_or(100).min(100) as u8;
                    r.set_member(tg, client, listen, quality);
                    applied += 1;
                }
                (200, json!({ "ok": true, "applied": applied }))
            }
            None => (400, json!({ "ok": false, "error": "members required" })),
        },

        "tg.member" => match (u32_of(body, "tg"), client_of(server, body)) {
            (Some(tg), Some(client)) => {
                let listen = body
                    .get("listen")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                // 0..100, where 100 is full quieting.
                let quality = u32_of(body, "quality").unwrap_or(100).min(100) as u8;
                r.set_member(tg, client, listen, quality);
                (200, json!({ "ok": true }))
            }
            _ => (
                400,
                json!({ "ok": false, "error": "tg, server and client required" }),
            ),
        },

        "key" => match (u32_of(body, "tg"), client_of(server, body)) {
            (Some(tg), Some(client)) => match r.key(tg, client) {
                Ok(crate::router::Keyed::Mixed) => {
                    // AM. Accepted AND heard, on top of whoever was already
                    // there. No announcement and no recording: the call that
                    // is open on this frequency is still theirs, and this
                    // audio joins it rather than starting another.
                    println!("MIXED        tg {tg} <- {client}");
                    (200, json!({ "ok": true, "mixed": true }))
                }

                Ok(crate::router::Keyed::Doubled) => {
                    // Somebody else already holds this frequency. Accepted -
                    // conventional has nothing to refuse with - but not heard,
                    // and not announced as a call because it is not one.
                    println!("DOUBLED      tg {tg} <- {client}");
                    (200, json!({ "ok": true, "doubled": true }))
                }

                Ok(_) => {
                    // Tell consoles the moment the grant lands, before any
                    // audio exists. A console is a supervisory position: it
                    // needs to see that a channel is taken, not wait to hear
                    // it, and every gap between words would otherwise read as
                    // the channel going clear.
                    let unit = body
                        .get("unit")
                        .map(|v| match v {
                            Value::String(s) => s.clone(),
                            other => other.to_string(),
                        })
                        .unwrap_or_default();
                    announce_call(&r, streams, tg, true, &unit);

                    if let Ok(mut rec) = recorder.lock() {
                        rec.begin(tg, server, client.player, &unit, crate::dsp::RATE);
                    }

                    let bound = r.session_of(client).is_some();
                    println!(
                        "KEY          tg {tg} <- {client}{}",
                        if bound {
                            ""
                        } else {
                            "  (no mumble session bound yet)"
                        }
                    );
                    (200, json!({ "ok": true, "bound": bound }))
                }
                Err(e) => {
                    // On a platform this usually means ANOTHER SERVER got there
                    // first, not that the node restarted. The losing server
                    // turns it into a bonk.
                    println!("KEY REFUSED  tg {tg} <- {client}: {e}");
                    (409, json!({ "ok": false, "error": e }))
                }
            },
            _ => (
                400,
                json!({ "ok": false, "error": "tg, server and client required" }),
            ),
        },

        "unkey" => match u32_of(body, "tg") {
            Some(tg) => {
                // Scoped to the caller when it names one: dispatch can preempt a
                // unit, and the preempted unit still sends its own unkey.
                match client_of(server, body) {
                    Some(c) => {
                        r.unkey_as(tg, c);
                    }
                    None => r.unkey(tg),
                }
                // AFTER the release, and only once the channel has actually
                // gone clear. Announcing first ended the call for a release
                // that released nothing: a preempted unit letting go darkened
                // every console module while dispatch was mid-sentence and
                // closed dispatch's recording, after which the rest of it was
                // dropped for having no open entry. The AM case contradicted
                // the key path outright - a Mixed talker deliberately does not
                // start a call, and then ended the first talker's.
                //
                // Not conditioned on what unkey_as returns: when the router
                // lost the key first - a Mumble drop, an unkey after a node
                // restart - nothing was released and nobody is talking, and the
                // call-end still has to go out or the module stays lit.
                //
                // Two different questions, deliberately. A recording is keyed
                // by ROUTE and ends when that route goes quiet. The indication
                // covers the whole patch group, like the announcement itself,
                // because a patch made mid-incident can leave two people
                // holding two members of one channel - and a module must stay
                // lit while either of them is still talking.
                if r.talkers_on(tg).is_empty() {
                    if let Ok(mut rec) = recorder.lock() {
                        rec.end(tg);
                    }
                }
                if r.is_clear(tg) {
                    announce_call(&r, streams, tg, false, "");
                }
                println!("UNKEY        tg {tg}");
                (200, json!({ "ok": true }))
            }
            None => (400, json!({ "ok": false, "error": "tg required" })),
        },

        // FXServer mints a token per player and passes the same value to that
        // player's NUI. Without this the audio socket would be an open scanner
        // feed of every talkgroup on the system.
        "session.token" => match (
            client_of(server, body),
            body.get("token").and_then(|v| v.as_str()),
        ) {
            (Some(client), Some(token)) if !token.is_empty() => {
                if let Ok(mut s) = streams.lock() {
                    s.authorize(token.to_string(), client);
                }
                println!("token issued for {client}");
                (200, json!({ "ok": true }))
            }
            _ => (
                400,
                json!({ "ok": false, "error": "server, client and non-empty token required" }),
            ),
        },

        "session.drop" => match client_of(server, body) {
            Some(client) => {
                if let Ok(mut s) = streams.lock() {
                    s.revoke_player(client);
                }
                (200, json!({ "ok": true }))
            }
            None => (
                400,
                json!({ "ok": false, "error": "server and client required" }),
            ),
        },

        "stats" => {
            let connected = streams.lock().map(|s| s.connected()).unwrap_or(0);
            (
                200,
                json!({
                    "ok": true,
                    "server": server,
                    "summary": r.summary(),
                    "streams": connected,
                }),
            )
        }

        other => (
            404,
            json!({ "ok": false, "error": format!("unknown endpoint {other}") }),
        ),
    }
}

async fn respond(socket: &mut TcpStream, code: u16, body: &Value) -> Result<()> {
    let text = body.to_string();
    let reason = match code {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        409 => "Conflict",
        431 => "Request Header Fields Too Large",
        _ => "Error",
    };
    let head = format!(
        "HTTP/1.1 {code} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        text.len()
    );
    socket.write_all(head.as_bytes()).await?;
    socket.write_all(text.as_bytes()).await?;
    socket.flush().await?;
    Ok(())
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::{Streams, KIND_CALL};

    const FIELD: u32 = 1;

    struct Node {
        router: Shared,
        streams: SharedStreams,
        recorder: crate::recorder::SharedRecorder,
    }

    fn node() -> Node {
        Node {
            router: Arc::new(Mutex::new(Router::default())),
            streams: Arc::new(Mutex::new(Streams::default())),
            recorder: crate::recorder::SharedRecorder::default(),
        }
    }

    /// Whether a call frame said the channel was keyed, in the order they
    /// arrived.
    fn calls(rx: &mut tokio::sync::mpsc::Receiver<Vec<u8>>) -> Vec<bool> {
        let mut out = Vec::new();
        while let Ok(f) = rx.try_recv() {
            if f[0] == KIND_CALL {
                out.push(f[1] == 1);
            }
        }
        out
    }

    /// The preempted unit still sends its own unkey when its operator lets go,
    /// and that must not end the transmission that took the channel off them.
    ///
    /// This used to announce the call ended and finalise the recording BEFORE
    /// asking the router whether anything was actually released - so every
    /// console module went dark mid-sentence, and the rest of what dispatch
    /// said was dropped for having no open recording to go into.
    #[test]
    fn a_preempted_units_unkey_does_not_end_dispatch() {
        let n = node();
        let unit = ClientId::new(FIELD, 7);
        let console = ClientId::new(CONSOLE_SERVER, 1);

        let mut rx = {
            let mut r = n.router.lock().unwrap();
            r.open(1001, false);
            r.set_member(1001, unit, true, 100);
            r.set_member(1001, console, true, 100);
            r.key(1001, unit).unwrap();
            // Refused, then insisted on: preemption takes two presses.
            assert!(r.key(1001, console).is_err());
            r.key(1001, console).unwrap();
            n.streams.lock().unwrap().test_sink(console)
        };

        let (code, _) = dispatch(
            "unkey",
            &json!({ "tg": 1001, "client": 7 }),
            FIELD,
            &n.router,
            &n.streams,
            &n.recorder,
        );

        assert_eq!(code, 200);
        assert!(
            calls(&mut rx).is_empty(),
            "the unit that was cut off ended dispatch's call by letting go"
        );
        assert_eq!(
            n.router
                .lock()
                .unwrap()
                .destination_of(console)
                .map(|(t, _)| t),
            Some(1001),
            "dispatch is still transmitting"
        );
    }

    /// And the ordinary case still works, or the fix above would simply have
    /// left every module lit forever.
    #[test]
    fn an_ordinary_unkey_still_ends_the_call() {
        let n = node();
        let unit = ClientId::new(FIELD, 7);
        let console = ClientId::new(CONSOLE_SERVER, 1);

        let mut rx = {
            let mut r = n.router.lock().unwrap();
            r.open(1001, false);
            r.set_member(1001, unit, true, 100);
            r.set_member(1001, console, true, 100);
            n.streams.lock().unwrap().test_sink(console)
        };

        let body = json!({ "tg": 1001, "client": 7, "unit": "ENG 1" });
        dispatch("key", &body, FIELD, &n.router, &n.streams, &n.recorder);
        dispatch("unkey", &body, FIELD, &n.router, &n.streams, &n.recorder);

        assert_eq!(calls(&mut rx), vec![true, false]);
    }

    /// Instant recall walks the whole recording store, and it used to do that
    /// with the global router mutex held - so a read-only query from one
    /// dispatcher stalled routing for every player on every enrolled world.
    #[test]
    fn a_recall_query_does_not_wait_for_the_router() {
        let n = node();
        let held = n.router.lock().unwrap();

        let (router, streams, recorder) = (n.router.clone(), n.streams.clone(), n.recorder.clone());
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let answered = dispatch_console(
                "rec.index",
                &json!({ "since": 0, "limit": 10 }),
                &router,
                &streams,
                &recorder,
            );
            let _ = tx.send(answered.0);
        });

        // Generous: the point is that it never waits at all, not that it is
        // fast.
        let code = rx.recv_timeout(std::time::Duration::from_secs(5));
        assert_eq!(
            code.ok(),
            Some(200),
            "the recorder index blocked on the router lock"
        );

        drop(held);
        worker.join().unwrap();
    }
}
