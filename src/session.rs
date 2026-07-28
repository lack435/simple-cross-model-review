//! Named review sessions.
//!
//! The calling agent only ever deals in names it chooses ("default", "auth-refactor").
//! We keep the mapping from that name to the reviewer CLI's own opaque session id on
//! disk, so a review session survives an MCP server restart.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Distinguishes temp files written by concurrent writers in this process.
static TMP_SEQ: AtomicU32 = AtomicU32::new(0);

/// How long to wait for another process to release the session file.
const LOCK_WAIT: Duration = Duration::from_secs(5);

/// A lock older than this is assumed to belong to a process that died holding it.
const LOCK_STALE: Duration = Duration::from_secs(60);

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionRecord {
    pub reviewer: String,
    /// The reviewer CLI's session identifier: a Claude Code session uuid, or a Codex
    /// thread id. Both are stable across resumes (verified).
    pub cli_session_id: String,
    pub model: String,
    pub effort: String,
    pub cwd: String,
    pub turns: u32,
    pub created_unix: u64,
    pub updated_unix: u64,
}

#[derive(Default, Serialize, Deserialize)]
struct StoreFile {
    #[serde(default)]
    sessions: BTreeMap<String, SessionRecord>,
}

pub struct SessionStore {
    path: PathBuf,
    /// Serialises our own writes. Cross-process safety comes from re-reading the file
    /// inside the lock before every mutation, plus an atomic rename on write.
    lock: Mutex<()>,
}

impl SessionStore {
    pub fn new(state_dir: &Path) -> Self {
        Self {
            path: state_dir.join("sessions.json"),
            lock: Mutex::new(()),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn get(&self, name: &str) -> Option<SessionRecord> {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        self.read().sessions.get(name).cloned()
    }

    pub fn list(&self) -> Vec<(String, SessionRecord)> {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        self.read().sessions.into_iter().collect()
    }

    /// Record the result of a completed turn, creating the session on first use.
    pub fn record_turn(
        &self,
        name: &str,
        reviewer: &str,
        cli_session_id: &str,
        model: &str,
        effort: &str,
        cwd: &str,
    ) -> io::Result<SessionRecord> {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        // Held across the read and the write: this is a read-modify-write, so another
        // process reading between the two would write back a snapshot missing this turn.
        let _file_lock = FileLock::acquire(&self.path);
        let mut store = self.read();
        let now = now_unix();

        let record = match store.sessions.get(name) {
            // Same underlying session: another turn on it.
            Some(existing) if existing.cli_session_id == cli_session_id => SessionRecord {
                turns: existing.turns.saturating_add(1),
                created_unix: existing.created_unix,
                updated_unix: now,
                reviewer: reviewer.to_string(),
                cli_session_id: cli_session_id.to_string(),
                model: model.to_string(),
                effort: effort.to_string(),
                cwd: cwd.to_string(),
            },
            // New session, or the name was rebound to a fresh reviewer session.
            _ => SessionRecord {
                reviewer: reviewer.to_string(),
                cli_session_id: cli_session_id.to_string(),
                model: model.to_string(),
                effort: effort.to_string(),
                cwd: cwd.to_string(),
                turns: 1,
                created_unix: now,
                updated_unix: now,
            },
        };

        store.sessions.insert(name.to_string(), record.clone());
        self.write(&store)?;
        Ok(record)
    }

    pub fn forget(&self, name: &str) -> io::Result<bool> {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let _file_lock = FileLock::acquire(&self.path);
        let mut store = self.read();
        let removed = store.sessions.remove(name).is_some();
        if removed {
            self.write(&store)?;
        }
        Ok(removed)
    }

    /// A missing or corrupt store is treated as empty rather than fatal: losing the
    /// ability to resume is recoverable, refusing to start the server is not.
    fn read(&self) -> StoreFile {
        match std::fs::read_to_string(&self.path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
                eprintln!(
                    "cross-review: warning: {} is not readable as session state ({e}); \
                     starting from an empty set of sessions",
                    self.path.display()
                );
                StoreFile::default()
            }),
            Err(_) => StoreFile::default(),
        }
    }

    fn write(&self, store: &StoreFile) -> io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(store)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        // Write-then-rename so a crash mid-write cannot truncate existing state. The
        // temp name carries our pid and a counter: a shared name would let two processes
        // clobber each other's half-written file and rename the wrong bytes into place.
        let tmp = self.path.with_extension(format!(
            "{}.{}.tmp",
            std::process::id(),
            TMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&tmp, json)?;

        // std::fs::rename is MoveFileExW with MOVEFILE_REPLACE_EXISTING, which replaces
        // atomically. It can still lose to a transient sharing violation from a scanner
        // or a concurrent reader, so retry briefly. Never unlink the live file first:
        // that trades a retry for a window where the state does not exist at all.
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

/// A best-effort cross-process lock around the session file.
///
/// The in-process mutex cannot help here: two `cross-review` servers pointed at the same
/// project share a state directory, and every mutation is a read-modify-write. Without
/// this, two processes recording different sessions can each write back a snapshot taken
/// before the other's change, silently dropping it.
///
/// Best-effort by design: a lock older than `LOCK_STALE` is stolen, because a crashed
/// process must not make reviews unresumable forever, and failing to acquire does not
/// abort the write. Losing a session mapping is recoverable; refusing to work is worse.
struct FileLock {
    path: PathBuf,
    held: bool,
}

impl FileLock {
    fn acquire(target: &Path) -> Self {
        let path = target.with_extension("lock");
        let deadline = Instant::now() + LOCK_WAIT;

        loop {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(_) => return Self { path, held: true },
                Err(_) => {
                    if let Ok(meta) = std::fs::metadata(&path) {
                        let stale = meta
                            .modified()
                            .ok()
                            .and_then(|m| m.elapsed().ok())
                            .map(|age| age > LOCK_STALE)
                            .unwrap_or(false);
                        if stale {
                            // The holder died. Take it over.
                            std::fs::remove_file(&path).ok();
                            continue;
                        }
                    }
                    if Instant::now() >= deadline {
                        eprintln!(
                            "cross-review: warning: could not acquire {} within {:?}; \
                             writing session state anyway",
                            path.display(),
                            LOCK_WAIT
                        );
                        return Self { path, held: false };
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
            }
        }
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        if self.held {
            std::fs::remove_file(&self.path).ok();
        }
    }
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static SEQ: AtomicU32 = AtomicU32::new(0);

    /// A fresh directory per test so they can run in parallel.
    fn temp_dir() -> PathBuf {
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir()
            .join("cross-review-session-tests")
            .join(format!("{}-{}", std::process::id(), n));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn record(store: &SessionStore, name: &str, cli_id: &str) -> SessionRecord {
        store
            .record_turn(name, "codex", cli_id, "gpt-5.6-terra", "xhigh", "C:\\repo")
            .expect("record turn")
    }

    #[test]
    fn unknown_session_is_none() {
        let dir = temp_dir();
        let store = SessionStore::new(&dir);
        assert!(store.get("default").is_none());
        assert!(store.list().is_empty());
    }

    #[test]
    fn first_turn_creates_the_session() {
        let dir = temp_dir();
        let store = SessionStore::new(&dir);
        let rec = record(&store, "default", "thread-1");
        assert_eq!(rec.turns, 1);
        assert_eq!(rec.cli_session_id, "thread-1");
        assert_eq!(store.get("default").unwrap().turns, 1);
    }

    #[test]
    fn turns_accumulate_on_the_same_reviewer_session() {
        let dir = temp_dir();
        let store = SessionStore::new(&dir);
        record(&store, "default", "thread-1");
        record(&store, "default", "thread-1");
        let rec = record(&store, "default", "thread-1");
        assert_eq!(rec.turns, 3);
        // Creation time is preserved across turns.
        assert_eq!(rec.created_unix, store.get("default").unwrap().created_unix);
    }

    #[test]
    fn rebinding_a_name_to_a_new_reviewer_session_restarts_the_count() {
        let dir = temp_dir();
        let store = SessionStore::new(&dir);
        record(&store, "default", "thread-1");
        record(&store, "default", "thread-1");
        // A fresh review under the same name is turn 1 again, not turn 3.
        let rec = record(&store, "default", "thread-2");
        assert_eq!(rec.turns, 1);
        assert_eq!(rec.cli_session_id, "thread-2");
    }

    #[test]
    fn sessions_are_independent() {
        let dir = temp_dir();
        let store = SessionStore::new(&dir);
        record(&store, "default", "thread-1");
        record(&store, "auth-work", "thread-2");
        record(&store, "auth-work", "thread-2");
        assert_eq!(store.get("default").unwrap().turns, 1);
        assert_eq!(store.get("auth-work").unwrap().turns, 2);
        assert_eq!(store.list().len(), 2);
    }

    #[test]
    fn state_survives_a_new_store_over_the_same_directory() {
        // This is what makes a review resumable after the MCP server restarts.
        let dir = temp_dir();
        record(&SessionStore::new(&dir), "default", "thread-1");
        let reopened = SessionStore::new(&dir);
        let rec = reopened.get("default").expect("session persisted");
        assert_eq!(rec.cli_session_id, "thread-1");
        assert_eq!(rec.turns, 1);
    }

    #[test]
    fn forget_removes_only_the_named_session() {
        let dir = temp_dir();
        let store = SessionStore::new(&dir);
        record(&store, "default", "thread-1");
        record(&store, "other", "thread-2");
        assert!(store.forget("default").expect("forget"));
        assert!(store.get("default").is_none());
        assert!(store.get("other").is_some());
        // Forgetting something absent is not an error.
        assert!(!store.forget("default").expect("forget again"));
    }

    #[test]
    fn a_corrupt_store_is_treated_as_empty_rather_than_fatal() {
        // Losing the ability to resume is recoverable; refusing to start is not.
        let dir = temp_dir();
        let store = SessionStore::new(&dir);
        std::fs::write(store.path(), "{ this is not json").expect("write garbage");
        assert!(store.get("default").is_none());
        // And it recovers on the next write.
        record(&store, "default", "thread-1");
        assert_eq!(store.get("default").unwrap().cli_session_id, "thread-1");
    }

    #[test]
    fn writing_creates_missing_parent_directories() {
        let dir = temp_dir().join("nested").join("deeper");
        let store = SessionStore::new(&dir);
        record(&store, "default", "thread-1");
        assert!(store.path().is_file());
    }

    #[test]
    fn no_temp_file_is_left_behind() {
        let dir = temp_dir();
        let store = SessionStore::new(&dir);
        record(&store, "default", "thread-1");
        let leftover = store.path().with_extension("json.tmp");
        assert!(
            !leftover.exists(),
            "atomic write should rename, not leave a .tmp file"
        );
    }
}
