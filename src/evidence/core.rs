use std::collections::{HashMap, VecDeque};
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant, UNIX_EPOCH};

use serde_json::{json, Value};

use super::{Bundle, EvidenceError, Limits, VcsKind, CODEX_TOOL_TIMEOUT_SECS};

const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

// --- Bounded read watchdog (issue #61) -------------------------------------------------------
//
// A `repository_read` (and the per-file read `repository_search` performs) can block indefinitely
// inside a filesystem syscall — `File::open`, `read_to_end`, `canonicalize`,
// `GetFinalPathNameByHandleW` — when an on-access AV scanner is holding the file, an oplock is
// contended, or the path is slow/redirected. The cooperative `deadline()` cannot interrupt a
// blocked syscall (it only fires between operations), so such a read stalls the single-threaded
// request loop until Codex's client-side `tool_timeout_sec` abandons the call at the transport
// level and fails the *whole* review. The fix runs the blocking read on a detached worker thread
// bounded by a wall-clock budget: on timeout we stop waiting and return a fast in-band error the
// review survives, converting the fatal transport abandon into an ordinary tool error.

/// Total wall-clock budget for **one request** — every stage that request runs, not one favoured
/// stage — measured from the instant its line was read off the wire. Derived from the Codex ceiling
/// with a margin covering queue wait, worker spawn, the retry backoff, and response serialisation,
/// so our in-band answer always beats the transport abandon.
///
/// This being *the* budget rather than *a* budget is issue #71's fix. Bounding `repository_read`
/// alone (issue #61) left every sibling free to hold the single-threaded request loop for longer
/// than the client will wait: `repository_list` had no bound at all, `walk_files`' own syscalls
/// only a cooperative check between directories, and the Git operations a fresh
/// `operation_timeout_ms` anchored at the moment they *started* rather than at receipt — so a
/// request that waited in the dispatch queue could still spend the full per-operation timeout on
/// top of that wait and answer past the ceiling. A per-stage timeout that does not derive from the
/// request's own clock cannot compose; deriving all of them from this one constant is what makes
/// the guarantee hold for the whole surface instead of one tool.
const READ_CEILING_MARGIN_MS: u64 = 10_000;
const REQUEST_BUDGET_MS: u64 = CODEX_TOOL_TIMEOUT_SECS * 1000 - READ_CEILING_MARGIN_MS;
/// Per-attempt wait cap; a retry is bounded by whatever budget remains, never a fresh full cap.
const READ_ATTEMPT_MS: u64 = 9_000;
const READ_RETRY_BACKOFF_MS: u64 = 500;
const MAX_READ_ATTEMPTS: u32 = 2;
/// Hard cap on concurrently-live read worker threads. The evidence server runs *outside* the
/// reviewer job object, so an abandoned worker lives until its syscall returns or the evidence
/// process exits; capping the live count turns unbounded thread growth into a deterministic
/// in-band `read_unavailable` refusal (finding f3).
const READ_WORKER_CAP: usize = 8;

// Compile-time coupling guard (finding f4): the whole read budget plus its margin must sit under
// the Codex ceiling. Deriving both from one constant means this can never drift into inversion —
// a bad change fails to compile rather than shipping a budget that races the abandon.
const _: () = assert!(REQUEST_BUDGET_MS + READ_CEILING_MARGIN_MS <= CODEX_TOOL_TIMEOUT_SECS * 1000);
const _: () = assert!(READ_ATTEMPT_MS <= REQUEST_BUDGET_MS);

/// Number of read worker threads currently alive (awaited plus abandoned). Bounded by
/// `READ_WORKER_CAP`. A `WorkerGuard` decrements it when a worker thread exits, whether it was
/// awaited or abandoned.
static LIVE_READ_WORKERS: AtomicUsize = AtomicUsize::new(0);

/// Directory walks get their **own** pool rather than sharing the read pool, and a small one.
///
/// Sharing would have made this change's own fix a new failure: a stalled `repository_list` or
/// search walk could consume the read pool's capacity and make every later `repository_read` refuse
/// with `read_unavailable` — trading one way of losing a review for another (round-1 finding f1).
/// Separate quotas mean a wedged walk can only ever cost walks. The cap is deliberately much
/// smaller than the read cap: walks are rare next to reads, one is enough to serve a turn, and a
/// second is headroom for the abandoned predecessor of a retry that is not coming.
///
/// The drift stamp deliberately stays on the *read* pool where #61 put it: `read` computes it
/// inline, so moving it here would let two stalled walks make every first read of a turn refuse —
/// re-introducing exactly the cross-operation coupling this separation exists to remove.
static LIVE_WALK_WORKERS: AtomicUsize = AtomicUsize::new(0);
const WALK_WORKER_CAP: usize = 2;

/// Child-process operations (`repository_history`, `repository_revision`) get a third pool, for the
/// same reason walks got the second: a wedged Git command must not spend capacity reads need.
///
/// It exists because reserving the drain grace inside the child's timeout is *not* sufficient on its
/// own (round-2 finding f2). `reviewer::run` kills a child that overran and then calls
/// `child.wait()`, which has no bound: a process that will not die — one stuck in an
/// uninterruptible wait on a hung network path is the realistic case — holds the request loop
/// indefinitely no matter what timeout it was given. Running the whole command on a bounded worker
/// makes the loop's freedom independent of whether the child can actually be reaped. The reservation
/// is kept as well, so in the ordinary case the child is bounded *and* returns its output rather
/// than being abandoned.
static LIVE_CHILD_WORKERS: AtomicUsize = AtomicUsize::new(0);
const CHILD_WORKER_CAP: usize = 2;

struct WorkerGuard(&'static AtomicUsize);
impl Drop for WorkerGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// The watchdog's tunables, bundled so the timing and the worker-pool bound are injectable in
/// tests (which drive a tiny budget and an isolated counter) while production uses `READ_WATCHDOG`.
struct ReadWatchdog {
    budget: Duration,
    attempt_cap: Duration,
    backoff: Duration,
    max_attempts: u32,
    live: &'static AtomicUsize,
    cap: usize,
}

const READ_WATCHDOG: ReadWatchdog = ReadWatchdog {
    budget: Duration::from_millis(REQUEST_BUDGET_MS),
    attempt_cap: Duration::from_millis(READ_ATTEMPT_MS),
    backoff: Duration::from_millis(READ_RETRY_BACKOFF_MS),
    max_attempts: MAX_READ_ATTEMPTS,
    live: &LIVE_READ_WORKERS,
    cap: READ_WORKER_CAP,
};

/// Directory walks (`repository_list`'s enumeration, `repository_search`'s `walk_files`) get the
/// same *shape* as the drift stamp — one attempt over whatever budget remains, because retrying a
/// walk that stalled on a contended `read_dir`/`symlink_metadata` just spends the rest of the
/// budget on the same blocked syscall — but their own pool and cap (see `LIVE_WALK_WORKERS`).
const WALK_WATCHDOG: ReadWatchdog = ReadWatchdog {
    budget: Duration::from_millis(REQUEST_BUDGET_MS),
    attempt_cap: Duration::from_millis(REQUEST_BUDGET_MS),
    backoff: Duration::from_millis(0),
    max_attempts: 1,
    live: &LIVE_WALK_WORKERS,
    cap: WALK_WORKER_CAP,
};

/// Same shape again for a child-process command: one attempt over what remains, its own pool. A
/// retry would re-spawn a process against whatever is wedging the first one.
const CHILD_WATCHDOG: ReadWatchdog = ReadWatchdog {
    budget: Duration::from_millis(REQUEST_BUDGET_MS),
    attempt_cap: Duration::from_millis(REQUEST_BUDGET_MS),
    backoff: Duration::from_millis(0),
    max_attempts: 1,
    live: &LIVE_CHILD_WORKERS,
    cap: CHILD_WORKER_CAP,
};

/// The drift-stamp walk gets a single attempt over the whole budget: retrying a timed-out
/// repository-wide walk is pointless, so `max_attempts: 1` with the full budget as the per-attempt
/// cap. Shares the live-worker pool and cap with reads.
const STAMP_WATCHDOG: ReadWatchdog = ReadWatchdog {
    budget: Duration::from_millis(REQUEST_BUDGET_MS),
    attempt_cap: Duration::from_millis(REQUEST_BUDGET_MS),
    backoff: Duration::from_millis(0),
    max_attempts: 1,
    live: &LIVE_READ_WORKERS,
    cap: READ_WORKER_CAP,
};

/// A read failure that preserves whether the underlying OS error was a transient sharing/lock
/// contention (retryable) rather than flattening every `io::Error` to `read_failed` and losing the
/// raw code before the retry predicate can see it (finding f10/f14). The classification is made at
/// the point of failure, at *every* blocking worker stage — resolve, `is_file`, verify, read.
struct ReadFailure {
    code: &'static str,
    message: String,
    retryable: bool,
}

impl ReadFailure {
    fn fatal(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            retryable: false,
        }
    }

    fn into_evidence(self) -> EvidenceError {
        EvidenceError::new(self.code, self.message)
    }
}

/// Windows `ERROR_SHARING_VIOLATION` (32) and `ERROR_LOCK_VIOLATION` (33): the fast form a
/// contended file (AV scan, another process's handle) surfaces as, distinct from a stall.
fn is_transient_io(error: &io::Error) -> bool {
    matches!(error.raw_os_error(), Some(32) | Some(33))
}

/// Map an `io::Error` from a blocking read stage to a `ReadFailure`, preserving transient
/// sharing/lock contention as retryable while everything else stays fatal under `fatal_code`.
fn classify_read_io(fatal_code: &'static str, context: &str, error: &io::Error) -> ReadFailure {
    ReadFailure {
        code: fatal_code,
        message: format!("{context}: {error}"),
        retryable: is_transient_io(error),
    }
}

#[derive(Clone)]
enum ReadTarget {
    /// A caller-supplied relative path that the worker must validate and resolve (the `read` path).
    Raw(String),
    /// An already-resolved, in-root file (a `search` walk hit); the worker skips re-resolution.
    Resolved(PathBuf),
}

/// Everything the blocking read worker needs, owned so the worker thread borrows nothing shared —
/// which is why an abandoned worker can never strand `Core` state or a lock and deadlock the next
/// request (the reason the whole `Core::call` dispatch is *not* bounded this way).
///
/// The worker bounds only the file I/O — resolve, `is_file`, open, verify, read. The drift stamp is
/// deliberately *not* computed here: it is a repository-wide `tree_stamp` walk that belongs with the
/// other deferred directory walks (`list`/`scope`), and doing it in the worker would (a) run the
/// walk ahead of the main thread's content checks, reordering a prompt content error behind a
/// repo-wide stamp (code-review finding f3), and (b) fold `tree_stamp`'s unclassified errors into
/// the retry path (f2). The main thread computes it via `current_stamp()` after content validation,
/// exactly as before this change.
#[derive(Clone)]
struct ReadJob {
    target: ReadTarget,
    root: PathBuf,
    max_path_bytes: usize,
    max_file_bytes: usize,
}

impl ReadJob {
    fn path_label(&self) -> String {
        match &self.target {
            ReadTarget::Raw(raw) => raw.clone(),
            ReadTarget::Resolved(path) => path.display().to_string(),
        }
    }
}

/// The materials the response needs, returned owned from the worker so the main thread never
/// re-resolves the path (a second unbounded syscall) to build the response (finding f2).
#[derive(Debug)]
struct ReadOutput {
    resolved: PathBuf,
    bytes: Vec<u8>,
}

/// The pure blocking sequence: resolve → `is_file` → open → verify → stat → read. Runs entirely on
/// the worker thread and carries raw-error classification through every stage.
fn read_job(job: ReadJob) -> Result<ReadOutput, ReadFailure> {
    let resolved = match &job.target {
        ReadTarget::Raw(raw) => {
            resolve_existing_bounded(&job.root, job.max_path_bytes, raw, false)?
        }
        ReadTarget::Resolved(path) => path.clone(),
    };
    if let ReadTarget::Raw(raw) = &job.target {
        // Replace `Path::is_file()` (which silently discards the metadata error) with an explicit,
        // classified metadata call so a sharing/lock violation here is retryable, not a false
        // `not_file` (finding f14).
        let meta = fs::metadata(&resolved)
            .map_err(|e| classify_read_io("read_failed", &format!("cannot stat '{raw}'"), &e))?;
        if !meta.is_file() {
            return Err(ReadFailure::fatal(
                "not_file",
                format!("'{raw}' is not a file"),
            ));
        }
    }
    let bytes = read_file_bounded(&resolved, job.max_file_bytes, &job.root)?;
    Ok(ReadOutput { resolved, bytes })
}

/// Run a read job under the production watchdog: a detached worker bounded by the receipt-anchored
/// budget, one retry on a transient failure, and the live-worker cap. Returns a fast in-band error
/// rather than ever blocking the request loop.
fn run_bounded_read(job: ReadJob, received_at: Instant) -> Result<ReadOutput, EvidenceError> {
    let label = job.path_label();
    bounded_attempts(&READ_WATCHDOG, received_at, &label, move || {
        let job = job.clone();
        move || read_job(job)
    })
}

/// Run the drift-stamp tree walk under the watchdog, so a first `repository_read` (or a
/// `repository_scope`) whose `tree_stamp` stalls on a contended `read_dir`/`symlink_metadata`
/// cannot hang the request loop past the budget either (code-review finding f4). A single attempt
/// (no retry — re-walking the whole tree after a timeout is pointless) using the full budget; a
/// `tree_stamp` error is fatal, not a transient to retry.
fn run_bounded_stamp(
    root: &Path,
    limits: &Limits,
    received_at: Instant,
) -> Result<String, EvidenceError> {
    let root = root.to_path_buf();
    let limits = limits.clone();
    bounded_attempts(&STAMP_WATCHDOG, received_at, "drift-stamp", move || {
        let root = root.clone();
        let limits = limits.clone();
        move || tree_stamp(&root, &limits).map_err(|e| ReadFailure::fatal(e.code, e.message))
    })
}

/// The watchdog loop, generic over the worker's output and over how each attempt's worker is built
/// so tests can inject a slow or failing worker and a tiny budget. `make_worker` yields a fresh
/// worker per attempt; each runs on a detached thread the loop stops waiting on at the deadline.
fn bounded_attempts<T, M, W>(
    watchdog: &ReadWatchdog,
    received_at: Instant,
    label: &str,
    mut make_worker: M,
) -> Result<T, EvidenceError>
where
    T: Send + 'static,
    M: FnMut() -> W,
    W: FnOnce() -> Result<T, ReadFailure> + Send + 'static,
{
    let mut last: Option<EvidenceError> = None;
    for attempt in 0..watchdog.max_attempts {
        let remaining = watchdog
            .budget
            .checked_sub(received_at.elapsed())
            .unwrap_or_default();
        if remaining.is_zero() {
            break;
        }
        // Only the single request thread reaches here, so this load-then-add is not racing another
        // spawn; abandoned workers only ever decrement.
        if watchdog.live.load(Ordering::Acquire) >= watchdog.cap {
            return Err(EvidenceError::new(
                "read_unavailable",
                "too many concurrent stalled reads; retry shortly",
            ));
        }
        let wait = watchdog.attempt_cap.min(remaining);
        let (tx, rx) = mpsc::channel();
        let live = watchdog.live;
        live.fetch_add(1, Ordering::AcqRel);
        let worker = make_worker();
        // Builder::spawn returns an error instead of panicking when the OS refuses a thread, so a
        // thread-creation failure becomes an in-band refusal rather than aborting the whole
        // evidence process (code-review finding f5).
        let spawned = std::thread::Builder::new()
            .name("evidence-read".to_string())
            .spawn(move || {
                let _guard = WorkerGuard(live); // decrements the live count when the thread exits
                let _ = tx.send(worker()); // send fails harmlessly if we abandoned it
            });
        if spawned.is_err() {
            // The worker never started, so its `WorkerGuard` will never run — undo our increment.
            live.fetch_sub(1, Ordering::AcqRel);
            return Err(EvidenceError::new(
                "read_unavailable",
                "cannot spawn a read worker thread; retry shortly",
            ));
        }
        match rx.recv_timeout(wait) {
            Ok(Ok(output)) => return Ok(output),
            Ok(Err(failure)) => {
                let retryable = failure.retryable;
                last = Some(failure.into_evidence());
                if retryable && attempt + 1 < watchdog.max_attempts {
                    let backoff = watchdog.backoff.min(
                        watchdog
                            .budget
                            .checked_sub(received_at.elapsed())
                            .unwrap_or_default(),
                    );
                    std::thread::sleep(backoff);
                    continue;
                }
                return Err(last.unwrap());
            }
            // Timed out: abandon the worker (it decrements the counter when its syscall returns)
            // and let the loop retry if a meaningful budget remains.
            Err(mpsc::RecvTimeoutError::Timeout) => {
                last = Some(read_timeout_error(label));
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(EvidenceError::new(
                    "read_failed",
                    "read worker terminated unexpectedly",
                ));
            }
        }
    }
    Err(last.unwrap_or_else(|| read_timeout_error(label)))
}

fn read_timeout_error(path: &str) -> EvidenceError {
    EvidenceError::new(
        "read_timeout",
        format!(
            "reading '{path}' exceeded the {}ms evidence read budget",
            REQUEST_BUDGET_MS
        ),
    )
}

/// What is left of this request's budget. Zero once it is spent — every stage clamps to this, so
/// the stages compose to the ceiling instead of past it.
fn remaining_budget(received_at: Instant) -> Duration {
    Duration::from_millis(REQUEST_BUDGET_MS)
        .checked_sub(received_at.elapsed())
        .unwrap_or_default()
}

/// What is left for a **child process** to spend, which is less than the request's remaining budget.
///
/// `reviewer::run` keeps collecting a killed child's pipes for up to `DRAIN_GRACE` *after* its
/// timeout fires, so a child handed the whole remaining budget would return that much past the
/// deadline it was supposed to honour. Reserving the drain up front is what makes the child's
/// timeout an actual request deadline rather than a per-command one (round-1 finding f2).
pub(super) fn child_budget(received_at: Instant) -> Duration {
    remaining_budget(received_at).saturating_sub(crate::reviewer::DRAIN_GRACE)
}

/// Run a blocking directory walk under the walk watchdog, so a stalled `read_dir`,
/// `symlink_metadata` or `canonicalize` returns a fast in-band error instead of holding the request
/// loop. The closure owns everything it touches (cloned root, limits and cancel flag) for the same
/// reason the read worker does: an abandoned worker must never strand `Core` state.
fn run_bounded_walk<T, W>(
    watchdog: &ReadWatchdog,
    label: &str,
    received_at: Instant,
    worker: W,
) -> Result<T, EvidenceError>
where
    T: Send + 'static,
    W: FnOnce() -> Result<T, EvidenceError> + Send + Clone + 'static,
{
    bounded_attempts(watchdog, received_at, label, move || {
        let worker = worker.clone();
        move || worker().map_err(|e| ReadFailure::fatal(e.code, e.message))
    })
}

#[derive(Clone)]
struct CursorPage {
    operation: String,
    values: Vec<Value>,
    offset: usize,
    source_complete: bool,
}

pub struct Core {
    bundle: Bundle,
    root: PathBuf,
    cursors: HashMap<String, CursorPage>,
    next_cursor: u64,
    calls: u32,
    returned_bytes: u64,
    observed_stamp: Option<String>,
    cancel: Arc<AtomicBool>,
    /// The watchdog the walk-bearing routes (`repository_list`, `repository_search`) run under.
    ///
    /// A seam, not a setting: production always gets `WALK_WATCHDOG`, and it exists so a test can
    /// drive the *real* route to its timeout with a tiny budget instead of asserting on
    /// `bounded_attempts` in isolation and hoping the route is wired to it (round-1 finding f4).
    /// Injecting the bound beats sleeping in a test: the assertion stays deterministic and the
    /// suite stays fast.
    walk_watchdog: &'static ReadWatchdog,
}

impl Core {
    #[cfg(test)]
    pub fn new(bundle: Bundle) -> Result<Self, EvidenceError> {
        Self::new_with_cancel(bundle, Arc::new(AtomicBool::new(false)))
    }

    pub fn new_with_cancel(bundle: Bundle, cancel: Arc<AtomicBool>) -> Result<Self, EvidenceError> {
        bundle.validate()?;
        let root = fs::canonicalize(&bundle.root).map_err(|e| {
            EvidenceError::new("invalid_root", format!("cannot canonicalize root: {e}"))
        })?;
        if !root.is_dir() || is_reparse(&root)? {
            return Err(EvidenceError::new(
                "invalid_root",
                "repository root must be a real directory, not a reparse point",
            ));
        }
        if !same_path(&root, Path::new(&bundle.root)) {
            return Err(EvidenceError::new(
                "invalid_root",
                "bundle root is not the canonical repository path",
            ));
        }
        Ok(Self {
            bundle,
            root,
            cursors: HashMap::new(),
            next_cursor: 0,
            calls: 0,
            returned_bytes: 0,
            observed_stamp: None,
            cancel,
            walk_watchdog: &WALK_WATCHDOG,
        })
    }

    /// Dispatch a call whose receipt clock starts now. Convenience for tests and any caller that
    /// is not measuring against Codex's client-side timer.
    #[cfg(test)]
    pub fn call(&mut self, name: &str, arguments: &Value) -> Result<Value, EvidenceError> {
        self.call_with_receipt(name, arguments, Instant::now())
    }

    /// Dispatch a call, measuring the read watchdog's budget from `received_at` — the instant the
    /// request was read off the wire, *before* any wait in the dispatch channel (issue #61, finding
    /// f1). The transport threads the true receipt through so a queued read cannot be handed a fresh
    /// budget after Codex's 30s timer has already been running.
    pub fn call_with_receipt(
        &mut self,
        name: &str,
        arguments: &Value,
        received_at: Instant,
    ) -> Result<Value, EvidenceError> {
        if self.cancel.load(Ordering::Acquire) {
            return Err(EvidenceError::new(
                "cancelled",
                "the evidence operation was cancelled or its parent transport closed",
            ));
        }
        // This request's budget is already gone before any work starts — it waited behind a slower
        // one for longer than the client will wait for an answer. Refuse now, fast and in-band,
        // rather than spending the loop on a result the client has abandoned and making the *next*
        // request late too. Deliberately ahead of the call counter: a request that did no work
        // should not consume the caller's request budget as well as its own (#71).
        if remaining_budget(received_at).is_zero() {
            return Err(EvidenceError::new(
                "request_expired",
                format!(
                    "the request waited longer than the {REQUEST_BUDGET_MS}ms evidence request \
                     budget before it could be dispatched; retry it"
                ),
            ));
        }
        self.calls = self
            .calls
            .checked_add(1)
            .ok_or_else(|| EvidenceError::new("limit_exceeded", "request counter overflow"))?;
        if self.calls > self.bundle.limits.max_calls {
            return Err(EvidenceError::new(
                "limit_exceeded",
                "evidence request budget exhausted",
            ));
        }
        let args = arguments.as_object().ok_or_else(|| {
            EvidenceError::new("invalid_arguments", "arguments must be an object")
        })?;
        let result = match name {
            "repository_scope" => {
                require_only(args, &[])?;
                self.scope(received_at)
            }
            "repository_list" => {
                require_only(args, &["path", "cursor", "limit"])?;
                self.list(args, received_at)
            }
            "repository_search" => {
                require_only(args, &["query", "path", "cursor", "limit"])?;
                self.search(args, received_at)
            }
            "repository_read" => {
                require_only(args, &["path", "start_line", "line_count"])?;
                self.read(args, received_at)
            }
            "repository_change" => {
                require_only(args, &["cursor", "limit_bytes"])?;
                self.change(args)
            }
            "repository_history" => {
                require_only(args, &["path", "before", "cursor", "limit"])?;
                self.history(args, received_at)
            }
            "repository_revision" => {
                require_only(args, &["id", "path", "cursor", "limit_bytes"])?;
                self.revision(args, received_at)
            }
            _ => Err(EvidenceError::new(
                "unknown_tool",
                format!("unknown evidence tool '{name}'"),
            )),
        }?;
        let bytes = serde_json::to_vec(&result)
            .map_err(|e| EvidenceError::new("internal", format!("cannot encode result: {e}")))?
            .len() as u64;
        if bytes > self.bundle.limits.max_response_bytes as u64 {
            return Err(EvidenceError::new(
                "limit_exceeded",
                "evidence response exceeded the configured byte cap",
            ));
        }
        self.returned_bytes = self
            .returned_bytes
            .checked_add(bytes)
            .ok_or_else(|| EvidenceError::new("limit_exceeded", "byte counter overflow"))?;
        if self.returned_bytes > self.bundle.limits.max_total_bytes {
            return Err(EvidenceError::new(
                "limit_exceeded",
                "cumulative evidence byte budget exhausted",
            ));
        }
        Ok(result)
    }

    fn scope(&mut self, received_at: Instant) -> Result<Value, EvidenceError> {
        let current_stamp = self.current_stamp(received_at)?;
        let drifted = current_stamp != self.bundle.initial_stamp;
        Ok(json!({
            "schema_version": self.bundle.schema_version,
            "nonce": self.bundle.nonce,
            "root": self.root.to_string_lossy(),
            "vcs": self.bundle.vcs,
            "change_label": self.bundle.change_label,
            "status_summary": self.bundle.status_summary,
            "limits": self.bundle.limits,
            "excluded_directory_names": [".git", ".hg", ".svn", "target", "dist"],
            "initial_stamp": self.bundle.initial_stamp,
            "current_stamp": current_stamp,
            "drifted": drifted,
            "complete": true,
            "truncated": false,
            "cursor": Value::Null,
        }))
    }

    fn list(
        &mut self,
        args: &serde_json::Map<String, Value>,
        received_at: Instant,
    ) -> Result<Value, EvidenceError> {
        let limit = limit_arg(
            args,
            "limit",
            self.bundle.limits.default_entries,
            self.bundle.limits.max_entries,
        )?;
        if let Some(cursor) = optional_string(args, "cursor")? {
            cursor_only(args, &["cursor", "limit"])?;
            return self.cursor_page("repository_list", &cursor, limit, "entries");
        }
        let path = optional_string(args, "path")?.unwrap_or_default();
        // The whole enumeration — resolve, `read_dir`, and a `symlink_metadata` per child — is
        // blocking filesystem I/O on a single-threaded loop, and before #71 it carried no bound at
        // all: not even the cooperative check the search walk had. One contended entry could hold
        // the loop until the client abandoned the call it was serving *and* the next one. It now
        // runs on a watchdog-bounded worker owning only clones, exactly like a read.
        let root = self.root.clone();
        let max_path_bytes = self.bundle.limits.max_path_bytes as usize;
        let max_files = self.bundle.limits.max_files as usize;
        let raw = path.clone();
        let mut entries = run_bounded_walk(
            self.walk_watchdog,
            &format!("list '{path}'"),
            received_at,
            move || {
                let dir = resolve_existing_bounded(&root, max_path_bytes, &raw, true)
                    .map_err(ReadFailure::into_evidence)?;
                let mut entries = Vec::new();
                for entry in fs::read_dir(&dir).map_err(|e| {
                    EvidenceError::new("read_failed", format!("cannot list '{raw}': {e}"))
                })? {
                    let entry =
                        entry.map_err(|e| EvidenceError::new("read_failed", e.to_string()))?;
                    let name = entry.file_name();
                    if excluded_name(&name) {
                        continue;
                    }
                    let child = entry.path();
                    let meta = fs::symlink_metadata(&child)
                        .map_err(|e| EvidenceError::new("read_failed", e.to_string()))?;
                    let kind = if metadata_is_reparse(&meta) {
                        "link"
                    } else if meta.is_dir() {
                        "directory"
                    } else if meta.is_file() {
                        "file"
                    } else {
                        "other"
                    };
                    let relative = relative_slash(&root, &child)?;
                    entries.push(json!({"path": relative, "type": kind, "bytes": meta.len()}));
                    if entries.len() > max_files {
                        return Err(EvidenceError::new(
                            "limit_exceeded",
                            "listing exceeded file budget",
                        ));
                    }
                }
                Ok(entries)
            },
        )?;
        entries.sort_by_key(value_path);
        self.first_page("repository_list", entries, limit, "entries", true)
    }

    fn search(
        &mut self,
        args: &serde_json::Map<String, Value>,
        received_at: Instant,
    ) -> Result<Value, EvidenceError> {
        let limit = limit_arg(
            args,
            "limit",
            self.bundle.limits.default_matches,
            self.bundle.limits.max_matches,
        )?;
        if let Some(cursor) = optional_string(args, "cursor")? {
            cursor_only(args, &["cursor", "limit"])?;
            return self.cursor_page("repository_search", &cursor, limit, "matches");
        }
        let query = required_string(args, "query")?;
        if query.is_empty() || query.len() > self.bundle.limits.max_query_bytes as usize {
            return Err(EvidenceError::new(
                "invalid_arguments",
                "query is empty or too long",
            ));
        }
        let path = optional_string(args, "path")?.unwrap_or_default();
        // Base resolution and the tree walk run together on a bounded worker (#71); the per-file
        // reads below go through the same watchdog as `repository_read` (#61). Every stage of this
        // operation now derives its wait from the one request budget.
        let files = self.resolve_and_walk(&path, received_at)?;
        let mut matches = Vec::new();
        let mut source_complete = true;
        for file in files {
            if self.cancel.load(Ordering::Acquire) {
                return Err(EvidenceError::new(
                    "cancelled",
                    "evidence search was cancelled",
                ));
            }
            deadline(received_at, &self.bundle.limits)?;
            let bytes = match run_bounded_read(
                ReadJob {
                    target: ReadTarget::Resolved(file.clone()),
                    root: self.root.clone(),
                    max_path_bytes: self.bundle.limits.max_path_bytes as usize,
                    max_file_bytes: self.bundle.limits.max_file_bytes as usize,
                },
                received_at,
            ) {
                Ok(output) => output.bytes,
                Err(_) => {
                    source_complete = false;
                    continue;
                }
            };
            if bytes.contains(&0) {
                continue;
            }
            let Ok(text) = std::str::from_utf8(&bytes) else {
                continue;
            };
            for (index, line) in text.lines().enumerate() {
                if line.contains(&query) {
                    let excerpt = cap_chars(line, self.bundle.limits.max_line_bytes as usize);
                    matches.push(json!({
                        "path": relative_slash(&self.root, &file)?,
                        "line": index + 1,
                        "excerpt": excerpt,
                    }));
                    if matches.len() >= self.bundle.limits.max_matches as usize {
                        source_complete = false;
                        break;
                    }
                }
            }
            if matches.len() >= self.bundle.limits.max_matches as usize {
                break;
            }
        }
        self.first_page(
            "repository_search",
            matches,
            limit,
            "matches",
            source_complete,
        )
    }

    fn read(
        &mut self,
        args: &serde_json::Map<String, Value>,
        received_at: Instant,
    ) -> Result<Value, EvidenceError> {
        let path = required_string(args, "path")?;
        let start_line = positive_arg(args, "start_line", 1, u32::MAX)? as usize;
        let line_count = positive_arg(
            args,
            "line_count",
            self.bundle.limits.default_lines,
            self.bundle.limits.max_lines,
        )? as usize;
        // The blocking file I/O — resolve, is_file, open, verify, stat, read — runs on a
        // watchdog-bounded worker so a contended file cannot stall the request loop past the read
        // budget. The drift stamp is computed afterward on the main thread (below), preserving the
        // original content-errors-before-stamp order (code-review finding f3).
        let output = run_bounded_read(
            ReadJob {
                target: ReadTarget::Raw(path.clone()),
                root: self.root.clone(),
                max_path_bytes: self.bundle.limits.max_path_bytes as usize,
                max_file_bytes: self.bundle.limits.max_file_bytes as usize,
            },
            received_at,
        )?;
        let bytes = output.bytes;
        if bytes.contains(&0) {
            return Err(EvidenceError::new("binary", format!("'{path}' is binary")));
        }
        let text = std::str::from_utf8(&bytes)
            .map_err(|_| EvidenceError::new("non_utf8", format!("'{path}' is not UTF-8")))?;
        let all: Vec<&str> = text.lines().collect();
        let begin = start_line.saturating_sub(1).min(all.len());
        let end = begin.saturating_add(line_count).min(all.len());
        let mut lines = Vec::with_capacity(end.saturating_sub(begin));
        for (offset, line) in all[begin..end].iter().enumerate() {
            if line.len() > self.bundle.limits.max_line_bytes as usize {
                return Err(EvidenceError::new(
                    "line_too_long",
                    format!("'{path}' contains a line over the configured cap"),
                ));
            }
            lines.push(json!({"line": begin + offset + 1, "text": line}));
        }
        let fingerprint = crate::digest::Fingerprint::of(&bytes)
            .ok_or_else(|| EvidenceError::new("digest_unavailable", "SHA-256 is unavailable"))?;
        // Only after the content is known good do we compute the (cached) drift stamp. The walk is
        // watchdog-bounded (below) so a first read whose stamp stalls still cannot hang the loop.
        let current_stamp = self.current_stamp(received_at)?;
        let drifted = current_stamp != self.bundle.initial_stamp;
        Ok(json!({
            "path": relative_slash(&self.root, &output.resolved)?,
            "bytes": bytes.len(),
            "sha256": fingerprint.sha256,
            "total_lines": all.len(),
            "lines": lines,
            "complete": end == all.len(),
            "truncated": end < all.len(),
            "cursor": Value::Null,
            "drifted": drifted,
        }))
    }

    fn current_stamp(&mut self, received_at: Instant) -> Result<String, EvidenceError> {
        if let Some(stamp) = &self.observed_stamp {
            return Ok(stamp.clone());
        }
        // The drift-stamp walk runs under the watchdog so a stalled `read_dir`/`symlink_metadata`
        // cannot hang the request loop (#61 finding f4). It bounds the walk for both `read` and
        // `scope`, and stays on the *read* pool: `read` computes it inline, so moving it to the
        // small walk pool would let two stalled walks refuse every first read of a turn.
        // `list`/search-base `walk_files` are no longer the exception here -- they are bounded on
        // their own pool as of #71.
        let current_stamp = run_bounded_stamp(&self.root, &self.bundle.limits, received_at)?;
        self.observed_stamp = Some(current_stamp.clone());
        Ok(current_stamp)
    }

    fn change(&mut self, args: &serde_json::Map<String, Value>) -> Result<Value, EvidenceError> {
        let limit = limit_arg(
            args,
            "limit_bytes",
            self.bundle.limits.default_change_bytes,
            self.bundle.limits.max_change_bytes,
        )? as usize;
        let offset = if let Some(cursor) = optional_string(args, "cursor")? {
            return self.change_cursor(&cursor, limit);
        } else {
            0
        };
        self.change_page(offset, limit)
    }

    fn history(
        &mut self,
        args: &serde_json::Map<String, Value>,
        received_at: Instant,
    ) -> Result<Value, EvidenceError> {
        if self.bundle.vcs != VcsKind::Git {
            return Err(EvidenceError::new(
                "unsupported",
                "repository_history is unsupported for Perforce",
            ));
        }
        let limit = limit_arg(
            args,
            "limit",
            self.bundle.limits.default_history,
            self.bundle.limits.max_history,
        )?;
        if let Some(cursor) = optional_string(args, "cursor")? {
            cursor_only(args, &["cursor", "limit"])?;
            return self.cursor_page("repository_history", &cursor, limit, "commits");
        }
        let path = optional_string(args, "path")?.unwrap_or_default();
        if !path.is_empty() {
            self.validate_relative(&path)?;
        }
        let before = optional_string(args, "before")?.unwrap_or_default();
        if !before.is_empty() && !valid_object_id(&before) {
            return Err(EvidenceError::new(
                "invalid_arguments",
                "before must be a full Git object id",
            ));
        }
        // The Git child runs on a bounded worker: the request loop must stay free even if the
        // process cannot be reaped (round-2 finding f2).
        let root = self.root.clone();
        let limits = self.bundle.limits.clone();
        let cancel = Arc::clone(&self.cancel);
        let (commits, source_complete) =
            run_bounded_walk(&CHILD_WATCHDOG, "git history", received_at, move || {
                super::git::history(&root, &path, &before, &limits, &cancel, received_at)
            })?;
        self.first_page(
            "repository_history",
            commits,
            limit,
            "commits",
            source_complete,
        )
    }

    fn revision(
        &mut self,
        args: &serde_json::Map<String, Value>,
        received_at: Instant,
    ) -> Result<Value, EvidenceError> {
        if self.bundle.vcs != VcsKind::Git {
            return Err(EvidenceError::new(
                "unsupported",
                "repository_revision is unsupported for Perforce",
            ));
        }
        let limit = limit_arg(
            args,
            "limit_bytes",
            self.bundle.limits.default_change_bytes,
            self.bundle.limits.max_change_bytes,
        )? as usize;
        if let Some(cursor) = optional_string(args, "cursor")? {
            cursor_only(args, &["cursor", "limit_bytes"])?;
            return self.text_cursor("repository_revision", &cursor, limit, "content");
        }
        let id = required_string(args, "id")?;
        if !valid_object_id(&id) {
            return Err(EvidenceError::new(
                "invalid_arguments",
                "id must be a full Git object id",
            ));
        }
        let path = optional_string(args, "path")?.unwrap_or_default();
        if !path.is_empty() {
            self.validate_relative(&path)?;
        }
        let root = self.root.clone();
        let limits = self.bundle.limits.clone();
        let cancel = Arc::clone(&self.cancel);
        let text = run_bounded_walk(&CHILD_WATCHDOG, "git revision", received_at, move || {
            super::git::revision(&root, &id, &path, &limits, &cancel, received_at)
        })?;
        self.first_text_page("repository_revision", text, limit, "content")
    }

    /// Resolve the search base and walk it, all on one watchdog-bounded worker.
    ///
    /// Both halves are blocking filesystem I/O that the cooperative `deadline()` cannot interrupt:
    /// it fires *between* directories, so a `symlink_metadata`, `canonicalize` or `read_dir` that
    /// blocks inside one holds the request loop indefinitely. #61 bounded the per-file reads this
    /// walk performs and recorded the walk itself as the deferred sibling; #71 is that follow-up.
    fn resolve_and_walk(
        &self,
        path: &str,
        received_at: Instant,
    ) -> Result<Vec<PathBuf>, EvidenceError> {
        let root = self.root.clone();
        let limits = self.bundle.limits.clone();
        let cancel = Arc::clone(&self.cancel);
        let max_path_bytes = self.bundle.limits.max_path_bytes as usize;
        let raw = path.to_string();
        run_bounded_walk(
            self.walk_watchdog,
            &format!("search '{path}'"),
            received_at,
            move || {
                let base = resolve_existing_bounded(&root, max_path_bytes, &raw, false)
                    .map_err(ReadFailure::into_evidence)?;
                walk_files(&root, &base, &limits, &cancel, received_at)
            },
        )
    }

    fn validate_relative(&self, raw: &str) -> Result<PathBuf, EvidenceError> {
        validate_relative_path(self.bundle.limits.max_path_bytes as usize, raw)
            .map_err(ReadFailure::into_evidence)
    }
}

/// The blocking walk itself, over owned inputs so it can run on a detached worker that borrows
/// nothing from `Core`.
fn walk_files(
    root: &Path,
    base: &Path,
    limits: &Limits,
    cancel: &AtomicBool,
    start: Instant,
) -> Result<Vec<PathBuf>, EvidenceError> {
    {
        if base.is_file() {
            return Ok(vec![base.to_path_buf()]);
        }
        if !base.is_dir() {
            return Err(EvidenceError::new(
                "not_found",
                "search path does not exist",
            ));
        }
        let mut queue = VecDeque::from([base.to_path_buf()]);
        let mut files = Vec::new();
        while let Some(dir) = queue.pop_front() {
            if cancel.load(Ordering::Acquire) {
                return Err(EvidenceError::new(
                    "cancelled",
                    "evidence walk was cancelled",
                ));
            }
            deadline(start, limits)?;
            let dir_meta = fs::symlink_metadata(&dir)
                .map_err(|e| EvidenceError::new("read_failed", e.to_string()))?;
            if metadata_is_reparse(&dir_meta) {
                continue;
            }
            let dir = fs::canonicalize(&dir)
                .map_err(|e| EvidenceError::new("read_failed", e.to_string()))?;
            if !within(&dir, root) {
                return Err(EvidenceError::new(
                    "path_escape",
                    "directory changed to resolve outside the repository root",
                ));
            }
            let mut children = Vec::new();
            for entry in
                fs::read_dir(&dir).map_err(|e| EvidenceError::new("read_failed", e.to_string()))?
            {
                let entry = entry.map_err(|e| EvidenceError::new("read_failed", e.to_string()))?;
                if excluded_name(&entry.file_name()) {
                    continue;
                }
                children.push(entry.path());
            }
            children.sort_by_key(|p| p.to_string_lossy().to_ascii_lowercase());
            for child in children {
                let meta = fs::symlink_metadata(&child)
                    .map_err(|e| EvidenceError::new("read_failed", e.to_string()))?;
                if metadata_is_reparse(&meta) {
                    continue;
                }
                if meta.is_dir() {
                    queue.push_back(child);
                } else if meta.is_file() {
                    files.push(child);
                    if files.len() > limits.max_files as usize {
                        return Err(EvidenceError::new(
                            "limit_exceeded",
                            "walk exceeded file budget",
                        ));
                    }
                }
            }
        }
        files.sort_by_key(|p| p.to_string_lossy().to_ascii_lowercase());
        Ok(files)
    }
}

impl Core {
    fn first_page(
        &mut self,
        operation: &str,
        values: Vec<Value>,
        limit: u32,
        field: &str,
        source_complete: bool,
    ) -> Result<Value, EvidenceError> {
        let take = (limit as usize).min(values.len());
        let page = values[..take].to_vec();
        let cursor = if take < values.len() {
            Some(self.store_cursor(operation, values, take, source_complete))
        } else {
            None
        };
        Ok(page_result(field, page, cursor, source_complete))
    }

    fn cursor_page(
        &mut self,
        operation: &str,
        cursor: &str,
        limit: u32,
        field: &str,
    ) -> Result<Value, EvidenceError> {
        let state = self.take_cursor(cursor, operation)?;
        let start = state.offset;
        let end = start.saturating_add(limit as usize).min(state.values.len());
        let page = state.values[start..end].to_vec();
        let next = if end < state.values.len() {
            Some(self.store_cursor(operation, state.values, end, state.source_complete))
        } else {
            None
        };
        Ok(page_result(field, page, next, state.source_complete))
    }

    fn store_cursor(
        &mut self,
        operation: &str,
        values: Vec<Value>,
        offset: usize,
        source_complete: bool,
    ) -> String {
        self.next_cursor = self.next_cursor.saturating_add(1);
        let token = format!("{}-{:016x}", self.bundle.nonce, self.next_cursor);
        self.cursors.insert(
            token.clone(),
            CursorPage {
                operation: operation.to_string(),
                values,
                offset,
                source_complete,
            },
        );
        token
    }

    fn take_cursor(&mut self, token: &str, operation: &str) -> Result<CursorPage, EvidenceError> {
        let state = self.cursors.remove(token).ok_or_else(|| {
            EvidenceError::new(
                "invalid_cursor",
                "cursor is missing, expired, or already consumed",
            )
        })?;
        if state.operation != operation {
            return Err(EvidenceError::new(
                "invalid_cursor",
                "cursor does not match this operation",
            ));
        }
        Ok(state)
    }

    fn change_page(&mut self, offset: usize, limit: usize) -> Result<Value, EvidenceError> {
        let text = self.bundle.change.clone().unwrap_or_default();
        let end = utf8_end(&text, offset.saturating_add(limit).min(text.len()));
        let content = text.get(offset..end).ok_or_else(|| {
            EvidenceError::new("invalid_cursor", "change cursor is not on a UTF-8 boundary")
        })?;
        let cursor = if end < text.len() {
            let values = vec![json!({"offset": end})];
            Some(self.store_cursor("repository_change", values, 0, true))
        } else {
            None
        };
        Ok(json!({
            "label": self.bundle.change_label,
            "content": content,
            "bytes": text.len(),
            "complete": end == text.len(),
            "truncated": end < text.len(),
            "cursor": cursor,
        }))
    }

    fn change_cursor(&mut self, token: &str, limit: usize) -> Result<Value, EvidenceError> {
        let state = self.take_cursor(token, "repository_change")?;
        let offset = state
            .values
            .first()
            .and_then(|v| v.get("offset"))
            .and_then(Value::as_u64)
            .ok_or_else(|| EvidenceError::new("invalid_cursor", "malformed change cursor"))?
            as usize;
        self.change_page(offset, limit)
    }

    fn first_text_page(
        &mut self,
        operation: &str,
        text: String,
        limit: usize,
        field: &str,
    ) -> Result<Value, EvidenceError> {
        let end = utf8_end(&text, limit.min(text.len()));
        let content = text[..end].to_string();
        let cursor = if end < text.len() {
            Some(self.store_cursor(
                operation,
                vec![json!({"text": text, "offset": end})],
                0,
                true,
            ))
        } else {
            None
        };
        Ok(text_result(field, content, end, cursor))
    }

    fn text_cursor(
        &mut self,
        operation: &str,
        token: &str,
        limit: usize,
        field: &str,
    ) -> Result<Value, EvidenceError> {
        let state = self.take_cursor(token, operation)?;
        let item = state
            .values
            .first()
            .ok_or_else(|| EvidenceError::new("invalid_cursor", "malformed text cursor"))?;
        let text = item
            .get("text")
            .and_then(Value::as_str)
            .ok_or_else(|| EvidenceError::new("invalid_cursor", "malformed text cursor"))?
            .to_string();
        let offset = item
            .get("offset")
            .and_then(Value::as_u64)
            .ok_or_else(|| EvidenceError::new("invalid_cursor", "malformed text cursor"))?
            as usize;
        let end = utf8_end(&text, offset.saturating_add(limit).min(text.len()));
        let content = text[offset..end].to_string();
        let cursor = if end < text.len() {
            Some(self.store_cursor(
                operation,
                vec![json!({"text": text, "offset": end})],
                0,
                true,
            ))
        } else {
            None
        };
        Ok(text_result(field, content, end, cursor))
    }
}

/// Validate a caller-supplied relative path (length, absolute/device/ADS, forbidden components).
/// Pure and non-blocking; every failure is fatal (`invalid_path`), never retryable.
fn validate_relative_path(max_path_bytes: usize, raw: &str) -> Result<PathBuf, ReadFailure> {
    if raw.len() > max_path_bytes {
        return Err(ReadFailure::fatal("invalid_path", "path is too long"));
    }
    let path = Path::new(raw);
    if path.is_absolute() || raw.contains(':') {
        return Err(ReadFailure::fatal(
            "invalid_path",
            "absolute, device, and ADS paths are forbidden",
        ));
    }
    let mut clean = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(name) if !reserved_name(name) && !excluded_name(name) => {
                clean.push(name)
            }
            Component::CurDir => {}
            _ => {
                return Err(ReadFailure::fatal(
                    "invalid_path",
                    "path contains a forbidden component",
                ))
            }
        }
    }
    Ok(clean)
}

/// Resolve a validated relative path to a canonical in-root path, classifying the raw OS error at
/// each blocking stage (`symlink_metadata`, `canonicalize`) so a transient sharing/lock violation
/// is retryable rather than a false `not_found` (finding f14). Runs on the read worker thread.
fn resolve_existing_bounded(
    root: &Path,
    max_path_bytes: usize,
    raw: &str,
    directory: bool,
) -> Result<PathBuf, ReadFailure> {
    let clean = validate_relative_path(max_path_bytes, raw)?;
    let mut current = root.to_path_buf();
    for component in clean.components() {
        current.push(component.as_os_str());
        let meta = fs::symlink_metadata(&current).map_err(|e| {
            if is_transient_io(&e) {
                classify_read_io("read_failed", &format!("cannot stat '{raw}'"), &e)
            } else {
                ReadFailure::fatal("not_found", format!("'{raw}' does not exist"))
            }
        })?;
        if metadata_is_reparse(&meta) {
            return Err(ReadFailure::fatal(
                "link_forbidden",
                "links and reparse points are forbidden",
            ));
        }
    }
    let canonical = fs::canonicalize(&current).map_err(|e| {
        if is_transient_io(&e) {
            classify_read_io("read_failed", &format!("cannot resolve '{raw}'"), &e)
        } else {
            ReadFailure::fatal("not_found", e.to_string())
        }
    })?;
    if !within(&canonical, root) {
        return Err(ReadFailure::fatal(
            "path_escape",
            "resolved path escaped the repository root",
        ));
    }
    if directory && !canonical.is_dir() {
        return Err(ReadFailure::fatal(
            "not_directory",
            format!("'{raw}' is not a directory"),
        ));
    }
    Ok(canonical)
}

/// Open, verify, and read a resolved in-root file, capped at `max`. Every blocking stage
/// classifies its raw OS error so a transient sharing/lock violation is retryable (findings
/// f10/f14). Runs on the read worker thread.
fn read_file_bounded(path: &Path, max: usize, root: &Path) -> Result<Vec<u8>, ReadFailure> {
    let mut file = File::open(path).map_err(|e| {
        classify_read_io(
            "read_failed",
            &format!("cannot open '{}'", path.display()),
            &e,
        )
    })?;
    verify_open_file(&file, path, root)?;
    let len = file
        .metadata()
        .map_err(|e| classify_read_io("read_failed", "cannot stat open file", &e))?
        .len();
    if len > max as u64 {
        return Err(ReadFailure::fatal(
            "file_too_large",
            format!("file is {len} bytes; cap is {max}"),
        ));
    }
    file.seek(SeekFrom::Start(0))
        .map_err(|e| classify_read_io("read_failed", "cannot seek open file", &e))?;
    let mut bytes = Vec::with_capacity(len as usize);
    file.take(max as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| classify_read_io("read_failed", "cannot read open file", &e))?;
    if bytes.len() > max {
        return Err(ReadFailure::fatal(
            "file_too_large",
            "file grew beyond the read cap",
        ));
    }
    Ok(bytes)
}

#[cfg(windows)]
fn verify_open_file(file: &File, expected: &Path, root: &Path) -> Result<(), ReadFailure> {
    use std::os::windows::ffi::OsStringExt;
    use std::os::windows::io::AsRawHandle;
    extern "system" {
        fn GetFinalPathNameByHandleW(
            handle: *mut std::ffi::c_void,
            path: *mut u16,
            len: u32,
            flags: u32,
        ) -> u32;
    }
    let handle = file.as_raw_handle();
    let needed = unsafe { GetFinalPathNameByHandleW(handle, std::ptr::null_mut(), 0, 0) };
    if needed == 0 {
        // Classify the WinAPI failure via GetLastError so a sharing/lock contention here is
        // retryable rather than a false fatal (finding f2).
        return Err(classify_read_io(
            "read_failed",
            "cannot verify opened file path",
            &io::Error::last_os_error(),
        ));
    }
    let mut buf = vec![0u16; needed as usize + 1];
    let written =
        unsafe { GetFinalPathNameByHandleW(handle, buf.as_mut_ptr(), buf.len() as u32, 0) };
    if written == 0 {
        return Err(classify_read_io(
            "read_failed",
            "cannot verify opened file path",
            &io::Error::last_os_error(),
        ));
    }
    if written as usize >= buf.len() {
        return Err(ReadFailure::fatal(
            "read_failed",
            "cannot verify opened file path",
        ));
    }
    let actual = PathBuf::from(std::ffi::OsString::from_wide(&buf[..written as usize]));
    let expected = fs::canonicalize(expected)
        .map_err(|e| classify_read_io("read_failed", "cannot canonicalize expected path", &e))?;
    if !within(&actual, root) {
        return Err(ReadFailure::fatal(
            "path_escape",
            "opened file resolved outside the repository root",
        ));
    }
    if !same_path(&actual, &expected) {
        return Err(ReadFailure::fatal(
            "path_changed",
            "file path changed while it was opened",
        ));
    }
    Ok(())
}

fn tree_stamp(root: &Path, limits: &Limits) -> Result<String, EvidenceError> {
    let start = Instant::now();
    let mut queue = VecDeque::from([root.to_path_buf()]);
    let mut rows = Vec::new();
    while let Some(dir) = queue.pop_front() {
        deadline(start, limits)?;
        let mut children = Vec::new();
        for entry in
            fs::read_dir(&dir).map_err(|e| EvidenceError::new("read_failed", e.to_string()))?
        {
            let entry = entry.map_err(|e| EvidenceError::new("read_failed", e.to_string()))?;
            if excluded_name(&entry.file_name()) {
                continue;
            }
            children.push(entry.path());
        }
        children.sort_by_key(|p| p.to_string_lossy().to_ascii_lowercase());
        for child in children {
            let meta = fs::symlink_metadata(&child)
                .map_err(|e| EvidenceError::new("read_failed", e.to_string()))?;
            if metadata_is_reparse(&meta) {
                continue;
            }
            let relative = relative_slash(root, &child)?;
            let modified = meta
                .modified()
                .ok()
                .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            rows.push(format!("{relative}\0{}\0{modified}", meta.len()));
            if rows.len() > limits.max_files as usize {
                return Err(EvidenceError::new(
                    "limit_exceeded",
                    "drift scan exceeded file budget",
                ));
            }
            if meta.is_dir() {
                queue.push_back(child);
            }
        }
    }
    let joined = rows.join("\n");
    let fp = crate::digest::Fingerprint::of(joined.as_bytes())
        .ok_or_else(|| EvidenceError::new("digest_unavailable", "SHA-256 is unavailable"))?;
    Ok(fp.sha256)
}

pub fn initial_stamp(root: &Path, limits: &Limits) -> Result<String, EvidenceError> {
    tree_stamp(root, limits)
}

/// The cooperative between-steps check, against **both** ceilings: the per-operation timeout the
/// bundle configures, and the request budget every stage shares. Both are anchored at the same
/// receipt instant, so this is just the tighter of the two — but taking the minimum is what stops a
/// long-queued request from being handed a full fresh operation timeout on top of its wait (#71).
fn deadline(start: Instant, limits: &Limits) -> Result<(), EvidenceError> {
    let ceiling = Duration::from_millis(limits.operation_timeout_ms.min(REQUEST_BUDGET_MS));
    if start.elapsed() > ceiling {
        Err(EvidenceError::new(
            "deadline_exceeded",
            "evidence operation exceeded its deadline",
        ))
    } else {
        Ok(())
    }
}

#[cfg(windows)]
fn metadata_is_reparse(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

fn is_reparse(path: &Path) -> Result<bool, EvidenceError> {
    let meta =
        fs::symlink_metadata(path).map_err(|e| EvidenceError::new("read_failed", e.to_string()))?;
    Ok(metadata_is_reparse(&meta))
}

fn same_path(a: &Path, b: &Path) -> bool {
    normalize_path(a) == normalize_path(b)
}
fn within(path: &Path, root: &Path) -> bool {
    let path = normalize_path(path);
    let root = normalize_path(root);
    path == root || path.starts_with(&(root + "\\"))
}
fn normalize_path(path: &Path) -> String {
    let mut value = path.to_string_lossy().replace('/', "\\");
    if let Some(rest) = value.strip_prefix(r"\\?\UNC\") {
        value = format!(r"\\{rest}");
    } else if let Some(rest) = value.strip_prefix(r"\\?\") {
        value = rest.to_string();
    }
    value.trim_end_matches('\\').to_lowercase()
}

fn relative_slash(root: &Path, path: &Path) -> Result<String, EvidenceError> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| EvidenceError::new("path_escape", "path escaped root"))?;
    Ok(relative.to_string_lossy().replace('\\', "/"))
}

fn excluded_name(name: &OsStr) -> bool {
    matches!(
        name.to_string_lossy().to_ascii_lowercase().as_str(),
        ".git" | ".hg" | ".svn" | "target" | "dist"
    )
}

fn reserved_name(name: &OsStr) -> bool {
    let raw = name.to_string_lossy();
    if raw.is_empty() || raw.ends_with([' ', '.']) {
        return true;
    }
    let stem = raw
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end_matches([' ', '.'])
        .to_ascii_uppercase();
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || stem
            .strip_prefix("COM")
            .is_some_and(|n| matches!(n.parse::<u8>(), Ok(1..=9)))
        || stem
            .strip_prefix("LPT")
            .is_some_and(|n| matches!(n.parse::<u8>(), Ok(1..=9)))
}

fn require_only(
    args: &serde_json::Map<String, Value>,
    allowed: &[&str],
) -> Result<(), EvidenceError> {
    if let Some(key) = args.keys().find(|key| !allowed.contains(&key.as_str())) {
        return Err(EvidenceError::new(
            "invalid_arguments",
            format!("unknown argument '{key}'"),
        ));
    }
    Ok(())
}
fn cursor_only(
    args: &serde_json::Map<String, Value>,
    allowed: &[&str],
) -> Result<(), EvidenceError> {
    require_only(args, allowed)
}
fn required_string(
    args: &serde_json::Map<String, Value>,
    key: &str,
) -> Result<String, EvidenceError> {
    optional_string(args, key)?
        .ok_or_else(|| EvidenceError::new("invalid_arguments", format!("missing '{key}'")))
}
fn optional_string(
    args: &serde_json::Map<String, Value>,
    key: &str,
) -> Result<Option<String>, EvidenceError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(EvidenceError::new(
            "invalid_arguments",
            format!("'{key}' must be a string"),
        )),
    }
}
fn positive_arg(
    args: &serde_json::Map<String, Value>,
    key: &str,
    default: u32,
    max: u32,
) -> Result<u32, EvidenceError> {
    let value = match args.get(key) {
        None | Some(Value::Null) => default,
        Some(v) => u32::try_from(v.as_u64().ok_or_else(|| {
            EvidenceError::new(
                "invalid_arguments",
                format!("'{key}' must be a positive integer"),
            )
        })?)
        .map_err(|_| EvidenceError::new("invalid_arguments", format!("'{key}' is too large")))?,
    };
    if value == 0 || value > max {
        return Err(EvidenceError::new(
            "invalid_arguments",
            format!("'{key}' must be 1..={max}"),
        ));
    }
    Ok(value)
}
fn limit_arg(
    args: &serde_json::Map<String, Value>,
    key: &str,
    default: u32,
    max: u32,
) -> Result<u32, EvidenceError> {
    positive_arg(args, key, default, max)
}
fn page_result(
    field: &str,
    values: Vec<Value>,
    cursor: Option<String>,
    source_complete: bool,
) -> Value {
    let mut map = serde_json::Map::new();
    map.insert(field.to_string(), Value::Array(values));
    map.insert(
        "complete".into(),
        Value::Bool(cursor.is_none() && source_complete),
    );
    map.insert(
        "truncated".into(),
        Value::Bool(cursor.is_some() || !source_complete),
    );
    map.insert(
        "cursor".into(),
        cursor.map(Value::String).unwrap_or(Value::Null),
    );
    Value::Object(map)
}
fn text_result(field: &str, content: String, end: usize, cursor: Option<String>) -> Value {
    let mut map = serde_json::Map::new();
    map.insert(field.to_string(), Value::String(content));
    map.insert("end_byte".into(), json!(end));
    map.insert("complete".into(), Value::Bool(cursor.is_none()));
    map.insert("truncated".into(), Value::Bool(cursor.is_some()));
    map.insert(
        "cursor".into(),
        cursor.map(Value::String).unwrap_or(Value::Null),
    );
    Value::Object(map)
}
fn value_path(v: &Value) -> String {
    v.get("path")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase()
}
fn cap_chars(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    value[..utf8_end(value, max_bytes)].to_string()
}
fn utf8_end(value: &str, mut end: usize) -> usize {
    end = end.min(value.len());
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    end
}
fn valid_object_id(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::temp_dir;

    fn bundle(root: &Path) -> Bundle {
        let limits = Limits::default();
        Bundle {
            schema_version: super::super::SCHEMA_VERSION,
            nonce: "test-nonce".into(),
            root: fs::canonicalize(root)
                .unwrap()
                .to_string_lossy()
                .to_string(),
            vcs: VcsKind::Git,
            change_label: "working tree".into(),
            status_summary: "clean".into(),
            change: Some("abcdef".into()),
            limits: limits.clone(),
            initial_stamp: initial_stamp(root, &limits).unwrap(),
        }
    }

    #[test]
    fn read_list_search_and_change_are_bounded_and_paged() {
        let dir = temp_dir("evidence-core");
        fs::write(dir.as_path().join("a.txt"), "alpha\nbeta alpha\n").unwrap();
        fs::create_dir(dir.as_path().join("sub")).unwrap();
        fs::write(dir.as_path().join("sub").join("b.txt"), "alpha\n").unwrap();
        let mut core = Core::new(bundle(dir.as_path())).unwrap();
        let listed = core.call("repository_list", &json!({"limit":1})).unwrap();
        assert_eq!(listed["entries"].as_array().unwrap().len(), 1);
        assert!(listed["truncated"].as_bool().unwrap());
        let searched = core
            .call("repository_search", &json!({"query":"alpha","limit":2}))
            .unwrap();
        assert_eq!(searched["matches"].as_array().unwrap().len(), 2);
        let search_cursor = searched["cursor"].as_str().unwrap();
        let searched_tail = core
            .call(
                "repository_search",
                &json!({"cursor":search_cursor,"limit":2}),
            )
            .unwrap();
        assert_eq!(searched_tail["matches"].as_array().unwrap().len(), 1);
        assert_eq!(searched_tail["complete"], true);
        let read = core
            .call(
                "repository_read",
                &json!({"path":"a.txt","start_line":2,"line_count":1}),
            )
            .unwrap();
        assert_eq!(read["lines"][0]["text"], "beta alpha");
        let first = core
            .call("repository_change", &json!({"limit_bytes":3}))
            .unwrap();
        assert_eq!(first["content"], "abc");
        let cursor = first["cursor"].as_str().unwrap();
        let second = core
            .call(
                "repository_change",
                &json!({"cursor":cursor,"limit_bytes":3}),
            )
            .unwrap();
        assert_eq!(second["content"], "def");
        assert_eq!(second["complete"], true);
    }

    #[test]
    fn rejects_parent_absolute_ads_and_devices() {
        let dir = temp_dir("evidence-paths");
        fs::write(dir.as_path().join("ok.txt"), "ok").unwrap();
        fs::create_dir(dir.as_path().join(".git")).unwrap();
        fs::write(dir.as_path().join(".git").join("config"), "secret").unwrap();
        let core = Core::new(bundle(dir.as_path())).unwrap();
        for bad in [
            "../x",
            r"C:\Windows\win.ini",
            "ok.txt:secret",
            "NUL",
            "COM1.txt",
            ".git/config",
        ] {
            // The free resolver is what every bounded worker calls; `Core` no longer wraps it.
            let limit = core.bundle.limits.max_path_bytes as usize;
            assert!(
                resolve_existing_bounded(&core.root, limit, bad, false).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn listing_and_search_never_follow_a_junction() {
        let dir = temp_dir("evidence-junction-root");
        let outside = temp_dir("evidence-junction-outside");
        fs::write(outside.as_path().join("secret.txt"), "outside-secret").unwrap();
        let system = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
        let link = dir.as_path().join("escape");
        let made = std::process::Command::new(format!(r"{system}\System32\cmd.exe"))
            .args(["/c", "mklink", "/J"])
            .arg(&link)
            .arg(outside.as_path())
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !made {
            eprintln!("skipping: could not create junction");
            return;
        }
        let mut core = Core::new(bundle(dir.as_path())).unwrap();
        let listed = core.call("repository_list", &json!({})).unwrap();
        assert_eq!(listed["entries"][0]["type"], "link");
        let searched = core
            .call("repository_search", &json!({"query":"outside-secret"}))
            .unwrap();
        assert!(searched["matches"].as_array().unwrap().is_empty());
        assert!(core
            .call("repository_read", &json!({"path":"escape/secret.txt"}))
            .is_err());
    }

    #[test]
    fn omitted_arguments_can_be_an_empty_object() {
        let dir = temp_dir("evidence-scope");
        let mut core = Core::new(bundle(dir.as_path())).unwrap();
        assert_eq!(
            core.call("repository_scope", &json!({})).unwrap()["nonce"],
            "test-nonce"
        );
    }

    #[test]
    fn drift_stamp_is_observed_once_per_service_turn() {
        let dir = temp_dir("evidence-drift-cache");
        fs::write(dir.as_path().join("before.txt"), "before").unwrap();
        let mut core = Core::new(bundle(dir.as_path())).unwrap();

        fs::write(dir.as_path().join("after.txt"), "after").unwrap();
        let first = core.call("repository_scope", &json!({})).unwrap();
        assert_eq!(first["drifted"], true);
        let observed = first["current_stamp"].clone();

        fs::write(dir.as_path().join("later.txt"), "later").unwrap();
        let second = core.call("repository_scope", &json!({})).unwrap();
        assert_eq!(second["current_stamp"], observed);
        assert_eq!(second["drifted"], true);
    }

    fn ok_output(bytes: &[u8]) -> ReadOutput {
        ReadOutput {
            resolved: PathBuf::from("x"),
            bytes: bytes.to_vec(),
        }
    }

    // The watchdog must stop waiting on a read that blocks past the budget, and — because the
    // abandoned worker borrows no shared state — a later read on the same pool must still succeed.
    #[test]
    fn read_watchdog_fires_then_a_later_read_succeeds() {
        static LIVE: AtomicUsize = AtomicUsize::new(0);
        let watchdog = ReadWatchdog {
            budget: Duration::from_millis(200),
            attempt_cap: Duration::from_millis(60),
            backoff: Duration::from_millis(5),
            max_attempts: MAX_READ_ATTEMPTS,
            live: &LIVE,
            cap: READ_WORKER_CAP,
        };
        let stalled = bounded_attempts(&watchdog, Instant::now(), "slow", || {
            || {
                std::thread::sleep(Duration::from_millis(600));
                Ok(ok_output(b""))
            }
        });
        assert_eq!(stalled.unwrap_err().code, "read_timeout");
        let fast = bounded_attempts(&watchdog, Instant::now(), "fast", || {
            || Ok(ok_output(b"hi"))
        });
        assert_eq!(fast.unwrap().bytes, b"hi");
    }

    // A transient (retryable) failure on the first attempt recovers silently on the second, within
    // the budget, and a fresh worker is built for the retry.
    #[test]
    fn read_retry_recovers_from_a_transient_failure() {
        static LIVE: AtomicUsize = AtomicUsize::new(0);
        let watchdog = ReadWatchdog {
            budget: Duration::from_millis(500),
            attempt_cap: Duration::from_millis(200),
            backoff: Duration::from_millis(5),
            max_attempts: MAX_READ_ATTEMPTS,
            live: &LIVE,
            cap: READ_WORKER_CAP,
        };
        let mut built = 0u32;
        let result = bounded_attempts(&watchdog, Instant::now(), "flaky", || {
            built += 1;
            let n = built;
            move || {
                if n == 1 {
                    Err(ReadFailure {
                        code: "read_failed",
                        message: "sharing violation".into(),
                        retryable: true,
                    })
                } else {
                    Ok(ok_output(b"ok"))
                }
            }
        });
        assert_eq!(result.unwrap().bytes, b"ok");
        assert_eq!(
            built, 2,
            "should build a fresh worker for exactly one retry"
        );
    }

    // Raw sharing/lock OS errors classify as retryable; a genuine not-found does not (findings
    // f10/f14). This is the classification `read_job` applies at every blocking stage.
    #[test]
    fn transient_os_errors_are_classified_retryable() {
        assert!(is_transient_io(&io::Error::from_raw_os_error(32)));
        assert!(is_transient_io(&io::Error::from_raw_os_error(33)));
        assert!(!is_transient_io(&io::Error::from_raw_os_error(2)));
        let sharing = classify_read_io(
            "read_failed",
            "cannot open",
            &io::Error::from_raw_os_error(32),
        );
        assert!(sharing.retryable);
        assert_eq!(sharing.code, "read_failed");
        assert!(
            !classify_read_io(
                "read_failed",
                "cannot open",
                &io::Error::from_raw_os_error(2)
            )
            .retryable
        );
    }

    // At the worker cap, a read refuses in-band without spawning another worker (finding f3).
    #[test]
    fn read_worker_cap_refuses_without_spawning() {
        static LIVE: AtomicUsize = AtomicUsize::new(4);
        let watchdog = ReadWatchdog {
            budget: Duration::from_millis(500),
            attempt_cap: Duration::from_millis(200),
            backoff: Duration::from_millis(5),
            max_attempts: MAX_READ_ATTEMPTS,
            live: &LIVE,
            cap: 4,
        };
        let mut built = 0u32;
        let result = bounded_attempts(&watchdog, Instant::now(), "capped", || {
            built += 1;
            || Ok(ok_output(b""))
        });
        assert_eq!(result.unwrap_err().code, "read_unavailable");
        assert_eq!(built, 0, "no worker is built once the cap is reached");
        assert_eq!(
            LIVE.load(Ordering::Acquire),
            4,
            "the live count is untouched"
        );
    }

    // The read budget is coupled below the Codex ceiling, and even a worst-case max-attempt read
    // finishes under it (finding f4). The `const _` assertions guarantee the raw-millisecond
    // relationship at compile time; this exercises the live `READ_WATCHDOG` values a read actually
    // uses, including the worst-case attempt sum.
    #[test]
    fn read_budget_stays_under_the_codex_ceiling() {
        let ceiling = Duration::from_secs(CODEX_TOOL_TIMEOUT_SECS);
        let watchdog = &READ_WATCHDOG;
        assert!(watchdog.budget + Duration::from_millis(READ_CEILING_MARGIN_MS) <= ceiling);
        assert!(watchdog.attempt_cap <= watchdog.budget);
        assert!(watchdog.attempt_cap * watchdog.max_attempts + watchdog.backoff < ceiling);
    }

    // #71: a request whose budget was consumed while it waited behind a slower one is refused at
    // dispatch, before any work, rather than served to a client that has already abandoned it. The
    // refusal must also not consume the caller's call budget: it did nothing.
    #[test]
    fn a_request_past_its_budget_is_refused_before_any_work() {
        let dir = temp_dir("evidence-expired");
        fs::write(dir.as_path().join("a.txt"), "hello\n").unwrap();
        let mut core = Core::new(bundle(dir.as_path())).unwrap();
        let stale = Instant::now() - Duration::from_millis(REQUEST_BUDGET_MS + 1);
        let error = core
            .call_with_receipt("repository_read", &json!({"path":"a.txt"}), stale)
            .unwrap_err();
        assert_eq!(error.code, "request_expired");
        assert_eq!(core.calls, 0, "an expired request spends no call budget");
        // The service is still usable: expiry is per-request, not a poisoned core.
        let ok = core
            .call("repository_read", &json!({"path":"a.txt"}))
            .unwrap();
        assert_eq!(ok["total_lines"], json!(1));
    }

    // #71: every operation is bounded, not just `repository_read`. Each of these holds the
    // single-threaded request loop, so each must refuse rather than run when the budget is gone —
    // the property that stops one slow call making the *next* one late past the client ceiling.
    #[test]
    fn every_operation_is_bounded_by_the_request_budget() {
        let dir = temp_dir("evidence-all-bounded");
        fs::write(dir.as_path().join("a.txt"), "hello\n").unwrap();
        let mut core = Core::new(bundle(dir.as_path())).unwrap();
        let stale = Instant::now() - Duration::from_millis(REQUEST_BUDGET_MS + 1);
        for (tool, args) in [
            ("repository_scope", json!({})),
            ("repository_list", json!({})),
            ("repository_search", json!({"query":"hello"})),
            ("repository_read", json!({"path":"a.txt"})),
            ("repository_change", json!({})),
            ("repository_history", json!({})),
            ("repository_revision", json!({"id":"0".repeat(40)})),
        ] {
            let error = core.call_with_receipt(tool, &args, stale).unwrap_err();
            assert_eq!(error.code, "request_expired", "{tool} ran past its budget");
        }
    }

    // The cooperative check takes the *tighter* of the per-operation timeout and the request
    // budget, so a queued request cannot be handed a full fresh operation timeout on top of its
    // wait. Both ceilings are anchored at the same receipt instant.
    #[test]
    fn the_deadline_honours_both_ceilings() {
        // An operation timeout far beyond the request budget must not extend the request.
        let mut limits = Limits {
            operation_timeout_ms: REQUEST_BUDGET_MS * 10,
            ..Default::default()
        };
        let spent = Instant::now() - Duration::from_millis(REQUEST_BUDGET_MS + 50);
        assert_eq!(
            deadline(spent, &limits).unwrap_err().code,
            "deadline_exceeded"
        );
        // And the per-operation timeout still binds when it is the tighter of the two.
        limits.operation_timeout_ms = 10;
        let recent = Instant::now() - Duration::from_millis(50);
        assert_eq!(
            deadline(recent, &limits).unwrap_err().code,
            "deadline_exceeded"
        );
        assert!(deadline(Instant::now(), &limits).is_ok());
    }

    // A stalled walk returns a fast in-band error like a stalled read, and leaves the pool usable.
    #[test]
    fn a_stalled_walk_times_out_in_band() {
        static LIVE: AtomicUsize = AtomicUsize::new(0);
        let watchdog = ReadWatchdog {
            budget: Duration::from_millis(200),
            attempt_cap: Duration::from_millis(200),
            backoff: Duration::from_millis(0),
            max_attempts: 1,
            live: &LIVE,
            cap: READ_WORKER_CAP,
        };
        let stalled: Result<Vec<PathBuf>, EvidenceError> =
            bounded_attempts(&watchdog, Instant::now(), "walk", || {
                || {
                    std::thread::sleep(Duration::from_millis(600));
                    Ok(Vec::new())
                }
            });
        assert_eq!(stalled.unwrap_err().code, "read_timeout");
        assert_eq!(
            WALK_WATCHDOG.max_attempts, 1,
            "a stalled walk is not retried into the same blocked syscall"
        );
    }

    // Round-1 f4: drive the *real* routes to their bound, rather than asserting on
    // `bounded_attempts` in isolation and assuming the routes are wired to it. The watchdog is
    // injected with a budget of zero, so both walk-bearing routes must refuse without depending on
    // how fast the filesystem happens to be.
    #[test]
    fn the_walk_bearing_routes_are_bounded_at_the_route() {
        static LIVE: AtomicUsize = AtomicUsize::new(0);
        static SPENT: ReadWatchdog = ReadWatchdog {
            budget: Duration::ZERO,
            attempt_cap: Duration::ZERO,
            backoff: Duration::ZERO,
            max_attempts: 1,
            live: &LIVE,
            cap: WALK_WORKER_CAP,
        };
        let dir = temp_dir("evidence-route-bounded");
        fs::write(dir.as_path().join("a.txt"), "hello\n").unwrap();
        let mut core = Core::new(bundle(dir.as_path())).unwrap();
        core.walk_watchdog = &SPENT;
        for (tool, args) in [
            ("repository_list", json!({})),
            ("repository_search", json!({"query":"hello"})),
        ] {
            let error = core.call(tool, &args).unwrap_err();
            assert_eq!(error.code, "read_timeout", "{tool} ran unbounded");
        }
        // With the production watchdog the same calls succeed, so the assertion above is the bound
        // firing rather than the route being broken.
        core.walk_watchdog = &WALK_WATCHDOG;
        assert!(core.call("repository_list", &json!({})).is_ok());
        assert!(core
            .call("repository_search", &json!({"query":"hello"}))
            .is_ok());
    }

    // Round-1 f1: a wedged walk must not be able to spend the read pool's capacity.
    //
    // The exhaustion half runs against an *injected* pool with its own counter, never the live
    // statics. Filling a global counter would have made every concurrently-running test that lists
    // or searches fail — which is what the first draft of this test did, and is the same class of
    // cross-test interference as issue #72.
    #[test]
    fn a_stalled_walk_cannot_starve_reads() {
        assert!(
            !std::ptr::eq(WALK_WATCHDOG.live, READ_WATCHDOG.live),
            "walks must not draw on the read pool"
        );
        const { assert!(WALK_WATCHDOG.cap < READ_WATCHDOG.cap) };
        // The drift stamp deliberately stays on the read pool: `read` computes it inline, so a walk
        // pool shared with it would let two stalled walks refuse every first read of a turn.
        assert!(std::ptr::eq(STAMP_WATCHDOG.live, READ_WATCHDOG.live));

        static FULL: AtomicUsize = AtomicUsize::new(WALK_WORKER_CAP);
        static EXHAUSTED: ReadWatchdog = ReadWatchdog {
            budget: Duration::from_millis(REQUEST_BUDGET_MS),
            attempt_cap: Duration::from_millis(REQUEST_BUDGET_MS),
            backoff: Duration::ZERO,
            max_attempts: 1,
            live: &FULL,
            cap: WALK_WORKER_CAP,
        };
        let dir = temp_dir("evidence-pool-isolation");
        fs::write(dir.as_path().join("a.txt"), "hello\n").unwrap();
        let mut core = Core::new(bundle(dir.as_path())).unwrap();
        core.walk_watchdog = &EXHAUSTED;
        let refused = core.call("repository_list", &json!({})).unwrap_err();
        assert_eq!(refused.code, "read_unavailable");
        // Reads draw on their own pool and are untouched by a walk pool at its cap.
        let read = core
            .call("repository_read", &json!({"path":"a.txt"}))
            .unwrap();
        assert_eq!(read["total_lines"], json!(1));
    }

    // Round-1 f2: a child process must be given less than the request has left, because the runner
    // keeps draining its pipes after the timeout fires. A budget that ignored the drain could answer
    // a full `DRAIN_GRACE` past the deadline it claimed to honour.
    #[test]
    fn a_child_is_never_given_the_whole_remaining_budget() {
        // Both figures are read from a live clock, so this asserts the reservation, not an exact
        // equality that the microseconds between the two calls would break.
        let fresh = Instant::now();
        // Whole first: both are read from a live clock, so sampling the child budget first would
        // measure it against a larger remainder than the comparison below then sees.
        let whole = remaining_budget(fresh);
        let child = child_budget(fresh);
        assert!(child < whole);
        assert!(whole - child >= crate::reviewer::DRAIN_GRACE);
        assert!(!child.is_zero(), "a fresh request can still run a child");
        // Once too little remains to run a child *and* drain it, nothing is started at all — while
        // the request itself still has budget left to answer with.
        let nearly_spent = Instant::now() - Duration::from_millis(REQUEST_BUDGET_MS)
            + crate::reviewer::DRAIN_GRACE;
        assert!(child_budget(nearly_spent).is_zero());
        assert!(!remaining_budget(nearly_spent).is_zero());
    }

    // A read reached before any scope must still populate the drift cache, so the stamp is computed
    // once and reused — the worker preserving `observed_stamp` semantics (finding f7).
    #[test]
    fn a_first_read_populates_the_drift_cache() {
        let dir = temp_dir("evidence-read-first");
        fs::write(dir.as_path().join("a.txt"), "hello\n").unwrap();
        let mut core = Core::new(bundle(dir.as_path())).unwrap();
        let _ = core
            .call("repository_read", &json!({"path":"a.txt"}))
            .unwrap();
        // The read cached the stamp, so both later scopes report the same value it computed.
        fs::write(dir.as_path().join("b.txt"), "new").unwrap();
        let scope = core.call("repository_scope", &json!({})).unwrap();
        let scope2 = core.call("repository_scope", &json!({})).unwrap();
        assert_eq!(scope["current_stamp"], scope2["current_stamp"]);
    }
}
