# Fixing #62's observability half: a restatement is a claim, and a resolution is final

Status: **proposal, revision 2** (gate round 1 accepted, one fix reduced). Supersedes
[`stale-open-findings.md`](stale-open-findings.md), which is kept as the record of why revisions 1–5
were not built and should be read before this one is extended.

**Scope, stated up front:** this closes the *observability* half of #62 — a caller can tell a
finding that was re-examined from one that was not. It does **not** close the liveness half; nothing
here compels a reviewer to re-examine anything. That half stays open under its own issue. See
[What it does not do](#what-it-does-not-do).

## The bug

On a resumed session a finding can sit at `open` across turns while its siblings resolve around it,
so the session cannot converge even after the work is done. The caller cannot tell that finding from
one the reviewer is holding open deliberately, because on the wire they are the same bytes.

## The cause is the prompt, not the ledger

`reconcile` in [`src/findings.rs`](../src/findings.rs) is faithful: told `open`, it stores `open`.
The defect is what the reviewer is *required* to say. From `machine_block_section` in
[`src/prompt.rs`](../src/prompt.rs):

> In `"prior_findings"`, report a status for **every** id above, **exactly once** […] A missing id,
> an extra id, or a duplicate fails the turn.

**The protocol demands an assertion the reviewer may have no grounds for.** Twenty-five findings are
listed; the reviewer re-examined four; it must still emit a status for all twenty-five or lose the
turn. The cheapest way to satisfy that for the other twenty-one is to repeat what the digest just
said, and the server records those repetitions as though they were judgements — because it cannot
tell, and neither can the caller.

The distinction is destroyed at the point of collection, so nothing downstream can recover it. Every
previous revision tried to add it back with a new field. The straightforward fix is to stop
destroying it.

A second defect compounds it: the ledger only ever grows. A session with thirty resolved findings
still parades all thirty past the reviewer every turn for a status it has no reason to revisit.

## The change: two rules

### 1. A restatement is a claim, so making one is optional

`prior_findings` is redefined from *a status for every prior finding* to **the findings you
re-examined this turn, with their current status**. An id the reviewer does not list stands exactly
as recorded. Saying nothing about a finding becomes the honest report of not having looked at it.

`ReconcileError::MissingId` is deleted. `UnknownId` and `DuplicateId` stay: inventing an id and
double-reporting one are the reviewer writing something the server owns, which is still an error.

`Finding` gains `last_verified_turn: Option<u32>` (`#[serde(default)]`), stamped with the current
turn when the id appears in the block and when the finding is minted. It is **derived from
presence**, never self-reported, so a re-examination cannot be claimed without making one.

### 2. A resolution is final

**A finding that reaches `resolved` is closed.** It is never asked for a status again and is not
restatable — an id already resolved is `UnknownId` if it reappears in `prior_findings`, on the same
reasoning as any other id the reviewer does not own.

**`Status::Regressed` is deleted.** A defect seen again after a resolution is a *new* finding with a
new id, carrying `regression_of: Option<String>` naming the resolved finding it recurs from. This is
the maintainer's decision and it is the right one: a regression at turn 9 of something fixed at turn
3 is new work, discovered fresh, with its own evidence — modelling it as the old finding changing
its mind loses the fact that it was fixed once and broke again.

**Closed findings stay visible as titles and locations, and are not restatable.** The digest gains a
short second section — `- f7 Allowlist fingerprint binding is inconsistent (src/profile.rs:212) —
resolved turn 5`, one line each, no status to report and nothing to account for. The reviewer is
told these are closed, that a recurrence is possible, and that it raises one as a new finding naming
the closed id in `regression_of`.

The location is carried because a recurrence is recognised by *where* it is at least as often as by
what it was called, and a title alone can be too generic to cue one (round-2 `f7`). Severity is
deliberately not carried: how bad a finding was when it was open does not help anyone recognise it
coming back, and this list is a cue, not a record.

This is the round-1 gate's `f1` accepted at a smaller size, and it earns its place twice over: it
keeps the regression cue the reviewer would otherwise lose, and it is what makes `regression_of`
populatable by a reviewer whose context has been compacted. It matches the maintainer's own framing
— a resolved point "can be kept in mind if future revisions regress it". What it deliberately is
**not** is a full closed-finding index with statuses, or a new stale-closed-findings gate; see
[The objection this has to answer](#the-objection-this-has-to-answer).

`regression_of` is **advisory metadata**. It is kept when it names an id this ledger issued that is
resolved, and dropped otherwise. There is no warning path and no new failure mode: `reconcile`
returns findings and a counter, adding a warning channel to it would be new plumbing, and what is at
stake is a cross-reference, not a review. The reviewer has the closed ids in front of it, so a bad
reference is unlikely and cheap when it happens.

### What the two rules do together

The digest stops being a ledger dump and becomes a **work list**: four open findings to report on,
and a list of closed titles to keep in mind. Turn 7 of a long session asks the reviewer for four
statuses, not twenty-nine. The bookkeeping that made lazy restatement the rational answer is gone
rather than policed, and what remains grows with *closed titles* rather than with restatable
entries.

**One honest correction to the "shrinking" claim** (round-1 `f2`). `Ledger::digest_bytes` today
serializes every finding in full, and its own doc comment calls that "the size of the prior-findings
digest injected into each prompt". After this change that is simply false, so `digest_bytes` is
narrowed to measure what is actually injected — open findings in full plus closed titles. That is
not new machinery; it is an existing function being made to mean what it already claims. `Budget`'s
other cap, `max_findings`, keeps counting **every** finding, closed ones included, because that one
bounds stored state rather than prompt size and stored state does still grow without limit. A
session that runs long enough to hit 500 findings is refused and rebaselined, which is the runaway
bound working.

### Ledger compatibility: a deliberate break, stated plainly

`last_verified_turn` and `regression_of` are additive and optional. **Deleting `Status::Regressed`
is not** (round-1 `f5`): a stored `"regressed"` fails typed deserialization in
`SessionRecord::ledger_load`, which falls to `LedgerLoad::Invalid` and refuses the resume. So an
in-flight session that spans the upgrade and holds a regressed finding is not resumable.

That is deliberate and no migration is owed, per `AGENTS.md`. But the cost is more than "re-run",
and this document will not imply otherwise: the ledger holds the stable ids, the immutable content
of every finding and the dispositions, so a refused resume means **a human carrying the still-open
findings into a fresh session by hand**. Bounded, occasional, and still cheaper than a compatibility
type in the loader.

## Why this is smaller than what it replaces

It **removes two ways for a turn to go wrong** and adds none. `MissingId` degrades the whole turn
today: a reviewer that re-examined everything and dropped one id out of twenty-five loses the entire
block, prose included. `Status::Regressed` is a third status every consumer has to interpret, for a
case that is better described as a new finding.

It adds **two optional fields and no required ones**. The previous design reached less observability
with a `basis` enum, a `note` string, two ledger fields, a warning path, `output_schema` changes and
an amendment permitting metadata inside a `prior_findings` entry.

## What it does not do

**It does not force a reviewer to re-examine anything.** A reviewer that looks at nothing files an
empty `prior_findings` and every open finding stays open. The liveness half of #62 is not closed by
this, and closing it means compelling a re-check — an extra model round trip and a new terminal
outcome. That belongs to its own issue and its own decision.

What changes is that the failure becomes visible and addressable: `last_verified_turn` sits turns
behind `turn`, the digest says so to the reviewer's face, and the caller can name the id.

**Convergence is unchanged, and already carries the guarantee worth having.** A finding reaches
`resolved` only by being listed, and being listed is being examined — so every finding in a
converged session was examined on the turn it resolved. True before, true after, and worth writing
down because `converged` is otherwise read as the stronger claim that everything was re-examined on
the approving turn.

## The objections this has to answer

### Omission is silence, and silence is what froze `f19` in the first place

Silence is now **truthful**. Today the protocol converts the reviewer's silence into a spoken `open`
and records it as a judgement. After this it stays silence, and silence is legible: it is exactly
the set of findings whose `last_verified_turn` is stale.

The related worry — that a truncated block now looks like a short honest list — does not arise. A
block cut off mid-array is not valid JSON, never reaches `reconcile`, and takes the existing
unstructured-turn recovery path. Round 1 of the gate verified this independently against
`src/findings.rs:520-548`. No new guard is needed.

### Terminal resolution could let a recurrence through to a false approval

This was round 1's only major finding, and half of it is accepted above: closed titles stay in the
prompt, so the regression cue survives. The other half — a full closed index carrying statuses, or a
new stale-closed-findings gate with a human outcome — is **declined**, and the reason is that the
scenario is not new and the gate would not be catching it.

Convergence needs `verdict == approve` **and** `open_count == 0`. Under this design an omitted open
finding stays open and blocks it, so the only route to a bad approval is a ledger with nothing open,
a recurrence of something already closed, and a reviewer that does not notice. **Today's protocol
reaches that same state by the same route.** It requires the reviewer to restate every closed
finding as `resolved`, and the cheapest way to satisfy that requirement is to echo the digest — the
exact reflex issue #62 exists to describe. A forced echo is not a check, so removing it removes a
ritual, not a safeguard.

What today's protocol does provide is the finding's *title* in front of the reviewer, and that is a
real cue. The closed-titles list keeps it, at one line per closed finding and no restatement burden.
A gate on top would be new machinery guarding a hole it does not close, against a failure that
already exists.

## Blast radius

- `src/findings.rs` — delete `ReconcileError::MissingId` and `Status::Regressed` with their match
  arms; two `serde(default)` fields; presence-driven stamping and the resolved-is-closed rule in
  `reconcile`; `render_digest` splits into open entries and closed titles; `digest_bytes` narrowed
  to what is injected; `output_schema` gains the optional fields.
- `src/prompt.rs` — the `prior_findings` instruction, the closed-findings and regression
  instructions, the digest line's last-re-examined turn.
- `README.md` — omission, terminal resolution, `regression_of` as advisory, and what `converged`
  does not mean.
- `docs/structured-findings-envelope.md` — Decision 2's accounting rule goes from exact-set to
  subset; resolved findings leave the restatable set; the `regressed` status is removed from the
  contract (round-1 `f4` names `:307-328` and `:1321-1322`).
- `docs/unstructured-turn-recovery.md` — its repairable-error inventory still lists `MissingId` and
  its total-accounting requirement (round-1 `f4`, `:130`). Active sections updated; the historical
  review records in both documents are left as written.

**No production wiring changes in `src/session.rs` or `src/tools.rs`** — but both are touched by
tests: `src/session.rs` gains the ledger-compatibility test that pins the deliberate
`Status::Regressed` break (a stored `"regressed"` must load as `Invalid` and refuse the resume,
while an otherwise-legacy ledger must still load with the new fields absent), and `src/tools.rs` has
a `Finding` fixture updated for the two new fields. The reconciliation, session and tool call paths
themselves are unchanged.

## Testing

- An omitted prior id is carried unchanged and **does not** degrade the turn — the deletion of
  `MissingId`, and the regression test for this issue.
- A listed id stamps `last_verified_turn`; an omitted one leaves it alone.
- `last_status_change_turn` is untouched by a re-examination that does not move the status, so the
  record of when a finding last moved survives.
- A resolved id restated in a later turn is `UnknownId`; resolved findings carry no status in the
  digest and appear only as a title and location.
- A new finding carrying `regression_of` for a resolved id keeps it; one naming an unissued or
  still-open id drops the reference silently and still records the finding.
- An empty `prior_findings` on a resumed turn is valid and carries everything.
- `digest_bytes` tracks the injected digest, so closing findings lowers it, while `max_findings`
  still counts closed findings and can still refuse a runaway session.
- A ledger written without the new fields loads and reports `null`; **a ledger holding
  `"regressed"` loads as `Invalid` and refuses the resume** — the deliberate break, asserted rather
  than discovered.
- **The issue's reproduction:** two findings open from turn 4; on turn 5 one is listed `resolved`
  and the other omitted. The first closes and leaves the digest; the second stays open with
  `last_verified_turn: 4` against `turn: 5` — so the caller can tell them apart, which is the whole
  claim.
