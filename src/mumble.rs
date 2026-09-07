//! The tap: a headless Mumble client attached to FXServer's built-in server.
//!
//! What we learned building this, from code/components/voip-server-mumble:
//!
//!   * It is uMurmur. External clients are refused unless the server sets
//!     `mumble_allowExternalConnections true`.
//!   * Usernames are `[%d] %s` - the bracketed prefix is the FXServer player
//!     id, and it is the ONLY thing tying a Mumble session to a player.
//!   * `Client_janitor` closes any client whose `lastActivity` has aged past
//!     INACTIVITY_TIMEOUT, and only `Client_voiceMsg` refreshes it. A silent
//!     listener is reaped, so we transmit periodically.
//!   * `case UDPTunnel:` routes tunnelled voice into `Client_voiceMsg`, so the
//!     keepalive can go over TLS. No UDP, and no OCB2 crypto, is needed.
//!   * Because we never send a UDP ping the server leaves `bUDP = false` and
//!     tunnels every other client's voice to us over the same connection.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use prost::Message as _;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

use crate::control::Shared;
use crate::dsp;
use crate::router::SessionId;
use crate::stream::SharedStreams;
use crate::varint;

#[allow(dead_code)] // the generated module carries every Mumble message; we use a few
mod proto {
    include!(concat!(env!("OUT_DIR"), "/mumble_proto.rs"));
}

mod msg {
    pub const VERSION: u16 = 0;
    pub const UDP_TUNNEL: u16 = 1;
    pub const AUTHENTICATE: u16 = 2;
    pub const PING: u16 = 3;
    pub const REJECT: u16 = 4;
    pub const SERVER_SYNC: u16 = 5;
    pub const CHANNEL_STATE: u16 = 7;
    pub const USER_REMOVE: u16 = 8;
    pub const USER_STATE: u16 = 9;
    pub const PERMISSION_DENIED: u16 = 12;
    pub const CODEC_VERSION: u16 = 21;
    pub const CRYPT_SETUP: u16 = 15;
    pub const SERVER_CONFIG: u16 = 24;
}

pub struct Settings {
    /// Which FXServer this tap is attached to. Player ids are only unique
    /// within one server, so every identity derived here is scoped by it.
    pub server: u32,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
}

// ---------------------------------------------------------------------------
// TLS
//
// FXServer presents a self-signed, expired certificate; that is normal for
// Mumble and the desktop client warns about exactly this. We are attaching to
// a server we own, so verification is skipped deliberately rather than by
// accident.
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct AcceptAny;

impl rustls::client::danger::ServerCertVerifier for AcceptAny {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls_pki_types::CertificateDer<'_>,
        _intermediates: &[rustls_pki_types::CertificateDer<'_>],
        _server_name: &rustls_pki_types::ServerName<'_>,
        _ocsp: &[u8],
        _now: rustls_pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls_pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls_pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        use rustls::SignatureScheme::*;
        vec![
            RSA_PKCS1_SHA256, RSA_PKCS1_SHA384, RSA_PKCS1_SHA512,
            ECDSA_NISTP256_SHA256, ECDSA_NISTP384_SHA384,
            RSA_PSS_SHA256, RSA_PSS_SHA384, RSA_PSS_SHA512,
            ED25519,
        ]
    }
}

// ---------------------------------------------------------------------------
// Voice packets
// ---------------------------------------------------------------------------

/// A zero-length Opus payload with the terminator bit set - the end-of-
/// transmission marker a real client sends on PTT release. Inaudible, and it
/// lands in `Client_voiceMsg`, which is the only thing that keeps us out of
/// the janitor's way.
fn keepalive_frame(seq: u64) -> Vec<u8> {
    let mut p = Vec::with_capacity(8);
    p.push(4 << 5); // codec 4 (Opus), target 0
    varint::write_varint(&mut p, seq);
    varint::write_varint(&mut p, 0x2000);
    p
}

struct VoicePacket {
    codec: u8,
    target: u8,
    session: u32,
    payload_start: usize,
    payload_len: usize,
}

fn parse_voice(data: &[u8]) -> Option<VoicePacket> {
    let header = *data.first()?;
    let mut r = varint::Reader::new(&data[1..]);

    let session = r.varint()? as u32;
    let _sequence = r.varint()?;

    let codec = header >> 5;
    let payload_len = if codec == 4 {
        (r.varint()? as u64 & 0x1fff) as usize
    } else {
        r.remaining()
    };

    let payload_start = 1 + r.pos();
    if payload_start + payload_len > data.len() {
        return None;
    }

    Some(VoicePacket { codec, target: header & 0x1f, session, payload_start, payload_len })
}

/// FiveM's voice client does not set the Opus terminator bit - it simply stops
/// sending - so a gap detector is the PRIMARY end-of-talkspurt signal, not a
/// fallback. Frames arrive every ~40 ms, so this is three missed frames.
const SILENCE_MS: u64 = 150;

struct Talk {
    packets: u64,
    bytes: u64,
    active: bool,
    started: Instant,
    last: Instant,
    routed_to: Option<u32>,
}

// ---------------------------------------------------------------------------

type Outbound = (u16, Vec<u8>);

fn frame(kind: u16, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(6 + body.len());
    out.extend_from_slice(&kind.to_be_bytes());
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(body);
    out
}

pub fn install_crypto_provider() -> Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .map_err(|_| anyhow::anyhow!("failed to install rustls crypto provider"))
}

pub async fn run(cfg: &Settings, router: Shared, streams: SharedStreams) -> Result<()> {
    let tls_config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAny))
        .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(tls_config));

    println!("[server {}] connecting to {}:{} as {:?}", cfg.server, cfg.host, cfg.port, cfg.username);

    let tcp = tokio::net::TcpStream::connect((cfg.host.as_str(), cfg.port))
        .await
        .with_context(|| format!("tcp connect to {}:{}", cfg.host, cfg.port))?;
    tcp.set_nodelay(true)?;

    let server_name = rustls_pki_types::ServerName::try_from("mumble")?.to_owned();
    let stream = connector.connect(server_name, tcp).await.context("tls handshake")?;
    println!("tls established");

    let (mut rd, mut wr) = tokio::io::split(stream);
    let (tx, mut rx) = mpsc::channel::<Outbound>(64);

    tokio::spawn(async move {
        while let Some((kind, body)) = rx.recv().await {
            if wr.write_all(&frame(kind, &body)).await.is_err() {
                break;
            }
        }
    });

    let version = proto::Version {
        version: Some((1 << 16) | (4 << 8)),
        release: Some("rfx-voiced".into()),
        os: Some(std::env::consts::OS.into()),
        os_version: Some("0.1".into()),
        ..Default::default()
    };
    tx.send((msg::VERSION, version.encode_to_vec())).await?;

    let auth = proto::Authenticate {
        username: Some(cfg.username.clone()),
        password: Some(cfg.password.clone()),
        opus: Some(true),
        ..Default::default()
    };
    tx.send((msg::AUTHENTICATE, auth.encode_to_vec())).await?;

    {
        let tx = tx.clone();
        tokio::spawn(async move {
            let mut seq: u64 = 0;
            let mut tick = tokio::time::interval(Duration::from_secs(2));
            loop {
                tick.tick().await;
                seq = seq.wrapping_add(1);
                if tx.send((msg::UDP_TUNNEL, keepalive_frame(seq))).await.is_err() {
                    break;
                }
            }
        });
    }

    {
        let tx = tx.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(5));
            loop {
                tick.tick().await;
                let ping = proto::Ping { timestamp: Some(0), ..Default::default() };
                if tx.send((msg::PING, ping.encode_to_vec())).await.is_err() {
                    break;
                }
            }
        });
    }

    let mut users: HashMap<u32, String> = HashMap::new();
    let mut heard: HashMap<u32, Talk> = HashMap::new();
    // One vocoder chain per talker, created on first audio and dropped when
    // they leave. Codec2 state is per-stream, so it cannot be shared.
    let mut talkers: HashMap<u32, dsp::Talker> = HashMap::new();
    let mut me: Option<u32> = None;

    // CHANNEL LISTENING - how the tap hears everyone.
    //
    // pma-voice puts each player in their own channel and, while speaking,
    // targets the channels of nearby players. Crucially its proximity loop
    // iterates GetActivePlayers(), which includes the speaker, so a speaker
    // ALWAYS targets their own channel - even standing alone in Blaine County.
    //
    // So listening to every channel receives every player unconditionally,
    // with no cooperation from the client at all. No tap channel, no voice
    // target manipulation, no Mumble natives in client Lua.
    let mut known_channels: Vec<u32> = Vec::new();
    let mut listening: std::collections::HashSet<u32> = std::collections::HashSet::new();

    let mut sweep = tokio::time::interval(Duration::from_millis(50));
    sweep.tick().await;

    let mut header = [0u8; 6];

    loop {
        tokio::select! {
            _ = sweep.tick() => {
                let now = Instant::now();
                for (session, t) in heard.iter_mut() {
                    if t.active && now.duration_since(t.last).as_millis() as u64 > SILENCE_MS {
                        t.active = false;
                        let who = users.get(session).map(String::as_str).unwrap_or("?");
                        let dest = match t.routed_to {
                            Some(tg) => format!("tg {tg}"),
                            None => "proximity only".into(),
                        };
                        println!("TX end    {who:<28} {} packets, {} ms, {dest}",
                            t.packets, now.duration_since(t.started).as_millis());
                    }
                }
            }

            read = rd.read_exact(&mut header) => {
                read.context("connection closed by server")?;

                let kind = u16::from_be_bytes([header[0], header[1]]);
                let len = u32::from_be_bytes([header[2], header[3], header[4], header[5]]) as usize;

                let mut body = vec![0u8; len];
                rd.read_exact(&mut body).await.context("short read")?;

                match kind {
                    msg::UDP_TUNNEL => {
                        if let Some(v) = parse_voice(&body) {
                            if Some(v.session) != me {
                                let now = Instant::now();
                                let who = users.get(&v.session).map(String::as_str).unwrap_or("?").to_string();

                                // Resolved per packet, not per talkspurt: the grant
                                // cannot change mid-transmission, but a listener's
                                // link quality can, and it should be heard changing.
                                let sid = SessionId::new(cfg.server, v.session);
                                let dest = router.lock().ok().and_then(|r| r.destination(sid));

                                let t = heard.entry(v.session).or_insert(Talk {
                                    packets: 0, bytes: 0, active: false,
                                    started: now, last: now, routed_to: None,
                                });

                                if !t.active {
                                    t.active = true;
                                    t.started = now;
                                    t.packets = 0;
                                    t.bytes = 0;
                                    t.routed_to = dest.as_ref().map(|(tg, _)| *tg);

                                    match &dest {
                                        Some((tg, listeners)) => println!(
                                            "TX start  {who:<28} -> tg {tg}, {} listener(s)  [codec {} target {}]",
                                            listeners.len(), v.codec, v.target),
                                        None => println!(
                                            "TX start  {who:<28} -> proximity only  [codec {} target {}]",
                                            v.codec, v.target),
                                    }
                                }

                                t.packets += 1;
                                t.bytes += v.payload_len as u64;
                                t.last = now;

                                // Vocode and deliver.
                                if let Some((tg, listeners)) = dest {
                                    if !listeners.is_empty() && v.codec == 4 && v.payload_len > 0 {
                                        let payload = &body[v.payload_start..v.payload_start + v.payload_len];

                                        let talker = match talkers.entry(v.session) {
                                            std::collections::hash_map::Entry::Occupied(e) => Some(e.into_mut()),
                                            std::collections::hash_map::Entry::Vacant(e) => {
                                                match dsp::Talker::new() {
                                                    Ok(t) => Some(e.insert(t)),
                                                    Err(err) => {
                                                        eprintln!("dsp: {err}");
                                                        None
                                                    }
                                                }
                                            }
                                        };

                                        if let Some(talker) = talker {
                                            let qualities: Vec<u8> =
                                                listeners.iter().map(|(_, q)| *q).collect();

                                            match talker.push(payload, &qualities) {
                                                Ok(lanes) => {
                                                    if let Ok(mut s) = streams.lock() {
                                                        for (lane, pcm) in &lanes {
                                                            if pcm.is_empty() { continue; }
                                                            for (client, q) in &listeners {
                                                                if dsp::lane_of(*q) == *lane {
                                                                    s.send_pcm(*client, tg, pcm);
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                                Err(e) => eprintln!("vocoder: {e}"),
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }

                    msg::SERVER_SYNC => {
                        let m = proto::ServerSync::decode(&body[..])?;
                        me = m.session;
                        println!("authenticated; our session is {:?}", me);
                        // Channels are usually announced before we are told our
                        // own session id, so catch up on all of them here.
                        if let Some(session) = me {
                            let pending: Vec<u32> = known_channels
                                .iter().copied()
                                .filter(|c| !listening.contains(c))
                                .collect();
                            if !pending.is_empty() {
                                for c in &pending { listening.insert(*c); }
                                let us = proto::UserState {
                                    session: Some(session),
                                    listening_channel_add: pending.clone(),
                                    ..Default::default()
                                };
                                tx.send((msg::USER_STATE, us.encode_to_vec())).await?;
                                println!("listening to {} channel(s)", pending.len());
                            }
                        }
                        if let Ok(r) = router.lock() {
                            println!("router: {}", r.summary());
                        }
                    }

                    msg::USER_STATE => {
                        let m = proto::UserState::decode(&body[..])?;
                        if let Some(s) = m.session {
                            if let Some(name) = m.name.clone() {
                                let bound = router
                                    .lock()
                                    .ok()
                                    .and_then(|mut r| r.bind(SessionId::new(cfg.server, s), &name));
                                match bound {
                                    Some(client) => println!("user {s}: {name}  -> {client}"),
                                    None => println!("user {s}: {name}  (no player id in username)"),
                                }
                                users.insert(s, name);
                            }
                        }
                    }

                    msg::USER_REMOVE => {
                        let m = proto::UserRemove::decode(&body[..])?;
                        if let Ok(mut r) = router.lock() {
                            r.unbind(SessionId::new(cfg.server, m.session));
                        }
                        users.remove(&m.session);
                        heard.remove(&m.session);
                        talkers.remove(&m.session);
                    }

                    msg::CHANNEL_STATE => {
                        let m = proto::ChannelState::decode(&body[..])?;
                        if let (Some(id), Some(name)) = (m.channel_id, m.name.clone()) {
                            if !known_channels.contains(&id) {
                                known_channels.push(id);
                                println!("channel {id}: {name}");
                            }
                            // Listen rather than join: a client may only be IN
                            // one channel, but it may listen to many.
                            if let Some(session) = me {
                                if listening.insert(id) {
                                    let us = proto::UserState {
                                        session: Some(session),
                                        listening_channel_add: vec![id],
                                        ..Default::default()
                                    };
                                    tx.send((msg::USER_STATE, us.encode_to_vec())).await?;
                                    println!("listening to channel {id} ({name})");
                                }
                            }
                        }
                    }

                    msg::REJECT => {
                        let m = proto::Reject::decode(&body[..])?;
                        anyhow::bail!("rejected: {:?} - {}", m.r#type(), m.reason.unwrap_or_default());
                    }

                    msg::PERMISSION_DENIED => {
                        let m = proto::PermissionDenied::decode(&body[..])?;
                        println!("permission denied: {:?} {}", m.r#type(), m.reason.unwrap_or_default());
                    }

                    msg::VERSION | msg::PING | msg::CRYPT_SETUP
                    | msg::CODEC_VERSION | msg::SERVER_CONFIG => {}

                    other => println!("(unhandled message type {other}, {len} bytes)"),
                }
            }
        }
    }
}
