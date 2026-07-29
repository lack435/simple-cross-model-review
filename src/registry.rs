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

#[derive(Default)]
pub struct Registry {
    reviews: Mutex<HashMap<String, Review>>,
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
        let mut guard = self.reviews.lock().unwrap_or_else(|e| e.into_inner());

        if let Some(running) = guard
            .values()
            .find(|r| r.session == session && r.status == Status::Running)
        {
            return Err(running.id.clone());
        }

        // Ids are unique per process and readable in logs. No RNG dependency: the
        // process id plus a monotonic counter is already collision-free here.
        let id = format!(
            "rv-{}-{}",
            std::process::id(),
            self.counter.fetch_add(1, Ordering::Relaxed) + 1
        );
        let cancel = Arc::new(AtomicBool::new(false));
        guard.insert(
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
            },
        );
        drop(guard);
        self.changed.notify_all();
        Ok((id, cancel))
    }

    pub fn finish(&self, id: &str, outcome: Outcome) {
        {
            let mut guard = self.reviews.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(review) = guard.get_mut(id) {
                review.finished = Some(Instant::now());
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
        }
        self.changed.notify_all();
    }

    /// Most recent review for a session, running or not.
    pub fn latest_for_session(&self, session: &str) -> Option<String> {
        let guard = self.reviews.lock().unwrap_or_else(|e| e.into_inner());
        guard
            .values()
            .filter(|r| r.session == session)
            .max_by_key(|r| r.started)
            .map(|r| r.id.clone())
    }

    pub fn exists(&self, id: &str) -> bool {
        let guard = self.reviews.lock().unwrap_or_else(|e| e.into_inner());
        guard.contains_key(id)
    }

    pub fn cancel(&self, id: &str) -> bool {
        let guard = self.reviews.lock().unwrap_or_else(|e| e.into_inner());
        match guard.get(id) {
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
        let mut guard = self.reviews.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            let review = guard.get(id)?;
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
