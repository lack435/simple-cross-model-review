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

/// What cancelling a request should do to the review it is bound to.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum CancelAction {
    /// Stop the review. The request *owns* it — a `cross_model_review` whose `review_id` was
    /// never delivered, so nobody could ever collect it. Carries the id to stop.
    Kill(String),
    /// Leave the review running and collectible; only stop waiting on it. A cancelled
    /// `cross_model_review_result` poll: the caller holds the `review_id` and can come back for
    /// it, so the destructive read of "won't return" is not made. The parked wait is woken so the
    /// handler thread does not linger.
    Detach,
    /// Nothing to stop: no review was bound, or a response was already committed (the client has
    /// the id and a late cancellation must not take the review away from it).
    Nothing,
}

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
    /// Whether this request *owns* the bound review's lifecycle. `true` for the start call
    /// (`attach_owned`), `false` for a result poll (`attach_wait`). Only an owned review is killed
    /// on cancellation; a waited one is merely detached.
    owned: bool,
}

impl RequestCancel {
    pub fn new() -> Self {
        Self::default()
    }

    /// Bind this request to a review it *owns*, and report whether the client has already
    /// cancelled. Used by `cross_model_review`: a cancellation kills the review, because its id
    /// was never delivered and nobody could ever collect it.
    ///
    /// Bind and read happen under one lock. Doing them separately would lose a cancellation that
    /// lands in the gap: the notification would find no review attached, and the caller would see a
    /// stale "not cancelled" and start waiting on a review nobody wants.
    pub fn attach_owned(&self, review_id: &str) -> bool {
        self.attach(review_id, true)
    }

    /// Bind this request as a *waiter* on a review it does not own, and report whether the client
    /// has already cancelled. Used by `cross_model_review_result`: a cancellation only detaches the
    /// wait and leaves the review running and collectible. On the pre-attach race — a cancellation
    /// that arrived before this bind — the caller returns `CANCELLED` **without** stopping the
    /// review, so the review survives even that narrow window.
    pub fn attach_wait(&self, review_id: &str) -> bool {
        self.attach(review_id, false)
    }

    fn attach(&self, review_id: &str, owned: bool) -> bool {
        let mut state = self.lock();
        state.review_id = Some(review_id.to_string());
        state.owned = owned;
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

    /// Mark the request cancelled, and report what should happen to the review it is bound to.
    ///
    /// A request that has already committed to a response reports `Nothing` — the client has the
    /// id and the review must not be taken away. Otherwise an *owned* review is `Kill`ed and a
    /// *waited* one is `Detach`ed; an unbound request reports `Nothing`.
    pub fn cancel(&self) -> CancelAction {
        let mut state = self.lock();
        state.cancelled = true;
        if state.responded {
            return CancelAction::Nothing;
        }
        match &state.review_id {
            Some(id) if state.owned => CancelAction::Kill(id.clone()),
            Some(_) => CancelAction::Detach,
            None => CancelAction::Nothing,
        }
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
        assert_eq!(RequestCancel::new().cancel(), CancelAction::Nothing);
    }

    #[test]
    fn cancelling_an_owned_review_kills_it_and_bars_a_response() {
        let request = RequestCancel::new();
        assert!(!request.attach_owned("rv-1-1"));
        assert_eq!(request.cancel(), CancelAction::Kill("rv-1-1".to_string()));
        assert!(!request.try_claim_response());
    }

    #[test]
    fn cancelling_a_waited_review_detaches_rather_than_killing() {
        // A cross_model_review_result poll: the caller holds the id and can come back for the
        // review, so cancellation must not stop it.
        let request = RequestCancel::new();
        assert!(!request.attach_wait("rv-1-1b"));
        assert_eq!(request.cancel(), CancelAction::Detach);
        assert!(!request.try_claim_response());
    }

    #[test]
    fn attaching_an_owned_review_after_a_cancellation_says_so() {
        // The losing side of the race: the notification arrived before the review was
        // registered, so attach must tell the caller to stop it itself.
        let request = RequestCancel::new();
        assert_eq!(request.cancel(), CancelAction::Nothing);
        assert!(request.attach_owned("rv-1-2"));
    }

    #[test]
    fn attaching_a_waited_review_after_a_cancellation_says_so() {
        // Same race on the poll path: the caller is told it is cancelled and returns CANCELLED,
        // but it does *not* stop the review — the review survives this window too.
        let request = RequestCancel::new();
        assert_eq!(request.cancel(), CancelAction::Nothing);
        assert!(request.attach_wait("rv-1-2b"));
    }

    #[test]
    fn cancelling_twice_still_names_the_review() {
        let request = RequestCancel::new();
        request.attach_owned("rv-1-3");
        assert_eq!(request.cancel(), CancelAction::Kill("rv-1-3".to_string()));
        assert_eq!(request.cancel(), CancelAction::Kill("rv-1-3".to_string()));
    }

    #[test]
    fn a_claimed_response_wins_and_keeps_its_review() {
        let request = RequestCancel::new();
        request.attach_owned("rv-1-4");
        assert!(request.try_claim_response());
        // The client has been handed the review id, so a cancellation arriving now must
        // not go on to kill the review that response named.
        assert_eq!(request.cancel(), CancelAction::Nothing);
    }

    #[test]
    fn a_cancellation_wins_against_a_later_claim() {
        let request = RequestCancel::new();
        request.attach_owned("rv-1-5");
        assert_eq!(request.cancel(), CancelAction::Kill("rv-1-5".to_string()));
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

    #[test]
    fn an_unbound_cancellation_does_nothing_either_way() {
        // No review attached at all -- neither owned nor waited -- so there is nothing to kill
        // and nothing to detach.
        let request = RequestCancel::new();
        assert_eq!(request.cancel(), CancelAction::Nothing);
    }
}
