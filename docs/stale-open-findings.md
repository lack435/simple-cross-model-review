# Stale `open` findings on a resumed session — design

Status: **proposal, revision 5 — paused.** Filed against issue #62 ("Resumed review session freezes
findings at a stale `open` status (ledger not re-evaluated)").

**Why it is paused, in one line: the design was over-engineering itself, and stopping was the
correction.** Not a dependency, not a blocked prerequisite — the plan was growing a schema bump, a
migration and a compatibility type to fix a reviewer that restates `open` without looking. Revision
5 is the cut-back. Read the blockquote below before anything else in this file; it is the point.

**Two more things to know before picking this up.** Revisions 1–4 were reviewed through this
repository's own gate over six rounds; **revision 5 has not been reviewed at its current size**,
because it is strictly a subset of what those rounds covered — do not read the history below as
approval of what is specified now. And this plan **re-scopes #62 rather than closing it**: see
[What this does not do](#what-this-does-not-do), and the comment on the issue proposing the split.

> **This revision is deliberately much smaller than the four before it, and the reason is worth
> recording because it is a lesson about the process rather than the problem.**
>
> Revisions 1–4 went through six gate rounds and accumulated ten findings. Every finding was
> correct. The design still ended up roughly ten times the size of the bug: a schema version bump, a
> migration, a provenance flag, a structural invariant, a load-order specification, a private
> compatibility type, terminal resolution, a regression relation, a closed index, and amendments
> across five documents. Almost none of that was about a reviewer restating `open` without looking.
>
> Two things caused it. First, **iterating a design through a gate that optimises for rigor ratchets
> toward rigor** — each round asks "what if this edge case", each honest answer adds machinery, and
> nothing in the loop ever argues for less. Second, one early decision (bumping the ledger schema
> version) generated a chain of consequences that four of the ten findings lived inside.
>
> The maintainer supplied the missing counterweight. It outgrew this document and now lives in
> [`AGENTS.md`](../AGENTS.md) under "How much rigor, and where", which is the canonical statement —
> rigor where the blast radius is real, proportion everywhere else, and *perfect is the enemy of
> good*. It is referenced rather than restated here so the two cannot drift.

## What is actually broken

`reconcile` in [`src/findings.rs`](../src/findings.rs) is faithful: given a block saying
`{"id":"f19","status":"open"}` it persists `open`, and an id the reviewer fails to restate is
`MissingId`, which degrades the turn loudly. So where the freeze was observed, **the reviewer
actively restated those ids as `open`** and the server recorded exactly that.

The defect is in the protocol:

> `{"id":"f19","status":"open"}` is used for two different assertions — *"I re-examined this and it
> is still broken"* and *"I am restating what the ledger told me"* — and nothing can tell them
> apart. Not the server, not the caller, and, once its context has been compacted, arguably not the
> reviewer.

Two findings from dogfooding shape what follows.

**The ledger signature is not diagnostic.** While fixing #71, this repository's own gate held a
finding `open` across two rounds with siblings resolving around it — #62's exact signature — while
the caller argued twice, with quoted code and a passing test, that it was resolved. The reviewer was
right both times. It happened again on this very plan: a finding marked `regressed` at turn 4 stayed
there, and a fresh control review found the real defect the caller had missed. **Three times, the
held finding was correct and the caller's confidence was not.** Nothing here may let the caller or
the server treat a stale-*looking* finding as resolved.

**Part of the symptom was #73, and #73 is now fixed.** A structured turn used to return
`review_prose: null`, so a client surfacing only `structuredContent` never saw anything the reviewer
said outside its findings list — the entire eight-round #71 review ran without the caller reading a
line of prose. A reviewer that holds a finding open *and explains why* was indistinguishable from
one that froze it silently, so some unknown share of #62 was an unread explanation rather than an
absent one. That was the reason to do #73 first, and
[#76](https://github.com/lack435/simple-cross-model-review/pull/76) did it. See
[What has changed since the pause](#what-has-changed-since-the-pause-and-the-smaller-option-it-opens)
for what that leaves — including a step smaller than anything specified below.

## What has changed since the pause, and the smaller option it opens

#73 landed in [#76](https://github.com/lack435/simple-cross-model-review/pull/76). `review_prose` is
now unconditional on every turn that ran — attachment is enforced by construction, so `null` means
exactly "no reviewer ran" — with `captured`, `denials`, `resumable` and the merged `warnings` beside
it. A caller reading `structuredContent` alone can now read why a reviewer is holding a finding
open, if the reviewer said.

This plan named #73 as the better thing to do first. It has been done, and it does not close the
gap: prose is a place a reviewer *may* explain itself, `basis` is a question it *has* to answer.
But it changes the size of the gap from unknown-but-large to **unmeasured** — the case for revision
5 was written for a caller who could not read a held finding's explanation at all, and that caller
no longer exists.

That reopens a question the pause was about, and it deserves the counterweight rather than another
round of design:

> **Revision 5 is the cut-back from revisions 1–4. It has never been asked whether it is itself
> proportionate.**

Applying the test from [`AGENTS.md`](../AGENTS.md), "How much rigor, and where":

- **What is the blast radius?** A finding held open when it should not be **blocks** an approval; it
  does not produce a false one. By that document's own division, this is the cheap side of the line
  — a review that is lost and re-run. The one branch that points the other way is a finding
  *resolved* without a re-check, and revision 5's answer there is a warning the reviewer can satisfy
  by relabelling, which this document already concedes.
- **What does the machinery cost?** Two protocol fields, two ledger fields, a `Basis` type, a
  warning path, `output_schema` changes, eight test cases, and amendments to `README.md` and
  `docs/structured-findings-envelope.md` — the last of which means reopening Decision 2.

**So there is a smaller step that was never on the table, because #73 had not landed when these
revisions were written: change `src/prompt.rs` and nothing else.** Ask the reviewer, on every
restatement, to say whether it re-examined the finding and why it is still open. Post-#76 that
answer reaches the caller as prose, on the structured channel, with no new field, no ledger change,
no schema surface, no `output_schema` edit, no Decision 2 amendment and no new way for a turn to
degrade. It is a prompt change and a prompt test.

It buys less than revision 5: prose cannot be filtered mechanically, and a reviewer that ignores the
request leaves no machine-readable trace. Whether that matters is exactly the thing to find out —
and it is cheap to find out, because a prompt change that proves insufficient is deleted, while a
protocol field that proves insufficient is a published contract.

**Neither should be decided from the current evidence, because no review has run through the
repaired channel.** `dist\cross-review.exe` was staged 2026-08-13 15:14; the #73 fix was committed
21:49 the same day, and both MCP configs point at `dist\` — so every gate review in either
direction, including the review of #73's own implementation, ran through a pre-#73 binary. *(The
binary was restaged on 2026-08-14; the reviews to learn from are the ones after that.)*

The recommended order is therefore: **let a few real gate reviews run post-restage; then the prompt
change if the reviewers are not volunteering their basis; then this plan, if the prompt change
demonstrably is not enough.** Each step is only taken when the one before it has failed to be
sufficient, which is the opposite of how revisions 1–4 were arrived at.

## The change, if it is still wanted

Three optional fields and a warning. No new failure mode, no schema version bump, no migration.

### `basis`, and a note

`prior_findings` entries may carry two more fields:

```json
{"id": "f19", "status": "open", "basis": "rechecked", "note": "still reproduces at line 88"}
```

`basis` is `rechecked` (I examined the current state of this code on this turn) or `carried` (I did
not; I am restating what the server showed me). The prompt asks for it on every restatement, and for
a one-line `note` on anything still open — the note being the answer to "why is this still open",
whose absence cost two rounds of the #71 review.

**Both are optional in the schema even though the prompt asks for them.** A reviewer that omits
`basis` records `null`, which reads as *not stated* and counts as not affirmed. Making them required
would mean a non-conforming reviewer degrades the whole turn — a new way to lose a review, which is
the one cost this design exists to avoid.

`note` is model-authored text and is labelled as such wherever it surfaces: it carries exactly the
authority of anything else the reviewer wrote in prose, which is none the server vouches for. It is
never merged into the finding's `detail`, and it is replaced each turn.

### What the ledger keeps

`Finding` gains `last_verified_turn: Option<u32>` (the last turn it was restated as `rechecked`, or
minted) and `last_basis: Option<Basis>`, both `#[serde(default)]`. `last_status_change_turn` is
untouched — the issue's own suggestion, to re-stamp it every turn, would destroy the only record of
when a finding's state last moved while changing nothing about the behaviour.

**No `stale` field.** A caller computes it from `last_verified_turn != turn`, and the envelope
already carries `turn`. The field would need a separate wire view to keep a derived value out of the
persisted record; the caller can do the comparison instead.

### Resolving without a re-check is a warning

A block that moves a finding to `resolved` on a `carried` or absent basis produces a warning naming
the id, not a reconciliation error. The stricter version — degrade the turn and correct it through
the block-repair seam — was specified in revision 3 and dropped here: it spends a whole turn
enforcing a claim the reviewer can satisfy by relabelling anyway, and losing a review is the outcome
this design is built to avoid.

### Ledgers are disposable

**No schema version bump and no migration**, because the new fields are additive and optional: an
existing ledger loads unchanged and reports `null` for them.

More importantly, as a standing decision beyond this change:

> **A findings ledger is disposable.** A session is resumable for at most 10 turns and 55 idle
> minutes, so any ledger older than about an hour is already unusable. If a future change does make
> old ledgers unreadable, the loader's existing fail-closed behaviour *is* the migration story:
> `Invalid`, refuse the resume, tell the caller to start fresh. No compatibility types, no version
> dispatch, no provenance flags.

The cost of that policy is that someone mid-review when they upgrade loses accumulated findings and
re-runs. That is minutes and tokens, avoidable by finishing first, and cheaper than permanent
machinery in the loader to prevent it. Revisions 3 and 4 built that machinery; four of the ten gate
findings existed only inside it.

## What this does not do

**This plan re-scopes #62 rather than closing it, and two independent reviewers reached that
conclusion separately.** #62 reports a *liveness* failure — a finding that stays open forever. This
makes that failure visible, attributable and cheap to act on. It does not make it impossible:
nothing compels the reviewer to re-examine a finding it decides to carry, and a caller that ignores
`basis: carried` gets exactly the experience #62 describes.

The issue should be split: an observability half this closes, and a liveness half that stays open.
An automatic in-turn re-verification challenge is the written candidate for the second (revision 2
specified one in full), and it is deliberately not built now — an extra model round trip plus a new
terminal outcome is more of the enumerate-until-converged character that is this tool's real drift.

`converged` continues to mean what it meant: no finding is open and the reviewer approved. It does
**not** mean every finding was re-examined on the approving turn, and the README should say so,
because a caller reading `converged: true` will otherwise supply the stronger meaning for free.

## Parked, with reasons

- **Terminal resolution** — a resolved finding is closed, never restated, and a regression is raised
  as a new finding referencing it. This is a good idea from the maintainer and the only part of the
  larger design that makes reviews *less* ritualistic: the digest shrinks as work is done instead of
  growing forever. It is also a break in a published contract with its own blast radius (Decision 2's
  rationale, `Status::Regressed`, a `regression_of` relation, a closed index, five documents). It
  deserves its own issue and its own decision rather than riding along inside a bug fix.
- **The widened delta window** (revision 1's Layer B) — cut on measurement. The incremental delta
  fired 5 times in 171 recorded turns, and never in the direction where both #62 incidents happened.
- **Carrying findings across a rebaseline** (Layer D) — separable, and its motivation went with the
  challenge.

## Blast radius

- `src/findings.rs` — `Basis`; the two optional ledger fields; `note` carried through reconcile; the
  resolved-without-recheck warning; `output_schema` gaining the optional fields.
- `src/prompt.rs` — the basis/note request in `machine_block_section`, and a digest line reporting
  when each finding was last re-checked.
- `README.md` — the new fields, what `basis` means, `note`'s standing as untrusted commentary, and
  the sentence about what `converged` does not mean.
- `docs/structured-findings-envelope.md` — Decision 2 currently says a `prior_findings` entry may
  carry no other field. Amended: it may carry `basis` and `note`, which are metadata about how a
  status was reached rather than finding content, and still cannot name, retarget or rewrite
  anything the server owns.

No change to `src/session.rs`, no migration, no version bumps.

## Testing

- Every `basis` value round-trips; an absent one records `null` and **does not** degrade the turn.
- `last_verified_turn` advances only on `rechecked`, and **`last_status_change_turn` is unchanged by
  a re-verification that does not move the status** — the regression test for this whole issue.
- A new finding is minted with `last_verified_turn = turn`.
- Resolving on a `carried` or absent basis warns and still records the resolution.
- `note` is capped, is never merged into `detail`, and is replaced rather than accumulated.
- An existing ledger carrying none of the new fields loads unchanged and reports them as `null`.
- **The issue's reproduction:** two findings raised on the same turn, one restated
  `resolved`/`rechecked` and one `open`/`carried`, asserts the second reports its basis and its last
  verified turn while the first does not — so a caller can tell them apart, which is the whole of
  what this change claims to do.

## Open questions

1. **Is optional-in-schema right?** It is the choice that cannot lose a review, and it means a
   reviewer can quietly not answer. The alternative makes the answer mandatory at the price of a
   degraded turn whenever it is missing.
2. **Two basis values or three?** Revision 1 had `unverifiable` as well — "I tried and could not" is
   a different fact from "I did not" — at the cost of a third thing to explain.
3. **Should `note` exist at all?** It is model-authored text in a structure built to exclude it. The
   argument for it is that `basis` alone says a finding was not re-examined but never why one that
   *was* re-examined is still open.
