//! Persisted last-observed usage headroom, per reviewer entry, for the proactive gate.
//!
//! The gate must decide *before* spawning a reviewer, but the headroom signal is only observed
//! *after* a turn (Codex's rollout `token_count`, Claude's `rate_limit_event`). This store is the
//! bridge: every armed turn writes its observation here, and the next fresh review reads it before
//! deciding whether to skip an entry. See `docs/usage-remaining-gate.md`.
//!
//! Cross-process safety mirrors [`crate::session::SessionStore`] exactly — an [`ExclusiveLock`]
//! held across a read-modify-write, an atomic temp+rename, corrupt-file preservation, and
//! stale-write rejection — because a shared `--state-dir` means two servers can race here too.
//!
//! Keyed by **resolved binary + account fingerprint**, not raw config: the same binary can be
//! re-authenticated as a different account, so binding an observation to the account it was
//! recorded under is what stops a snapshot crossing an account switch. A key that cannot include
//! a current account fingerprint is never formed, so such an entry is simply never gated
//! (fail-open).

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::reviewer::Headroom;
use crate::session::ExclusiveLock;

/// How long the in-process/cross-process lock is waited for before giving up (matches the
/// session store). A failure to lock degrades to a no-op, never an unprotected write.
const LOCK_WAIT: Duration = Duration::from_secs(5);

/// Backstop staleness bound for an observation whose window carries no `resets_at`: past this
/// age the observation is treated as `Unknown` regardless. A conservative cap set to the largest
/// window either provider is known to use (a 7-day Codex window); a precise `resets_at`, when
/// present, bounds it more tightly still.
const DEFAULT_TTL_SECS: u64 = 7 * 24 * 60 * 60;

static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Serialize, Deserialize, Clone, Debug)]
struct Observation {
    headroom: Headroom,
    observed_at: u64,
}

#[derive(Serialize, Deserialize, Default)]
struct StoreFile {
    /// Keyed by [`entry_key`]. A `BTreeMap` for stable, diffable serialization.
    entries: BTreeMap<String, Observation>,
}

/// The store key for a reviewer entry: its family, resolved binary, and current account
/// fingerprint. All three must be known to form a key; a caller that cannot establish the
/// account fingerprint passes `None` and gets no key, so the entry is never gated.
pub fn entry_key(reviewer: &str, resolved_bin: &Path, account: &str) -> String {
    // Windows paths are case-insensitive; lower-case so the same executable keys consistently.
    let bin = resolved_bin
        .to_string_lossy()
        .to_lowercase()
        .replace('\\', "/");
    format!("{reviewer}\u{1f}{bin}\u{1f}{account}")
}

pub struct HeadroomStore {
    path: PathBuf,
    lock: Mutex<()>,
}

impl HeadroomStore {
    pub fn new(state_dir: &Path) -> Self {
        Self {
            path: state_dir.join("usage-headroom.json"),
            lock: Mutex::new(()),
        }
    }

    fn lock_path(&self) -> PathBuf {
        self.path.with_extension("json.lock")
    }

    /// The last-observed headroom for `key`, or `Headroom::Unknown` when there is none, when it
    /// has aged past its `resets_at` or the backstop TTL, or when the store cannot be read. Never
    /// errors: an unreadable store simply means "no usable observation", i.e. fail-open.
    pub fn get(&self, key: &str, now: u64) -> Headroom {
        let store = match self.read_or_corrupt() {
            Ok(s) => s,
            Err(_) => return Headroom::Unknown,
        };
        match store.entries.get(key) {
            Some(obs) if is_actionable(obs, now) => obs.headroom,
            _ => Headroom::Unknown,
        }
    }

    /// Record `headroom` for `key`, observed at `now`. Best-effort: a lock, read, or write error
    /// is logged and dropped rather than propagated — a lost observation only means the next
    /// review is ungated for this entry, never a wrong gate. Stale-write rejection keeps a
    /// newer observation from being overwritten by an older one racing behind it.
    pub fn record(&self, key: &str, headroom: Headroom, now: u64) {
        if let Err(e) = self.record_inner(key, headroom, now) {
            eprintln!("cross-review: warning: could not record usage headroom: {e}");
        }
    }

    fn record_inner(&self, key: &str, headroom: Headroom, now: u64) -> io::Result<()> {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        // Held across the read and the write: this is a read-modify-write, so another process
        // reading between the two would write back a snapshot missing this observation.
        let _file_lock = ExclusiveLock::acquire(&self.lock_path(), LOCK_WAIT)?;
        let mut store = self.read_or_corrupt()?;
        match store.entries.get(key) {
            // Stale-write rejection: never regress to an older observation.
            Some(existing) if existing.observed_at > now => {}
            _ => {
                store.entries.insert(
                    key.to_string(),
                    Observation {
                        headroom,
                        observed_at: now,
                    },
                );
            }
        }
        self.write(&store)
    }

    fn read_or_corrupt(&self) -> io::Result<StoreFile> {
        match std::fs::read_to_string(&self.path) {
            // Only a genuinely missing file is an empty store. An existing empty/whitespace file
            // (never produced by `write`) or unparseable content is fail-closed so a mutator
            // refuses rather than overwriting whatever is really there.
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(StoreFile::default()),
            Err(e) => Err(e),
            Ok(text) if text.trim().is_empty() => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "{} is present but empty; refusing to overwrite it",
                    self.path.display()
                ),
            )),
            Ok(text) => serde_json::from_str(&text).map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{} is corrupt (did not parse: {e})", self.path.display()),
                )
            }),
        }
    }

    fn write(&self, store: &StoreFile) -> io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(store)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        // Write-then-rename so a crash mid-write cannot truncate existing state; a pid+counter
        // temp name keeps two processes from clobbering each other's half-written file.
        let tmp = self.path.with_extension(format!(
            "{}.{}.tmp",
            std::process::id(),
            TMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&tmp, json)?;
        let mut last = None;
        for attempt in 0..10 {
            match std::fs::rename(&tmp, &self.path) {
                Ok(()) => return Ok(()),
                Err(e) => {
                    last = Some(e);
                    std::thread::sleep(Duration::from_millis(20 * (attempt + 1)));
                }
            }
        }
        std::fs::remove_file(&tmp).ok();
        Err(last.unwrap_or_else(|| io::Error::other("rename failed")))
    }
}

/// Whether an observation is still actionable: within its window's own reset (when it carried
/// one) *and* within the backstop TTL. A missing `resets_at` therefore still expires via the TTL
/// rather than gating indefinitely.
fn is_actionable(obs: &Observation, now: u64) -> bool {
    if now.saturating_sub(obs.observed_at) >= DEFAULT_TTL_SECS {
        return false;
    }
    match obs.headroom.resets_at() {
        Some(reset) => now < reset,
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reviewer::HeadroomLevel;

    fn store() -> (HeadroomStore, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "cr-usage-{}-{}",
            std::process::id(),
            TMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        (HeadroomStore::new(&dir), dir)
    }

    #[test]
    fn record_then_get_round_trips() {
        let (s, dir) = store();
        let now = 1_000_000;
        let h = Headroom::Level {
            level: HeadroomLevel::Warning,
            resets_at: Some(now + 3600),
        };
        s.record("k", h, now);
        assert_eq!(s.get("k", now + 10), h);
        assert_eq!(s.get("other", now + 10), Headroom::Unknown);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn past_reset_and_past_ttl_are_unknown() {
        let (s, dir) = store();
        let now = 2_000_000;
        // Past its own reset.
        s.record(
            "reset",
            Headroom::Fraction {
                remaining_pct: 5.0,
                resets_at: Some(now + 100),
            },
            now,
        );
        assert_eq!(s.get("reset", now + 200), Headroom::Unknown);
        assert!(matches!(
            s.get("reset", now + 50),
            Headroom::Fraction { .. }
        ));
        // No reset, but past the backstop TTL.
        s.record(
            "noreset",
            Headroom::Fraction {
                remaining_pct: 5.0,
                resets_at: None,
            },
            now,
        );
        assert!(matches!(
            s.get("noreset", now + 10),
            Headroom::Fraction { .. }
        ));
        assert_eq!(s.get("noreset", now + DEFAULT_TTL_SECS), Headroom::Unknown);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_stale_write_does_not_regress_a_newer_observation() {
        let (s, dir) = store();
        let newer = Headroom::Fraction {
            remaining_pct: 50.0,
            resets_at: None,
        };
        s.record("k", newer, 100);
        // An older observation racing in behind it must not overwrite.
        s.record(
            "k",
            Headroom::Fraction {
                remaining_pct: 1.0,
                resets_at: None,
            },
            90,
        );
        assert_eq!(s.get("k", 110), newer);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_corrupt_store_reads_as_unknown_and_is_not_overwritten_by_get() {
        let (s, dir) = store();
        std::fs::write(&s.path, "not json").unwrap();
        assert_eq!(s.get("k", 1), Headroom::Unknown);
        // The corrupt file is preserved (get never writes).
        assert_eq!(std::fs::read_to_string(&s.path).unwrap(), "not json");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn different_accounts_key_separately() {
        let bin = Path::new("C:/x/codex.exe");
        let a = entry_key("codex", bin, "acct-A");
        let b = entry_key("codex", bin, "acct-B");
        assert_ne!(a, b);
        // Case-insensitive on the bin path (Windows).
        assert_eq!(
            entry_key("codex", Path::new("C:/X/Codex.exe"), "acct-A"),
            entry_key("codex", Path::new("c:/x/codex.exe"), "acct-A"),
        );
    }
}
