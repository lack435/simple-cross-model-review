# Stale `open` findings on a resumed session — design

Status: **proposal, revision 5.** Filed against issue #62 ("Resumed review session freezes findings
at a stale `open` status (ledger not re-evaluated)").

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
> The maintainer supplied the missing counterweight, and it is recorded here as a standing principle
> rather than an aside:
>
> > **Rigor belongs where the blast radius is real — account management, credentials, the read-only
> > and write boundaries. For a review, the worst case is that it is lost and re-run, costing tokens
> > and time. Avoid that; do not fortify against every edge case to prevent it.**

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

**Part of the symptom is #73.** A structured turn returns `review_prose: null`, so a client
surfacing only `structuredContent` never sees anything the reviewer said outside its findings list —
the entire eight-round #71 review ran without the caller reading a line of prose. A reviewer that
holds a finding open *and explains why* is indistinguishable from one that froze it silently. Some
unknown share of #62 is an unread explanation rather than an absent one, and #73 is smaller than any
version of this change.

## The change

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
