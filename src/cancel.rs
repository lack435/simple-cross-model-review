//! Per-request cancellation state, shared between the protocol loop and the tools.
//!
//! An MCP client abandons a request with `notifications/cancelled`. The receiver is meant
//! to stop processing it and send no response, so every `tools/call` handler carries one
//! of these. The protocol loop flips it and the handler's reply is suppressed.
//!
//! It also carries the review the call started or is waiting on, because suppressing the
//! reply is the cheap half. The expensive half is the reviewer: left alone it keeps
//! working, and keeps costing, for the rest of its timeout budget on behalf of a caller
//! that has gone away.

use std::sync::{Mutex, MutexGuard};

/// Cancellation state for one in-flight request.
#[derive(Default)]
pub struct RequestCancel {
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    cancelled: bool,
    /// Whether a response has been committed to. Once it has, the client has been given
    /// the review's id and a late cancellation must not take the review away from it.
    responded: bool,
    /// The review this request started or is waiting on, once it is known.
    review_id: Option<String>,
}

impl RequestCancel {
    pub fn new() -> Self {
        Self::default()
    }

    /// Bind this request to a review, and report whether the client has already
    /// cancelled.
    ///
    /// Both happen under one lock. Doing them separately would lose a cancellation that
    /// lands in the gap: the notification would find no review attached, and the caller
    /// would see a stale "not cancelled" and start waiting on a review nobody wants.
    pub fn attach_review(&self, review_id: &str) -> bool {
        let mut state = self.lock();
        state.review_id = Some(review_id.to_string());
        state.cancelled
    }

    /// Claim the right to answer this request, or report that the client has cancelled
    /// and no response may be sent.
    ///
    /// Claiming and cancelling contend for one lock, so exactly one of them wins. That is
    /// what stops the sequence "response goes out naming review X, then X is killed" --
    /// the loser of the race either suppresses its response or leaves the review alone.
    /// The lock is held for a flag flip only, never across the write to stdout: a client
    /// that stopped draining stdout would otherwise stall the reader thread, and with it
    /// every other request's cancellation.
    pub fn try_claim_response(&self) -> bool {
        let mut state = self.lock();
        if state.cancelled {
            return false;
        }
        state.responded = true;
        true
    }

    /// Mark the request cancelled, returning the review to stop if there is one still to
    /// stop. A request that has already committed to a response reports none.
    pub fn cancel(&self) -> Option<String> {
        let mut state = self.lock();
        state.cancelled = true;
        if state.responded {
            return None;
        }
        state.review_id.clone()
    }

    /// Has the client abandoned this request?
    ///
    /// Progress notifications use this as a best-effort pre-send check. The lock is never
    /// held across stdout: a client that stopped draining it must not stall the protocol
    /// reader and prevent cancellation of every other request.
    pub fn is_cancelled(&self) -> bool {
        self.lock().cancelled
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_request_may_answer_and_owns_nothing() {
        assert!(RequestCancel::new().try_claim_response());
        assert_eq!(RequestCancel::new().cancel(), None);
    }

    #[test]
    fn cancelling_reports_the_attached_review_and_bars_a_response() {
        let request = RequestCancel::new();
        assert!(!request.attach_review("rv-1-1"));
        assert_eq!(request.cancel().as_deref(), Some("rv-1-1"));
        assert!(!request.try_claim_response());
    }

    #[test]
    fn attaching_after_a_cancellation_says_so() {
        // The losing side of the race: the notification arrived before the review was
        // registered, so attach must tell the caller to stop it itself.
        let request = RequestCancel::new();
        assert_eq!(request.cancel(), None);
        assert!(request.attach_review("rv-1-2"));
    }

    #[test]
    fn cancelling_twice_still_names_the_review() {
        let request = RequestCancel::new();
        request.attach_review("rv-1-3");
        assert_eq!(request.cancel().as_deref(), Some("rv-1-3"));
        assert_eq!(request.cancel().as_deref(), Some("rv-1-3"));
    }

    #[test]
    fn a_claimed_response_wins_and_keeps_its_review() {
        let request = RequestCancel::new();
        request.attach_review("rv-1-4");
        assert!(request.try_claim_response());
        // The client has been handed the review id, so a cancellation arriving now must
        // not go on to kill the review that response named.
        assert_eq!(request.cancel(), None);
    }

    #[test]
    fn a_cancellation_wins_against_a_later_claim() {
        let request = RequestCancel::new();
        request.attach_review("rv-1-5");
        assert_eq!(request.cancel().as_deref(), Some("rv-1-5"));
        assert!(!request.try_claim_response());
    }

    #[test]
    fn cancellation_state_is_observable_without_claiming_a_response() {
        let request = RequestCancel::new();
        assert!(!request.is_cancelled());
        request.cancel();
        assert!(request.is_cancelled());
        assert!(!request.try_claim_response());
    }
}
