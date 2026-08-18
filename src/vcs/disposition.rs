//! Whether a resumed turn reviewed only the delta, surfaced to the *caller*.
//!
//! The incremental-resume delta (git PR #36, Perforce PR #38) narrows what the reviewer is
//! shown on a re-review, but it does so silently as far as the *calling agent* is concerned:
//! the reviewer's prompt says when it saw only a delta, the tool response does not. This type
//! is the missing signal. It names, for one resumed turn, exactly one of three outcomes --
//! the delta fired, a delta was never intended, or an intended delta fell back to a full
//! re-capture -- so a delta that quietly stopped happening (a moved base, a repointed
//! `--diff`) or fired against lost context is legible instead of silent.
//!
//! See `docs/incremental-resume-disposition.md` for the design and the decision order. The
//! rule that governs this module: it reports what the server *sent*, never what the reviewer
//! received or still holds, which the server cannot know.

/// The resume disposition of one turn. Only ever constructed for a turn that both resumed and
/// sent a change; a fresh turn or a no-change turn carries `None` and renders nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Disposition {
    /// The delta fired: only the incremental change since the reviewer's last turn was sent.
    Incremental(Incremental),
    /// A delta was never intended this turn. Correct and unremarkable -- an info line, never a
    /// warning.
    FullByDesign(FullByDesign),
    /// An *eligible* delta failed a safety guard, so the whole change was re-captured. The one
    /// state that also earns a caller-facing warning, because it is where the delta the caller
    /// configured for stopped happening.
    FellBackToFull(FellBack),
}

/// What an incremental turn actually sent. Perforce-only: git no longer captures, so it has no
/// incremental-resume delta (retire-capture-modes).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Incremental {
    /// Perforce: how many evidence units were re-sent versus collapsed as byte-identical to the
    /// previous server-generated capture.
    PerforceEvidence { resent: usize, collapsed: usize },
}

/// Why a delta was never intended this turn. Never warns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FullByDesign {
    /// `--no-incremental-resume`. The G1 gate. (Git no longer captures, so this is Perforce-only.)
    Disabled,
}

/// Why an eligible delta fell back to a full re-capture. Warns on a resumed turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FellBack {
    // Git no longer captures, so it has no fall-back-to-full reasons (retire-capture-modes).
    // --- Perforce ---
    /// A resumed Perforce session whose `perforce_baseline` is `None` (the prior turn persisted
    /// no baseline field). Strictly absent -- a persisted `Disabled` is `PriorBaselineUnusable`.
    PriorBaselineMissing,
    /// The previous Perforce turn left an uncleared in-progress marker (confirmed present).
    PriorTurnPending,
    /// This Perforce turn could not write its in-progress marker; a later crash would be
    /// undetectable, so it refuses to elide. Also surfaces as a persistence warning when
    /// elision is enabled.
    MarkerUnwritable,
    /// The marker-state read errored; fail-closed, but distinct from a confirmed pending marker.
    MarkerStateUnreadable,
    /// The persisted binding cannot be compared: outer `identity` is `None`, `include_shelved`
    /// is `None`, or the persisted `identity` is `Some` with a `None` nested `client_spec_digest`.
    PriorBindingIncomplete,
    /// This turn's identity cannot be *established* (its `client_spec_digest` is `None`). Not a
    /// change -- the mirror of the persisted nested-`None` case, on the current side.
    IdentityUnconfirmed,
    /// Both identities are confirmed and differ.
    IdentityChanged,
    /// The shelved-capture flag differs.
    ModeOrShelvedChanged,
    /// The prior baseline is `Disabled` or an otherwise unusable inventory. Lowest precedence:
    /// selected only when none of the identity/mode/binding reasons applies.
    PriorBaselineUnusable,
}

impl Disposition {
    /// Whether this disposition earns a caller-facing *disposition* warning on a resumed turn.
    /// Only a `FellBackToFull` does: an eligible delta that stopped happening is a cost surprise.
    /// This is separate from the pre-existing capture/persistence warnings, which flow on their
    /// own terms.
    pub fn warns(&self) -> bool {
        matches!(self, Disposition::FellBackToFull(_))
    }

    /// A compact, stable kebab-case tag for the usage log, so an after-the-fact audit can see
    /// which turns deltaed and which re-billed the full range without parsing prose.
    pub fn tag(&self) -> String {
        match self {
            Disposition::Incremental(Incremental::PerforceEvidence { .. }) => {
                "incremental:perforce".into()
            }
            Disposition::FullByDesign(FullByDesign::Disabled) => "full-by-design:disabled".into(),
            Disposition::FellBackToFull(reason) => format!("fell-back:{}", reason.tag()),
        }
    }

    /// The informational `disposition:` line for the caller, describing what the server sent.
    pub fn summary(&self) -> String {
        match self {
            Disposition::Incremental(Incremental::PerforceEvidence { resent, collapsed }) => {
                format!(
                    "incremental -- {resent} evidence unit(s) re-sent, {collapsed} collapsed as \
                     byte-identical to the previous capture"
                )
            }
            Disposition::FullByDesign(reason) => {
                format!("full re-capture (by design: {})", reason.reason_str())
            }
            Disposition::FellBackToFull(reason) => {
                format!(
                    "full re-capture (an incremental delta was expected but fell back: {})",
                    reason.reason_str()
                )
            }
        }
    }
}

impl FullByDesign {
    fn reason_str(&self) -> &'static str {
        match self {
            FullByDesign::Disabled => "incremental resume disabled (--no-incremental-resume)",
        }
    }
}

impl FellBack {
    /// A compact kebab-case tag for the usage log; see [`Disposition::tag`].
    fn tag(&self) -> &'static str {
        match self {
            FellBack::PriorBaselineMissing => "prior-baseline-missing",
            FellBack::PriorTurnPending => "prior-turn-pending",
            FellBack::MarkerUnwritable => "marker-unwritable",
            FellBack::MarkerStateUnreadable => "marker-state-unreadable",
            FellBack::PriorBindingIncomplete => "prior-binding-incomplete",
            FellBack::IdentityUnconfirmed => "identity-unconfirmed",
            FellBack::IdentityChanged => "identity-changed",
            FellBack::ModeOrShelvedChanged => "mode-or-shelved-changed",
            FellBack::PriorBaselineUnusable => "prior-baseline-unusable",
        }
    }

    fn reason_str(&self) -> &'static str {
        match self {
            FellBack::PriorBaselineMissing => "no prior Perforce baseline was recorded",
            FellBack::PriorTurnPending => {
                "the previous turn did not finish persisting its baseline"
            }
            FellBack::MarkerUnwritable => "this turn could not record a durable in-progress marker",
            FellBack::MarkerStateUnreadable => "the in-progress marker state could not be read",
            FellBack::PriorBindingIncomplete => "the recorded resume binding was incomplete",
            FellBack::IdentityUnconfirmed => "the current capture identity could not be confirmed",
            FellBack::IdentityChanged => "the capture identity changed since the last turn",
            FellBack::ModeOrShelvedChanged => {
                "the shelved-capture mode changed since the last turn"
            }
            FellBack::PriorBaselineUnusable => "the prior baseline was not usable",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_fell_back_warns() {
        assert!(!Disposition::FullByDesign(FullByDesign::Disabled).warns());
        assert!(!Disposition::Incremental(Incremental::PerforceEvidence {
            resent: 1,
            collapsed: 2,
        })
        .warns());
        assert!(Disposition::FellBackToFull(FellBack::PriorTurnPending).warns());
        assert!(Disposition::FellBackToFull(FellBack::MarkerUnwritable).warns());
    }

    #[test]
    fn perforce_evidence_summary_names_the_counts() {
        let d = Disposition::Incremental(Incremental::PerforceEvidence {
            resent: 3,
            collapsed: 5,
        });
        let s = d.summary();
        assert!(s.contains("3 evidence unit(s) re-sent"), "{s}");
        assert!(s.contains("5 collapsed"), "{s}");
    }

    #[test]
    fn fell_back_summary_names_the_reason() {
        let s = Disposition::FellBackToFull(FellBack::PriorBaselineMissing).summary();
        assert!(s.contains("fell back"), "{s}");
        assert!(s.contains("no prior Perforce baseline"), "{s}");
    }
}
