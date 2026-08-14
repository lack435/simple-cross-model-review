# Structured-channel parity (issue #73)

**Status:** **implemented.** The plan converged on its round 9 (`rv-28052-11`), and the
implementation converged on its round 3 (`rv-28052-14`, `outcome: converged`, `verdict: approve`,
0 of 8 findings open). See §13 for the implementation review.
**Issue:** [#73](https://github.com/lack435/simple-cross-model-review/issues/73) — `approve_with_comments`
returns no readable comments.
**Supersedes** the `review_prose` rule in
[`unstructured-turn-recovery.md` § Decision B](unstructured-turn-recovery.md#decision-b--the-prose-travels-on-the-structured-channel-when-the-machine-channel-is-incomplete)
(issue #63). It does not supersede anything else in that document.

## 1. What is actually broken

`Envelope::with_prose` (`src/findings.rs:854`) attaches the reviewer's prose to the structured
channel only when `structured == false` or the reason is `turn_not_durable`. On a clean structured
turn `review_prose` is `null`, so a client that surfaces only `structuredContent` — Claude Code is
one — sees nothing the reviewer said outside its findings list. Issue #73 filed the sharpest case:
`approve_with_comments` returns `outcome: escalate` ("a person decides") with nothing for that person
to read. This is not hypothetical; it happened on this repository's own PR #71 review, and it
happened again on round 1 of this very plan's review, which returned `review_prose: null` alongside
seven findings.

The issue suggests a narrow fix: widen the `with_prose` condition to cover `approve_with_comments`,
`escalate`/`rebaseline`, and optionally `blocked`. This plan argues for unconditional attachment
instead, **and it argues it from the parity invariant in §2, not from what happened on any particular
turn of any particular session.** Rounds 1 and 2 of this plan's own review both held (finding `f1`)
that the first draft rested on an unprovable classification of the #71 rounds. That prop is gone; the
two arguments below stand on the design and on the repository's own instructions.

### 1.1 A condition on `outcome` or `verdict_detail` re-collapses the two axes #63 separated

This is the decisive argument, and it is internal to the design rather than forensic.

`unstructured-turn-recovery.md` § Decision C states the rule plainly: **"`outcome` is the action
axis; it is deliberately not the content axis. Whether this turn produced a machine record is
`structured`, and what to read when it did not is `review_prose`. The two are orthogonal, and
collapsing them was the draft's mistake."** The #63 design rejected an earlier draft precisely
because it keyed a content decision off an action-shaped discriminator.

The issue's suggested fix does exactly that, in the other direction: it makes *what the envelope
carries* a function of `outcome` and `verdict_detail`. Whether the reviewer wrote something worth
reading is a property of the reviewer's prose, not of the action the caller should take next. Any
condition of that shape will be wrong on some turn, because the two axes are independent by design —
and the maintenance cost is a rule that has to be re-litigated every time a verdict or outcome value
is added. Unconditional attachment removes the question.

### 1.2 The repository already carries a standing workaround for the ordinary-turn case

`AGENTS.md` currently tells every agent working here:

> **You may not be able to read the reviewer's prose.** … Until that is fixed, ask for reasons and
> paths to be stated *in findings*, and do not read silence as absence of an answer.

That instruction is unconditional. It is not scoped to `escalate` turns, and it exists because the
loss bites during ordinary iteration — the phase that is almost entirely `changes_requested`. A fix
that leaves `changes_requested` turns unchanged does not let that paragraph be deleted, which is a
good test of whether the bug is actually closed.

### 1.3 What the #71 evidence does not establish

The issue reports that in rounds 5 and 6 of the #71 review the agent asked why `f10` was still open,
got no readable answer, and argued the finding was resolved when it was not.

**The plan makes no claim about the `outcome` of those rounds, and does not need one.**
`src/metrics.rs:451` records `structured` per turn but not `outcome`, `verdict`, or `open_count`, so
no artifact remains that would settle it, and an open finding does not by itself imply
`changes_requested` — `blocked` gives `escalate`, and the coverage and budget conditions give
`rebaseline` (`src/findings.rs:749-779`). Rounds 1 and 2 of this review held finding `f1` against
earlier drafts that inferred it anyway; the inference is deleted rather than defended, because
nothing in the plan depends on it.

What the #71 report does establish, without any classification: the reviewer said something
load-bearing that no channel the caller could read carried, and a live finding was argued closed as a
result. That is the defect. Which `outcome` the turn wore is the axis §1.1 says should not be
consulted in the first place.

### 1.4 The same defect, wider than the issue reports

The text body of a completed result carries several facts that never reach `structuredContent`:

| In the text body (`src/tools.rs:1121-1238`) | In `structuredContent` today |
| --- | --- |
| `WARNING:` lines — capture partial, tree dirty, turn not saved, account moved mid-turn, usage unknown | absent (`warnings` carries only the turn-evaluation warnings) |
| `Note: the reviewer tried N command(s) it was not permitted to run…` | absent |
| `captured:` — resolved range, file/line counts, truncation, complete/partial | absent |
| `disposition:` — what was sent on a resumed turn | absent |
| `usage:`, `elapsed:`, `reviewer:`, `review_id` | absent |
| "this review was not saved as a resumable session" | absent |

Two of those are the exact failures `AGENTS.md` already documents: a capture widened by a stale local
`main` (1707 insertions instead of 208) and a reviewer that burned its turn on policy-blocked
commands (#68). Both read as an ordinary successful review to a `structuredContent`-only client.
That is the "quietly thinner than it looks" case `AGENTS.md` singles out as the one review failure
that is *not* merely retryable, because it produces a false approval rather than a lost review.

Conversely, the envelope's own `warnings` (e.g. "reviewer marked verdict approve but 3 finding(s) are
still open; treated as changes") are never rendered as `WARNING:` lines in the human text — they
appear only inside the JSON `_OUT` block. The text channel is poorer in that one direction.

## 2. The invariant

> **A completed result's `structuredContent` is never *silently* poorer than its text body.** Every
> fact the text body carries that bears on weighing the review is also on the structured channel, in
> full; the reviewer's prose is there up to a declared bound that the envelope itself reports; and
> the two machine-readable copies of that channel are identical.

Two words in that are load-bearing, and both were added because a draft without them overclaimed.

**"Silently"**, because of the prose cap. `structuredContent.review_prose` is capped at 16,000
characters (§3.1) while the text body carries the reviewer's prose in full, so on an over-cap review
the structured channel *is* poorer — and §3.1 accepts that rather than truncating the text channel to
match. A blanket "never strictly poorer" therefore contradicted this plan's own §3.1, which is what
the focused control review (`rv-28052-9`) caught. What the design actually guarantees is that the
shortfall is bounded, confined to free-form prose, and **announced**: `review_prose_truncated` is on
the wire precisely so a structured-only client knows it is holding a partial copy rather than a
complete one. A gap a client can detect is a different thing from one it cannot, and only the second
is the bug in issue #73.

**One-directional**, deliberately. Earlier drafts wrote the invariant as a symmetry — "and the text
body carries everything the structured channel carries" — which the resumed review held finding `f6`
against four times. Narrowing is the resolution rather than propping it up: the reverse is not
something this design guarantees, because the human body is assembled by hand (§5). The focused
control review agreed with that specific call, on the grounds that the human body intentionally
carries prose, framing and next-step guidance that no machine JSON represents.

It is also the form the repository already settled on. `running_structured_value` carries the
progress and liveness group specifically so that "the structured channel is not strictly poorer than
the text one" (round-13 implementation review) — one direction, the one that matters, because the
text channel was never the one going unread. This plan applies that same rule to the completed
variant, which is where it was never applied.

| Claim | Status |
| --- | --- |
| The two machine channels (`structuredContent` and the `_OUT` block) are identical | **Guaranteed by construction.** One `Value`, built once by `completed_result`, which owns it (§5). |
| The structured channel carries every text fact that bears on weighing the review | **Guaranteed by construction.** Both renderings read one `ResultContext`, built once (§5). |
| The structured channel carries the reviewer's prose | **Guaranteed up to a declared bound.** Complete under 16,000 characters; above it, the head plus a note, with `review_prose_truncated: true` saying so (§3.1). Never short without saying it is short. |
| The text body renders every key the structured channel carries | **Not guaranteed. A tested property** (§9 test 11), and not part of the invariant. Worth keeping honest — the test iterates the value's keys so a field added to one side alone fails — but the plan does not claim the text body is bound to the envelope's shape, and it is not. |

## 3. Decision 1 — the prose is attached whenever a turn ran, unconditionally

`review_prose` is non-null **iff a reviewer turn ran**. No verdict test, no outcome test, no
`structured` test.

| Situation | `review_prose` |
| --- | --- |
| Any turn that ran — converged, `changes_requested`, `escalate`, `rebaseline`, degraded, not durable | the prose (capped) |
| Over-budget on entry (no reviewer ran) | `null` |

`Some("")` and `None` stay distinct and both are meaningful: an empty string is "a turn ran and the
reviewer wrote nothing outside its block"; `null` is "no turn ran". That distinction already exists
in the code (`src/findings.rs:1554`) and is kept. It also becomes load-bearing elsewhere — see §6.

**Enforced by construction, not by a rule.** `Envelope::with_prose` is deleted. Prose becomes a
required argument of the constructors that build a ran-a-turn envelope:

- `finalize_turn` sets `review_prose` directly in both arms (it already owns the prose).
- `not_durable_envelope(session, turn, prior, prose, carry_warnings)` takes it as a parameter, so the
  one external call site (`src/tools.rs:3213`) cannot forget it.
- `over_budget_on_entry_envelope` keeps `review_prose: None` — no turn ran, and there is no prose to
  pass.
- `Envelope::turn_ran()` is added as the named reading of `review_prose.is_some()`, so the rest of
  the code asks the question rather than re-deriving it.

A builder step a caller may omit is what produced this bug; removing the step removes the class.

**One prose value, assembled before either channel is rendered.** Today `src/tools.rs:2926-2935`
appends block-repair notes to `parsed.text` *after* `finalize_turn` has produced the envelope, so the
text body carries the notes and the envelope's prose does not — a structured-only client would lose
exactly the "I have reconsidered f2" content the note exists to preserve (round-1 finding `f3`).
`TurnAssessment` gains `push_repair_note(&mut self, note: &str)`, which owns the
`--- BEGIN BLOCK REPAIR NOTE ---` framing that currently lives in `tools.rs`. `tools.rs` sets
`parsed.text = turn_eval.review_prose` and appends nothing further.

### 3.1 The cap applies to the reviewer's prose, and the repair notes survive it

Round-2 finding `f8` caught that the 16,000-character cap makes "both channels carry the identical
string" false at the boundary. The round-3 draft answered by *documenting* the consequence — that
`cap_prose` keeps the head, notes are appended at the tail, so an over-cap prose loses its repair
note from the structured copy — and round 3 promptly **regressed `f3`** for it. Correctly: `f3` asked
for one prose value reaching both channels, and a documented path where the note is dropped from one
of them is that finding coming back, not a caveat on it.

So the composition changes rather than the caveat. `TurnAssessment` keeps the reviewer's prose and
the repair notes as **separate** fields, and `finalize_turn` composes each copy from them:

| Copy | Content |
| --- | --- |
| `TurnEvaluation::review_prose` → the text body's `--- BEGIN REVIEW ---` block | full prose + rendered notes |
| `Envelope::review_prose` → `structuredContent.review_prose` | `cap_prose(prose)` + the same rendered notes |
| The `_OUT` block's `review_prose` | byte-identical to `structuredContent.review_prose` — same `Value`, one construction (§5) |

The cap bounds the one unbounded input, which is the reviewer's free prose. The notes are already
bounded independently (`REPAIR_NOTE_CHARS` per note, and the repair-attempt budget bounds how many),
so appending them after the cap keeps the whole thing bounded while making the note unlosable. Both
compositions live in `finalize_turn`, so there is still exactly one owner of "what prose does this
turn have" — `tools.rs` reads, and does not assemble.

**Both compositions are carried, because there are two consumers.** `TurnEvaluation` gains
`envelope_prose: CappedProse` alongside `review_prose: String` (the full-plus-notes copy). The
not-durable path needs it: it builds a *different* envelope in `tools.rs:3213`, and handing
`not_durable_envelope` the full copy would either blow the cap or, if it re-capped, truncate exactly
the notes this section exists to protect — the control review's finding `f2`. So
`not_durable_envelope` takes the already-composed value and caps nothing itself; capping happens
once, in `finalize_turn`, for every path.

`CappedProse { text: String, truncated: bool }` is a type rather than a bare `String` because the cap
produces two facts and both have to travel. `not_durable_envelope` currently hardcodes
`review_prose_truncated: false` (`src/findings.rs:1627-1630`); handed only the text, it would report
a truncated prose as complete — the control review's finding `f8`, which is exactly the kind of thing
a two-field return value smuggles past a reviewer when it is flattened to one. `cap_prose` returns
`CappedProse`, and every envelope constructor sets both fields from it.

§9 tests an over-cap repair note on the not-durable path specifically, and asserts
`review_prose_truncated` there, since that is the composition the earlier drafts got wrong twice.

The claim that is now made, and is testable: **the two machine-readable channels are byte-identical
to each other**, and the structured copy is the full prose (or its capped head plus the truncation
note) followed by every repair note. Not "all three copies are the same string" — the text copy is
uncapped, and that difference is the cap doing its job. §9 asserts both the under-cap and over-cap
cases, and asserts the note's presence in the structured copy specifically when the prose is over the
cap, which is the case that regressed `f3`.

Why not the narrower condition the issue suggests: §1.1. Why not a second, harder cap (the issue's
alternative): two caps is two behaviours and a second thing to reason about; the existing
`MAX_ENVELOPE_PROSE_CHARS` (16,000 chars) with `review_prose_truncated` already bounds it, and a
typical review is 2–8 KB.

The truncation note is reworded. It currently ends "The full review is in the text channel", which is
advice a `structuredContent`-only client cannot act on. It will state what was dropped and say
plainly that the remainder is available only on the text channel — accurate, and it names the channel
to go and read rather than implying the rest is somewhere the caller can reach. The residual gap for
a >16,000-character prose on a structured-only client is stated and accepted, not machined around;
§2's invariant is worded around it rather than over it, which is the difference between a bounded,
announced shortfall and the silent one issue #73 is about.

## 4. Decision 2 — the result-context group joins the structured channel

The completed structured value gains the facts the text body already prints:

| Key | Type | Source |
| --- | --- | --- |
| `reviewer` | string \| null | the entry that actually ran (`active`), else the chain description; `null` when no reviewer ran (§4.1) |
| `resumed` | boolean | whether this turn continued an earlier review |
| `resumable` | boolean | whether the session can be resumed — today a converged turn whose findings marker could not be cleared is indistinguishable from a clean one |
| `usage` | string \| null | `Usage::summary()`, `null` when empty |
| `captured` | string \| null | `CaptureSummary::summary()`, `null` when no change was sent to a reviewer |
| `disposition` | string \| null | `Disposition::summary()`, `null` on a fresh or no-change turn |
| `denials` | array of string | the same bounded examples the text prints (10) |
| `denial_count` | integer | how many commands the reviewer was refused — **exact only when `denial_count_is_floor` is false** |
| `denial_count_is_floor` | boolean | true when the source output was capped and later refusals were dropped, so `denial_count` is a lower bound (`src/registry.rs:693-696`), which the text renders as "at least N" (`src/tools.rs:1178-1184`) |

**The group started at eleven keys and is now nine.** Round 1 asked which were unearned; the round-2
answer split them into a load-bearing group — `captured`, `denials`, `denial_count`,
`denial_count_is_floor`, `resumable` and the unioned `warnings`, the ones that can turn a thin review
into a false approval — and a cheap identity group, and named the second as the honest place to cut
if anyone wanted one cut. The control review took that offer and named two specifically:
**`review_id` and `elapsed_seconds` are gone.** Neither stops a thin review being mistaken for a
sound one, and each was buying required-schema surface, rendering, sanitisation, tests and response
bytes with nothing in the "false approval" column. `AGENTS.md`'s ratchet warning says to take that
trade, so the plan takes it rather than defending fields it had already conceded were the cut line.

`reviewer`, `resumed`, `usage` and `disposition` stay. Each answers "what evidence did this reviewer
actually see, and under what continuity" — `reviewer` in particular is what tells a caller the review
came from a different model at all, which is this tool's entire premise.

`warnings` keeps its name and becomes the **union** actually shown to the caller: the envelope's own
turn-evaluation warnings first, then the run/server warnings. The text body renders the same union,
in the same order, in its existing `WARNING:` block — so the verdict warnings become visible in the
human text for the first time, and the parity test can compare the two as sequences rather than as
sets. Widening the existing key rather than adding a second one is deliberate: two warning arrays is
a way for a client to read one and miss the other, the distinction between them is not actionable,
and the schema version bump announces the change.

The draft had this order the other way round, which round-2 finding `f9` caught as an internal
contradiction: §6 requires the durability warning to come first, and that warning lives on the
envelope side, so run-warnings-first would have buried it. Envelope-first makes §6 true by
construction rather than by a second rule, which is the only reason to prefer one order over the
other — either reads fine on its own.

One known cosmetic redundancy is accepted rather than machined away: a failed block repair produces a
run warning ("…the attempt failed (CODE): …") and a turn-evaluation warning ("…did not supply a
usable one…") that say overlapping things. Both are already emitted today, each on its own channel;
the union shows both. No de-duplication pass.

**Strings, not structures.** `captured`, `disposition` and `usage` ride as the rendered summary
lines, not as typed objects. The invariant in §2 is parity — the structured channel must not be
*poorer* than the text channel — and the text channel is exactly these strings. Typing
`CaptureSummary`'s two backend variants into the wire schema would be structural *enrichment*, a
larger and separately-reviewable change, and it would put a second renderer next to `summary()` that
can drift from it. Explicitly out of scope; noted here so it is a decision rather than an oversight.

This keeps `src/findings.rs` pure: it receives `&str`/numbers, never `vcs` types.

### 4.1 A no-turn result must not report context facts about a turn that did not happen

Round-1 finding `f6`, confirmed against the code and **a live bug today**, not merely a hazard the
plan would introduce. `attempt`'s over-budget-on-entry short circuit returns `disposition: None,
capture_summary: None` (`src/tools.rs:2556-2572`) precisely because no reviewer ran — and then `run`'s
success arm overwrites both from the capture that had already been taken
(`src/tools.rs:2000-2005`), and assigns `active` unconditionally (`src/tools.rs:2085`). So the text
body already prints a `captured:` line for a turn in which nothing was sent to any reviewer.
Surfacing that into the structured channel unchanged would satisfy parity while propagating a false
fact, which is worse than the gap being fixed.

The finding offered two fixes: suppress the fields for no-turn results, **or move the budget check
earlier**. Rounds 2 and 3 both took the first, and `f6` was held open both times. The second is the
better answer — the whole class disappears rather than being masked, which is `AGENTS.md`'s "answer a
finding by removing the thing that made it reachable".

**The check moves to the top of `run`, ahead of `set_active`, `mark_pending` and the capture.** The
round-4 draft said "ahead of the capture" and was wrong three times over; the control review's
finding `f1` established exactly where, and the plan takes its correction rather than its own reading:

| What the round-4 draft assumed | What the code does |
| --- | --- |
| `run` assigns `active` only after the walk (`src/tools.rs:2085`), so returning early leaves it unset | `run` **publishes `active` before the capture**, at `src/tools.rs:1736`, so a snapshot taken during the `Capturing` phase names the entry that will run |
| An `Outcome` with `active: None` clears it | `Registry::finish` **preserves** the already-published value: `if outcome.active.is_some() { review.active = outcome.active }` (`src/registry.rs:490-493`) |
| `active: None` renders as no `reviewer:` line | `render_completed` **falls back to the chain description**: `snapshot.active.clone().unwrap_or_else(\|\| self.cfg.describe_reviewer())` (`src/tools.rs:1137-1140`) |

So "before the capture" would still have produced a `reviewer:` line for a turn in which no reviewer
ran, on both channels, for three independent reasons. The check goes above all of them — above
`set_active` at `src/tools.rs:1736`, above the Perforce `marker_state` read and `mark_pending` at
`src/tools.rs:1757-1762`, and above `vcs::capture` at `src/tools.rs:1792`. It needs nothing from any
of them: it reads `self.prior_findings`, which the job already carries into `run`. What follows:

- No capture is taken for a review that will not run. Today the server runs a full `git diff` /
  `p4 describe` — the expensive part of a turn — and discards it.
- No entry is published as active, so `reviewer` is genuinely absent rather than suppressed. The
  field is `Option<&str>` in `ResultContext` and `string | null` on the wire, and the renderer's
  chain-description fallback is made conditional on a turn having run — otherwise the fallback
  re-introduces the false attribution the move exists to remove.
- No Perforce `.pending` marker is written, so the compensating `clear_pending` in `attempt`
  (`src/tools.rs:2548-2553`) is deleted rather than kept. The round-4 draft flagged this ordering as
  "to confirm during implementation"; the control review confirmed it, and the answer is that the
  marker is written at `src/tools.rs:1761`, which the check now precedes.
- No `turn_ran()`-conditional attachment and no suppression branch. Less code than any earlier draft.

**It is a branch, not an early return.** The control review's finding `f7` caught what a naive
relocation would have cost: `run` holds a `FinishGuard` whose `Drop` records
`Outcome::failed(worker_panicked)` while armed (`src/tools.rs:1633`), and the normal path disarms
only after `registry.finish` (`src/tools.rs:2109-2110`). A bare `return` from the top of `run` would
therefore have replaced the `ledger_too_large` envelope with `WORKER_PANICKED` — turning a clean,
actionable "rebaseline this session" into a spurious crash report, which is worse than the false
`captured:` line the move exists to remove.

So the check does not return. It sets the outcome and **skips the capture and the reviewer walk**,
falling through to the same tail `run` already has: `registry.finish`, `guard.armed = false`, and the
metrics record. Nothing about finishing, disarming, panic safety or usage accounting becomes a second
path that has to be kept in step with the first — which is also why the guard stays above the check
rather than being moved below it, so a panic in the check itself is still caught. The
`set_terminal_reason` persist moves with the check; the `clear_pending` compensation is deleted
because there is no longer a marker to clear.

**Falling through that tail is not free, and three things in it assume a turn ran.** The round-6
draft asserted the branch "keeps those unreached" and was wrong; the control review's finding `f9`
names each, and the plan takes its list rather than its own reading:

- **`outcome.active` is assigned unconditionally in the tail** (`src/tools.rs:2085`), *after* the
  walk — so skipping the walk does not skip it. The assignment moves inside the turn-ran branch, and
  `active_bin_resolved` is `false` on the no-turn path, so no entry is attributed. This is the same
  mistake round 4 made about the same field, now in its third location: `set_active` (:1736),
  `Registry::finish` (`src/registry.rs:490`), the render fallback (:1137), and now the tail. All four
  have to be handled, and the regression test asserts the *rendered outcome*, not any one of them.
- **The metrics call reads the capture** (`capture.change.as_ref()` at `src/tools.rs:2122`, plus the
  `disposition_tag` and `captured_tag` derived from it). With no capture taken there is no binding to
  read, so the no-turn path binds neutral values — no change, no tags — and still writes a record.
- **`resolved_bin`** follows `active_bin_resolved` and is therefore `None`.

The record is still written: a turn that was refused before it ran is worth one line in the usage log
precisely because it burned nothing, and dropping it would make "no record" ambiguous between
"refused" and "never called".

`resumed` and `resumable` are still reported on that path — each is a true fact about the call
regardless of whether a reviewer ran — and `usage` and `denials` are empty there already.
`Envelope::turn_ran()` survives only as the named reading of `review_prose.is_some()` for §3's rule
and for §6; if the implementation finds no second caller, it is dropped rather than kept for symmetry.

The regression test asserts all of it on the over-budget-on-entry path: `captured`, `disposition` and
`reviewer` are `null` on both channels, the text renders none of those three lines, no capture
command runs, and no Perforce `.pending` marker is left behind.

## 5. Decision 3 — one constructor builds both channels

Parity has to be structural or it will rot. Today `render_completed` and
`snapshot_structured_content` are two functions over the same `Snapshot`, called separately from
`review_result_both`, with no shared source.

Round 1 (finding `f2`) rejected the first draft of this decision, which exposed
`out_block(value: &Value, nonce: &str)`: a public helper taking an arbitrary `Value` can still be
handed a partial or stale object, so "`completed_value` is the only wire path" would have been a
convention rather than a guarantee — weaker than the `Envelope`-bound `to_out_block` it replaced. The
corrected form emits both channels from one call and never accepts a loose `Value`:

```rust
// src/findings.rs — plain data, no I/O types
pub struct ResultContext<'a> {
    pub reviewer: Option<&'a str>,
    pub resumed: bool,
    pub resumable: bool,
    pub usage: Option<&'a str>,
    pub captured: Option<&'a str>,
    pub disposition: Option<&'a str>,
    pub run_warnings: &'a [String],
    pub denials: &'a [String],
    pub denial_count: usize,
    pub denial_count_is_floor: bool,
}

/// Both channels of a completed result, built together from one envelope and one context so they
/// cannot disagree. The only public path to the completed wire format.
///
/// The fields are private and there is no constructor but `completed_result`: with them public, a
/// caller could replace one and leave the other, so "identical" would have been true only at the
/// moment of construction (focused control review, `f2`). The accessors hand out the value and a
/// block rendered from that same value, so identity holds for the lifetime of the object rather
/// than for an instant.
pub struct CompletedResult {
    value: Value,
    out_block: String,
}

impl CompletedResult {
    pub fn value(&self) -> &Value;
    pub fn out_block(&self) -> &str;
}

pub fn completed_result(env: &Envelope, ctx: &ResultContext, nonce: &str) -> CompletedResult;
```

- `Envelope::to_structured_value` becomes private (`fn core_value`) and `Envelope::to_out_block` is
  removed. No public function takes a caller-supplied `Value`, so production cannot emit a poorer or
  divergent object by accident. Tests that need the bare envelope use `completed_result` with a
  `ResultContext::empty()` helper.
- `src/tools.rs` replaces `render_completed` + the completed arm of `snapshot_structured_content`
  with one `render_completed_both(&Snapshot) -> (String, Value)` that builds the `ResultContext`
  once, renders the text from it, calls `completed_result`, appends `out_block` after the whole-body
  `strip_marker_lines` sweep as today, and returns that same `value`.
- `review_result_both` uses both halves; the test-only `review_result` uses `.0`.

**What this guarantees is exactly the table in §2, and no more.** `completed_result` binds the two
machine channels; the human header is still assembled by hand in `render_completed_both`, so a
context field added later could reach the `Value` and be omitted from the text. That is outside the
invariant by §2's third row, not a hole in it. §9 test 11 keeps it honest by iterating the object's
keys rather than naming the fields it expects, but it is a test, not a guarantee, and the plan does
not describe it as one.
- The `Running` and `Failed` arms are unchanged. The running variant does not gain the context group
  — it has no envelope, its progress group is already the parity fix for that variant, and adding
  more would be scope with no reported failure behind it. `running_out_block` is already typed and
  stays as it is.

### 5.1 Marker neutralisation happens once, at the source

Round-1 finding `f7`. The text body is swept by `strip_marker_lines` (`src/tools.rs:1228`), which
*deletes* any line beginning with a `_IN`/`_OUT` sentinel; the structured value would carry the same
strings raw. So prose or a warning containing a marker-looking line would differ between the two
channels — the parity promise broken by the mechanism that protects the text channel.

Resolution: apply `strip_marker_lines` **once, where each untrusted string enters the result** — the
prose (in `finalize_turn`, before it reaches the envelope, after `strip_reviewer_block` has removed
the reviewer's own nonce-bearing block) and **every** `ResultContext` string without exception:
warnings, denials, `captured`, `disposition`, `usage`, `reviewer`. Both channels then carry
identical, already-neutralised values.

Rounds 2 and 3 held `f7` open, and the drafts' weakness was a list with judgement in it: they swept
the strings that looked untrusted and waved at the rest as "covered by the whole-body sweep". Every
string on the wire is swept now, whether or not it looks reachable, because the alternative is an
argument per field that has to be re-made whenever a field is added. The two fields the earlier
drafts hand-waved are also settled here rather than left to the sweep:

- **`session`** cannot form a marker line: `src/tools.rs:438` rejects a session name containing any
  control character, so it can never contain a newline, so it can never start a line of its own.
  The whole-body sweep over it stays as defence in depth, exactly as its comment says, and the
  structured `session` needs no treatment. This is an argument from a validated input, not from a
  rendering accident.
- **`review_id`** is server-minted (`rv-<n>-<n>`), so the same holds trivially.

Four properties of the move, since the exception has to be stated precisely rather than gestured at:

- **The text channel's guarantee is not weakened.** The whole-body sweep in `render_completed_both`
  stays exactly where it is, immediately before the one canonical `_OUT` block is appended. Over the
  pre-swept fields it is now idempotent; over everything else the body assembles — the session name,
  the framing prose, the `review_id` — it is doing exactly what it did. The guarantee that a client
  parses exactly one nonce-bearing `_OUT` block, and that it is the server's, is unchanged, because
  it was never the per-field pass that provided it.
- **The pre-sweep is strictly stronger than the whole-body sweep, not weaker.** `strip_marker_lines`
  matches on `trim_start().starts_with(…)`, so a marker at the head of a warning survives the
  whole-body pass today (in the body it sits after the `WARNING: ` prefix, mid-line) and does not
  survive the per-field pass (in isolation it starts its line). More is neutralised, never less, and
  both channels see the same result. A marker on a *second* line of a multi-line field is caught by
  either pass identically.
- **Nothing depended on the raw form.** The structured copy was inert regardless — JSON escaping
  renders every embedded newline as `\n` inside one string value, so a lookalike marker there could
  never form its own line (Decision B's original analysis, unchanged). The pre-sweep is for parity,
  not for safety, and this plan does not claim otherwise.
- **It is defined for every shared string, not for a chosen subset**, which is the difference
  between an exception and a rule. `ResultContext` is constructed in one place; the sweep is applied
  there, to every string field it holds, so a field added later gets it without anyone remembering.

**Two shared strings are not `ResultContext` fields, and the round-4 draft missed both** (control
review, finding `f4`). The rule is therefore stated as *every string that reaches the wire is swept
where it is composed*, which covers three composition points rather than one:

- **The prose**, in `finalize_turn` — which now includes the repair notes (§3.1), whose text comes
  straight from the reviewer's repair response (`src/tools.rs:2862-2869`) and is as untrusted as the
  prose itself.
- **The warning union**, where it is built. The envelope's own warnings are not caller-supplied but
  they do embed reviewer-controlled content: `describe_reconcile` interpolates finding ids taken from
  the reviewer's block (`src/findings.rs:1471-1481`). Sweeping the union at its one composition point
  covers both halves without an argument about which half is reachable.
- **`ResultContext`**, as above.

§9 test 8 asserts it on prose, a repair note, an envelope warning, a run warning, a denial,
`captured` and `disposition` — the earlier drafts tested prose alone.

`ENVELOPE_SCHEMA_VERSION` goes 2 → 3. `LEDGER_SCHEMA_VERSION` is untouched — this is exactly the
split those two constants exist for, so no ledger on disk is marked foreign and no in-flight session
is refused a resume. `output_schema()`'s completed branch gains every new key as `required`, keeping
`additionalProperties: false`; the `oneOf` disjointness with the running branch is preserved and in
fact widened, since every new key sits on the completed side.

## 6. Decision 4 — the not-durable fallback stops discarding the turn's warnings

Round-1 finding `f4`, another live bug the plan would otherwise have carried forward. When a turn
cannot be recorded, `src/tools.rs:3205-3228` discards `turn_eval.envelope` and rebuilds from
`not_durable_envelope`, whose `warnings` contain only the generic durability sentence
(`src/findings.rs:1621-1625`). Everything `finalize_turn` had recorded for that turn — the verdict
contradiction warning, the block-repair narrative (`src/findings.rs:1386-1398`) — is dropped on the
floor, on the one path where the caller is being told to reconstruct the turn by hand.

`not_durable_envelope` gains a `carry_warnings: &[String]` parameter; the durability warning stays
first (it is the actionable one) and the turn's own warnings follow. Since §4's union puts the
envelope's warnings ahead of the run warnings, the durability sentence is first in the rendered list
too, on both channels. `block_repair` is already carried across at the call site and stays. Tested on
the not-durable path specifically.

### 6.1 Evaluation warnings describe observations; the envelope's fields report the disposition

Carrying the warnings surfaces a wording problem that predates this plan, and round-2 finding `f9`
plus the control review's finding `f3` between them show it is not one string but a class. Every
warning `finalize_turn` and `resolve_structured` produce asserts a *disposition* alongside its
observation:

- "…it was asked once more and supplied one, **so this turn is structured**" (`src/findings.rs:1392-1394`)
- "reviewer marked verdict approve but N finding(s) are still open; **treated as changes**" (`src/findings.rs:730-741`)
- "reviewer requested changes but named no open findings; **treated as changes**" (`src/findings.rs:736-741`)

On the ordinary path each trailing clause is redundant — `structured` and `verdict` already say it.
On the not-durable path each is a flat contradiction inside the same object, which reports
`structured: false`, `verdict: unknown` and `outcome: rebaseline`.

So the rule, applied to all of them at their source rather than rewritten on carry: **an evaluation
warning states what was observed; the envelope's fields state what was made of it.** Drop the
disposition clauses. "reviewer marked verdict approve but 3 finding(s) are still open" is true on
every path, and a caller that wants the disposition reads `verdict`, which is the field whose whole
job that is. No conditional wording, no rewrite-on-carry logic, no per-path variants — three strings
change, and the result is more accurate on the ordinary path too.

§9 tests the not-durable path with both a recovered repair and a verdict contradiction in flight, and
asserts no carried warning contradicts the envelope it now sits in.

## 7. What this deliberately does not do

- No typed `captured`/`disposition` objects (§4).
- No second or harder prose cap; `MAX_ENVELOPE_PROSE_CHARS` stays 16,000 (§3).
- No context group on the running variant (§5).
- No de-duplication of overlapping warnings (§4).
- No new metrics fields. `src/metrics.rs` not recording per-turn `outcome` is what made §1.3
  unprovable, and it is a real gap — but it is a *different* gap, with no reported failure behind it,
  and adding it here would be the ratchet `AGENTS.md` warns about. Worth its own issue, not this PR.
- **No removal of the prose from the `_OUT` text block**, even though the text body then carries the
  prose twice (once between the review markers, once inside the JSON). "Both channels carry the
  identical value" is load-bearing: a client that parses `_OUT` out of the text must not get a poorer
  envelope than a `structuredContent` client. The duplication is bounded by the cap. This is the one
  place where the plan knowingly pays bytes for the invariant.
- No change to how findings, verdicts, convergence, coverage, or the ledger work. Nothing here reads
  the prose for meaning: it is transport, and `verdict_source` stays `structured | none`.

## 8. Files

| File | Change |
| --- | --- |
| `src/findings.rs` | delete `with_prose`; prose as a constructor argument; `turn_ran()`; `TurnAssessment::push_repair_note`; `TurnEvaluation::envelope_prose`; `not_durable_envelope` carries warnings; drop the disposition clauses from the three evaluation warnings; `ResultContext`; `CompletedResult` / `completed_result`; `core_value` private; marker neutralisation at each composition point; `output_schema()` completed branch; `ENVELOPE_SCHEMA_VERSION` → 3; truncation-note wording |
| `src/tools.rs` | `render_completed_both`; build `ResultContext` once (sweeping every string); render the unioned warnings (envelope first); hand repair notes to the assessment before finalize; pass `envelope_prose` and carried warnings to `not_durable_envelope`; **move the over-budget-on-entry check out of `attempt` to the top of `run`, above `set_active` (:1736), `mark_pending` (:1761) and `vcs::capture` (:1792)**, deleting the `.pending` compensation and making the `reviewer:` chain-description fallback conditional on a turn having run; `review_result_both` / `review_result` wiring |
| `src/mcp.rs` | tool description: prose is present whenever a turn ran; one sentence naming the context group |
| `docs/structured-findings-envelope.md` | the authoritative envelope contract: completed-variant schema, `review_prose` semantics (§364-380), the widened `warnings` contract (§545-546), and the envelope version (§550-556) |
| `docs/unstructured-turn-recovery.md` | a superseding note on Decision B's table pointing here (the document stays as the record of #63) |
| `README.md` | "Reading a completed result" — the new fields and the parity rule |
| `AGENTS.md` | delete the "You may not be able to read the reviewer's prose" bullet, which this closes |
| `smoke.ps1` | assert `structuredContent.review_prose` is non-null on a real completed turn, and that the context keys are present |

## 9. Tests

Pure unit tests in `src/findings.rs` unless noted.

1. **Prose over the whole outcome matrix** — converged, `changes_requested` (open findings),
   `escalate` via `approve_with_comments`, `escalate` via `blocked`, `changes_requested` via verdict
   contradiction, degraded, `rebaseline` via `turn_not_durable`, `rebaseline` via
   `ledger_unavailable`: `review_prose` is `Some` in every one. Over-budget-on-entry: `None`.
2. **The issue's acceptance assertion**, stated directly: no completed envelope from a turn that ran
   carries `outcome: escalate` (or `rebaseline`) with `review_prose: null`.
3. **Empty prose stays `Some("")`**, distinct from the no-turn `None`; `turn_ran()` agrees with both.
4. **Repair notes are in the prose the envelope carries**, not only in the text body (§3) — asserted
   under the cap, on an over-cap prose (where the round-3 draft would have dropped the note), and on
   the **not-durable** path with an over-cap prose, which is the composition the round-4 draft still
   got wrong (§3.1).
5. **The not-durable envelope carries the turn's own warnings**, durability warning first, and no
   carried warning contradicts it — run with a recovered repair, with an `approve`-but-open-findings
   contradiction, and with the zero-open `request_changes` contradiction, since those are the three
   that used to assert a disposition (§6.1).
6. **Truncation boundary** — existing tests preserved (exactly at the cap is not truncated, one over
   is), plus the reworded note's content.
7. **The prose composition across the three copies** (§3.1) — under the cap, the structured copy is
   the full prose plus the notes; over it, the capped head plus the truncation note plus the notes,
   with `review_prose_truncated` set; and the two machine channels are byte-identical in both cases.
   `CappedProse` carries the flag on every path, including the not-durable one, which used to
   hardcode it `false`.
8. **Marker neutralisation parity, at all three composition points** — prose, a repair note, an
   envelope warning, a run warning, a denial, `captured` and `disposition`, each containing a literal
   `_IN`/`_OUT` marker line: the structured value and the text body carry the same neutralised
   string, and the body still yields exactly one parseable block bearing the result's nonce (§5.1).
   The last part is an existing property tested on a genuinely new case, since prose now rides the
   `_OUT` block on turns where it never did.
9. **Schema/value key parity, both directions** — every key `completed_result` emits is declared and
   required by `output_schema()`, and every required key is emitted.
10. **Running/completed disjointness** — unchanged, re-asserted against the widened completed branch.
11. **The denial count's floor case survives both channels** (`src/tools.rs`) — a snapshot with
    `denial_count_is_floor` set renders "at least N" in the text and carries both `denial_count` and
    the flag on the structured channel, so a client cannot read the count as exact when it is not.
12. **Text/structured parity, by iteration not by enumeration** (`src/tools.rs`) — a snapshot with
    every context field populated: the test walks the keys of the structured object and asserts each
    is represented in the rendered text, rather than naming the fields it expects, so a field added
    later without a rendering fails here (§5). It also asserts the `warnings` array matches the
    `WARNING:` lines in order.
13. **`_OUT` identity** (`src/tools.rs`) — `review_result_both` returns a text whose `_OUT` block
    parses to a value identical to the returned `structuredContent`.
14. **No-turn result reports no attribution, takes no capture, leaves no marker** (`src/tools.rs`) —
    the over-budget-on-entry path reports `captured: null`, `disposition: null` and `reviewer: null`
    on both channels, the text body renders none of those three lines (including via the chain
    description fallback), no capture command is run, and no Perforce `.pending` marker is left
    behind (§4.1).
15. **The no-turn branch still finishes normally** (`src/tools.rs`) — the over-budget-on-entry path
    delivers its `ledger_too_large` envelope and **not** `WORKER_PANICKED`, the `FinishGuard` is
    disarmed, `terminal_reason` is persisted, and a metrics record is written with no capture tags
    and no `resolved_bin` (§4.1). This is the regression the control review's `f7` predicted for a
    naive early return, and the tail state its `f9` named.

## 10. Verification

- `.\build.ps1` — fmt, clippy `-D warnings`, unit tests, release build, restage.
- `smoke.ps1 -Reviewer claude` — this changes the response envelope, which is protocol, so the real
  round trip is required rather than optional. It calls a model for real and costs tokens. The
  evidence service is untouched, so the Codex-only evidence assertions are not what is at stake here;
  a `-Reviewer codex` run is worth adding if any evidence-path code moves, and this plan moves none.
- CI's `CLI_NOT_FOUND` contract check is unaffected.

## 11. Cost

Every completed response grows by up to the prose cap on each of two channels (the structured value
and the `_OUT` block), plus a small fixed context group. For a typical 2–8 KB review that is a few
kilobytes per channel on turns that previously carried none. That is the price of the invariant, and
it is paid on the turns that dominate a session — stated here plainly so it is a chosen trade and not
a surprise.

## 12. Review history

**Round 1** (`rv-28052-1`, 7 findings, all accepted, none disputed):

| # | Finding | Resolution |
| --- | --- | --- |
| `f1` | §1 did not establish that the #71 rounds 5–6 were `changes_requested` | §1 rewritten. The forensic claim is withdrawn as unprovable (§1.3) and the argument now rests on the axis-collapse argument (§1.1) and `AGENTS.md`'s standing workaround (§1.2). |
| `f2` | `out_block(&Value)` left the parity seam generic | Replaced by `completed_result(env, ctx, nonce) -> CompletedResult`, which emits both channels from one call; no public function takes a loose `Value` (§5). |
| `f3` | Block-repair notes would stay text-only | `TurnAssessment::push_repair_note`; notes fold into the prose before `finalize_turn` (§3). |
| `f4` | The non-durable fallback drops turn-evaluation warnings | New §6: `not_durable_envelope` carries them. |
| `f5` | `docs/structured-findings-envelope.md` was missing from the doc changes | Added to §8 with the specific sections to update. |
| `f6` | The no-review over-budget path reports false context facts | New §4.1. Confirmed as a live bug, not just a hazard: `run` overwrites the deliberate `None`s. |
| `f7` | Marker sanitisation would break the parity promise | New §5.1: neutralise once at the source, both channels carry the same neutralised value. |

**Round 2** (`rv-28052-2`): `f2`, `f3`, `f4`, `f5` confirmed resolved. `f1`, `f6`, `f7` held open, and
two new findings. Nothing was disputed; each held finding was taken back to the code, and each turned
out to have a residual the round-2 draft had left:

| # | What was still wrong | Resolution |
| --- | --- | --- |
| `f1` | §1.3 still carried a bounded inference about the #71 rounds, and §1 still led with the forensics | The inference is deleted outright (`AGENTS.md`: answering a finding by removing what made it reachable is a legitimate resolution). §1 now states up front that the argument rests on the invariant, not on any turn's `outcome`. |
| `f6` | The round-2 draft kept `reviewer`/`active`, which the finding had named — a partial dismissal presented as a decision | `reviewer` is now `string \| null` and suppressed on a no-turn result, with the reasoning re-derived from `src/tools.rs:2081-2089` rather than from the draft's own argument (§4.1). |
| `f7` | The exception was asserted but not stated precisely, and the only marker test covered prose | §5.1 now states three specific properties, including why the pre-sweep is strictly stronger than the whole-body pass; §9 test 8 covers warnings and denials too. |
| `f8` | The 16,000-character cap makes "both channels carry the identical string" false at the boundary | New §3.1 replaces it with the claim that is actually true and testable: the two machine channels are byte-identical to each other, and the capped copy is a prefix of the full prose plus the note. The consequence for repair notes at the tail is stated, not engineered around. |
| `f9` | A carried `recovered` warning asserts "this turn is structured" on a `structured: false` envelope, and §4's union order contradicted §6's "durability first" | The warning is reworded at source to claim only what the repair established (§6). The union order is flipped to envelope-warnings-first, which makes §6 true by construction (§4). |

**Round 3** (`rv-28052-3`): `f1`, `f8`, `f9` confirmed resolved. `f3` **regressed**. `f6` and `f7`
held open for a third time. Nothing disputed.

| # | What was still wrong | Resolution |
| --- | --- | --- |
| `f3` (regressed) | §3.1 had answered `f8` by *documenting* that an over-cap prose drops its repair note from the structured copy — which is `f3` (one prose value reaching both channels) coming back as a caveat | §3.1 rewritten: the cap applies to the reviewer's prose, and the separately-bounded repair notes are appended **after** it, so the note is unlosable. My round-3 question "is stating it the right call?" got its answer, and it was no. |
| `f6` | Rounds 2 and 3 both took the finding's *first* option (suppress the fields) and both were held | Take the second: move the over-budget check ahead of the capture in `run`. The false facts stop existing rather than being masked, an expensive capture for a review that never runs is no longer taken, the `.pending` compensation is deleted, and there is less code than the round-3 draft had, not more. |
| `f7` | The drafts swept a *judged subset* of strings and hand-waved the rest onto the whole-body pass; the only marker test covered prose | Every `ResultContext` string is swept, at the single place the context is built. `session` and `review_id` are settled by argument from validated input (`src/tools.rs:438` rejects control characters) rather than left to the sweep. Test 8 covers five fields. |

**Round 4** (`rv-28052-4`): no status changed on any of the nine findings — `f3` still regressed,
`f6` and `f7` still open, despite substantive edits to all three. Rather than dispute a third time,
`AGENTS.md`'s prescribed instrument was used: a **fresh control review** (`rv-28052-5`, session
`issue-73-parity-plan-control`, `fresh: true`), given the plan and the code but not the prior
verdicts.

It independently raised the same three concerns, which per `AGENTS.md` is strong evidence they were
real and the fixes incomplete — and it was sharper than the resumed session about why. Every one of
its six findings was accepted; three are corrections to claims this document made about the code, and
the plan was wrong in each case:

| Control finding | What it established | Resolution |
| --- | --- | --- |
| `f1` (= `f6`) | "Ahead of the capture" was insufficient **three times over**: `run` publishes `active` at `src/tools.rs:1736`, *before* the capture; `Registry::finish` preserves it when `Outcome.active` is `None` (`src/registry.rs:490-493`); and the renderer falls back to the chain description regardless (`src/tools.rs:1137-1140`) | §4.1 rewritten. The check moves above `set_active`, `mark_pending` and `vcs::capture`; `reviewer` becomes `Option<&str>`; the fallback is made conditional. |
| `f2` (= `f3`) | The not-durable path builds its own envelope from `turn_eval.review_prose`, so the §3.1 composition would either blow the cap there or re-cap and truncate the very notes it protects | `TurnEvaluation::envelope_prose` carries the composed structured copy; `not_durable_envelope` takes it and caps nothing. Tested with an over-cap note on that path. |
| `f3` (= `f9`, wider) | The contradiction is a class, not one string: `resolve_structured`'s "treated as changes" warnings contradict a not-durable envelope too | New §6.1 states the rule — evaluation warnings describe observations, the envelope's fields report the disposition — and drops the disposition clause from all three. |
| `f4` (= `f7`, wider) | Two shared strings are not `ResultContext` fields: repair-note text (`src/tools.rs:2862-2869`) and envelope warnings, which embed reviewer-controlled ids (`src/findings.rs:1471-1481`) | §5.1's rule restated as "every string that reaches the wire is swept where it is composed", naming all three composition points. |
| `f5` | `review_id` and `elapsed_seconds` are disproportionate: schema surface, rendering, sanitisation, tests and bytes for nothing in the false-approval column | **Both cut.** The group is nine keys, not eleven. |
| `f6` | `completed_result` binds the two machine channels but not the hand-written human header | §5 narrows the structural claim to machine-channel identity and adds test 11, which iterates the value's keys instead of naming them. Held again in round 5; see below. |

**Round 5** (`rv-28052-6`, control session turn 2): `f1`–`f5` confirmed resolved. `f6` held, and two
new findings — both of which would have broken the `f1` fix in implementation.

| # | What was still wrong | Resolution |
| --- | --- | --- |
| `f6` (held) | §5 had narrowed its own claim, but §2's headline invariant still asserted both directions with one mechanism, so the document as a whole still overclaimed | §2 now carries a three-row table separating what is guaranteed by construction from what is enforced by test, and says plainly that the third row is the weaker of the two. §5 defers to it rather than restating it. |
| `f7` | The relocated check would return from `run` while the `FinishGuard` is armed, so `Drop` (`src/tools.rs:1633`) would record `WORKER_PANICKED` in place of the `ledger_too_large` envelope — the normal path disarms only after `registry.finish` (`src/tools.rs:2109-2110`) | §4.1: it is a **branch, not an early return**. The check sets the outcome and skips the capture and the walk, falling through to the existing `finish` / disarm / metrics tail, so none of those become a second path. Test 14. |
| `f8` | `not_durable_envelope` hardcodes `review_prose_truncated: false` (`src/findings.rs:1627-1630`), so handing it a pre-capped string would report a truncated prose as complete | `cap_prose` returns `CappedProse { text, truncated }`, and every constructor sets both fields from it. The flag stops being something a call site can forget. |

**Round 6** (`rv-28052-7`): `f7` and `f8` confirmed resolved. `f6` held a fourth time, and one new
finding — which answered a question this document had answered wrongly for itself.

| # | What was still wrong | Resolution |
| --- | --- | --- |
| `f6` (held ×4) | The drafts kept *qualifying* the symmetry instead of dropping it. §2's invariant still asserted both directions as an invariant, with the table as a footnote | The invariant is now **one-directional**: the structured channel is never strictly poorer than the text body, full stop. The reverse is explicitly **not part of it** — a tested property, not a guarantee. This is also the exact form the round-13 review settled on for the running variant, so the plan stops inventing a stronger claim than the repository's own precedent. |
| `f9` | Round 6 asserted the no-turn branch "keeps the turn-dependent tail unreached". It does not: `outcome.active` is assigned unconditionally in the tail at `src/tools.rs:2085`, and the metrics call reads the capture at `src/tools.rs:2122` | §4.1 lists all three: the `active` assignment moves inside the turn-ran branch, `active_bin_resolved` is `false`, and the metrics call binds neutral capture values. `active` has now needed handling in **four** separate places, which is why the test asserts the rendered outcome rather than any one of them. |

**Round 7** (`rv-28052-8`): `f9` confirmed resolved; `f6` held a fifth time, its status never having
moved off turn 1 while every other finding in the session moved when addressed. Rather than dispute
or guess a fourth remedy, a **second fresh control review** was run against §2 and §5 alone
(`rv-28052-9`, session `issue-73-parity-f6-control`, `fresh: true`), asking only whether the
invariant was honest, whether keeping the reverse direction outside it was right, and whether
`completed_result` really made divergence impossible.

It **endorsed the §5 decision explicitly** — keeping the reverse direction as a tested property is
sound "because the human body intentionally contains prose, framing, and next-step guidance not
represented by machine JSON" — and then found the invariant overclaiming for a reason no round had
named:

| # | What was wrong | Resolution |
| --- | --- | --- |
| `f1` | The invariant said "never strictly poorer", while §3.1 accepts that an over-cap prose is available in full only on the text channel. The document contradicted itself, and the contradiction sat in its headline claim | §2 now says **never *silently* poorer**: every weighing-relevant fact in full, the prose up to a declared bound, and `review_prose_truncated` on the wire so a short copy announces itself. A bounded, announced shortfall is a different thing from the silent one issue #73 is about. A fourth table row states the prose claim separately. |
| `f2` | `CompletedResult`'s public `value` and `out_block` fields mean a caller can replace one and keep the other, so "identical" held only at construction | Fields private, no constructor but `completed_result`, accessors only. Identity now holds for the object's lifetime rather than for an instant. |

So `f6` was right for five rounds, and about a sentence none of my four attempts had touched — every
one of them rewrote the *symmetry* while the actual overclaim was the prose cap.

**Round 8** (`rv-28052-10`): `f6` confirmed resolved, and with it every finding from rounds 1–7. One
new minor finding:

| # | What was wrong | Resolution |
| --- | --- | --- |
| `f10` | §4's table called `denial_count` "the exact total". It is not: `src/registry.rs:693-696` allows it to be a lower bound when the source output was capped, which the text already renders as "at least N" (`src/tools.rs:1178-1184`) | The table says exact **only when `denial_count_is_floor` is false**, and describes what the flag means. Test 11 covers the floor case on both channels — a structured client must not read a floor as a total, which is the same "thinner than it looks" failure the whole context group exists to prevent. |

**Round 9** (`rv-28052-11`): **converged** — `outcome: converged`, `verdict: approve`,
`open_count: 0`, all ten findings resolved. The plan is approved for implementation.

### What the nine rounds cost, and what that says

Ten findings over nine rounds and three sessions. Worth recording honestly, because `AGENTS.md` asks
for exactly this judgement and the answer is not flattering in one direction or the other:

- **Six of the ten were the plan being wrong about the code**, not about design taste — `f1`, `f2`,
  `f7`, `f8`, `f9` and `f10` each cited a line that contradicted something this document asserted.
  Three of them (`f7`, `f9`, and the four separate `active` sites in `f1`) would have shipped a
  regression worse than the bug being fixed. That is the gate doing the job it exists for.
- **The plan got smaller twice** — `review_id` and `elapsed_seconds` cut, the §4.1 suppression logic
  replaced by a check that moves — and larger nowhere except in tests and precision of claim. The
  ratchet `AGENTS.md` warns about did not happen here, and the reason it did not is that two findings
  argued for removal and both were taken.
- **`f6` cost five rounds because I answered the clause I expected to be wrong** rather than reading
  the claim against the rest of the document. Four rewrites of the symmetry sentence; the actual
  overclaim was the prose cap, two sections away. The lesson is the one `AGENTS.md` already states —
  go back to the code, and to the whole document, rather than to your reasoning about the part you
  suspect.
- **Every round returned `review_prose: null`**, so no reviewer reasoning was ever readable. Finding
  the real defect behind `f6` took a separately-billed focused review that existed only to ask "what
  did you actually mean". That is this issue's bug, paid for nine times during its own fix.

Rounds 1–4 each demonstrated the bug under discussion: every one returned its findings with
`review_prose: null`, so the reviewer's reasons for holding `f1`, `f6` and `f7` were never readable,
and each residual had to be found by re-reading the code or by paying for a fresh review. Four rounds
of this plan's own review have been spent partly on the defect the plan exists to fix.

## 13. Implementation review

Session `issue-73-parity-impl`, three rounds, **eight findings, all accepted, none disputed**, ending
`converged` / `approve` with zero open (`rv-28052-14`).

| # | Finding | Resolution |
| --- | --- | --- |
| `f1` | `warning_union` copied `env.warnings` raw, so a reviewer-controlled finding id carrying a marker line (via `describe_reconcile`) was swept out of the text body and kept on the structured channel | §5.1's rule was implemented in one of the two places it names. `warning_union` now sweeps everything it returns, both sources. |
| `f2` | The structured channel carried every retained denial while the text printed ten, and the text normalised a missing count that the structured channel emitted raw — the same two fields meaning different things per channel | One bounded list (`DENIAL_EXAMPLES`) and one reconciled count, computed once and rendered by both. |
| `f3` | The no-turn regression test built an envelope by hand and never called `Job::run`, so it could not see capture avoidance, marker ordering, guard disarming, terminal persistence or metrics | Rewritten to construct a real `Job` with a 600-finding ledger and call `run`. No reviewer CLI is involved, because the refusal precedes the walk. |
| `f4` | The over-cap prose test never compared the `_OUT` body with the structured value — the only identity test used a short fixture | Both the evaluated and not-durable envelopes are now compared at over-cap size, which is the input that would break the claim. |
| `f5`, `f8` | Two stale `schema_version: 2` references in `unstructured-turn-recovery.md` | Corrected, and Decision D now notes that a second wire bump with no ledger marked foreign is that decision paying off again. |
| `f6` | The text/structured parity test searched the whole body — including the appended `_OUT` block, which is a serialisation of the object under test, so it could pass on the JSON alone | Splits at the `_OUT` marker and searches only the human prefix, plus asserts the split shortened the text so the check cannot silently become a tautology again. |
| `f7` | The no-turn test used git only, never passed a resume id, and did not inspect metrics — so it could not catch a Perforce marker regression or wrong accounting | Two more tests: a `--vcs perforce` run asserting `MarkerState::Absent` (the direct regression test for the deleted `clear_pending` compensation), and one asserting exactly one metrics record with `resumed: true` and no capture, disposition or resolved-binary attribution. |

Three of the eight were tests that did not test what they claimed — `f3`, `f6` and `f7` — which is
worth recording separately from the code findings. A test that asserts the shape of a hand-built
fixture, or that searches a haystack containing its own needle, reads exactly like coverage.

### Verification

- `build.ps1`: `cargo fmt --check`, clippy `-D warnings`, **729 unit tests**, release build. Only the
  `dist\` restage is blocked, by this session's own MCP server holding the binary — which `build.ps1`
  reports by design.
- `smoke.ps1 -Reviewer claude`: passed end to end, twice (before and after the last two review
  rounds). A **converged** turn returned `review_prose: "SMOKE-OK\nCOUNTER=1"` at
  `schema_version: 3`, with `captured`, `resumable`, `reviewer`, `usage`, `disposition` and a real
  capture warning all on the structured channel — every one of which the same turn would have
  withheld before this change.
