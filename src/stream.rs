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

use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::tungstenite::Message;

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

pub async fn serve(addr: String, streams: SharedStreams) -> Result<()> {
    let listener = TcpListener::bind(&addr).await?;
    println!("audio stream listening on {addr}");

    loop {
        let (socket, peer) = listener.accept().await?;
        let streams = streams.clone();

        tokio::spawn(async move {
            // The token arrives in the request URI. Captured during the
            // handshake because tungstenite does not surface it afterwards.
            let token = Arc::new(Mutex::new(None::<String>));
            let captured = token.clone();

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
                while let Some(frame) = rx.recv().await {
                    if tx_ws.send(Message::Binary(frame.into())).await.is_err() {
                        break;
                    }
                }
            });

            // We expect nothing from the client; reading is only how we learn
            // the socket has closed.
            while let Some(Ok(msg)) = rx_ws.next().await {
                if msg.is_close() {
                    break;
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
