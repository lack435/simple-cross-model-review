//! In-flight review tracking.
//!
//! `cross_model_review` returns immediately and a worker thread runs the reviewer CLI.
//! `cross_model_review_result` long-polls here, so the calling agent waits on a
//! condition variable instead of burning turns on a retry loop.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::errors::Failure;

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
    /// Problems that did not invalidate the review but that the caller must know about.
    pub warnings: Vec<String>,
    /// Whether a follow-up call on this session name will actually reach the same
    /// reviewer conversation. Tracked rather than assumed, so the response never invites
    /// a resume that would silently start over.
    pub resumable: bool,
    pub started: Instant,
    pub finished: Option<Instant>,
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
    pub warnings: Vec<String>,
    pub resumable: bool,
}

impl Outcome {
    pub fn failed(failure: Failure) -> Self {
        Self {
            review: None,
            failure: Some(failure),
            denials: Vec::new(),
            warnings: Vec::new(),
            resumable: false,
        }
    }

    #[cfg(test)]
    fn completed(review: &str) -> Self {
        Self {
            review: Some(review.to_string()),
            failure: None,
            denials: Vec::new(),
            warnings: Vec::new(),
            resumable: true,
        }
    }
}

/// Everything the registry mutates, under one lock.
///
/// The evicted-session set lives here rather than beside it so that dropping a review and
/// recording that its session lost one cannot be observed half-done: a poll landing
/// between the two would otherwise be told no review was ever started.
#[derive(Default)]
struct State {
    reviews: HashMap<String, Review>,
    /// Sessions that have had a review evicted.
    ///
    /// Names only. A name is a handful of bytes standing in for the kilobytes of review
    /// text it replaced, so this cannot reintroduce the growth the caps exist to prevent.
    /// Ids need no equivalent -- see `Registry::was_issued`.
    evicted_sessions: HashSet<String>,
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

        for (id, session) in doomed {
            self.reviews.remove(&id);
            self.evicted_sessions.insert(session);
        }
    }
}

#[derive(Default)]
pub struct Registry {
    state: Mutex<State>,
    changed: Condvar,
    counter: AtomicU64,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Claim a session and register a new review for it, or fail with the id of the
    /// review already running there.
    ///
    /// The check and the insert happen under a single lock acquisition. Doing them
    /// separately would let two concurrent `tools/call` handlers both see an idle
    /// session and both start a review against the same reviewer conversation.
    pub fn try_start(
        &self,
        session: &str,
        turn: u32,
        resumed: bool,
    ) -> Result<(String, Arc<AtomicBool>), String> {
        let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());

        if let Some(running) = guard
            .reviews
            .values()
            .find(|r| r.session == session && r.status == Status::Running)
        {
            return Err(running.id.clone());
        }

        // Ids are unique per process and readable in logs. No RNG dependency: the
        // process id plus a monotonic counter is already collision-free here.
        let seq = self.counter.fetch_add(1, Ordering::Relaxed) + 1;
        let id = format!("rv-{}-{}", std::process::id(), seq);
        let cancel = Arc::new(AtomicBool::new(false));
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
                warnings: Vec::new(),
                resumable: false,
                started: Instant::now(),
                finished: None,
                cancel: Arc::clone(&cancel),
                finish_seq: 0,
            },
        );
        drop(guard);
        self.changed.notify_all();
        Ok((id, cancel))
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
                review.warnings = outcome.warnings;
                review.resumable = outcome.resumable;
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
        match seq.parse::<u64>() {
            Ok(seq) => seq >= 1 && seq <= self.counter.load(Ordering::Relaxed),
            Err(_) => false,
        }
    }

    /// Did this session have a review that has since been evicted?
    ///
    /// Answers the `session`-keyed form of the same question `lookup` answers for an id.
    /// Without it, a caller polling by session name after the global cap emptied that
    /// session is told no review was ever started for it -- the "never issued" advice
    /// that eviction is specifically not supposed to produce.
    pub fn session_had_evicted(&self, session: &str) -> bool {
        let guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        guard.evicted_sessions.contains(session)
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

    /// Block until this review leaves `Running`, or until `timeout` elapses.
    /// Returns a snapshot either way.
    pub fn wait(&self, id: &str, timeout: Duration) -> Option<Snapshot> {
        let deadline = Instant::now() + timeout;
        let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            let review = guard.reviews.get(id)?;
            if review.status != Status::Running {
                return Some(Snapshot::of(review));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Some(Snapshot::of(review));
            }
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
    pub warnings: Vec<String>,
    pub resumable: bool,
    pub elapsed: Duration,
}

impl Snapshot {
    fn of(review: &Review) -> Self {
        Self {
            id: review.id.clone(),
            session: review.session.clone(),
            turn: review.turn,
            resumed: review.resumed,
            status: review.status,
            review: review.review.clone(),
            failure: review.failure.clone(),
            denials: review.denials.clone(),
            warnings: review.warnings.clone(),
            resumable: review.resumable,
            elapsed: review.elapsed(),
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
        assert_eq!(refused, first);
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
            .wait(&running, Duration::from_millis(1))
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
            .wait(&slow, Duration::from_millis(1))
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
            .wait(&slow, Duration::from_millis(1))
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
            .wait(&id, Duration::from_secs(30))
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
            .wait(&id, Duration::from_secs(30))
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
            .wait(&id, Duration::from_millis(50))
            .expect("snapshot");
        assert_eq!(snapshot.status, Status::Running);
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
        let snapshot = registry.wait(&id, Duration::ZERO).expect("snapshot");
        assert_eq!(
            snapshot.warnings,
            vec!["could not save session".to_string()]
        );
        // A review that could not be persisted must not be advertised as resumable.
        assert!(!snapshot.resumable);
    }

    #[test]
    fn a_persisted_review_is_marked_resumable() {
        let registry = Registry::new();
        let (id, _c) = registry.try_start("default", 1, false).expect("start");
        registry.finish(&id, Outcome::completed("ok"));
        let snapshot = registry.wait(&id, Duration::ZERO).expect("snapshot");
        assert!(snapshot.resumable);
        assert!(snapshot.warnings.is_empty());
    }
}
