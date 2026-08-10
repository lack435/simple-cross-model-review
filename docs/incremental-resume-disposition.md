# Incremental-resume disposition, surfaced to the caller — design

Status: **implemented and merge-gate approved.** This document was the plan; it went through
eight rounds of this repository's own `cross-review` gate (Codex, gpt-5.6-luna, effort=max)
ending in APPROVE, and the implementation then went through two rounds of the same gate ending
in APPROVE — "all three round-1 findings are resolved, with no new correctness, security,
interface, or resource regressions found." The engine lives in `src/vcs/disposition.rs`,
`src/vcs/git.rs`, `src/vcs/perforce.rs`, `src/session.rs`, `src/tools.rs`, `src/registry.rs`,
`src/metrics.rs`, `src/vcs/shared.rs` and `src/vcs/mod.rs`. Filed against issue #41 ("Incremental
re-review: send the reviewer only the delta since its last turn"), but deliberately *not* a
re-implementation of it — see [What #41 already got](#what-41-already-got).

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
- **Perforce** (PR #38): per-file elision keyed on a fingerprint of the evidence the server
  rendered in the previous capture, fail-closed on every uncertain signal, with a durable
  in-progress marker so a crash cannot collapse against a stale baseline. See
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
change (`capture.change.is_some()`). If either is false there is no disposition line and no
*disposition-specific* warning:

- A **fresh** turn (turn 1, or a name rebound to a new reviewer session) has no prior turn to
  be incremental against. Internally this is the `Full` state; it is never rendered.
- A turn where the server **sent no change** — `--diff none`, `--diff auto` with a
  shell-equipped reviewer (`supplies_change()` is false, `capture.change` is `None`), or a
  capture that **failed or was cancelled** — cannot honestly carry a disposition about a diff
  the reviewer was never handed. Saying "full re-capture" there would violate the
  say-only-what-was-sent boundary as surely as an overclaiming `Incremental` line would.

**This suppresses only the new disposition output — it must not touch the existing warnings.**
A *failed* capture already produces `Capture::warn(...)` (`src/vcs/git.rs:441`, `:519`) and
those warnings are forwarded to the caller as the fail-closed contract requires; the
disposition gate adds nothing there and removes nothing. The rule is precisely: no disposition
line, and no disposition *warning*, when `change` is absent — every capture, adapter, usage and
persistence warning that flows today still flows.

So the disposition is `Option`-typed and computed from `capture.change`, not alongside it.

### The disposition value: an ordered decision, not a flat list

The states below are **decided in order**; the first matching rule wins. Presenting them as a
flat list is what let the first two drafts leave a resumed-no-baseline turn described by two
states at once, and let a single `None` from `effective_base` stand in for five different
facts. The order *is* the precedence.

**Two absolute gates come first, for *both* backends, above every fall-back tree.** The round-5
review found the Perforce tree could otherwise warn about a marker failure on a run where the
feature was switched off — so these are gates, not tree steps:

- **G0 — not a resume, or no change sent.** Handled already in
  [When a disposition exists at all](#when-a-disposition-exists-at-all): a fresh turn is the
  internal `Full` state and is never rendered; a turn that sent no change emits nothing. In
  particular a *fresh* Perforce session has no baseline, but that is `Full` (internal), **never**
  a `FellBackToFull` fall-back.
- **G1 — `FullByDesign { Disabled }`** — `--no-incremental-resume` is set. Intentional, rendered
  as an info line, and it emits **no *disposition* warning** (the `FellBackToFull` cost-surprise
  kind); as a gate it wins over *every* fall-back reason below, including the Perforce marker
  guards. It also emits **no marker *persistence* warning**: those exist to protect a *future*
  elision, and with the feature disabled there is no future elision to protect, so a failed
  `mark_pending()` under G1 is immaterial. (This is the distinction the round-7 review forced —
  a disposition warning and a persistence warning are two different channels; see
  [`MarkerUnwritable`'s scope](#perforce-durability-reasons) and
  [How the caller sees it](#how-the-caller-sees-it).)

Only for a **resumed, change-sending, feature-enabled** turn does a backend fall-back tree run.
The git backend then decides:

1. **`FullByDesign { ModeNotDeltable }`** — the mode is not a HEAD-anchored committed range: a
   working-tree (`auto`/`HEAD`) or staged diff (which carry uncommitted work a commit delta
   would drop), or a fixed window like `HEAD~3..HEAD~1` (which does not move with HEAD).
   Intentional; **never warns**. Warning on a resumed `--diff HEAD` review would be the false
   alarm the round-1 review flagged. (git-only — see the Perforce note below.)
2. **`FellBackToFull { NoCompleteBaselineRetained }`** — the mode *is* an eligible
   HEAD-anchored range, but the session carries **no complete `(head, base)` pair** to delta
   from. This is where a resumed turn with no usable baseline lands — resolving the ambiguity
   the round-2 review found: `Full` is reserved for *fresh* turns; a resumed eligible turn
   without a baseline is a fall-back, not a `Full`. It covers first-turn truncation followed
   by a resume (no baseline ever persisted). It does **not** cover truncation *after* an
   established baseline, because the session retains the prior complete pair
   (`src/session.rs:208`) and that later turn stays `Incremental`. **When both this and #3
   hold (no baseline *and* HEAD will not resolve), this wins** — the absence of a baseline is
   the more fundamental block, and it needs no git call to decide.
3. **`FellBackToFull { CurrentHeadUnavailable }`** — HEAD will not resolve now, so there is no
   right endpoint to delta to. This is an **unborn HEAD** (a repository with no commits) or a
   failed `git rev-parse HEAD` — **not** a detached HEAD, which `rev-parse` resolves to its
   commit SHA and accepts (`src/vcs/git.rs:996`); the round-3 review corrected the first
   draft's inclusion of "detached" here. Distinct from #2: a complete prior baseline may still
   exist; what is missing is *this* turn's HEAD.
4. **`FellBackToFull { CurrentBaseUnresolvable }`** — the configured range's left ref no longer
   resolves, or a three-dot merge-base cannot be computed. The base is unknown, so `BaseMoved`
   cannot even be evaluated — this is why it must be a *separate* reason decided *before* it.
5. **`FellBackToFull { PriorBaselineInvalid }`** — the **stored** prior head *or* prior base is
   not a syntactically usable object id (a corrupt or truncated session record). Decided here,
   **before** `BaseMoved`, because the round-3 review found the first draft comparing
   `resume.base` against the current base *before* validating it — a garbage stored base would
   otherwise be reported as a moved base. Both stored fields are checked, not just the head.
   Syntactic validation (`is_object_name`) needs no git call; this does **not** claim the
   stored ids are still *reachable* objects (that would need a query and is not required — see
   below).
6. **`FellBackToFull { BaseMoved }`** — the stored base and the current effective base are both
   present and valid (per #4 and #5) **and differ** (`main` advanced, or `--diff` was
   repointed). This is a comparison of two object ids the server itself produced; it does not
   assert either is still reachable, only that the recorded base is not the current one.
7. **`FellBackToFull { AncestryUndecidable }`** — the ancestry check could not be *run* (git
   error/timeout): a *three-way* result, not the `false` that today's `is_ancestor` collapses
   error into. Reported as its own reason rather than mislabelled `BranchRewritten`.
8. **`FellBackToFull { BranchRewritten }`** — the prior HEAD is a valid commit that is
   definitively **not** an ancestor of HEAD (rebase, amend, force-push).
9. **`Incremental`** — none of the above fired: the delta is `<prior>..<HEAD>`. Carries the
   range (git, free — both endpoints are already pinned) and optional backend detail; see
   [What the detail can honestly say](#what-the-detail-can-honestly-say).

Every `FellBackToFull { … }` reason warns (an eligible delta that stopped happening is a cost
surprise); every `FullByDesign { … }` reason does not; `Incremental` does not. That mapping is
now a property of the state, not a per-call judgement.

The **Perforce** backend shares the two absolute gates (G0 fresh/no-change, G1 `Disabled`) and
the `Incremental` outcome (step 9), and substitutes its own durability/identity reasons for the
git fall-back steps #2–#8. Two clarifications the reviews forced: **git step 1
(`ModeNotDeltable`) does not apply at all**, because `--diff` is git-only and a Perforce review
always intends to capture its named changelists (`src/config.rs:465`, `:581`); and the Perforce
reasons are **not** all decided in one place. Where each is decided — split between `tools.rs`
(the pre-capture marker guards, subordinate to the G1 `Disabled` gate) and `perforce::capture`
(identity, shelved-mode, inventory usability, via `elision_active`) — is spelled out under
[Perforce durability reasons](#perforce-durability-reasons).

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
- **Both stored baseline fields (prior head *and* prior base) are validated before either is
  used** — the base before the `BaseMoved` comparison, the head before the ancestry check — so
  a corrupt/truncated stored id is `PriorBaselineInvalid` rather than silently miscompared or
  fed to git. Today the code compares `resume.base` before validating the stored head
  (`src/vcs/git.rs:700`, `:703`) and never validates the stored base; this reorders and
  completes that.
- **Fresh-vs-resumed is invisible to `vcs::capture`, which receives only `Option<Resume>`
  (`src/vcs/mod.rs:34-38`).** `None` there means *both* "a fresh turn" and "a resumed turn
  whose session held no baseline" — the backend cannot tell them apart, and it must not, since
  the fresh case emits nothing while the resumed-no-baseline case is
  `FellBackToFull { NoCompleteBaselineRetained }`. So the **fresh-vs-resumed framing and the
  no-baseline reason are assigned in `tools.rs`**, which knows `resume_id` and whether a
  baseline was passed; the backend only reports the decisions that are its own (mode, base,
  ancestry).
- **The Perforce durability reasons are decided in *two* layers, not one:** the marker guards
  in `tools.rs:805-833` before capture, and identity/mode/inventory inside `perforce::capture`
  (`elision_active`). The round-3 review corrected the first draft's claim that they all live
  in `tools.rs` — see [below](#perforce-durability-reasons).

None of these add a git/p4 call for the *decision* (the ancestry and base-resolve commands
already run); they change what the existing calls' outcomes are allowed to say, and add
*syntactic* validation of the stored ids, which is free. Two things are explicitly **not**
free and are called out where they arise: proving a stored id is still a *reachable* object
(not required — `BaseMoved` only compares recorded-vs-current, and the ancestry command
reports an unavailable prior head as `AncestryUndecidable`), and the optional commit *count*
(a `rev-list --count`, see below).

Then thread the disposition out through `Capture` (which already carries `head_sha` /
`base_sha` — note that `CapturedChange` does **not**; the first draft misplaced this) to
`tools.rs`, where the fresh/resumed layer above is applied.

#### Perforce durability reasons

The Perforce full-capture reasons are **decided in two different places**, and each successive
review found the first drafts under-enumerating them. The taxonomy below is meant to be
*complete* and *deterministic* — but note precisely what "non-overlapping" means here, because
the round-6 review rightly pushed on it. The underlying predicates are **not** mutually
exclusive: a turn can be both `PriorBindingIncomplete` and `PriorBaselineUnusable`, or both
`IdentityChanged` and `ModeOrShelvedChanged`, at once. What is guaranteed is that **exactly one
reason is *selected*, by the precedence order below** — "complete" meaning every "do not elide"
path maps to at least one reason (none comes back unclassified), and "non-overlapping" meaning
the precedence picks exactly one. The round-4 review found three uncovered paths — an absent
baseline, a `None` persisted binding field, and an *unconfirmed* (not *changed*) identity —
which the earlier drafts left with no reason or blamed on `IdentityChanged`.

These reasons are reached **only after the G0 and G1 gates above** — a fresh turn is `Full`
(never a `PriorBaselineMissing` fall-back) and `--no-incremental-resume` is `FullByDesign
{ Disabled }`, both of which win over every reason here, including the marker guards (the
round-5 review found the marker guards otherwise firing on a disabled run).

**Decided in `tools.rs`, before capture** (`:805-833`), which forces `resume = None` in these
cases:

- `PriorBaselineMissing` — a **resumed** session whose `perforce_baseline` is **`None`**: the
  prior turn persisted no baseline field at all. Strictly `None`, **not** `Some(Disabled)` — a
  persisted `Disabled` is a *present* baseline that is unusable, which is `PriorBaselineUnusable`
  below (the round-6 review drew this line: `tools.rs` passes `Some(Disabled)` to the backend,
  where `usable_inventory()` rejects it). The Perforce analogue of git's
  `NoCompleteBaselineRetained`. A *fresh* session with no baseline is G0's `Full`, not this.
- The **marker guards**, decided from a three-valued marker read (present / absent /
  **unreadable**), because `is_pending()` returns `true` on any marker-state I/O error
  (`src/session.rs:321-326`) — so "the prior turn left a marker" and "the marker state could
  not be read" must not collapse into one claim:
  - `MarkerUnwritable` — *this* turn could not write its in-progress marker (`!pending_marked`);
    a later crash would be undetectable, so the turn refuses to elide and records `Disabled`.
  - `PriorTurnPending` — the marker read **confirmed present**: the previous turn crashed or
    failed to persist, so its baseline may be stale.
  - `MarkerStateUnreadable` — the marker read **errored**; fail-closed, but reported as its own
    reason rather than as a false `PriorTurnPending`.

**Decided inside `perforce::capture`, via `elision_active`/`matches`** (`src/vcs/baseline.rs:118`,
`:125`):

- `PriorBindingIncomplete` — the persisted binding cannot be compared because a needed field is
  `None`. Three shapes, all folded here (the round-5 review found the third uncovered):
  the outer `identity` is `None`; `include_shelved` is `None` (both records predating those
  fields, `src/session.rs:86`, `:91`); **or the persisted `identity` is `Some` but its nested
  `client_spec_digest` is `None`**, which `matches` also rejects (`src/vcs/baseline.rs:118`,
  `:125`) even against a usable `Full` inventory. Not a *change* — there is nothing complete to
  compare against.
- `IdentityUnconfirmed` — *this* turn's identity cannot be confirmed because its
  `client_spec_digest` is `None`, which `matches` intentionally rejects
  (`src/vcs/baseline.rs:118`, `:125`). Distinct from `IdentityChanged`: the identity did not
  differ, it could not be *established*. (The mirror of the nested-`None` case above, on the
  *current* rather than the *persisted* side.)
- `IdentityChanged` — both identities are confirmed and **differ**.
- `ModeOrShelvedChanged` — the shelved-capture flag differs.
- `PriorBaselineUnusable` — a prior baseline that is `Disabled` or an otherwise unusable
  inventory. Its predicate can co-occur with the identity/mode/binding ones above, but it sits
  **lowest in precedence**, so it is *selected* only when none of them applies — which is what
  keeps its reported reason distinct without claiming the predicates are mutually exclusive
  (the round-6/7 refinement of "non-overlapping").

`tools.rs` combines the backend's decision with its pre-capture decision. **Precedence when
several hold** (first wins): `MarkerUnwritable` (this turn cannot guarantee its *own*
durability — the most immediate failure) → `PriorTurnPending` → `MarkerStateUnreadable` →
`PriorBaselineMissing` → `PriorBindingIncomplete` → `IdentityUnconfirmed` → `IdentityChanged` →
`ModeOrShelvedChanged` → `PriorBaselineUnusable`.

**`MarkerUnwritable` warns whenever elision is enabled, independent of the disposition.** A
failed `mark_pending()` means the *next* turn cannot safely elide — so, **when the feature is
on**, it surfaces as an ordinary **persistence warning** on its own footing, whether or not this
turn rendered a disposition. Two boundaries the reviews drew:

- **G0 does not suppress it.** A *resumed* turn that sent no change has its disposition
  suppressed by G0, but if `mark_pending()` failed, the turn still forces `Disabled` and the
  next turn still cannot elide — so the persistence warning fires there too. G0 suppresses only
  the *disposition* output, the same discipline as the failed-capture warnings.
- **G1 *does* moot it.** Under `--no-incremental-resume` there is no future elision to protect,
  so a failed marker write is immaterial and no persistence warning fires — this is why G1's
  "no warning" and this "warns" rule do not contradict: the persistence warning is conditioned
  on elision being enabled, and G1 is exactly the case where it is not.

A **persistence warning** (durability of a future elision) and a **disposition warning** (this
turn's `FellBackToFull` cost surprise) are separate channels; keeping them separate is what
resolves the round-7 contradiction.

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
  Only one state also earns a **disposition warning** in `warnings`: **`FellBackToFull` on a
  resume**, the cost surprise where the delta the caller configured for silently stopped
  happening. `FullByDesign` and `Incremental` produce no disposition warning. This is separate
  from the pre-existing capture/persistence warnings (including `MarkerUnwritable`), which flow
  on their own terms regardless of the disposition — see the two-channel distinction under
  [Perforce durability reasons](#perforce-durability-reasons).
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
- `src/session.rs` — a three-valued marker read: change `is_pending()` (or add a sibling API)
  to return present / absent / **unreadable** instead of folding an I/O error into `true`, so
  `PriorTurnPending` and `MarkerStateUnreadable` are distinguishable. Call-site and
  legacy-record migration tests included.
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
  - `CurrentHeadUnavailable` — an **unborn** HEAD (no commits); asserts a retained prior
    baseline is not mislabelled `NoCompleteBaselineRetained`. A separate test asserts a
    **detached** (but committed) HEAD stays `Incremental` — `rev-parse HEAD` resolves it, so it
    is *not* `CurrentHeadUnavailable` (round-3 finding 4).
  - `PriorBaselineInvalid` — a corrupt stored prior **base** (asserting it is not miscompared
    as `BaseMoved`) *and* a corrupt stored prior **head** (asserting it never reaches the
    ancestry command). Both stored fields, per round-3 finding 2.
  - `FullByDesign { ModeNotDeltable }` — a working-tree (`--diff HEAD`) or non-HEAD range
    resume; asserts it emits the info line but **no** warning.
  - `FullByDesign { Disabled }` — `--no-incremental-resume`; same no-warning assertion.
  - **`NoCompleteBaselineRetained`, both ways (round-1 finding 3):** (a) first-turn truncation,
    where no baseline ever existed → fall-back; and (b) truncation *after* an established
    baseline, which must still delta from the retained older complete `(head, base)` pair
    (`src/session.rs:208`, `src/tools.rs:821-825`) → **`Incremental`**, proving the reason is
    "no complete baseline retained," not "the last capture truncated."
- **Unit, no-capture states (round-2 finding 3, round-3 finding 1):** `--diff auto` with a
  shell-equipped reviewer, `--diff none`, and a cancelled capture emit **no disposition line
  and no disposition warning**. A **failed** capture likewise emits no *disposition* output —
  but its existing `Capture::warn(...)` warning must still reach the caller unchanged; the test
  asserts both (the disposition is absent *and* the pre-existing failure warning is preserved),
  so the disposition gate cannot regress the fail-closed contract.
- **Unit, Perforce reason coverage (round-4 finding 1–2):** each `elision_active`/marker
  false-path maps to its distinct reason and none is left unclassified — `PriorBaselineMissing`
  (no stored baseline), `PriorBindingIncomplete` (`None` persisted `identity`/`include_shelved`),
  `IdentityUnconfirmed` (current `client_spec_digest` is `None`, asserting it is **not**
  `IdentityChanged`), `IdentityChanged` (both confirmed and differing), `ModeOrShelvedChanged`,
  `PriorBaselineUnusable` (`Disabled` inventory), and the three marker states from a
  three-valued read — `MarkerUnwritable`, `PriorTurnPending` (confirmed present),
  `MarkerStateUnreadable` (read errored, asserting it is **not** a false `PriorTurnPending`).
  `PriorBindingIncomplete` is tested in all three shapes, including the round-5 case: a
  persisted `identity: Some` whose nested `client_spec_digest` is `None` paired with a usable
  `Full` inventory (asserting it is **not** left unclassified and **not** `IdentityChanged`).
  `PriorBaselineMissing` (`perforce_baseline == None`) is tested distinctly from
  `PriorBaselineUnusable` (`Some(Disabled)`), per round-6 finding 1. **Precedence** is tested for
  the combinations where predicates genuinely co-occur (round-6 finding 2), not just one pair:
  `MarkerUnwritable` over `PriorTurnPending`; G1 `Disabled` over a marker failure (info line,
  **no** warning); `PriorBindingIncomplete` over `PriorBaselineUnusable`; `IdentityChanged` over
  `ModeOrShelvedChanged`.
- **Unit, persistence warning (round-6 finding 3, round-7 finding 1):** with the feature
  **enabled**, a resumed no-change turn whose `mark_pending()` fails emits **no** disposition
  (G0) but **does** emit the ordinary `MarkerUnwritable` persistence warning — G0 suppresses only
  disposition output. Its counterpart: with `--no-incremental-resume` (G1), the *same*
  marker-write failure emits **no** persistence warning either, because there is no future
  elision to protect — the two tests together pin the enabled-only scope that resolves the
  round-7 contradiction.
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
