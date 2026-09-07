//! rfx_p25 voice node.
//!
//! A PLATFORM, not a per-server script: one node serves many FXServers and
//! they share one talkgroup space. A unit on server A and a unit on server B,
//! both on TG 1001, hear each other - which is how a real statewide network
//! spans agencies and dispatch centres. See docs/07-platform.md.
//!
//! Three parts meeting in the routing table:
//!
//!   * the TAPS - one headless Mumble client per FXServer, receiving every
//!     player's Opus stream (src/mumble.rs);
//!   * the CONTROL API - what each server's `server/voice.lua` posts routing
//!     decisions to (src/control.rs);
//!   * DELIVERY - one WebSocket per player, carrying vocoded 8 kHz PCM
//!     (src/stream.rs).
//!
//! Identity is always `(server, player)`. FXServer player ids are unique only
//! within one server, and treating them as global puts audio on the wrong
//! continent.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;

mod control;
mod dsp;
mod mumble;
mod platform;
mod router;
mod servers;
mod stream;
mod varint;

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

/// Enrolment and administration, before any of the runtime starts.
///
/// A server does not choose its own id: enrolment assigns one and mints a key,
/// and the key is what identifies that server from then on. Later this is the
/// web dashboard; the semantics do not change, only the front end.
fn cli(args: &[String]) -> Result<bool> {
    let path = servers::store_path();
    let mut store = servers::Store::load(&path)?;

    match args.first().map(String::as_str) {
        Some("enroll") => {
            let name = args.get(1).cloned().unwrap_or_else(|| "unnamed".into());
            let addr = args.get(2).cloned().unwrap_or_else(|| "127.0.0.1:30120".into());
            let (host, port) = match addr.rsplit_once(':') {
                Some((h, p)) => (h.to_string(), p.parse().unwrap_or(30120)),
                None => (addr.clone(), 30120u16),
            };

            let s = store.enroll(&name, &host, port)?;
            store.save(&path)?;

            println!("enrolled \"{}\" as server {}", s.name, s.id);
            println!("  tap        {}:{}", s.host, s.port);
            println!("  store      {}", path.display());
            println!();
            println!("Put this in that server's rfx_p25 config/config.lua:");
            println!();
            println!("  Config.ServerId  = {}", s.id);
            println!("  Config.ServerKey = '{}'", s.key);
            println!();
            println!("The key identifies the server. Anyone holding it can post routing");
            println!("as this server, so treat it like a password and rotate it if it leaks.");
            Ok(true)
        }

        Some("list") => {
            if store.servers.is_empty() {
                println!("no servers enrolled - run: rfx-voiced enroll <name> <host:port>");
            }
            for s in &store.servers {
                println!(
                    "{:>3}  {:<24} {}:{:<6} {}  key {}...{}",
                    s.id, s.name, s.host, s.port,
                    if s.enabled { "enabled " } else { "DISABLED" },
                    &s.key[..8], &s.key[s.key.len() - 4..]
                );
            }
            Ok(true)
        }

        Some("rotate") => {
            let id: u32 = args.get(1).and_then(|v| v.parse().ok())
                .ok_or_else(|| anyhow::anyhow!("usage: rotate <server id>"))?;
            match store.rotate(id)? {
                Some(s) => {
                    store.save(&path)?;
                    println!("server {} key rotated - the old one is now dead", s.id);
                    println!("  Config.ServerKey = '{}'", s.key);
                }
                None => println!("no server {id}"),
            }
            Ok(true)
        }

        Some("revoke") => {
            let id: u32 = args.get(1).and_then(|v| v.parse().ok())
                .ok_or_else(|| anyhow::anyhow!("usage: revoke <server id>"))?;
            if store.revoke(id) {
                store.save(&path)?;
                println!("server {id} revoked");
            } else {
                println!("no server {id}");
            }
            Ok(true)
        }

        Some("help") | Some("--help") | Some("-h") => {
            println!("rfx-voiced                       run the voice node");
            println!("rfx-voiced enroll <name> <host:port>   add a server, print its key");
            println!("rfx-voiced list                  show enrolled servers");
            println!("rfx-voiced rotate <id>           re-issue a key");
            println!("rfx-voiced revoke <id>           remove a server");
            Ok(true)
        }

        _ => Ok(false),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    mumble::install_crypto_provider()?;

    let args: Vec<String> = std::env::args().skip(1).collect();
    if cli(&args)? {
        return Ok(());
    }

    // The platform is the source of truth when it is configured. When it is
    // not - or when it is unreachable and there is no cache - the local store
    // is what runs, which is how a node works standalone.
    let plat = platform::Platform::from_env();

    let (enrolled, source) = match &plat {
        Some(p) => p.resolve().await,
        None => (platform::local_servers()?, "servers.json"),
    };

    if enrolled.is_empty() {
        anyhow::bail!(
            "no servers to serve (source: {source}). Either set VOICED_PLATFORM_URL              and VOICED_PLATFORM_KEY, or run:  rfx-voiced enroll <name> <host:port>"
        );
    }

    println!("{} server(s) from {source}", enrolled.len());

    let keys: control::SharedKeys =
        Arc::new(Mutex::new(servers::KeyIndex::build(enrolled.iter())));

    let control_addr = env_or("VOICED_CONTROL", "127.0.0.1:8787");
    // Players' game clients connect here, so unlike the control port this one
    // has to be reachable from outside the host.
    let stream_addr = env_or("VOICED_STREAM", "0.0.0.0:8788");

    let shared: control::Shared = Arc::new(Mutex::new(router::Router::default()));
    let streams: stream::SharedStreams = Arc::new(Mutex::new(stream::Streams::default()));

    {
        let shared = shared.clone();
        let streams = streams.clone();
        let keys = keys.clone();
        tokio::spawn(async move {
            if let Err(e) = control::serve(control_addr, shared, streams, keys).await {
                eprintln!("control api stopped: {e}");
            }
        });
    }

    {
        let streams = streams.clone();
        tokio::spawn(async move {
            if let Err(e) = stream::serve(stream_addr, streams).await {
                eprintln!("audio stream stopped: {e}");
            }
        });
    }

    // Telemetry: liveness for the dashboard, and the numbers behind the
    // Discord embed. Best effort throughout - a reporting failure must never
    // be able to interrupt audio.
    if let Some(p) = plat {
        let shared = shared.clone();
        // Every server we are ATTACHED to, not only the ones with activity.
        // Liveness is the point: an idle server is still online, and reporting
        // only busy ones leaves a working server reading "never seen".
        let attached: Vec<u32> = enrolled.iter().map(|s| s.id).collect();

        tokio::spawn(async move {
            // First report immediately. Waiting a full minute to say hello
            // makes a fresh start look broken.
            let mut tick = tokio::time::interval(Duration::from_secs(30));
            loop {
                tick.tick().await;

                let stats = match shared.lock() {
                    Ok(r) => r.stats(),
                    Err(_) => continue,
                };

                let rows: Vec<(u32, u32, u32, u32)> = attached
                    .iter()
                    .map(|&id| {
                        let (players, radios, talkers) = stats.get(&id).copied().unwrap_or((0, 0, 0));
                        (id, players, radios, talkers)
                    })
                    .collect();

                p.heartbeat(&rows).await;
            }
        });
    }

    let user = env_or("VOICED_USER", "[999] radiotap");
    let pass = env_or("VOICED_PASS", "");

    let mut taps = Vec::new();
    for s in enrolled {
        let settings = mumble::Settings {
            server: s.id,
            host: s.host.clone(),
            port: s.port,
            username: user.clone(),
            password: pass.clone(),
        };
        let shared = shared.clone();
        let streams = streams.clone();

        taps.push(tokio::spawn(async move {
            // Each tap reconnects on its own. One FXServer restarting must not
            // disturb the others, and on the target host nobody is watching to
            // restart this process by hand.
            loop {
                match mumble::run(&settings, shared.clone(), streams.clone()).await {
                    Ok(()) => println!("[server {}] tap closed cleanly", settings.server),
                    Err(e) => eprintln!("[server {}] tap: {e:#}", settings.server),
                }

                // Drop only THIS server's identities and routes. Everyone else
                // on the platform keeps talking.
                if let Ok(mut r) = shared.lock() {
                    r.drop_server(settings.server);
                }
                if let Ok(mut s) = streams.lock() {
                    s.drop_server(settings.server);
                }

                tokio::time::sleep(Duration::from_secs(3)).await;
            }
        }));
    }

    for tap in taps {
        let _ = tap.await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_or_falls_back() {
        assert_eq!(env_or("RFX_DEFINITELY_UNSET_VAR", "fallback"), "fallback");
    }
}
