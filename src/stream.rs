//! Audio delivery to players.
//!
//! Each player's NUI page opens one WebSocket here and receives 8 kHz PCM for
//! whatever their radio is entitled to hear. This is the only path by which
//! radio audio reaches anyone.
//!
//! **Authentication is not optional.** This socket is reachable by anything
//! that can route to the port, so without a token it is an open scanner feed
//! of every talkgroup on the system. FXServer mints a token per player, posts
//! it to `/session.token`, and passes the same token to that player's NUI.
//! An unauthenticated or unknown token is closed immediately.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::tungstenite::Message;

use crate::control::Shared;
use crate::router::ClientId;

/// Wire format, first byte is the frame kind.
///
///   0 hello  [0][u32 sample rate BE]
///   1 audio  [1][reserved][u32 talkgroup BE][i16 LE PCM ...]
///
/// The talkgroup is a u32 because a CONVENTIONAL route id is one: it is
/// `0x80000000 | frequency`, which is how a frequency and a talkgroup share an
/// id space without colliding. It used to be written as a u16, which truncated
/// every conventional channel to a number matching nothing - so conventional
/// audio reached consoles addressed to a route that did not exist, and was
/// dropped. Trunked ids fit in 16 bits, which is why only conventional broke.
///
/// The header is 6 bytes and that keeps the PCM 2-byte aligned. Alignment is
/// not a nicety: typed array views require their offset to be a multiple of
/// the element size, so an odd header makes `new Int16Array(buf, n)` throw
/// rather than read slowly.
pub const KIND_HELLO: u8 = 0;
pub const KIND_AUDIO: u8 = 1;
/// A call started or ended on a talkgroup.
///
/// Sent when the GRANT is decided, not when audio arrives. A radio is busy
/// from the instant its transmission is approved; a console that waits for
/// the first samples shows the channel clear while somebody is already
/// talking, and clear again in every gap between words.
pub const KIND_CALL: u8 = 2;
/// A transmit request was refused, and why. `[3][0][u32 tg][utf8 reason]`
pub const KIND_DENY: u8 = 3;

// Inbound, from a console. Numbered well clear of the outbound kinds so a
// frame going the wrong way is obviously wrong rather than subtly valid.
const TX_KEY: u8 = 16;
const TX_UNKEY: u8 = 17;
const TX_AUDIO: u8 = 18;

/// Where the PCM starts in an audio frame. Even, so the PCM stays alignable.
pub const AUDIO_HEADER: usize = 6;

/// The consoles monitoring a talkgroup.
///
/// Takes an already-locked router deliberately. Locking it here would mean
/// callers holding the streams lock could acquire the two in the opposite
/// order from every other path in this crate, which is a deadlock waiting for
/// a busy night.
fn consoles_on(r: &crate::router::Router, tg: u32) -> Vec<crate::router::ClientId> {
    r.listeners_of(tg)
        .into_iter()
        .filter(|c| c.server == crate::control::CONSOLE_SERVER)
        .collect()
}

#[derive(Default)]
pub struct Streams {
    /// token -> client, minted by FXServer.
    tokens: HashMap<String, ClientId>,
    /// client -> their live socket.
    sinks: HashMap<ClientId, mpsc::Sender<Vec<u8>>>,
}

pub type SharedStreams = Arc<Mutex<Streams>>;

impl Streams {
    pub fn authorize(&mut self, token: String, client: ClientId) {
        self.tokens.insert(token, client);
    }

    pub fn revoke_player(&mut self, client: ClientId) {
        self.tokens.retain(|_, c| *c != client);
        self.sinks.remove(&client);
    }

    /// Everything belonging to one server, for when a tap disconnects. The
    /// other servers on the platform must be unaffected.
    pub fn drop_server(&mut self, server: u32) {
        self.tokens.retain(|_, c| c.server != server);
        self.sinks.retain(|c, _| c.server != server);
    }

    fn client_for(&self, token: &str) -> Option<ClientId> {
        self.tokens.get(token).copied()
    }

    pub fn connected(&self) -> usize {
        self.sinks.len()
    }

    /// Delivers PCM for one talkgroup to one player.
    ///
    /// Drops the frame rather than blocking if that player's socket is
    /// backed up. Late radio audio is worse than missing radio audio - a
    /// stalled listener must never hold up the vocoder for everyone else.
    /// `[2][keyed][u16 tg BE][utf8 talker]`
    ///
    /// The talker is a label rather than an id: whatever the server called the
    /// unit, or empty when it did not say.
    pub fn send_call(&mut self, client: ClientId, tg: u32, keyed: bool, talker: &str) {
        let Some(sink) = self.sinks.get(&client) else {
            return;
        };

        let mut frame = Vec::with_capacity(AUDIO_HEADER + talker.len());
        frame.push(KIND_CALL);
        frame.push(u8::from(keyed));
        frame.extend_from_slice(&tg.to_be_bytes());
        frame.extend_from_slice(talker.as_bytes());

        let _ = sink.try_send(frame);
    }

    pub fn send_deny(&mut self, client: ClientId, tg: u32, reason: &str) {
        let Some(sink) = self.sinks.get(&client) else {
            return;
        };
        let mut frame = Vec::with_capacity(AUDIO_HEADER + reason.len());
        frame.push(KIND_DENY);
        frame.push(0);
        frame.extend_from_slice(&tg.to_be_bytes());
        frame.extend_from_slice(reason.as_bytes());
        let _ = sink.try_send(frame);
    }

    pub fn send_pcm(&mut self, client: ClientId, tg: u32, pcm: &[i16]) {
        let Some(sink) = self.sinks.get(&client) else {
            return;
        };

        let mut frame = Vec::with_capacity(AUDIO_HEADER + pcm.len() * 2);
        frame.push(KIND_AUDIO);
        frame.push(0); // reserved - keeps the PCM 2-byte aligned
        frame.extend_from_slice(&tg.to_be_bytes());
        for s in pcm {
            frame.extend_from_slice(&s.to_le_bytes());
        }

        if sink.try_send(frame).is_err() {
            // Full or closed; the reader task cleans up closed sockets.
        }
    }
}

pub async fn serve(addr: String, streams: SharedStreams, router: Shared) -> Result<()> {
    let listener = TcpListener::bind(&addr).await?;
    println!("audio stream listening on {addr}");

    loop {
        let (socket, peer) = listener.accept().await?;
        let streams = streams.clone();
        let router = router.clone();

        tokio::spawn(async move {
            // The token arrives in the request URI. Captured during the
            // handshake because tungstenite does not surface it afterwards.
            let token = Arc::new(Mutex::new(None::<String>));
            let captured = token.clone();

            // The Err variant is tungstenite's Response, and its size is fixed by
            // the callback signature the handshake requires - there is nothing
            // here to box. Clippy is right about the size and wrong about who
            // can do anything about it.
            #[allow(clippy::result_large_err)]
            let ws =
                tokio_tungstenite::accept_hdr_async(socket, move |req: &Request, res: Response| {
                    if let Some(q) = req.uri().query() {
                        for pair in q.split('&') {
                            if let Some(v) = pair.strip_prefix("token=") {
                                *captured.lock().unwrap() = Some(v.to_string());
                            }
                        }
                    }
                    Ok(res)
                })
                .await;

            let Ok(ws) = ws else { return };

            let token = token.lock().unwrap().clone();
            let client = token
                .as_deref()
                .and_then(|t| streams.lock().ok().and_then(|s| s.client_for(t)));

            let Some(client) = client else {
                println!("stream: refused {peer} (no valid token)");
                return;
            };

            let (mut tx_ws, mut rx_ws) = ws.split();
            let (tx, mut rx) = mpsc::channel::<Vec<u8>>(64);

            // Tell the page the sample rate rather than hardcoding 8000 in two
            // places that can drift apart.
            let mut hello = vec![KIND_HELLO];
            hello.extend_from_slice(&crate::dsp::RATE.to_be_bytes());
            let _ = tx.try_send(hello);

            if let Ok(mut s) = streams.lock() {
                s.sinks.insert(client, tx);
            }
            println!("stream: {client} connected from {peer}");

            let writer = tokio::spawn(async move {
                // A radio channel is silent most of the time, and this socket
                // sends nothing while it is. Every proxy between here and the
                // player treats a silent connection as a dead one: Cloudflare
                // closes an idle WebSocket at around 100 seconds, and consumer
                // NAT tables drop idle flows sooner than that.
                //
                // Without this the failure is nastily specific - everything
                // works while somebody is talking, and listeners silently drop
                // during exactly the quiet spells that precede the traffic they
                // are waiting for.
                let mut ping = tokio::time::interval(Duration::from_secs(25));
                ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

                loop {
                    tokio::select! {
                        frame = rx.recv() => match frame {
                            Some(frame) => {
                                if tx_ws.send(Message::Binary(frame.into())).await.is_err() {
                                    break;
                                }
                            }
                            None => break,
                        },
                        _ = ping.tick() => {
                            if tx_ws.send(Message::Ping(Vec::new().into())).await.is_err() {
                                break;
                            }
                        }
                    }
                }
            });

            // A console transmits over this same socket. Keying by HTTP would
            // add a round trip to every press, and push-to-talk latency is the
            // one thing a dispatcher feels immediately.
            let mut talker: Option<crate::dsp::Talker> = None;
            const QUIET: std::time::Duration = std::time::Duration::from_secs(1);
            let mut quiet_since: Option<std::time::Instant> = None;

            while let Some(Ok(msg)) = rx_ws.next().await {
                if msg.is_close() {
                    break;
                }
                let Message::Binary(buf) = msg else { continue };
                if buf.len() < AUDIO_HEADER {
                    continue;
                }

                let kind = buf[0];
                let tg = u32::from_be_bytes([buf[2], buf[3], buf[4], buf[5]]);

                match kind {
                    TX_KEY => {
                        // The router decides, exactly as it does for a radio.
                        // This is the platform's global no-double rule, and a
                        // console gets no exemption from it: if a field unit
                        // holds the talkgroup, dispatch is refused and told so.
                        // Both under ONE router lock, and released before the
                        // streams lock is taken. Every other path here locks
                        // router-then-streams, and taking them the other way
                        // round in one place is how a deadlock gets built.
                        let (outcome, watching) = {
                            let mut r = match router.lock() {
                                Ok(g) => g,
                                Err(p) => p.into_inner(),
                            };
                            let outcome = r.key(tg, client);
                            (outcome, consoles_on(&r, tg))
                        };

                        let mut s = match streams.lock() {
                            Ok(g) => g,
                            Err(p) => p.into_inner(),
                        };
                        match outcome {
                            Ok(keyed) => {
                                if let crate::router::Keyed::Preempted(was) = keyed {
                                    // Tell the unit it was cut off. Its own
                                    // server still believes it is talking, and
                                    // it will find out on its next attempt -
                                    // but the person holding the radio should
                                    // hear about it now, not later.
                                    println!("console {client} PREEMPTED {was} on tg {tg}");
                                    s.send_deny(was, tg, "preempted by dispatch");
                                    s.send_call(was, tg, false, "");
                                } else {
                                    println!("console {client} keyed tg {tg}");
                                }
                                // EVERY console on the talkgroup, not just
                                // the one that keyed. A game unit's key fans
                                // out through announce_call; a console's went
                                // only back to itself, so a position could
                                // watch a channel it was monitoring stay dark
                                // while another position transmitted on it -
                                // and two dispatchers would double because
                                // neither could see the other.
                                for c in &watching {
                                    s.send_call(*c, tg, true, "DISPATCH");
                                }
                                s.send_call(client, tg, true, "DISPATCH");
                            }
                            Err(e) => {
                                println!("console {client} refused tg {tg}: {e}");
                                s.send_deny(client, tg, e);
                            }
                        }
                    }

                    TX_UNKEY => {
                        let watching = {
                            let mut r = match router.lock() {
                                Ok(g) => g,
                                Err(p) => p.into_inner(),
                            };
                            // Only release what we actually hold. An unkey for
                            // somebody else's call would be a console able to
                            // cut off a field unit by asking nicely.
                            r.unkey_as(tg, client);
                            consoles_on(&r, tg)
                        };
                        let mut s = match streams.lock() {
                            Ok(g) => g,
                            Err(p) => p.into_inner(),
                        };
                        for c in &watching {
                            s.send_call(*c, tg, false, "");
                        }
                        s.send_call(client, tg, false, "");
                    }

                    TX_AUDIO => {
                        // The talkgroup comes from the ROUTER, not the frame.
                        // Taking it from the frame would let a position
                        // transmit on a talkgroup it never keyed.
                        let dest = {
                            let r = match router.lock() {
                                Ok(g) => g,
                                Err(p) => p.into_inner(),
                            };
                            r.destination_of(client)
                                .map(|(tg, ls)| (tg, r.source_sink(tg, client), ls))
                        };
                        // Every way this can fail says so, once a second.
                        //
                        // There were four silent `continue`s here and only one
                        // of them logged. A console could transmit perfectly
                        // into a black hole - grant accepted, frames arriving,
                        // nothing delivered - and the node had nothing to say
                        // about it. Diagnosing that cost an evening, so each
                        // branch now names itself.
                        //
                        // Rate limited because this runs fifty times a second
                        // and an unthrottled line would be the fault.
                        let mut whine = |why: &str| {
                            let now = std::time::Instant::now();
                            let last = quiet_since.get_or_insert(now - QUIET);
                            if now.duration_since(*last) >= QUIET {
                                *last = now;
                                println!("console {client} audio dropped: {why}");
                            }
                        };

                        let Some((live_tg, src, listeners)) = dest else {
                            whine("not keyed on any route - key was lost or released");
                            continue;
                        };
                        if listeners.is_empty() {
                            whine(&format!("nobody is listening on tg {live_tg}"));
                            continue;
                        }

                        // 48 kHz mono i16, little endian, after the 4-byte
                        // header - the same alignment reason as outbound.
                        let pcm: Vec<i16> = buf[AUDIO_HEADER..]
                            .as_chunks::<2>()
                            .0
                            .iter()
                            .map(|c| i16::from_le_bytes(*c))
                            .collect();

                        if talker.is_none() {
                            match crate::dsp::Talker::new() {
                                Ok(t) => talker = Some(t),
                                Err(e) => {
                                    println!("console {client} audio dropped: vocoder will not start: {e}");
                                    continue;
                                }
                            }
                        }

                        // A dispatch console has no RF of its own, so its
                        // source hop is clean - but a listener across a patch
                        // still gets two hops, and hears the console the way a
                        // patch device would deliver it.
                        let sinks: Vec<crate::dsp::Sink> =
                            listeners.iter().map(|l| l.sink()).collect();
                        let Some(t) = talker.as_mut() else { continue };
                        let lanes = match t.push_pcm(&pcm, src, &sinks) {
                            Ok(l) => l,
                            Err(e) => {
                                whine(&format!("vocoder refused the frame: {e}"));
                                continue;
                            }
                        };

                        let mut s = match streams.lock() {
                            Ok(g) => g,
                            Err(p) => p.into_inner(),
                        };
                        let mut sent = 0usize;
                        for (key, pcm8) in &lanes {
                            if pcm8.is_empty() {
                                continue;
                            }
                            for l in &listeners {
                                if crate::dsp::key_of(l.sink()) == *key {
                                    s.send_pcm(l.client, live_tg, pcm8);
                                    sent += 1;
                                }
                            }
                        }
                        drop(s);

                        // Vocoded audio that matched no listener's lane. The
                        // frame was accepted, encoded, and then quietly fitted
                        // nobody - which is its own failure and reads exactly
                        // like the others from outside.
                        if sent == 0 && lanes.iter().any(|(_, p)| !p.is_empty()) {
                            whine(&format!(
                                "{} lane(s) encoded but matched none of {} listener(s) on tg {live_tg}",
                                lanes.len(),
                                listeners.len()
                            ));
                        }
                    }

                    _ => {}
                }
            }

            // A socket that drops mid-transmission must not leave the
            // talkgroup keyed against everybody else on it.
            {
                let mut r = match router.lock() {
                    Ok(g) => g,
                    Err(p) => p.into_inner(),
                };
                if let Some((tg, _)) = r.destination_of(client) {
                    println!("console {client} dropped while keyed on tg {tg}");
                    r.unkey(tg);
                }
            }

            writer.abort();
            if let Ok(mut s) = streams.lock() {
                s.sinks.remove(&client);
            }
            println!("stream: {client} disconnected");
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::router::ClientId;

    /// A conventional route id, as `Conventional.routeId` builds one:
    /// `0x80000000 | round(MHz * 10000)`.
    const CONV: u32 = 0x8000_0000 | 1_552_350;

    fn one_frame(build: impl FnOnce(&mut Streams, ClientId)) -> Vec<u8> {
        let mut s = Streams::default();
        let client = ClientId::new(0, 1);
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        s.sinks.insert(client, tx);
        build(&mut s, client);
        rx.try_recv().expect("a frame was sent")
    }

    /// The regression this exists for: the talkgroup was written as a u16, so
    /// every conventional route truncated to a number matching nothing and its
    /// audio was delivered addressed to a route that did not exist. Trunked
    /// ids fit in 16 bits, which is why only conventional broke and why it
    /// went unnoticed.
    #[test]
    fn a_conventional_route_survives_the_wire() {
        let frame = one_frame(|s, c| s.send_pcm(c, CONV, &[1, -1]));

        assert_eq!(frame[0], KIND_AUDIO);
        assert_eq!(
            u32::from_be_bytes([frame[2], frame[3], frame[4], frame[5]]),
            CONV,
            "a conventional id must arrive whole, not truncated to 16 bits"
        );
    }

    #[test]
    fn call_and_deny_carry_the_same_id_width() {
        for frame in [
            one_frame(|s, c| s.send_call(c, CONV, true, "unit")),
            one_frame(|s, c| s.send_deny(c, CONV, "busy")),
        ] {
            assert_eq!(
                u32::from_be_bytes([frame[2], frame[3], frame[4], frame[5]]),
                CONV,
            );
        }
    }

    /// The PCM is wrapped in an Int16Array by both clients, and a typed array
    /// view whose offset is not a multiple of its element size throws outright
    /// rather than reading slowly. An odd header would break every listener.
    #[test]
    fn the_audio_header_stays_even_and_matches_the_frame() {
        assert_eq!(AUDIO_HEADER % 2, 0, "PCM must start 2-byte aligned");

        let frame = one_frame(|s, c| s.send_pcm(c, 1001, &[7, 8, 9]));
        assert_eq!(frame.len(), AUDIO_HEADER + 3 * 2);

        // Little endian, matching every tool that will ever open this.
        assert_eq!(
            i16::from_le_bytes([frame[AUDIO_HEADER], frame[AUDIO_HEADER + 1]]),
            7,
        );
    }

    /// The hello states the sample rate as a u32. A console that read it as a
    /// u16 took the two high bytes of 8000, got zero, and divided by it.
    #[test]
    fn the_hello_states_the_rate_as_a_u32() {
        let mut hello = vec![KIND_HELLO];
        hello.extend_from_slice(&crate::dsp::RATE.to_be_bytes());

        assert_eq!(hello.len(), 5);
        assert_eq!(
            u32::from_be_bytes([hello[1], hello[2], hello[3], hello[4]]),
            crate::dsp::RATE,
        );
    }
}
