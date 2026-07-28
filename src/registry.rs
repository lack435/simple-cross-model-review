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

    /// Ids are unique per process and readable in logs. No RNG dependency: the
    /// process id plus a monotonic counter is already collision-free for our purposes.
    pub fn next_id(&self) -> String {
        let n = self.counter.fetch_add(1, Ordering::Relaxed) + 1;
        format!("rv-{}-{}", std::process::id(), n)
    }

    pub fn start(&self, id: String, session: String, turn: u32, resumed: bool) -> Arc<AtomicBool> {
        let cancel = Arc::new(AtomicBool::new(false));
        let review = Review {
            id: id.clone(),
            session,
            turn,
            resumed,
            status: Status::Running,
            review: None,
            failure: None,
            denials: Vec::new(),
            started: Instant::now(),
            finished: None,
            cancel: Arc::clone(&cancel),
        };
        let mut guard = self.reviews.lock().unwrap_or_else(|e| e.into_inner());
        guard.insert(id, review);
        drop(guard);
        self.changed.notify_all();
        cancel
    }

    pub fn finish(&self, id: &str, outcome: Outcome) {
        {
            let mut guard = self.reviews.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(review) = guard.get_mut(id) {
                review.finished = Some(Instant::now());
                review.denials = outcome.denials;
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

    /// Is a review currently running for this named session?
    pub fn running_for_session(&self, session: &str) -> Option<String> {
        let guard = self.reviews.lock().unwrap_or_else(|e| e.into_inner());
        guard
            .values()
            .find(|r| r.session == session && r.status == Status::Running)
            .map(|r| r.id.clone())
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
            elapsed: review.elapsed(),
        }
    }
}
