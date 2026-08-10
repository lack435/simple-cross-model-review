# Capture summary, surfaced to the caller — design

Status: **proposed.** This document is the plan. It is intended to go through this repository's
own `cross-review` gate (Codex, gpt-5.6-luna, effort=max) before any code is written, the same
way [`incremental-resume-disposition.md`](incremental-resume-disposition.md) did. Filed against
issue [#46](https://github.com/lack435/simple-cross-model-review/issues/46) ("Surface a capture
summary to the caller in every review response").

## Problem: the caller cannot see what change the reviewer was given

When the server captures a change and hands it to the reviewer, the tool response that comes
back from `cross_model_review_result` describes the review but not the *evidence*. It carries
the review text, a `usage:` line, and — on a resumed turn — a `disposition:` line. What it does
**not** carry is any statement of what was actually captured and sent.

The reviewer's *prompt* echoes the diff command (`### git diff …`), but the prompt is not in the
response. So from the response alone a caller cannot distinguish:

- a correct capture of the intended range;
- a capture of the **wrong range** (a `--diff` pointed at the wrong base, or a stacked branch
  whose pinned `main...HEAD` swept in the PR underneath);
- a **stale-`main`** capture — the "1707 insertions instead of 208" case
  [`AGENTS.md`](../AGENTS.md) documents, where a stale local `main` silently widens the range;
- a **truncated** diff, where the reviewer was shown only the first 400 KB.

All four produce a response that reads identically. This is exactly why `AGENTS.md` pushes so
much defensive bookkeeping onto the caller before every call — fetch `origin`, confirm local
`main` is current, check the tree is clean — *because the capture is otherwise invisible after
the fact.* [PR #44](https://github.com/lack435/simple-cross-model-review/pull/44) added a
`disposition:` line for the incremental-resume slice of this, but that fires only on a resumed
turn and describes only the resume delta. The general capture — what was sent on **every** turn,
fresh or resumed — is still opaque.

## What "surfaced" has to mean here

This repository already treats "the reviewer was given partial or unexpected evidence" as a
first-class safety property. Truncation is stated in the prompt and warned to the caller; skipped
changelists are warned; a dirty tree is flagged; the resume disposition is surfaced. The capture
summary is the one rung below all of them that is still missing: the *baseline* statement of what
the reviewer saw, present whenever a change was sent, that every one of those other signals
qualifies.

The design rule, inherited from the disposition work: **report what the server *sent*, never what
the reviewer received or still holds.** The summary describes the capture the server produced and
handed over. It says nothing about whether the reviewer read all of it.

## Proposal: a typed `CaptureSummary`, computed by each backend, rendered once

Add a `captured:` line to every completed review response that supplied a change, beside `usage:`
and `disposition:`:

```
captured: git diff <base>..<head> — 12 files, +487/-89, not truncated
captured: changelists 43650, 43651 — 8 evidence units, not truncated
```

The value is carried as a typed `CaptureSummary`, not a preformatted string, so it is testable,
each backend owns its own variant, and the metrics log can record a compact tag from the same
value — exactly the shape `Disposition` already has.

### When a summary exists at all

**A summary is emitted exactly when the server sent a change** — that is, when `capture.change`
is `Some`. This is one gate, and it is simpler than the disposition's two:

- There is **no resume gate.** Unlike a disposition (which is meaningful only relative to a prior
  turn), the capture summary describes the change on *its own terms*, so it is present on a fresh
  turn 1 exactly as on a resume.
- A turn that **sent no change** carries no summary. That covers `--diff none`, `--diff auto` with
  a shell-equipped reviewer (`supplies_change()` is false, so `capture.change` is `None` and the
  reviewer fetches its own diff), and a capture that **failed or was cancelled** (`change` is
  `None`; the caller is told through the existing fail-closed *warning*, not through a summary
  line that would have nothing honest to say).

Because the gate is precisely `change.is_some()`, the natural home for the value is on
`CapturedChange` itself — the struct that exists if and only if a change was captured. It is
**not** `Option` at that layer: a `CapturedChange` always has a summary. The `Option` appears one
layer up, where a whole `Capture` may or may not carry a `change`.

This also settles the failure interaction cleanly. A **failed reviewer attempt** renders as an
error (`review_result` returns `Err(failure)` for `Status::Failed`), not through
`render_completed`, so no `captured:` line is shown there regardless — consistent with the
acceptance criterion, which is scoped to a *completed* review. See
[Failed reviews](#failed-reviews-and-the-metrics-log) for what the log does instead.

### The `CaptureSummary` value

A VCS-neutral enum in a new `src/vcs/capture_summary.rs`, parallel to `disposition.rs`. Each
backend constructs its own variant from data it already has in hand; the enum owns the rendering.

```rust
pub enum CaptureSummary {
    Git {
        /// The exact command the reviewer was shown, so the line cannot drift from the prompt.
        command: String,
        files: usize,
        insertions: usize,
        deletions: usize,
        /// New files carried alongside the diff (git-untracked), which a diff cannot cover.
        untracked_files: usize,
        truncated: bool,
    },
    Perforce {
        /// The changelists actually captured (not merely requested).
        changelists: Vec<u64>,
        /// Requested changelists that were skipped, so the count is honestly a subset.
        skipped: usize,
        evidence_units: usize,
        truncated: bool,
    },
}
```

`CaptureSummary::summary()` renders the body of the `captured:` line; `CaptureSummary::tag()`
renders a compact kebab-case tag for the metrics log (e.g. `git:12f+487-89`,
`perforce:2cl/8u`), matching how `Disposition::tag()` is used.

### Git: what the numbers are, and where they come from

Everything the git variant needs is already in the backend's `Change` (`src/vcs/git.rs`) at the
moment `git_capture` (`src/vcs/mod.rs`) adapts it into a `CapturedChange`:

- **`command`** is `change.command` — the resolved, **pinned** command string the reviewer was
  actually shown. For a HEAD-anchored range this is the two-dot `git diff <base>..<head>` with
  concrete commit ids substituted for symbolic refs (`main...HEAD` is captured as
  `<merge-base>..<head_sha>`; see `src/vcs/git.rs` around the `effective` mode). Reusing the same
  string the prompt heading uses is deliberate: it is a single source of truth, so the summary
  **cannot drift** from what the reviewer saw, and the resolved base commit is precisely what lets
  a caller catch a stale-`main` capture without re-running git.
- **`files` / `insertions` / `deletions`** are a cheap parse of `change.diff.text` (the unified
  diff, already captured and already the post-truncation text):
  - `files` = count of `diff --git ` header lines. A rename-only, mode-only, or binary file still
    emits that header, so it is counted with zero line changes — which is correct.
  - `insertions` = lines beginning with `+` **excluding** the `+++ ` file header;
    `deletions` = lines beginning with `-` **excluding** the `--- ` file header.
  - The parse is VCS-neutral in mechanics but git-specific in what the counts *mean*, so it lives
    in the git backend, exposed as a small `diff_line_counts(&str) -> (usize, usize, usize)`
    helper with its own unit tests.
- **`untracked_files`** is `change.untracked.len()` — new files git has never seen, carried
  alongside the diff and therefore *not* in the `+/-` counts. Reported separately so the line is
  honest about what the `+/-` figures do and do not include.
- **`truncated`** is `change.diff.truncated`, the same flag `diff_bytes` is recorded next to
  today. When it is set, the reviewer was shown only the first `MAX_DIFF_BYTES` of the diff, so
  the parsed counts are a **floor** — the line says so rather than presenting them as exact
  (see [Truncation honesty](#truncation-honesty)).

### Perforce: what the numbers are, and where they come from

Everything the Perforce variant needs is in `perforce::capture` (`src/vcs/perforce.rs`) at the
point the `CapturedChange` is built:

- **`changelists`** is the `captured` vector — the changelists that actually produced a segment,
  not the requested set. A changelist requested but skipped is not in it.
- **`skipped`** is `skipped.len()`. The individual skip reasons are already rendered into the
  prompt and surfaced as a warning; the summary only needs the count, so the `changelists`
  figure is understood as a subset when `skipped > 0`.
- **`evidence_units`** is the count of units across all captured segments
  (`segments.iter().flat_map(|s| &s.units).count()`), matching the vocabulary the Perforce
  disposition already uses ("evidence unit"). On a resumed turn where elision collapsed some
  units, the *count of units* is unchanged — a collapsed unit is still a unit — and the
  resent/collapsed split is the disposition's job, not the summary's. The summary reports the
  total shape of what was captured; the disposition reports how much of it was re-sent.
- **`truncated`** is `budget.diff_truncated`, the same flag the Perforce path already records
  next to `diff_bytes`.

### Truncation honesty

Truncation already produces a caller-facing **warning** in both backends (git:
`src/vcs/git.rs`; Perforce: `src/vcs/perforce.rs`). The `captured:` line does not replace that —
it restates truncation compactly in the one-liner, and, crucially, marks the size figures as a
lower bound when it is set. A truncated git diff undercounts files and lines because the text was
cut at 400 KB; presenting `+487/-89` as if exact on a truncated capture would be the same species
of silent shortfall this codebase refuses everywhere else. So the rendered forms are:

```
… — 12 files, +487/-89, not truncated
… — at least 40 files, +90000/-1200, truncated (diff cut at the 400 KB cap; counts are a floor)
```

The `not truncated` / `truncated` token is always present — its absence would itself be
ambiguous.

## How the caller sees it

`render_completed` in `src/tools.rs` gains one block, placed **after `usage:` and before
`disposition:`** — cost first, then what was sent (the general statement), then the resume delta
(the refinement of what was sent):

```
usage:     …
captured:  git diff <base>..<head> — 12 files, +487/-89, not truncated
disposition: incremental — only the delta since your last turn (<a>..<b>) was sent, 2 new commits
```

On a fresh turn there is no `disposition:` line but there **is** a `captured:` line — which is the
whole point of the general summary. The rendering reads the value off the snapshot; nothing in
`render_completed` knows how the counts were computed.

The line answers the acceptance criteria directly: it states the capture command/range, a size
summary, and whether it was truncated, and a caller can confirm the reviewer saw the intended
change from the response alone — the resolved range and `+/-` counts together catch the
stale-`main` case (the base commit is wrong *and* the numbers are inflated) without the caller
re-running git or p4.

## Plumbing: the path from capture to response

The value follows the exact path `disposition` already travels, so the change is mechanical and
the reviewer can check it against a known-good precedent:

1. **`CapturedChange`** (`src/vcs/shared.rs`) gains a non-optional `summary: CaptureSummary`
   field. Both construction sites fill it: `git_capture` (`src/vcs/mod.rs`) from the git `Change`,
   and `perforce::capture` (`src/vcs/perforce.rs`) directly.
2. **`tools.rs`** takes `capture.change.as_ref().map(|c| &c.summary)` where it already takes the
   disposition, producing an `Option<CaptureSummary>` (None iff no change was sent).
3. **`Outcome`**, **`Review`**, and **`Snapshot`** (`src/registry.rs`) each gain
   `capture_summary: Option<CaptureSummary>`, threaded through `Registry::finish` and
   `Snapshot::of` exactly as `disposition` is. `Outcome::failed` sets it to `None`.
4. **`render_completed`** (`src/tools.rs`) renders the line from `snapshot.capture_summary`.

### Failed reviews and the metrics log

Two deliberate choices at the edges:

- The value is attached to the **successful** outcome only, mirroring `disposition` (`Outcome::failed`
  carries `None`). A failed review never reaches `render_completed`, so this changes no output; it
  keeps the two response-facing signals consistent rather than special-casing one.
- The **metrics log** (`src/metrics.rs` `Record`) already stores `diff_bytes` and `diff_truncated`
  for every turn including failed ones, so the *size* and *truncation* are already auditable
  after the fact. What the log lacks — and what this adds, for symmetry with the `disposition`
  tag it sits beside — is the resolved **command/range**, recorded as an optional
  `captured: Option<String>` tag from `CaptureSummary::tag()`. This closes the same audit gap for
  the general capture that the disposition tag closed for the resume: an after-the-fact reader can
  see *which range* each turn actually reviewed, not only how many bytes it was. The field is
  `#[serde(default, skip_serializing_if = "Option::is_none")]`, so older records and no-change
  turns stay clean, consistent with the existing `disposition` field.

## What this must not do

- **It must not run any new VCS subprocess.** Every figure is a parse of what was already
  captured or a count of a vector already in memory. No extra `git diff --numstat`, no extra `p4`
  call — that would spend the capture budget and reintroduce a timeout surface for a cosmetic
  line. (This mirrors the incremental commit-count nicety, which runs only on leftover budget; the
  summary does not even do that — it needs no subprocess at all.)
- **It must not change what is captured or rendered into the prompt.** The summary is a read of
  the capture, computed after it. The golden prompt snapshot in `src/vcs/mod.rs` must be
  byte-for-byte unchanged, because the reviewer's prompt does not gain the `captured:` line —
  only the tool response does.
- **It must not overclaim on a truncated or partial capture.** Truncation makes the counts a
  floor and the line says so; skipped changelists make `changelists` a subset and the count says
  so. The existing warnings still fire on their own terms — the summary adds to them, removes
  nothing.
- **It must not restate the reviewer's confidence.** It reports the *evidence sent*, full stop. A
  reviewer that ignored half the diff is a separate concern the denials/warnings channels already
  cover.

## Blast radius

New: `src/vcs/capture_summary.rs` (the enum, `summary()`, `tag()`, unit tests). Touched:
`src/vcs/shared.rs` (field on `CapturedChange`), `src/vcs/mod.rs` (git construction + the
`diff_line_counts` helper site), `src/vcs/git.rs` (the `diff_line_counts` helper),
`src/vcs/perforce.rs` (Perforce construction), `src/registry.rs` (field on `Outcome`/`Review`/
`Snapshot`, threaded through `finish`/`of`), `src/tools.rs` (take the value, render the line),
`src/metrics.rs` (the optional `captured` tag field). Every touched site already has a
`disposition` line of code one line away, so the diff is small and local despite the file count.

No security boundary moves: the summary reads capture data that already crosses the
backend→server seam; it introduces no new file read, no new subprocess, and no new
attacker-influenced string beyond the diff command (which is already rendered into the prompt and
sanitised there). Path text is not interpolated — the summary carries counts and a command that
`command_line()` already built from a fixed argument list.

## Testing

- **`capture_summary.rs` unit tests**: `summary()` for git (exact counts; the truncated-floor
  wording; zero untracked omitted vs present) and Perforce (evidence-unit count; skipped subset);
  `tag()` stability for both. Singular/plural where the disposition doc's precedent applies
  ("1 file" vs "12 files").
- **`diff_line_counts` unit tests** (git): a plain multi-file diff; a rename-only entry (file
  counted, zero lines); a binary entry (file counted, zero lines); `+++`/`--- ` headers excluded
  from the line counts; an empty diff (0/0/0).
- **`git_capture` / `perforce::capture` construction tests**: the summary reflects the pinned
  command and the untracked/skipped/truncation state of a fixture capture.
- **`render_completed` tests** (`src/tools.rs`): the `captured:` line appears on a fresh completed
  turn (where no `disposition:` line does); it sits after `usage:` and before `disposition:`; it
  is absent when the snapshot carried no change (a no-diff review).
- **Golden prompt test** (`src/vcs/mod.rs`): unchanged, asserted to still pass — proof the prompt
  did not move.
- **Metrics test** (`src/metrics.rs`): a record round-trips the `captured` tag; a no-change record
  omits it.

Unit tests only; no new `smoke.ps1` surface is required, because nothing in the protocol,
spawning, or session handling changes — this is a read of already-captured data rendered into an
existing response.

## Open questions for the reviewer

1. **Command form for the git line.** The plan reuses `change.command`, which is the fully
   resolved, pinned form (`git diff <base_sha>..<head_sha> --no-ext-diff … -- .`). That is exact
   and drift-proof but verbose, and it shows resolved SHAs rather than the configured `main...HEAD`
   spelling. Is the resolved form the right call for catching stale-`main` (the base SHA is the
   tell), or should the line show the *configured* spelling alongside it for readability at the
   cost of a second source of truth?
2. **Untracked files in the git file count.** The plan reports `untracked_files` as a separate
   figure rather than folding it into `files`, because the `+/-` counts cannot include untracked
   content. Is a separate figure the honest presentation, or is a combined "N files" (with the
   `+/-` caveat) clearer to a caller?
3. **Evidence-unit count on an elided Perforce turn.** The plan reports total units (collapsed
   included) and leaves the resent/collapsed split to the disposition. Is that the right division
   of labour, or should the `captured:` line itself note how many units were collapsed?
4. **The metrics `captured` tag.** Adding it is for audit symmetry with the `disposition` tag, but
   it is strictly beyond issue #46's response-only ask. Is it in scope for this change, or should
   it be split into its own follow-up so the response feature lands minimal?
