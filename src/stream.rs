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
///   1 audio  [1][reserved][u16 talkgroup BE][i16 LE PCM ...]
///
/// The audio header is padded to 4 bytes so the PCM starts 2-byte aligned.
/// Without that pad the receiver cannot wrap it in an Int16Array at all -
/// typed array views require their offset to be a multiple of the element
/// size, and it throws rather than reading slowly.
pub const KIND_HELLO: u8 = 0;
pub const KIND_AUDIO: u8 = 1;
/// A call started or ended on a talkgroup.
///
/// Sent when the GRANT is decided, not when audio arrives. A radio is busy
/// from the instant its transmission is approved; a console that waits for
/// the first samples shows the channel clear while somebody is already
/// talking, and clear again in every gap between words.
pub const KIND_CALL: u8 = 2;
/// A transmit request was refused, and why. `[3][0][u16 tg][utf8 reason]`
pub const KIND_DENY: u8 = 3;

// Inbound, from a console. Numbered well clear of the outbound kinds so a
// frame going the wrong way is obviously wrong rather than subtly valid.
const TX_KEY: u8 = 16;
const TX_UNKEY: u8 = 17;
const TX_AUDIO: u8 = 18;

/// Where the PCM starts in an audio frame.
pub const AUDIO_HEADER: usize = 4;

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

        let mut frame = Vec::with_capacity(4 + talker.len());
        frame.push(KIND_CALL);
        frame.push(u8::from(keyed));
        frame.extend_from_slice(&(tg as u16).to_be_bytes());
        frame.extend_from_slice(talker.as_bytes());

        let _ = sink.try_send(frame);
    }

    pub fn send_deny(&mut self, client: ClientId, tg: u32, reason: &str) {
        let Some(sink) = self.sinks.get(&client) else {
            return;
        };
        let mut frame = Vec::with_capacity(4 + reason.len());
        frame.push(KIND_DENY);
        frame.push(0);
        frame.extend_from_slice(&(tg as u16).to_be_bytes());
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
        frame.extend_from_slice(&(tg as u16).to_be_bytes());
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

            while let Some(Ok(msg)) = rx_ws.next().await {
                if msg.is_close() {
                    break;
                }
                let Message::Binary(buf) = msg else { continue };
                if buf.len() < 4 {
                    continue;
                }

                let kind = buf[0];
                let tg = u16::from_be_bytes([buf[2], buf[3]]) as u32;

                match kind {
                    TX_KEY => {
                        // The router decides, exactly as it does for a radio.
                        // This is the platform's global no-double rule, and a
                        // console gets no exemption from it: if a field unit
                        // holds the talkgroup, dispatch is refused and told so.
                        let outcome = {
                            let mut r = match router.lock() {
                                Ok(g) => g,
                                Err(p) => p.into_inner(),
                            };
                            r.key(tg, client)
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
                                s.send_call(client, tg, true, "DISPATCH");
                            }
                            Err(e) => {
                                println!("console {client} refused tg {tg}: {e}");
                                s.send_deny(client, tg, e);
                            }
                        }
                    }

                    TX_UNKEY => {
                        {
                            let mut r = match router.lock() {
                                Ok(g) => g,
                                Err(p) => p.into_inner(),
                            };
                            // Only release what we actually hold. An unkey for
                            // somebody else's call would be a console able to
                            // cut off a field unit by asking nicely.
                            r.unkey_as(tg, client);
                        }
                        let mut s = match streams.lock() {
                            Ok(g) => g,
                            Err(p) => p.into_inner(),
                        };
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
                        let Some((live_tg, src, listeners)) = dest else {
                            continue;
                        };
                        if listeners.is_empty() {
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
                                    eprintln!("console {client}: vocoder: {e}");
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
                        let Ok(lanes) = t.push_pcm(&pcm, src, &sinks) else {
                            continue;
                        };

                        let mut s = match streams.lock() {
                            Ok(g) => g,
                            Err(p) => p.into_inner(),
                        };
                        for (key, pcm8) in &lanes {
                            if pcm8.is_empty() {
                                continue;
                            }
                            for l in &listeners {
                                if crate::dsp::key_of(l.sink()) == *key {
                                    s.send_pcm(l.client, live_tg, pcm8);
                                }
                            }
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
