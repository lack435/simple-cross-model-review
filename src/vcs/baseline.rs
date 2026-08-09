//! The persisted state of a Perforce resume delta.
//!
//! These types are the interface between the three parts of the feature: the Perforce backend
//! *produces* a [`PerforceBaseline`] each turn, the session store *persists* it on the
//! `SessionRecord`, and the next turn *consults* it to decide what to collapse. They are pure
//! serde data with no logic, so the session store does not depend on the backend's internals
//! and the backend does not depend on the store's file format.
//!
//! The design and every invariant these types encode is in `docs/perforce-resume-delta.md`.

use serde::{Deserialize, Serialize};

use crate::digest::Fingerprint;

/// Schema version for the persisted inventory and the capture identity.
///
/// Bumped whenever the canonical fingerprint input, the digest algorithm, or the `p4`
/// invocation that produces the evidence changes, so a baseline written by an incompatible
/// build is invalidated on read rather than compared across two formats. A `Full` baseline
/// whose `schema` does not match this constant is treated as absent.
pub const INVENTORY_SCHEMA: u32 = 1;

/// Which segment of a changelist an evidence unit belongs to. The same depot path has distinct
/// evidence in each, so the basis is part of a unit's identity.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Basis {
    /// A pending changelist's workspace diff.
    Workspace,
    /// A pending changelist's shelved snapshot.
    Shelved,
    /// A submitted changelist's server-side diff.
    Submitted,
}

/// What kind of evidence a unit carries. Part of a unit's identity so a path that changes kind
/// between turns (an edit reverted then re-added) registers as a mismatch, not a key hit.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum UnitKind {
    /// A textual `p4 diff` section.
    TextDiff,
    /// A pending added file's body, read from the workspace.
    AddBody,
    /// A one-line note about non-elidable evidence (binary, delete, out-of-root, ...). Kept in
    /// the inventory so the next turn can tell "still present, still non-elidable" from
    /// "removed", but never carries a fingerprint.
    Note,
}

/// One unit in the prior turn's inventory: its identity, and -- when it was a fully-shown
/// elidable unit -- the fingerprint the next turn compares against.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct InventoryEntry {
    pub change: u64,
    pub basis: Basis,
    pub kind: UnitKind,
    pub depot: String,
    /// The comparator the evidence was taken against: a depot revision, the `NoDepotBase`
    /// sentinel for a pending add, or an empty string for a note. Folded into the fingerprint
    /// input too, so a base-revision change alone breaks a match.
    pub comparator: String,
    /// Present only for a fully-shown elidable unit (a complete `TextDiff` or `AddBody`).
    /// `None` means the unit was present but non-elidable -- binary, delete, note, or shown
    /// only partially (budget-cut, truncated output, or lossy-decoded) -- so it is always
    /// re-shown, never collapsed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<Fingerprint>,
}

/// The comparator sentinel for a pending added file, which has no depot base revision. A real
/// comparator, not a "missing" one, so an unchanged add body still elides.
pub const NO_DEPOT_BASE: &str = "\u{0}add";

/// The delta baseline a Perforce session carries into its next turn.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "lowercase")]
pub enum PerforceBaseline {
    /// The previous persisted turn captured every changelist completely and suppressed no
    /// metadata: elision is permitted against `entries`. `schema` invalidates the baseline on a
    /// format change.
    Full {
        schema: u32,
        entries: Vec<InventoryEntry>,
    },
    /// The previous persisted turn was incomplete in some way (a truncation, a skipped or
    /// cancelled changelist, an over-cap read, a suppressed note): the next turn full-captures.
    Disabled,
}

impl PerforceBaseline {
    /// The inventory to elide against, or `None` when elision is not permitted -- because the
    /// prior turn was `Disabled`, or its `schema` no longer matches this build.
    pub fn usable_inventory(&self) -> Option<&[InventoryEntry]> {
        match self {
            PerforceBaseline::Full { schema, entries } if *schema == INVENTORY_SCHEMA => {
                Some(entries)
            }
            _ => None,
        }
    }
}

/// The workspace/capture identity a Perforce session is bound to.
///
/// A resume whose resolved identity differs from the one recorded -- a changed server, client,
/// charset, or client spec (view, root, `AltRoots`, options) -- re-captures in full rather than
/// eliding against a mapping the reviewer's earlier diff was never taken under. The client spec
/// is carried as a digest of its canonical `p4 client -o` text rather than field-by-field, so
/// any change to the view or roots moves it without this type having to model the spec.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct CaptureIdentity {
    /// `P4PORT` / server address.
    pub server: String,
    pub client: String,
    pub charset: String,
    /// SHA-256 (hex) of the canonical client spec, or `None` when it could not be captured --
    /// in which case the identity is incomplete and elision is disabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_spec_digest: Option<String>,
}

impl CaptureIdentity {
    /// Whether two identities may share an elision baseline: equal in every field, and both
    /// carry a client-spec digest (an absent digest means the spec could not be confirmed, so
    /// the two are never treated as the same capture).
    pub fn matches(&self, other: &CaptureIdentity) -> bool {
        self == other && self.client_spec_digest.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_baseline_is_usable_only_at_the_current_schema() {
        let entries = vec![InventoryEntry {
            change: 42,
            basis: Basis::Workspace,
            kind: UnitKind::TextDiff,
            depot: "//depot/a".into(),
            comparator: "3".into(),
            fingerprint: Fingerprint::of(b"x"),
        }];
        let current = PerforceBaseline::Full {
            schema: INVENTORY_SCHEMA,
            entries: entries.clone(),
        };
        assert!(current.usable_inventory().is_some());

        let stale = PerforceBaseline::Full {
            schema: INVENTORY_SCHEMA + 1,
            entries,
        };
        assert!(stale.usable_inventory().is_none());
        assert!(PerforceBaseline::Disabled.usable_inventory().is_none());
    }

    #[test]
    fn capture_identity_matches_only_with_a_confirmed_spec() {
        let base = CaptureIdentity {
            server: "ssl:perforce:1666".into(),
            client: "ws".into(),
            charset: "utf8".into(),
            client_spec_digest: Some("abc".into()),
        };
        assert!(base.matches(&base.clone()));

        // A different server (or client, charset, spec) never matches.
        let moved = CaptureIdentity {
            server: "ssl:other:1666".into(),
            ..base.clone()
        };
        assert!(!base.matches(&moved));

        // An absent spec digest never matches, even against itself: the spec was not confirmed.
        let unconfirmed = CaptureIdentity {
            client_spec_digest: None,
            ..base.clone()
        };
        assert!(!unconfirmed.matches(&unconfirmed.clone()));
    }

    #[test]
    fn baseline_round_trips_through_json() {
        let baseline = PerforceBaseline::Full {
            schema: INVENTORY_SCHEMA,
            entries: vec![
                InventoryEntry {
                    change: 7,
                    basis: Basis::Submitted,
                    kind: UnitKind::TextDiff,
                    depot: "//depot/x".into(),
                    comparator: "12".into(),
                    fingerprint: Fingerprint::of(b"diff"),
                },
                InventoryEntry {
                    change: 7,
                    basis: Basis::Workspace,
                    kind: UnitKind::Note,
                    depot: "//depot/bin".into(),
                    comparator: String::new(),
                    fingerprint: None,
                },
            ],
        };
        let json = serde_json::to_string(&baseline).expect("serialize");
        let back: PerforceBaseline = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(baseline, back);
    }
}
