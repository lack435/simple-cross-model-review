//! Named review sessions.
//!
//! The calling agent only ever deals in names it chooses ("default", "auth-refactor").
//! We keep the mapping from that name to the reviewer CLI's own opaque session id on
//! disk, so a review session survives an MCP server restart.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Distinguishes temp files written by concurrent writers in this process.
static TMP_SEQ: AtomicU32 = AtomicU32::new(0);

/// How long to wait for another process to release the session file. Short: this only
/// guards a read-modify-write of a small JSON file.
const LOCK_WAIT: Duration = Duration::from_secs(5);

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

    /// A sibling of the state file rather than the state file itself: locking the file we
    /// are about to atomically replace would fight with the replace.
    fn lock_path(&self) -> PathBuf {
        self.path.with_extension("json.lock")
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
        // A failure to lock is returned, not ignored, and reaches the caller as a warning.
        let _file_lock = ExclusiveLock::acquire(&self.lock_path(), LOCK_WAIT)?;
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
        let _file_lock = ExclusiveLock::acquire(&self.lock_path(), LOCK_WAIT)?;
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

/// An exclusive cross-process lock, backed by the OS rather than by bookkeeping.
///
/// The in-process mutex cannot help across processes: two `cross-review` servers pointed
/// at the same project share a state directory, and every mutation is a
/// read-modify-write, so without a lock each can write back a snapshot taken before the
/// other's change and silently drop it.
///
/// Exclusivity comes from opening the lock file with a share mode of zero: while one
/// process holds that handle, every other process's open fails. Two properties fall out
/// of letting Windows own it. The lock is released when the handle closes, *including*
/// when the process dies, so there is no such thing as a stale lock to reason about. And
/// nothing ever deletes the lock file, so there is no window in which one process removes
/// a lock another has just acquired.
///
/// An earlier version tracked liveness itself: it stole any lock older than 60 seconds and
/// wrote anyway on timeout. Both were wrong. A process merely paused could have its lock
/// stolen, and on drop it would then delete the *new* owner's lock; writing anyway simply
/// reinstated the lost-update race the lock existed to prevent.
pub struct ExclusiveLock {
    // Held purely for its side effect: dropping it releases the lock.
    _file: File,
}

impl ExclusiveLock {
    /// Take the lock, retrying until `wait` elapses. Failure is returned rather than
    /// ignored, so callers surface it instead of writing unprotected.
    pub fn acquire(path: &Path, wait: Duration) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let deadline = Instant::now() + wait;
        loop {
            match OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .share_mode(0)
                .open(path)
            {
                Ok(file) => return Ok(Self { _file: file }),
                Err(e) => {
                    // Only a sharing conflict means "someone else holds it". Retrying an
                    // access-denied or bad-path error would stall for the whole wait and
                    // then report contention, telling the caller to pick a different
                    // session name -- which would fail identically.
                    if !is_sharing_conflict(&e) {
                        return Err(io::Error::new(
                            e.kind(),
                            format!("cannot open lock file {}: {e}", path.display()),
                        ));
                    }
                    if Instant::now() >= deadline {
                        return Err(io::Error::new(
                            io::ErrorKind::WouldBlock,
                            format!(
                                "another cross-review process is holding {} ({e})",
                                path.display()
                            ),
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
            }
        }
    }
}

/// Windows reports a lock held elsewhere as a sharing or lock violation. Anything else --
/// a denied ACL, a missing volume, a read-only disk -- is a real error, not contention.
fn is_sharing_conflict(e: &io::Error) -> bool {
    const ERROR_SHARING_VIOLATION: i32 = 32;
    const ERROR_LOCK_VIOLATION: i32 = 33;
    matches!(
        e.raw_os_error(),
        Some(ERROR_SHARING_VIOLATION) | Some(ERROR_LOCK_VIOLATION)
    )
}

/// Lock path for a named review session, used to stop two server processes from
/// resuming the same reviewer conversation at once.
pub fn session_lock_path(state_dir: &Path, session: &str) -> PathBuf {
    let safe: String = session
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(48)
        .collect();
    // Sanitising is lossy: `a/b` and `a?b` both flatten to `a_b`, and anything past the
    // truncation point disappears. A hash of the whole original name keeps distinct
    // sessions on distinct lock files, so they cannot spuriously report SESSION_BUSY.
    state_dir.join(format!(
        "session-{safe}-{:016x}.lock",
        crate::config::fnv1a64(session)
    ))
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
    ///
    /// Cleared before use: the name is only unique per (pid, counter), Windows recycles
    /// process ids, and an aborted run leaves its directories behind. Without the clear a
    /// later run that draws a matching pid inherits the earlier run's `sessions.json` and
    /// lock files, and the session tests assert against state they did not create.
    fn temp_dir() -> PathBuf {
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir()
            .join("cross-review-session-tests")
            .join(format!("{}-{}", std::process::id(), n));
        std::fs::remove_dir_all(&dir).ok();
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
    fn an_exclusive_lock_excludes_a_second_holder() {
        let dir = temp_dir();
        let path = dir.join("thing.lock");

        let held = ExclusiveLock::acquire(&path, Duration::from_millis(50)).expect("first acquire");
        // Windows enforces this through the share mode, so the second open fails while
        // the first handle is alive. No staleness heuristic is involved.
        let blocked = ExclusiveLock::acquire(&path, Duration::from_millis(100));
        assert!(blocked.is_err(), "a second holder must be refused");

        drop(held);
        // Releasing the handle releases the lock; the file itself is never deleted, so
        // there is no window where one process removes another's lock.
        ExclusiveLock::acquire(&path, Duration::from_millis(500)).expect("acquire after release");
        assert!(path.exists());
    }

    #[test]
    fn recording_a_turn_holds_and_releases_the_lock() {
        let dir = temp_dir();
        let store = SessionStore::new(&dir);
        record(&store, "default", "thread-1");
        // If the lock had leaked, this second write would time out.
        record(&store, "default", "thread-1");
        assert_eq!(store.get("default").unwrap().turns, 2);
    }

    #[test]
    fn a_held_session_lock_blocks_a_second_claim() {
        let dir = temp_dir();
        let path = session_lock_path(&dir, "auth-work");
        let _held = ExclusiveLock::acquire(&path, Duration::from_millis(50)).expect("acquire");
        assert!(ExclusiveLock::acquire(&path, Duration::from_millis(50)).is_err());
    }

    #[test]
    fn session_lock_paths_are_distinct_and_filename_safe() {
        let dir = temp_dir();
        let a = session_lock_path(&dir, "auth-work");
        let b = session_lock_path(&dir, "other");
        assert_ne!(a, b);

        // Names come from the calling agent, so path separators must not escape.
        let nasty = session_lock_path(&dir, "../../etc/passwd");
        let name = nasty.file_name().unwrap().to_string_lossy().to_string();
        assert!(!name.contains('/'), "{name}");
        assert!(!name.contains('\\'), "{name}");
        assert!(!name.contains(".."), "{name}");
        assert_eq!(nasty.parent(), Some(dir.as_path()));
    }

    #[test]
    fn no_temp_file_is_left_behind() {
        let dir = temp_dir();
        let store = SessionStore::new(&dir);
        record(&store, "default", "thread-1");
        // Enumerate the directory rather than probing one guessed name. The previous
        // version asserted `sessions.json.tmp` did not exist, but temp files are named
        // `sessions.<pid>.<seq>.tmp`, so it was asserting the absence of a path the code
        // never creates -- it would have passed while every temp file leaked.
        let leftovers: Vec<String> = std::fs::read_dir(&dir)
            .expect("read state dir")
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "atomic write should rename, not leave temp files: {leftovers:?}"
        );
    }
}
