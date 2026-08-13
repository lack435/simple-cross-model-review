# Stale `open` findings on a resumed session — design

Status: **proposal, revision 2.** Filed against issue #62 ("Resumed review session freezes
findings at a stale `open` status (ledger not re-evaluated)"), and building on the code-path
investigation recorded in that issue's comment, which located the freeze precisely and
reproduced it live.

> **Review history.** Round 1 against this repository's own gate (Codex, gpt-5.6-luna,
> effort=max) returned REQUEST CHANGES with five `major` and two `minor` findings. All seven
> were accepted; none were disputed. See [Round 1 response](#round-1-response) for what each
> one changed. Two of them — a challenge that could drop or duplicate the turn's own new
> findings, and a carried `resolved` finding that could converge without ever being verified —
> were holes in the core mechanism, not details.

## What is actually broken

The issue reports a resumed session in which findings `f19` and `f23` stayed `open` with
`last_status_change_turn: 4` across turns 5, 6 and 7 while ~23 siblings resolved, and a
`fresh` session over the same content did not re-raise either — proving the resumed ledger
was misreporting. The investigation comment then reproduced it in a second session (`f8`,
`f14`), with the sharpest possible control: `f13` and `f14` were first seen on the same turn,
addressed in the same revision, and only `f13` flipped.

**The reconcile is not the bug.** `reconcile` in [`src/findings.rs`](../src/findings.rs) is
keyed on exact server-owned ids, enforces exact-set accounting (`UnknownId` / `MissingId` /
`DuplicateId`), and carries a finding's content forward untouched by design (Decision 2 of
[`structured-findings-envelope.md`](structured-findings-envelope.md)). Given a block that says
`{"id":"f19","status":"open"}`, persisting `open` is the only correct behaviour. Omission is
not a freeze either: a prior id the reviewer fails to restate is `MissingId`, which degrades
the whole turn loudly. So on the turns where the freeze was observed, **the reviewer actively
restated those ids as `open`**, and the server recorded exactly that.

The defect is one level up, in the protocol:

> `{"id":"f19","status":"open"}` is used for two different assertions — *"I re-examined this
> and it is still broken"* and *"I am restating what the ledger told me"* — and nothing in the
> system can tell them apart. Not the server, not the caller, and, once its context has been
> compacted, arguably not the reviewer.

Three subsystems then conspire to make the second assertion the easy one:

1. **The incremental-resume delta** (`decide_incremental`, [`src/vcs/git.rs`](../src/vcs/git.rs))
   sends a resumed turn only `<prior_head>..HEAD`. If the fix for `f19` landed in commits that
   are no longer inside that window, the reviewer is never shown the fixing code on the turn it
   is asked to re-judge `f19`.
2. **The digest anchors the answer.** `render_digest` injects `- f19 [major] … — currently
   open`, and total accounting requires a status back for every id. The lowest-effort answer
   that satisfies the contract is to echo the status it was just handed.
3. **Nothing costs anything.** A restated `open` is free, indistinguishable from a verified
   one, and permanently blocks `converged`.

[`incremental-resume-disposition.md`](incremental-resume-disposition.md) already named this
hazard — "a delta that fired against a reviewer whose context was compacted under-reviews
silently" — and deliberately scoped it out as "about the *caller's* view, not the reviewer's
memory". This document is the other half.

### Why the issue's own suggestion is not the fix

The issue suggests re-emitting every carried finding with a current `last_status_change_turn`.
Rejected: that field means what it says, it is the only record of *when* a finding's state
last moved, and overwriting it every turn would destroy the evidence that made this bug
diagnosable in the first place — the symptom would disappear while the freeze continued. The
missing information is not "when was this last written" but **"when was this last actually
checked, and by what means"**, which no current field records. So this design adds fields
rather than repurposing one.

## What must not change

Every one of these is load-bearing elsewhere and is preserved verbatim:

- **The server never judges code.** No auto-resolution from the diff, no prose parsing. The
  reviewer remains the sole authority on whether a finding is fixed; the server's job is to
  make sure it was in a position to answer and to record what it said.
- **The server owns identity and content.** Ids are minted server-side; `title`, `file`,
  `line`, `detail`, `severity` are captured on first raise and never rewritten.
- **Fail-closed, whole-turn.** No partial trust in a block: any schema violation or
  reconciliation failure degrades the entire turn, exactly as today.
- **`converged` stays the full conjunction**, never a bare `open_count == 0`.
- **Only observationally-honest claims.** Nothing new asserts what the reviewer received,
  retained, or still holds in context.

## The design, in four layers

Layers A–C are one coherent mechanism: **make the assertion honest (A), make honest
re-examination possible (B), and make a dishonest one cost a challenge (C)**. Layer D removes
the penalty from the escape hatch.

### Layer A — a per-finding *basis*: how this status was determined

`prior_findings` entries gain one required field:

```json
{"id": "f19", "status": "open", "basis": "rechecked"}
```

with `basis` one of:

| value | meaning |
| --- | --- |
| `rechecked` | I examined the current state of this code **on this turn** and this status is my judgement of what is there now. |
| `carried` | I did not re-examine it this turn; I am restating the status the server showed me. |
| `unverifiable` | I tried to re-examine it and could not (the code was not in what I was shown, and I could not read it). |

`carried` and `unverifiable` both mean *not affirmed this turn*; they are kept apart because
the difference between "did not" and "could not" is exactly the difference between a lazy
turn and a starved one, and this repository's whole posture is to report which. The field is
required on **every** prior entry, not only the open ones: a finding restated `resolved`
without a re-check is the more dangerous of the two, because it can converge.

**Server-enforced invariant: a status *change* requires `basis: "rechecked"`.** A block
claiming `open → resolved`, `resolved → regressed`, or any other transition on a `carried` or
`unverifiable` basis is a new fail-closed reconciliation error, `UnverifiedChange(id)`, with a
corrective that routes through the existing block-repair seam ("you reported `f3` resolved
without re-checking it; either re-check it, or restate it honestly"). This is what makes
`carried` safe to accept at all: **within a conversation running this protocol**, a `resolved`
in the ledger is always a status that was verified on the turn it flipped. Two populations sit
outside that guarantee and are handled explicitly below: findings resolved under v1, before
`basis` existed, and findings carried into a new session by Layer D.

A new finding is **minted with `last_verified_turn = turn`.** Raising a finding entails having
looked at the code this turn, so a new finding is verified by construction — and this is what
keeps it out of the challenge set in Layer C.

Ledger `Finding` gains three fields (`schema_version` 1 → 2, migration below):

- `last_verified_turn: Option<u32>` — the last turn this finding was restated with basis
  `rechecked` (or minted). `None` for a finding never verified under this protocol — a
  v1-migrated one. **This is the field the issue actually wanted**; `last_status_change_turn`
  keeps its current meaning untouched.
- `last_basis: Option<Basis>` — the basis of the most recent restatement (`None` on a ledger
  migrated from v1, before the field existed).
- `anchor: Option<Anchor>` — the capture endpoints recorded when this finding was last
  verified against a **complete** capture. Layer B's input, and **ledger-only**; see below.

**Convergence gains one conjunct: every ledger finding must have been verified at least once
under this protocol** (`last_verified_turn.is_some()`). Without it the dangerous direction is
wide open: a finding already `resolved` in a v1 ledger, or carried in by Layer D, could be
restated `resolved`/`carried` forever and converge having never been checked by the conversation
that approved it. This conjunct is why Layer C's challenge set is defined over *never-verified*
findings as well as open ones, rather than over open ones alone. The cost is one
sweep: the first turn after an upgrade or a carry must re-check everything it inherited, driven
by the digest and, if the reviewer will not, by the challenge. That is the correct price for
the ledger's central claim.

**Persisted and wire shapes are separated.** `Finding` is the on-disk record and keeps
`anchor`; the envelope serializes an explicit `EnvelopeFinding` view carrying
`last_verified_turn`, `last_basis` and a derived `stale: bool` (not verified on this turn, and
either still open or never verified), and **not** `anchor` — a capture endpoint is server
bookkeeping, not something a caller should be parsing. Today `Finding` is serialized straight
into the envelope, so without the split, adding `anchor` would either leak onto the wire or
collide with `output_schema()`'s `additionalProperties: false`, and `stale` would have nowhere
to live. Envelope `schema_version` 2 → 3, `output_schema()` describing the view.

The digest renders the new state as evidence, and asks the question the old one did not:

```
- f19 [major] Allowlist fingerprint binding is inconsistent (src/allowlist.rs:88)
  — currently open; last re-checked on turn 4 (3 turns ago)
- f7 [minor] Provisional setup is not serialized (src/setup.rs:412)
  — currently resolved; never re-checked in this conversation
```

plus an instruction block stating the three basis values and, plainly, that restating a status
you did not verify is a legitimate answer *only* when you say so with `carried`.

### Layer B — the evidence window must reach back to every open finding

Driver 1 above is a real, mechanical starvation, and it is fixable exactly: **a resumed delta
must never be narrower than the oldest still-open finding's anchor.**

Each finding stores the capture endpoints in force when it was raised or last verified:

```rust
enum Anchor { Git { turn: u32, head: String, base: String } }
```

**An anchor is written only from a capture that was complete.** This is the same rule that
already voids the session baseline: `capture` returns `(None, None)` for `head_sha`/`base_sha`
when the diff was cut by the byte cap or arrived short or lossy, precisely because a later
`<head>..HEAD` would never re-show the omitted part. A finding verified against a truncated
capture therefore **keeps its previous anchor** rather than advancing to one that would license
a window it was never shown — older is always the safe direction — and a finding that has never
been verified against a complete capture has no anchor at all.

The delta start is then `min` by `turn` over **the anchors of currently-open findings only**.
The session's stored baseline is used when there are no open findings, and is never mixed into
that ordering: it carries no turn of its own (`SessionRecord` stores `head_sha`/`base_sha` with
no provenance), and `record_turn` *retains* an older pair when a capture is incomplete, so its
label and its age can disagree. Ordering a turn-less baseline against turn-stamped anchors is
exactly how an open finding ends up behind an advanced baseline and never sees its fix.

`SessionRecord` gains `baseline_turn: Option<u32>` beside the existing pair, so the retained-pair
case is legible in the record and the disposition can name the turn honestly rather than
implying the baseline is from the previous turn when it is not.

**If any open finding has no usable anchor, the turn captures in full** —
`FellBack::OpenFindingAnchorUnusable`. This is the load-bearing fail-closed rule: absent anchor,
failed guard, or an anchor the current configuration cannot be reconciled with all land here,
and a full capture by construction shows the reviewer everything.

The chosen anchor otherwise goes through the *existing* guards unchanged — both ids
syntactically valid (`PriorBaselineInvalid`), stored base identical to the current effective
base (`BaseMoved`), prior head an ancestor of HEAD (`BranchRewritten` / `AncestryUndecidable`) —
and any failure falls back to the full range. A rebased branch therefore does not need the
anchors to be ordered correctly to stay safe: the ancestry guard rejects them first.

Two new dispositions, in the taxonomy
[`incremental-resume-disposition.md`](incremental-resume-disposition.md) already defines:

- `Incremental::GitRange` gains `widened_to_turn: Option<u32>`, rendered as "widened from
  turn N's baseline to cover findings still open since then" — the delta is legible, per that
  document's whole premise.
- `FellBack::OpenFindingAnchorUnusable` — an open finding's anchor failed the guards or is
  absent (a v1-migrated ledger, or a turn whose HEAD never resolved). Warns, and falls back to
  the full range.

The cost is bounded and self-correcting: the window grows back toward the full range only
while findings stay open — precisely when the extra context is what the reviewer needs — and
collapses to the ordinary delta as soon as they resolve.

The reviewer-facing incremental note in `git::render` is amended in step: it currently says the
diff is "only what changed since your previous turn", which would be false on a widened window.
It must say the window covers every commit since the last turn on which each still-open finding
was verified, so the reviewer knows the fixing code is expected to be inside it.

**Perforce.** The analogue of a starved window is evidence elided as byte-identical. Rule: on a
turn with open findings, evidence units whose path matches an open finding's `file` are always
re-sent, never collapsed; and if any open finding names no usable path, elision is disabled for
that turn (new reason `FellBack::OpenFindingUnanchorable`). Path matching goes through
[`src/pathcmp.rs`](../src/pathcmp.rs), not string equality. This is the one piece of the design
that is separable — see [open questions](#open-questions-for-the-reviewer).

### Layer C — a stale-open finding is challenged inside the same turn

Layers A and B remove the excuses; Layer C makes an unexcused `carried` cost something before
it reaches the caller. It reuses the seam issue #63 built for block repair
([`unstructured-turn-recovery.md`](unstructured-turn-recovery.md)): a short follow-up prompt in
the **same reviewer conversation**, between `assess_turn` and `finalize_turn`, with the decision
to send it made by a pure, unit-tested planner.

- `plan_verification(&assessment, attempts_remaining, cancelled) -> Option<VerificationRequest>`
  mirrors `plan_repair`. It fires when the turn is structured, reconciliation was clean, and the
  **challenge set** is non-empty. The challenge set is every finding not verified on this turn
  that is either still open, or has never been verified under this protocol (the v1-migrated and
  carried populations Layer A's convergence conjunct refuses to converge). New findings minted
  this turn are verified by construction and are never in it.
- `prompt::verification_challenge` names exactly those ids with their title, location and
  recorded detail, and — where the server can read the recorded path — a bounded excerpt of the
  **current** file content around the recorded line. The instruction is narrow: judge these ids
  against what is in front of you now; answer `rechecked` with a real status, or `unverifiable`
  and say in prose why. It is explicitly not a re-review and must not revise other findings.
- **The challenge reconciles against this turn's *intermediate* ledger, never the pre-turn one.**
  The intermediate ledger is what the turn's own block already produced: prior findings with
  their new statuses applied, plus this turn's new findings already minted with their ids. The
  challenge block accounts for every id in *that* set, exactly once, and must carry an empty
  `new_findings` — a challenge is not a re-review, and the schema says so. Reconciling against
  the pre-turn ledger instead would corrupt the turn either way round: repeating this turn's new
  findings would mint them a second time under fresh ids, and omitting them would drop them from
  the review entirely. The challenge moves statuses on an established id set; it can never
  change what ids exist, and `next_seq` does not move.
- Otherwise it is the **same block schema and the same reconciler** — including
  `UnverifiedChange`, so the challenge cannot itself resolve something on a `carried` basis. No
  partial-block path is introduced: one schema, one reconciler, no new place for partial trust.
  `apply_verification` supersedes the earlier result when the answer extracts and reconciles
  cleanly, and leaves it untouched when it does not. A status the reviewer moves on an id that
  was *not* in the challenge set is accepted (it is a judgement under the same invariant), and
  noted in the warnings so the caller sees the challenge did more than it was asked.
- Attempts and per-attempt timeout are configured as `--stale-verification-attempts` (default 1,
  small hard cap) and `--stale-verification-timeout-seconds`, mirroring the block-repair pair,
  and are folded into the total-deadline arithmetic in `config.rs` that already accounts for
  repair attempts.
- Any prose the reviewer writes alongside the answer is preserved and labelled, exactly as a
  block-repair note is.

The excerpt read is a new file read on a **model-authored path**, and the first draft's
"canonicalise, check it is inside the root, then open it" is not good enough: that is
check-then-use, and on Windows a reparse point swapped between the check and the open defeats
it, while a UNC or device path (`\\?\`, `\\server\share`, a drive-relative `C:foo`) can resolve
somewhere real before any root test rejects it. So the excerpt does **not** get its own path
validator. It goes through **the same bounded, no-follow reader the evidence service already
uses** for `repository_read` — reject unsafe path *forms* before touching the filesystem, open
and verify the handle without following reparse points, read under the existing byte and line
caps. One reader, one boundary, already reviewed and documented in the README; a second
implementation of the same rules is a second thing to get wrong.

The excerpt is rendered as **repository evidence, labelled as evidence and not as
instructions**, exactly as the change capture is. It is content from the repository under
review, at a location chosen by an earlier turn of the model, so it is precisely the shape of
thing the README's prompt-injection framing exists for. If any step fails there is no excerpt
and the challenge still goes: an unquotable finding is still challengeable.

**The outcome of a failed challenge.** After the challenge, if a finding is still open on a
basis other than `rechecked`:

- The envelope reports `stale: true` on those findings and a warning naming them.
- A new `NonConvergenceReason::FindingsUnverified` maps to `Outcome::Rebaseline`, ranked
  above `OpenFindings` and below `ReviewerWithheldApprove`: the open count itself is not
  trustworthy, so "act on the findings and re-review" is the wrong advice — this session cannot
  converge, and a person (or Layer D) should carry the still-open findings into a new one.
- **This stronger outcome requires the server to have actually asked.** If the challenge could
  not run — attempts configured to 0, cancelled, no resumable conversation id, or the challenge
  child failed — the turn reports `stale: true` and the warning but keeps `open_findings` /
  `changes_requested`. The server does not escalate for a question it never put.

This is the pivot the issue is really about: today a frozen finding is an invisible permanent
block; afterwards it is either re-verified, or named, escalated, and carried.

### Layer D — a rebaseline that does not lose the ledger

The issue's second impact bullet is that the workaround costs everything: `fresh: true` starts a
new reviewer conversation *and* wipes the ledger, so prior dispositions are lost and a new layer
of findings surfaces. With Layers A–C, a rebaseline becomes a routine instruction, so it should
be cheap.

`cross_model_review` gains `carry_findings: bool` (meaningful only with `fresh: true`). When set
and the prior record's ledger loads `Valid`, the new session starts with that ledger's findings —
same ids, same content, same statuses — `next_seq` preserved so no id is ever reused, every
`anchor` and `last_verified_turn` cleared (turn 1 is a full capture; nothing is inherited that
could justify a narrow window).

**The over-budget case is the one that matters most, and the first draft got it backwards.**
`ledger_too_large` is a `rebaseline` outcome — carrying findings into a new session is exactly
the remedy — yet a ledger that tripped the budget is still structurally valid, so carrying it
whole would re-trip the pre-call budget gate before the new reviewer ran and defeat the only
escape it has. So the carry is bounded: **resolved findings are dropped and the still-open ones
are carried**, with the dropped ids listed in a warning and the consequence stated plainly — a
regression in dropped work reappears as a new finding under a new id rather than reattaching to
its original one. If the carried set is *still* over budget, the call is **refused with an
actionable error** naming the count and the remedy, not started into a gate that will kill it.
Silently starting a review that cannot survive its own entry check is the failure mode this
project refuses everywhere else.

Coverage of the new session is `whole_conversation` **iff the source ledger was**, else
`legacy_uncovered`. Carried findings arrive with `last_verified_turn: None` — so Layer A's new
conjunct blocks convergence until the new conversation has re-checked each one itself, which is
what stops a carry from laundering an unverified disposition into a convergeable session.

With that conjunct in place the coverage argument is exact rather than convenient: turn 1 of
the new conversation renders the carried findings as its digest, total accounting requires a
status for every one of them, and the new conjunct requires each to be *re-checked* by this
conversation before it can converge. So the new ledger covers its own conversation from turn 1,
and no carried status is trusted merely because someone else once trusted it. If the source
ledger was already broken, its coverage is inherited broken and nothing is laundered.

One prompt consequence: the digest and total-accounting section are currently gated on
`resumed`. They become gated on **the digest being non-empty**, since a carried-forward turn 1
is a first turn *with* prior findings.

## Wire, schema and migration

- **Ledger `schema_version` 1 → 2.** `SessionRecord::ledger_load` gates on exact equality
  today, so a v1 ledger would load `Invalid` and refuse the resume — that would brick every
  in-flight session on upgrade. An explicit upgrade path is therefore part of this change: a v1
  record deserializes into the v2 shape with `last_verified_turn: None`, `last_basis: None`,
  `anchor: None`, and is re-written at v2 by the first turn that persists. The upgraded defaults
  are all fail-closed — no anchor means a full capture, no basis means nothing is treated as
  verified.
- **Envelope `schema_version` 2 → 3**, with `output_schema()`'s completed variant describing the
  `EnvelopeFinding` view — `last_verified_turn`, `last_basis` and `stale` required (the renderer
  always emits them), `anchor` absent by construction rather than by omission.
  `additionalProperties: false` means the schema and the renderer have to move together, which is
  why they live in one file.
- **The two versions move independently, and that is the point.** They were split precisely so a
  wire change could not mark every on-disk ledger foreign; this change happens to move both, which
  is not a licence for anything to assume they are equal. Any code or doc that pairs them is
  corrected rather than followed.
- **`docs/unstructured-turn-recovery.md` is amended too.** Its Decision D pins envelope v2 and
  ledger v1 and carries migration tests against those numbers, so leaving it alone would ship two
  design documents that specify different contracts for the same file.
- **`docs/structured-findings-envelope.md` is amended, not contradicted.** Decision 2 states
  that a `prior_findings` entry "can carry *no* other field"; `basis` is a deliberate,
  documented widening — it is metadata about *how the status was reached*, and it still cannot
  name, retarget, or rewrite anything the server owns. That document gets an amendment section
  recording the change and this rationale.
- **`docs/incremental-resume-disposition.md`** gets the two new dispositions and a note that the
  "reviewer's memory is out of scope" boundary is now partly closed by Layer B.
- **`README.md`** documents the new fields, the new outcome, the new flags, and `carry_findings`.
- **`src/metrics.rs`** records per-turn stale counts and the challenge outcome, so a session
  audit can show how often this fires — the same attribution argument the disposition record won.

## Failure modes

| situation | behaviour |
| --- | --- |
| Block omits `basis` on an entry | schema violation → whole-turn degrade → existing block-repair seam re-asks (this is the upgrade path for a reviewer mid-session) |
| `basis` present, status changed, basis not `rechecked` | `UnverifiedChange(id)` → degrade + corrective → repair seam |
| Open finding restated `carried`, challenge clears it | ordinary turn; `last_verified_turn` advances |
| Open finding still unverified after a challenge that ran | `stale: true`, `findings_unverified` → `rebaseline` |
| Challenge could not run (disabled, cancelled, no target, child failed) | `stale: true` + warning, outcome stays `changes_requested` |
| Challenge answer carries a non-empty `new_findings` | schema violation → challenge discarded, the turn's own result stands unchanged |
| Challenge answer omits an id the intermediate ledger holds | `MissingId` → challenge discarded, the turn's own result stands unchanged |
| Resolved finding never verified under this protocol (v1 or carried) | in the challenge set; blocks convergence until re-checked |
| Open finding's anchor missing or failing the guards | full capture + `FellBackToFull { OpenFindingAnchorUnusable }` warning |
| Finding verified against a truncated/short capture | keeps its previous anchor; never advances to a window it was not shown |
| v1 ledger on disk | upgraded in place; nothing is treated as verified until the reviewer says so, and nothing converges until it does |
| `carry_findings` set on an over-budget ledger | resolved findings dropped (ids warned), and the call refused outright if what remains is still over budget |
| Reviewer claims `rechecked` untruthfully | undetectable, and out of scope — as with every other status the reviewer supplies. Narrowed, not closed; stated here rather than hidden. |

## Blast radius

Wide by intent (per the task framing, foundational completeness over minimal diff), but
cohesive — every file below is touched because it holds one end of the same contract:

- `src/findings.rs` — `Basis`, the ledger fields, `UnverifiedChange`, the digest render,
  `plan_verification` / `apply_verification`, `FindingsUnverified` + its rank and `Outcome`
  mapping, envelope + `output_schema`, migration shim.
- `src/prompt.rs` — the basis contract in `machine_block_section`, `verification_challenge`,
  the digest gate moving from `resumed` to "digest non-empty".
- `src/tools.rs` — the challenge loop beside the repair loop, the anchor set passed to capture,
  `carry_findings`, the stale warnings.
- `src/session.rs` — ledger v1→v2 upgrade, `baseline_turn` on the record, carry-forward on
  `fresh` (including the bounded/refused over-budget carry).
- `src/evidence/core.rs` — the bounded no-follow reader gains a caller inside the server (the
  challenge excerpt), rather than a second path validator being written elsewhere.
- `src/vcs/git.rs` — anchor-widened delta, the two dispositions, the amended incremental note.
- `src/vcs/disposition.rs`, `src/vcs/mod.rs`, `src/vcs/shared.rs` — the new disposition variants
  and their rendering.
- `src/vcs/perforce.rs`, `src/vcs/baseline.rs` — open-finding evidence protection (Layer B,
  Perforce).
- `src/config.rs` — the two new flags and the deadline arithmetic.
- `src/mcp.rs` — `carry_findings` in the input schema.
- `src/metrics.rs` — the new record fields.
- Docs as listed above.

## Testing

Pure `findings.rs` unit tests carry the weight, as they do for the existing contract:

- Every `basis` value round-trips; a missing `basis` degrades; an unknown value degrades.
- `UnverifiedChange` fires for each transition on a non-`rechecked` basis, and **not** for a
  restatement of the same status.
- `last_verified_turn` advances only on `rechecked`; `last_status_change_turn` is unchanged by
  a re-verification that does not move the status (the regression test for this whole issue).
- A new finding is minted verified: `last_verified_turn == turn` on mint, and it is never in the
  challenge set on its own turn.
- `stale` is exactly "not verified this turn, and either still open or never verified".
- **Convergence is refused while any finding has `last_verified_turn: None`**, with `open_count`
  at zero and a clean `approve` — the v1-migrated and carried populations, tested separately.
- `plan_verification` returns `None` for a clean turn, a degraded turn, zero attempts, and a
  cancelled call; `Some` for each stale shape, including a *resolved* never-verified one.
- **The challenge's ledger identity:** a turn that raises new findings *and* has a stale one
  asserts the challenge accounts for the new ids too, that a challenge answer repeating them as
  `new_findings` is rejected, that one omitting them is rejected, and that `next_seq` is
  unmoved either way — the round-1 f1 regression, in three parts.
- `apply_verification` supersedes on a clean answer and preserves the original on a malformed
  one.
- `FindingsUnverified` precedence against every other reason; and the "challenge did not run"
  case keeps `open_findings`.
- **The issue's reproduction, as a test:** a three-turn ledger where `f13` and `f14` are raised
  on the same turn, `f13` is restated `resolved`/`rechecked` and `f14` `open`/`carried`, asserts
  the challenge fires naming `f14` alone, and that a cleared challenge flips it.

Beyond that: git tests for the widened window (widened, collapsed once resolved, each guard
failure, an anchor *not* advanced by a truncated capture, and an open finding with no anchor
forcing a full capture); a two-turn temp-repo round trip asserting the fix's commit is inside
the widened range; Perforce tests for forced re-send and for the unanchorable disable; session
tests for the v1→v2 upgrade and for `carry_findings` (ids preserved, `next_seq` preserved,
coverage inherited correctly, resolved findings dropped when over budget, and the refusal when
what remains is still over budget); an excerpt test asserting a reparse point and a UNC/device
path are refused before any read; prompt goldens; and `smoke.ps1` for one real challenge round
trip, noted as costing tokens.

## Implementation order

Each phase is independently useful and independently reviewable:

1. **A** — basis, ledger v2 + migration, `UnverifiedChange`, the verified-at-least-once
   convergence conjunct, the `EnvelopeFinding` split, envelope/schema, digest. The freeze becomes
   visible, and an unverified `resolved` stops converging.
2. **C** — the challenge (against the intermediate ledger) and `FindingsUnverified`. The freeze
   becomes self-correcting or loud. Phase 1 without phase 2 makes an upgrade non-convergent until
   the reviewer volunteers a re-check, so these two ship together.
3. **B (git)** — the widened window. The commonest cause of an honest `unverifiable` goes away.
4. **B (Perforce)** and **D** — parity and a cheap rebaseline.

## Round 1 response

Seven findings, all accepted, none disputed. Two were holes in the mechanism rather than
refinements of it, and both are now load-bearing parts of the design:

- **f1 — the challenge could drop or duplicate the turn's own new findings** (`major`).
  Correct, and the failure was total either way: reconciling the challenge against the *pre-turn*
  ledger meant a challenge answer repeating this turn's new findings would mint them again under
  fresh ids, and one omitting them would delete them from the review. Fixed by reconciling the
  challenge against the turn's **intermediate ledger** and forbidding `new_findings` in a
  challenge answer, so the challenge can move statuses on an established id set and nothing else.
- **f2 — a carried `resolved` finding could converge unverified** (`major`). Correct, and it
  falsified a claim the draft made in its own text. The invariant "a `resolved` was verified when
  it flipped" holds only inside a conversation running this protocol; v1-migrated and
  Layer-D-carried findings sit outside it. Fixed with a new convergence conjunct — every ledger
  finding must have been verified at least once under this protocol — and by putting
  never-verified findings in the challenge set regardless of status.
- **f3 — Layer B had no baseline provenance** (`major`). Correct: `SessionRecord` stores no turn
  for its baseline, `record_turn` retains an older pair on an incomplete capture, and git returns
  no baseline at all for a truncated one, so "lowest turn wins" was ordering things that had no
  comparable turn. Fixed by writing anchors only from complete captures, taking the minimum over
  open findings' anchors *only*, persisting `baseline_turn`, and forcing a full capture whenever
  an open finding has no usable anchor.
- **f4 — `carry_findings` could not carry an over-budget ledger** (`major`). Correct, and it
  defeated the feature exactly where it was most needed, since `ledger_too_large` is the outcome
  that most wants a carry. Fixed with a bounded carry (resolved findings dropped and reported)
  and an explicit refusal when what remains still will not fit.
- **f5 — the excerpt read was check-then-use** (`major`). Correct on Windows reparse races and on
  UNC/device path forms. Fixed by routing the excerpt through the evidence service's existing
  bounded no-follow reader instead of writing a second validator, and by labelling the excerpt as
  repository evidence.
- **f6 — a contradictory design contract** (`minor`). Correct: `unstructured-turn-recovery.md`
  pins envelope v2 / ledger v1. Added to the amendment list, with a note that the two versions are
  deliberately independent.
- **f7 — persisted and wire finding shapes were not separated** (`minor`). Correct; `anchor` would
  have leaked onto the wire or collided with the strict schema. Fixed with an explicit
  `EnvelopeFinding` view, `anchor` staying ledger-only and `stale` computed at render.

## Open questions — my answers

Stated as decisions rather than questions this round. The useful review response is to flag any
of these you think is wrong, not to ratify them.

1. **`findings_unverified` → `rebaseline`: keeping it.** The open count is not trustworthy once a
   finding is frozen, and `changes_requested` tells the caller to do the one thing that
   reproduces the freeze. The carve-out that makes it fair is that the server must have actually
   put the question, with evidence, before it escalates.
2. **Keeping three basis values.** `carried` and `unverifiable` are treated identically by every
   rule, so nothing depends on the distinction — but "I could not check" and "I did not check"
   are different facts about the review, and this project reports which. The cost is one enum
   variant.
3. **Perforce parity in this design, last in the order.** Layers A and C fix the reported bug on
   both backends; Layer B's Perforce half is phase 4 and is the piece to cut if anything is cut.
4. **Keeping the excerpt, now that f5 has moved it onto the evidence reader.** The failure mode
   is a reviewer whose context was compacted; telling it to go and read the file is asking it to
   do the thing it just demonstrably did not do. With no new path-handling code, the objection
   that remained was cost, and the excerpt is bounded.
5. **`carry_findings` inherits `whole_conversation`, and f2's conjunct is what earns it.** A
   carried finding arrives unverified and blocks convergence until the new conversation re-checks
   it, so the carry transports *evidence*, never a disposition the new conversation has not
   confirmed.

Two things I would still like a view on:

- **Is the challenge's "accept status moves on ids outside the challenge set" rule right?** It is
  consistent with the reconciler and it is honest reviewer judgement, but it does let a challenge
  quietly become a small re-review.
- **Does the new convergence conjunct make the first turn after an upgrade too expensive** on a
  large ledger — every finding must be re-checked once, and a reviewer that will not do it turns
  an upgrade into an immediate `rebaseline`?
