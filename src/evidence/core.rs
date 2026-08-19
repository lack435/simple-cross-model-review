use std::collections::{HashMap, VecDeque};
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant, UNIX_EPOCH};

use serde_json::{json, Value};

use super::{Bundle, Drift, EvidenceError, Limits, StampMethod, VcsKind, CODEX_TOOL_TIMEOUT_SECS};

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
///
/// The cap is three because a Git-scoped search spends two walk workers in sequence — one to
/// resolve the base (which decides the explicit-file short-circuit, so it cannot be folded into
/// the second) and one to check the enumeration against the filesystem — leaving one slot of
/// headroom for the abandoned predecessor of a retry that is not coming. It is still much smaller
/// than the read cap, for the reasons above.
static LIVE_WALK_WORKERS: AtomicUsize = AtomicUsize::new(0);
const WALK_WORKER_CAP: usize = 3;

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
    source: Option<Vec<String>>,
    cancel: &Arc<AtomicBool>,
    received_at: Instant,
) -> Result<String, EvidenceError> {
    let root = root.to_path_buf();
    let limits = limits.clone();
    let cancel = Arc::clone(cancel);
    bounded_attempts(&STAMP_WATCHDOG, received_at, "drift-stamp", move || {
        let root = root.clone();
        let limits = limits.clone();
        let cancel = Arc::clone(&cancel);
        let source = source.clone();
        move || {
            let stamp = match &source {
                Some(paths) => {
                    accept_git_paths(&root, paths, limits.max_path_bytes as usize, &cancel)
                        .and_then(|(accepted, dropped)| {
                            // A stamp over a set that lost paths is not comparable with one that
                            // did not: the next scan may lose a different set, and the difference
                            // would read as drift. Same rule as a short enumeration.
                            if dropped {
                                return Err(EvidenceError::new(
                                    "read_failed",
                                    "some enumerated paths could not be examined, so no \
                                     comparable stamp could be taken",
                                ));
                            }
                            let rows: Vec<String> = accepted
                                .iter()
                                .map(|entry| stamp_row(&entry.relative, entry.meta.as_ref()))
                                .collect();
                            digest_rows(&rows)
                        })
                }
                None => tree_stamp(&root, &limits, &cancel),
            };
            stamp.map_err(|e| ReadFailure::fatal(e.code, e.message))
        }
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
    /// The turn's single drift observation, successful or not. `Option<String>` could not hold the
    /// second case, so a failed scan was re-run on every read; `Drift::Unavailable` is cached like
    /// any other answer.
    observed_stamp: Option<Drift>,
    /// Which scan the turn's observation actually used. Not derivable from `bundle.vcs`: a Git
    /// bundle with no Git binary falls back to the filesystem walk, and `repository_scope` must
    /// describe the scan that ran rather than the one the VCS implies.
    observed_method: Option<StampMethod>,
    cancel: Arc<AtomicBool>,
    /// The nonce-bound side-channel file each `repository_diff` call appends a serve-record to, for
    /// the parent's `captured:` line and fail-closed gate (retire-capture-modes mechanism 3). `None`
    /// in tests and whenever the parent supplied no path; a missing sink is silent, never fatal — a
    /// review that cannot record its serve-records fails closed at the gate, not here.
    serve_record: Option<PathBuf>,
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
            observed_method: None,
            cancel,
            serve_record: None,
            walk_watchdog: &WALK_WATCHDOG,
        })
    }

    /// Point `repository_diff` at the nonce-bound serve-record file the parent will read. Set once,
    /// in `serve_stdio`, before the request loop starts.
    pub fn set_serve_record(&mut self, path: PathBuf) {
        self.serve_record = Some(path);
    }

    /// Append one serve-record line (JSON) to the side-channel file, best-effort. A write failure is
    /// swallowed: the review continues and the gate fails closed on the missing record rather than
    /// crashing the evidence server mid-turn.
    fn append_serve_record(&self, record: &Value) {
        let Some(path) = &self.serve_record else {
            return;
        };
        if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(file, "{record}");
        }
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
            "repository_diff" => {
                require_only(args, &["base", "head", "path", "cursor", "limit_bytes"])?;
                self.diff(args, received_at)
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
        let observed = self.current_stamp(received_at)?;
        let (drifted, reason) =
            crate::evidence::compare_drift(&self.bundle.initial_stamp, &observed);
        // Which file set the recursive scans cover, in words. Without it a reviewer that searches
        // for a vendored symbol, finds nothing, and concludes the code does not exist has no way
        // to know it was looking at a smaller tree than it thought. Taken from the scan that
        // actually ran, since a Git bundle with no Git binary falls back to the walk.
        let scan_scope = self
            .observed_method
            .unwrap_or(match self.bundle.vcs {
                VcsKind::Git => StampMethod::Git,
                VcsKind::Perforce => StampMethod::Filesystem,
            })
            .scan_scope();
        Ok(json!({
            "schema_version": self.bundle.schema_version,
            "nonce": self.bundle.nonce,
            "root": self.root.to_string_lossy(),
            "vcs": self.bundle.vcs,
            "change_label": self.bundle.change_label,
            "status_summary": self.bundle.status_summary,
            "limits": self.bundle.limits,
            "excluded_directory_names": [".git", ".hg", ".svn", "target", "dist"],
            "initial_stamp": self.bundle.initial_stamp.sha256(),
            "current_stamp": observed.sha256(),
            "drifted": drifted,
            "drift_unavailable": reason,
            "scan_scope": scan_scope,
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
        let (mut entries, complete) = run_bounded_walk(
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
                    // As in `walk_files`: a directory bigger than the budget is truncated with the
                    // completeness flag cleared, not refused.
                    if entries.len() >= max_files {
                        return Ok((entries, false));
                    }
                }
                Ok((entries, true))
            },
        )?;
        entries.sort_by_key(value_path);
        self.first_page("repository_list", entries, limit, "entries", complete)
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
        let (files, walk_complete) = self.resolve_and_walk(&path, received_at)?;
        let mut matches = Vec::new();
        let mut source_complete = walk_complete;
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
        // Cap the returned window by the served-page ceiling's encoded size when one is set, so a
        // large file read is not truncated/diverted by a capped MCP client (issue #114, f2). The
        // `complete`/`truncated` fields below already report a short window, which the reviewer
        // continues from with `start_line`; no cursor is added (repository_read has none).
        let end = match self.bundle.page_bytes_ceiling {
            Some(ceiling) => encoded_line_window_end(&all, begin, end, ceiling as usize),
            None => end,
        };
        let mut lines = Vec::with_capacity(end.saturating_sub(begin));
        // Set when a single line's own encoded size exceeds the ceiling. `encoded_line_window_end`
        // always yields at least one line to guarantee progress, so a lone quote-heavy line (under
        // the raw `max_line_bytes` cap but over the ceiling once escaped) would otherwise be served
        // whole and diverted by a capped MCP client. Truncate that one line's text to fit and flag
        // the window truncated, so the reviewer sees most of it and knows it was cut, rather than
        // losing the whole read (issue #114). Every other line already fits — the window's total
        // encoded size is bounded, so no line but a forced oversized first can be cut here.
        let mut line_truncated = false;
        for (offset, line) in all[begin..end].iter().enumerate() {
            if line.len() > self.bundle.limits.max_line_bytes as usize {
                return Err(EvidenceError::new(
                    "line_too_long",
                    format!("'{path}' contains a line over the configured cap"),
                ));
            }
            let text: &str = match self.bundle.page_bytes_ceiling {
                Some(ceiling) => {
                    let cut = encoded_bounded_end(line, 0, line.len(), ceiling as usize);
                    if cut < line.len() {
                        line_truncated = true;
                    }
                    &line[..cut]
                }
                None => line,
            };
            lines.push(json!({"line": begin + offset + 1, "text": text}));
        }
        let complete = end == all.len() && !line_truncated;
        let fingerprint = crate::digest::Fingerprint::of(&bytes)
            .ok_or_else(|| EvidenceError::new("digest_unavailable", "SHA-256 is unavailable"))?;
        // Only after the content is known good do we compute the (cached) drift stamp. The walk is
        // watchdog-bounded (below) so a first read whose stamp stalls still cannot hang the loop.
        // The reason travels with the verdict here as well as in `repository_scope`, because scope
        // is recommended to the reviewer rather than required and a bare null explains nothing.
        let (drifted, drift_unavailable) = self.drift(received_at)?;
        Ok(json!({
            "path": relative_slash(&self.root, &output.resolved)?,
            "bytes": bytes.len(),
            "sha256": fingerprint.sha256,
            "total_lines": all.len(),
            "lines": lines,
            "complete": complete,
            "truncated": !complete,
            "cursor": Value::Null,
            "drifted": drifted,
            "drift_unavailable": drift_unavailable,
        }))
    }

    /// This turn's drift verdict as the wire carries it: `(drifted, reason)`, where a null verdict
    /// always has a reason beside it.
    fn drift(&mut self, received_at: Instant) -> Result<(Value, Value), EvidenceError> {
        let observed = self.current_stamp(received_at)?;
        let (drifted, reason) =
            crate::evidence::compare_drift(&self.bundle.initial_stamp, &observed);
        Ok((
            drifted.map(Value::from).unwrap_or(Value::Null),
            reason.map(Value::from).unwrap_or(Value::Null),
        ))
    }

    fn current_stamp(&mut self, received_at: Instant) -> Result<Drift, EvidenceError> {
        if let Some(stamp) = &self.observed_stamp {
            return Ok(stamp.clone());
        }
        // The drift-stamp walk runs under the watchdog so a stalled `read_dir`/`symlink_metadata`
        // cannot hang the request loop (#61 finding f4). It bounds the walk for both `read` and
        // `scope`, and stays on the *read* pool: `read` computes it inline, so moving it to the
        // small walk pool would let two stalled walks refuse every first read of a turn.
        // `list`/search-base `walk_files` are no longer the exception here -- they are bounded on
        // their own pool as of #71. Since #86 the Git enumeration that precedes it runs on the
        // child pool instead, sequentially, so neither stage waits on the other's pool.
        let (current_stamp, method) = observe_drift(
            &self.root,
            &self.bundle.limits,
            self.bundle.vcs,
            &self.cancel,
            received_at,
        )?;
        self.observed_stamp = Some(current_stamp.clone());
        self.observed_method = Some(method);
        Ok(current_stamp)
    }

    /// End offset for a text page: the requested raw byte `limit` from `start`, on a UTF-8 boundary,
    /// tightened further so the JSON-escaped content fits the bundle's served-page ceiling when one
    /// is set. The reviewer's MCP client caps the *serialized* tool result, not the raw text (issue
    /// #114), so an escape-heavy diff (quotes, backslashes, control bytes) must be paged against its
    /// encoded size, not its byte length. The request is never rejected — a large `limit_bytes` is
    /// just served in more, smaller slices, each with a continuation cursor. Bundles without a
    /// ceiling (Codex) are bounded only by the requested limit.
    fn page_end(&self, text: &str, start: usize, limit: usize) -> usize {
        let hard = utf8_end(text, start.saturating_add(limit).min(text.len()));
        match self.bundle.page_bytes_ceiling {
            Some(ceiling) => encoded_bounded_end(text, start, hard, ceiling as usize),
            None => hard,
        }
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

    /// Diff the live working tree (or a commit range) against a base, on demand (retire-capture-modes
    /// mechanism 1). Replaces the pre-rendered `repository_change` blob: the change under review is
    /// derived live here rather than captured before the turn.
    fn diff(
        &mut self,
        args: &serde_json::Map<String, Value>,
        received_at: Instant,
    ) -> Result<Value, EvidenceError> {
        if self.bundle.vcs != VcsKind::Git {
            return Err(EvidenceError::new(
                "unsupported",
                "repository_diff is unsupported for Perforce",
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
            return self.diff_cursor(&cursor, limit);
        }
        let base = optional_string(args, "base")?.unwrap_or_else(|| "branch-base".to_string());
        let head = optional_string(args, "head")?.unwrap_or_else(|| "worktree".to_string());
        let base_ep = DiffEndpoint::parse_base(&base)?;
        let head_ep = DiffEndpoint::parse_head(&head)?;
        let path = optional_string(args, "path")?.unwrap_or_default();
        if !path.is_empty() {
            self.validate_relative(&path)?;
        }
        // The canonical diff — the one a formal approval must be served (mechanism 4) — is the whole
        // working tree against the branch's fork point, unnarrowed. Anything else is exploratory.
        let canonical = matches!(base_ep, DiffEndpoint::BranchBase)
            && matches!(head_ep, DiffEndpoint::Worktree)
            && path.is_empty();
        let root = self.root.clone();
        let limits = self.bundle.limits.clone();
        let cancel = Arc::clone(&self.cancel);
        let composed = run_bounded_walk(&CHILD_WATCHDOG, "git diff", received_at, move || {
            compose_diff(
                &root,
                &base_ep,
                &head_ep,
                &path,
                &limits,
                &cancel,
                received_at,
            )
        })?;
        self.first_diff_page(canonical, composed, limit)
    }

    /// First page of a `repository_diff`, recording the serve-record the parent's `captured:` line
    /// and gate read (mechanism 3). A new operation id is minted here and carried in the cursor so
    /// every follow-up page records under the same operation; `terminal` says whether this page was
    /// the last, which is what lets the gate require the canonical diff was paged to its end (f1).
    fn first_diff_page(
        &mut self,
        canonical: bool,
        composed: ComposedDiff,
        limit: usize,
    ) -> Result<Value, EvidenceError> {
        let op = format!("{}-diff-{}", self.bundle.nonce, self.calls);
        let text = composed.text;
        let end = self.page_end(&text, 0, limit);
        let content = text[..end].to_string();
        let terminal = end >= text.len();
        let cursor = if terminal {
            None
        } else {
            Some(self.store_cursor(
                "repository_diff",
                vec![json!({
                    "text": text, "offset": end, "op": op,
                    "canonical": canonical, "complete": composed.complete
                })],
                0,
                true,
            ))
        };
        self.append_serve_record(&json!({
            "op": op,
            "canonical": canonical,
            "complete": composed.complete,
            "terminal": terminal,
            "base": composed.base_token,
            "base_source": composed.base_source,
            "files": composed.files,
            "insertions": composed.insertions,
            "deletions": composed.deletions,
            "untracked": composed.untracked_files,
        }));
        Ok(text_result("diff", content, end, cursor))
    }

    /// A continuation page of a `repository_diff`, recording under the operation id carried in the
    /// cursor. Its `terminal` flag closing an operation is what the gate reads as "the reviewer was
    /// served the whole canonical diff."
    fn diff_cursor(&mut self, token: &str, limit: usize) -> Result<Value, EvidenceError> {
        let state = self.take_cursor(token, "repository_diff")?;
        let item = state
            .values
            .first()
            .ok_or_else(|| EvidenceError::new("invalid_cursor", "malformed diff cursor"))?;
        let text = item
            .get("text")
            .and_then(Value::as_str)
            .ok_or_else(|| EvidenceError::new("invalid_cursor", "malformed diff cursor"))?
            .to_string();
        let offset = item
            .get("offset")
            .and_then(Value::as_u64)
            .ok_or_else(|| EvidenceError::new("invalid_cursor", "malformed diff cursor"))?
            as usize;
        let op = item
            .get("op")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let canonical = item
            .get("canonical")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let complete = item
            .get("complete")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let end = self.page_end(&text, offset, limit);
        let content = text[offset..end].to_string();
        let terminal = end >= text.len();
        let cursor = if terminal {
            None
        } else {
            Some(self.store_cursor(
                "repository_diff",
                vec![json!({
                    "text": text, "offset": end, "op": op,
                    "canonical": canonical, "complete": complete
                })],
                0,
                true,
            ))
        };
        self.append_serve_record(&json!({
            "op": op,
            "canonical": canonical,
            "complete": complete,
            "terminal": terminal,
            "continuation": true,
        }));
        Ok(text_result("diff", content, end, cursor))
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
    ) -> Result<(Vec<PathBuf>, bool), EvidenceError> {
        let root = self.root.clone();
        let limits = self.bundle.limits.clone();
        let cancel = Arc::clone(&self.cancel);
        let max_path_bytes = self.bundle.limits.max_path_bytes as usize;
        let raw = path.to_string();
        // Resolve first, in its own bounded worker, because an explicitly named file is answerable
        // without any enumeration at all -- and must be, or the documented contract ("naming a
        // file searches it, ignored or not") would be at the mercy of a Git timeout. A Git-scoped
        // directory search therefore spends two walk workers in sequence, which is what
        // `WALK_WORKER_CAP` is sized for.
        let (base, base_is_file) = {
            let root = root.clone();
            let raw = raw.clone();
            run_bounded_walk(
                self.walk_watchdog,
                &format!("search base '{path}'"),
                received_at,
                move || {
                    let base = resolve_existing_bounded(&root, max_path_bytes, &raw, false)
                        .map_err(ReadFailure::into_evidence)?;
                    // Decided *inside* the worker, and from a classified metadata call rather than
                    // `Path::is_file()`: on the request thread this syscall is unbounded, which
                    // would defeat the watchdog the rest of the search runs under, and `is_file()`
                    // silently turns an error into "not a file".
                    let meta = fs::metadata(&base).map_err(|e| {
                        classify_read_io("read_failed", "cannot stat the search base", &e)
                            .into_evidence()
                    })?;
                    Ok((base, meta.is_file()))
                },
            )?
        };
        // An explicitly named file is searched whether or not Git ignores it: naming a file is the
        // same opt-in as reading it, it cannot cost more than one file, and the alternative would
        // make `repository_search` weaker than `repository_read` for no benefit. Scoping applies to
        // directory bases, where the cost is unbounded and the noise is the problem.
        if base_is_file {
            return Ok((vec![base], true));
        }
        let source = if self.bundle.vcs == VcsKind::Git {
            let root = root.clone();
            let limits = limits.clone();
            let cancel = Arc::clone(&cancel);
            let outcome =
                run_bounded_walk(&CHILD_WATCHDOG, "git ls-files", received_at, move || {
                    Ok(super::git::reviewable_paths(
                        &root,
                        &limits,
                        &cancel,
                        received_at,
                    ))
                })?;
            match scan_source(outcome) {
                ScanSource::Refused(e) => return Err(e),
                other => other,
            }
        } else {
            ScanSource::Filesystem
        };
        // Git does not descend a submodule boundary for `--others`, so untracked files inside a
        // submodule are not in this list although the filesystem walk would have seen them. A
        // search that never looked at them must not report itself whole: "no matches" is what a
        // reviewer turns into "this code does not exist".
        let (paths, mut complete) = match source {
            ScanSource::Git(paths, complete) => {
                let submodules = paths.iter().any(|p| p == ".gitmodules");
                (Some(paths), complete && !submodules)
            }
            _ => (None, true),
        };
        let walk_root = root.clone();
        let (files, dropped) = run_bounded_walk(
            self.walk_watchdog,
            &format!("search '{path}'"),
            received_at,
            move || {
                let Some(paths) = &paths else {
                    return walk_files(&walk_root, &base, &limits, &cancel, received_at)
                        .map(|(files, complete)| (files, !complete));
                };
                let (accepted, dropped) =
                    accept_git_paths(&walk_root, paths, max_path_bytes, &cancel)?;
                let files = accepted
                    .into_iter()
                    // A path that vanished between the enumeration and the stat has nothing to read.
                    .filter(|entry| entry.meta.is_some())
                    .map(|entry| entry.path)
                    // `within`, not `Path::starts_with`: the latter compares components
                    // case-sensitively, and these two paths reach here by different routes -- Git's
                    // spelling joined onto the root, against a canonicalised base. On Windows those
                    // can differ in case for the same directory, which would silently return no
                    // matches for a subdirectory search.
                    .filter(|candidate| within(candidate, &base))
                    .collect();
                Ok((files, dropped))
            },
        )?;
        complete = complete && !dropped;
        Ok((files, complete))
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
) -> Result<(Vec<PathBuf>, bool), EvidenceError> {
    {
        if base.is_file() {
            return Ok((vec![base.to_path_buf()], true));
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
                    // Truncate rather than refuse: the response already has a completeness flag,
                    // and "the tree was bigger than the budget" and "there is nothing here" must
                    // not be the same answer to a reviewer (issue #86).
                    if files.len() >= limits.max_files as usize {
                        files.sort_by_key(|p| p.to_string_lossy().to_ascii_lowercase());
                        return Ok((files, false));
                    }
                }
            }
        }
        files.sort_by_key(|p| p.to_string_lossy().to_ascii_lowercase());
        Ok((files, true))
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
        let end = self.page_end(&text, offset, limit);
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
        let end = self.page_end(&text, 0, limit);
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
        let end = self.page_end(&text, offset, limit);
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

/// What a Git enumeration outcome means for the scan that asked for it.
enum ScanSource {
    /// Git answered; the flag says whether its answer was the whole of what was asked for.
    Git(Vec<String>, bool),
    /// No Git binary to ask — the one condition that is verified rather than inferred, and the
    /// only route to the filesystem walk.
    Filesystem,
    /// Git is here and did not answer. Walking the tree is the expensive wrong reply to that: it
    /// is the 400,000-file scan issue #86 exists to avoid, and it would run for a timeout, a
    /// corrupt index or a permissions failure alike.
    Refused(EvidenceError),
}

fn scan_source(outcome: Result<super::git::Enumeration, super::git::GitFailure>) -> ScanSource {
    match outcome {
        Ok(enumeration) => ScanSource::Git(enumeration.paths, enumeration.complete),
        Err(super::git::GitFailure::NoGit) => ScanSource::Filesystem,
        // Cancellation arrives here as `Refused` with the `cancelled` code, which every caller
        // re-raises rather than degrading: a cancelled request must not leave an observation
        // behind for the next one to trust.
        Err(other) => ScanSource::Refused(other.into_evidence()),
    }
}

/// What an ancestor directory of an enumerated path turned out to be. Memoised per directory, so
/// the three cases have to be distinguishable: a *missing* ancestor is an observation about a whole
/// subtree, and collapsing it into a hole would turn "this directory was deleted" — ordinary drift —
/// into unavailable drift.
#[derive(Clone, Copy, PartialEq)]
enum Ancestor {
    Usable,
    Missing,
    Hole,
}

impl Ancestor {
    fn of(path: &Path) -> Self {
        match fs::symlink_metadata(path) {
            Ok(meta) if metadata_is_reparse(&meta) => Self::Hole,
            Ok(_) => Self::Usable,
            // A deleted directory is the same class of event as a deleted file: the enumeration
            // named something that is gone, which the stamp records as `missing`. Collapsing it
            // into a hole would make a removed directory produce *unavailable* drift, not drift.
            Err(e) if e.kind() == io::ErrorKind::NotFound => Self::Missing,
            Err(_) => Self::Hole,
        }
    }
}

/// One path from Git's answer that survived every check the filesystem walk applies.
#[derive(Debug)]
struct Accepted {
    relative: String,
    path: PathBuf,
    /// `None` when the path vanished between the enumeration and the `stat`. A race this change
    /// introduces, and one the stamp records rather than aborting on: the absence *is* the
    /// observation. Search skips these, since there is nothing to read.
    meta: Option<fs::Metadata>,
}

/// Turn Git's answer into the file set this service will actually look at.
///
/// Git's output is untrusted input. `walk_files` guarantees that a scan never yields a path under
/// an excluded directory name, never follows a reparse point (its own or an ancestor's), and never
/// yields anything but a regular file — and its results are handed to reads as already-`Resolved`
/// targets, which skip the `is_file` guard. Every one of those checks is reapplied here, or a
/// tracked symlink, an in-root junction, a file under a tracked `dist/`, or a corrupt index entry
/// would widen what a scan reaches (issue #86, finding f2).
///
/// A path that fails is *skipped*, not fatal: Git listing something this service will not look at
/// is not an error, and one bad index entry must not lose the whole scan. The returned flag says
/// whether anything was dropped, so a scan that lost paths cannot report itself whole.
fn accept_git_paths(
    root: &Path,
    paths: &[String],
    max_path_bytes: usize,
    cancel: &AtomicBool,
) -> Result<(Vec<Accepted>, bool), EvidenceError> {
    let cancelled = || {
        cancel
            .load(Ordering::Acquire)
            .then(|| EvidenceError::new("cancelled", "the evidence enumeration was cancelled"))
    };
    // Checked before the loop as well as inside it: an empty enumeration would otherwise skip the
    // check entirely and hand back a "successful" empty answer for a cancelled request.
    if let Some(e) = cancelled() {
        return Err(e);
    }
    let mut accepted = Vec::with_capacity(paths.len());
    let mut dropped = false;
    // Ancestor directories are memoised: the path list is sorted, so siblings share ancestors, and
    // a per-file check alone would still let `root/junction/file` through.
    let mut ancestors: HashMap<String, Ancestor> = HashMap::new();
    'paths: for raw in paths {
        if let Some(e) = cancelled() {
            return Err(e);
        }
        let Ok(clean) = validate_relative_path(max_path_bytes, raw) else {
            dropped = true;
            continue;
        };
        let components: Vec<_> = clean.components().collect();
        if components.is_empty() {
            dropped = true;
            continue;
        }
        let mut current = root.to_path_buf();
        let mut key = String::new();
        let mut ancestor_missing = false;
        for component in &components[..components.len() - 1] {
            current.push(component.as_os_str());
            key.push_str(&component.as_os_str().to_string_lossy());
            key.push('/');
            let state = *ancestors
                .entry(key.clone())
                .or_insert_with(|| Ancestor::of(&current));
            match state {
                Ancestor::Usable => {}
                Ancestor::Missing => {
                    ancestor_missing = true;
                    break;
                }
                Ancestor::Hole => {
                    dropped = true;
                    continue 'paths;
                }
            }
        }
        // Joined from `clean` rather than continuing to push onto `current`, which stops at
        // whichever ancestor ended the loop early.
        let current = root.join(&clean);
        let relative = clean.to_string_lossy().replace('\\', "/");
        if ancestor_missing {
            accepted.push(Accepted {
                relative,
                path: current,
                meta: None,
            });
            continue;
        }
        match fs::symlink_metadata(&current) {
            Ok(meta) if metadata_is_reparse(&meta) || !meta.is_file() => dropped = true,
            Ok(meta) => accepted.push(Accepted {
                relative,
                path: current,
                meta: Some(meta),
            }),
            // An absent file is an observation, not a loss: the enumeration named it and it is
            // gone, which is exactly what the stamp should record. Any *other* metadata error --
            // a sharing violation, a permissions failure -- means the path could not be looked at,
            // which is a hole in the scan and has to clear the completeness flag instead.
            Err(e) if e.kind() == io::ErrorKind::NotFound => accepted.push(Accepted {
                relative,
                path: current,
                meta: None,
            }),
            Err(_) => dropped = true,
        }
    }
    Ok((accepted, dropped))
}

fn stamp_row(relative: &str, meta: Option<&fs::Metadata>) -> String {
    match meta {
        None => format!("{relative}\0missing"),
        Some(meta) => {
            let modified = meta
                .modified()
                .ok()
                .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            format!("{relative}\0{}\0{modified}", meta.len())
        }
    }
}

fn digest_rows(rows: &[String]) -> Result<String, EvidenceError> {
    let joined = rows.join("\n");
    let fp = crate::digest::Fingerprint::of(joined.as_bytes())
        .ok_or_else(|| EvidenceError::new("digest_unavailable", "SHA-256 is unavailable"))?;
    Ok(fp.sha256)
}

fn tree_stamp(root: &Path, limits: &Limits, cancel: &AtomicBool) -> Result<String, EvidenceError> {
    let start = Instant::now();
    let mut queue = VecDeque::from([root.to_path_buf()]);
    let mut rows = Vec::new();
    while let Some(dir) = queue.pop_front() {
        // Checked here as `walk_files` does, so a cancelled request cannot finish a scan and cache
        // an observation nobody is waiting for.
        if cancel.load(Ordering::Acquire) {
            return Err(EvidenceError::new(
                "cancelled",
                "the drift scan was cancelled",
            ));
        }
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
            if meta.is_dir() {
                queue.push_back(child);
                // Directories do not get rows, and do not count against the budget: an empty
                // directory appearing is not drift worth reporting, every directory that holds
                // anything is already represented by its files, and charging them to `max_files`
                // spends a budget meant for reviewable content on the tree's shape.
                continue;
            }
            let relative = relative_slash(root, &child)?;
            rows.push(stamp_row(&relative, Some(&meta)));
            if rows.len() > limits.max_files as usize {
                return Err(EvidenceError::new(
                    "limit_exceeded",
                    "drift scan exceeded file budget",
                ));
            }
        }
    }
    digest_rows(&rows)
}

/// The capture-time drift baseline, from the parent process.
///
/// Runs the same two bounded stages the service does, anchored at `Instant::now()` rather than a
/// request receipt. Before issue #86 this path had no watchdog at all — only the cooperative
/// `deadline()` between directories — so a stalled `read_dir` could hang a review before it
/// started, and a tree over `max_files` refused it outright. Both now end in `Drift::Unavailable`.
pub fn initial_stamp(root: &Path, limits: &Limits, vcs: VcsKind) -> Drift {
    let cancel = Arc::new(AtomicBool::new(false));
    match observe_drift(root, limits, vcs, &cancel, Instant::now()) {
        Ok((drift, _)) => drift,
        // Unreachable in practice: nothing cancels a capture-time scan. Kept total rather than
        // unwrapped so a future caller with a real flag cannot panic here.
        Err(e) => Drift::unavailable(e.to_string()),
    }
}

/// Observe the tree once: enumerate on the child pool, then stamp on the read pool.
///
/// Sequential, never nested — no worker waits on another pool's worker, so a wedged Git child can
/// only ever cost the child pool. Every failure becomes `Drift::Unavailable` with the reason
/// attached; the one exception is cancellation, which is not an observation and stays an error.
fn observe_drift(
    root: &Path,
    limits: &Limits,
    vcs: VcsKind,
    cancel: &Arc<AtomicBool>,
    received_at: Instant,
) -> Result<(Drift, StampMethod), EvidenceError> {
    let enumeration = if vcs == VcsKind::Git {
        let owned_root = root.to_path_buf();
        let owned_limits = limits.clone();
        let owned_cancel = Arc::clone(cancel);
        // The inner `Result` is deliberate: the watchdog must only report *its own* failure, so
        // the enumeration's classification survives to the match below instead of being flattened.
        let outcome = run_bounded_walk(&CHILD_WATCHDOG, "git ls-files", received_at, move || {
            Ok(super::git::reviewable_paths(
                &owned_root,
                &owned_limits,
                &owned_cancel,
                received_at,
            ))
        });
        match outcome {
            Ok(inner) => scan_source(inner),
            Err(e) if e.code == "cancelled" => return Err(e),
            // The Git method was the one attempted, so it is the one to report even though no
            // stamp came of it: a later request would attempt the same thing.
            Err(e) => return Ok((Drift::unavailable(e.to_string()), StampMethod::Git)),
        }
    } else {
        ScanSource::Filesystem
    };

    let (method, source) = match enumeration {
        // A hash of an arbitrary prefix of the tree is not a stamp: two scans truncating at
        // different points would compare unequal and report drift that never happened.
        ScanSource::Git(_, false) => {
            return Ok((
                Drift::unavailable(
                    "the Git file enumeration was short of the whole tree, so no comparable \
                     stamp could be taken",
                ),
                StampMethod::Git,
            ))
        }
        ScanSource::Git(paths, true) => (StampMethod::Git, Some(paths)),
        ScanSource::Filesystem => (StampMethod::Filesystem, None),
        ScanSource::Refused(e) if e.code == "cancelled" => return Err(e),
        ScanSource::Refused(e) => return Ok((Drift::unavailable(e.to_string()), StampMethod::Git)),
    };

    match run_bounded_stamp(root, limits, source, cancel, received_at) {
        Ok(sha256) => Ok((Drift::Stamp { method, sha256 }, method)),
        Err(e) if e.code == "cancelled" => Err(e),
        Err(e) => Ok((Drift::unavailable(e.to_string()), method)),
    }
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

/// The bytes `c` occupies inside a JSON string literal as serde_json emits it (surrounding quotes
/// excluded): `"` and `\` and the five short control escapes (`\b \t \n \f \r`) cost two, any other
/// control char (< 0x20) six (`\u00XX`), and every other character — ASCII or multibyte UTF-8 — its
/// own UTF-8 length (serde_json does not `\u`-escape non-ASCII). This is the quantity a capped MCP
/// client counts, not the raw byte length (issue #114, f1).
fn json_escaped_char_len(c: char) -> usize {
    match c {
        '"' | '\\' | '\n' | '\t' | '\r' | '\u{08}' | '\u{0c}' => 2,
        c if (c as u32) < 0x20 => 6,
        c => c.len_utf8(),
    }
}

/// Encoded byte length of `s` as a JSON string body (no surrounding quotes).
fn json_escaped_len(s: &str) -> usize {
    s.chars().map(json_escaped_char_len).sum()
}

/// Largest end offset in `start..hard_end` (a char boundary) whose JSON-escaped content fits
/// `max_encoded` bytes. Always advances at least one character when any remain, so a page can never
/// stall on a single expensive character — the smallest ceiling in use dwarfs one character's
/// six-byte worst case.
fn encoded_bounded_end(text: &str, start: usize, hard_end: usize, max_encoded: usize) -> usize {
    let mut used = 0usize;
    let mut end = start;
    for (i, c) in text[start..hard_end].char_indices() {
        let cost = json_escaped_char_len(c);
        if used + cost > max_encoded && end > start {
            break;
        }
        used = used.saturating_add(cost);
        end = start + i + c.len_utf8();
    }
    end
}

/// Largest line index in `begin..hard_end` whose lines, once JSON-encoded as `repository_read`
/// emits them, fit `max_encoded` bytes. Each line becomes a `{"line":N,"text":"…"}` object; the
/// per-line structural overhead is over-estimated so the window stays under the ceiling. Always
/// returns at least one line past `begin` when any remain, so a read cannot stall (issue #114, f2).
fn encoded_line_window_end(
    all: &[&str],
    begin: usize,
    hard_end: usize,
    max_encoded: usize,
) -> usize {
    // `{"line":<digits>,"text":"<escaped>"},` — the fixed punctuation is 20 bytes; 12 covers the
    // line-number digits and array comma with room to spare. Deliberately generous: better a page
    // slightly under the ceiling than one that serializes past it.
    const PER_LINE_OVERHEAD: usize = 32;
    let mut used = 0usize;
    let mut end = begin;
    for (offset, line) in all[begin..hard_end].iter().enumerate() {
        let cost = json_escaped_len(line).saturating_add(PER_LINE_OVERHEAD);
        if used + cost > max_encoded && end > begin {
            break;
        }
        used = used.saturating_add(cost);
        end = begin + offset + 1;
    }
    end
}
fn valid_object_id(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|b| b.is_ascii_hexdigit())
}

/// A resolved endpoint of a `repository_diff` (retire-capture-modes mechanism 1). The closed
/// sentinel set plus full-hex ids is what keeps model input off git's ref/option surface: nothing
/// symbolic reaches git — a base sentinel maps to a fixed, server-written token, and an id is
/// validated by `valid_object_id`.
#[derive(Clone, Debug)]
enum DiffEndpoint {
    Worktree,
    Index,
    Head,
    BranchBase,
    Commit(String),
}

impl DiffEndpoint {
    /// A base is a commit to diff *from*: `branch-base` (the fork point), `head`, or an id. The
    /// working tree and index are not commits, so they are not valid bases.
    fn parse_base(raw: &str) -> Result<Self, EvidenceError> {
        match raw {
            "branch-base" => Ok(Self::BranchBase),
            "head" => Ok(Self::Head),
            other if valid_object_id(other) => Ok(Self::Commit(other.to_string())),
            _ => Err(EvidenceError::new(
                "invalid_arguments",
                "base must be 'branch-base', 'head', or a full Git object id",
            )),
        }
    }

    /// A head is what to diff *to*: `worktree` (default, live tree incl. untracked), `index`
    /// (staged), `head`, or an id. `branch-base` is a base concept, not a head.
    fn parse_head(raw: &str) -> Result<Self, EvidenceError> {
        match raw {
            "worktree" => Ok(Self::Worktree),
            "index" => Ok(Self::Index),
            "head" => Ok(Self::Head),
            other if valid_object_id(other) => Ok(Self::Commit(other.to_string())),
            _ => Err(EvidenceError::new(
                "invalid_arguments",
                "head must be 'worktree', 'index', 'head', or a full Git object id",
            )),
        }
    }
}

/// Resolve `branch-base` to the branch's fork point: `merge-base(HEAD, <default branch>)`.
///
/// The base ref is the repository's **default branch** — `origin/HEAD` (a symref to
/// `origin/<default>`), then `origin/main`/`origin/master` as fallbacks — *not* `@{upstream}`. That
/// distinction is f1, and it is a correctness one: `@{upstream}` is the branch's configured upstream,
/// which under the common push-to-same-name workflow is the branch's *own* remote ref, so
/// `merge-base(HEAD, @{upstream})` can equal HEAD and yield a canonical diff of only uncommitted
/// changes while the gate still accepts an approval — the committed branch work silently omitted.
/// The default branch is what a review is actually *against*. All candidates are remote-tracking, so
/// a stale *local* branch can never be the base. When none resolves it fails closed rather than
/// guessing a local base; the caller can pass an explicit base commit id instead. It never fetches —
/// an unfetched remote-tracking ref is the residual the plan leaves to the author.
fn resolve_branch_base(
    root: &Path,
    limits: &Limits,
    cancel: &AtomicBool,
    received_at: Instant,
) -> Result<(String, String), EvidenceError> {
    let head = super::git::resolve_commit(root, "HEAD", limits, cancel, received_at)?
        .ok_or_else(|| EvidenceError::new("branch_base_unresolved", "HEAD does not resolve"))?;
    // Fully-qualified `refs/remotes/...` names, not `origin/main` (f10): git resolves
    // `refs/heads/<name>` before `refs/remotes/<name>`, so a local branch literally named
    // `origin/main` would otherwise shadow the intended remote-tracking ref and could point at HEAD,
    // re-opening the omit-committed-changes hole.
    let mut base_ref = None;
    for candidate in [
        "refs/remotes/origin/HEAD",
        "refs/remotes/origin/main",
        "refs/remotes/origin/master",
    ] {
        if let Some(id) = super::git::resolve_commit(root, candidate, limits, cancel, received_at)?
        {
            base_ref = Some((id, candidate));
            break;
        }
    }
    let (default_branch, source) = base_ref.ok_or_else(|| {
        EvidenceError::new(
            "branch_base_unresolved",
            "the default branch could not be resolved (no refs/remotes/origin/HEAD, .../main, or \
             .../master); pass an explicit base commit id, e.g. base: <full object id>",
        )
    })?;
    let base = super::git::merge_base(root, &head, &default_branch, limits, cancel, received_at)?
        .ok_or_else(|| {
        EvidenceError::new(
            "branch_base_unresolved",
            "HEAD shares no history with the default branch",
        )
    })?;
    Ok((base, format!("merge-base(HEAD, {source})")))
}

/// Read one untracked file's bounded content, symlink-safe. The path came from `git ls-files
/// --others`, but it is still routed through `resolve_existing_bounded` so a symlink pointing out
/// of the root cannot exfiltrate an outside file. Returns the (lossy-UTF-8) content and whether it
/// was truncated at `max_file_bytes`.
fn read_untracked(
    root: &Path,
    rel: &str,
    limits: &Limits,
) -> Result<(String, bool), EvidenceError> {
    let safe = resolve_existing_bounded(root, limits.max_path_bytes as usize, rel, false)
        .map_err(ReadFailure::into_evidence)?;
    let cap = limits.max_file_bytes as usize;
    // Read at most cap+1 bytes rather than slurping the whole file and capping after (f5): a huge
    // untracked file must not be pulled into memory in full. The +1 distinguishes "exactly cap"
    // from "more remained".
    let file = File::open(&safe).map_err(|e| {
        EvidenceError::new(
            "provider_failed",
            format!("cannot read untracked file: {e}"),
        )
    })?;
    // Verify the *opened handle* still points inside the root (f8): resolve_existing_bounded checked
    // the path, but a concurrent path/reparse-point swap between resolution and open could redirect
    // it outside — the same guard read_file_bounded applies.
    verify_open_file(&file, &safe, root).map_err(ReadFailure::into_evidence)?;
    let mut data = Vec::new();
    file.take(cap as u64 + 1)
        .read_to_end(&mut data)
        .map_err(|e| {
            EvidenceError::new(
                "provider_failed",
                format!("cannot read untracked file: {e}"),
            )
        })?;
    let truncated = data.len() > cap;
    data.truncate(cap);
    // Cutting mid-codepoint is fine: from_utf8_lossy substitutes the partial tail.
    let text = String::from_utf8_lossy(&data).into_owned();
    Ok((text, truncated))
}

/// A composed working-tree diff plus the metadata the serve-record needs (mechanism 3). `complete`
/// is the untracked half's wholeness — the tracked half is always whole here, because `git diff`
/// output that hit the byte cap makes `git::diff` return an error rather than a truncated string, so
/// an over-cap canonical diff surfaces as a failed call and fails the gate closed by its absence.
struct ComposedDiff {
    text: String,
    base_token: String,
    base_source: String,
    complete: bool,
    files: usize,
    insertions: usize,
    deletions: usize,
    untracked_files: usize,
}

/// Resolve the endpoints, run the tracked diff, and — for a working-tree head — compose the
/// untracked files `git diff` omits (f2). Runs on the watchdog-bounded worker. The text carries a
/// one-line header naming the resolved base, so the reviewer sees what it is diffing against.
fn compose_diff(
    root: &Path,
    base: &DiffEndpoint,
    head: &DiffEndpoint,
    path: &str,
    limits: &Limits,
    cancel: &AtomicBool,
    received_at: Instant,
) -> Result<ComposedDiff, EvidenceError> {
    let (base_token, base_source) = match base {
        DiffEndpoint::BranchBase => resolve_branch_base(root, limits, cancel, received_at)?,
        DiffEndpoint::Head => ("HEAD".to_string(), "HEAD".to_string()),
        DiffEndpoint::Commit(id) => (id.clone(), id.clone()),
        DiffEndpoint::Worktree | DiffEndpoint::Index => {
            return Err(EvidenceError::new(
                "invalid_arguments",
                "base cannot be the working tree or index",
            ))
        }
    };
    let mut spec: Vec<String> = Vec::new();
    let compose_untracked = matches!(head, DiffEndpoint::Worktree);
    match head {
        DiffEndpoint::Worktree => spec.push(base_token.clone()),
        DiffEndpoint::Index => {
            spec.push("--cached".to_string());
            spec.push(base_token.clone());
        }
        DiffEndpoint::Head => {
            spec.push(base_token.clone());
            spec.push("HEAD".to_string());
        }
        DiffEndpoint::Commit(id) => {
            spec.push(base_token.clone());
            spec.push(id.clone());
        }
        DiffEndpoint::BranchBase => {
            return Err(EvidenceError::new(
                "invalid_arguments",
                "head cannot be 'branch-base'",
            ))
        }
    }
    let spec_refs: Vec<&str> = spec.iter().map(String::as_str).collect();
    let tracked = super::git::diff(root, &spec_refs, path, limits, cancel, received_at)?;
    let (files, insertions, deletions) =
        super::git::numstat(root, &spec_refs, path, limits, cancel, received_at)?;

    let mut complete = true;
    let mut untracked_files = 0usize;
    let mut out = String::new();
    out.push_str(&format!(
        "# repository_diff base {base_source} = {base_token}\n\n"
    ));
    out.push_str(&tracked);
    if compose_untracked {
        let (paths, listing_complete) =
            super::git::untracked_paths(root, limits, cancel, received_at)?;
        // Normalize the request path to git's slash form before comparing (f9): backslashes, a
        // leading `./`, embedded `.`/empty components (`src/./foo`, `src//foo`), and a trailing
        // slash are all valid but would never prefix-match git's normalized untracked paths
        // otherwise. Dropping every `.`/empty component leaves the canonical slash path; empty means
        // the whole tree — no narrowing.
        let norm: String = path
            .replace('\\', "/")
            .split('/')
            .filter(|c| !c.is_empty() && *c != ".")
            .collect::<Vec<_>>()
            .join("/");
        let paths: Vec<String> = if norm.is_empty() {
            paths
        } else {
            // Match at that path or under it, on a path-component boundary so `src/foo` does not also
            // pull in `src/foobar` (f6).
            let prefix = format!("{norm}/");
            paths
                .into_iter()
                .filter(|p| *p == norm || p.starts_with(&prefix))
                .collect()
        };
        complete = listing_complete;
        untracked_files = paths.len();
        if !paths.is_empty() {
            out.push_str("\n\n# untracked (new) files\n");
        }
        for p in &paths {
            match read_untracked(root, p, limits) {
                Ok((content, truncated)) => {
                    out.push_str(&format!("\n=== new file: {p} ===\n"));
                    out.push_str(&content);
                    out.push('\n');
                    if truncated {
                        complete = false;
                        out.push_str("… (content truncated)\n");
                    }
                }
                Err(_) => {
                    complete = false;
                    out.push_str(&format!("\n=== new file: {p} (unreadable, omitted) ===\n"));
                }
            }
        }
        if !complete {
            out.push_str("\n# note: untracked listing or contents were incomplete\n");
        }
    }
    Ok(ComposedDiff {
        text: out,
        base_token,
        base_source,
        complete,
        files,
        insertions,
        deletions,
        untracked_files,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::temp_dir;

    #[test]
    fn diff_endpoints_accept_only_sentinels_and_full_ids() {
        // Base sentinels and a full id resolve; the working tree/index are not bases.
        assert!(matches!(
            DiffEndpoint::parse_base("branch-base"),
            Ok(DiffEndpoint::BranchBase)
        ));
        assert!(matches!(
            DiffEndpoint::parse_base("head"),
            Ok(DiffEndpoint::Head)
        ));
        assert!(matches!(
            DiffEndpoint::parse_base(&"a".repeat(40)),
            Ok(DiffEndpoint::Commit(_))
        ));
        assert!(matches!(
            DiffEndpoint::parse_base(&"b".repeat(64)),
            Ok(DiffEndpoint::Commit(_))
        ));
        // Head sentinels.
        assert!(matches!(
            DiffEndpoint::parse_head("worktree"),
            Ok(DiffEndpoint::Worktree)
        ));
        assert!(matches!(
            DiffEndpoint::parse_head("index"),
            Ok(DiffEndpoint::Index)
        ));

        // Symbolic refs, options, partial ids, and cross-role sentinels are all rejected — nothing
        // symbolic or option-shaped can reach git.
        for bad in [
            "main",
            "HEAD~3",
            "origin/main",
            "@{upstream}",
            "--output=/tmp/x",
            "-x",
            "abc123",
            &"g".repeat(40),
        ] {
            assert!(
                DiffEndpoint::parse_base(bad).is_err(),
                "base accepted {bad:?}"
            );
            assert!(
                DiffEndpoint::parse_head(bad).is_err(),
                "head accepted {bad:?}"
            );
        }
        // Cross-role: the working tree is not a base; branch-base is not a head.
        assert!(DiffEndpoint::parse_base("worktree").is_err());
        assert!(DiffEndpoint::parse_base("index").is_err());
        assert!(DiffEndpoint::parse_head("branch-base").is_err());
    }

    /// A bundle over a plain temp directory. `VcsKind::Perforce` because these fixtures are not
    /// Git repositories: the filesystem walk is the scan they are testing, and a Git bundle here
    /// would (correctly, since issue #86) refuse rather than walk a root Git does not recognise.
    /// `git_bundle` covers the other side.
    fn bundle(root: &Path) -> Bundle {
        bundle_for(root, VcsKind::Perforce)
    }

    fn git_bundle(root: &Path) -> Bundle {
        bundle_for(root, VcsKind::Git)
    }

    fn bundle_for(root: &Path, vcs: VcsKind) -> Bundle {
        let limits = Limits::default();
        Bundle {
            schema_version: super::super::SCHEMA_VERSION,
            nonce: "test-nonce".into(),
            root: fs::canonicalize(root)
                .unwrap()
                .to_string_lossy()
                .to_string(),
            vcs,
            change_label: "working tree".into(),
            status_summary: "clean".into(),
            change: Some("abcdef".into()),
            limits: limits.clone(),
            initial_stamp: initial_stamp(root, &limits, vcs),
            page_bytes_ceiling: None,
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

    /// A walk pool of this test module's own, with headroom. The production pool is capped at two
    /// because the service is serial: one worker to serve the turn and one of headroom. A test
    /// harness is not serial, so tests that only *use* a walk (rather than testing the pool) draw
    /// on this instead, or they refuse each other with `read_unavailable` — the cross-test
    /// interference `a_stalled_walk_cannot_starve_reads` already warns about.
    static ISOLATED_WALK_LIVE: AtomicUsize = AtomicUsize::new(0);
    static ISOLATED_WALK: ReadWatchdog = ReadWatchdog {
        budget: Duration::from_millis(REQUEST_BUDGET_MS),
        attempt_cap: Duration::from_millis(REQUEST_BUDGET_MS),
        backoff: Duration::ZERO,
        max_attempts: 1,
        live: &ISOLATED_WALK_LIVE,
        cap: 64,
    };

    fn junction(link: &Path, target: &Path) -> bool {
        let system = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
        std::process::Command::new(format!(r"{system}\System32\cmd.exe"))
            .args(["/c", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    // Git's answer is untrusted input. `walk_files` never yields a path under an excluded
    // directory name, never follows a reparse point (its own or an ancestor's), and never yields
    // anything but a regular file -- and its results are read without re-resolution. Every one of
    // those checks has to survive the move to an enumeration (issue #86, finding f2).
    #[test]
    fn git_paths_keep_every_check_the_walk_applies() {
        let dir = temp_dir("evidence-accept");
        let root = fs::canonicalize(dir.as_path()).unwrap();
        fs::write(root.join("keep.txt"), "keep").unwrap();
        fs::create_dir(root.join("dist")).unwrap();
        fs::write(root.join("dist").join("excluded.txt"), "no").unwrap();
        fs::create_dir(root.join("sub")).unwrap();
        fs::write(root.join("sub").join("nested.txt"), "yes").unwrap();
        fs::create_dir(root.join("plain-dir")).unwrap();

        let outside = temp_dir("evidence-accept-outside");
        fs::write(outside.as_path().join("secret.txt"), "secret").unwrap();
        let linked = junction(&root.join("link"), outside.as_path());

        let mut listed = vec![
            "keep.txt".to_string(),
            "sub/nested.txt".to_string(),
            // Nothing stops a repository tracking files under an excluded directory name.
            "dist/excluded.txt".to_string(),
            // A gitlink for an uninitialised submodule, and a plain directory: neither is a file.
            "plain-dir".to_string(),
            // A corrupt index entry.
            "../escape.txt".to_string(),
            // Enumerated, then deleted before the stat.
            "vanished.txt".to_string(),
        ];
        if linked {
            listed.push("link/secret.txt".to_string());
        }
        listed.sort();

        let cancel = AtomicBool::new(false);
        let (accepted, dropped) = accept_git_paths(&root, &listed, 4096, &cancel).unwrap();
        let kept: Vec<&str> = accepted.iter().map(|a| a.relative.as_str()).collect();
        assert_eq!(kept, ["keep.txt", "sub/nested.txt", "vanished.txt"]);
        assert!(dropped, "dropping paths must clear the completeness flag");
        assert!(
            accepted
                .iter()
                .any(|a| a.relative == "vanished.txt" && a.meta.is_none()),
            "a path that disappeared before the stat is recorded, not fatal"
        );
        if !linked {
            eprintln!("note: junction could not be created; the ancestor check was not exercised");
        }
    }

    #[test]
    fn a_stamp_over_an_enumeration_tracks_only_what_it_enumerated() {
        let dir = temp_dir("evidence-git-stamp");
        let root = fs::canonicalize(dir.as_path()).unwrap();
        fs::write(root.join("tracked.txt"), "one").unwrap();
        fs::write(root.join("ignored.txt"), "one").unwrap();
        let listed = ["tracked.txt".to_string()];
        let cancel = AtomicBool::new(false);
        let stamp = |root: &Path| {
            let (accepted, _) = accept_git_paths(root, &listed, 4096, &cancel).unwrap();
            let rows: Vec<String> = accepted
                .iter()
                .map(|a| stamp_row(&a.relative, a.meta.as_ref()))
                .collect();
            digest_rows(&rows).unwrap()
        };
        let before = stamp(&root);

        fs::write(root.join("ignored.txt"), "a much longer body").unwrap();
        assert_eq!(
            stamp(&root),
            before,
            "a file outside the enumeration is not drift"
        );

        fs::write(root.join("tracked.txt"), "a much longer body").unwrap();
        assert_ne!(stamp(&root), before, "an enumerated file changing is drift");
    }

    // The rule that decides whether a scan may fall back to walking the whole tree. Only a missing
    // binary may: every other failure means Git is here and did not answer, and the walk is the
    // 400,000-file scan issue #86 exists to avoid -- for a timeout, a corrupt index and a
    // permissions failure alike.
    #[test]
    fn only_a_missing_git_binary_sends_a_scan_to_the_filesystem() {
        assert!(matches!(
            scan_source(Err(super::super::git::GitFailure::NoGit)),
            ScanSource::Filesystem
        ));
        for failure in [
            super::super::git::GitFailure::Failed(EvidenceError::new(
                "provider_failed",
                "exit 128",
            )),
            super::super::git::GitFailure::OutOfTime(EvidenceError::new(
                "deadline_exceeded",
                "slow",
            )),
        ] {
            assert!(matches!(scan_source(Err(failure)), ScanSource::Refused(_)));
        }
        assert!(matches!(
            scan_source(Ok(super::super::git::Enumeration {
                paths: vec!["a".into()],
                complete: false,
            })),
            ScanSource::Git(_, false)
        ));
    }

    // A tree bigger than the budget used to refuse the whole review here.
    #[test]
    fn a_tree_over_the_file_budget_yields_unknown_drift_rather_than_an_error() {
        let dir = temp_dir("evidence-stamp-budget");
        fs::write(dir.as_path().join("a.txt"), "a").unwrap();
        fs::write(dir.as_path().join("b.txt"), "b").unwrap();
        let limits = Limits {
            max_files: 1,
            ..Default::default()
        };
        let drift = initial_stamp(dir.as_path(), &limits, VcsKind::Perforce);
        assert!(matches!(drift, Drift::Unavailable { .. }), "{drift:?}");
    }

    // Unknown drift must reach the reviewer as unknown, with a reason, on both tools that carry
    // it -- `repository_scope` is recommended to the reviewer, not required, so a read that says
    // only `null` explains nothing. The Git bundle over a non-repository is the realistic way to
    // reach an unavailable observation without a git repository in the test.
    #[test]
    fn an_unobservable_stamp_reads_as_unknown_and_is_observed_once() {
        let dir = temp_dir("evidence-unknown-drift");
        fs::write(dir.as_path().join("a.txt"), "hello\n").unwrap();
        let mut core = Core::new(git_bundle(dir.as_path())).unwrap();
        core.walk_watchdog = &ISOLATED_WALK;

        let read = core
            .call("repository_read", &json!({"path":"a.txt"}))
            .unwrap();
        assert_eq!(read["drifted"], Value::Null);
        assert!(read["drift_unavailable"]
            .as_str()
            .is_some_and(|r| !r.is_empty()));
        assert!(
            core.observed_stamp.is_some(),
            "an unavailable observation is cached like any other, not re-run on every read"
        );

        let scope = core.call("repository_scope", &json!({})).unwrap();
        assert_eq!(scope["drifted"], Value::Null);
        assert_eq!(scope["initial_stamp"], Value::Null);
        assert_eq!(scope["current_stamp"], Value::Null);
        assert!(scope["drift_unavailable"].as_str().is_some());
        assert!(scope["scan_scope"]
            .as_str()
            .is_some_and(|s| s.contains("Git")));
    }

    // Issue #114 (f1): the served-page ceiling bounds the JSON-ENCODED bytes of every page below
    // what the reviewer requested, without rejecting the request, so a reviewer whose MCP client
    // caps the *serialized* tool result is instead paged in slices it can consume. Exercised
    // through `repository_change` (no git needed); diff/revision share the one `page_end` seam.
    #[test]
    fn a_page_ceiling_bounds_the_encoded_size_of_every_served_page() {
        let ceiling: usize = 24 * 1024;

        // Page the whole `change` at once (limit_bytes at the request maximum) and collect every
        // served content page. The request is honoured, just paged.
        fn page_all(change: String, ceiling: usize) -> Vec<String> {
            let dir = temp_dir("evidence-page-ceiling");
            let bundle = Bundle::create(
                dir.as_path(),
                crate::config::Vcs::Perforce,
                "nonce-ceil",
                "working tree".into(),
                "captured".into(),
                Some(change),
                Some(ceiling as u32),
            )
            .unwrap();
            let mut core = Core::new(bundle).unwrap();
            let mut resp = core
                .call(
                    "repository_change",
                    &json!({"limit_bytes": super::Limits::default().max_change_bytes}),
                )
                .unwrap();
            let mut pages = vec![resp["content"].as_str().unwrap().to_string()];
            while resp["complete"] == json!(false) {
                let cursor = resp["cursor"].as_str().unwrap().to_string();
                resp = core
                    .call("repository_change", &json!({ "cursor": cursor }))
                    .unwrap();
                pages.push(resp["content"].as_str().unwrap().to_string());
            }
            pages
        }

        // Plain text (encoded == raw): pages span the change and reassemble it whole.
        let plain = "x".repeat(100 * 1024);
        let pages = page_all(plain.clone(), ceiling);
        assert!(
            pages.len() > 1,
            "a change several ceilings wide must span pages"
        );
        for p in &pages {
            assert!(
                super::json_escaped_len(p) <= ceiling,
                "encoded page {} exceeds ceiling",
                super::json_escaped_len(p)
            );
        }
        assert_eq!(pages.concat(), plain, "the whole change is reassembled");

        // Escape-heavy text: every byte is a quote, which serialises to two bytes. A raw-byte clamp
        // would serve ~24 KiB raw that becomes ~48 KiB encoded and gets diverted; the encoded bound
        // instead serves ~12 KiB raw so the ENCODED page still fits, and it still reassembles whole.
        let quotes = "\"".repeat(100 * 1024);
        let qpages = page_all(quotes.clone(), ceiling);
        for p in &qpages {
            assert!(
                super::json_escaped_len(p) <= ceiling,
                "encoded quote page {} exceeds ceiling",
                super::json_escaped_len(p)
            );
            assert!(
                p.len() <= ceiling / 2,
                "raw quote page {} should be about half the ceiling (each char encodes to two bytes)",
                p.len()
            );
        }
        assert_eq!(
            qpages.concat(),
            quotes,
            "the whole escape-heavy change is reassembled"
        );
    }

    // Issue #114 (f2): repository_read's window is bounded by the same encoded ceiling, so a large
    // file read is not diverted by a capped MCP client. It has no cursor, so a short window is
    // signalled through `truncated`/`complete`, which the reviewer continues from with `start_line`.
    #[test]
    fn a_page_ceiling_bounds_the_repository_read_window() {
        let dir = temp_dir("evidence-read-ceiling");
        let ceiling: usize = 24 * 1024;
        // 5,000 lines of 100 chars ~= 500 KiB, far past the ceiling.
        let body = (0..5000)
            .map(|i| format!("line {i:04} {}", "abcdefghij".repeat(9)))
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(dir.as_path().join("big.txt"), &body).unwrap();
        let bundle = Bundle::create(
            dir.as_path(),
            crate::config::Vcs::Perforce,
            "nonce-read-ceil",
            "working tree".into(),
            "captured".into(),
            None,
            Some(ceiling as u32),
        )
        .unwrap();
        let mut core = Core::new(bundle).unwrap();
        // Ask for the whole file (line_count at the max). The read is honoured but the window is
        // capped, so it comes back short and marked truncated rather than diverted.
        let resp = core
            .call(
                "repository_read",
                &json!({"path":"big.txt","line_count": super::Limits::default().max_lines}),
            )
            .unwrap();
        assert_eq!(resp["truncated"], json!(true));
        assert_eq!(resp["complete"], json!(false));
        let lines = resp["lines"].as_array().unwrap();
        assert!(!lines.is_empty(), "at least one line is always returned");
        assert!(
            lines.len() < 5000,
            "the window must be cut short of the whole file"
        );
        // The encoded size of the returned lines fits the ceiling.
        let encoded: usize = lines
            .iter()
            .map(|l| super::json_escaped_len(l["text"].as_str().unwrap()) + 32)
            .sum();
        assert!(
            encoded <= ceiling,
            "encoded read window {encoded} exceeds ceiling"
        );
    }

    // Issue #114 (f2 turn 2): a lone line under the raw `max_line_bytes` cap but over the encoded
    // ceiling (here 15,000 quotes: 15 KB raw, ~30 KB escaped) must not be served whole and diverted.
    // The one forced line is truncated to fit and the read is marked not-complete, rather than lost.
    #[test]
    fn a_single_over_ceiling_line_is_truncated_not_served_oversized() {
        let dir = temp_dir("evidence-read-line-ceiling");
        let ceiling: usize = 24 * 1024;
        fs::write(dir.as_path().join("one.txt"), "\"".repeat(15_000)).unwrap();
        let bundle = Bundle::create(
            dir.as_path(),
            crate::config::Vcs::Perforce,
            "nonce-line-ceil",
            "working tree".into(),
            "captured".into(),
            None,
            Some(ceiling as u32),
        )
        .unwrap();
        let mut core = Core::new(bundle).unwrap();
        let resp = core
            .call("repository_read", &json!({"path":"one.txt"}))
            .unwrap();
        // The whole file is one line, yet the read is not complete: its single line was cut to fit.
        assert_eq!(resp["complete"], json!(false));
        assert_eq!(resp["truncated"], json!(true));
        let lines = resp["lines"].as_array().unwrap();
        assert_eq!(lines.len(), 1);
        let text = lines[0]["text"].as_str().unwrap();
        assert!(
            text.len() < 15_000,
            "the oversized line must be truncated, got {} chars",
            text.len()
        );
        assert!(
            super::json_escaped_len(text) <= ceiling,
            "the truncated line's encoded size {} must fit the ceiling",
            super::json_escaped_len(text)
        );
    }

    // Git present, root not a work tree: the scan refuses rather than quietly walking a tree the
    // caller declared to be Git. Surfacing the misconfiguration beats scanning the wrong thing.
    #[test]
    fn a_git_root_git_rejects_refuses_the_search_rather_than_walking_it() {
        if crate::reviewer::on_path("git").is_none() {
            eprintln!("skipping: git is not on PATH");
            return;
        }
        let dir = temp_dir("evidence-not-a-worktree");
        fs::write(dir.as_path().join("a.txt"), "hello\n").unwrap();
        let mut core = Core::new(git_bundle(dir.as_path())).unwrap();
        core.walk_watchdog = &ISOLATED_WALK;
        // Any refusal is the point. The failure this guards against is the opposite: a *successful*
        // search, which here could only come from having walked the filesystem after Git declined.
        let result = core.call("repository_search", &json!({"query":"hello"}));
        assert!(
            result.is_err(),
            "a Git root Git rejects must not silently fall back to the walk: {result:?}"
        );
    }

    // A path that is *gone* is an observation; a path that could not be looked at is a hole. The
    // first belongs in the stamp, the second must clear completeness -- and a stamp taken over a
    // set that lost paths is not comparable with one that did not, so it is refused outright.
    #[test]
    fn an_unreadable_path_is_a_hole_and_a_missing_one_is_an_observation() {
        let dir = temp_dir("evidence-dropped");
        let root = fs::canonicalize(dir.as_path()).unwrap();
        fs::write(root.join("here.txt"), "here").unwrap();
        let cancel = AtomicBool::new(false);

        let listed = ["here.txt".to_string(), "gone.txt".to_string()];
        let (accepted, dropped) = accept_git_paths(&root, &listed, 4096, &cancel).unwrap();
        assert_eq!(accepted.len(), 2);
        assert!(
            !dropped,
            "a file the enumeration named and that is absent is an observation, not a lost path"
        );

        // A directory in the file's place: `symlink_metadata` succeeds but it is not a regular
        // file, which is the same class of hole as an unreadable one.
        fs::create_dir(root.join("wrong-kind")).unwrap();
        let listed = ["here.txt".to_string(), "wrong-kind".to_string()];
        let (accepted, dropped) = accept_git_paths(&root, &listed, 4096, &cancel).unwrap();
        assert_eq!(accepted.len(), 1);
        assert!(dropped);

        // And a stamp over a set with a hole in it is refused rather than hashed.
        let stamped = run_bounded_stamp(
            &root,
            &Limits::default(),
            Some(listed.to_vec()),
            &Arc::new(AtomicBool::new(false)),
            Instant::now(),
        );
        assert_eq!(stamped.unwrap_err().code, "read_failed");
    }

    // A cancelled request must not leave an observation behind -- not a stamp, and not an
    // `Unavailable` the next request would trust as "we looked and could not tell".
    #[test]
    fn a_cancelled_scan_is_an_error_rather_than_an_observation() {
        let dir = temp_dir("evidence-cancelled-scan");
        fs::write(dir.as_path().join("a.txt"), "a").unwrap();
        let root = fs::canonicalize(dir.as_path()).unwrap();
        let cancel = Arc::new(AtomicBool::new(true));

        // The filesystem walk had no cancellation check at all before issue #86.
        assert_eq!(
            tree_stamp(&root, &Limits::default(), &cancel)
                .unwrap_err()
                .code,
            "cancelled"
        );
        assert_eq!(
            accept_git_paths(&root, &["a.txt".to_string()], 4096, &cancel)
                .unwrap_err()
                .code,
            "cancelled"
        );
        assert_eq!(
            observe_drift(
                &root,
                &Limits::default(),
                VcsKind::Perforce,
                &cancel,
                Instant::now()
            )
            .unwrap_err()
            .code,
            "cancelled"
        );
    }

    // `Path::starts_with` is component-wise but case-sensitive, and these two paths arrive by
    // different routes: Git's spelling joined onto the root, against a canonicalised base.
    #[test]
    fn base_containment_is_case_insensitive_like_the_filesystem() {
        let dir = temp_dir("evidence-case");
        let root = fs::canonicalize(dir.as_path()).unwrap();
        let base = root.join("Src");
        assert!(within(&root.join("src").join("a.txt"), &base));
        assert!(!within(&root.join("srcx").join("a.txt"), &base));
        assert!(
            !Path::new("src/a.txt").starts_with("Src"),
            "the bug this guards"
        );
    }

    // A deleted *directory* is the same event as a deleted file, one level up. Collapsing it into a
    // hole would turn ordinary drift -- someone removed a directory mid-review -- into unavailable
    // drift, which is the opposite of what the reviewer needs to hear.
    #[test]
    fn a_missing_ancestor_is_recorded_as_missing_not_as_a_hole() {
        let dir = temp_dir("evidence-missing-ancestor");
        let root = fs::canonicalize(dir.as_path()).unwrap();
        fs::write(root.join("kept.txt"), "kept").unwrap();
        let cancel = AtomicBool::new(false);
        let listed = ["gone-dir/a.txt".to_string(), "kept.txt".to_string()];

        let (accepted, dropped) = accept_git_paths(&root, &listed, 4096, &cancel).unwrap();
        assert!(
            !dropped,
            "a removed directory is an observation, not a hole"
        );
        let missing = accepted
            .iter()
            .find(|a| a.relative == "gone-dir/a.txt")
            .expect("the vanished path is still recorded");
        assert!(missing.meta.is_none());
        assert_eq!(
            missing.path,
            root.join("gone-dir").join("a.txt"),
            "the recorded path must be the whole path, not the prefix the ancestor walk stopped at"
        );

        // And it is drift: the stamp moves when the directory goes away.
        fs::create_dir(root.join("gone-dir")).unwrap();
        fs::write(root.join("gone-dir").join("a.txt"), "here").unwrap();
        let stamp = |root: &Path| {
            let (accepted, _) = accept_git_paths(root, &listed, 4096, &cancel).unwrap();
            digest_rows(
                &accepted
                    .iter()
                    .map(|a| stamp_row(&a.relative, a.meta.as_ref()))
                    .collect::<Vec<_>>(),
            )
            .unwrap()
        };
        let present = stamp(&root);
        fs::remove_dir_all(root.join("gone-dir")).unwrap();
        assert_ne!(stamp(&root), present);
    }

    // Directories are the tree's shape, not its content: an empty directory appearing is not drift
    // worth reporting, and charging directories to `max_files` spends a budget meant for reviewable
    // content -- which is the budget this whole change is about.
    #[test]
    fn the_filesystem_stamp_hashes_files_not_directories() {
        let dir = temp_dir("evidence-stamp-dirs");
        let root = fs::canonicalize(dir.as_path()).unwrap();
        fs::write(root.join("a.txt"), "a").unwrap();
        let cancel = AtomicBool::new(false);
        let before = tree_stamp(&root, &Limits::default(), &cancel).unwrap();
        fs::create_dir(root.join("empty")).unwrap();
        assert_eq!(
            tree_stamp(&root, &Limits::default(), &cancel).unwrap(),
            before
        );

        // Three directories and one file fit a one-file budget, because only the file is counted.
        fs::create_dir(root.join("empty").join("deeper")).unwrap();
        fs::create_dir(root.join("another")).unwrap();
        let tight = Limits {
            max_files: 1,
            ..Default::default()
        };
        assert!(tree_stamp(&root, &tight, &cancel).is_ok());
    }

    // "Naming a file searches it, ignored or not" cannot be at the mercy of the enumeration, so the
    // base is resolved before Git is asked. This fixture is a Git bundle over a directory Git will
    // refuse, which is exactly the case that used to take the explicit file down with it.
    #[test]
    fn naming_a_file_searches_it_even_when_the_enumeration_fails() {
        let dir = temp_dir("evidence-explicit-file");
        fs::write(dir.as_path().join("a.txt"), "needle\n").unwrap();
        let mut core = Core::new(git_bundle(dir.as_path())).unwrap();
        core.walk_watchdog = &ISOLATED_WALK;
        let searched = core
            .call(
                "repository_search",
                &json!({"query":"needle","path":"a.txt"}),
            )
            .unwrap();
        assert_eq!(searched["matches"].as_array().unwrap().len(), 1);
        assert_eq!(searched["complete"], true);
        // ... while the directory search in the same repository still refuses.
        assert!(core
            .call("repository_search", &json!({"query":"needle"}))
            .is_err());
    }

    // `scan_scope` must describe the scan that ran. A Git bundle whose scan fell back to the walk
    // would otherwise tell the reviewer its searches were Git-scoped when they were not.
    #[test]
    fn scan_scope_reports_the_scan_that_ran_not_the_vcs() {
        let dir = temp_dir("evidence-scan-scope");
        let mut core = Core::new(git_bundle(dir.as_path())).unwrap();
        core.observed_stamp = Some(Drift::unavailable("stubbed"));
        core.observed_method = Some(StampMethod::Filesystem);
        let scope = core.call("repository_scope", &json!({})).unwrap();
        assert_eq!(scope["scan_scope"], StampMethod::Filesystem.scan_scope());
        assert_ne!(scope["scan_scope"], StampMethod::Git.scan_scope());
    }

    // Both ceilings truncate with the completeness flag cleared instead of refusing the call.
    #[test]
    fn the_file_budget_truncates_a_listing_and_a_walk() {
        let dir = temp_dir("evidence-budget-truncates");
        for name in ["a.txt", "b.txt", "c.txt"] {
            fs::write(dir.as_path().join(name), "hello\n").unwrap();
        }
        let mut small = bundle(dir.as_path());
        small.limits.max_files = 2;
        let mut core = Core::new(small).unwrap();
        core.walk_watchdog = &ISOLATED_WALK;

        let listed = core.call("repository_list", &json!({})).unwrap();
        assert_eq!(listed["entries"].as_array().unwrap().len(), 2);
        assert_eq!(listed["complete"], false);

        let searched = core
            .call("repository_search", &json!({"query":"hello"}))
            .unwrap();
        assert_eq!(searched["complete"], false);
        assert!(!searched["matches"].as_array().unwrap().is_empty());
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
