//! Capturing the change under review.
//!
//! Only **Perforce** captures a change here now: [`perforce`] runs `p4` over a client and hands the
//! reviewer the named changelists. Git no longer pre-captures (retire-capture-modes) — a git review
//! derives the change live through the read-only evidence service's `repository_diff` tool (see
//! [`crate::evidence`]), so the git arm of [`capture`] returns an empty [`Capture`].
//!
//! This module is the seam. [`capture`] is what the rest of the server calls; the Perforce backend
//! renders its own [`CapturedChange`]. The VCS-neutral primitives it shares -- truncation, capped
//! reads, fenced rendering, path sanitisation, omission bookkeeping -- live in [`shared`].

pub mod baseline;
pub mod capture_summary;
pub mod disposition;
pub mod perforce;
mod shared;

use std::sync::atomic::AtomicBool;

use crate::config::{Config, Vcs};
pub use capture_summary::CaptureSummary;
pub use disposition::Disposition;
pub use shared::{Capture, CapturedChange};
// `mod shared` is private, so these crate-visible items are not otherwise nameable from outside
// `vcs` (e.g. `config::max_wait_secs`, `reviewer::codex`). Re-exported here rather than widening
// the module, so the reachable surface stays explicit.
pub(crate) use shared::{read_capped, CAPTURE_BUDGET};

/// Capture the change under review, using whichever backend the configuration selected.
///
/// The dispatch is an exhaustive match on [`Vcs`] rather than a trait object: there are two
/// backends, both "shell out to a local CLI on Windows and parse text", and neither is
/// extended from outside. A new backend has to state its arm here rather than opt itself in.
///
/// `changes` and `include_shelved` are the Perforce backend's per-call inputs -- the
/// changelist numbers to capture, and whether to pull shelved content. The git backend is
/// driven entirely by `cfg` and ignores both.
pub fn capture(
    cfg: &Config,
    changes: &[u64],
    include_shelved: bool,
    resume: Option<Resume<'_>>,
    cancel: &AtomicBool,
) -> Capture {
    match cfg.vcs {
        // Git reviews no longer pre-capture: the change is derived live through the evidence
        // service's `repository_diff` (retire-capture-modes). `should_capture_change` gates this
        // call off for git, so this arm is unreachable at runtime; it returns an empty capture
        // rather than panicking if that ever changes. `changes`/`include_shelved`/`resume` are the
        // Perforce backend's inputs and are unused here.
        Vcs::Git => Capture::empty(),
        Vcs::Perforce => {
            let pf = resume.map(|Resume::Perforce(p)| p);
            perforce::capture(cfg, changes, include_shelved, pf, cancel)
        }
    }
}

/// The prior turn's baseline for the Perforce backend, assembled by `tools.rs` from the session
/// record. Git no longer captures, so it has no resume shape here.
pub enum Resume<'a> {
    Perforce(perforce::PerforceResume<'a>),
}
