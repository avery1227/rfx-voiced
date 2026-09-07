//! Call recording, with retention.
//!
//! Records the VOCODED audio, not the clean input: what is kept is what people
//! actually heard, which is the only version worth arguing about afterwards.
//! It is also already 8 kHz mono, so a call costs about 16 kB per second and a
//! busy channel-hour lands near 57 MB before compression.
//!
//! Written per call, as raw PCM with a small JSON sidecar, into a directory
//! tree by day. Raw rather than a container because the writer must never
//! block the vocoder: appending samples to an open file is a memcpy and a
//! syscall, and any encoder here would be work happening on the audio path.
//! Day directories rather than one flat pile because pruning thirty days then
//! becomes deleting a directory rather than stat-ing a hundred thousand files.
//!
//! Retention is enforced on a timer AND on startup. A node that was off for a
//! week must not come back holding five weeks of audio.

use std::fs::{self, File};
use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

/// The recorder plus whatever it is writing right now.
///
/// One lock, held briefly, shared by the tap and both transmit paths. Calls
/// are keyed by talkgroup because that is what a recording IS - one
/// transmission on one channel - and because the router already guarantees
/// only one talker holds a talkgroup at a time.
#[derive(Default)]
pub struct Active {
    pub rec: Option<Recorder>,
    pub open: HashMap<u32, Recording>,
}

pub type SharedRecorder = Arc<Mutex<Active>>;

impl Active {
    pub fn begin(&mut self, tg: u32, server: u32, player: u32, unit: &str, rate: u32) {
        let Some(r) = self.rec.as_mut() else { return };
        // A key while one is already open is a reply inside hang time, which is
        // the same call continuing.
        if self.open.contains_key(&tg) {
            return;
        }
        self.open
            .insert(tg, r.begin(tg, server, player, unit, rate));
    }

    pub fn push(&mut self, tg: u32, pcm: &[i16]) {
        if let Some(c) = self.open.get_mut(&tg) {
            c.push(pcm);
        }
    }

    pub fn end(&mut self, tg: u32) {
        if let Some(c) = self.open.remove(&tg) {
            c.finish();
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallMeta {
    pub id: String,
    /// Talkgroup, or a conventional route id.
    pub tg: u32,
    pub server: u32,
    pub player: u32,
    /// Whatever the FXServer called the unit. Empty when it said nothing.
    pub unit: String,
    pub started: u64,
    pub seconds: f32,
    pub samples: usize,
    pub rate: u32,
}

/// One call being written. Dropping it finalises the sidecar.
pub struct Recording {
    meta: CallMeta,
    file: Option<File>,
    dir: PathBuf,
}

impl Recording {
    pub fn push(&mut self, pcm: &[i16]) {
        let Some(f) = self.file.as_mut() else { return };

        // Little endian, matching the wire format and every tool that will
        // ever open this.
        let mut bytes = Vec::with_capacity(pcm.len() * 2);
        for s in pcm {
            bytes.extend_from_slice(&s.to_le_bytes());
        }

        if f.write_all(&bytes).is_err() {
            // A full disk must not take audio down with it. Stop writing this
            // call and carry on serving it.
            self.file = None;
            return;
        }
        self.meta.samples += pcm.len();
    }

    pub fn finish(mut self) -> Option<CallMeta> {
        self.file.take()?;

        self.meta.seconds = self.meta.samples as f32 / self.meta.rate as f32;

        // Calls too short to contain speech are a keyup and an immediate
        // release - a fumbled button, not a transmission. Keeping them would
        // bury the recorder in noise.
        if self.meta.seconds < 0.4 {
            let _ = fs::remove_file(self.dir.join(format!("{}.pcm", self.meta.id)));
            return None;
        }

        let json = serde_json::to_string(&self.meta).ok()?;
        let _ = fs::write(self.dir.join(format!("{}.json", self.meta.id)), json);
        Some(self.meta)
    }
}

pub struct Recorder {
    root: PathBuf,
    days: u64,
    seq: u64,
}

impl Recorder {
    /// None when recording is switched off, which is a supported way to run.
    pub fn from_env() -> Option<Self> {
        let root = std::env::var("VOICED_RECORD_DIR").ok()?;
        if root.is_empty() {
            return None;
        }

        let days = std::env::var("VOICED_RECORD_DAYS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(30);

        let root = PathBuf::from(root);
        if fs::create_dir_all(&root).is_err() {
            eprintln!(
                "recorder: cannot write to {}; recording disabled",
                root.display()
            );
            return None;
        }

        println!("recording to {}, keeping {days} days", root.display());
        let r = Self { root, days, seq: 0 };
        r.prune();
        Some(r)
    }

    fn day_dir(&self, at: u64) -> PathBuf {
        // Days since the epoch. Not a calendar date on purpose: no timezone,
        // no locale, and it sorts and compares as an integer.
        self.root.join(format!("d{}", at / 86_400))
    }

    pub fn begin(&mut self, tg: u32, server: u32, player: u32, unit: &str, rate: u32) -> Recording {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        self.seq += 1;
        let id = format!("{now}-{tg}-{}", self.seq);

        let dir = self.day_dir(now);
        let _ = fs::create_dir_all(&dir);
        let file = File::create(dir.join(format!("{id}.pcm"))).ok();

        Recording {
            meta: CallMeta {
                id,
                tg,
                server,
                player,
                unit: unit.to_string(),
                started: now,
                seconds: 0.0,
                samples: 0,
                rate,
            },
            file,
            dir,
        }
    }

    /// Deletes whole day directories past the retention window.
    pub fn prune(&self) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let cutoff = (now / 86_400).saturating_sub(self.days);

        let Ok(entries) = fs::read_dir(&self.root) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let Some(day) = name.strip_prefix('d').and_then(|d| d.parse::<u64>().ok()) else {
                continue;
            };

            if day < cutoff && fs::remove_dir_all(entry.path()).is_ok() {
                println!("recorder: pruned {name}");
            }
        }
    }

    /// Calls in the window, newest first. Reads sidecars only, never audio.
    pub fn index(&self, since: u64, limit: usize) -> Vec<CallMeta> {
        let mut out = Vec::new();
        let Ok(days) = fs::read_dir(&self.root) else {
            return out;
        };

        let mut dirs: Vec<PathBuf> = days
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        dirs.sort();
        dirs.reverse();

        for dir in dirs {
            let Ok(files) = fs::read_dir(&dir) else {
                continue;
            };
            for f in files.flatten() {
                let path = f.path();
                if path.extension().and_then(|e| e.to_str()) != Some("json") {
                    continue;
                }
                let Ok(text) = fs::read_to_string(&path) else {
                    continue;
                };
                let Ok(meta) = serde_json::from_str::<CallMeta>(&text) else {
                    continue;
                };
                if meta.started >= since {
                    out.push(meta);
                }
            }
            if out.len() >= limit {
                break;
            }
        }

        out.sort_by_key(|c| std::cmp::Reverse(c.started));
        out.truncate(limit);
        out
    }

    /// The raw PCM for one call, found by id rather than by path.
    ///
    /// The id is used to BUILD a filename, never joined as one: an id
    /// containing a slash or a `..` would otherwise read any file the process
    /// can reach.
    pub fn audio(&self, id: &str) -> Option<Vec<u8>> {
        if !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return None;
        }

        let day = id.split('-').next()?.parse::<u64>().ok()?;
        let path = self.day_dir(day).join(format!("{id}.pcm"));
        fs::read(path).ok()
    }
}
