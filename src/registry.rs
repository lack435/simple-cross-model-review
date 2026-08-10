//! In-flight review tracking.
//!
//! `cross_model_review` returns immediately and a worker thread runs the reviewer CLI.
//! `cross_model_review_result` long-polls here, so the calling agent waits on a
//! condition variable instead of burning turns on a retry loop.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::errors::Failure;
use crate::metrics::Usage;

/// Finished reviews kept per session. A review holds its full text for the life of the
/// process, so a long agent session doing many reviews accumulated all of them.
///
/// Per session rather than by age, because age can discard a review the caller was still
/// entitled to collect. Keeping the newest few means the review a caller is most likely
/// to ask for -- the one it just started -- is never the one thrown away.
pub const MAX_TERMINAL_PER_SESSION: usize = 3;

/// Finished reviews kept across all sessions, since session names are chosen by the
/// caller and a caller that invents a new one each time would otherwise accumulate one
/// review per name for ever.
pub const MAX_TERMINAL_TOTAL: usize = 50;

/// Why a review could not be registered.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum StartRefused {
    /// The session already has a review in flight. Carries that review's id, which the
    /// caller is told to collect or cancel.
    Busy(String),
    /// The per-process cap on concurrently-running reviews is already reached. Carries the
    /// configured limit. The caller is told to collect or cancel an outstanding review.
    TooManyRunning { limit: u32 },
    /// Stdin has closed and the process is on its way out.
    ShuttingDown,
}

/// What the registry knows about an id.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IdState {
    /// Present, running or finished.
    Known,
    /// Finished, then dropped to bound memory.
    Evicted,
    /// Never issued by this process.
    Unknown,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    Running,
    Completed,
    Failed,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }
}

/// The observable part of the worker pipeline while a review is running.
///
/// These are deliberately phases rather than percentages. Reviewer turns vary too much
/// in size to estimate completion honestly, but naming the work currently under way lets
/// a caller distinguish a live, long-running review from a request that disappeared.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    Preparing,
    Capturing,
    Launching,
    Reviewing,
    Finalizing,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Preparing => "preparing the review",
            Self::Capturing => "capturing the change",
            Self::Launching => "launching the reviewer",
            Self::Reviewing => "reviewer process running",
            Self::Finalizing => "processing the reviewer response",
        }
    }
}

pub struct Review {
    pub id: String,
    pub session: String,
    pub turn: u32,
    pub resumed: bool,
    pub status: Status,
    pub review: Option<String>,
    pub failure: Option<Failure>,
    /// Read-only commands the reviewer attempted but was not permitted to run. Surfaced
    /// so the caller can tell a thin review from a blocked one.
    pub denials: Vec<String>,
    /// Total number of denied commands. `denials` is only a bounded set of examples.
    pub denial_count: usize,
    /// Whether `denial_count` is a lower bound (the source output was capped). Reported as
    /// "at least N" so a count thinned by truncation is not presented as the exact total.
    pub denial_count_is_floor: bool,
    /// Problems that did not invalidate the review but that the caller must know about.
    pub warnings: Vec<String>,
    /// The resume disposition of this turn: whether the reviewer was sent only the delta, a
    /// full change by design, or a full change because an intended delta fell back -- and why.
    /// `None` on a fresh turn or a turn that sent no change, which render no disposition line.
    pub disposition: Option<crate::vcs::Disposition>,
    /// What the server captured and sent this turn, for the `captured:` response line. `None`
    /// when no change was sent (`--diff none`, a shell-equipped reviewer, or a failed capture).
    pub capture_summary: Option<crate::vcs::CaptureSummary>,
    /// Whether a follow-up call on this session name will actually reach the same
    /// reviewer conversation. Tracked rather than assumed, so the response never invites
    /// a resume that would silently start over.
    pub resumable: bool,
    pub started: Instant,
    pub finished: Option<Instant>,
    pub phase: Phase,
    pub phase_started: Instant,
    /// Most recent point at which the worker changed phase or confirmed the reviewer
    /// process was still alive. Liveness is not claimed as forward progress: streamed
    /// output is tracked separately below.
    pub last_activity: Instant,
    /// Bytes observed on the reviewer's stdout and stderr pipes so far. This is evidence
    /// of activity, not a completion estimate: Claude commonly emits nothing until the
    /// final response, while Codex emits a JSONL event stream as it works.
    pub output_bytes: usize,
    /// What the turn cost, once it has finished. Zero while it is running: the CLIs
    /// report usage on completion, so there is nothing honest to show before then.
    pub usage: Usage,
    /// The reviewer entry currently (or last) running this review, as a describe string. Set by
    /// `set_active` before capture and before each fall-through attempt, so a running poll and the
    /// terminal result name the entry that actually ran rather than the whole chain. `None` until
    /// the first `set_active`. See `docs/reviewer-fallback-chain.md`.
    pub active: Option<String>,
    /// The structured findings envelope this turn produced, once it has finished. `None` while
    /// running or on a failed turn; the completed-result renderer emits both channels from it.
    pub envelope: Option<crate::findings::Envelope>,
    pub cancel: Arc<AtomicBool>,
    /// Order in which this process finished the review, or 0 while it is still running.
    /// Assigned under the registry lock; see `State::finishes`.
    finish_seq: u64,
}

impl Review {
    pub fn elapsed(&self) -> Duration {
        self.finished
            .unwrap_or_else(Instant::now)
            .duration_since(self.started)
    }
}

pub struct Outcome {
    pub review: Option<String>,
    pub failure: Option<Failure>,
    pub denials: Vec<String>,
    /// Total number of denied commands. `denials` is only a bounded set of examples.
    pub denial_count: usize,
    /// Whether `denial_count` is a lower bound (the source output was capped).
    pub denial_count_is_floor: bool,
    pub warnings: Vec<String>,
    /// The resume disposition of this turn; see [`Review::disposition`].
    pub disposition: Option<crate::vcs::Disposition>,
    /// What the server captured and sent this turn; see [`Review::capture_summary`].
    pub capture_summary: Option<crate::vcs::CaptureSummary>,
    pub resumable: bool,
    /// What this turn cost, as the reviewer CLI reported it.
    pub usage: Usage,
    /// The reviewer entry that actually produced this outcome, as a describe string, so the
    /// terminal result names the entry that ran (a fallback, when the walk fell through) rather
    /// than the whole chain. `None` leaves the response to fall back to the chain description.
    pub active: Option<String>,
    /// The structured findings envelope this turn produced. `None` on a failed turn.
    pub envelope: Option<crate::findings::Envelope>,
}

impl Outcome {
    pub fn failed(failure: Failure) -> Self {
        Self {
            review: None,
            failure: Some(failure),
            denials: Vec::new(),
            denial_count: 0,
            denial_count_is_floor: false,
            warnings: Vec::new(),
            disposition: None,
            capture_summary: None,
            resumable: false,
            usage: Usage::default(),
            active: None,
            envelope: None,
        }
    }

    #[cfg(test)]
    fn completed(review: &str) -> Self {
        Self {
            review: Some(review.to_string()),
            failure: None,
            denials: Vec::new(),
            denial_count: 0,
            denial_count_is_floor: false,
            warnings: Vec::new(),
            disposition: None,
            capture_summary: None,
            resumable: true,
            usage: Usage::default(),
            active: None,
            envelope: None,
        }
    }
}

/// Everything the registry mutates, under one lock.
#[derive(Default)]
struct State {
    reviews: HashMap<String, Review>,
    /// Completion order, assigned under the lock as each review finishes.
    ///
    /// Eviction ranks on this rather than on the finish `Instant`. Two reviews can land in
    /// the same clock tick, and ties would then fall back to `HashMap::values()` order,
    /// which is randomly seeded per process; a counter is total and monotonic by
    /// construction, so there are no ties to break and no dependence on clock resolution.
    ///
    /// It is *completion* order, not issue order. Issue order is not the same thing: a
    /// review that started early and ran long holds the lowest issue counter, so ranking
    /// on that would place it oldest at the very moment it finished, and the sweep
    /// triggered by its own `finish` could evict it before its caller could collect it.
    finishes: u64,
    /// Set once stdin has closed and the process is on its way out. Waiters observe it
    /// alongside their own deadline, so shutdown does not have to outlast a long poll.
    ///
    /// It lives in here rather than beside the condvar as an `AtomicBool` so that it
    /// cannot be touched without the lock — which is what makes it race-free, not its
    /// atomicity. An `Atomic` would read as "safe from anywhere" and is exactly the trap:
    /// publishing it outside the lock, or hoisting the read in `wait` out of the loop,
    /// would compile and would reinstate a lost wakeup too narrow for a test to catch.
    /// See `begin_shutdown`.
    shutdown: bool,
}

impl State {
    /// Drop finished reviews beyond the retention caps, newest kept.
    ///
    /// Running reviews are never touched: one is by definition still owed to a caller,
    /// and removing it would strand a poll on an id that has no terminal state to reach.
    fn evict(&mut self) {
        let mut terminal: Vec<(String, String, u64)> = self
            .reviews
            .values()
            .filter(|r| r.status != Status::Running)
            .map(|r| (r.id.clone(), r.session.clone(), r.finish_seq))
            .collect();
        // Most recently finished first, so "keep the newest N" is a prefix. See
        // `State::finishes` for why this key and not the finish time.
        terminal.sort_by_key(|(_, _, finish_seq)| std::cmp::Reverse(*finish_seq));

        let mut per_session: HashMap<String, usize> = HashMap::new();
        let mut kept = 0usize;
        let mut doomed = Vec::new();
        for (id, session, _) in terminal {
            let seen = per_session.entry(session.clone()).or_insert(0);
            *seen += 1;
            if *seen > MAX_TERMINAL_PER_SESSION || kept >= MAX_TERMINAL_TOTAL {
                doomed.push((id, session));
            } else {
                kept += 1;
            }
        }

        for (id, _session) in doomed {
            self.reviews.remove(&id);
        }
    }
}

#[derive(Default)]
pub struct Registry {
    state: Mutex<State>,
    changed: Condvar,
    counter: AtomicU64,
    /// Per-process cap on concurrently-running reviews, enforced in `try_start`. `0` disables it.
    /// Held here rather than passed per call so the many `Registry::new()` test call sites keep the
    /// cap off by default and only the cap's own test opts in.
    max_concurrent: u32,
    /// Bumped immediately before each `wait_timeout`, while the state lock is still held.
    ///
    /// That timing is the whole point, and it is what lets a test prove a waiter has parked
    /// rather than assume it: a test that has seen this counter move knows the waiter holds
    /// the lock and is about to park, so anything it does next that needs the lock cannot
    /// proceed until `wait_timeout` has atomically released it and joined the wait set.
    #[cfg(test)]
    parks: AtomicU64,
}

impl Registry {
    /// The tests want a registry with the concurrency cap off; production always goes through
    /// `with_max_concurrent` with the configured limit.
    #[cfg(test)]
    pub fn new() -> Self {
        Self::default()
    }

    /// A registry that refuses to start more than `max_concurrent` reviews at once (`0` disables).
    pub fn with_max_concurrent(max_concurrent: u32) -> Self {
        Self {
            max_concurrent,
            ..Self::default()
        }
    }

    /// Claim a session and register a new review for it, or fail with the id of the
    /// review already running there.
    ///
    /// The check and the insert happen under a single lock acquisition. Doing them
    /// separately would let two concurrent `tools/call` handlers both see an idle
    /// session and both start a review against the same reviewer conversation.
    ///
    /// Refuses outright once shutdown has begun. A handler can reach here after stdin
    /// closed — it may have spent the interval in the auth preflight or waiting for the
    /// session lease — and a review registered at that point can never be collected,
    /// because the process exits as soon as the handler returns. Starting one would spend
    /// a reviewer turn on a result with nowhere to go, and answer "review started" to a
    /// caller that will never see the review. Tested here, under the lock that publishes
    /// the flag, rather than by the caller beforehand: a pre-check outside it would leave
    /// exactly the window this closes.
    pub fn try_start(
        &self,
        session: &str,
        turn: u32,
        resumed: bool,
    ) -> Result<(String, Arc<AtomicBool>), StartRefused> {
        let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());

        if guard.shutdown {
            return Err(StartRefused::ShuttingDown);
        }

        if let Some(running) = guard
            .reviews
            .values()
            .find(|r| r.session == session && r.status == Status::Running)
        {
            return Err(StartRefused::Busy(running.id.clone()));
        }

        // The concurrency backstop is checked after the same-session busy check, so a re-entry on
        // a session that is already running gets the specific, actionable `Busy` error rather than
        // the coarser cap. Counted under the same lock that inserts below, so two concurrent starts
        // cannot both pass a cap of `n` into `n + 1`. `0` disables the cap entirely.
        if self.max_concurrent > 0 {
            let running = guard
                .reviews
                .values()
                .filter(|r| r.status == Status::Running)
                .count();
            if running >= self.max_concurrent as usize {
                return Err(StartRefused::TooManyRunning {
                    limit: self.max_concurrent,
                });
            }
        }

        // Ids are unique per process and readable in logs. No RNG dependency: the
        // process id plus a monotonic counter is already collision-free here.
        let seq = self.counter.fetch_add(1, Ordering::Relaxed) + 1;
        let id = format!("rv-{}-{}", std::process::id(), seq);
        let cancel = Arc::new(AtomicBool::new(false));
        let now = Instant::now();
        guard.reviews.insert(
            id.clone(),
            Review {
                id: id.clone(),
                session: session.to_string(),
                turn,
                resumed,
                status: Status::Running,
                review: None,
                failure: None,
                denials: Vec::new(),
                denial_count: 0,
                denial_count_is_floor: false,
                warnings: Vec::new(),
                disposition: None,
                capture_summary: None,
                resumable: false,
                started: now,
                finished: None,
                phase: Phase::Preparing,
                phase_started: now,
                last_activity: now,
                output_bytes: 0,
                usage: Usage::default(),
                active: None,
                envelope: None,
                cancel: Arc::clone(&cancel),
                finish_seq: 0,
            },
        );
        drop(guard);
        self.changed.notify_all();
        Ok((id, cancel))
    }

    /// Record which reviewer entry a running review is now on, so a running poll and the terminal
    /// result attribute to the entry that actually ran. Uses the same `State` lock as `finish`
    /// and the snapshot path: an owned clone stored under the lock, updated only while the review
    /// is `Running`, and copied out by snapshots under the same lock. See
    /// `docs/reviewer-fallback-chain.md`.
    pub fn set_active(&self, id: &str, active: String) {
        let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(review) = guard.reviews.get_mut(id) {
            if review.status == Status::Running {
                review.active = Some(active);
            }
        }
    }

    /// Move a running review into another observable phase.
    pub fn set_phase(&self, id: &str, phase: Phase) {
        let changed = {
            let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
            match guard.reviews.get_mut(id) {
                Some(review) if review.status == Status::Running => {
                    let now = Instant::now();
                    review.phase = phase;
                    review.phase_started = now;
                    review.last_activity = now;
                    true
                }
                _ => false,
            }
        };
        if changed {
            self.changed.notify_all();
        }
    }

    /// Record a liveness check and the amount of reviewer output observed so far.
    pub fn report_activity(&self, id: &str, output_bytes: usize) {
        let changed = {
            let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
            match guard.reviews.get_mut(id) {
                Some(review) if review.status == Status::Running => {
                    let phase_changed = review.phase != Phase::Reviewing;
                    let output_changed = review.output_bytes != output_bytes;
                    let now = Instant::now();
                    review.phase = Phase::Reviewing;
                    if phase_changed {
                        review.phase_started = now;
                    }
                    review.last_activity = now;
                    review.output_bytes = output_bytes;
                    phase_changed || output_changed
                }
                _ => false,
            }
        };
        // A heartbeat with no new output does not need to wake a terminal-state waiter.
        // MCP progress reporters read a fresh snapshot on their own interval.
        if changed {
            self.changed.notify_all();
        }
    }

    pub fn finish(&self, id: &str, outcome: Outcome) {
        {
            let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
            guard.finishes += 1;
            let finish_seq = guard.finishes;
            if let Some(review) = guard.reviews.get_mut(id) {
                review.finished = Some(Instant::now());
                review.finish_seq = finish_seq;
                review.denials = outcome.denials;
                review.denial_count = outcome.denial_count;
                review.denial_count_is_floor = outcome.denial_count_is_floor;
                review.warnings = outcome.warnings;
                review.disposition = outcome.disposition;
                review.capture_summary = outcome.capture_summary;
                review.resumable = outcome.resumable;
                review.usage = outcome.usage;
                if outcome.active.is_some() {
                    review.active = outcome.active;
                }
                review.envelope = outcome.envelope;
                match outcome.failure {
                    Some(failure) => {
                        review.status = Status::Failed;
                        review.failure = Some(failure);
                    }
                    None => {
                        review.status = Status::Completed;
                        review.review = outcome.review;
                    }
                }
            }
            // Swept on finish rather than on a timer: this is the only moment a review
            // becomes evictable, and it keeps the registry free of a background thread.
            guard.evict();
        }
        self.changed.notify_all();
    }

    /// Most recent review for a session, running or not.
    pub fn latest_for_session(&self, session: &str) -> Option<String> {
        let guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        guard
            .reviews
            .values()
            .filter(|r| r.session == session)
            .max_by_key(|r| r.started)
            .map(|r| r.id.clone())
    }

    /// What this process knows about an id: present, evicted, or never issued.
    pub fn lookup(&self, id: &str) -> IdState {
        let guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if guard.reviews.contains_key(id) {
            IdState::Known
        } else if self.was_issued(id) {
            IdState::Evicted
        } else {
            IdState::Unknown
        }
    }

    /// Current state without waiting. Used for out-of-band MCP progress notifications.
    pub fn snapshot(&self, id: &str) -> Option<Snapshot> {
        let guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        guard
            .reviews
            .get(id)
            .map(|review| Snapshot::of(review, guard.shutdown))
    }

    /// Was this id minted by this process?
    ///
    /// Derived from the id rather than remembered, which is what makes the evicted/unknown
    /// distinction hold for the whole process lifetime. A list of tombstones would have to
    /// be bounded, and the oldest entry falling off would silently turn a valid id back
    /// into "never existed" -- reintroducing exactly the misleading message the
    /// distinction exists to prevent, at the point where the caller is least likely to
    /// question it.
    ///
    /// Ids are `rv-<pid>-<counter>` and the counter only ever increases, so membership is
    /// a parse and two comparisons.
    fn was_issued(&self, id: &str) -> bool {
        let Some(rest) = id.strip_prefix("rv-") else {
            return false;
        };
        let Some((pid, seq)) = rest.split_once('-') else {
            return false;
        };
        if pid != std::process::id().to_string() {
            return false;
        }
        // Canonical spelling only. `parse` accepts `0001` and `+1`, which would report an
        // id this process never minted as one it evicted -- a smaller version of exactly
        // the wrong answer this distinction exists to avoid.
        match seq.parse::<u64>() {
            Ok(parsed) => {
                parsed.to_string() == seq
                    && parsed >= 1
                    && parsed <= self.counter.load(Ordering::Relaxed)
            }
            Err(_) => false,
        }
    }

    pub fn cancel(&self, id: &str) -> bool {
        let guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        match guard.reviews.get(id) {
            Some(review) if review.status == Status::Running => {
                review.cancel.store(true, Ordering::SeqCst);
                true
            }
            _ => false,
        }
    }

    /// Tell parked waiters that the process is going away, so `serve` can join their
    /// handler threads instead of waiting out their deadlines.
    ///
    /// The flag is set under the same lock the condition variable waits on, and released
    /// before the notify. Setting it outside that lock would reintroduce the stall this
    /// exists to remove: a waiter that had read the flag but not yet reached `wait_timeout`
    /// still holds the lock, so a `notify_all` from here would land before it joined the
    /// wait set, be lost, and leave it sleeping out its full remaining budget anyway.
    /// Keeping the flag inside `State` is what makes that ordering the only spelling
    /// available.
    pub fn begin_shutdown(&self) {
        let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        guard.shutdown = true;
        drop(guard);
        self.changed.notify_all();
    }

    /// Wake every parked waiter so it re-evaluates its own exit conditions — in practice, a caller
    /// that has abandoned its `cross_model_review_result` poll and whose `wait` should stop even
    /// though the review keeps running.
    ///
    /// It takes and drops the state lock before notifying, exactly as `begin_shutdown` does, and
    /// for the same reason: `wait` checks the caller's cancelled flag *while holding this lock*, so
    /// a `notify_all` sent without acquiring it could land after the waiter read the flag as clear
    /// but before it joined the wait set, be lost, and leave it parked out its whole budget. The
    /// caller must set whatever the waiter checks (the request's cancelled flag) *before* calling
    /// this — `handle_cancellation` does, via `RequestCancel::cancel` — so the lock here orders the
    /// two: either the waiter parks first and this wakes it, or this runs first and the waiter's
    /// next check under the lock sees the set flag.
    pub fn wake(&self) {
        let guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        drop(guard);
        self.changed.notify_all();
    }

    /// How many times a waiter has reached the point of parking. See `Registry::parks`.
    #[cfg(test)]
    fn parks(&self) -> u64 {
        self.parks.load(Ordering::SeqCst)
    }

    /// Block until this review leaves `Running`, until `timeout` elapses, until shutdown begins, or
    /// until the caller abandons the request (`is_cancelled`). Returns a snapshot in every case.
    ///
    /// `is_cancelled` is the poll's own cancellation flag. When it trips, the wait returns but the
    /// review is left running and collectible — abandoning a poll detaches the wait, it does not
    /// stop the reviewer. It is read under the state lock so a `wake()` cannot be lost between the
    /// read and the park; see `wake` for the ordering argument.
    pub fn wait(
        &self,
        id: &str,
        timeout: Duration,
        is_cancelled: &dyn Fn() -> bool,
    ) -> Option<Snapshot> {
        let deadline = Instant::now() + timeout;
        let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            // Copied out per iteration, not hoisted: it lives under this lock, and
            // `begin_shutdown` needs the lock to set it, so re-reading it here is what
            // makes a shutdown that lands mid-wait visible on the next wake.
            let shutting_down = guard.shutdown;
            let review = guard.reviews.get(id)?;
            if review.status != Status::Running {
                return Some(Snapshot::of(review, shutting_down));
            }
            // Tested after the terminal state above, so a review that landed in the same
            // moment stdin closed is still returned as the result it is.
            if shutting_down {
                return Some(Snapshot::of(review, shutting_down));
            }
            // The caller abandoned this poll. Stop waiting, but leave the review running — it is
            // still collectible by `review_id`. Read under the lock, after the terminal check, so a
            // review that finished in the same moment is still returned as its result.
            if is_cancelled() {
                return Some(Snapshot::of(review, shutting_down));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Some(Snapshot::of(review, shutting_down));
            }
            // Recorded while the lock is still held, so a test can tell "about to park"
            // from "not scheduled yet". See `Registry::parks`.
            #[cfg(test)]
            self.parks.fetch_add(1, Ordering::SeqCst);
            let (next, _) = self
                .changed
                .wait_timeout(guard, remaining)
                .unwrap_or_else(|e| e.into_inner());
            guard = next;
        }
    }
}

/// A copy of a review's state, taken under the lock so callers never hold it.
#[derive(Clone)]
pub struct Snapshot {
    pub id: String,
    pub session: String,
    pub turn: u32,
    pub resumed: bool,
    pub status: Status,
    pub review: Option<String>,
    pub failure: Option<Failure>,
    pub denials: Vec<String>,
    /// Total number of denied commands. `denials` is only a bounded set of examples.
    pub denial_count: usize,
    /// Whether `denial_count` is a lower bound (the source output was capped).
    pub denial_count_is_floor: bool,
    pub warnings: Vec<String>,
    /// The resume disposition of this turn; see [`Review::disposition`].
    pub disposition: Option<crate::vcs::Disposition>,
    /// What the server captured and sent this turn; see [`Review::capture_summary`].
    pub capture_summary: Option<crate::vcs::CaptureSummary>,
    pub resumable: bool,
    pub elapsed: Duration,
    pub phase: Phase,
    pub phase_elapsed: Duration,
    pub activity_age: Duration,
    pub output_bytes: usize,
    pub usage: Usage,
    /// The reviewer entry currently (or last) running this review; see [`Review::active`].
    pub active: Option<String>,
    /// The structured findings envelope, on a completed turn. `None` while running or on a
    /// failed turn.
    pub envelope: Option<crate::findings::Envelope>,
    /// The server had begun shutting down when this was taken. A `Running` snapshot with
    /// this set means the wait was cut short, not that the caller's budget ran out — and
    /// that no later call can collect the review, because the process is exiting.
    pub shutting_down: bool,
}

impl Snapshot {
    fn of(review: &Review, shutting_down: bool) -> Self {
        Self {
            id: review.id.clone(),
            session: review.session.clone(),
            turn: review.turn,
            resumed: review.resumed,
            status: review.status,
            review: review.review.clone(),
            failure: review.failure.clone(),
            denials: review.denials.clone(),
            denial_count: review.denial_count,
            denial_count_is_floor: review.denial_count_is_floor,
            warnings: review.warnings.clone(),
            disposition: review.disposition.clone(),
            capture_summary: review.capture_summary.clone(),
            resumable: review.resumable,
            elapsed: review.elapsed(),
            phase: review.phase,
            phase_elapsed: review.phase_started.elapsed(),
            activity_age: review.last_activity.elapsed(),
            output_bytes: review.output_bytes,
            usage: review.usage,
            active: review.active.clone(),
            envelope: review.envelope.clone(),
            shutting_down,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_admits_only_one_running_review() {
        let registry = Registry::new();
        let (first, _cancel) = registry
            .try_start("default", 1, false)
            .expect("first start");
        // The check and insert are atomic, so the second claim must be refused and must
        // name the review already holding the session.
        let refused = registry.try_start("default", 2, true).unwrap_err();
        assert_eq!(refused, StartRefused::Busy(first));
    }

    /// Start and immediately finish a review on `session`, returning its id.
    fn run_one(registry: &Registry, session: &str, turn: u32) -> String {
        let (id, _c) = registry.try_start(session, turn, turn > 1).expect("start");
        registry.finish(&id, Outcome::completed("done"));
        id
    }

    #[test]
    fn finished_reviews_beyond_the_per_session_cap_are_evicted_newest_first() {
        // Entries were inserted and never removed, so each completed review held its full
        // text for the life of the process.
        let registry = Registry::new();
        let ids: Vec<String> = (1..=MAX_TERMINAL_PER_SESSION as u32 + 2)
            .map(|turn| run_one(&registry, "default", turn))
            .collect();

        let kept = ids.len() - MAX_TERMINAL_PER_SESSION;
        for old in &ids[..kept] {
            assert_eq!(
                registry.lookup(old),
                IdState::Evicted,
                "{old} should be gone"
            );
        }
        for recent in &ids[kept..] {
            assert_eq!(
                registry.lookup(recent),
                IdState::Known,
                "{recent} should stay"
            );
        }

        // The newest survives, so the review a caller is most likely to collect -- the one
        // it just started -- is never the one thrown away.
        assert_eq!(
            registry.latest_for_session("default").as_deref(),
            ids.last().map(String::as_str)
        );
    }

    #[test]
    fn an_evicted_id_is_distinguishable_from_one_that_never_existed() {
        // Both end in "start a new review", but a caller told its id was never issued has
        // reason to suspect it mangled the id and will go looking for a bug that is not
        // there.
        let registry = Registry::new();
        let ids: Vec<String> = (1..=MAX_TERMINAL_PER_SESSION as u32 + 1)
            .map(|turn| run_one(&registry, "default", turn))
            .collect();

        assert_eq!(registry.lookup(&ids[0]), IdState::Evicted);
        assert_eq!(registry.lookup("rv-1-999999"), IdState::Unknown);
    }

    #[test]
    fn a_running_review_is_never_evicted() {
        // One is by definition still owed to a caller; removing it would strand a poll on
        // an id that has no terminal state left to reach.
        let registry = Registry::new();
        let (running, _c) = registry.try_start("busy", 1, false).expect("start");

        // Enough finished reviews elsewhere to push well past every cap.
        for turn in 1..=MAX_TERMINAL_PER_SESSION as u32 + 3 {
            run_one(&registry, "other", turn);
        }
        assert_eq!(registry.lookup(&running), IdState::Known);

        // And it is still waitable, which is the property that actually matters.
        let snapshot = registry
            .wait(&running, Duration::from_millis(1), &|| false)
            .expect("snapshot");
        assert_eq!(snapshot.status, Status::Running);
    }

    #[test]
    fn a_long_running_review_is_not_evicted_by_the_sweep_its_own_finish_triggers() {
        // Ranking by issue order alone would do exactly that: a review started early and
        // running long holds the lowest counter, so at the moment it finished it would
        // rank oldest and be swept by its own `finish` -- before its caller, which is
        // still holding the id it was handed, could collect it. Recency has to be the
        // primary key; the counter is only a tiebreaker.
        let registry = Registry::new();
        let (slow, _c) = registry.try_start("slow", 1, false).expect("start");

        // Enough traffic elsewhere, all of it started *and* finished after `slow` began,
        // to fill the process-wide cap.
        for n in 0..MAX_TERMINAL_TOTAL {
            run_one(&registry, &format!("busy-{n}"), 1);
        }

        registry.finish(&slow, Outcome::completed("worth having"));
        assert_eq!(
            registry.lookup(&slow),
            IdState::Known,
            "the review that just finished was evicted by its own sweep"
        );
        let snapshot = registry
            .wait(&slow, Duration::from_millis(1), &|| false)
            .expect("snapshot");
        assert_eq!(snapshot.review.as_deref(), Some("worth having"));
    }

    #[test]
    fn eviction_order_does_not_depend_on_the_clock_at_all() {
        // Ranking on the finish `Instant` left a hole: reviews landing in one clock tick
        // compare equal, so a long-running review could still be outranked by later
        // arrivals whose only advantage was a tiebreaker. Completion order is assigned
        // under the lock, so there are no ties to break and the clock is not consulted.
        let registry = Registry::new();
        let (slow, _c) = registry.try_start("slow", 1, false).expect("start");
        for n in 0..MAX_TERMINAL_TOTAL {
            run_one(&registry, &format!("busy-{n}"), 1);
        }
        registry.finish(&slow, Outcome::completed("worth having"));

        // Whatever the clock did, `slow` finished last and is therefore ranked first.
        assert_eq!(registry.lookup(&slow), IdState::Known);
        // Nothing in the sweep reads a timestamp: the finish times of the busy reviews are
        // free to be identical to each other and to `slow`.
        let snapshot = registry
            .wait(&slow, Duration::from_millis(1), &|| false)
            .expect("snapshot");
        assert_eq!(snapshot.review.as_deref(), Some("worth having"));
    }

    #[test]
    fn an_evicted_id_stays_distinguishable_however_many_evictions_follow() {
        // A bounded list of tombstones would drop its oldest entry, turning a valid id
        // back into "never existed" -- the misleading message the distinction exists to
        // prevent, arriving at the point a caller is least likely to question it. The
        // answer is derived from the id instead, so it holds for the process lifetime.
        let registry = Registry::new();
        let first = run_one(&registry, "first", 1);
        assert_eq!(registry.lookup(&first), IdState::Known);

        // Far more evictions than any tombstone list would have held.
        for n in 0..600 {
            run_one(&registry, &format!("later-{n}"), 1);
        }
        assert_eq!(
            registry.lookup(&first),
            IdState::Evicted,
            "an old id decayed into Unknown once enough evictions followed"
        );

        // An id this process never minted is still Unknown, so the distinction is real
        // and not just "anything shaped like an id".
        assert_eq!(registry.lookup("rv-999999-1"), IdState::Unknown);
        assert_eq!(registry.lookup("not-an-id"), IdState::Unknown);
        // Nor may it claim ids it has not issued yet.
        assert_eq!(registry.lookup("rv-999999999-1"), IdState::Unknown);
        let future = format!("rv-{}-999999", std::process::id());
        assert_eq!(registry.lookup(&future), IdState::Unknown);
    }

    #[test]
    fn only_the_canonical_spelling_of_an_id_is_recognised() {
        // `parse::<u64>` accepts `0001` and `+1`, so a lenient check would report an id
        // this process never minted as one it evicted -- a smaller version of the wrong
        // answer the whole distinction exists to avoid.
        let registry = Registry::new();
        let real = run_one(&registry, "s", 1);
        let seq = real.rsplit('-').next().expect("counter");
        assert_eq!(seq, "1");

        let pid = std::process::id();
        for alias in [
            format!("rv-{pid}-0001"),
            format!("rv-{pid}-+1"),
            format!("rv-{pid}- 1"),
            format!("rv-{pid}-1 "),
        ] {
            assert_eq!(
                registry.lookup(&alias),
                IdState::Unknown,
                "{alias} was accepted as a real id"
            );
        }
        // The canonical form is of course still recognised.
        assert_eq!(registry.lookup(&real), IdState::Known);
    }

    #[test]
    fn the_global_cap_bounds_a_caller_that_invents_a_new_session_each_time() {
        // Session names come from the caller, so a per-session cap alone would still
        // accumulate one review per name for ever.
        let registry = Registry::new();
        let ids: Vec<String> = (0..MAX_TERMINAL_TOTAL + 10)
            .map(|n| run_one(&registry, &format!("session-{n}"), 1))
            .collect();

        let live = ids
            .iter()
            .filter(|id| registry.lookup(id) == IdState::Known)
            .count();
        assert_eq!(live, MAX_TERMINAL_TOTAL);
        // The survivors are the most recent ones.
        assert_eq!(registry.lookup(ids.last().unwrap()), IdState::Known);
        assert_eq!(registry.lookup(&ids[0]), IdState::Evicted);
    }

    #[test]
    fn different_sessions_do_not_block_each_other() {
        let registry = Registry::new();
        registry.try_start("one", 1, false).expect("one");
        registry.try_start("two", 1, false).expect("two");
    }

    #[test]
    fn a_session_is_reusable_once_its_review_finishes() {
        let registry = Registry::new();
        let (first, _c) = registry.try_start("default", 1, false).expect("first");
        registry.finish(&first, Outcome::completed("done"));
        let (second, _c) = registry.try_start("default", 2, true).expect("second turn");
        assert_ne!(second, first);
    }

    #[test]
    fn ids_are_unique() {
        let registry = Registry::new();
        let (a, _) = registry.try_start("a", 1, false).expect("a");
        let (b, _) = registry.try_start("b", 1, false).expect("b");
        assert_ne!(a, b);
    }

    #[test]
    fn wait_returns_immediately_once_finished() {
        let registry = Registry::new();
        let (id, _c) = registry.try_start("default", 1, false).expect("start");
        registry.finish(&id, Outcome::failed(crate::errors::cancelled()));
        // Already terminal: must not burn the timeout.
        let started = Instant::now();
        let snapshot = registry
            .wait(&id, Duration::from_secs(30), &|| false)
            .expect("snapshot");
        assert_eq!(snapshot.status, Status::Failed);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn wait_is_woken_by_a_finish_on_another_thread() {
        // Guards the condvar pairing: the waiter must not sleep out its full timeout.
        let registry = Arc::new(Registry::new());
        let (id, _c) = registry.try_start("default", 1, false).expect("start");

        let worker = {
            let registry = Arc::clone(&registry);
            let id = id.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(150));
                registry.finish(&id, Outcome::completed("late"));
            })
        };

        let started = Instant::now();
        let snapshot = registry
            .wait(&id, Duration::from_secs(30), &|| false)
            .expect("snapshot");
        worker.join().expect("worker");
        assert_eq!(snapshot.status, Status::Completed);
        assert_eq!(snapshot.review.as_deref(), Some("late"));
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "waiter was not woken promptly"
        );
    }

    #[test]
    fn wait_on_a_still_running_review_reports_running() {
        let registry = Registry::new();
        let (id, _c) = registry.try_start("default", 1, false).expect("start");
        let snapshot = registry
            .wait(&id, Duration::from_millis(50), &|| false)
            .expect("snapshot");
        assert_eq!(snapshot.status, Status::Running);
    }

    #[test]
    fn snapshots_expose_observed_phase_liveness_and_output() {
        let registry = Registry::new();
        let (id, _c) = registry.try_start("default", 1, false).expect("start");

        let preparing = registry.snapshot(&id).expect("snapshot");
        assert_eq!(preparing.phase, Phase::Preparing);
        assert_eq!(preparing.output_bytes, 0);

        registry.set_phase(&id, Phase::Launching);
        registry.report_activity(&id, 12_345);
        let reviewing = registry.snapshot(&id).expect("snapshot");
        assert_eq!(reviewing.phase, Phase::Reviewing);
        assert_eq!(reviewing.output_bytes, 12_345);
        assert!(
            reviewing.activity_age < Duration::from_secs(1),
            "the activity report did not refresh liveness"
        );

        registry.set_phase(&id, Phase::Finalizing);
        assert_eq!(
            registry.snapshot(&id).expect("snapshot").phase,
            Phase::Finalizing
        );
    }

    #[test]
    fn shutdown_wakes_a_parked_waiter() {
        // The regression: `serve` joins in-flight handlers, so a poll parked here with a
        // 300s budget used to hold the process open for the rest of it after stdin closed.
        let registry = Arc::new(Registry::new());
        let (id, _c) = registry.try_start("default", 1, false).expect("start");

        let closer = {
            let registry = Arc::clone(&registry);
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(150));
                registry.begin_shutdown();
            })
        };

        // Long enough that only the shutdown can plausibly end it, short enough that a
        // regression fails on the assertion below rather than stalling CI for minutes.
        let started = Instant::now();
        let snapshot = registry
            .wait(&id, Duration::from_secs(30), &|| false)
            .expect("snapshot");
        closer.join().expect("closer");
        assert_eq!(snapshot.status, Status::Running);
        assert!(snapshot.shutting_down);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "shutdown did not end the wait"
        );
    }

    /// Proves the wake path instead of trusting a sleep, which the test above cannot: it
    /// assumes 150 ms was enough for the waiter to reach `wait_timeout`, and on a loaded box
    /// a waiter that was never scheduled at all is indistinguishable from a parked one — so
    /// it would pass even with `notify_all` deleted.
    ///
    /// The lock is what settles it. The waiter records `parks` while still holding the state
    /// lock, so once this thread has seen that counter move, `begin_shutdown` cannot get the
    /// lock until `wait_timeout` has released it — and `wait_timeout` releases it and joins
    /// the condvar's wait set as one atomic step. The flag is therefore raised only after
    /// the waiter is provably in the wait set, which leaves the notify as the only thing
    /// that can free it before the 30s deadline.
    #[test]
    fn a_provably_parked_waiter_is_freed_by_the_notify() {
        let registry = Arc::new(Registry::new());
        let (id, _c) = registry.try_start("default", 1, false).expect("start");

        let poller = {
            let registry = Arc::clone(&registry);
            let id = id.clone();
            std::thread::spawn(move || registry.wait(&id, Duration::from_secs(30), &|| false))
        };

        // Bounded: `cargo test` has no per-test timeout, so an unbounded spin would turn a
        // waiter that never parks into a silent CI hang instead of a readable failure.
        let give_up = Instant::now() + Duration::from_secs(10);
        while registry.parks() == 0 {
            assert!(
                Instant::now() < give_up,
                "no waiter ever reached the point of parking"
            );
            std::thread::sleep(Duration::from_millis(5));
        }

        let started = Instant::now();
        registry.begin_shutdown();
        let snapshot = poller.join().expect("poller").expect("snapshot");
        assert!(snapshot.shutting_down);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the notify did not free a waiter that was definitely parked"
        );
    }

    /// The detach wake: abandoning a `cross_model_review_result` poll must free the parked wait
    /// *without* stopping the review. Uses the same `parks` barrier as the shutdown test above so a
    /// lost-notification regression fails here rather than passing on timing luck — the flag is set
    /// and `wake()` called only once the waiter is provably in the condvar's wait set.
    #[test]
    fn a_cancelled_poll_frees_the_waiter_without_stopping_the_review() {
        let registry = Arc::new(Registry::new());
        let (id, _c) = registry.try_start("default", 1, false).expect("start");
        let cancelled = Arc::new(AtomicBool::new(false));

        let poller = {
            let registry = Arc::clone(&registry);
            let id = id.clone();
            let cancelled = Arc::clone(&cancelled);
            std::thread::spawn(move || {
                registry.wait(&id, Duration::from_secs(30), &|| {
                    cancelled.load(Ordering::SeqCst)
                })
            })
        };

        let give_up = Instant::now() + Duration::from_secs(10);
        while registry.parks() == 0 {
            assert!(
                Instant::now() < give_up,
                "no waiter ever reached the point of parking"
            );
            std::thread::sleep(Duration::from_millis(5));
        }

        // Set the flag *before* wake(), exactly as `handle_cancellation` sets the request's
        // cancelled flag before calling `registry.wake()`. The state lock in `wake()` is the
        // barrier that orders the two.
        let started = Instant::now();
        cancelled.store(true, Ordering::SeqCst);
        registry.wake();
        let snapshot = poller.join().expect("poller").expect("snapshot");

        // The wait ended because the poll was abandoned, not because of shutdown...
        assert!(!snapshot.shutting_down);
        // ...and the review was left running and collectible, not stopped.
        assert_eq!(snapshot.status, Status::Running);
        assert_eq!(
            registry.snapshot(&id).expect("still tracked").status,
            Status::Running
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the cancellation wake did not free a waiter that was definitely parked"
        );
    }

    #[test]
    fn the_concurrency_cap_refuses_starts_past_the_limit() {
        let registry = Registry::with_max_concurrent(2);
        let (a, _ca) = registry.try_start("a", 1, false).expect("first");
        let (_b, _cb) = registry.try_start("b", 1, false).expect("second");
        assert_eq!(
            registry.try_start("c", 1, false).unwrap_err(),
            StartRefused::TooManyRunning { limit: 2 }
        );
        // A running session still reports the specific, actionable Busy rather than the cap.
        assert!(matches!(
            registry.try_start("a", 2, true).unwrap_err(),
            StartRefused::Busy(_)
        ));
        // Finishing a review frees its slot.
        registry.finish(&a, Outcome::completed("done"));
        registry.try_start("c", 1, false).expect("a slot freed up");
    }

    #[test]
    fn a_zero_concurrency_cap_is_disabled() {
        let registry = Registry::with_max_concurrent(0);
        for name in ["a", "b", "c", "d", "e"] {
            registry
                .try_start(name, 1, false)
                .unwrap_or_else(|_| panic!("start {name}"));
        }
    }

    #[test]
    fn shutdown_refuses_a_review_that_could_never_be_collected() {
        // The other half of shutdown: a handler can arrive here after stdin closed, having
        // spent the interval in preflight or waiting for the session lease. Registering the
        // review would answer "review started" and then exit, billing a reviewer turn for a
        // result with nowhere to go.
        let registry = Registry::new();
        registry.begin_shutdown();
        assert_eq!(
            registry.try_start("default", 1, false).unwrap_err(),
            StartRefused::ShuttingDown
        );
    }

    #[test]
    fn a_wait_begun_after_shutdown_does_not_park_at_all() {
        let registry = Registry::new();
        let (id, _c) = registry.try_start("default", 1, false).expect("start");
        registry.begin_shutdown();

        let started = Instant::now();
        let snapshot = registry
            .wait(&id, Duration::from_secs(30), &|| false)
            .expect("snapshot");
        assert_eq!(snapshot.status, Status::Running);
        assert!(snapshot.shutting_down);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn a_review_that_finished_is_still_returned_during_shutdown() {
        // Ordering inside `wait`: a review completing in the same moment stdin closes must
        // hand back its text, not a running snapshot that discards it.
        let registry = Registry::new();
        let (id, _c) = registry.try_start("default", 1, false).expect("start");
        registry.begin_shutdown();
        registry.finish(&id, Outcome::completed("just in time"));

        let snapshot = registry
            .wait(&id, Duration::from_secs(30), &|| false)
            .expect("snapshot");
        assert_eq!(snapshot.status, Status::Completed);
        assert_eq!(snapshot.review.as_deref(), Some("just in time"));
    }

    #[test]
    fn cancel_only_applies_to_running_reviews() {
        let registry = Registry::new();
        let (id, cancel) = registry.try_start("default", 1, false).expect("start");
        assert!(registry.cancel(&id));
        assert!(cancel.load(Ordering::SeqCst));

        registry.finish(&id, Outcome::failed(crate::errors::cancelled()));
        assert!(
            !registry.cancel(&id),
            "a finished review cannot be cancelled"
        );
        assert!(
            !registry.cancel("rv-nope"),
            "an unknown id cannot be cancelled"
        );
    }

    #[test]
    fn warnings_survive_to_the_snapshot() {
        let registry = Registry::new();
        let (id, _c) = registry.try_start("default", 1, false).expect("start");
        registry.finish(
            &id,
            Outcome {
                warnings: vec!["could not save session".into()],
                resumable: false,
                ..Outcome::completed("ok")
            },
        );
        let snapshot = registry
            .wait(&id, Duration::ZERO, &|| false)
            .expect("snapshot");
        assert_eq!(
            snapshot.warnings,
            vec!["could not save session".to_string()]
        );
        // A review that could not be persisted must not be advertised as resumable.
        assert!(!snapshot.resumable);
    }

    #[test]
    fn a_disposition_survives_to_the_snapshot() {
        // The typed disposition must ride the full Outcome -> Review -> Snapshot path so the
        // blocking collect can render its line. This pins the plumbing PR #43 restructured under
        // the feature -- a rebase that dropped the field would compile and pass every other test.
        use crate::vcs::disposition::{Disposition, FellBack, FullByDesign, Incremental};
        let registry = Registry::new();

        // An incremental delta carries through with its detail intact.
        let (id, _c) = registry.try_start("default", 2, true).expect("start");
        registry.finish(
            &id,
            Outcome {
                disposition: Some(Disposition::Incremental(Incremental::GitRange {
                    prior: "aaaa".into(),
                    head: "bbbb".into(),
                    commits: Some(2),
                })),
                ..Outcome::completed("ok")
            },
        );
        let snapshot = registry
            .wait(&id, Duration::ZERO, &|| false)
            .expect("snapshot");
        assert!(
            matches!(
                snapshot.disposition,
                Some(Disposition::Incremental(Incremental::GitRange {
                    commits: Some(2),
                    ..
                }))
            ),
            "the incremental disposition and its count must reach the snapshot: {:?}",
            snapshot.disposition
        );

        // A by-design full capture carries through and does not warn.
        let (id2, _c2) = registry.try_start("other", 2, true).expect("start");
        registry.finish(
            &id2,
            Outcome {
                disposition: Some(Disposition::FullByDesign(FullByDesign::ModeNotDeltable)),
                ..Outcome::completed("ok")
            },
        );
        let snap2 = registry
            .wait(&id2, Duration::ZERO, &|| false)
            .expect("snapshot");
        assert_eq!(
            snap2.disposition,
            Some(Disposition::FullByDesign(FullByDesign::ModeNotDeltable))
        );
        assert!(!snap2.disposition.unwrap().warns());

        // A failed turn carries no disposition (it sent no reviewable change).
        let (id3, _c3) = registry.try_start("third", 2, true).expect("start");
        registry.finish(&id3, Outcome::failed(crate::errors::cancelled()));
        let snap3 = registry
            .wait(&id3, Duration::ZERO, &|| false)
            .expect("snapshot");
        assert!(snap3.disposition.is_none());
        // Sanity: FellBack is the warning-bearing variant, distinct from the above.
        assert!(Disposition::FellBackToFull(FellBack::BaseMoved).warns());
    }

    #[test]
    fn denial_count_survives_to_the_snapshot_separately_from_examples() {
        let registry = Registry::new();
        let (id, _c) = registry.try_start("default", 1, false).expect("start");
        registry.finish(
            &id,
            Outcome {
                denials: vec!["git grep example".into()],
                denial_count: 101,
                denial_count_is_floor: true,
                ..Outcome::completed("ok")
            },
        );
        let snapshot = registry
            .wait(&id, Duration::ZERO, &|| false)
            .expect("snapshot");
        assert_eq!(snapshot.denial_count, 101);
        assert_eq!(snapshot.denials, vec!["git grep example"]);
        // The floor flag must travel with the count, or the render presents a truncated
        // total as exact.
        assert!(snapshot.denial_count_is_floor);
    }

    #[test]
    fn a_persisted_review_is_marked_resumable() {
        let registry = Registry::new();
        let (id, _c) = registry.try_start("default", 1, false).expect("start");
        registry.finish(&id, Outcome::completed("ok"));
        let snapshot = registry
            .wait(&id, Duration::ZERO, &|| false)
            .expect("snapshot");
        assert!(snapshot.resumable);
        assert!(snapshot.warnings.is_empty());
    }
}
