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
  and staged diffs never delta). Around 15 incremental-resume tests in `src/vcs/git.rs` cover
  the round trip and the documented fallbacks (rewritten branch, moved base, non-HEAD range,
  truncated capture, disabled flag) — though *not*, today, an ancestry-check error/timeout,
  which is one gap this proposal's `AncestryUndecidable` reason forces a test for.
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

### When a disposition exists at all

Before the value, the precondition, because the second review found the first draft computing
a disposition in cases where the server sent nothing. **A disposition is emitted only when
both** (a) the turn is a resume (`resume_id.is_some()`) **and** (b) the server actually sent a
change (`capture.change.is_some()`). If either is false there is no line and no warning:

- A **fresh** turn (turn 1, or a name rebound to a new reviewer session) has no prior turn to
  be incremental against. Internally this is the `Full` state; it is never rendered.
- A turn where the server **sent no change** — `--diff none`, `--diff auto` with a
  shell-equipped reviewer (`supplies_change()` is false, `capture.change` is `None`), or a
  capture that **failed or was cancelled** — cannot honestly carry a disposition about a diff
  the reviewer was never handed. Saying "full re-capture" there would violate the
  say-only-what-was-sent boundary as surely as an overclaiming `Incremental` line would.

So the disposition is `Option`-typed and computed from `capture.change`, not alongside it.

### The disposition value: an ordered decision, not a flat list

The states below are **decided in order**; the first matching rule wins. Presenting them as a
flat list is what let the first two drafts leave a resumed-no-baseline turn described by two
states at once, and let a single `None` from `effective_base` stand in for five different
facts. The order *is* the precedence:

Given a resumed turn that sent a change, the git backend decides:

1. **`FullByDesign { Disabled }`** — `--no-incremental-resume` is set. Intentional; **never
   warns**.
2. **`FullByDesign { ModeNotDeltable }`** — the mode is not a HEAD-anchored committed range: a
   working-tree (`auto`/`HEAD`) or staged diff (which carry uncommitted work a commit delta
   would drop), or a fixed window like `HEAD~3..HEAD~1` (which does not move with HEAD).
   Intentional; **never warns**. Warning on a resumed `--diff HEAD` review would be the false
   alarm the round-1 review flagged.
3. **`FellBackToFull { NoCompleteBaselineRetained }`** — the mode *is* an eligible
   HEAD-anchored range, but the session carries **no complete `(head, base)` pair** to delta
   from. This is where a resumed turn with no usable baseline lands — resolving the ambiguity
   the round-2 review found: `Full` is reserved for *fresh* turns; a resumed eligible turn
   without a baseline is a fall-back, not a `Full`. It covers first-turn truncation followed
   by a resume (no baseline ever persisted). It does **not** cover truncation *after* an
   established baseline, because the session retains the prior complete pair
   (`src/session.rs:208`) and that later turn stays `Incremental`.
4. **`FellBackToFull { CurrentHeadUnavailable }`** — HEAD will not resolve now (unborn,
   detached, git failed), so there is no right endpoint to delta to. Distinct from #3: a
   complete prior baseline may still exist; what is missing is *this* turn's HEAD.
5. **`FellBackToFull { CurrentBaseUnresolvable }`** — the configured range's left ref no longer
   resolves, or a three-dot merge-base cannot be computed. The base is unknown, so `BaseMoved`
   cannot even be evaluated — this is why it must be a *separate* reason decided *before* it.
6. **`FellBackToFull { BaseMoved }`** — both the prior baseline's base and the current
   effective base are usable **and differ** (`main` advanced, or `--diff` was repointed). Only
   reachable once #5 has established the current base resolves.
7. **`FellBackToFull { PriorBaselineInvalid }`** — the current base matches, but the stored
   prior HEAD is not a usable object id (a corrupt or truncated record). Guards the ancestry
   check's input.
8. **`FellBackToFull { AncestryUndecidable }`** — the ancestry check could not be *run* (git
   error/timeout): a *three-way* result, not the `false` that today's `is_ancestor` collapses
   error into. Reported as its own reason rather than mislabelled `BranchRewritten`.
9. **`FellBackToFull { BranchRewritten }`** — the prior HEAD is a valid commit that is
   definitively **not** an ancestor of HEAD (rebase, amend, force-push).
10. **`Incremental`** — none of the above fired: the delta is `<prior>..<HEAD>`. Carries the
    range (git, free — both endpoints are already pinned) and optional backend detail; see
    [What the detail can honestly say](#what-the-detail-can-honestly-say).

Every `FellBackToFull { … }` reason warns (an eligible delta that stopped happening is a cost
surprise); every `FullByDesign { … }` reason does not; `Incremental` does not. That mapping is
now a property of the state, not a per-call judgement.

The **Perforce** backend substitutes its own durability/identity reasons for #3–#9, decided in
`tools.rs` before capture (see [finding 4](#perforce-durability-reasons)); #1, #2 and #10 apply
unchanged.

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

- **`effective_base` (`src/vcs/git.rs:643-663`) collapses *five* outcomes into one `None`:**
  a non-HEAD-anchored mode, an unresolvable left ref, an unavailable current HEAD (the
  three-dot path needs it — `:660`), a missing merge-base, and a merge-base *command* failure.
  Two drafts treated `None` as a single fact; it is not. Split the return into a status enum
  distinguishing at least `ModeNotDeltable`, `CurrentHeadUnavailable`, and
  `CurrentBaseUnresolvable` (the last folding "left ref won't resolve", "no merge base", and
  "merge-base failed" — all "the base is unknown"). Only once a base *is* known can
  `BaseMoved` be evaluated against the prior baseline; deciding `BaseMoved` on an unresolved
  base is the bug this split exists to prevent.
- **`is_ancestor` (`src/vcs/git.rs:1012-1018`) returns `false` for both "not an ancestor" and
  "git errored / timed out."** Labelling every `false` as `BranchRewritten` would be a factual
  claim the code cannot back. Return a three-way answer (yes / no / undecidable) so the
  no-case is `BranchRewritten` and the error-case is `AncestryUndecidable`.
- **The prior HEAD is validated before the ancestry check**, so a corrupt/truncated stored id
  is `PriorBaselineInvalid`, not fed to git.
- **Fresh-vs-resumed is invisible to `vcs::capture`, which receives only `Option<Resume>`
  (`src/vcs/mod.rs:34-38`).** `None` there means *both* "a fresh turn" and "a resumed turn
  whose session held no baseline" — the backend cannot tell them apart, and it must not, since
  the fresh case emits nothing while the resumed-no-baseline case is
  `FellBackToFull { NoCompleteBaselineRetained }`. So the **fresh-vs-resumed framing and the
  no-baseline reason are assigned in `tools.rs`**, which knows `resume_id` and whether a
  baseline was passed; the backend only reports the decisions that are its own (mode, base,
  ancestry).
- **The Perforce durability reasons are decided in `tools.rs:805-833` *before* `capture`
  runs** — see [below](#perforce-durability-reasons).

None of these add a git/p4 call for the *decision* (the ancestry and base-resolve commands
already run); they change what the existing calls' outcomes are allowed to say. The one place
a new query would be needed is an optional commit *count* — see below.

Then thread the disposition out through `Capture` (which already carries `head_sha` /
`base_sha` — note that `CapturedChange` does **not**; the first draft misplaced this) to
`tools.rs`, where the fresh/resumed layer above is applied.

#### Perforce durability reasons

The Perforce full-capture cases are already *decided* in `tools.rs` (`:805-833`, `:862`), but
the first draft's single `PriorTurnUnpersisted` conflated distinct ones. Separate them (or use
one `DurabilityGuard` reason carrying structured detail), with a defined precedence when more
than one holds:

- `PriorTurnPending` — the *previous* turn left an uncleared in-progress marker
  (`is_pending`): it crashed or failed to persist, so its baseline may be stale.
- `MarkerUnwritable` — *this* turn could not write its in-progress marker (`!pending_marked`),
  so a later crash would be undetectable and the turn refuses to elide (and records
  `Disabled`). A current durability failure, not evidence about the prior turn.
- `PriorBaselineUnusable` — the prior turn persisted `PerforceBaseline::Disabled` or an
  inventory that does not match this turn's identity/mode. Distinct from a missing marker.

Plus the identity/mode cases already named: `IdentityChanged`, `ModeOrShelvedChanged`. When
several apply at once, precedence is: marker failures (they mean the durability guarantee is
absent) before identity/mode mismatches before baseline-content mismatches.

### What the detail can honestly say

- **git range:** `<prior>..<HEAD>` is free — both endpoints are already pinned by the capture.
- **git commit count:** *not* free. It needs a `git rev-list --count <prior>..<HEAD>`, a
  bounded extra query. Treat the count as optional: report the range unconditionally, and the
  count only if we decide the extra call is worth it (open question 4). The first draft
  asserted "no new git calls" while also promising a count; that was contradictory.
- **Perforce:** `baseline.rs` stores *evidence entries*, not file counts, and the fingerprint
  is over **rendered evidence**, not arbitrary file state (`src/vcs/perforce.rs:503-510`). So
  the honest phrasing is "N of M evidence units re-sent, the rest collapsed as byte-identical
  to evidence included in the previous server-generated capture" — with the counts computed
  *during* elision (they are not stored today). Note the careful wording the round-2 review
  drew out: the server knows only what it **generated and sent last turn**, not what the
  reviewer received or retained, so the phrasing says "the previous server-generated capture,"
  never "what you were last shown." And never "M of K files changed," which would claim a
  file-level comparison Perforce does not perform.

### How the caller sees it

In `tools.rs`, on a **resumed turn that sent a change** (see
[When a disposition exists at all](#when-a-disposition-exists-at-all)), render the disposition
into the caller-facing response. Two sub-decisions, both for the reviewer to weigh in on:

- **Channel — a typed field, not a warning string.** `warnings` is framed as "the captured
  change was incomplete" — a shortfall — and it is a `Vec<String>`. A clean `Incremental` or
  `FullByDesign` turn is not a shortfall, and encoding structured data as prose in the warning
  list is the wrong shape twice over. So carry a **typed disposition field**
  through the result path — `Outcome` → `Review` → `Snapshot` in `src/registry.rs`
  (`:143-155`, `:558-599`), which the first draft omitted from the plumbing entirely — and
  render it into the response as its own informational `disposition:` line beside `usage:`.
  Only one state also earns a `warnings` entry: **`FellBackToFull` on a resume**, the cost
  surprise where the delta the caller configured for silently stopped happening.
  `FullByDesign` never warns.
- **Recording.** Record the disposition in the usage/metrics record next to `prompt_bytes`,
  so an after-the-fact audit of a session can see which turns deltaed and which re-billed
  the full range — the attribution point 1 above is missing today.

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
  byte-identical to evidence in the previous **server-generated** capture. It must not assert
  the reviewer received, retained, or still holds the earlier context, none of which the
  server can know.

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

- `src/vcs/git.rs` — the disposition, and the reason-preserving refactors: `effective_base`
  split into a status enum (`ModeNotDeltable` / `CurrentHeadUnavailable` /
  `CurrentBaseUnresolvable` / resolved-base), `is_ancestor` made three-way (yes / no /
  undecidable), the prior-HEAD validation ahead of the ancestry check, plus the optional
  `rev-list --count`.
- `src/vcs/perforce.rs` — compute the re-sent/collapsed evidence-unit counts during elision
  (not stored today).
- `src/vcs/shared.rs` / `src/vcs/mod.rs` — carry the *optional* typed disposition on `Capture`
  (**not** `CapturedChange`, which does not hold the baseline), computed only when
  `change.is_some()`.
- `src/tools.rs` — the layer the backend cannot see: apply the fresh-vs-resumed framing and
  the `NoCompleteBaselineRetained` reason (the backend receives only `Option<Resume>`), split
  the Perforce durability reasons at their pre-capture decision point (`:805-833`) rather than
  losing them to `resume = None`, gate on `change.is_some()`, and surface + record.
- `src/registry.rs` — thread the typed field through `Outcome` → `Review` → `Snapshot`
  (`:143-155`, `:558-599`); this is the plumbing the first draft missed.
- `src/metrics.rs` — the record field, plus tests throughout.

Per the task framing, foundational completeness is preferred over minimising this. The change
is cohesive, but it is not a one-liner: getting the *reasons* accurate (round-1 findings 2–3,
round-2 findings 1–4) is most of the work, and doing it wrong — a mislabelled `BranchRewritten`,
a `BaseMoved` decided on an unresolved base, a false `FellBackToFull` warning on an intentional
`--diff HEAD`, or a disposition emitted when nothing was sent — would be worse than reporting
nothing.

## Testing

- **Unit, git — every reason maps correctly, including the ones the code must now tell apart.
  The distinctions the round-2 review forced each need their own test:**
  - `BranchRewritten` — prior HEAD is a valid, *available* commit that is genuinely not an
    ancestor (a divergent branch), producing a definite "no" from the three-way check.
  - `AncestryUndecidable` — the check could not *run*. Test with a syntactically **valid but
    unavailable** object id (or a forced git failure), **not** an invalid string (rejected by
    `is_object_name` before ancestry) and **not** a valid unrelated commit (that is
    `BranchRewritten`). This is the case with no coverage today.
  - `BaseMoved` — base ref advanced to a different, *resolvable* commit.
  - `CurrentBaseUnresolvable` — the configured left ref no longer resolves; asserts it does
    **not** get mislabelled `BaseMoved`.
  - `CurrentHeadUnavailable` — HEAD will not resolve; asserts a retained prior baseline is not
    mislabelled `NoCompleteBaselineRetained`.
  - `PriorBaselineInvalid` — a corrupt stored prior HEAD; asserts it never reaches the
    ancestry command.
  - `FullByDesign { ModeNotDeltable }` — a working-tree (`--diff HEAD`) or non-HEAD range
    resume; asserts it emits the info line but **no** warning.
  - `FullByDesign { Disabled }` — `--no-incremental-resume`; same no-warning assertion.
  - **`NoCompleteBaselineRetained`, both ways (round-1 finding 3):** (a) first-turn truncation,
    where no baseline ever existed → fall-back; and (b) truncation *after* an established
    baseline, which must still delta from the retained older complete `(head, base)` pair
    (`src/session.rs:208`, `src/tools.rs:821-825`) → **`Incremental`**, proving the reason is
    "no complete baseline retained," not "the last capture truncated."
- **Unit, no-capture states (round-2 finding 3):** `--diff auto` with a shell-equipped
  reviewer, `--diff none`, a cancelled capture, and a failed capture each emit **no**
  disposition and **no** warning — the server sent no change, so it says nothing about one.
- **Unit, git round trip:** a two-turn temp repo asserts turn 2's disposition is
  `Incremental` over `prior..HEAD` and that the caller-facing line contains the range.
- **Unit, tools:** a resumed turn that sent a change emits the disposition line; a fresh turn
  does not; a `FellBackToFull` resume also emits the warning; a `FullByDesign` resume emits the
  info line but **no** warning.
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
