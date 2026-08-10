# Incremental-resume disposition, surfaced to the caller — design

Status: **proposal only — no implementation yet.** Intended as the artifact for a
cross-model review to work out the details before any code is written. Filed against
issue #41 ("Incremental re-review: send the reviewer only the delta since its last turn"),
but deliberately *not* a re-implementation of it — see [What #41 already got](#what-41-already-got).

## What #41 already got

Issue #41 asked for the reviewer to be sent only the incremental range on a resumed
review. **That already exists for both backends** and is not what this proposal builds:

- **Git** (PR #36, commit `1a19437`): a resumed turn whose `--diff` is a HEAD-anchored
  committed range captures only `<prior_head>..HEAD` — the commits added since the last
  turn — instead of the whole range. Guarded by an ancestry check (a rewritten branch
  falls back to the full range), a base-identity check (a moved base ref falls back), a
  truncation guard (a truncated capture never becomes a baseline), and mode (working-tree
  and staged diffs never delta). ~15 tests in `src/vcs/git.rs` cover the round trip and
  every fallback.
- **Perforce** (PR #38): per-file elision keyed on a fingerprint of what the reviewer was
  last shown, fail-closed on every uncertain signal, with a durable in-progress marker so a
  crash cannot collapse against a stale baseline. See
  [`perforce-resume-delta.md`](perforce-resume-delta.md).

So the *mechanism* is built and robust. What is missing is one rung below it, and it is a
gap this repository treats as a first-class safety property everywhere else.

## Problem: the delta is invisible to the calling agent

When a resumed turn narrows to a delta, **the reviewer is told** — the prompt says "this is
a follow-up review, and the diff below is only what changed since your previous turn." But
**the calling agent is told nothing.** The tool response that comes back from
`cross_model_review_result` carries no signal distinguishing:

- a genuine incremental turn (`<prior>..HEAD`, a couple of new commits), from
- a turn that *silently fell back to a full re-capture* of the whole range, from
- a delta that fired when it arguably should not have.

Nothing in the response tells the two apart. That is precisely the failure mode `AGENTS.md`
already calls out for the stale-`main` case: a review handed 1707 insertions instead of 208,
where "nothing in the response distinguishes that from a large PR, so the check is yours to
make." The incremental delta is the same shape of invisibility, and it cuts both ways:

1. **A silent permanent fall-back reintroduces the exact cost problem #36 set out to kill.**
   If the configured base ref keeps moving (a long-lived branch whose `main` advances between
   every re-review), or `--diff` is repointed, or `--no-incremental-resume` is set somewhere
   the caller forgot, every resumed turn re-captures and re-bills the full range. The delta
   the caller is paying the session-tracking machinery to get is quietly not happening, and
   the response looks identical to one where it is. The usage numbers climb turn over turn —
   the same signal #36 was written to remove — with no attribution.

2. **A delta that fired against a reviewer whose context was compacted under-reviews
   silently.** The correctness of every incremental turn rests on one assumption the server
   states but cannot verify: "that conversation is still in your context." When it is not —
   the reviewer CLI summarised or dropped the earlier full diff — the reviewer reviews a
   fragment as though it were the whole change. The server cannot detect that. But it *can*
   make the disposition legible, so a caller debugging a suspiciously thin review has
   something to look at instead of silence.

The repository's whole posture is that the caller must be able to tell what the reviewer was
actually shown — it is why capture `warnings` reach the caller at all, and why the diff
command line is echoed into the prompt verbatim. The resume disposition is the one thing the
caller currently cannot see, and it is the thing that most directly governs cost and coverage
across an iterating branch.

## Proposal: a VCS-neutral resume disposition, computed once and surfaced

Every capture already *decides* its disposition — that decision is what selects the range.
The proposal is to name that decision, carry it out to the caller, and record it, without
changing which range is captured.

### The disposition value

A small, VCS-neutral enum. The first review of this design conflated two very different
things under "not a delta" — an intentional non-delta and a *failed* delta — and the
distinction is the crux of getting the warning channel right (see
[How the caller sees it](#how-the-caller-sees-it)). So the enum has four states, not three:

- **`Full`** — a fresh review: turn 1, or a resumed turn with no prior baseline to be
  incremental against. Reported as nothing on a fresh call; see
  [Only on resume](#only-on-resume).
- **`FullByDesign { reason }`** — the configuration *never intended* a delta this turn, so
  its absence is correct and unremarkable, **not** a fall-back and **never** a warning:
  - `Disabled` — `--no-incremental-resume`.
  - `ModeNotDeltable` — the mode is not a HEAD-anchored committed range: a working-tree
    (`auto`/`HEAD`) or staged diff (which carry uncommitted work a commit delta would drop),
    or a fixed window like `HEAD~3..HEAD~1` (which does not move with HEAD). These are the
    common, correct cases — warning on a resumed `--diff HEAD` review would be a false alarm
    that trains callers to ignore the channel.
- **`Incremental`** — the delta fired. Carries the range `<prior>..<HEAD>` (git, free — the
  endpoints are already pinned) and optional backend detail; see
  [What the detail can honestly say](#what-the-detail-can-honestly-say).
- **`FellBackToFull { reason }`** — an *eligible* delta (a HEAD-anchored range, feature on)
  that failed a safety guard and re-captured the whole range. This is the state worth a
  warning, because it is where the delta the caller configured for stopped happening:
  - `BranchRewritten` — the prior HEAD is a valid commit that is **not** an ancestor of HEAD
    (rebase, amend, force-push). Distinct from the next one — see
    [Reasons the code must be refactored to tell apart](#reasons-the-code-must-be-refactored-to-tell-apart).
  - `BaseMoved` — the configured base still resolves, but to a different commit than the
    baseline's recorded base (`main` advanced, or `--diff` was repointed).
  - `NoCompleteBaselineRetained` — the session holds no complete `(head, base)` pair to
    delta from (the prior turn truncated *and* no earlier complete baseline survived, or HEAD
    could not be resolved). **Not** simply "the prior capture was truncated" — see finding 3
    below.
  - `AncestryUndecidable` — the ancestry check could not be *run* (git error/timeout), as
    opposed to answering "no". Conservatively a full re-capture, but reported as its own
    reason rather than mislabelled `BranchRewritten`.
  - Perforce-specific: `IdentityChanged`, `ModeOrShelvedChanged`, `PriorTurnUnpersisted`
    (the durable-marker cases already decided in `tools.rs`).

Making it an enum — not a prose sentence — is the point: machine-checkable by the calling
agent, assertable in unit tests, greppable in the smoke test. The prose the caller reads is
*rendered from* the enum, single-sourced the way `incomplete()` centralises capture-shortfall
phrasing and `command_line()` derives from `diff_args()`.

### Where it comes from, and what has to change to get it right

The first draft claimed the reason is "information already computed and currently discarded."
That is only half true, and the reviewer was right to push on it. The decision *inputs* all
exist at the point the capture decides, but the current functions **collapse distinct
outcomes into a single `None`/`false`**, so the reason is not recoverable without refactoring
them to preserve it. Concretely, three call sites lose information today:

#### Reasons the code must be refactored to tell apart

- **`effective_base` (`src/vcs/git.rs:643-663`) returns `None` for both "the mode is not a
  HEAD-anchored range" and "the base ref would not resolve."** The first is `FullByDesign
  { ModeNotDeltable }`; the second, on a resume that had a baseline, is a genuine fall-back.
  Split the return so the caller can tell an ineligible mode from a resolution failure.
- **`is_ancestor` (`src/vcs/git.rs:1012-1018`) returns `false` for both "not an ancestor" and
  "git errored / timed out."** Labelling every `false` as `BranchRewritten` would be a
  factual claim the code cannot back. Return a three-way answer (yes / no / undecidable) so
  the no-case is `BranchRewritten` and the error-case is `AncestryUndecidable`.
- **The Perforce pending-marker cases are turned into `resume = None` in `tools.rs:805-833`
  *before* `capture` runs,** so the backend never sees why. The reason has to be captured at
  that pre-capture decision point and carried into the disposition explicitly, not
  reconstructed inside the backend.

None of these add a git/p4 call for the *decision* (the ancestry and base-resolve commands
already run); they change what the existing calls' outcomes are allowed to say. The one place
a new query would be needed is an optional commit *count* — see below.

Then thread the disposition out through `Capture` (which already carries `head_sha` /
`base_sha` — note that `CapturedChange` does **not**; the first draft misplaced this) to
`tools.rs`.

### What the detail can honestly say

- **git range:** `<prior>..<HEAD>` is free — both endpoints are already pinned by the capture.
- **git commit count:** *not* free. It needs a `git rev-list --count <prior>..<HEAD>`, a
  bounded extra query. Treat the count as optional: report the range unconditionally, and the
  count only if we decide the extra call is worth it (open question 4). The first draft
  asserted "no new git calls" while also promising a count; that was contradictory.
- **Perforce:** `baseline.rs` stores *evidence entries*, not file counts, and the fingerprint
  is over **rendered evidence**, not arbitrary file state (`src/vcs/perforce.rs:503-510`). So
  the honest phrasing is "N of M evidence units re-sent, the rest collapsed as byte-identical
  to what you were last shown," with the counts computed *during* elision (they are not stored
  today) — never "M of K files changed," which claims a file-level comparison Perforce does
  not perform.

### How the caller sees it

In `tools.rs`, on a **resumed** turn, render the disposition into the caller-facing response.
Two sub-decisions, both for the reviewer to weigh in on:

- **Channel — a typed field, not a warning string.** `warnings` is framed as "the captured
  change was incomplete" — a shortfall — and it is a `Vec<String>`. A clean `Incremental`,
  `Full`, or `FullByDesign` turn is not a shortfall, and encoding structured data as prose in
  the warning list is the wrong shape twice over. So carry a **typed disposition field**
  through the result path — `Outcome` → `Review` → `Snapshot` in `src/registry.rs`
  (`:143-155`, `:558-599`), which the first draft omitted from the plumbing entirely — and
  render it into the response as its own informational `disposition:` line beside `usage:`.
  Only one state also earns a `warnings` entry: **`FellBackToFull` on a resume**, the cost
  surprise where the delta the caller configured for silently stopped happening.
  `FullByDesign` never warns.
- **Recording.** Record the disposition in the usage/metrics record next to `prompt_bytes`,
  so an after-the-fact audit of a session can see which turns deltaed and which re-billed
  the full range — the attribution point 1 above is missing today.

### Only on resume

A fresh review (turn 1, `resume_id` is `None`) says nothing new — there is no prior turn to
be incremental against, and emitting "full re-capture" there would be noise on every first
call. The disposition line appears only when `resume_id.is_some()`.

## What this must not do

- **It must not change which range is captured.** This is purely observational: the
  disposition is derived from the decision the capture already made, never a second decision
  that could disagree with it. The byte-for-byte reviewer-prompt golden test in
  `src/vcs/mod.rs` stays untouched, because the disposition is caller-facing only and is not
  added to the reviewer's prompt (the reviewer already has its own "follow-up review"
  framing). If a future revision *does* add a line to the reviewer prompt, that golden is
  regenerated deliberately, never blindly.
- **It must not introduce a new failure mode.** A disposition that cannot be computed (some
  unexpected `None`) degrades to reporting nothing, never to blocking or mis-capturing the
  review.
- **It must not claim more than it verified.** The new caller-facing `Incremental` line says
  what the server *sent* — the range, and (for Perforce) which evidence was collapsed as
  byte-identical to what was last shown. It must not assert the reviewer still holds the
  earlier context, which the server cannot know.

  One honesty note the first review drew out: the **existing reviewer-facing prompt already**
  asserts "that conversation is still in your context" (`src/vcs/git.rs:748-759`). That is a
  pre-existing assumption of the resume model, not something this change introduces or can
  close, and it is deliberately **out of scope** here — this proposal is about making the
  disposition legible to the *caller*, not about verifying the *reviewer's* memory. Calling
  the caller-facing boundary "complete" was overstated: it is complete for the new line only.
  Whether the reviewer prompt should soften that assertion is a separate question, noted here
  so it is not silently inherited.

## Blast radius

Additive to the response and to one enum return type — but wider than the first draft
admitted. Touched files:

- `src/vcs/git.rs` — the disposition and the reason-preserving refactors of `effective_base`
  (ineligible-mode vs unresolved-base) and `is_ancestor` (three-way), plus the optional
  `rev-list --count`.
- `src/vcs/perforce.rs` — compute the re-sent/collapsed evidence-unit counts during elision
  (not stored today).
- `src/vcs/shared.rs` / `src/vcs/mod.rs` — carry the typed disposition on `Capture` (**not**
  `CapturedChange`, which does not hold the baseline).
- `src/tools.rs` — capture the Perforce pending-marker reason at its pre-capture decision
  point (`:805-833`) rather than losing it to `resume = None`, and surface + record the
  disposition on a resumed turn.
- `src/registry.rs` — thread the typed field through `Outcome` → `Review` → `Snapshot`
  (`:143-155`, `:558-599`); this is the plumbing the first draft missed.
- `src/metrics.rs` — the record field, plus tests throughout.

Per the task framing, foundational completeness is preferred over minimising this. The change
is cohesive, but it is not a one-liner: getting the *reasons* accurate (findings 2 and 3) is
most of the work, and doing it wrong — a mislabelled `BranchRewritten`, a false `FellBackToFull`
warning on an intentional `--diff HEAD` — would be worse than reporting nothing.

## Testing

- **Unit, git — every reason maps correctly, including the ones the code must now tell
  apart:**
  - `BranchRewritten` — prior HEAD valid but not an ancestor.
  - `AncestryUndecidable` — the ancestry check could not run (distinct from the above; test
    by pointing it at a prior commit git cannot place).
  - `BaseMoved` — base ref advanced to a different commit.
  - `FullByDesign { ModeNotDeltable }` — a working-tree (`--diff HEAD`) or non-HEAD range
    resume, asserting it does **not** produce a warning.
  - `FullByDesign { Disabled }` — `--no-incremental-resume`.
  - **`NoCompleteBaselineRetained`, both ways (finding 3):** (a) first-turn truncation, where
    no baseline ever existed, and (b) truncation *after* an established baseline, which must
    still delta from the retained older complete `(head, base)` pair
    (`src/session.rs:197-214`, `src/tools.rs:821-825`) rather than fall back — proving the
    reason is "no complete baseline retained," not "the last capture truncated."

  These extend the existing fall-back tests, which today assert only that the *range* fell
  back, by additionally asserting the *reason*.
- **Unit, git round trip:** a two-turn temp repo asserts turn 2's disposition is
  `Incremental` over `prior..HEAD` and that the caller-facing line contains the range.
- **Unit, tools:** a resumed turn emits the disposition line; a fresh turn does not; a
  `FellBackToFull` resume also emits the warning; a `FullByDesign` resume emits the info line
  but **no** warning.
- **`smoke.ps1`:** on a resumed run, assert the disposition line appears in the response
  (real model call — run only when the change touches this path, and note the cost).

## Open questions for the reviewer

1. **Channel split.** Is the typed-field-plus-warning-only-on-`FellBackToFull` split right, or
   should every resume disposition go through one channel? The concern driving the split is
   not diluting `warnings` with the common, unremarkable `FullByDesign` and `Incremental`
   turns.
2. **Perforce depth now vs later.** Ship git fully plus the coarse `{Full, FullByDesign,
   Incremental, FellBackToFull+reason}` states for Perforce now, and leave the evidence-unit
   counts as a follow-up? The counts are **not** stored today (`baseline.rs` holds evidence
   entries, not tallies); computing re-sent-vs-collapsed during elision is a small addition
   but a real one, so it is a legitimate scope-discipline call rather than free parity.
3. **Should `FellBackToFull { BaseMoved }` be more than a warning?** A base ref that moves
   every turn defeats the whole feature permanently. Is a one-time warning enough, or should
   the response actively suggest pinning the base (the `--diff <resolved-base>..HEAD`
   remedy)?
4. **The git commit count — worth an extra `git rev-list --count`?** The range `prior..HEAD`
   is free and probably sufficient. The count is a nicety that costs one more bounded git call
   per resumed turn. Include it, or ship the range alone?
