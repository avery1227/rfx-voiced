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
            RSA_PKCS1_SHA256,
            RSA_PKCS1_SHA384,
            RSA_PKCS1_SHA512,
            ECDSA_NISTP256_SHA256,
            ECDSA_NISTP384_SHA384,
            RSA_PSS_SHA256,
            RSA_PSS_SHA384,
            RSA_PSS_SHA512,
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

    Some(VoicePacket {
        codec,
        target: header & 0x1f,
        session,
        payload_start,
        payload_len,
    })
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

/// The Mumble protocol caps a control message at 8 MiB, and nothing this tap
/// parses comes near it.
const MAX_FRAME: usize = 8 * 1024 * 1024;

/// The inbound half of `frame`, read a piece at a time.
///
/// It exists as a struct because the read loop races the talkspurt sweep in a
/// `select!`, and `read_exact` is NOT cancellation safe: its count of bytes
/// already taken lives inside the future, so a sweep tick landing between the
/// two halves of a header split across TLS records dropped that future and lost
/// those bytes for good. The next header was then read out of the middle of a
/// message and every frame after it was garbage - the tap died on a decode
/// error at best, and audio for that whole FXServer stopped until the reconnect.
///
/// Keeping the fill count out here fixes that, because `read` IS cancel safe:
/// cancelled, it has read nothing, and what we already had is still counted.
#[derive(Default)]
struct Header {
    buf: [u8; 6],
    fill: usize,
}

impl Header {
    /// One cancellable step. `Some((kind, len))` once six bytes are in hand,
    /// `None` while it is still short.
    ///
    /// The single await is the only cancellation point, and nothing has been
    /// consumed when it is reached.
    async fn read_from<R: tokio::io::AsyncRead + Unpin>(
        &mut self,
        rd: &mut R,
    ) -> Result<Option<(u16, usize)>> {
        // fill is always < 6 here, so the slice is never empty and a zero-byte
        // read really does mean the far end has gone.
        let n = rd
            .read(&mut self.buf[self.fill..])
            .await
            .context("connection closed by server")?;
        if n == 0 {
            anyhow::bail!("connection closed by server");
        }
        self.fill += n;
        if self.fill < 6 {
            return Ok(None);
        }
        self.fill = 0;

        let kind = u16::from_be_bytes([self.buf[0], self.buf[1]]);
        let len = u32::from_be_bytes([self.buf[2], self.buf[3], self.buf[4], self.buf[5]]) as usize;

        // Refuse an impossible length BEFORE anybody allocates for it. A
        // desynchronised or hostile stream can name 4 GiB here, and that either
        // fails - an allocation failure in Rust aborts the process, taking every
        // other tap, the control API and all audio with it - or succeeds and
        // wedges the read in a body that will never arrive, since there is no
        // read timeout. There is no safe place to resynchronise to either, so
        // this is fatal to the connection by design: the supervisor drops this
        // server's routes and redials in three seconds.
        if len > MAX_FRAME {
            anyhow::bail!("oversized mumble frame: kind {kind}, {len} bytes");
        }

        Ok(Some((kind, len)))
    }
}

pub fn install_crypto_provider() -> Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .map_err(|_| anyhow::anyhow!("failed to install rustls crypto provider"))
}

pub async fn run(
    cfg: &Settings,
    router: Shared,
    streams: SharedStreams,
    recorder: crate::recorder::SharedRecorder,
) -> Result<()> {
    let tls_config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAny))
        .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(tls_config));

    println!(
        "[server {}] connecting to {}:{} as {:?}",
        cfg.server, cfg.host, cfg.port, cfg.username
    );

    let tcp = tokio::net::TcpStream::connect((cfg.host.as_str(), cfg.port))
        .await
        .with_context(|| format!("tcp connect to {}:{}", cfg.host, cfg.port))?;
    tcp.set_nodelay(true)?;

    let server_name = rustls_pki_types::ServerName::try_from("mumble")?.to_owned();
    let stream = connector
        .connect(server_name, tcp)
        .await
        .context("tls handshake")?;
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
                if tx
                    .send((msg::UDP_TUNNEL, keepalive_frame(seq)))
                    .await
                    .is_err()
                {
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
                let ping = proto::Ping {
                    timestamp: Some(0),
                    ..Default::default()
                };
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

    // One mixer per AM frequency, alive only while people are colliding on it.
    let mut ammix: HashMap<u32, dsp::AmMix> = HashMap::new();
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

    let mut header = Header::default();

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

            read = header.read_from(&mut rd) => {
                // Short of a whole header. What arrived is held for the next
                // pass rather than being read again from the start.
                let Some((kind, len)) = read? else { continue };

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
                                    t.routed_to = dest.as_ref().map(|(tg, _, _)| *tg);

                                    match &dest {
                                        Some((tg, _, listeners)) => println!(
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
                                if let Some((tg, src, listeners)) = dest {
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
                                            let sinks: Vec<dsp::Sink> =
                                                listeners.iter().map(|l| l.sink()).collect();

                                            match talker.push(payload, src, &sinks) {
                                                Ok(lanes) => {
                                                    // AM does not capture, so more
                                                    // than one radio can be up on
                                                    // this frequency at once and what
                                                    // a listener hears is the sum. It
                                                    // has to be summed here: two
                                                    // talkers' frames arriving at one
                                                    // client would interleave into
                                                    // alternating chunks of each.
                                                    //
                                                    // Only the AM renderings. The same
                                                    // transmission patched onto a P25
                                                    // talkgroup is not colliding
                                                    // there - that side has a grant.
                                                    let out = if router
                                                        .lock()
                                                        .ok()
                                                        .is_some_and(|r| r.am_collision(tg))
                                                    {
                                                        let mix = ammix.entry(tg).or_default();
                                                        let mut rest = Vec::new();
                                                        for (key, pcm) in lanes {
                                                            if key.0 == dsp::Mode::Am {
                                                                mix.add(key, v.session, &pcm, now);
                                                            } else {
                                                                rest.push((key, pcm));
                                                            }
                                                        }
                                                        let mut mixed = mix.drain(now);
                                                        mixed.extend(rest);
                                                        mixed
                                                    } else {
                                                        // A collision that just ended:
                                                        // what is still queued belongs
                                                        // to whoever is left.
                                                        match ammix.remove(&tg) {
                                                            Some(mut mix) => {
                                                                let mut out = mix.flush();
                                                                out.extend(lanes);
                                                                out
                                                            }
                                                            None => lanes,
                                                        }
                                                    };

                                                    // Record what the SOURCE side
                                                    // heard, cleanest lane. Not a
                                                    // patched rendering: the call
                                                    // happened on one channel, and
                                                    // keeping the version that went
                                                    // through two extra hops would be
                                                    // keeping the worst copy of it.
                                                    // Not one listener's bad reception
                                                    // either - a recording nobody can
                                                    // make out is not evidence of
                                                    // anything.
                                                    let best = out
                                                        .iter()
                                                        .filter(|((m, _, bridged), _)| {
                                                            !bridged && *m == src.mode
                                                        })
                                                        .max_by_key(|((_, lane, _), _)| *lane);

                                                    if let Some((_, pcm)) = best {
                                                        if let Ok(mut r) = recorder.lock() {
                                                            r.push(tg, pcm);
                                                        }
                                                    }

                                                    if let Ok(mut s) = streams.lock() {
                                                        for (key, pcm) in &out {
                                                            if pcm.is_empty() {
                                                                continue;
                                                            }
                                                            for l in &listeners {
                                                                if dsp::key_of(l.sink()) == *key {
                                                                    s.send_pcm(l.client, tg, pcm);
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

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    /// A header arriving in two pieces is one header. Murmur coalesces queued
    /// messages into TLS records and anything over 16 KiB forces a split, so a
    /// six-byte header straddling a record boundary is ordinary traffic on a
    /// busy server rather than a curiosity.
    #[tokio::test]
    async fn a_header_split_across_reads_is_still_one_header() {
        let (mut server, mut tap) = tokio::io::duplex(64);
        let mut header = Header::default();

        server.write_all(&[0, 5, 0]).await.unwrap();
        assert_eq!(header.read_from(&mut tap).await.unwrap(), None);

        server.write_all(&[0, 0, 42]).await.unwrap();
        assert_eq!(header.read_from(&mut tap).await.unwrap(), Some((5, 42)));
    }

    /// And the half already taken survives the sweep tick cancelling the read.
    /// This is the whole reason the fill count lives outside the future: with
    /// `read_exact` those three bytes were gone, the next header was parsed out
    /// of the middle of a message, and every frame afterwards was garbage.
    #[tokio::test]
    async fn a_cancelled_header_read_keeps_what_it_already_took() {
        let (mut server, mut tap) = tokio::io::duplex(64);
        let mut header = Header::default();

        server.write_all(&[0, 5, 0]).await.unwrap();
        // The 50 ms talkspurt sweep fires while the header is half read, and
        // the read future is dropped where it stands.
        let cancelled = tokio::time::timeout(Duration::from_millis(50), async {
            loop {
                if header.read_from(&mut tap).await.unwrap().is_some() {
                    break;
                }
            }
        })
        .await;
        assert!(cancelled.is_err(), "the header completed early");

        server.write_all(&[0, 0, 42]).await.unwrap();
        let resumed = tokio::time::timeout(Duration::from_millis(500), header.read_from(&mut tap))
            .await
            .expect("the framing desynchronised - the first three bytes were lost");
        assert_eq!(resumed.unwrap(), Some((5, 42)));
    }

    /// A length off a desynchronised or hostile stream is refused before
    /// anything allocates for it. Failing an allocation aborts the process,
    /// which would take every other tap and the control API down with it.
    #[tokio::test]
    async fn an_impossible_frame_length_is_refused_rather_than_allocated() {
        let (mut server, mut tap) = tokio::io::duplex(64);
        let mut header = Header::default();

        server
            .write_all(&[0, 5, 0xff, 0xff, 0xff, 0xff])
            .await
            .unwrap();
        let err = header
            .read_from(&mut tap)
            .await
            .expect_err("a 4 GiB frame was accepted");
        assert!(err.to_string().contains("oversized"), "{err}");
    }

    /// The cap is Mumble's own, and everything this tap actually parses fits
    /// inside it with room to spare.
    #[tokio::test]
    async fn a_frame_at_the_limit_is_still_accepted() {
        let (mut server, mut tap) = tokio::io::duplex(64);
        let mut header = Header::default();

        let mut h = vec![0, 7];
        h.extend_from_slice(&(MAX_FRAME as u32).to_be_bytes());
        server.write_all(&h).await.unwrap();
        assert_eq!(
            header.read_from(&mut tap).await.unwrap(),
            Some((7, MAX_FRAME))
        );
    }
}
