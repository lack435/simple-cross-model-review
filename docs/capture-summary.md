# Capture summary, surfaced to the caller — design

Status: **proposed.** This document is the plan. It is intended to go through this repository's
own `cross-review` gate (Codex, gpt-5.6-luna, effort=max) before any code is written, the same
way [`incremental-resume-disposition.md`](incremental-resume-disposition.md) did. Filed against
issue [#46](https://github.com/lack435/simple-cross-model-review/issues/46) ("Surface a capture
summary to the caller in every review response").

**Revision note (round 1 → 2).** The first draft conflated "the diff was not truncated" with "the
capture is complete", proposed a metrics tag that could not tell two ranges apart, and reused the
raw resolved command string without sanitising it. All three are fixed below and the four
open questions are now decided; the changes are called out inline as *(r1)*.

## Problem: the caller cannot see what change the reviewer was given

When the server captures a change and hands it to the reviewer, the tool response that comes back
from `cross_model_review_result` describes the review but not the *evidence*. It carries the
review text, a `usage:` line, and — on a resumed turn — a `disposition:` line. What it does
**not** carry is any statement of what was actually captured and sent.

The reviewer's *prompt* echoes the diff command (`### git diff …`), but the prompt is not in the
response. So from the response alone a caller cannot distinguish:

- a correct capture of the intended range;
- a capture of the **wrong range** (a `--diff` pointed at the wrong base, or a stacked branch
  whose pinned `main...HEAD` swept in the PR underneath);
- a **stale-`main`** capture — the "1707 insertions instead of 208" case
  [`AGENTS.md`](../AGENTS.md) documents, where a stale local `main` silently widens the range;
- a **truncated** diff, where the reviewer was shown only the first 400 KB.

All four produce a response that reads identically. This is exactly why `AGENTS.md` pushes so much
defensive bookkeeping onto the caller before every call — fetch `origin`, confirm local `main` is
current, check the tree is clean — *because the capture is otherwise invisible after the fact.*
[PR #44](https://github.com/lack435/simple-cross-model-review/pull/44) added a `disposition:` line
for the incremental-resume slice of this, but that fires only on a resumed turn and describes only
the resume delta. The general capture — what was sent on **every** turn, fresh or resumed — is
still opaque.

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

A second rule the round-1 review forced to the surface: **a size figure is not a completeness
claim.** "Not truncated" means only that the diff budget was not hit; it does **not** mean every
file was shown. The summary therefore carries a *separate* completeness verdict (below), and its
truncation token is scoped explicitly to the diff.

## Proposal: a typed `CaptureSummary`, computed by each backend, rendered once

Add a `captured:` line to every completed review response that supplied a change, beside `usage:`
and `disposition:`:

```
captured: git diff <base>..<head> — 12 files, +487/-89, 0 untracked — complete
captured: changelists 43650, 43651 — 8 evidence units — complete
```

The value is carried as a typed `CaptureSummary`, not a preformatted string, so it is testable,
each backend owns its own variant, and the metrics log can record a compact tag from the same
value — exactly the shape `Disposition` already has.

### When a summary exists at all

**A summary is emitted exactly when the server sent a change** — that is, when `capture.change` is
`Some`. This is one gate, and it is simpler than the disposition's two:

- There is **no resume gate.** Unlike a disposition (which is meaningful only relative to a prior
  turn), the capture summary describes the change on *its own terms*, so it is present on a fresh
  turn 1 exactly as on a resume.
- A turn that **sent no change** carries no summary. That covers `--diff none`, `--diff auto` with
  a shell-equipped reviewer (`supplies_change()` is false, so `capture.change` is `None` and the
  reviewer fetches its own diff), and a capture that **failed or was cancelled** (`change` is
  `None`; the caller is told through the existing fail-closed *warning*, not through a summary line
  that would have nothing honest to say).

Because the gate is precisely `change.is_some()`, the natural home for the value is on
`CapturedChange` itself — the struct that exists if and only if a change was captured. It is
**not** `Option` at that layer: a `CapturedChange` always has a summary. The `Option` appears one
layer up, where a whole `Capture` may or may not carry a `change`.

### The `CaptureSummary` value

A VCS-neutral enum in a new `src/vcs/capture_summary.rs`, parallel to `disposition.rs`. Each
backend constructs its own variant from data it already has in hand; the enum owns the rendering.

```rust
pub enum CaptureSummary {
    Git {
        /// A safe, bounded range descriptor — resolved hex endpoints for a pinned range, a fixed
        /// string for working-tree/staged, or the sanitised configured spelling otherwise. Never
        /// the raw command string. See "The git range descriptor".
        range: String,
        files: usize,
        insertions: usize,
        deletions: usize,
        /// New files carried alongside the diff (git-untracked), which a diff cannot cover.
        untracked_files: usize,
        /// The diff specifically was cut at MAX_DIFF_BYTES, so the counts above are a floor.
        diff_truncated: bool,
        /// The whole capture is complete: nothing truncated, omitted, or unrun. See below.
        complete: bool,
    },
    Perforce {
        /// The changelists actually captured (not merely requested).
        changelists: Vec<u64>,
        /// Requested changelists that were skipped, so the count is honestly a subset.
        skipped: usize,
        evidence_units: usize,
        diff_truncated: bool,
        complete: bool,
    },
}
```

`CaptureSummary::summary()` renders the body of the `captured:` line; `CaptureSummary::tag()`
renders a compact, bounded kebab-case tag for the metrics log (see [Metrics](#the-metrics-tag)),
matching how `Disposition::tag()` is used.

### Completeness is a first-class field, distinct from truncation *(r1)*

The round-1 review's major finding: `diff_truncated` alone can report a partial capture as
complete, because both backends have completeness gaps that truncation does not cover.

- **Perforce.** A changelist stays in `captured` even when its `Segment.complete` is `false` — a
  binary or unreadable added file, an out-of-root or deleted file, a lossy or truncated
  `p4 describe` (`src/vcs/perforce.rs` around the per-segment `complete` computation). The existing
  caller warnings cover only *skipped* changelists and the diff budget, so a capture can be
  `skipped: 0`, `diff_truncated: false`, and still be missing evidence.
- **Git.** The `git status` listing can be truncated independently of the diff, `git status` can
  fail to run at all (`change.notes`), and untracked content can be omitted or its listing cut
  short (`OmissionReport::capture_level`) — none of which is `change.diff.truncated`.

So each variant carries an explicit `complete: bool`, computed **at the backend, from the same
gap facts that already drive the caller warnings** — not re-derived at the render seam (which
would risk drifting from the warning logic) and not inferred from "warnings is non-empty" (which
would wrongly count the Perforce *over-caps* warning, a statement about the *next* turn's resume,
not this capture's evidence):

- **Git** `complete` = `!diff.truncated && !status.truncated && notes.is_empty() &&
  omissions.capture_level.is_empty()`. Every one of those is in scope at the point
  `git::capture` builds the summary.
- **Perforce** `complete` = `skipped.is_empty() && !diff_truncated && segments.iter().all(|s|
  s.complete)`. This is deliberately *narrower* than the existing `capture_complete` local, which
  also requires `identity.client_spec_digest.is_some()`: that extra condition governs whether the
  capture may seed a resume *baseline*, which is a different question from whether the evidence the
  reviewer saw this turn was whole. A capture can be fully shown yet ineligible as a baseline.

`complete` is a strict superset of "not truncated": a `diff_truncated` capture is always
`complete: false`, but a capture can be `complete: false` with the diff intact.

### The git range descriptor *(r1)*

The round-1 review's third finding: `change.command` is built from the raw revision spelling
(`cfg.diff`) and would carry unsanitised, unbounded text into both the response and the log. The
summary therefore does **not** reuse `change.command`. It carries a purpose-built `range` string,
constructed in `git::capture` where the resolved endpoints and the effective mode are both in
scope:

- **A pinned HEAD-anchored range** (the common case: `--diff main...HEAD`, `--diff <base>..HEAD`,
  and every incremental delta) renders from the resolved commit ids as
  `git diff <base12>..<head12>`, abbreviated to 12 hex chars each. This is drift-proof, and the
  resolved base commit is exactly the tell that catches a stale-`main` capture — the review
  endorsed the resolved-SHA form for pinned ranges over the configured `main...HEAD` spelling.
- **Working-tree / staged modes** render as the fixed strings `git diff HEAD` / `git diff
  --cached` — no user-influenced text at all.
- **Any other mode** (a fixed window like `HEAD~3..HEAD~1`, or a range whose endpoints did not
  resolve) retains the *configured* spelling, but passed through `shared::safe_label` (control-char
  filtered, backtick-stripped, length-bounded) before it is stored. This is the one path where
  operator-supplied text reaches the descriptor, and it is sanitised at construction so neither
  the response nor the log ever holds a raw spelling.

Hex ids are inherently safe; `safe_label` bounds the fallback. The result is that the `range`
field is always safe to interpolate into Markdown and into the JSON log, which the plan can then
rely on rather than asserting the input was harmless.

### Git: what the numbers are, and where they come from

Constructed in `git::capture` (`src/vcs/git.rs`), which has every input in scope at the point it
builds the `Change`:

- **`range`** as above.
- **`files` / `insertions` / `deletions`** are a cheap parse of the captured diff text
  (`change.diff.text`, already the post-truncation string), via a small
  `diff_line_counts(&str) -> (usize, usize, usize)` helper with its own unit tests:
  - `files` = count of `diff --git ` header lines. A rename-only, mode-only, or binary file still
    emits that header, so it is counted with zero line changes — which is correct.
  - `insertions` = lines beginning with `+` **excluding** the `+++ ` file header;
    `deletions` = lines beginning with `-` **excluding** the `--- ` file header.
- **`untracked_files`** is `change.untracked.len()` — new files git has never seen, carried
  alongside the diff and therefore *not* in the `+/-` counts. Reported separately (the review
  endorsed keeping it separate) so the line is honest about what the `+/-` figures include.
- **`diff_truncated`** is `change.diff.truncated`; **`complete`** as defined above.

The parse mechanics are string-level, but what the counts *mean* is git's, so the helper lives in
the git backend.

### Perforce: what the numbers are, and where they come from

Constructed in `perforce::capture` (`src/vcs/perforce.rs`) at the point the `CapturedChange` is
built:

- **`changelists`** is the `captured` vector — the changelists that actually produced a segment,
  not the requested set.
- **`skipped`** is `skipped.len()`. The individual skip reasons are already rendered into the
  prompt and surfaced as a warning; the summary needs only the count, and the `changelists` figure
  is understood as a subset when `skipped > 0`.
- **`evidence_units`** is the count of units across all captured segments
  (`segments.iter().flat_map(|s| &s.units).count()`), matching the "evidence unit" vocabulary the
  Perforce disposition already uses. On a resumed turn where elision collapsed some units, the
  *count of units* is unchanged — a collapsed unit is still a unit — and the resent/collapsed split
  is the disposition's job (the review endorsed this division of labour, given `complete` now
  surfaces incompleteness).
- **`diff_truncated`** is `budget.diff_truncated`; **`complete`** as defined above.

### How the size and completeness read together

`render_completed` renders the size, then the completeness verdict, and marks the counts a floor
only when the diff specifically was truncated:

```
… — 12 files, +487/-89, 0 untracked — complete
… — 12 files, +487/-89, 3 untracked — partial (some evidence was omitted; see warnings above)
… — at least 40 files, +90000/-1200, 0 untracked — partial (diff cut at the 400 KB cap; counts are a floor; see warnings above)
… (perforce) — 8 evidence units — partial (1 of 3 changelists incomplete or skipped; see warnings above)
```

The verdict is always one of `complete` / `partial`, so its absence is never ambiguous. When
`partial`, the line points to the WARNING lines already printed in the same response rather than
duplicating their text — the warnings carry the per-gap detail, the summary carries the verdict.
When `diff_truncated`, the `+/-` (and file) figures are prefixed "at least" and flagged as a
floor, because a diff cut at 400 KB undercounts them.

## How the caller sees it

`render_completed` in `src/tools.rs` gains one block, placed **after `usage:` and before
`disposition:`** — cost first, then what was sent (the general statement), then the resume delta
(the refinement of what was sent):

```
usage:     …
captured:  git diff <base>..<head> — 12 files, +487/-89, 0 untracked — complete
disposition: incremental — only the delta since your last turn (<a>..<b>) was sent, 2 new commits
```

On a fresh turn there is no `disposition:` line but there **is** a `captured:` line — which is the
whole point of the general summary. The rendering reads the value off the snapshot; nothing in
`render_completed` knows how the counts were computed.

The line answers the acceptance criteria directly: it states the capture command/range, a size
summary, and whether it was truncated (and, beyond the ticket, whether it was otherwise complete),
and a caller can confirm the reviewer saw the intended change from the response alone — the
resolved range and `+/-` counts together catch the stale-`main` case (the base commit is wrong
*and* the numbers are inflated) without the caller re-running git or p4.

## Plumbing: the path from capture to response

The value follows the exact path `disposition` already travels, so the change is mechanical and
the reviewer can check it against a known-good precedent:

1. **The backends** construct the `CaptureSummary`: `git::capture` (`src/vcs/git.rs`) builds the
   `Git` variant and carries it on its internal `Change`; `perforce::capture`
   (`src/vcs/perforce.rs`) builds the `Perforce` variant directly. *(r1: git construction moves
   into `git::capture` rather than the adapter, so the resolved endpoints and mode are in scope
   for the safe `range`.)*
2. **`CapturedChange`** (`src/vcs/shared.rs`) gains a non-optional `summary: CaptureSummary` field.
   `git_capture` (`src/vcs/mod.rs`) moves the git `Change`'s summary across; Perforce sets it
   inline.
3. **`tools.rs`** takes `capture.change.as_ref().map(|c| &c.summary)`, producing an
   `Option<CaptureSummary>` (None iff no change was sent).
4. **`Outcome`**, **`Review`**, and **`Snapshot`** (`src/registry.rs`) each gain
   `capture_summary: Option<CaptureSummary>`, threaded through `Registry::finish` and
   `Snapshot::of` exactly as `disposition` is. `Outcome::failed` sets it to `None`.
5. **`render_completed`** (`src/tools.rs`) renders the line from `snapshot.capture_summary`.

### The metrics tag *(r1)*

Decision on open question 4: **the metrics field stays in scope**, because a capture summary that
is legible live but invisible in the audit log would reproduce, for the general capture, exactly
the gap this project closed for the resume disposition. But the round-1 review was right that a
counts-only tag (`git:12f+487-89`) defeats the field's purpose — it cannot tell two ranges apart
or reveal a stale base. So the tag carries the **identity of the capture**, not its size:

- Git: the resolved endpoints plus a truncation/partial marker, e.g. `git:<base12>..<head12>` with
  a `+t` (diff-truncated) / `+p` (partial) suffix.
- Perforce: the captured changelist numbers plus the same markers, e.g. `p4:43650,43651+p`.

Both are built from the already-safe `range` / `changelists` fields, and the whole tag is
length-bounded before it is written. It is recorded on `Record` (`src/metrics.rs`) as
`captured: Option<String>`, `#[serde(default, skip_serializing_if = "Option::is_none")]`, so older
records and no-change turns stay clean — consistent with the existing `disposition` field.

Following the `disposition_tag` precedent (`src/tools.rs`, where the tag is taken from the local
value *before* the reviewer attempt), the `captured` tag is likewise extracted from the local
`CaptureSummary` before the attempt, so a **failed** reviewer attempt — which still captured and
sent a change — is logged with what it sent. This is the one place the log and the response
diverge on purpose: the *response* line rides the successful outcome (a failed review renders as
an error via `Status::Failed`, never through `render_completed`), while the *log tag* is taken
early so failure is still audited. Both are drawn from the same `CaptureSummary`.

## What this must not do

- **It must not run any new VCS subprocess.** Every figure is a parse of what was already captured
  or a count of a vector already in memory. No extra `git diff --numstat`, no extra `p4` call —
  that would spend the capture budget and reintroduce a timeout surface for a cosmetic line.
- **It must not change what is captured or rendered into the prompt.** The summary is a read of the
  capture, computed after it. The golden prompt snapshot in `src/vcs/mod.rs` must be
  byte-for-byte unchanged, because the reviewer's prompt does not gain the `captured:` line — only
  the tool response does.
- **It must not overclaim on a truncated or partial capture.** `complete` is `false` on *any* gap,
  not just diff truncation; a truncated diff additionally makes the counts a floor and the line
  says so; skipped or incomplete changelists make `changelists` a subset and the verdict `partial`.
  The existing warnings still fire on their own terms — the summary adds to them, removes nothing.
- **It must not carry unsanitised operator text.** The git `range` is hex or `safe_label`-bounded
  at construction; the metrics tag is built from those safe fields and length-bounded. Neither the
  response nor the log ever holds the raw `--diff` spelling.
- **It must not restate the reviewer's confidence.** It reports the *evidence sent*, full stop. A
  reviewer that ignored half the diff is a separate concern the denials/warnings channels cover.

## Blast radius

New: `src/vcs/capture_summary.rs` (the enum, `summary()`, `tag()`, unit tests). Touched:
`src/vcs/git.rs` (build the `Git` variant + the `diff_line_counts` helper + the `range`
descriptor; new field on the internal `Change`), `src/vcs/mod.rs` (move the summary across in the
adapter), `src/vcs/shared.rs` (field on `CapturedChange`; the `range` fallback reuses the existing
`safe_label`), `src/vcs/perforce.rs` (build the `Perforce` variant), `src/registry.rs` (field on
`Outcome`/`Review`/`Snapshot`, threaded through `finish`/`of`), `src/tools.rs` (take the value,
extract the tag before the attempt, render the line), `src/metrics.rs` (the optional `captured`
tag field). Every touched site already has a `disposition` line of code one line away, so the diff
is small and local despite the file count.

No security boundary moves: the summary reads capture data that already crosses the
backend→server seam; it introduces no new file read, no new subprocess. The one string derived
from operator configuration — the git `range` — is sanitised and bounded at construction (hex
endpoints, or `safe_label` on the fallback spelling), so it is not a new injection surface.

## Testing

- **`capture_summary.rs` unit tests**: `summary()` for git (exact counts; the `complete` vs
  `partial` verdict; the diff-truncated floor wording *with* the "at least" prefix; zero vs
  present untracked) and Perforce (evidence-unit count; the `partial` verdict from a skipped *or*
  an incomplete-but-not-skipped changelist); `tag()` stability and bounding for both; singular vs
  plural ("1 file" vs "12 files"), following the disposition doc's precedent.
- **`diff_line_counts` unit tests** (git): a plain multi-file diff; a rename-only entry (file
  counted, zero lines); a binary entry (file counted, zero lines); `+++`/`--- ` headers excluded
  from the line counts; an empty diff (0/0/0).
- **`range` descriptor tests** (git): a pinned range renders resolved hex endpoints; working-tree
  and staged render the fixed strings; a non-resolving / fixed-window mode renders the *sanitised*
  configured spelling (assert a control char or backtick in the spelling does not survive).
- **Completeness construction tests** — the gap the round-1 review named:
  - Perforce: a capture with `skipped == 0`, `diff_truncated == false`, but a segment with
    `complete == false` yields `complete == false` and renders `partial`.
  - Git: a capture whose `status` truncated, or whose untracked listing was cut short, or whose
    `git status` did not run, yields `complete == false` while the diff itself is intact.
- **`render_completed` tests** (`src/tools.rs`): the `captured:` line appears on a fresh completed
  turn (where no `disposition:` line does); it sits after `usage:` and before `disposition:`; it
  is absent when the snapshot carried no change (a no-diff review).
- **Golden prompt test** (`src/vcs/mod.rs`): unchanged, asserted to still pass — proof the prompt
  did not move.
- **Metrics tests** (`src/metrics.rs`): a record round-trips the `captured` tag; a no-change record
  omits it; a **failed** turn that captured a change still records the tag (proving the
  extract-before-attempt path).

Unit tests only; no new `smoke.ps1` surface is required, because nothing in the protocol,
spawning, or session handling changes — this is a read of already-captured data rendered into an
existing response.

## Decisions (formerly open questions)

Resolved with the round-1 reviewer; recorded here so the implementation does not reopen them.

1. **Command form for the git line — resolved SHAs.** The `range` shows resolved hex endpoints for
   a pinned range (the base SHA is the tell for a stale-`main` capture), fixed strings for
   working-tree/staged, and the *sanitised* configured spelling only for non-resolving modes. The
   configured `main...HEAD` spelling is **not** shown alongside; a second source of truth would be
   a drift risk for no gain over the resolved base commit.
2. **Untracked files — separate figure.** Kept out of the `files` count, because the `+/-` figures
   cannot include untracked content; a combined count would misdescribe what the numbers mean.
3. **Evidence-unit count on an elided Perforce turn — total units.** The `captured:` line reports
   total units (collapsed included); the resent/collapsed split stays the disposition's job, now
   that `complete` surfaces any incompleteness independently.
4. **Metrics tag — in scope, carrying identity not size.** Kept for audit symmetry with the
   disposition tag, but redesigned to carry the resolved range / changelist identity plus
   truncation/partial markers, and extracted before the reviewer attempt so failed turns are
   logged. See [The metrics tag](#the-metrics-tag).
