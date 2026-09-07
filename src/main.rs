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

use std::collections::{HashMap, HashSet};
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
            let addr = args
                .get(2)
                .cloned()
                .unwrap_or_else(|| "127.0.0.1:30120".into());
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
                    s.id,
                    s.name,
                    s.host,
                    s.port,
                    if s.enabled { "enabled " } else { "DISABLED" },
                    &s.key[..8],
                    &s.key[s.key.len() - 4..]
                );
            }
            Ok(true)
        }

        Some("rotate") => {
            let id: u32 = args
                .get(1)
                .and_then(|v| v.parse().ok())
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
            let id: u32 = args
                .get(1)
                .and_then(|v| v.parse().ok())
                .ok_or_else(|| anyhow::anyhow!("usage: revoke <server id>"))?;
            if store.revoke(id) {
                store.save(&path)?;
                println!("server {id} revoked");
            } else {
                println!("no server {id}");
            }
            Ok(true)
        }

        Some("version") | Some("--version") | Some("-V") => {
            println!("{}", env!("VOICED_VERSION"));
            Ok(true)
        }

        Some("help") | Some("--help") | Some("-h") => {
            println!("rfx-voiced {}", env!("VOICED_VERSION"));
            println!();
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
    // First line of every run, so a log answers "which build is this" without
    // anyone having to infer it from behaviour.
    println!("rfx-voiced {}", env!("VOICED_VERSION"));

    mumble::install_crypto_provider()?;

    let args: Vec<String> = std::env::args().skip(1).collect();
    if cli(&args)? {
        return Ok(());
    }

    // The platform is the source of truth when it is configured. When it is
    // not - or when it is unreachable and there is no cache - the local store
    // is what runs, which is how a node works standalone.
    // Behind an Arc because two long-lived tasks need it: the heartbeat and
    // the supervisor. Platform holds only a client and config, so sharing one
    // is also the right thing - a second reqwest client would mean a second
    // connection pool to the same host.
    let plat = platform::Platform::from_env().map(Arc::new);

    let (enrolled, patches, source) = match &plat {
        Some(p) => p.resolve().await,
        None => (platform::local_servers()?, Vec::new(), "servers.json"),
    };

    // An empty list is NOT fatal. A node deployed with a valid key before any
    // server has been added is correctly configured and simply has nothing to
    // do yet; exiting would put it in a restart loop until somebody happened
    // to add a server, and the operator would see a crashing node rather than
    // an idle one. It comes up, serves nothing, and attaches when the platform
    // starts listing servers.
    println!("{} server(s) from {source}", enrolled.len());
    if enrolled.is_empty() {
        println!(
            "nothing to serve yet - waiting for servers. Add one on the dashboard, \
             or run: rfx-voiced enroll <name> <host:port>"
        );
    }

    let keys: control::SharedKeys = Arc::new(Mutex::new(servers::KeyIndex::build(enrolled.iter())));

    // The console plane authenticates with this rather than a server key. It is
    // the same secret the node presents to the platform, deliberately: the
    // platform should be the only thing that can attach a console.
    let node_key = std::env::var("VOICED_PLATFORM_KEY")
        .ok()
        .filter(|k| !k.is_empty());

    let control_addr = env_or("VOICED_CONTROL", "127.0.0.1:8787");
    // Players' game clients connect here, so unlike the control port this one
    // has to be reachable from outside the host.
    let stream_addr = env_or("VOICED_STREAM", "0.0.0.0:8788");

    let shared: control::Shared = Arc::new(Mutex::new(router::Router::default()));
    if let Ok(mut r) = shared.lock() {
        r.set_patches(patches);
    }
    let streams: stream::SharedStreams = Arc::new(Mutex::new(stream::Streams::default()));

    {
        let shared = shared.clone();
        let streams = streams.clone();
        let keys = keys.clone();
        tokio::spawn(async move {
            if let Err(e) = control::serve(control_addr, shared, streams, keys, node_key).await {
                eprintln!("control api stopped: {e}");
            }
        });
    }

    {
        let streams = streams.clone();
        let shared = shared.clone();
        tokio::spawn(async move {
            if let Err(e) = stream::serve(stream_addr, streams, shared).await {
                eprintln!("audio stream stopped: {e}");
            }
        });
    }

    // Telemetry: liveness for the dashboard, and the numbers behind the
    // Discord embed. Best effort throughout - a reporting failure must never
    // be able to interrupt audio.
    // What the node is tapping right now. Shared, because the supervisor below
    // changes it while the heartbeat is reading it.
    let attached: Arc<Mutex<HashSet<u32>>> = Arc::new(Mutex::new(HashSet::new()));

    if let Some(p) = &plat {
        let shared = shared.clone();
        let attached = attached.clone();
        let p = Arc::clone(p);

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

                // Every server we are ATTACHED to, not only the ones with
                // activity. Liveness is the point: an idle server is still
                // online, and reporting only busy ones leaves a working server
                // reading "never seen".
                let ids: Vec<u32> = match attached.lock() {
                    Ok(a) => a.iter().copied().collect(),
                    Err(_) => continue,
                };

                let rows: Vec<(u32, u32, u32, u32)> = ids
                    .iter()
                    .map(|&id| {
                        let (players, radios, talkers) =
                            stats.get(&id).copied().unwrap_or((0, 0, 0));
                        (id, players, radios, talkers)
                    })
                    .collect();

                p.heartbeat(&rows).await;
            }
        });
    }

    let user = env_or("VOICED_USER", "[999] radiotap");
    let pass = env_or("VOICED_PASS", "");

    let poll = env_or("VOICED_POLL_SECONDS", "60")
        .parse::<u64>()
        .unwrap_or(60);

    // ----------------------------------------------------------------------
    // The supervisor.
    //
    // The server list is not read once at boot. A node is deployed with a key
    // and then left alone: servers are added and removed on the dashboard
    // afterwards, and a node that only ever saw the list it started with would
    // need a restart - and therefore an operator - every time. Nothing about
    // running this should require touching the box.
    //
    // Each poll reconciles three things against the platform:
    //
    //   * the KEY INDEX, so a server added a minute ago can authenticate.
    //     Without this a new server's first call is refused with "no such
    //     route" and its radios sit dead until somebody restarts the node;
    //   * the TAPS, attaching new servers and detaching gone ones;
    //   * the ATTACHED set the heartbeat reports.
    //
    // A server whose host or port CHANGED counts as gone and then new: the tap
    // is pointed at an address that no longer serves it, and reconnecting to
    // the old one forever is the failure that looks most like working.
    // ----------------------------------------------------------------------
    {
        let shared = shared.clone();
        let streams = streams.clone();
        let keys = keys.clone();
        let attached = attached.clone();
        let plat = plat.clone();

        tokio::spawn(async move {
            let mut taps: HashMap<u32, tokio::task::JoinHandle<()>> = HashMap::new();
            let mut known: HashMap<u32, (String, u16)> = HashMap::new();

            // The first pass uses the list already fetched at startup, so the
            // node does not sit silent for a poll interval before attaching.
            let mut list = enrolled;

            loop {
                let want: HashMap<u32, (String, u16)> = list
                    .iter()
                    .map(|s| (s.id, (s.host.clone(), s.port)))
                    .collect();

                if let Ok(mut k) = keys.lock() {
                    *k = servers::KeyIndex::build(list.iter());
                }

                let stale: Vec<u32> = taps
                    .keys()
                    .copied()
                    .filter(|id| want.get(id) != known.get(id))
                    .collect();

                for id in stale {
                    if let Some(h) = taps.remove(&id) {
                        h.abort();
                    }
                    known.remove(&id);

                    // Drop only THIS server's identities and routes. Everyone
                    // else on the platform keeps talking.
                    if let Ok(mut r) = shared.lock() {
                        r.drop_server(id);
                    }
                    if let Ok(mut s) = streams.lock() {
                        s.drop_server(id);
                    }
                    if let Ok(mut a) = attached.lock() {
                        a.remove(&id);
                    }
                    println!("server {id} detached");
                }

                for (id, (host, port)) in &want {
                    if taps.contains_key(id) {
                        continue;
                    }

                    let settings = mumble::Settings {
                        server: *id,
                        host: host.clone(),
                        port: *port,
                        username: user.clone(),
                        password: pass.clone(),
                    };
                    let shared = shared.clone();
                    let streams = streams.clone();

                    let handle = tokio::spawn(async move {
                        // Each tap reconnects on its own. One FXServer
                        // restarting must not disturb the others, and on the
                        // target host nobody is watching to restart this by
                        // hand.
                        loop {
                            match mumble::run(&settings, shared.clone(), streams.clone()).await {
                                Ok(()) => {
                                    println!("[server {}] tap closed cleanly", settings.server)
                                }
                                Err(e) => eprintln!("[server {}] tap: {e:#}", settings.server),
                            }

                            if let Ok(mut r) = shared.lock() {
                                r.drop_server(settings.server);
                            }
                            if let Ok(mut s) = streams.lock() {
                                s.drop_server(settings.server);
                            }

                            tokio::time::sleep(Duration::from_secs(3)).await;
                        }
                    });

                    taps.insert(*id, handle);
                    known.insert(*id, (host.clone(), *port));
                    if let Ok(mut a) = attached.lock() {
                        a.insert(*id);
                    }
                    println!("server {id} attached ({host}:{port})");
                }

                tokio::time::sleep(Duration::from_secs(poll)).await;

                // Re-read for the next pass. A failed pull returns the cached
                // list rather than an empty one, so an unreachable dashboard
                // does not detach every server on the network.
                let (next, groups) = match &plat {
                    Some(p) => {
                        let (s, g, _) = p.resolve().await;
                        (s, g)
                    }
                    None => (platform::local_servers().unwrap_or_default(), Vec::new()),
                };
                list = next;

                // Patches are network-wide state, so they are replaced whole
                // on every poll rather than diffed - a patch that half-applied
                // is a channel joined in one direction only.
                if let Ok(mut r) = shared.lock() {
                    r.set_patches(groups);
                }
            }
        });
    }

    // The egg stops this with SIGINT, and the taps close their connections on
    // the way out. A killed node leaves every FXServer holding routes to a
    // process that is gone.
    tokio::signal::ctrl_c().await?;
    println!("shutting down");
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
