//! What the server captured and handed to the reviewer, surfaced to the *caller*.
//!
//! Every completed review response that supplied a change carries a one-line `captured:`
//! summary beside `usage:` and `disposition:`: the resolved command/range, a size summary, an
//! explicit diff-truncation token, and a completeness verdict. Without it a wrong-range, a
//! stale-`main`, and a truncated capture all read identically in the response, so a caller
//! cannot confirm the reviewer saw the intended change without re-running git or p4. See
//! `docs/capture-summary.md` for the design and the seven rounds of gate review behind it.
//!
//! Two rules govern this module, inherited from the disposition work it mirrors:
//!
//! - It reports what the server *sent*, never what the reviewer received or still holds.
//! - A size figure is not a completeness claim. The `diff:` token reports one precise fact --
//!   whether the combined diff hit the byte budget -- and cannot contradict the separate
//!   `complete`/`partial` verdict. A count is marked a floor ("at least") only when *its own*
//!   evidence was shortened, so a shortfall in one stream never falsely qualifies another's
//!   count.

use std::fmt::Write as _;

use super::shared::MAX_DIFF_BYTES;

/// The combined-diff byte cap, in kB, for the `diff:` token. Derived from the shared constant so
/// the rendered figure cannot drift from the cap that is actually enforced.
fn cap_kb() -> usize {
    MAX_DIFF_BYTES / 1000
}

/// A summary of the change one turn captured and sent, per backend. Constructed only when a
/// change was actually sent (`capture.change.is_some()`); a turn that sent nothing carries none.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CaptureSummary {
    Git {
        /// A safe, bounded range descriptor -- resolved hex endpoints for a pinned range, a fixed
        /// string for working-tree/staged, or the sanitised configured spelling otherwise. Never
        /// the raw command string.
        range: String,
        /// Shown counts: the files/lines the reviewer was actually given. A floor when the diff
        /// stream was shortened (`diff_incomplete`), never an overstatement.
        files: usize,
        insertions: usize,
        deletions: usize,
        /// New files whose content was included (git-untracked); not in the `+/-` counts.
        untracked_files: usize,
        /// `untracked_files` may understate the true number of new files: the enumeration failed,
        /// short-streamed, was cut at its cap, or the content budget dropped whole files. Its own
        /// typed floor, independent of the diff figures.
        untracked_files_floor: bool,
        /// The combined diff hit the byte cap. Drives the `diff:` token -- a precise fact that
        /// cannot contradict the verdict.
        diff_truncated: bool,
        /// The diff was shortened at all -- byte cap, short pipe, or lossy decode. Drives the
        /// `+/-`/file count floor. `diff_incomplete >= diff_truncated`.
        diff_incomplete: bool,
        /// Capture-level wholeness (streams, caps, enumeration, unrun commands, truncated included
        /// bodies) -- not the per-file exclusion of un-includable files.
        complete: bool,
    },
    Perforce {
        /// The changelists actually captured (not merely requested).
        changelists: Vec<u64>,
        /// Requested changelists that were skipped, so `changelists` is a subset.
        skipped: usize,
        /// Captured changelists whose evidence was incomplete (binary/out-of-root/lossy/etc.),
        /// distinct from skipped.
        incomplete_changelists: usize,
        /// The exact number of evidence units sent. A floor on the requested change whenever
        /// anything was skipped, incomplete, or truncated.
        evidence_units: usize,
        /// The combined diff hit the byte budget. Segment-level short/lossy `p4 diff` is folded
        /// into `incomplete_changelists`, not here.
        diff_truncated: bool,
        complete: bool,
    },
}

impl CaptureSummary {
    /// The informational `captured:` line body for the caller.
    pub fn summary(&self) -> String {
        match self {
            CaptureSummary::Git {
                range,
                files,
                insertions,
                deletions,
                untracked_files,
                untracked_files_floor,
                diff_truncated,
                diff_incomplete,
                complete,
            } => {
                let files_prefix = if *diff_incomplete { "at least " } else { "" };
                let untracked_prefix = if *untracked_files_floor {
                    "at least "
                } else {
                    ""
                };
                let diff_token = if *diff_truncated {
                    format!(
                        "diff truncated at the {} kB cap (counts are a floor)",
                        cap_kb()
                    )
                } else if *diff_incomplete {
                    "diff within budget (stream incomplete or lossy; counts are a floor)"
                        .to_string()
                } else {
                    "diff within budget".to_string()
                };
                format!(
                    "{range} — {files_prefix}{files} file{}, +{insertions}/-{deletions}, \
                     {untracked_prefix}{untracked_files} untracked — {diff_token} — {}",
                    plural(*files),
                    verdict(*complete, None),
                )
            }
            CaptureSummary::Perforce {
                changelists,
                skipped,
                incomplete_changelists,
                evidence_units,
                diff_truncated,
                complete,
            } => {
                let units_floor = *diff_truncated || *incomplete_changelists > 0 || *skipped > 0;
                let units_prefix = if units_floor { "at least " } else { "" };
                let diff_token = if *diff_truncated {
                    format!("diff truncated at the {} kB cap", cap_kb())
                } else {
                    "diff within budget".to_string()
                };
                // The partial reason names how many requested changelists did not arrive whole,
                // when that is a cause; diff truncation shows in the token above.
                let reason = {
                    let short = skipped + incomplete_changelists;
                    if short > 0 {
                        let total = changelists.len() + skipped;
                        Some(format!(
                            "{short} of {total} changelist(s) incomplete or skipped"
                        ))
                    } else {
                        None
                    }
                };
                format!(
                    "changelists {} — {units_prefix}{evidence_units} evidence unit{} — \
                     {diff_token} — {}",
                    join_changelists(changelists),
                    plural(*evidence_units),
                    verdict(*complete, reason),
                )
            }
        }
    }

    /// A compact, bounded kebab-ish tag for the metrics log: the *identity* of the capture (its
    /// resolved range / captured changelists) plus `+t` (diff hit the byte cap) and `+p` (capture
    /// partial). `+t` implies `+p`. Lets an after-the-fact audit see which range each turn
    /// reviewed, not only how many bytes it was.
    pub fn tag(&self) -> String {
        let (prefix, identity, diff_truncated, complete) = match self {
            CaptureSummary::Git {
                range,
                diff_truncated,
                complete,
                ..
            } => {
                // Strip the "git diff " prefix the range carries for the response, leaving the
                // endpoints/mode; the range is already hex or `safe_label`-bounded.
                let core = range.strip_prefix("git diff ").unwrap_or(range).to_string();
                ("git:", core, *diff_truncated, *complete)
            }
            CaptureSummary::Perforce {
                changelists,
                diff_truncated,
                complete,
                ..
            } => (
                "p4:",
                join_changelists(changelists),
                *diff_truncated,
                *complete,
            ),
        };
        // Bound the *identity* portion, then append the markers, so a long range or changelist set
        // can never truncate `+t`/`+p` away and make the log under-report an incomplete capture.
        let markers = format!(
            "{}{}",
            if diff_truncated { "+t" } else { "" },
            if complete { "" } else { "+p" }
        );
        let identity_budget = MAX_TAG_LEN.saturating_sub(prefix.len() + markers.len());
        format!("{prefix}{}{markers}", bound(&identity, identity_budget))
    }
}

/// `complete`, or `partial (…; see warnings below)`. The reason is inline when there is a
/// count-shaped one; the warnings printed just below carry the detail either way.
fn verdict(complete: bool, reason: Option<String>) -> String {
    if complete {
        return "complete".to_string();
    }
    match reason {
        Some(r) => format!("partial ({r}; see warnings below)"),
        None => "partial (see warnings below)".to_string(),
    }
}

/// The changelists, comma-separated. Bounded here so a huge changelist set cannot make the line
/// or the tag unbounded; the count already appears in the warnings when it is that large.
fn join_changelists(cls: &[u64]) -> String {
    let mut out = String::new();
    for (i, cl) in cls.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        if out.len() > MAX_CHANGELIST_LABEL {
            let _ = write!(out, "… (+{} more)", cls.len() - i);
            break;
        }
        let _ = write!(out, "{cl}");
    }
    out
}

/// Cut a string to at most `limit` bytes *including* the ellipsis, on a char boundary. The
/// range/changelist text may already be a bounded `safe_label`, but the assembled tag still needs
/// a hard ceiling for the log; counting the ellipsis inside `limit` keeps the final tag within
/// its stated bound rather than three bytes over it.
fn bound(s: &str, limit: usize) -> String {
    if s.len() <= limit {
        return s.to_string();
    }
    let ellipsis = '…'.len_utf8();
    let mut end = limit.saturating_sub(ellipsis);
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

const MAX_TAG_LEN: usize = 96;
const MAX_CHANGELIST_LABEL: usize = 60;

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(clippy::too_many_arguments)]
    fn git(
        range: &str,
        files: usize,
        ins: usize,
        del: usize,
        untracked: usize,
        untracked_floor: bool,
        diff_truncated: bool,
        diff_incomplete: bool,
        complete: bool,
    ) -> CaptureSummary {
        CaptureSummary::Git {
            range: range.to_string(),
            files,
            insertions: ins,
            deletions: del,
            untracked_files: untracked,
            untracked_files_floor: untracked_floor,
            diff_truncated,
            diff_incomplete,
            complete,
        }
    }

    #[test]
    fn git_complete_capture_reads_cleanly() {
        let s = git(
            "git diff a1b2c3d4e5f6..0f1e2d3c4b5a",
            12,
            487,
            89,
            0,
            false,
            false,
            false,
            true,
        );
        let out = s.summary();
        assert_eq!(
            out,
            "git diff a1b2c3d4e5f6..0f1e2d3c4b5a — 12 files, +487/-89, 0 untracked — \
             diff within budget — complete"
        );
    }

    #[test]
    fn one_file_is_singular() {
        let s = git("git diff HEAD", 1, 2, 0, 0, false, false, false, true);
        assert!(s.summary().contains("1 file,"), "{}", s.summary());
        assert!(!s.summary().contains("1 files"), "{}", s.summary());
    }

    #[test]
    fn a_byte_truncated_diff_floors_the_counts_and_names_the_cap() {
        let s = git(
            "git diff a..b",
            40,
            90000,
            1200,
            0,
            false,
            true,
            true,
            false,
        );
        let out = s.summary();
        assert!(out.contains("at least 40 files"), "{out}");
        assert!(
            out.contains("diff truncated at the 400 kB cap (counts are a floor)"),
            "{out}"
        );
        assert!(out.ends_with("partial (see warnings below)"), "{out}");
    }

    #[test]
    fn a_short_stream_floors_the_counts_without_claiming_truncation() {
        // diff_incomplete but not diff_truncated: the byte budget held, the stream did not.
        let s = git("git diff a..b", 12, 487, 89, 0, false, false, true, false);
        let out = s.summary();
        assert!(out.contains("at least 12 files"), "{out}");
        assert!(
            out.contains("diff within budget (stream incomplete or lossy; counts are a floor)"),
            "{out}"
        );
        assert!(!out.contains("truncated at"), "{out}");
    }

    #[test]
    fn the_untracked_floor_is_independent_of_the_diff_floor() {
        // Diff whole, but the enumeration was short: only the untracked count is a floor.
        let s = git("git diff HEAD", 12, 487, 89, 2, true, false, false, false);
        let out = s.summary();
        assert!(
            out.contains("12 files, +487/-89, at least 2 untracked"),
            "{out}"
        );
        assert!(out.contains("diff within budget"), "{out}");
        assert!(!out.contains("at least 12 files"), "{out}");
    }

    #[test]
    fn git_tag_carries_the_endpoints_and_markers() {
        let s = git(
            "git diff a1b2c3d4e5f6..0f1e2d3c4b5a",
            40,
            9,
            9,
            0,
            false,
            true,
            true,
            false,
        );
        assert_eq!(s.tag(), "git:a1b2c3d4e5f6..0f1e2d3c4b5a+t+p");

        let clean = git(
            "git diff a1b2c3d4e5f6..0f1e2d3c4b5a",
            1,
            1,
            1,
            0,
            false,
            false,
            false,
            true,
        );
        assert_eq!(clean.tag(), "git:a1b2c3d4e5f6..0f1e2d3c4b5a");
    }

    fn perforce(
        changelists: Vec<u64>,
        skipped: usize,
        incomplete: usize,
        units: usize,
        diff_truncated: bool,
        complete: bool,
    ) -> CaptureSummary {
        CaptureSummary::Perforce {
            changelists,
            skipped,
            incomplete_changelists: incomplete,
            evidence_units: units,
            diff_truncated,
            complete,
        }
    }

    #[test]
    fn perforce_complete_capture_reads_cleanly() {
        let s = perforce(vec![43650, 43651], 0, 0, 8, false, true);
        assert_eq!(
            s.summary(),
            "changelists 43650, 43651 — 8 evidence units — diff within budget — complete"
        );
    }

    #[test]
    fn an_incomplete_segment_floors_the_units_and_names_the_shortfall() {
        // skipped == 0, diff not truncated, but one captured changelist was incomplete.
        let s = perforce(vec![43650, 43651, 43652], 0, 1, 8, false, false);
        let out = s.summary();
        assert!(out.contains("at least 8 evidence units"), "{out}");
        assert!(out.contains("diff within budget"), "{out}");
        assert!(
            out.contains("1 of 3 changelist(s) incomplete or skipped"),
            "{out}"
        );
    }

    #[test]
    fn a_skipped_changelist_floors_the_units_even_when_every_captured_one_is_whole() {
        // The round-4 case: skipped > 0 with no incomplete captured segment.
        let s = perforce(vec![43650, 43651], 1, 0, 8, false, false);
        let out = s.summary();
        assert!(out.contains("at least 8 evidence units"), "{out}");
        // total = captured (2) + skipped (1) = 3; short = skipped (1).
        assert!(
            out.contains("1 of 3 changelist(s) incomplete or skipped"),
            "{out}"
        );
    }

    #[test]
    fn perforce_tag_carries_the_changelists_and_partial_marker() {
        let s = perforce(vec![43650, 43651], 1, 0, 8, false, false);
        assert_eq!(s.tag(), "p4:43650, 43651+p");
    }

    #[test]
    fn a_long_perforce_tag_is_bounded_but_keeps_its_markers() {
        // Many changelists and an incomplete capture: the identity is bounded, but the `+p`
        // marker must survive so the log does not under-report the shortfall.
        let many: Vec<u64> = (0..200).collect();
        let s = perforce(many, 3, 0, 1, false, false);
        let tag = s.tag();
        assert!(tag.len() <= MAX_TAG_LEN, "{tag}");
        assert!(tag.starts_with("p4:"), "{tag}");
        assert!(tag.ends_with("+p"), "{tag}");
    }

    #[test]
    fn a_long_git_tag_keeps_its_markers() {
        // A long (sanitised, non-hex) range plus a truncated, partial capture: `+t+p` must
        // survive the length bound rather than being cut off the end.
        let long_range = format!("git diff {}", "a".repeat(200));
        let s = git(&long_range, 1, 1, 1, 0, false, true, true, false);
        let tag = s.tag();
        assert!(tag.len() <= MAX_TAG_LEN, "{tag}");
        assert!(tag.starts_with("git:"), "{tag}");
        assert!(tag.ends_with("+t+p"), "{tag}");
    }
}
