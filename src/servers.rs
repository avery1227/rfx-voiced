//! Enrolled servers, and the keys that identify them.
//!
//! A server does not get to say which server it is. It presents a key, and the
//! key determines the id. That distinction is the whole point: without it, any
//! process that can reach the control port could post as server 3 and steal or
//! poison another world's audio routing.
//!
//! Enrolment happens out of band - a CLI command now, the web dashboard later
//! - and the operator carries the key to that FXServer's `Config.ServerKey`.
//!
//! The store is a plain JSON file so it can be inspected, backed up, and
//! edited when something has gone wrong at three in the morning.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Server {
    pub id: u32,
    pub name: String,
    /// Where the tap connects. This is the FXServer's game port, because that
    /// is where its voice server listens.
    pub host: String,
    pub port: u16,
    /// The shared secret, for locally enrolled servers. Empty when the server
    /// came from the platform, which sends a hash and never a key.
    #[serde(default)]
    pub key: String,
    /// SHA-256 of the key, as the platform stores it. Preferred when present:
    /// a node that never holds a key cannot leak one.
    #[serde(default)]
    pub key_hash: Option<String>,
    #[serde(default = "yes")]
    pub enabled: bool,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Store {
    #[serde(default)]
    pub servers: Vec<Server>,
}

pub fn store_path() -> PathBuf {
    PathBuf::from(std::env::var("VOICED_STORE").unwrap_or_else(|_| "servers.json".into()))
}

impl Store {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let text = serde_json::to_string_pretty(self)?;
        std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))
    }

    fn next_id(&self) -> u32 {
        self.servers.iter().map(|s| s.id).max().unwrap_or(0) + 1
    }

    pub fn enroll(&mut self, name: &str, host: &str, port: u16) -> Result<Server> {
        let server = Server {
            id: self.next_id(),
            name: name.to_string(),
            host: host.to_string(),
            port,
            key: mint_key()?,
            key_hash: None,
            enabled: true,
        };
        self.servers.push(server.clone());
        Ok(server)
    }

    pub fn revoke(&mut self, id: u32) -> bool {
        let before = self.servers.len();
        self.servers.retain(|s| s.id != id);
        self.servers.len() != before
    }

    /// Re-issues a key for a server, invalidating the old one. For when a key
    /// has leaked, which is the only reason to do this.
    pub fn rotate(&mut self, id: u32) -> Result<Option<Server>> {
        let key = mint_key()?;
        for s in self.servers.iter_mut() {
            if s.id == id {
                s.key = key;
                return Ok(Some(s.clone()));
            }
        }
        Ok(None)
    }

    pub fn enabled(&self) -> impl Iterator<Item = &Server> {
        self.servers.iter().filter(|s| s.enabled)
    }

}

#[derive(Debug, Default, Clone)]
pub struct KeyIndex {
    /// Hashes, not keys. A server enrolled locally has its key hashed on the
    /// way in, so this table looks the same either way and the node never
    /// holds a secret it does not need.
    by_hash: HashMap<String, u32>,
}

impl KeyIndex {
    pub fn build<'a>(servers: impl Iterator<Item = &'a Server>) -> Self {
        let mut by_hash = HashMap::new();
        for s in servers {
            let hash = match &s.key_hash {
                Some(h) => h.to_lowercase(),
                None if !s.key.is_empty() => sha256_hex(&s.key),
                None => continue,
            };
            by_hash.insert(hash, s.id);
        }
        Self { by_hash }
    }

    /// Resolves a presented key to the server it identifies.
    ///
    /// Compared in constant time. The window is small and the payoff for an
    /// attacker is large - being able to post routing for another world - so
    /// it is not worth being clever about.
    pub fn resolve(&self, presented: &str) -> Option<u32> {
        let presented = sha256_hex(presented);
        let mut found = None;
        for (hash, id) in &self.by_hash {
            if constant_time_eq(hash.as_bytes(), presented.as_bytes()) {
                found = Some(*id);
            }
        }
        found
    }

}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn sha256_hex(input: &str) -> String {
    use ring::digest;
    let d = digest::digest(&digest::SHA256, input.as_bytes());
    d.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

/// 256 bits of system randomness, hex encoded. `ring` is already in the tree
/// via rustls, so this costs no new dependency.
fn mint_key() -> Result<String> {
    use ring::rand::SecureRandom;
    let rng = ring::rand::SystemRandom::new();
    let mut bytes = [0u8; 32];
    rng.fill(&mut bytes)
        .map_err(|_| anyhow::anyhow!("system randomness unavailable"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_unique_and_long_enough_to_matter() {
        let mut store = Store::default();
        let a = store.enroll("A", "127.0.0.1", 30120).unwrap();
        let b = store.enroll("B", "127.0.0.1", 30121).unwrap();

        assert_eq!(a.id, 1);
        assert_eq!(b.id, 2, "ids are assigned, never chosen by the server");
        assert_ne!(a.key, b.key);
        assert_eq!(a.key.len(), 64, "256 bits, hex");
        assert!(a.key_hash.is_none(), "a locally enrolled server holds the key itself");
    }

    #[test]
    fn a_key_resolves_to_exactly_one_server() {
        let mut store = Store::default();
        let a = store.enroll("A", "h", 1).unwrap();
        let b = store.enroll("B", "h", 2).unwrap();

        let idx = KeyIndex::build(store.enabled());
        assert_eq!(idx.resolve(&a.key), Some(a.id));
        assert_eq!(idx.resolve(&b.key), Some(b.id));
        assert_eq!(idx.resolve("not a key"), None);
        assert_eq!(idx.resolve(""), None);
    }

    #[test]
    fn a_revoked_key_stops_working() {
        let mut store = Store::default();
        let a = store.enroll("A", "h", 1).unwrap();
        assert!(store.revoke(a.id));
        assert_eq!(KeyIndex::build(store.enabled()).resolve(&a.key), None);
    }

    #[test]
    fn rotating_invalidates_the_old_key() {
        let mut store = Store::default();
        let a = store.enroll("A", "h", 1).unwrap();
        let rotated = store.rotate(a.id).unwrap().expect("server exists");

        assert_eq!(rotated.id, a.id, "same server");
        assert_ne!(rotated.key, a.key);

        let idx = KeyIndex::build(store.enabled());
        assert_eq!(idx.resolve(&a.key), None, "the leaked key is dead");
        assert_eq!(idx.resolve(&rotated.key), Some(a.id));
    }

    #[test]
    fn a_disabled_server_cannot_authenticate() {
        let mut store = Store::default();
        let a = store.enroll("A", "h", 1).unwrap();
        store.servers[0].enabled = false;
        assert_eq!(KeyIndex::build(store.enabled()).resolve(&a.key), None);
    }
}
