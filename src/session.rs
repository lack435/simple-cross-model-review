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

use crate::vcs::baseline::{CaptureIdentity, PerforceBaseline};

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
    /// The last cumulative usage this session's reviewer reported, when it reports
    /// cumulatively at all.
    ///
    /// Codex reports the whole thread's running total on every turn, and the per-turn
    /// figure is nowhere in its event stream -- so the only way to get one is to subtract
    /// the previous total, which means remembering it. Absent for Claude, which reports
    /// per turn already, and absent on a session recorded before this field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cumulative_usage: Option<crate::metrics::Usage>,
    /// The Perforce changelist set this session is bound to, canonicalised (sorted, deduped).
    ///
    /// A review session follows one changelist set: a resume that names a different set is
    /// refused rather than silently continuing against work the reviewer never saw. `None` is
    /// a git session (no binding) or a session recorded before this field existed, both of
    /// which the resume check treats as unbound. The pending changelists' *contents* may move
    /// between turns -- that is expected in a re-review and is handled by re-capturing, not by
    /// this field, which is identity only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changes: Option<Vec<u64>>,
    /// The git commit (`HEAD`) this session's last turn captured, when the backend is git and
    /// HEAD could be resolved.
    ///
    /// The next turn reviews only what changed since it (`<head_sha>..HEAD`) instead of the
    /// whole configured range, so the reviewer -- which already holds the earlier full diff in
    /// its resumed conversation -- is not re-sent a near-duplicate every turn. Advanced to the
    /// latest captured HEAD on every turn, unlike `changes`, which is an invariant binding.
    /// `None` for a Perforce session, a session recorded before this field existed, or a turn
    /// where HEAD could not be resolved (no commits yet, detached, git unavailable).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_sha: Option<String>,
    /// The resolved effective base of the range `head_sha` was captured under. The next turn
    /// deltas against `head_sha` only when its own range resolves to this same base; otherwise
    /// the base ref moved (or the mode changed) and a full capture is taken instead. Paired
    /// with `head_sha`: the two advance together on a complete capture and are retained
    /// together otherwise, so a `head` never sits beside a `base` from a different turn. `None`
    /// for Perforce or a record predating this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_sha: Option<String>,
    /// Which capture backend this session belongs to (`"git"` or `"perforce"`), so a resume
    /// cannot cross backends: a git record must never satisfy the Perforce-only binding logic,
    /// nor vice versa. `None` for a record written before this field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    /// Whether the last Perforce turn was capturing shelved content. Part of the resume
    /// binding: a session that showed a shelf and then omits it (or the reverse) must not
    /// silently leave stale evidence in scope, so a change here refuses the incremental resume.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_shelved: Option<bool>,
    /// The resolved Perforce capture identity (server, client, charset, client-spec digest) the
    /// last turn ran under. A resume whose identity differs re-captures in full. `None` for git
    /// or a record predating this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture_identity: Option<CaptureIdentity>,
    /// The Perforce resume-delta baseline the next turn collapses against: the last *persisted*
    /// turn's per-file inventory (`Full`) or `Disabled` when that turn was incomplete. Written
    /// explicitly every Perforce turn -- never inherited from a prior turn -- so a stale
    /// inventory can never be eluded against. `None` for git or a record predating this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub perforce_baseline: Option<PerforceBaseline>,
}

/// What a completed turn contributes to its session's record.
///
/// A struct rather than a parameter list: six of these in a row, five of them `&str`,
/// is a call site where two arguments can be transposed without the compiler noticing.
pub struct TurnFacts<'a> {
    pub reviewer: &'a str,
    /// The reviewer CLI's own session identifier for this conversation.
    pub cli_session_id: &'a str,
    pub model: &'a str,
    pub effort: &'a str,
    pub cwd: &'a str,
    /// The running total this turn reported, for reviewers that report cumulatively.
    pub cumulative_usage: Option<crate::metrics::Usage>,
    /// The canonical Perforce changelist set this session is bound to, or `None` for git.
    pub changes: Option<Vec<u64>>,
    /// The git HEAD this turn captured, so the next turn can review only what changed since
    /// it, and the resolved effective base of the range it was captured under. Both `None`
    /// together for Perforce, an unresolved HEAD, or a truncated capture; a resume only deltas
    /// when it has both.
    pub head_sha: Option<String>,
    pub base_sha: Option<String>,
    /// The capture backend (`"git"`/`"perforce"`), the shelved-capture flag, the resolved
    /// capture identity, and the Perforce delta baseline this turn produced. All `None` for a
    /// git turn; a Perforce turn always supplies `backend`, `include_shelved`, `identity` and a
    /// `perforce_baseline` (`Full` or `Disabled`).
    pub backend: Option<&'a str>,
    pub include_shelved: Option<bool>,
    pub capture_identity: Option<CaptureIdentity>,
    pub perforce_baseline: Option<PerforceBaseline>,
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
    pub fn record_turn(&self, name: &str, turn: TurnFacts) -> io::Result<SessionRecord> {
        let TurnFacts {
            reviewer,
            cli_session_id,
            model,
            effort,
            cwd,
            cumulative_usage,
            changes,
            head_sha,
            base_sha,
            backend,
            include_shelved,
            capture_identity,
            perforce_baseline,
        } = turn;
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        // Held across the read and the write: this is a read-modify-write, so another
        // process reading between the two would write back a snapshot missing this turn.
        // A failure to lock is returned, not ignored, and reaches the caller as a warning.
        let _file_lock = ExclusiveLock::acquire(&self.lock_path(), LOCK_WAIT)?;
        let mut store = self.read();
        let now = now_unix();

        // The baseline is a (head, base) pair the next turn deltas from, so it advances as a
        // unit: a turn that produced a complete pair replaces it; any incomplete one (a
        // truncated capture, an unresolved HEAD, a Perforce turn) retains the prior pair intact
        // rather than storing a half of one. The prior pair is only inherited from the *same*
        // conversation -- a fresh or rebound reviewer session (a different `cli_session_id`
        // under this name) never saw the old diff, so an incomplete first turn there must store
        // nothing rather than a baseline the new reviewer cannot resume against.
        let prior = store
            .sessions
            .get(name)
            .filter(|p| p.cli_session_id == cli_session_id);
        let (head_sha, base_sha) = match (&head_sha, &base_sha) {
            (Some(_), Some(_)) => (head_sha, base_sha),
            _ => (
                prior.and_then(|p| p.head_sha.clone()),
                prior.and_then(|p| p.base_sha.clone()),
            ),
        };

        let record = match store.sessions.get(name) {
            // Same underlying session: another turn on it. The changelist binding is
            // invariant across turns (a mismatched resume is refused before we get here), so
            // carrying this turn's value forward keeps it while tolerating a `None` from a
            // git turn without erasing an existing binding.
            Some(existing) if existing.cli_session_id == cli_session_id => SessionRecord {
                turns: existing.turns.saturating_add(1),
                created_unix: existing.created_unix,
                updated_unix: now,
                reviewer: reviewer.to_string(),
                cli_session_id: cli_session_id.to_string(),
                model: model.to_string(),
                effort: effort.to_string(),
                cwd: cwd.to_string(),
                cumulative_usage,
                changes: changes.or(existing.changes.clone()),
                // The (head, base) baseline was already resolved above as a unit -- this turn's
                // complete pair, or the prior one retained -- so it is stored as-is here.
                head_sha,
                base_sha,
                // Backend, shelved flag and capture identity are the binding: stable across a
                // session, so carry this turn's value while tolerating a `None` from a turn that
                // did not supply one rather than erasing what is bound.
                backend: backend.map(str::to_string).or(existing.backend.clone()),
                include_shelved: include_shelved.or(existing.include_shelved),
                capture_identity: capture_identity.or(existing.capture_identity.clone()),
                // The delta baseline, unlike the binding, is *this* turn's alone: it reflects
                // exactly what the reviewer was last shown, so it is stored directly and never
                // inherited -- a stale inventory must never be eluded against.
                perforce_baseline,
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
                cumulative_usage,
                changes,
                head_sha,
                base_sha,
                backend: backend.map(str::to_string),
                include_shelved,
                capture_identity,
                perforce_baseline,
            },
        };

        store.sessions.insert(name.to_string(), record.clone());
        self.write(&store)?;
        Ok(record)
    }

    /// The path of a session's "turn in progress" marker. A sibling of the state file, keyed by
    /// name the same way the session lock is, so it survives a crash independently of the JSON.
    fn pending_marker(&self, name: &str) -> PathBuf {
        let dir = self.path.parent().unwrap_or_else(|| Path::new("."));
        let safe: String = name
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
        dir.join(format!(
            "session-{safe}-{:016x}.pending",
            crate::config::fnv1a64(name)
        ))
    }

    /// Mark a Perforce turn as in progress. The marker is written *before* the turn does anything
    /// that could deliver a review without persisting its baseline, and cleared only once the turn
    /// is durably recorded. A crash, panic, or write failure in between therefore leaves it set,
    /// and [`is_pending`](Self::is_pending) tells the next resume to fall back to a full capture
    /// rather than collapse against a baseline that never advanced. Best effort: if even this
    /// small write fails there is nothing more the process can durably do.
    pub fn mark_pending(&self, name: &str) {
        let path = self.pending_marker(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        std::fs::write(&path, b"").ok();
    }

    /// Clear the in-progress marker after a turn is durably recorded.
    pub fn clear_pending(&self, name: &str) {
        std::fs::remove_file(self.pending_marker(name)).ok();
    }

    /// Whether the previous turn of this session left an uncleared in-progress marker -- it
    /// crashed, panicked, or failed to persist. Elision must be disabled until a clean turn
    /// clears it.
    pub fn is_pending(&self, name: &str) -> bool {
        self.pending_marker(name).exists()
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
    use crate::testutil::TempDir;

    /// A fresh directory per test so they can run in parallel. See `crate::testutil` for
    /// why it is both cleared on the way in and removed on the way out.
    fn temp_dir() -> TempDir {
        crate::testutil::temp_dir("cross-review-session-tests")
    }

    fn record(store: &SessionStore, name: &str, cli_id: &str) -> SessionRecord {
        store
            .record_turn(
                name,
                TurnFacts {
                    reviewer: "codex",
                    cli_session_id: cli_id,
                    model: "gpt-5.6-luna",
                    effort: "max",
                    cwd: "C:\\repo",
                    cumulative_usage: None,
                    changes: None,
                    head_sha: None,
                    base_sha: None,
                    backend: None,
                    include_shelved: None,
                    capture_identity: None,
                    perforce_baseline: None,
                },
            )
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
    fn a_recorded_baseline_survives_and_advances_across_turns() {
        // The cumulative baseline a turn stores is what the next turn reads back to
        // difference against. A turn that kept the mapping but never advanced this -- the
        // old id-less-resume path -- left the next turn subtracting against a stale total.
        let dir = temp_dir();
        let store = SessionStore::new(&dir);
        let turn_one = crate::metrics::Usage {
            input_tokens: Some(190_000),
            ..Default::default()
        };
        let turn_two = crate::metrics::Usage {
            input_tokens: Some(250_000),
            ..Default::default()
        };
        let facts = |usage| TurnFacts {
            reviewer: "codex",
            cli_session_id: "thread-1",
            model: "gpt-5.6-luna",
            effort: "max",
            cwd: "C:\\repo",
            cumulative_usage: Some(usage),
            changes: None,
            head_sha: None,
            base_sha: None,
            backend: None,
            include_shelved: None,
            capture_identity: None,
            perforce_baseline: None,
        };
        store
            .record_turn("default", facts(turn_one))
            .expect("turn 1");
        assert_eq!(
            store.get("default").unwrap().cumulative_usage,
            Some(turn_one)
        );
        store
            .record_turn("default", facts(turn_two))
            .expect("turn 2");
        let rec = store.get("default").unwrap();
        assert_eq!(rec.turns, 2);
        assert_eq!(
            rec.cumulative_usage,
            Some(turn_two),
            "the baseline advances to the latest running total"
        );
    }

    #[test]
    fn the_baseline_pair_advances_together_and_survives_an_incomplete_turn_intact() {
        // The next resume deltas from a (head, base) pair, so it advances as a unit: a complete
        // turn replaces it, an incomplete one (a truncated capture, an unresolved HEAD, a
        // Perforce turn -- all arriving as a `None` half) retains the prior pair rather than
        // erasing it or storing a head beside a base from a different turn.
        let dir = temp_dir();
        let store = SessionStore::new(&dir);
        let facts = |head: Option<&str>, base: Option<&str>| TurnFacts {
            reviewer: "claude",
            cli_session_id: "sid-1",
            model: "claude-opus-4-8",
            effort: "medium",
            cwd: "C:\\repo",
            cumulative_usage: None,
            changes: None,
            head_sha: head.map(str::to_string),
            base_sha: base.map(str::to_string),
            backend: None,
            include_shelved: None,
            capture_identity: None,
            perforce_baseline: None,
        };
        store
            .record_turn("g", facts(Some("aaa1"), Some("base0")))
            .expect("turn 1");
        let rec = store.get("g").unwrap();
        assert_eq!(rec.head_sha.as_deref(), Some("aaa1"));
        assert_eq!(rec.base_sha.as_deref(), Some("base0"));
        // A later complete turn advances both halves together.
        store
            .record_turn("g", facts(Some("bbb2"), Some("base0")))
            .expect("turn 2");
        let rec = store.get("g").unwrap();
        assert_eq!(rec.head_sha.as_deref(), Some("bbb2"));
        assert_eq!(rec.base_sha.as_deref(), Some("base0"));
        // An incomplete turn (here a resolved HEAD but no base -- a partial pair) must not
        // store half a baseline: the prior complete pair is retained intact.
        store
            .record_turn("g", facts(Some("ccc3"), None))
            .expect("turn 3");
        let rec = store.get("g").unwrap();
        assert_eq!(
            rec.head_sha.as_deref(),
            Some("bbb2"),
            "head retained, not ccc3"
        );
        assert_eq!(rec.base_sha.as_deref(), Some("base0"));
    }

    #[test]
    fn a_rebound_session_does_not_inherit_the_prior_conversations_baseline() {
        // A fresh review under an existing name is a different conversation. An incomplete
        // first turn there -- a truncated capture, say -- must not resume against the old
        // reviewer's baseline, which the new reviewer never saw. The prior pair is inherited
        // only within the same `cli_session_id`.
        let dir = temp_dir();
        let store = SessionStore::new(&dir);
        let facts = |cli: &'static str, head: Option<&str>, base: Option<&str>| TurnFacts {
            reviewer: "claude",
            cli_session_id: cli,
            model: "claude-opus-4-8",
            effort: "medium",
            cwd: "C:\\repo",
            cumulative_usage: None,
            changes: None,
            head_sha: head.map(str::to_string),
            base_sha: base.map(str::to_string),
            backend: None,
            include_shelved: None,
            capture_identity: None,
            perforce_baseline: None,
        };
        // The old conversation establishes a complete baseline.
        store
            .record_turn("s", facts("old", Some("h1"), Some("b1")))
            .expect("old turn");
        // Rebound to a new conversation whose first turn is incomplete (e.g. truncated).
        let rec = store
            .record_turn("s", facts("new", Some("h2"), None))
            .expect("rebound turn");
        assert_eq!(rec.turns, 1, "a rebound restarts the turn count");
        assert_eq!(rec.cli_session_id, "new");
        assert!(
            rec.head_sha.is_none() && rec.base_sha.is_none(),
            "an incomplete first turn of a new conversation must not inherit the old baseline"
        );
    }

    #[test]
    fn a_changelist_binding_persists_and_survives_a_none_turn() {
        let dir = temp_dir();
        let store = SessionStore::new(&dir);
        let facts = |changes| TurnFacts {
            reviewer: "codex",
            cli_session_id: "thread-1",
            model: "gpt-5.6-luna",
            effort: "max",
            cwd: "C:\\repo",
            cumulative_usage: None,
            changes,
            head_sha: None,
            base_sha: None,
            backend: None,
            include_shelved: None,
            capture_identity: None,
            perforce_baseline: None,
        };
        store
            .record_turn("p4", facts(Some(vec![43650, 43651])))
            .expect("turn 1");
        assert_eq!(store.get("p4").unwrap().changes, Some(vec![43650, 43651]));
        // A later turn that carried no binding must not erase the one already stored.
        store.record_turn("p4", facts(None)).expect("turn 2");
        assert_eq!(store.get("p4").unwrap().changes, Some(vec![43650, 43651]));
    }

    #[test]
    fn the_perforce_baseline_is_this_turns_value_and_is_never_inherited() {
        // Unlike the (head, base) pair, the delta baseline reflects exactly what the reviewer
        // was last shown, so every turn overwrites it: a `Full` inventory on one turn must not
        // survive a later `Disabled` turn, or the next turn would elide against a stale set.
        use crate::vcs::baseline::{
            Basis, InventoryEntry, PerforceBaseline, UnitKind, INVENTORY_SCHEMA,
        };
        let dir = temp_dir();
        let store = SessionStore::new(&dir);
        let facts = |baseline: Option<PerforceBaseline>| TurnFacts {
            reviewer: "codex",
            cli_session_id: "thread-1",
            model: "gpt-5.6-luna",
            effort: "max",
            cwd: "C:\\repo",
            cumulative_usage: None,
            changes: Some(vec![42]),
            head_sha: None,
            base_sha: None,
            backend: Some("perforce"),
            include_shelved: Some(false),
            capture_identity: None,
            perforce_baseline: baseline,
        };
        let full = PerforceBaseline::Full {
            schema: INVENTORY_SCHEMA,
            entries: vec![InventoryEntry {
                change: 42,
                basis: Basis::Workspace,
                kind: UnitKind::TextDiff,
                depot: "//depot/a".into(),
                comparator: "3".into(),
                fingerprint: crate::digest::Fingerprint::of(b"x"),
            }],
        };
        store
            .record_turn("p4", facts(Some(full.clone())))
            .expect("turn 1");
        assert_eq!(store.get("p4").unwrap().perforce_baseline, Some(full));
        // A later incomplete turn records `Disabled` -- the prior `Full` must not linger.
        store
            .record_turn("p4", facts(Some(PerforceBaseline::Disabled)))
            .expect("turn 2");
        assert_eq!(
            store.get("p4").unwrap().perforce_baseline,
            Some(PerforceBaseline::Disabled),
            "the baseline is overwritten every turn, never inherited"
        );
        // The binding (backend, include_shelved) persists across turns.
        let rec = store.get("p4").unwrap();
        assert_eq!(rec.backend.as_deref(), Some("perforce"));
        assert_eq!(rec.include_shelved, Some(false));
    }

    #[test]
    fn a_pending_marker_survives_and_is_cleared_only_deliberately() {
        // The marker is the durable poison: set before a turn, cleared only when it is durably
        // recorded, so a crash/failure in between leaves it set for the next resume to see.
        let dir = temp_dir();
        let store = SessionStore::new(&dir);
        assert!(!store.is_pending("p4"), "nothing pending initially");
        store.mark_pending("p4");
        assert!(store.is_pending("p4"), "marked pending");
        // It survives a fresh store over the same directory (i.e. an MCP server restart).
        assert!(SessionStore::new(&dir).is_pending("p4"));
        store.clear_pending("p4");
        assert!(!store.is_pending("p4"), "cleared");
        // Distinct session names have distinct markers.
        store.mark_pending("a");
        assert!(store.is_pending("a") && !store.is_pending("b"));
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
        // The guard has to outlive the path derived from it: bound to a temporary, it
        // would drop at the end of this statement and take the directory with it.
        let root = temp_dir();
        let dir = root.join("nested").join("deeper");
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
