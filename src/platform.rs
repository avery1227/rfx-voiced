//! The platform client.
//!
//! InteropHQ is the source of truth for which servers exist and what keys they
//! present. This replaces `servers.json` - though that file is still the
//! fallback, and the only thing needed to run the node standalone.
//!
//! Three layers, in order of preference:
//!
//!   1. A live pull from the platform.
//!   2. `servers.cache.json`, the last good pull. The dashboard being down
//!      must degrade the platform - no new enrolments take effect - rather
//!      than stop it. Voice for everyone already connected keeps working.
//!   3. `servers.json`, hand-written, for a node with no platform at all.
//!
//! The node receives key HASHES, never keys. It verifies a presented key by
//! hashing it, exactly as the platform does, so a compromised node cannot
//! impersonate the servers it serves.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::servers::{Server, Store};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PlatformServer {
    pub id: u32,
    pub name: String,
    pub host: String,
    pub port: u16,
    #[serde(rename = "keyHash")]
    pub key_hash: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ServerList {
    pub servers: Vec<PlatformServer>,
}

pub struct Platform {
    base: String,
    key: String,
    cache: PathBuf,
    client: reqwest::Client,
}

impl Platform {
    /// None when the platform is not configured, which is a supported way to
    /// run rather than an error.
    pub fn from_env() -> Option<Self> {
        let base = std::env::var("VOICED_PLATFORM_URL").ok()?;
        let key = std::env::var("VOICED_PLATFORM_KEY").ok()?;
        if base.is_empty() || key.is_empty() {
            return None;
        }

        Some(Self {
            base: base.trim_end_matches('/').to_string(),
            key,
            cache: PathBuf::from(
                std::env::var("VOICED_CACHE").unwrap_or_else(|_| "servers.cache.json".into()),
            ),
            client: reqwest::Client::builder()
                // A slow dashboard must not hold up a node restart.
                .timeout(Duration::from_secs(10))
                .build()
                .expect("http client"),
        })
    }

    pub async fn fetch(&self) -> Result<ServerList> {
        let url = format!("{}/api/node/servers", self.base);
        let res = self
            .client
            .get(&url)
            .header("x-node-key", &self.key)
            .send()
            .await
            .with_context(|| format!("GET {url}"))?;

        if !res.status().is_success() {
            anyhow::bail!("platform returned HTTP {}", res.status());
        }

        let list: ServerList = res.json().await.context("decoding the server list")?;
        Ok(list)
    }

    fn write_cache(&self, list: &ServerList) {
        if let Ok(text) = serde_json::to_string_pretty(list) {
            let _ = std::fs::write(&self.cache, text);
        }
    }

    fn read_cache(&self) -> Option<ServerList> {
        let text = std::fs::read_to_string(&self.cache).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// The server list to run with, whichever layer it comes from.
    pub async fn resolve(&self) -> (Vec<Server>, &'static str) {
        match self.fetch().await {
            Ok(list) => {
                self.write_cache(&list);
                (convert(list), "platform")
            }
            Err(e) => {
                eprintln!("platform: {e:#}");
                match self.read_cache() {
                    Some(list) => (convert(list), "cache"),
                    None => (Vec::new(), "none"),
                }
            }
        }
    }

    /// Liveness and live figures, for the dashboard and the Discord bot.
    ///
    /// Best effort by design: telemetry must never be able to interrupt audio,
    /// so a failure here is logged at most and never propagated.
    pub async fn heartbeat(&self, servers: &[(u32, u32, u32, u32)]) {
        #[derive(Serialize)]
        struct Row {
            id: u32,
            players: u32,
            radios: u32,
            talkers: u32,
        }

        let body = serde_json::json!({
            "servers": servers
                .iter()
                .map(|&(id, players, radios, talkers)| Row { id, players, radios, talkers })
                .collect::<Vec<_>>(),
        });

        let _ = self
            .client
            .post(format!("{}/api/node/heartbeat", self.base))
            .header("x-node-key", &self.key)
            .json(&body)
            .send()
            .await;
    }
}

fn convert(list: ServerList) -> Vec<Server> {
    list.servers
        .into_iter()
        .map(|s| Server {
            id: s.id,
            name: s.name,
            host: s.host,
            port: s.port,
            // The platform sends a hash. Nothing here ever holds a key.
            key: String::new(),
            key_hash: Some(s.key_hash),
            enabled: true,
        })
        .collect()
}

/// Falls back to the local store when the platform is not configured.
pub fn local_servers() -> Result<Vec<Server>> {
    let path = crate::servers::store_path();
    let store = Store::load(&path)?;
    Ok(store.enabled().cloned().collect())
}
