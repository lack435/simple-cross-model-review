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

A small, VCS-neutral enum, computed as a by-product of the existing capture decision:

- **`Full`** — a fresh review (no prior baseline), or the first turn of a session. Reported
  as nothing new on a fresh call; see [Only on resume](#only-on-resume).
- **`Incremental`** — the delta fired. Carries backend detail:
  - git: the pinned range `<prior>..<HEAD>` and the count of new commits.
  - Perforce: files re-sent vs files collapsed (`M of K files changed since your last
    review`), from the counts `baseline.rs` already computes.
- **`FellBackToFull { reason }`** — a resume that *could* have deltaed but did not, with a
  specific, enumerated reason:
  - `Disabled` — `--no-incremental-resume`.
  - `ModeNotHeadRange` — the mode is not a HEAD-anchored committed range (working-tree,
    staged, or a fixed window like `HEAD~3..HEAD~1`), so it never deltas by design.
  - `BranchRewritten` — the prior HEAD is no longer an ancestor of HEAD (rebase, amend,
    force-push).
  - `BaseMoved` — the configured base no longer resolves to the baseline's recorded base
    (`main` advanced, or `--diff` was repointed).
  - `PriorNotResumable` — the prior turn's capture was truncated / unresolved / did not
    persist a baseline, so there is nothing safe to delta from.
  - Perforce-specific: `IdentityChanged`, `ModeOrShelvedChanged`, `PriorTurnUnpersisted`
    (the durable-marker cases already handled in `tools.rs`).

Making it an enum — not just a prose sentence — is the point: it is machine-checkable by the
calling agent, assertable in unit tests, and greppable in the smoke test. The prose the
caller reads is *rendered from* the enum, single-sourced the way `incomplete()` centralises
capture-shortfall phrasing and `command_line()` derives from `diff_args()`.

### Where it comes from

`incremental_base` in `src/vcs/git.rs` currently returns `Option<String>` — the prior commit
or nothing — and *throws away the reason* for the `None`. Change it to return the disposition
(the prior commit on `Some`, the specific `reason` on fall-back). The capture already knows
every input to that decision at the point it makes it, so no new git calls are added; the
reason is information already computed and currently discarded.

Thread the disposition out through `Capture` / `CapturedChange` (which already carry
`head_sha` / `base_sha`) to `tools.rs`, the same path the baseline pair already travels.

### How the caller sees it

In `tools.rs`, on a **resumed** turn, render the disposition into the caller-facing response.
Two sub-decisions, both for the reviewer to weigh in on:

- **Channel.** `warnings` is framed as "the captured change was incomplete" — a shortfall.
  A clean `Incremental` or `Full` turn is *not* a shortfall, and dressing it as a warning
  would train callers to ignore the channel. Recommend a **distinct caller-facing line**
  (an informational `disposition:` note in the response, beside `usage:`), reserving
  `warnings` for the case that genuinely deserves attention: **`FellBackToFull` on a resume
  where the caller was configured for a delta** is a cost surprise worth a warning, because
  it means the thing they set up is not happening.
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
- **It must not claim more than it verified.** The `Incremental` note says what the server
  *sent*; it must not assert the reviewer still holds the earlier context, which the server
  cannot know. The wording stays on the server's side of that line.

## Blast radius

Additive to the response and to one enum return type. Touched files:
`src/vcs/git.rs` (disposition + reason on the git decision), `src/vcs/mod.rs` and
`src/vcs/shared.rs` (carry it on `Capture` / `CapturedChange`), `src/tools.rs` (surface +
record on resume), `src/metrics.rs` (record field), plus tests. Per the task framing,
foundational completeness is preferred over minimising this; the change is cohesive and the
surface it adds is small.

## Testing

- **Unit, git:** each fall-back reason maps to its disposition — `BranchRewritten` (prior
  HEAD not an ancestor), `BaseMoved` (base ref advanced), `ModeNotHeadRange` (working-tree
  and non-HEAD range), `Disabled` (`--no-incremental-resume`), `PriorNotResumable` (prior
  capture truncated). These extend the existing fall-back tests, which today assert only that
  the *range* fell back, by additionally asserting the *reason* surfaced.
- **Unit, git round trip:** a two-turn temp repo asserts turn 2's disposition is
  `Incremental { prior..HEAD, 1 commit }` and that the caller-facing line contains it.
- **Unit, tools:** a resumed turn emits the disposition line; a fresh turn does not; a
  resume that fell back emits the warning form.
- **`smoke.ps1`:** on a resumed run, assert the disposition line appears in the response
  (real model call — run only when the change touches this path, and note the cost).

## Open questions for the reviewer

1. **Channel split.** Is the info-line-plus-warning-on-fallback split right, or should every
   resume disposition go through one channel? The concern driving the split is not diluting
   `warnings`.
2. **Perforce depth now vs later.** Implement full git/Perforce parity (per-file counts for
   Perforce) in this change, or ship git fully plus a coarse `{Full, Incremental,
   FellBackToFull+reason}` for Perforce and leave the counts as a follow-up? `baseline.rs`
   already has the counts, so parity is feasible now; the question is scope discipline.
3. **Should `FellBackToFull { BaseMoved }` be more than a warning?** A base ref that moves
   every turn defeats the whole feature permanently. Is a one-time warning enough, or should
   the response actively suggest pinning the base (the `--diff <resolved-base>..HEAD`
   remedy)?
