# Structured findings + machine-readable verdict — design

Status: **planned.** This document is the plan. It goes through this repository's own
`cross-review` gate before implementation begins, and the implementation goes through it again.

Tracks issue #40. The issue asks for a machine-readable envelope so that an
autonomous "re-review until converged" loop stops re-deriving structure the tool could
provide: a top-level verdict, an `open_count`, stable finding IDs carried across a resumed
session, and per-finding status (resolved / open / regressed) on a re-review.

> **Review history.** Twenty-two rounds against this repository's own gate (the 22nd returned APPROVE) (Codex, gpt-5.6-luna,
> effort=max), each REQUEST CHANGES, each finding accepted and none disputed: round 1 (seven
> `major`), round 2 (four `major` + one `minor`), round 3 (four `major` + one `minor`), round 4
> (five `major` + one `minor`), round 5 (five `major` + one `minor`), round 6 (three `major` +
> three `minor`, consistency gaps from the round-5 edits), round 7 (one `major` — a mid-session
> degraded turn — plus two `minor`), round 8 (one `major` + two `minor`, propagation gaps from the
> round-7 edits), round 9 (one `major` + two `minor`, edge-case tightening), round 10 (one `major` + one `minor`,
> two stale table rows from the round-9 rewrite), round 11 (a fresh-reviewer holistic pass, four
> `major`), round 12 (four `major` + one `minor`, carrying round 11 through the tables and schema), round 13
> (two `major` + two `minor`, the `unestablished` edge), round 14 (two `major` + two `minor`, stale
> four-state summaries), round 15 (two `major` + one `minor`, precedence + last blind-fresh strings), round 16 (one `major`,
> a stale re-report, tightened further), round 17 (one `major` + one `minor`, the `invalid`
> resume-refusal split), round 18 (one `major`, propagating that split to Decision 3 and the tables),
> round 19 (one `minor`, qualifying persistence-failure reasons against already-broken coverage),
> round 20 (one `minor`, truth-table footnote 4), round 21 (a fresh-reviewer pass, one `major`: the
> corrupt-store one-shot path). Rounds 1–20 confirmed resolved; round 21 now closed. Recorded in the
> [Round 1](#round-1-response) … [Round 21](#round-21-response) responses. The sections they touched —
> the block schema, ID reconciliation (total-accounting), degradation and the `converged` signal
> (now including the budget/terminal condition), extraction (nonce-marked, dual-namespace, `_OUT`
> nonce), durability (a write-ahead marker that must succeed or abort, a fail-closed
> derived-at-load `invalid` state, and a sticky `terminal_reason`), the whole-conversation ledger
> rule (a one-way coverage state machine stored *inside* the ledger), tri-state store load with
> record-vs-store `fresh` semantics, the
> block-as-sole-machine-source contract, bounded growth with atomic over-cap persistence and
> deterministic reason precedence, and a discriminated running/completed `outputSchema` — were
> rewritten rather than patched. This document reflects the post-round-6 design.

## Problem

A review comes back as prose — a `## Verdict` line and a `## Findings` section of
`critical / major / minor` items. To drive the act-on-feedback loop the merge gate itself
runs, the calling agent has to:

- **grep the prose for the verdict** to decide whether to stop, and
- **invent its own finding numbering** (F1…Fn) and **hand-maintain a cross-round map** of
  which finding is resolved, still open, or regressed — because the tool renumbers nothing
  and reports status only in prose.

The issue was filed from dogfooding: across the 13 rounds of the Perforce resume-delta
review, that bookkeeping was a large share of the per-round effort and it is error-prone.
The model tracking convergence is re-deriving structure the tool already has in hand.

This is worse than tedious. An autonomous loop that decides "are we done?" by string-matching
`APPROVE` in a prose blob is one wording change away from either looping forever or merging
with open majors. The termination signal for the whole merge gate should not be a substring
search — and, as round 1 made sharp, it must not be reachable from a review that produced no
trustworthy structure at all.

## What "done" means (from the issue's acceptance)

1. An agent can loop "re-review until converged" **without string-matching the body**.
2. A resumed review reports **each prior finding's status by stable ID**.

Everything below serves those two sentences. Where a choice is open, the tie-breaker is
*foundational completeness and robustness* over minimising blast radius — the explicit
priority for this work.

## The single safe termination signal: `converged`

Round 1's first finding is the spine of the whole design, so it is stated before anything
else. **The loop must not terminate on `open_count == 0` directly.** A degraded turn — no
valid block, a corrupt ledger, a persistence failure — could otherwise present `open_count:
0` without any structure behind it and be read as success.

So the server computes one boolean, **`converged`**, and that is the only signal a loop is
told to stop on. `converged == true` is defined as the conjunction — there is no other
definition of it anywhere in this document, and where an earlier draft said "structured
convergence is `open_count == 0`" that was a round-2 self-contradiction and is now removed.
`converged == true` requires **all** of:

- **A ledger that covers the whole reviewer conversation**, recorded as a durable, persisted
  `ledger_coverage == whole_conversation` — not inferred per-turn from whether a ledger is
  present. A conversation that predates the ledger is stamped `legacy_uncovered` and is *never*
  convergeable — see [the whole-conversation rule](#convergence-requires-a-ledger-over-the-whole-conversation).
- the reviewer emitted exactly one valid, well-formed machine block this turn
  (`structured == true`), and
- the session's finding ledger is present and readable at a compatible version, and
- reconciliation completed without any fail-closed condition (below: unknown/duplicate id, a
  schema violation, **or any prior id the reviewer failed to account for**), and
- every finding in the ledger has `status == resolved`, so `open_count == 0`, and
- the reviewer's own `verdict_detail` is exactly `approve` — not `approve_with_comments`, not
  `request_changes`, not `blocked`. The reviewer's own top-level judgement must itself be a
  clean approve; numeric zero is necessary but not sufficient, and
- **the session is in no sticky terminal state** — not `ledger_too_large` (over the bounded
  budget) and not any future terminal escalation. Round 6's point: a session can have zero open
  findings *and* be over budget (many resolved findings still counted), so the budget condition is
  part of the conjunction, not merely something the post-cap path asserts separately. If the two
  disagreed, a clean-but-over-budget turn could satisfy the predicate while the post-cap path forced
  `converged: false`; folding it in removes that contradiction.

If any one is false, `converged` is `false`, and the envelope carries a machine-readable
`non_convergence_reason` (below) naming which one, so an autonomous loop can tell "keep working,
findings still open" from "the reviewer withheld a clean approve, a human should look" and
**cannot livelock silently**. `open_count` is a number **only** when the block is structured,
the ledger is valid, and reconciliation was clean; otherwise it is `null`, never `0`. A caller's
safe loop is **`while (!converged)`, re-reviewing on `open_findings`/`verdict_contradiction` and
*stopping to escalate to a human* on every other reason** — it never treats non-convergence as an
autonomous "just `fresh` and keep going" (that is the round-11 hole; see
[the reason table](#non_convergence_reason) and the rebaseline handoff). It still never has to
reason about *why* beyond that two-way split to stay safe.

### `non_convergence_reason`

A non-convergent turn is not necessarily a *broken* turn, and an autonomous loop must be able
to act differently on the two. The envelope carries one machine-readable reason (or `null` when
`converged`):

| `non_convergence_reason` | Meaning | What a loop should do |
| --- | --- | --- |
| `open_findings` | Structured, valid, but `open_count > 0`. | Act on the findings, re-review. |
| `reviewer_withheld_approve` | `open_count == 0` but `verdict_detail` is `approve_with_comments`. | The reviewer left only comments and declined a clean approve. Do **not** auto-stop; surface to a human. |
| `reviewer_blocked` | `verdict_detail` is `blocked` (the reviewer reports it cannot complete a clean review), at any `open_count`. | **Escalate to a human**; a blocked reviewer is not a re-review-and-hope state. |
| `verdict_contradiction` | Reviewer verdict and `open_count` disagree (e.g. `approve` with open findings, or `request_changes` with none). | Treat as changes; re-review. |
| `ledger_unavailable` | On-disk coverage is a *readable* persisted break — `legacy_uncovered` or `needs_rebaseline` — whether from **this turn's coverage-break write persisting** *or* the **session already being broken on entry**. (A degraded turn whose *own* break-write failed reports `turn_not_durable` **only** when it entered `whole_conversation`/`unestablished`; on an already-broken session `ledger_unavailable` holds regardless — precedence. An *unreadable* ledger — `invalid` — never reaches a completed envelope; it is a `SESSION_NOT_RESUMABLE` refusal carrying `ledger_unavailable` as its detail. See the persistence-first rule and the `invalid` note below.) | **Escalate to a human** for a rebaseline decision — do **not** autonomously `fresh`; a blind `fresh` can converge while abandoning the untracked findings that broke coverage (below). |
| `turn_not_durable` | This turn's ledger/coverage could not be persisted (`.findings-pending` sidecar left set); reported when on-disk coverage was not already broken — `whole_conversation` on entry, or a fresh turn 1 with no persisted coverage yet. The *prior* ledger, **if one exists**, is intact on disk. | **Escalate**, then a human/caller may `fresh` **carrying the preserved prior open findings** (only this turn's incremental output is at risk; a fresh turn 1 has none). |
| `state_corrupt` | The session store itself did not parse. Like `invalid`, this is caught at load, so it is a **pre-model `SESSION_NOT_RESUMABLE`-class refusal** carrying `state_corrupt` as its detail — **never a completed-envelope `non_convergence_reason`** (round 21); no review runs. | **Escalate to a human**; operator moves the corrupt store aside / uses `--state-dir`; do not silently start fresh (below). |
| `ledger_too_large` | The ledger/digest exceeded the bounded budget, before *or* after this turn ran. | **Escalate to a human**; the session cannot keep growing (below). |

`unstructured` is **not** a value of `non_convergence_reason` (round 8). "No valid block this turn"
(missing/duplicate/extra id, malformed block, wrong nonce, …) is carried by `structured: false` and
the warning detail; the *top-level reason* is never `unstructured`, because a degraded turn breaks
coverage and reports **`ledger_unavailable` when the on-disk coverage ends up a persisted break
(this turn's break persisted, or the session was already `legacy_uncovered`/`needs_rebaseline`), or
`turn_not_durable` only when it entered `whole_conversation`/`unestablished` and its own write
failed** (the persistence-first rule) — never `unstructured`. Keeping
`unstructured` out of the reason enum is what stops a loop being told to "re-review" a session that
a rebaseline must repair.

The `unestablished` coverage state (round 13) is **not** a `ledger_unavailable` case: it only ever
appears as the *pre-turn* coverage on a `turn_not_durable` turn (a fresh turn 1 whose write failed),
so it is reported *with* `turn_not_durable`, never as `ledger_unavailable`. The two *readable* broken
states — `legacy_uncovered`, `needs_rebaseline` — map to a completed-envelope `ledger_unavailable`;
the *unreadable* `invalid` state carries `ledger_unavailable` as the **detail of a
`SESSION_NOT_RESUMABLE` refusal** instead, since an unreadable ledger is caught before a turn can
run (round 17, the `invalid` note).

**None of the non-convergent reasons is an autonomous "just `fresh` and continue" signal** (round
11 closed that hole): `open_findings` and `verdict_contradiction` say re-review; **every other
reason is a human-escalation outcome** — `reviewer_withheld_approve` (the reviewer will keep
withholding), `reviewer_blocked` (the reviewer declared it cannot finish), `state_corrupt`,
`ledger_too_large`, `turn_not_durable`, and **`ledger_unavailable`**. The last is the subtle one:
telling an autonomous loop to blindly `fresh` on a coverage break re-introduces the silent drop the
coverage rule exists to prevent — a `fresh` review of the current code can converge while the
untracked prose finding that broke coverage is simply forgotten. So `ledger_unavailable` escalates
to a **human rebaseline decision** (next section), not an automatic restart. A caller should also
escalate on **repeated degraded turns** and on `SESSION_NOT_RESUMABLE` — both covered in
[the escalation policy](#bounded-growth-and-escalation-outcomes).

**Precedence is deterministic** (round-4 minor): more than one condition can hold at once — a
`legacy_uncovered` session can also have open findings; a persistence failure can coexist with a
degraded block — and reporting the wrong one would tell a loop to "re-review" a session that
can only be repaired by an escalation → rebaseline, so it would loop forever. The single reported
reason is the
**first that applies, in this fixed order of decreasing gravity**, so the advice is always the
most conservative applicable action:

The **persistence-first rule runs *before* precedence**: a degraded turn whose coverage-break write
*failed* is classified `turn_not_durable` (coverage unchanged on disk, `unestablished` on a fresh
turn 1), and only a degraded turn whose break *persisted* becomes a `ledger_unavailable` case. Once
each turn's reason is classified that way, precedence picks the first that applies:

This precedence is over the reasons a **completed envelope** can carry — `state_corrupt` and
`invalid` are *not* among them (both are pre-model refusals, caught before any completed envelope
exists), so they never enter this ordering:

`ledger_too_large` → `ledger_unavailable` (the completed-envelope readable-break states
`legacy_uncovered` / `needs_rebaseline` — *not* a degraded turn whose own write failed, which is
`turn_not_durable`) → `turn_not_durable` → `reviewer_blocked` → `verdict_contradiction` →
`reviewer_withheld_approve` → `open_findings`.

A permanently-uncovered session therefore always reports `ledger_unavailable` (**escalate →
rebaseline**), never `open_findings` ("re-review"), no matter how many findings it also has — so the
loop cannot be lured into re-reviewing a session it can never converge.

The `reviewer_withheld_approve` row is the livelock guard round 2 asked for: the loop is not
told "done," but it *is* told exactly why it is not done, so it stops spinning and escalates
rather than re-reviewing forever against a reviewer that will keep leaving the same comment.

## The three hard parts

Structure is easy to ask for and hard to make trustworthy. Three problems have to be solved
together, or the envelope is a liability rather than a convenience:

1. **Where does the structure come from?** The reviewer is a model emitting text. Either the
   server parses its prose into structure, or the reviewer emits the structure directly.
2. **Who owns finding identity, and how does it stay stable and *trustworthy* across rounds?**
   A stable ID is only stable if something remembers it between turns; it is only trustworthy
   if echoing it back cannot silently retarget it.
3. **What happens when the structure is missing, malformed, or contradicted?** A review that
   produced good prose but no valid machine block must still come back as a review, not an
   error — but it must never be able to reach `converged`.

### Decision 1 — the reviewer emits the structure; the server does not parse prose

The reviewer is instructed to emit, in addition to its prose, a single machine block
containing its findings and verdict in a defined schema, delimited by exact sentinel markers.
The server locates and validates that block. It does **not** reverse-engineer structure from
the `## Findings` markdown.

**The block contract is rendered on every turn, not once.** Round 3 caught a real bug in the
round-2 draft: it put the block instructions in the *preamble*, which `src/prompt.rs` emits only
on turn 1 (`resumed == false`). But the block must be emitted on *every* turn, the per-review
nonce (Decision 4) *changes* every turn, and the "account for every prior id exactly once"
contract (Decision 2) only exists *on* resumed turns — so the full block contract (current
nonce, the two-array schema, exactly-one-block, total id accounting) is part of the **per-turn
request section that renders on turn 1 and every resume alike**, not the once-only preamble. The
role-setting preamble stays turn-1-only; the machine-block instructions do not.

Why not parse the prose:

- Two different models (Codex reviewing Claude, Claude reviewing Codex) format prose
  differently, and both are free to reword their own headings. A parser tuned to one drifts
  against the other and against the next model version. This project pins models by full id
  precisely because model behaviour moves; a prose parser would re-introduce exactly that
  fragility one layer up.
- Severity and status are *judgements*, and the model is the authority on them. Asking it to
  state `"severity": "major"` in a field is more faithful than inferring "major" from where a
  sentence sat in a bulleted list.
- A delimited block is trivial to locate and cheap to validate; the prose stays untouched as
  the human-readable rendering and as each finding's `detail`.

Why not a *second* model call to extract structure from the prose: this project's whole
character is one call, one response, `serde` as the only dependency. A structuring pass would
double the model cost of every review and add a second failure surface. The reviewer emits
the block in the same turn it writes the prose.

> **Amended for issue #63** (see [`unstructured-turn-recovery.md`](unstructured-turn-recovery.md)).
> There *is* now a second call, on one path: when a turn produces no usable block, the server asks
> the same reviewer, in the same conversation, to re-emit the block alone. The reasoning above still
> holds and is what bounds it — it is not a structuring pass, it introduces no second model, it does
> not read prose, it runs only on a degraded turn, and it cannot fail the review (a failed repair
> returns the same degraded envelope the turn would have had). But "one call, one response" is no
> longer true of every turn, and this document does not claim it is.

**The block is the *sole* machine-authoritative source of findings and verdict — and the prompt
makes that a contract, not a hope.** Round 4 named a real, non-injection failure: the reviewer is
told to emit the block "in addition to" prose, and the server does not parse prose, so a response
whose prose lists a `Major` under `## Findings` while the block says `approve` with nothing open
would be *retained as prose* yet compute `converged: true` — a silent disagreement between the two
representations, exactly the "never silently dropped" goal inverted. Because re-introducing a
prose parser to cross-check is the fragility this decision exists to avoid, the resolution is at
the contract layer plus honest scoping:

- **The prompt states the block is authoritative and must be complete**: *every* finding the
  reviewer raises anywhere in its prose must have a corresponding `new_findings` or
  `prior_findings` entry, and the block's verdict must match the prose `## Verdict`; a prose
  finding with no block entry, or a verdict mismatch, is a contract violation on the reviewer's
  part. The reviewer here is a cooperative strong model (not the untrusted repository), so this
  instruction is a reasonable primary control — categorically different from the injection case,
  where the adversary is the repo.
- **Convergence is scoped honestly to what it certifies.** `converged: true` means *the structured
  contract passed* — it is **not** a claim that a human read the prose and agreed. The README and
  the tool text say exactly that, and the human prose is always returned so a person can still read
  it. An autonomous loop is told, in the docs, that `converged` certifies the machine contract and
  that a human sign-off on the prose is a separate step it should not assume.
- The residual is stated, not hidden: the server cannot *detect* a reviewer that violates the
  completeness contract without parsing prose, so this narrows the risk to reviewer non-compliance
  and does not claim to eliminate it. That is the same discipline applied to the injection residual
  in Decision 4.

### Decision 2 — the server owns finding identity *and its content*; the reviewer only supplies status for prior findings

This is the core of the design and, per round 1, the part most easily got wrong. Two round-1
findings (paraphrase/retarget under a stable ID; new findings keeping a `resolved` status)
share one root cause: letting the reviewer resupply the content of a finding it is only meant
to *status*. The fix is a hard split.

The server keeps a **per-session findings ledger**, persisted in the session record next to
the existing resume state. Each ledger entry is **server-owned and immutable once created**:
a stable id (`f1`, `f2`, … monotonic per session, never reused), the severity / title /
location / `detail` **as captured on the turn it was first raised**, its current status, and
the turns it was first and last seen on. The reviewer never rewrites any of those fields; it
can only move a finding's *status*, and only for an id the server already owns.

The reviewer's block is therefore split into **two disjoint arrays**:

- **`prior_findings`** — objects of exactly `{ "id": "f3", "status": "resolved" | "open" |
  "regressed" }`. Status only. No title, no detail, no severity: the server already holds
  those and renders its own copy. A `prior_findings` entry can carry *no* other field.
- **`new_findings`** — objects of `{ "severity", "title", "file", "line?", "detail" }`, with
  **no `status` field at all**. Every new finding is forced `open` by the server on creation.
  The schema does not give a new finding anywhere to claim it is already resolved.

Reconciliation, deterministic and fail-closed:

**Turn 1 (fresh session).** There are no prior findings. The server assigns `f1…fn` to the
`new_findings` in block order, records them `open`, returns them.

**Turn N (resumed session).** The server already holds the ledger. Before the review it
injects into the prompt **every ledger finding that has ever been raised — open *and*
resolved** — as `id`, `title`, `location`, `severity`, and current status, rendered as quoted
evidence (see Decision 4). Resolved findings are included precisely so a regression can be
reported as `f1: regressed` rather than being forced to appear as an unrelated new finding —
round 1 caught that the earlier "inject only open findings" draft made the `regressed` status
unreachable for anything already resolved. After the review, the server reconciles:

- **`prior_findings` must account for every id the ledger owns — open *and* resolved — exactly
  once.** This is the round-2 fix, and it is load-bearing. An earlier draft flagged only omitted
  *open* findings and let omitted resolved ones keep `resolved` silently; that left a hole where
  a finding regressed but unmentioned would keep every ledger entry resolved, the block would
  validate, and `converged` could go true. So the contract is total: the resumed reviewer is
  handed the full id list and must return a status for **each** one. A single missing id, or one
  extra id the ledger never issued, or any id twice, degrades the whole turn (below). There is
  no "the reviewer stayed silent on `f1`" path any more — silence on any id is a degraded turn,
  never an implied status.
- **Each `prior_findings` entry's** status is applied to its ledger entry; the entry's content
  is untouched. A resolved entry the reviewer now reports `regressed` reopens under its original
  id (this is why the digest includes resolved findings — round 1).
- **`new_findings`** get fresh monotonic ids, `status: open`, and are appended.
- **Fail-closed conditions that degrade the *entire* turn** (→ `structured: false`, and — being a
  degraded turn — a coverage break to `needs_rebaseline`/`legacy_uncovered` reported as
  `non_convergence_reason: ledger_unavailable` **when the on-disk break is persisted (this turn's, or
  an already-broken session), else `turn_not_durable`** — see Decision 3, the coverage rule, and the
  persistence-first rule): the set of
  ids in `prior_findings` is not *exactly* the ledger's id set (a missing id, an extra/unknown id,
  or any duplicate); a `prior_findings` object carrying any field other than `id`/`status`; a
  `new_findings` object carrying a `status`; any enum out of range. The block is trusted as a whole
  or not at all — a block the server had to *partially* discard is a block it cannot reason about,
  and round 1 was right that "ignore the bad entry with a warning" is exactly how a serious finding
  silently leaves `open_count`. Whole-turn is the correct granularity precisely because per-finding
  salvage is what re-opens that hole.

Reconciliation is therefore **keyed by explicit server-owned ids the reviewer is handed and
hands back, and the reviewer cannot rewrite what an id means** — only report its status. The
model, which raised the concern, is the authority on whether it is resolved; the server, which
owns the counter and the content, is the authority on identity. Neither guesses at the other's
job, and neither can do the other's.

**Merges and splits** now have a defined answer rather than an undefined one. The reviewer
cannot merge two ids into one or split one id into two, because it cannot mint or retire ids.
If it believes two prior findings are really one, it statuses both (e.g. resolve one, keep the
other open); if it believes one prior finding is really two, it keeps the prior id and files
the second aspect as a `new_finding`. The ledger's history stays a faithful, append-mostly
record; nothing is silently collapsed.

**Idempotency of id assignment.** Given the same prior ledger and the same block, id
assignment is a pure function (next-counter in block order), so re-running reconciliation on
an identical input is identical. The dangerous case — the reviewer's remote conversation
having advanced while the local ledger did not — is handled in Decision 5, not here.

### Decision 3 — the envelope always returns and degrades safely; degradation can never reach `converged`

A review that produced usable prose must never be turned into a tool error because its machine
block was absent, malformed, or contradicted. The failure codes in this project are reserved
for the reviewer being *unavailable* (`NOT_AUTHENTICATED`, `RATE_LIMITED`, `TIMEOUT`, …); "the
review happened but was unstructured" is not that.

So the **completed-result** envelope is always returned, with a `structured` boolean and the
`converged` signal from the top of this document. (This whole decision is about *completed*
results; a *running* result is the reduced variant with no convergence/findings group at all —
see [one schema for every successful response](#one-schema-for-every-successful-response). Wherever
this document says "the envelope always has `converged`," read it as the completed variant.)

Decision 3 is about **degradations that occur *after* a turn begins** — the reviewer ran, but its
output could not be fully trusted. A prerequisite failure that stops a turn from beginning at all —
notably an **unreadable/incompatible ledger, which is caught at load and is a `SESSION_NOT_RESUMABLE`
resume refusal** (round 17), not a completed envelope — is *not* in scope here; there is no turn and
no prose on that path. With that boundary:

- **Exactly one valid block, ledger present and compatible, reconciliation clean** →
  `structured: true`, findings and verdict from the block, `open_count` numeric, `converged`
  computed as defined.
- **Any degradation *of a turn that ran*** — no block, more than one candidate block,
  unparseable/invalid block, a fail-closed reconciliation condition, or a turn whose ledger could
  not be persisted (Decision 5, `turn_not_durable`) → the envelope still returns, but with
  `structured: false`, `open_count: null`, `verdict` **never `approve`** (it is reported as
  `unknown`), `converged: false`, and a warning naming which degradation occurred. The prose is
  returned in full on the text channel regardless, exactly as today — and since issue #63 a capped
  copy also rides the structured channel in `review_prose`, so a `structuredContent`-only client is
  not left with an empty `findings` list and nothing to read. (An *unreadable ledger* is not in this list — it is
  the pre-model resume refusal above, not a completed degraded envelope.)

The invariant round 1 demanded, made structural: **a numeric `open_count` and a `true`
`converged` are reachable only through the fully-validated path.** No degraded path can
produce a zero or a convergence, because both are computed from ledger state that the degraded
paths mark unavailable. An autonomous loop can trust `converged == true` to mean **everything
represented in the validated machine contract is resolved** — not, more strongly, "everything the
reviewer raised anywhere," since a reviewer that violates the completeness contract by writing a
prose finding it omits from the block is undetectable without a prose parser (Decision 1). The
guarantee is exactly as wide as the structured contract and no wider, and this document does not
claim otherwise; nothing in the pipeline can manufacture that boolean without the structure behind
it.

Robustness rule that makes even the *structured* path safe: **`verdict` and `open_count` are
kept consistent by the server, deterministically.** `converged` is **not** "`open_count == 0`";
it is the full conjunction defined at the top of this document — validated structure, valid
ledger over the whole conversation, clean reconciliation, `open_count == 0`, *and* a clean
`approve` from the reviewer. (Round 2 caught the earlier draft asserting the shorthand here
while the truth table required the reviewer's verdict too; the shorthand is gone.) If the
reviewer's block claims `verdict: approve` while the ledger still has open findings, that is a
contradiction; the server resolves it in favour of the ledger (machine verdict `changes`,
`converged: false`, `non_convergence_reason: verdict_contradiction`) and warns. If the reviewer
reports `approve_with_comments` with nothing open, that is not a contradiction but it is not a
clean approve either: `converged: false`, `non_convergence_reason: reviewer_withheld_approve`.
A loop cannot be talked into stopping by a reviewer that wrote `approve` at the top and left a
`major` open below, nor left spinning silently by one that only ever comments.

### Decision 4 — extraction is fail-closed on ambiguity, and the ledger is injected as quoted evidence

Round 1 was right that "a fenced ```json block" is underspecified: a review can contain
several JSON fences, quoted examples, or a sentinel copied out of the repository under review.
So:

- The reviewer's block uses the **`CROSS_REVIEW_FINDINGS_IN` input marker** (disjoint from the
  server's `CROSS_REVIEW_ENVELOPE_OUT`, see [the envelope](#the-envelope)), delimited by **exact
  begin/end sentinel lines carrying a per-review nonce**, on their own lines — not by a bare
  ```` ```json ```` fence, and not by a fixed marker either. The
  nonce is derived from the server's unique review id (ids are `rv-<pid>-<counter>`, so each
  review's markers differ) and is placed in the prompt only, never in the repository. This
  defeats the round-2 case of a repository embedding a *static* lookalike sentinel in its own
  source and having it quoted back as the sole block: a fixed marker could be forged from source
  the Codex reviewer can read; a per-review marker cannot be known ahead of the review.
  **Honest limit:** the nonce is in the prompt the reviewer sees, so it stops a *static* embed,
  not an *active* prompt-injection that reads the current turn's nonce out of its own context and
  reproduces it. It is defence-in-depth over the exactly-one-block rule, not a substitute for it.
- Extraction requires **exactly one** complete, well-formed sentinel block bearing this review's
  nonce. **Zero → degrade** (no structure this turn). **More than one → degrade** (fail-closed on
  ambiguity; the server does not pick "the first" or "the last"). A block that opens and never
  closes → degrade.
- The block body has a **strict size cap** and per-field length caps; over-cap → degrade. This
  is defence against a block that is technically parseable but pathological.
- The prior-findings digest the server injects on a resumed turn is rendered as **evidence,
  labelled as evidence, not as instructions** — exactly as the change capture and CLAUDE.md are
  (README's "labelled as evidence, not instructions"). It carries ids, titles, locations and
  statuses. Note what this does and does not claim: the *titles and locations are themselves
  model-authored data* from earlier turns, so the labelling frames them as evidence and cannot
  guarantee their content never reads as a directive — the guarantee is the framing and the
  fencing, not that model-authored strings are inherently inert. That is the same honesty the
  README already applies to the diff capture, and the same residual below applies.

The extractor treats the reviewer's block as *more trusted than the repository* but still
validates defensively: it is model output shaped by a prompt the server controls, but "more
trusted" is not "unvalidated," and every malformed shape degrades rather than partially
loading.

**Residual risk, stated not hidden.** Because status for a prior id comes from the reviewer's
judgement, a successful prompt-injection in the repository under review could in principle
steer the reviewer to report an open finding as `resolved`. This design *narrows* that surface
— the reviewer cannot rewrite a finding's content or mint ids, only move status on ids the
server owns, and the prior ledger is injected as quoted evidence — but it does not claim to
*close* it, because the reviewer's read access to the (untrusted) repository is the whole point
of the Codex direction and cannot be removed without removing the reviewer. Two things follow,
and both are load-bearing: (1) the confined **Claude reviewer direction** (no shell, reads
scoped to the project) is the one to point at a repository you do not trust, exactly as the
README already says; and (2) the human prose is always returned, so a falsely-resolved finding
is still visible to a person even on a turn the machine verdict got wrong. The envelope is a
convenience over the prose, never a replacement for it, and the design says so rather than
implying the machine verdict is incorruptible.

## The envelope

Delivered on **two channels at once**, because the client population is mixed and the change
must not break the prose channel anything already depends on:

1. **`structuredContent`** on the MCP tool result. The negotiated protocol here is
   `2025-06-18`, which defines `structuredContent`; clients that ignore it are unaffected, and
   the tool definition gains an `outputSchema` describing the envelope.
2. **The server's canonical envelope appended to the text content**, delimited by a
   **distinct server-output marker** — *not* the reviewer-input marker. Round 3 caught that
   reusing one marker namespace would put two nonce-bearing blocks in the text channel (the
   reviewer's own raw block plus the server's), breaking the exactly-one contract and leaving a
   client unable to tell which JSON to parse. Two defences, both applied:
   - **The reviewer-input block is stripped from the stored/rendered prose.** Once the server has
     extracted and validated the reviewer's block, that raw block is removed from the prose it
     renders and stores; the human prose keeps the reviewer's `## Findings` narrative but not the
     machine block, which was only ever a transport detail between reviewer and server.
   - **The two marker namespaces are disjoint, and *both* carry the per-result nonce.**
     `CROSS_REVIEW_FINDINGS_IN` is what the *reviewer* emits and the extractor parses;
     `CROSS_REVIEW_ENVELOPE_OUT` is what the *server* emits into the text channel. Round 4 caught
     that a *fixed* `_OUT` marker is itself forgeable — a repository can contain the literal
     `_OUT` marker and the reviewer can quote it into prose, and since only `_IN` is stripped, a
     client would see a lookalike `_OUT` block *plus* the real server block and could not tell
     which to parse. So the `_OUT` marker carries the **same per-result nonce** as `_IN`, and the
     server, before it emits, **strips or neutralises any lookalike `CROSS_REVIEW_ENVELOPE_OUT`
     marker line from the prose it is about to include** (the prose is model/repo-derived and
     therefore untrusted for this purpose). "Exactly one block" is enforced per namespace against
     *this result's nonce*: exactly one nonce-matching input block from the reviewer (or degrade),
     and exactly one nonce-matching output block from the server (always). A client parses the
     single `_OUT` block bearing the result's nonce.

   The human prose (`--- BEGIN REVIEW --- … --- END REVIEW ---`, now with the reviewer's raw
   machine block stripped) and the existing status/usage/warning header stay exactly as they are,
   above the server envelope.

```jsonc
{
  "schema_version": 1,
  "session": "auth-refactor",
  "turn": 3,
  "result_status": "completed",  // present so one schema covers running/completed (see below)
  "structured": true,            // false on any degradation (Decision 3)
  "converged": false,            // the ONLY safe loop-termination signal
  "non_convergence_reason": "open_findings",  // null iff converged; see the reason table
  "verdict": "changes",          // machine verdict; allowed: "approve" | "changes" | "unknown"
  "verdict_source": "structured",   // "structured" or "none"; the server never infers a verdict from prose
  "verdict_detail": "request_changes",  // the reviewer's own call: approve | approve_with_comments | request_changes | blocked | null
  "ledger_coverage": "whole_conversation",  // completed-envelope states: whole_conversation | legacy_uncovered | needs_rebaseline | unestablished (invalid is a resume-refusal, never here — see below)
  "findings_trusted": true,      // false when ledger_coverage is unestablished; when false, `findings` is [] and this turn's prose must be used
  "open_count": 2,               // count(status != resolved); NUMBER only when structured+valid+clean, else null
  "total_count": 3,              // count(all findings ever raised, any status); null when unstructured
  "findings": [
    {
      "id": "f1",
      "severity": "minor",       // "critical" | "major" | "minor"
      "status": "resolved",      // "open" | "resolved" | "regressed"
      "title": "Missing rustdoc on the public refresh API",
      "file": "src/auth/mod.rs",
      "detail": "<the reviewer's prose for this finding, as first captured>",
      "first_seen_turn": 1,      // turn the id was minted
      "last_status_change_turn": 2   // last turn the status changed (here: resolved on turn 2). NOT "last reported" — total-accounting reports every id every turn
    },
    {
      "id": "f2",
      "severity": "major",
      "status": "open",
      "title": "Refresh token not revoked on password change",
      "file": "src/auth/token.rs",
      "line": 88,
      "detail": "<...>",
      "first_seen_turn": 2,
      "last_status_change_turn": 2   // raised on turn 2, still open, unchanged since
    },
    {
      "id": "f3",
      "severity": "major",
      "status": "open",
      "title": "Race between refresh and revoke",
      "file": "src/auth/token.rs",
      "line": 129,               // optional; omitted when the finding is not line-scoped
      "detail": "<...>",
      "first_seen_turn": 1,
      "last_status_change_turn": 1   // open since turn 1, never changed
    }
  ],
  "warnings": []                 // carries contradiction/degradation notes when they occur; empty on a clean structured turn
}
```

This is a **completed, structured, non-convergent** example, and it is **internally consistent**:
three findings ever raised (`total_count: 3`), two of them non-resolved (`open_count: 2`, matching
`count(status != resolved)`). The reviewer returned `request_changes`, so `verdict` is `changes`,
the reason is `open_findings`, and `converged` is `false`. `ledger_coverage` is
`whole_conversation`, so `findings_trusted` is `true`. A *contradiction* example — the reviewer
says `approve` while findings are open — would instead show `verdict: "changes"`,
`non_convergence_reason: "verdict_contradiction"`, and a warning naming the disagreement (the
truth table below). The `verdict` field never disagrees with the findings: an `approve` that
contradicts open findings is resolved to `changes` before the envelope is built, never emitted raw.

Notes on the shape:

- **`converged` is the loop signal.** `verdict` is retained as the human-facing two-value
  summary the issue named, but the *machine* stop condition is `converged`, which folds in
  structured-ness and ledger validity so a caller cannot accidentally stop on a bare zero.
- **`verdict_detail`** preserves the reviewer's richer four-level judgement (`APPROVE / APPROVE
  WITH COMMENTS / REQUEST CHANGES / BLOCKED`) so nuance is not lost — a reviewer can
  approve-with-comments and the loop still sees the `minor`s in the findings list. See the
  [verdict truth table](#verdict-truth-table) for the complete mapping.
- **`open_count` is exactly `count(status != resolved)`** — i.e. `open` *and* `regressed` both
  count (round-4 minor). Stated as a formula so an implementation cannot count only `open` and
  report zero while a finding is `regressed`. `total_count` is `count(all findings ever raised)`.
- **`open_count` / `total_count` are `null`, not `0`, when unstructured.** This is the
  round-1 fix in the wire shape itself: absence is distinguishable from zero.
- **`first_seen_turn` / `last_status_change_turn`** (round 17): `first_seen_turn` is the turn the id
  was minted; `last_status_change_turn` is the last turn on which the reviewer *changed* this
  finding's status (raised, resolved, or regressed). It is deliberately **not** "last turn reported"
  — total-accounting (Decision 2) makes the reviewer restate *every* id *every* resumed turn, so a
  "last reported" turn would always equal the current turn and carry no information; "last status
  change" is the useful signal (e.g. how long a finding has sat open).
- **`ledger_coverage` and `findings_trusted` are the rebaseline discriminator** (round 11 required
  the recovery handoff; round 12 required it be a *machine* field, not just prose). In a **completed
  envelope** `ledger_coverage` is one of **four** run-reachable states — `whole_conversation` |
  `legacy_uncovered` | `needs_rebaseline` | `unestablished` — the states a turn that *actually ran*
  can be in, so a caller can tell *which* kind of state it is in without parsing text. **`invalid` is
  deliberately *not* a completed-envelope value** (round 17): an unreadable ledger is caught at
  **load time, before any reviewer runs**, so it is a **`SESSION_NOT_RESUMABLE` resume refusal**
  (an error result with a machine-readable remediation), not a completed envelope — there is no turn
  and no prose on that path. `unestablished` (round 13) is the honest value for a turn that ran but
  **persisted no coverage**: a fresh turn 1 whose `record_turn` failed (`turn_not_durable`) — *not*
  `whole_conversation` (that write didn't land) and *not* `legacy_uncovered` (no such transition
  persisted).
- **`findings_trusted`** is `true` **only** when `findings` is a complete, durable set the caller
  can rebaseline from — `ledger_coverage ∈ {whole_conversation, legacy_uncovered, needs_rebaseline}`.
  It is **`false` only for `unestablished`**, and then **`findings` is `[]`** — so an empty list is
  never mistaken for "no findings"; nothing was persisted, so the human reconstructs from **this
  turn's returned prose** (which exists, because the turn ran). On a `turn_not_durable` turn the
  reported `ledger_coverage` is whatever was **already on disk before the turn** — `whole_conversation`
  (prior ledger intact, `findings_trusted: true`, findings listed) if a record existed, or
  `unestablished` (`findings_trusted: false`, `findings: []`) for a fresh turn 1; this turn's
  un-persisted increment is not in `findings` and lives only in the prose. The four completed-state
  values are in the completed `outputSchema` and tested; `invalid` is tested as a resume refusal, not
  a completed envelope. (On a `running` result both fields are absent, like the rest of the group.)
- **`verdict_source` is `structured` or `none` — never `prose`** (round 4). The server does not
  parse prose for a verdict, not even as a degraded best-effort: a half-parsed prose verdict would
  both contradict Decision 1 and mislead. On any degraded turn the verdict is `unknown`, the source
  is `none`, and the turn is non-convergent regardless — so nothing is lost by refusing to guess.
- **`outcome`, `review_prose`, `review_prose_truncated`, `block_repair`** were added by issue #63
  (envelope `schema_version` 2; the *ledger* version is a separate constant and unchanged, so
  ledgers written before this still load). `outcome` is what a caller switches on — `converged` /
  `changes_requested` / `escalate` / `rebaseline` — derived totally from `non_convergence_reason`,
  so it adds no second precedence that could disagree with the first. `review_prose` carries the
  reviewer's prose on the structured channel **only when the machine channel does not represent the
  turn** (`structured: false`, or `turn_not_durable`), capped at 16,000 characters; it is
  transported, never interpreted, and `verdict_source` is unaffected. `block_repair` records whether
  the block was re-asked for and how that went. Full detail in
  [`unstructured-turn-recovery.md`](unstructured-turn-recovery.md).
- **`detail` is the reviewer's prose, captured when the finding was first raised**, and not
  rewritten on later turns (Decision 2). Nothing is lost by adopting the structure: the block
  is a spine, the prose is the body, and the human rendering is unchanged.
- **`schema_version`** on the envelope, and a matching version on the persisted ledger, follow
  this project's existing discipline (the metrics log versions its records and skips-and-counts
  foreign ones rather than guessing).

### One schema for every successful response

Round 1 noted that `cross_model_review_result` also returns `status=running` snapshots, so an
advertised `outputSchema` must describe those too or it is a lie. Round 5 sharpened that a vague
"null/absent findings group for running" does not actually define the running shape. So the
`outputSchema` is a **discriminated union on `result_status`**, `oneOf` two variants, and every
running field is pinned rather than left to "absent":

- **`result_status: "running"`** — a progress snapshot, *not* a verdict. The convergence/findings
  group is **absent** (not "null-and-maybe-meaningful"): no `converged`, no `verdict`, no
  `verdict_detail`, no `open_count`/`total_count`, no `findings`, no `non_convergence_reason`, and
  no `ledger_coverage`/`findings_trusted`. It carries only `schema_version`, `session`, `turn`,
  `result_status`, and progress fields (elapsed, phase, liveness), mirroring the existing running
  text snapshot. A caller never reads convergence off a running result — there is nothing there to
  misread. `structuredContent` for a running result is this running variant.
- **`result_status: "completed"`** — the full envelope below, where the "`non_convergence_reason`
  is `null` iff `converged`" rule and the whole findings/verdict group apply, **including
  `ledger_coverage`** (one of the **four** run-reachable states `whole_conversation` /
  `legacy_uncovered` / `needs_rebaseline` / `unestablished` — never `invalid`, which is a resume
  refusal) **and `findings_trusted`** (the rebaseline discriminator). This is the *only* variant in
  which `converged` exists at all.

Because the two variants are disjoint and discriminated, "`non_convergence_reason` null iff
converged" is stated only where it makes sense (completed), and a running result cannot violate it
by construction. A single `outputSchema` still validates every successful result; failure results
(`isError: true`) stay out of scope, as today. Both variants are emitted by the one shared renderer
(the `src/tools.rs` wiring), and every running field is tested.

### Verdict truth table

Completing the mapping round 1 flagged as underspecified. **This table maps only the
`verdict_detail` × open-count axis; it does *not* redefine `converged`.** The `converged` column
below reads `true` **only under the rest of the conjunction** from
[the top of the document](#the-single-safe-termination-signal-converged) — structured block,
`ledger_coverage == whole_conversation`, valid ledger, clean reconciliation, and not over budget
(`terminal_reason` unset). Where every one of those already holds, the table gives the last two
gates (the reviewer's own verdict and the open count); if any earlier conjunct fails, `converged`
is `false` no matter what this table's row says, and the reason is that conjunct's
(`ledger_unavailable` / `ledger_too_large` / `turn_not_durable` / …), not `open_findings`. So the
`approve` + 0-open row is `true` **only** on an otherwise-clean, covered, in-budget turn — never on
its own.

| reviewer `verdict_detail` | open findings after reconciliation | machine `verdict` | `converged` (given the rest of the conjunction holds) |
| --- | --- | --- | --- |
| `approve` | 0 | `approve` | **true** |
| `approve` | ≥1 (contradiction) | `changes` + warning | false, `verdict_contradiction` |
| `approve_with_comments` | 0 | `approve` | false¹, `reviewer_withheld_approve` |
| `approve_with_comments` | ≥1 | `changes` | false, `open_findings` |
| `request_changes` | ≥1 | `changes` | false, `open_findings` |
| `request_changes` | 0 (contradiction) | `changes` + warning | false², `verdict_contradiction` |
| `blocked` | any | `changes` | false, `reviewer_blocked`³ |
| *unstructured (no valid block, and the repair did not recover one)* | unknown (`null`) | `unknown` | false, `ledger_unavailable`⁴ |

¹ `approve_with_comments` with zero *open* findings means the reviewer left only informational
comments. Those are not open findings and do not block convergence *numerically*, but the
reviewer explicitly declined a clean `approve`, so the server honours that: `converged` stays
false until the reviewer itself returns a clean `approve`. Convergence follows the reviewer's
own top-level judgement, never overrides it toward "done."

² `request_changes` with zero open findings is a contradiction in the other direction (the
reviewer wants changes but named none as open). Fail safe: treat as `changes`, warn, do not
converge. Uncertainty never resolves to done.

³ `blocked` is a distinct outcome, not a flavour of contradiction or open findings (round 5): the
reviewer is reporting it *could not complete a clean review* at all. Its `non_convergence_reason`
is `reviewer_blocked` regardless of `open_count`, and it is a human-escalation outcome — a loop
must not re-review-and-hope against a reviewer that has declared itself blocked. It sits above
`verdict_contradiction` in the precedence order.

⁴ Since issue #63 a turn only reaches this row after the block repair has been tried and failed
(or was disabled, unavailable, or not applicable): the server asks the reviewer once more, in the
same conversation, before accepting that the turn has no machine record. A *recovered* turn is an
ordinary structured turn and takes one of the rows above.

An unstructured turn's *top-level* reason is `ledger_unavailable`, not `unstructured` (round 8):
a degraded turn breaks coverage (`needs_rebaseline` mid-session, `legacy_uncovered` on turn 1), and
coverage is stamped before reason selection, so the reason that reaches the caller is
`ledger_unavailable` (**escalate for a rebaseline**, not a blind `fresh`) whenever the on-disk break
is persisted — this turn's write landed, *or* the session was already
`legacy_uncovered`/`needs_rebaseline`. It reports `turn_not_durable` **only** when the turn entered
`whole_conversation`/`unestablished` and its *own* coverage-break write failed (round 19; an
already-broken session stays `ledger_unavailable` by precedence). Both escalate. `unstructured` is
the internal cause, carried by `structured: false` and the warning, never a `non_convergence_reason`
value.

## Prompt changes (`src/prompt.rs`)

The current builder splits the prompt into a **turn-1-only preamble** (role, ground rules,
"Your access") and a **per-turn request section**. Round 3's finding is that the machine-block
contract must live in the *per-turn* section, because it is needed on every turn, its nonce
changes every turn, and its total-accounting clause only applies on resumes. So:

- **The role-setting preamble stays turn-1-only**, unchanged in spirit.
- **A new per-turn "machine block" section renders on every turn** (turn 1 and every resume): the
  exact begin/end sentinel markers **carrying this turn's nonce**, the two-array schema
  (`prior_findings` status-only, `new_findings` content-only-no-status), that it is **in addition
  to** the prose (not a replacement), that severity/status is the reviewer's own honest
  judgement, and that it must emit **exactly one** such block. **It also carries the Decision 1
  completeness contract verbatim** (round 5): the block is the sole machine-authoritative source,
  so **every finding mentioned anywhere in the prose must have a corresponding block entry**, and
  the **block verdict must match the prose `## Verdict`** — a prose finding absent from the block,
  or a verdict mismatch, is a reviewer-side contract violation. This lives in the actual per-turn
  prompt, not only in the design prose, because that contract is the only control on the
  block/prose-disagreement residual.
- **On a resumed turn only**, that section additionally carries the **prior-findings digest** —
  **all** prior findings, open and resolved, with their ids, as quoted evidence — and the
  total-accounting instruction: *report a status for **every** listed id **exactly once** (not
  only the ones you changed); a missing or extra id fails the turn.* This replaces the round-2
  wording "each id it addresses," which round 3 correctly flagged as contradicting the exact-set
  contract in Decision 2. New concerns go in `new_findings`.

These are new `PromptParts` inputs — the nonce, the block-contract text, and (on resume) the
prior-findings digest — rendered after the change and before the follow-up instruction, in the
slot the resumed-capture note already establishes as "context about the prior turn." The
existing `FOLLOW_UP_GUIDANCE` is folded into this so the two do not drift apart.

## New module — `src/findings.rs`

Pure logic, no I/O, exhaustively unit-tested, in the spirit of `src/digest.rs`:

- The types: `Verdict`, `Severity`, `Status`, `Finding`, `Envelope`, `LedgerEntry`, `Ledger`.
- **Extraction**: locate the single sentinel-delimited block; fail-closed on zero, many, or
  malformed; enforce size/field caps; return a typed parse or a typed degradation reason.
- **Reconciliation**: `(prior ledger, this turn's parsed block, turn number) → (new ledger,
  envelope)`, implementing Decision 2 exactly — the two-array split, immutable content, id
  assignment, status carry-over, missing-id detection (any prior id unaccounted for → degrade), and the fail-closed conditions that
  degrade the whole turn.
- **Convergence + verdict resolution**: `(reviewer verdict, ledger, structured?) →
  (converged, machine verdict, warnings)`, implementing the truth table and the top-of-document
  `converged` definition.
- **Rendering**: `Envelope → serde_json::Value` for `structuredContent`, and `Envelope →
  String` for the sentinel-delimited text block.

Keeping all of this in one pure module means the entire id/status/verdict/convergence contract
is testable without a model, a network, or a filesystem — which is where the correctness risk
actually lives.

## Wiring (the I/O edges)

- **`src/session.rs`** — `SessionRecord` gains **two** optional, versioned fields: a
  `findings_ledger` and a sticky durable `terminal_reason`
  ([bounded growth](#bounded-growth-and-escalation-outcomes)) that records `ledger_too_large` (and
  is the natural home for any future sticky escalation). **Coverage provenance is *not* a separate
  field — it lives *inside* the ledger** (`Ledger.coverage`,
  [whole-conversation rule](#convergence-requires-a-ledger-over-the-whole-conversation)). Folding it
  in is what makes the `invalid` state derive-at-load with no separate poison-write (see that rule):
  an unreadable ledger has no independent `whole_conversation` stamp to keep. `TurnFacts` carries the
  reconciled ledger a completed turn produced; `record_turn` persists it — and sets `terminal_reason`
  when the turn is over budget — under the existing exclusive-lock read-modify-write. `resume_block`
  is extended to refuse a resume while `terminal_reason` is set (and the call site refuses an
  unreadable ledger, tagged `ledger_unavailable`), before the session is claimed.
- **The write-ahead marker is a *dedicated* `.findings-pending` sidecar, a sibling of the existing
  Perforce `.pending` one** (round-7 minor #3 proposed reusing `.pending`; implementation review
  found the two must stay separate because a set marker means *different things* to the two
  concerns). The repository already has the sidecar *mechanism* — `mark_pending` / `clear_pending` /
  `marker_state` write a sibling file *before* a turn does anything that could deliver a review
  without persisting, clear it only once the turn is durably recorded, and are **fail-closed**
  (`marker_state` treats an I/O error as `Present`). The findings path takes a **parallel** marker
  (`mark_findings_pending` / `clear_findings_pending` / `findings_marker_state`) built on the same
  `marker_path` helper, rather than sharing the one file. The reason is that a set marker triggers
  **opposite recoveries**: a leftover `.pending` (Perforce baseline) tells the next turn to take a
  **full re-capture** and keep going — not a refusal; a leftover `.findings-pending` tells
  `start_review` to **refuse the resume** (`SESSION_NOT_RESUMABLE`), because the ledger may now be
  stale relative to the reviewer's advanced conversation and a full diff re-capture would not repair
  that. Sharing one marker would entangle these: a Perforce turn that only needed a re-capture would
  over-refuse the next findings resume, and the findings gate would pre-empt the Perforce
  full-capture fallback even in the two cases where the ledger is provably *not* stale (a crash
  before the findings marker is written during capture, and a pre-launch reviewer failure that
  started no child — both of which `attempt` detects and clears the findings marker for). Keeping
  the two markers distinct lets each fire its own recovery. Both are still per-session sidecars, both
  fail-closed, both survive a crash/restart, and a **pre-upgrade** `.pending` left by an older
  Perforce-only build is understood unchanged (it is never read as a findings marker). This
  separation is tested: a set findings marker refuses the resume while the Perforce full-recapture
  path stays reachable for the not-stale cases.
- **`src/session.rs` — the store load itself becomes tri-state, which round 3 showed is the real
  fix, not a tolerant field.** Today `read()` (`src/session.rs:342`) maps *both* "file missing"
  and "file present but unparseable" to `StoreFile::default()` — an empty set — so a corrupt
  `sessions.json` silently makes every named session look absent, and the next call starts a
  fresh (and, worse, `whole_conversation`-convergeable) conversation under a name whose real
  history was lost. So the load distinguishes three states: **absent** (no file → genuinely empty,
  normal), **valid** (parsed), and **`invalid`** (file present, did not parse). An `invalid`
  store does **not** masquerade as absent: **every** store accessor — `get`, `record_turn`,
  `forget`, `list` — propagates the `Invalid` state rather than treating it as an empty set. A
  `get(name)` returns an `Invalid` sentinel, not `None`.

  Round 4 caught that refusing *resume* alone is not enough, because **`fresh: true` bypasses the
  lookup** (`start_review` sets `prior = None` for `fresh`, `src/tools.rs:179`): on a corrupt
  store the server cannot prove a name is genuinely absent, so a `fresh` call would write a
  brand-new `whole_conversation`-convergeable record on top of unreadable state — either
  clobbering the corrupt file or merging into it. So **an `invalid` store gates `fresh` too.**
  While the store is `invalid`:
  - **Every review is *refused before the model call*, `fresh` included — no one-shot review runs**
    (round 21 corrected an earlier "one-shot" idea). Exactly like an unreadable *ledger* (the
    `invalid` coverage state), a corrupt *store* is caught at load, so it is a **pre-model
    `SESSION_NOT_RESUMABLE`-class refusal** carrying `state_corrupt` as its detail — **not** a
    completed envelope. This is deliberate: a completed envelope must carry a defined
    `ledger_coverage` (one of the four run-reachable states), `findings_trusted`, and `open_count`,
    and a corrupt store can support none of them (nothing can be read or persisted), so a "one-shot
    completed envelope" would have no valid schema representation. Refusing is both the safe choice
    and the only representable one. `state_corrupt` is therefore an **error/refusal outcome, never a
    completed-envelope `non_convergence_reason`** (parallel to how `invalid` delivers
    `ledger_unavailable` as a refusal detail, round 17).
  - **The corrupt file is never overwritten or auto-replaced.** Recovery is an explicit operator
    action — move the corrupt `sessions.json` aside, or point `--state-dir` at a clean directory —
    not something the server does silently, because auto-replacing it would destroy the evidence of
    what was lost and could mask a systemic corruption source. The operator is told exactly this.

  The tolerant-*field* idea still applies *within* a valid store — one bad ledger record degrades
  that one session (→ that session `invalid`), not the file. Refusing all starts on a corrupt
  store is the safe choice, not an over-broad one: the alternative is exactly the false-convergence
  this closes. Which corruption cases are recoverable field-by-field versus which poison the whole
  store is documented explicitly; a journal/per-session-file layout is the more robust option if
  field-level recovery of arbitrary whole-file corruption is wanted later.
- **The worker** (in `src/reviewer/mod.rs` / `src/tools.rs`) gains, around the existing review
  call: on **every** turn — fresh or resumed — **`mark_findings_pending` the dedicated
  `.findings-pending` sidecar (a sibling of, and independent from, the Perforce `.pending` one — see
  the write-ahead-marker note above) and confirm the write succeeded before the reviewer is
  invoked** → load the prior ledger (if resuming) → hand it to the prompt builder → after the review
  text is collected, extract + reconcile into an envelope and a new ledger → **`record_turn` persists
  the new ledger (atomic JSON replace), then `clear_findings_pending` deletes the sidecar**. Round
  4's finding: if the marker write itself *fails* and the worker proceeds anyway, a crash can advance
  the reviewer conversation with no durable marker — the exact hole. So **a failed
  `mark_findings_pending` aborts before the model call**, via an existing failure/refusal path,
  rather than returning a review that could still be resumed unsafely. If the post-review
  `record_turn` fails, degrade this turn (`turn_not_durable`); the sidecar was never cleared, so the
  next call is refused a resume — an escalation → [rebaseline
  handoff](#recovery-from-a-broken-ledger-is-a-human-rebaseline-not-an-autonomous-fresh), not an
  autonomous restart. Because the sidecar clear is a *separate* step after the atomic record, a crash
  in the narrow gap between them leaves a **stale** findings marker on a turn that *was* durably
  recorded — that over-refuses toward the escalation path (the safe direction), the same fail-closed
  tolerance the Perforce `.pending` sidecar already has. Keeping the two markers separate is what
  lets a leftover `.findings-pending` refuse the resume while a leftover `.pending` only forces a
  full re-capture (their recoveries differ — see above). **This applies to `fresh` calls too**
  (round 9): a `fresh: true` that overwrites an existing record and crashes before `record_turn` must
  leave the sidecar set, so the stale old record cannot be resumed — see Decision 5.
- **`src/registry.rs`** — `Review`, `Outcome`, and `Snapshot` carry the `Envelope` so the
  result renderer can emit both channels. Computed once, when the turn finishes, travelling with
  the snapshot like `usage` and `warnings` already do.
- **`src/tools.rs`** — **both** the completed and the running result paths route through **one
  envelope renderer** (round 4). `render_completed` strips the reviewer's `CROSS_REVIEW_FINDINGS_IN`
  block from the prose, strips any lookalike `_OUT` marker from it, then appends exactly one
  nonce-bearing `CROSS_REVIEW_ENVELOPE_OUT` block; `render_running` (`src/tools.rs:418`) appends
  the same single `_OUT` block as the **running variant** — `result_status: "running"` with the
  convergence/findings group (`converged`, `verdict`, `verdict_detail`, `open_count`, `total_count`,
  `findings`, `non_convergence_reason`) **absent**, not `null` (round 6): the discriminated `oneOf`
  schema requires those keys to be *omitted* from the running variant, so a renderer that emitted
  them as `null` would fail its own schema. Neither path can emit zero or two `_OUT` blocks, and
  both validate against the one `outputSchema`. The completed result also carries the envelope
  object for `structuredContent`.
- **`src/mcp.rs`** — `text_result` (or a new `tool_result` beside it) attaches
  `structuredContent` alongside `content`; the dispatch path threads the envelope through. Tool
  definitions gain `outputSchema` for `cross_model_review_result`, covering running and completed
  per the single-schema rule above.

## Decision 5 — durability across the reviewer turn and the ledger write

Round 1's idempotency finding: persisting the ledger atomically with `record_turn` does not
make the *external reviewer turn* idempotent. If the process crashes, times out, or fails to
persist after the reviewer has already accepted and advanced its conversation, the remote
conversation is ahead of the local ledger; a naïve retry would resume with stale ids and could
mint colliding ones.

Round 2 was right that the round-1 answer did not actually close this. Pointing at the existing
"stops inviting a resume" behaviour is not enough, because that behaviour is **in-memory
rendering state** (`Snapshot.resumable`, which only changes the response text). It does not stop
a *subsequent* call from resuming: `start_review` reads the stored `SessionRecord`
([`src/tools.rs:179`](../src/tools.rs), under the held lease) and resumes it whenever
`resume_block` allows, and `SessionRecord` ([`src/session.rs:28`](../src/session.rs)) has no
durable "do not resume this" bit. Worse, a crash *before* the atomic replace has no chance to
set even the in-memory flag — the process is gone. So the fix is a **durable write-ahead intent
marker**. The repository already had the *mechanism* — the Perforce `.pending` sidecar
(`mark_pending`/`clear_pending`, `src/session.rs`) — and round-7 minor #3 proposed reusing that same
file. Implementation review kept the mechanism but gave findings its **own** sidecar,
`.findings-pending` (`mark_findings_pending`/`clear_findings_pending`/`findings_marker_state`),
because a set marker triggers **opposite recoveries** for the two concerns: a leftover `.pending`
means "take a full Perforce re-capture and continue" (not a refusal), while a leftover
`.findings-pending` means "**refuse the resume**". Sharing one file would make a Perforce turn that
only needed a re-capture over-refuse the next findings resume, and let the findings gate pre-empt the
Perforce full-capture fallback in the cases where the ledger is provably not stale. So the two are
separate, parallel, both fail-closed:

- **The `.findings-pending` sidecar is `mark_findings_pending`-ed *before* the reviewer is called**,
  while the lease is held. It is a per-session sibling file, so it survives a crash independently of
  the JSON and needs no change to `SessionRecord`. It records that a turn is in flight against this
  session's reviewer conversation.
- **The marker is written before *every* reviewer turn — resumed *and* fresh — not only resumes**
  (round 9 closed the exemption). The round-4 draft exempted a fresh turn because "a new
  conversation has nothing to strand," but round 9 found the hole: **`fresh: true` on a name that
  already has a record does not delete that record up front** — it sets `prior = None`
  (`src/tools.rs:179`) and only overwrites on `record_turn`. So if a fresh call advances a new
  reviewer conversation and then crashes before `record_turn`, the *old* record is still on disk
  with no marker, and the next normal call would resume that stale conversation, bypassing the
  fresh review entirely. Marking every turn closes it: on that crash the sidecar is set, so
  `resume_block` refuses to resume the stale record (an escalation, recovered by a human-directed
  rebaseline `fresh`). The only turn with
  genuinely nothing to protect is a fresh call on a name with **no existing record**, and marking
  it too is harmless (a stale marker on a name with no record is cleared on the next start, which
  has nothing to resume anyway) — so the rule is simply "mark every turn," with no exemption to get
  wrong.
- **The marker write is not best-effort — it must succeed, or the turn aborts before the model
  call** (round 4). A marker that failed to persist provides none of the protection this whole
  mechanism exists for, so proceeding to bill and advance the reviewer with no durable marker would
  reinstate the stale-id hole; the turn is refused instead, via an existing failure path, and
  nothing is billed. This holds for fresh calls too, since a fresh overwrite of an existing record
  is exactly the case round 9 exposed. Tested with a crash-injected failed fresh overwrite that must
  block a subsequent normal resume.
- **On a clean turn**, `record_turn` durably writes the new ledger (atomic JSON replace) and *then*
  `clear_findings_pending` removes the sidecar. The order matters: the record is durable before the
  marker is cleared, so a crash in the narrow gap leaves the marker *stale*-set on an already-recorded
  turn — which over-refuses toward `fresh`, the safe direction, the same fail-closed tolerance the
  Perforce `.pending` sidecar already has (a stale marker there just disables elision).
- **On any crash, timeout, or persistence failure**, the marker survives on disk because it was
  written first and cleared last. The next call finds it via `findings_marker_state` (**fail-closed**:
  an I/O error reading it counts as set). `start_review` refuses a non-`fresh` call while the
  sidecar is present, returning `SESSION_NOT_RESUMABLE`. This is not an invitation to *auto*-`fresh`:
  `SESSION_NOT_RESUMABLE` is itself an escalation outcome, and recovery goes through the
  [rebaseline handoff](#recovery-from-a-broken-ledger-is-a-human-rebaseline-not-an-autonomous-fresh)
  — a human decides, then a `fresh` **carries the human-selected still-open findings** into the new
  turn-1 baseline. A `fresh: true` call ignores the stored record entirely (`start_review` sets
  `prior = None`), so it starts a new conversation with a new ledger and no stale ids; **the poisoned
  *record* is replaced** and its stale sidecar cleared. The "new ledger, no stale ids" is about
  server-owned *identity* (old ids are not reused); it does not mean the *findings* are discarded —
  those are re-supplied through the rebaseline instructions, which is the whole point of routing
  recovery through a human rather than an automatic restart.

  Round 5's consistency point: this "`fresh` replaces the poisoned record" is about **a single
  bad session record inside a store that still parses** — a marker-poisoned, `invalid`-ledger, or
  `legacy_uncovered` record. That is a targeted overwrite of one entry in a writable store, and it
  is safe. It is **not** the same as [a corrupt *whole* `sessions.json`](#wiring-the-io-edges),
  where the store itself will not parse: there even `fresh` is gated, because the server cannot
  write one record into a file it cannot read without either clobbering it or losing the rest, and
  recovery is an explicit operator action. Record-level poison → `fresh` heals it; store-level
  corruption → operator recovery. The two are never conflated.

  **If that `fresh`-over-a-bad-record turn 1 *itself* fails to persist** (round 21), it is not a
  special case: it is exactly the `unestablished` / `turn_not_durable` turn-1 case — the new
  conversation has no durable coverage yet, so the completed envelope reports `ledger_coverage:
  unestablished`, `findings_trusted: false`, `findings: []`, `non_convergence_reason:
  turn_not_durable`, and the `.findings-pending` sidecar (set before the reviewer call) refuses the next
  resume. The old bad record is untouched on disk (the replacing write never landed), so nothing is
  lost that was not already lost; the next `fresh` retries. This is a completed envelope (a turn ran)
  — distinct from the corrupt-*store* case, which never runs a turn at all.
- **The reviewer turn and the ledger cannot silently diverge**, because the sidecar is cleared
  only *after* the atomic `record_turn` that wrote the matching ledger; any path that advanced the
  reviewer conversation but did not reach that record leaves the marker set, which blocks resume.
  The one direction this can err is over-caution — a stale marker after a durable record — which
  costs an unnecessary escalation-and-rebaseline, never a bad resume.

The residual honest limitation, unchanged and now genuinely the *only* one: a crash in the narrow
window after the reviewer accepted a turn and before the atomic `record_turn` loses that turn's
status updates. This is reported as **`turn_not_durable`** — **not** an autonomous-`fresh` signal:
**the loop stops and escalates**, and recovery is a **human-directed rebaseline** in which a human
starts `fresh` carrying the preserved prior findings (the on-disk ledger from before the lost turn is
intact and its open findings are listed in `findings`) plus anything the human recovers from the
lost turn's prose. There is no path here on which an autonomous caller silently restarts and
discards the unpersisted findings. That is the safe direction — a lost turn re-reviews from scratch
under human direction rather than resuming on a ledger that disagrees with the reviewer — and it
costs one rebaseline, not a corrupted convergence. It is stated rather than papered over.

## Convergence requires a ledger over the whole conversation

Round 2's third finding: "no ledger" is not uniformly safe. A session that began its reviewer
*conversation* before this feature existed has real findings living only in that conversation's
prose history, with **no server ids**. If such a session is resumed, the reviewer holds those
old concerns but the server's ledger is empty, the resumed prompt can omit them, and an
`approve` with no new findings would let an empty ledger read as `open_count == 0` and converge —
while the old issues remain untracked. That is exactly the false convergence this design exists
to prevent.

So convergence is gated on the ledger covering the **whole** conversation. Round 3 caught that
the round-2 wording ("no ledger on record → non-convergent") was not *durable*: a legacy session
could resume with no ledger, this turn create and persist one through `record_turn`, and on the
*next* turn present a perfectly readable ledger with no trace that it was bolted onto a
conversation that predated it — reopening the exact hole. An empty ledger from a clean turn 1 and
an empty ledger bolted onto a legacy conversation would then be byte-identical.

The fix is **persisted provenance, not an inference from ledger presence.** `SessionRecord`
carries a `ledger_coverage` enum. It is a **one-way state machine**: it can only ever move
*toward* non-convergeable, never back, and the only way to (re-)establish `whole_conversation` is
an explicit `fresh` conversation. Round 4 sharpened two transitions the round-3 draft left
implicit; both are now stated:

- **`whole_conversation`** — set at the moment a **valid turn-1 ledger is atomically persisted**
  on a genuinely fresh conversation (new session or `fresh: true`). **Convergeable.** An empty
  `whole_conversation` ledger legitimately means "the reviewer raised nothing" and *can* converge
  — *but only because it was validly produced and atomically persisted*; the provenance, not the
  emptiness, is what earns that. It is set only on a **clean, structured, persisted** turn 1 (next
  bullet).
- **`legacy_uncovered`** — set when a ledger is first attached to a conversation that **already
  had turns** (pre-feature), **or when turn 1 itself did not cleanly anchor a ledger** — a
  degraded turn 1 (no valid block) leaves the conversation carrying ungrounded prose findings
  exactly as a pre-feature conversation does, so it gets the same treatment. **Never
  convergeable** for the life of the conversation, even as later turns add valid findings: the
  early history was never grounded, so the ledger can never be trusted as total.
  `non_convergence_reason: ledger_unavailable`; escalate for a [rebaseline
  handoff](#recovery-from-a-broken-ledger-is-a-human-rebaseline-not-an-autonomous-fresh), not an
  autonomous `fresh`. Sticky — a conversation cannot launder itself convergeable by accumulating
  turns.
- **`needs_rebaseline`** — set when a **degraded turn happens mid-session** on a
  `whole_conversation` record (round 7). This is the subtle hole round 7 found: a resumed turn that
  produces **no valid block, or whose reconciliation fails closed**, preserves the *old* ledger —
  but the reviewer still emitted **prose**, which may carry a finding that never entered any block.
  The conversation now contains an ungrounded finding exactly as a degraded turn 1 or a pre-feature
  turn does. If nothing recorded that, a *later* clean turn could account for all the old ids,
  report them resolved, and reach `converged: true` while that intervening prose finding sits
  untracked. So a mid-session degrade **transitions `whole_conversation → needs_rebaseline`
  durably and stickily** (diagnostically distinct from `legacy_uncovered` — "a degraded turn broke
  coverage" versus "predates the ledger" — but behaviourally identical: non-convergeable,
  `non_convergence_reason: ledger_unavailable`, cleared only by a human-directed rebaseline `fresh`).
  The session may still be
  reviewed; it can never again converge. **The persistence-failure outcome is deterministic by
  result, the same rule as the over-cap path** (round-8 minor): the transition is written by the
  turn's `record_turn`; if that **succeeds**, coverage is durably `needs_rebaseline` and *this
  turn's* envelope reports `ledger_unavailable`; if it **fails**, nothing is persisted, the
  `.findings-pending` sidecar (set before the reviewer call) stays set, and *this turn* reports
  `turn_not_durable` (the next call is refused a resume anyway). Either way the break is durable and
  no later turn can converge. Tested by: valid turn 1 → malformed turn 2 that adds a new prose
  finding → valid turn 3 must **not** converge (`ledger_unavailable`).
- **`invalid`** — a **durable poison, derived at load** — and the implementation reaches that
  durability by *construction* rather than by writing a separate transition. Round 4's finding was
  that if `invalid` meant only "unreadable *this* load" **and coverage lived in a field separate
  from the ledger**, a `whole_conversation` record whose ledger bytes later failed to parse could
  keep its `whole_conversation` stamp and then accept a *replacement* ledger and converge. The
  implementation removes the premise: **coverage is stored *inside* the ledger JSON**
  (`Ledger.coverage`), not in a separate `SessionRecord` field, so there is no independent
  `whole_conversation` stamp that could outlive an unreadable ledger. When the ledger bytes fail to
  parse, deserialize at an incompatible version, or fail `is_structurally_valid`, the load yields
  `invalid` and *nothing readable claims `whole_conversation`*. `whole_conversation → invalid` is
  therefore a one-way door that needs **no poison-*write*** to be durable: the corruption is
  self-durable (nothing overwrites it on a refused resume), so every later load re-derives `invalid`.
  An `invalid` conversation is non-convergent for the rest of its life, the record is preserved
  (never reset to a fresh zero-open ledger), and only a human-directed rebaseline `fresh` escapes it.
  Because the unreadable ledger is caught **at load, before any reviewer runs**, an `invalid` resume
  is a **`SESSION_NOT_RESUMABLE` refusal** (a machine-readable error, no completed envelope and no
  prose — nothing ran), *not* a completed envelope; `invalid` is therefore never a completed-envelope
  `ledger_coverage` value. The ledger's findings are unrecoverable from the server, so the `fresh`
  re-reviews from scratch.

  **The turn fails closed with no write at all** (round 5's invariant, reached without a poison-write
  step): the server never falls back to treating the record as still `whole_conversation`, because
  the only place coverage is read from is the ledger, and the ledger did not parse. The corrupt
  ledger is detected **at load time, before the model call**, so it is a **resume refusal before
  anything is billed**: `resume_block` returns `SESSION_NOT_RESUMABLE` with the ledger reported
  `ledger_unavailable` (the record's own ledger is unreadable — *not* `state_corrupt`, which is
  reserved for a whole-store parse failure), which is an escalation → rebaseline, not an autonomous
  restart. Nothing runs as a `whole_conversation` turn. The next process load re-reads the
  still-corrupt ledger and again derives `invalid`, so the state is self-healing toward `invalid`,
  never toward `whole_conversation`. This holds across a restart because the on-disk bytes are
  unchanged, and `is_structurally_valid` additionally rejects a ledger that somehow carries an
  `invalid` (or transient `unestablished`) coverage on disk, so neither can ever load as usable.

The one-way lattice: the initial state before anything durably persists is **`unestablished`**
(round 13) — a fresh turn that has not yet recorded; a clean turn 1 moves it to
`whole_conversation`, while a turn 1 that fails to persist *stays* `unestablished` (reported on that
`turn_not_durable` turn, never mislabelled `whole_conversation`/`legacy_uncovered`).
`whole_conversation` is the only convergeable state; a corrupt load moves it irreversibly to
`invalid`; a pre-feature or degraded-turn-1 anchor that *does* persist lands at `legacy_uncovered`;
a mid-session degrade moves it to `needs_rebaseline`; none of
`unestablished`/`legacy_uncovered`/`needs_rebaseline`/`invalid` can ever reach `whole_conversation`
except via a clean `fresh` turn 1. The implementation may equivalently **not persist a ledger at
all** for a conversation it would mark `legacy_uncovered`; either way the invariant holds and is
tested across *several* later turns, not just the first.

**Persistence is evaluated first, then coverage is stamped, then the reason is selected** (round
7, refined round 9). The order is exact and resolves what would otherwise be a conflict between
"a degraded turn breaks coverage → `ledger_unavailable`" and "a failed persist →
`turn_not_durable`":

1. The turn attempts to persist its coverage transition (`legacy_uncovered` on turn 1,
   `needs_rebaseline` mid-session) via `record_turn`.
2. **If that persist fails**, no coverage transition took effect on disk (coverage is *unchanged*),
   so `ledger_unavailable`'s precondition (`coverage != whole_conversation`) is **not** met; the
   turn reports **`turn_not_durable`**, and the `.findings-pending` sidecar (set before the reviewer call)
   enforces the break by refusing the next resume. Persistence is checked before the coverage-based
   reason precisely so the two cannot both fire.
3. **If it succeeds**, coverage is now `legacy_uncovered`/`needs_rebaseline` on disk, and reason
   selection runs against the precedence order — which puts `ledger_unavailable` above everything a
   degraded turn else raises. The reported `non_convergence_reason` is **`ledger_unavailable`**
   (**escalate → human-directed rebaseline**), never a bare `unstructured` ("re-review").

`unstructured` survives only as the internal classification in the warning detail; it is never a
top-level `non_convergence_reason`. So "every degraded turn reports `ledger_unavailable`" holds
**when its coverage transition persisted**; when the write failed it reports `turn_not_durable` —
both escalate (they are not autonomous-`fresh` signals), and step 1 above is what makes the choice
between them deterministic rather than precedence-dependent.

**The precondition for `ledger_unavailable` is the *on-disk* coverage** — the persisted result of
*all* prior turns, not only this one — which cleanly settles the case round 10 raised: a turn on a
session that is **already** `legacy_uncovered`/`needs_rebaseline` from a prior persisted transition
(*not* `invalid` — an unreadable ledger refuses resume before a turn can run, so no turn reaches this
classification, round 17). There the on-disk coverage is already `!= whole_conversation`, so
`ledger_unavailable` holds **regardless of this turn's write**. If this turn *also* fails to persist, both
`ledger_unavailable` (already-broken coverage) and `turn_not_durable` (this turn's sidecar) apply;
precedence puts `ledger_unavailable` first, so that is reported — and both escalate, so the advice
is the same either way. `turn_not_durable` is therefore the reported reason in exactly the
situations where **on-disk coverage is *not* already broken and this turn's own write failed**: a
turn whose coverage was `whole_conversation` on entry, **or a fresh turn 1 that has no persisted
coverage yet** (round-12 minor) — in both, the failed write left no durable coverage break, so only
the sidecar enforces it. Everywhere else a degraded
turn reports `ledger_unavailable`.

The rule in one line: **a session can converge only if its `ledger_coverage` is
`whole_conversation`** — a durable, persisted, monotonically-degrading fact, never a per-turn
inference from whether a ledger happens to be present.

### Recovery from a broken ledger is a human rebaseline, not an autonomous `fresh`

Round 11 caught the loop-level counterpart of the coverage rule. Coverage breaks precisely because
a turn's *prose* may carry a finding that never entered the ledger — an untracked concern the
machine cannot see. If an autonomous loop reacted to `ledger_unavailable` by simply calling
`fresh: true`, it would start a brand-new conversation whose reviewer re-reviews the *current* code
from scratch; that review can reach `converged: true` while the untracked concern — which a
from-scratch review might not re-derive (it was context-dependent, or the review simply misses it
this pass) — is silently abandoned. That is the same silent drop the coverage rule refuses,
resurfacing one layer up.

So `ledger_unavailable` is a **human-escalation outcome**, and recovery is an explicit **rebaseline
handoff**, never an automatic restart:

- The loop **stops** and surfaces the break to a human. It does not `fresh` on its own.
- **What the machine can still hand over depends on how the break arrived**, and the contract is
  different for the readable-ledger states and the unreadable one:
  - **`legacy_uncovered` / `needs_rebaseline` / `unestablished`** are **completed envelopes** (the
    turn ran). For the first two the ledger is intact, so `findings` lists every prior open finding
    to re-supply verbatim (`findings_trusted: true`); for `unestablished` nothing persisted, so
    `findings` is `[]` (`findings_trusted: false`) and the human works from **this turn's returned
    prose**. Either way the reviewer's prose is in hand.
  - **`invalid`** is **not** a completed envelope: an unreadable ledger is caught before any turn
    runs, so it is a `SESSION_NOT_RESUMABLE` **resume refusal** — a machine-readable error, no prose
    (nothing ran). The ledger's findings are unrecoverable from the server, so recovery is a `fresh`
    that re-reviews from scratch (plus whatever the human recovers from their own records); the
    error's remediation says exactly this. `findings_trusted`/`findings` do not apply because there
    is no completed envelope.
- A human (or a caller acting on human-selected findings) then starts `fresh` **carrying the
  chosen open findings into the new instructions** as the new turn-1 baseline. The design is honest
  that this cannot be fully mechanised: the server cannot verify the payload is complete, because
  the very finding that broke coverage is by definition not machine-known — which is exactly why a
  human is in the loop rather than a payload-completeness check the server could never enforce.
- A human (or a caller acting on human-selected findings) then starts `fresh` **carrying the
  chosen open findings into the new instructions** as the new turn-1 baseline. The design is honest
  that this cannot be fully mechanised: the server cannot verify the payload is complete, because
  the very finding that broke coverage is by definition not machine-known — which is exactly why a
  human is in the loop rather than a payload-completeness check the server could never enforce.

This keeps the automation where it is safe (the loop runs unattended through `open_findings` /
`verdict_contradiction` re-reviews) and puts a human exactly at the one decision the machine cannot
make safely.

## Bounded growth and escalation outcomes

Round 3's minor, which is really about not designing a slow failure. Total-accounting (Decision
2) requires the reviewer to re-state a status for *every* prior id on *every* resumed turn, and
resolved ids are never retired (retiring them would reopen the regression hole). So the ledger —
and the prior-findings digest injected into each prompt — grows monotonically with the number of
findings a long session accumulates. The existing turn ceiling (`--session-max-turns`, default
10) bounds turns but can be set to 0 (disabled), and it does not bound *findings*. Left
unbounded, a very long session would eventually push the digest past context/output limits and
degrade every turn from then on — a session that can never converge and never says why.

So the ledger and digest are **explicitly bounded, and the bound is an escalation, not a silent
degradation**:

- The reconciler enforces a **maximum ledger size with a finite, non-disableable default**
  (round-4 guidance): **serialized digest bytes are the primary budget, finding count a secondary
  guard**. It is finite by default and — unlike `--session-max-turns` — cannot be set to 0, because
  disabling it reinstates exactly the slow-failure mode it exists to prevent. A CLI knob may raise
  or lower it (`--max-findings` / a digest-byte budget), but not switch it off.
- The cap is checked **both before the reviewer is invoked *and* after reconciliation** (round-4
  guidance): before, so an already-over-budget session does not spend a billed turn only to
  degrade; after, so a turn that *crossed* the budget by adding new findings is caught on the turn
  that crossed it rather than on the next one.
- **The before-call path is fully defined** (round 11 found it was not). When the loaded ledger's
  digest is already over budget *on entry*, the server **does not invoke the reviewer and does not
  advance a turn** — no findings write-ahead marker (the check precedes `mark_findings_pending`), no
  reviewer conversation, nothing billed. It **persists
  `terminal_reason = ledger_too_large`** onto the record (an idempotent atomic replace of just that
  field; the ledger is unchanged, so there is no ledger-vs-remote divergence and no findings-marker
  involvement) and returns a **completed** envelope with `structured: false`, `converged: false`,
  `non_convergence_reason: ledger_too_large`. This path exists for the case an after-check cannot
  cover: a ledger that became over-budget *without this process writing it* — a `--max-findings` or
  digest-byte budget lowered via CLI between runs, or an older ledger loaded under a tighter cap.
  `ledger_too_large` is a **human-escalation** outcome (the session outgrew a single conversation),
  so this is not an autonomous-retry signal: the loop stops. If the terminal-flag persist itself
  fails, the call is still non-convergent `ledger_too_large`, and the flag write is simply
  re-attempted on the **next operator-directed call** — not retried by an autonomous loop; nothing
  was billed or advanced, so there is no `turn_not_durable` to report and no findings marker to strand.
  Tested with a pre-call over-cap (budget
  lowered between turns) asserting no reviewer invocation and a sticky `terminal_reason`.
- **The after-reconciliation case has a durability rule** (round 5), because the reviewer's remote
  conversation has *already* advanced by the time the over-cap is detected. Discarding the new
  over-cap ledger while clearing the pending marker would lose those new findings locally while the
  remote conversation kept them — the stale-ledger divergence Decision 5 exists to prevent. So the
  **single atomic `record_turn` replace writes both together** (round 6): the complete over-cap
  ledger (every new finding included — **nothing is retired to fit**) and a **sticky, durable
  terminal flag `terminal_reason = ledger_too_large`** on the `SessionRecord`. Writing the terminal
  flag in the *same* replace as the ledger is what stops the round-6 hole: persisting the ledger
  alone would leave "an apparently normal `whole_conversation` record," but the flag rides with it,
  so the record is visibly over-budget as soon as it is durable. `resume_block` refuses a resume
  whenever `terminal_reason` is set, **before the session is claimed**, sticky — and a **`fresh`
  call clears `terminal_reason`** along with the rest of the record's durable non-convergent state
  (round-9 minor #3), so a `fresh` after an over-cap yields a clean, convergeable
  `whole_conversation` record rather than one that is permanently terminal. Only after that durable
  record does `clear_findings_pending` remove the `.findings-pending` sidecar (and, on a Perforce
  turn, `clear_pending` the Perforce `.pending` one). The turn reports `converged: false`,
  `non_convergence_reason: ledger_too_large`.
- **The failed-persist outcome is deterministic** (round 6 resolved the apparent precedence
  clash): the two are distinguished by *whether the atomic `record_turn` succeeded*, not by
  precedence. If it **succeeded**, `terminal_reason = ledger_too_large` is durable and every later
  call reports `ledger_too_large`. If it **failed**, *nothing* was written — no ledger, no terminal
  flag — and the `.findings-pending` sidecar (written before the reviewer call and never cleared) is still
  set, so it is a plain `turn_not_durable` like any other persistence failure, and the
  reason-precedence order never has to choose between the two because only one of them was ever
  recorded.
- Exceeding it yields `converged: false`, `non_convergence_reason: ledger_too_large`, and a
  message escalating to the operator: the session has outgrown a single review conversation and
  recovery is the same **human-directed rebaseline** — split the work, or start `fresh` **carrying
  the still-open findings** (which `findings` still lists, since the ledger is intact) into new
  instructions. **No id is ever silently retired** to make room — that would be exactly the silent
  drop the whole design refuses.
- The same escalation discipline covers every "trying again won't help" state, so an autonomous
  loop has a defined stopping point rather than spinning. **Only `open_findings` and
  `verdict_contradiction` are autonomous re-review reasons; every other reason escalates to a
  human** — **`ledger_unavailable`** (coverage break → [rebaseline
  handoff](#recovery-from-a-broken-ledger-is-a-human-rebaseline-not-an-autonomous-fresh), *not* a
  blind `fresh`, round-11 major #4), **`turn_not_durable`**, **`reviewer_withheld_approve`** (the
  reviewer keeps declining a clean approve), **`reviewer_blocked`**, **`state_corrupt`**,
  **`ledger_too_large`**, and **`SESSION_NOT_RESUMABLE`**. (Repeated degraded turns across
  rebaseline attempts are themselves a stop signal: a reviewer that cannot produce a valid block N
  times running is not going to.) The loop contract is therefore "while (!converged): if the reason
  is `open_findings`/`verdict_contradiction`, re-review; otherwise stop and escalate," and the
  reason field is what makes that expressible without string-matching.

This is the honest counterpart to total-accounting: it is the safe choice for correctness, and
its cost (an unbounded id list) is bounded here rather than allowed to become a slow, silent
failure.

## What deliberately does not change

- **The prose.** Every existing line of the completed-review rendering stays. The envelope is
  additive on both channels. Anything currently reading the prose keeps working.
- **The single-dependency character.** No new crate — the block is parsed with `serde`. (The
  *single-call* half no longer holds unconditionally: issue #63 added a bounded re-ask on degraded
  turns. See the amendment under Decision 1.)
- **The failure contract.** No new failure code. "Unstructured this turn" is a `structured:
  false`, non-convergent envelope with a warning, not a `REVIEWER_FAILED`.
- **The reviewer's isolation and read-only posture.** Nothing here touches the tool policy,
  the sandbox, or the capture; the reviewer emits more structured *output*, and its inputs and
  permissions are unchanged.

## Failure modes and how each is handled

Every uncertain signal the *server* can observe degrades toward "still open / say so / not
converged," never toward "resolved / silently dropped / done." The one thing the server cannot
observe is a reviewer that violates the completeness contract — emitting a block that omits a
finding it wrote in prose (Decision 1). That residual is narrowed to reviewer non-compliance and
its scope is stated honestly: `converged` certifies *the structured contract passed*, not that a
human read and agreed with the prose. So the "never silently dropped" property holds **within the
structured contract**, and is not overclaimed beyond it.

| Situation | Handling |
| --- | --- |
| Reviewer emits no block | `structured:false`, `open_count:null`, `converged:false`, verdict `unknown` + warning; prose returned in full. Being a degraded turn it breaks coverage → `ledger_unavailable` when the on-disk break is persisted (this turn's, or already-broken session); `turn_not_durable` only if it entered `whole_conversation`/`unestablished` and its own write failed (persistence-first rule); both escalate → rebaseline (below). |
| Reviewer emits more than one block | Fail-closed ambiguity → degrade as above; the server does not pick one. |
| Block present but invalid JSON / wrong schema / over size cap | Degrade as above; the block is not partially trusted. |
| `prior_findings` names an id the ledger never issued | Whole-turn degrade + warning; the reviewer cannot mint identity. |
| Same id twice in `prior_findings` | Whole-turn degrade + warning. |
| `new_findings` object carries a `status` field | Whole-turn degrade + warning; new findings are server-forced `open`. |
| Reviewer says `approve` but ledger has open findings | Machine verdict `changes`, `converged:false`, warning names the disagreement. |
| Reviewer says `request_changes` with zero open | Machine verdict `changes`, `converged:false`, `verdict_contradiction`; uncertainty never converges. |
| Reviewer says `blocked` (any open count) | Machine verdict `changes`, `converged:false`, `reviewer_blocked` — a **human-escalation** outcome, not re-review-and-hope. |
| Prior id the reviewer failed to account for | Whole-turn degrade; every prior id must appear exactly once — silence on any id is never an implied status. Being a degraded turn it also breaks coverage → `ledger_unavailable` when the on-disk break is persisted (this turn's, or already-broken session); `turn_not_durable` only if it entered `whole_conversation`/`unestablished` and its own write fails (persistence-first rule). |
| Regression of a previously resolved finding | Reviewer sends `f1: regressed` (resolved findings are in the injected digest, which lists every prior id); `f1` reopens, no new id. |
| One ledger *record* incompatible/undeserializable, store otherwise valid | Field-level tolerant load keeps the store and other sessions intact, but *this* session's ledger is unreadable → it is `invalid` → a **`SESSION_NOT_RESUMABLE` resume refusal** (pre-model, carrying `ledger_unavailable` as detail), **not** a completed envelope; record preserved, **not** reset to a fresh zero-open ledger (round 17). |
| The whole `sessions.json` store fails to parse | Tri-state load returns `invalid`, **not** empty: **all reviews refused before the model call** (resume *and* `fresh`) as a `SESSION_NOT_RESUMABLE`-class refusal with `state_corrupt` detail — **no one-shot review, no completed envelope** (round 21); corrupt file preserved, operator told; never silently a clean fresh conversation. |
| New session, or `fresh: true` **on a valid store** | `ledger_coverage = whole_conversation`, covers the conversation from turn 1 → convergeable normally (even with zero findings). `fresh` heals a poisoned *record* by resetting **all** durable non-convergent state — stale sidecar, `ledger_coverage` (`invalid`/`legacy_uncovered`/`needs_rebaseline`), **and `terminal_reason=ledger_too_large`** — in a valid store (round-9 minor #3). |
| Resumed conversation predating the ledger | `ledger_coverage = legacy_uncovered` (sticky, durable): reviewed but non-convergent (`ledger_unavailable`) for the life of the conversation; **escalate for a rebaseline handoff**, not an autonomous `fresh`. Later turns cannot launder it convergeable. |
| Turn's ledger could not be persisted | Envelope degrades (`turn_not_durable`); the `.findings-pending` sidecar stays set, so a resume is refused. Escalate; recovery is a `fresh` **carrying the preserved prior open findings** (Decision 5, rebaseline handoff). |
| Over budget **on entry** (before-call path) | No reviewer invoked, **no turn advanced, no findings write-ahead marker** (the check precedes `mark_findings_pending`); `terminal_reason=ledger_too_large` persisted via an idempotent single-field replace (ledger unchanged); returns a **completed** `structured:false`, `converged:false`, `ledger_too_large` envelope. Covers a budget lowered between runs (round-11 major #3). |
| That before-call terminal-flag persist itself fails | Still non-convergent (`ledger_too_large`, a human-escalation reason); the flag write is re-attempted on the next **operator-directed** call, not by an autonomous loop; **nothing billed or advanced, no findings marker, no `turn_not_durable`** — there was no turn. |
| Crosses budget **during a turn** (after-reconciliation path) | Over-cap ledger + sticky `terminal_reason=ledger_too_large` written in one atomic `record_turn`; the `.findings-pending` sidecar cleared *afterward* (a clear failure leaves a stale marker that safely forces the escalation path); `resume_block` refuses resume while `terminal_reason` set; `converged:false`, `ledger_too_large`; **no id silently retired** ([bounded growth](#bounded-growth-and-escalation-outcomes)). |
| That after-reconciliation `record_turn` itself fails | Nothing written; `.findings-pending` sidecar still set → plain `turn_not_durable`; resume refused, escalate. Deterministic: success ⇒ `ledger_too_large`, failure ⇒ `turn_not_durable` (round-6 major #2). |
| `whole_conversation` record whose ledger later fails to load / stored ledger found unreadable or incompatible at load (`invalid`) | No poison-*write* exists — coverage lives inside the ledger, so an unreadable ledger has no `whole_conversation` stamp to keep (round-4 major #1's hazard is unreachable by construction). Detected at load, **before the model call** → resume refused (`SESSION_NOT_RESUMABLE`, `ledger_unavailable` — *not* `state_corrupt`); never runs as `whole_conversation`; never accepts a replacement ledger to reconverge without an explicit human `fresh`; the corrupt bytes are never overwritten, so every later load re-derives `invalid` (self-durable across restarts). |
| Degraded turn 1 (no valid block on a fresh conversation) | No clean anchor → `ledger_coverage = legacy_uncovered` (stamped before reason selection) → reported `ledger_unavailable` (escalate for rebaseline), not `unstructured`; non-convergent, since turn 1's prose findings were never grounded. If turn 1's *own* write fails instead, `turn_not_durable` (no persisted coverage yet, round-12 minor). |
| `mark_findings_pending` fails before **any** reviewer call (fresh or resumed) | Turn aborts before the model call (nothing billed); never proceeds marker-less. There is **no exemption** — a `fresh: true` that overwrites an existing record must be marked too, or a crash before `record_turn` leaves the old record resumable (round-9 major #1). The fresh/no-existing-record case is harmless, not exempt. (Decision 5, round-4 major #3.) |
| Corrupt store + `fresh: true` | `fresh` is gated too: no convergeable record is written on an `invalid` store; recovery is an explicit operator action, the corrupt file is never auto-replaced (round-4 major #2). |
| Prose lists a finding the machine block omits | Not machine-detectable without a prose parser; narrowed by the prompt's completeness contract (block is the sole machine source) and by `converged` certifying only the structured contract; prose always returned for a human (round-4 major #5). |
| Lookalike `_OUT` marker embedded in repo / quoted in prose | Server strips/neutralises non-nonce `_OUT` marker lines from prose; the real `_OUT` block carries this result's nonce; client parses only the nonce-matching one (round-4 major #4). |
| Degraded turn mid-session (resumed turn, no valid block or reconciliation fail-closed) | Prior ledger preserved untouched, **and `ledger_coverage` transitions `whole_conversation → needs_rebaseline` durably and stickily** (round-7 major): the turn's ungrounded prose could carry an untracked finding, so the session can never again converge — reported `ledger_unavailable`, **escalate for a rebaseline handoff** (round-11 major #4), not an autonomous `fresh`. If that transition can't persist, the `.findings-pending` marker (still set) blocks resume as `turn_not_durable`. |

Every "whole-turn degrade" row above is a *degraded turn*, so it also breaks coverage
(`needs_rebaseline` on a resume, `legacy_uncovered` on turn 1) and its **top-level
`non_convergence_reason` is `ledger_unavailable`** when the on-disk break is persisted — this turn's
write landed, *or* the session was already `legacy_uncovered`/`needs_rebaseline` (round 8) — or
**`turn_not_durable`** only if this turn entered `whole_conversation`/`unestablished` and its own
write failed (the persistence-first rule). `unstructured` is never a top-level reason, only the internal
cause in the warning detail. Both `ledger_unavailable` and `turn_not_durable` **escalate to a human
rebaseline** (round-11 major #4), never an autonomous `fresh`.

The distinction is durable and keyed on the coverage stored **inside the ledger** (`Ledger.coverage`,
not a separate `SessionRecord` field), not inferred per turn. The `LedgerCoverage` enum has five
values, but **only three are ever persisted**: **`whole_conversation`** (ledger from turn 1 →
convergeable), **`legacy_uncovered`** (pre-feature / degraded-turn-1 ungrounded history →
non-convergent, sticky), and **`needs_rebaseline`** (a mid-session degraded turn broke coverage →
non-convergent, sticky). The other two are never written to disk: **`invalid`** (present-but-unreadable
ledger → non-convergent, session preserved, never reset) is **derived at load** from the ledger bytes
— because coverage lives inside the ledger, an unreadable ledger simply has no readable coverage, so
there is no separate poison-write; and **`unestablished`** (nothing durably persisted yet — a fresh
turn 1 whose write failed) is a **transient** report-only value that `is_structurally_valid` rejects
if it is ever found on disk. Delivery differs by readability: the two **readable** break states
(`legacy_uncovered`/`needs_rebaseline`) come back as **completed envelopes** with
`non_convergence_reason: ledger_unavailable`; **`invalid`** (unreadable) is caught at load and comes
back as a **`SESSION_NOT_RESUMABLE` refusal** carrying `ledger_unavailable` as its detail (never a
completed envelope, round 17); **`unestablished`** is a completed envelope reporting
`turn_not_durable` (it only appears as the pre-turn coverage on a failed-write turn), never
`ledger_unavailable`. All escalate → rebaseline.
Separately, the *store* load is tri-state (`absent`/`valid`/`invalid`) so file-level corruption
cannot pose as absence. Only `whole_conversation` can reach `converged`; recovery from any other
state is a human-directed rebaseline, not an autonomous `fresh`.

## Tests

- **`src/findings.rs` unit tests** — extraction (one clean block bearing the review nonce; a
  block among other fences and quoted examples; a block with a *stale/foreign* nonce → not
  matched; zero blocks; two blocks → degrade; unterminated block; over-cap field);
  reconciliation (turn-1 assignment; resolve; still-open; regressed-from-resolved; new finding
  mid-session; **a prior id the reviewer omitted → whole-turn degrade**; unknown-id → degrade;
  duplicate-id → degrade; `new_findings` with a status → degrade; monotonic non-reuse of ids
  across many turns); convergence + verdict (every row of the truth table; approve-with-open
  contradiction → `verdict_contradiction`; request-changes-with-none → `verdict_contradiction`;
  approve-with-comments + zero open → `reviewer_withheld_approve`, not converged; **`blocked` →
  `reviewer_blocked` at any open count** (round-5 major #4); unstructured →
  `converged:false`, `open_count:null`, and — being a degraded turn — `non_convergence_reason:
  ledger_unavailable` **when the on-disk break is persisted (this turn's, or an already-broken
  session), else `turn_not_durable`** (entered `whole_conversation`/`unestablished`, own write
  failed — both tested; never `unstructured`, internal-only, round-8/round-15/round-19)); **`open_count`
  counts a `regressed` finding** (`count(status != resolved)`, round-4 minor); **reason precedence**
  (a `legacy_uncovered` session with open findings reports `ledger_unavailable`, not
  `open_findings`; `reviewer_blocked` outranks `verdict_contradiction`); **a zero-open but
  over-budget turn does *not* converge** (round-6 major #1: `ledger_too_large` is in the conjunction).
- **`src/session.rs`** — ledger round-trips through persistence; a genuinely fresh turn-1
  converges (including with zero findings); a **resumed pre-ledger conversation is stamped
  `legacy_uncovered` and stays non-convergent across *several* later turns** even as its ledger
  fills, not just on the first late-ledger turn; a **degraded turn 1 is stamped `legacy_uncovered`**
  (round-4 major #1); a **mid-session degraded turn transitions `whole_conversation →
  needs_rebaseline`**, tested as valid t1 → malformed t2 with a new prose finding → valid t3 that
  **does not converge** and reports `ledger_unavailable` (round-7 major); a **`whole_conversation`
  record whose ledger later fails to load is `invalid` at load (derived from the unreadable bytes,
  no poison-write) and never accepts a replacement** (round-4 major #1's hazard is unreachable
  because coverage lives inside the ledger); a corrupt-ledger *record* degrades only its own session, and
  **`fresh` heals a poisoned record in a valid store** (round-5 major #1), and **a `fresh`-over-a-bad-
  record turn 1 whose own `record_turn` fails yields `ledger_coverage: unestablished` /
  `turn_not_durable` / `findings: []`, leaving the old record untouched** (round-21 major); **a
  corrupt whole store loads `invalid`, refuses *every* review (resume *and `fresh`*) before the model
  call as a `SESSION_NOT_RESUMABLE`-class refusal with `state_corrupt` detail — no model call, no
  one-shot review, no completed envelope — propagates `invalid` through
  `get`/`record_turn`/`forget`/`list`, and never poses as an empty/fresh session** (round-4 major #2
  / round-21 major); because coverage lives inside the ledger, an **unreadable ledger is
  self-durably `invalid` at load with no separate poison-write, and can never be usable as
  `whole_conversation`** (round-5 major #5); an **incompatible-version or otherwise-unreadable
  ledger record is `invalid` → a pre-model `SESSION_NOT_RESUMABLE` resume refusal, not a completed
  envelope** (round-17/18 major); the **dedicated `.findings-pending` sidecar blocks resume
  independently of the Perforce `.pending` one** (including a **pre-upgrade Perforce-only sidecar
  across a restart**, round-7 minor #3), a **failed `mark_findings_pending` aborts the turn before
  the model call on every turn including `fresh`** (round-4 major #3), a **crash-injected failed `fresh` overwrite of
  an existing record blocks a subsequent normal resume** (round-9 major #1), and a `fresh` call
  clears it; a **during-turn over-cap ledger is persisted atomically with a sticky
  `terminal_reason=ledger_too_large` that `resume_block` then refuses on** (round-6 major #2), while
  a **failed during-turn `record_turn` reports `turn_not_durable`** (deterministic by persist
  result); a **before-call over-cap (budget lowered between turns) invokes no reviewer, advances no
  turn, writes no findings marker, persists `terminal_reason` via an idempotent single-field replace, and
  returns a completed `ledger_too_large` (a human-escalation reason, so the loop stops); a failed
  terminal-flag persist there is simply non-convergent `ledger_too_large`, re-attempted only on the
  next operator-directed call (never an autonomous retry), never `turn_not_durable`** (round-11
  major #3 / round-12 major #2);
  **`fresh` after over-cap produces a clean, convergeable record** (`terminal_reason` cleared,
  round-9 minor #3). **The load-time `invalid` detection (`whole_conversation → invalid`, derived at
  load) and the during-turn `needs_rebaseline` transition are tested as *separate* paths** (round-11
  major #2 — they were wrongly conflated): the **`invalid` detection is a load-time event before any
  model call**, so an unreadable ledger surfaces as a **resume refusal / `ledger_unavailable`**,
  never `turn_not_durable` (no turn ran) — asserted same-process and across a restart; the
  **`needs_rebaseline` transition happens during a reviewer
  turn**, so its `record_turn` **success ⇒ `ledger_unavailable`, failure ⇒ `turn_not_durable`**.
  **Fault-injection cases for the persistence-first rule** (round-10 minor): a failed coverage-break
  write during a turn on (i) a **fresh turn 1** and (ii) a **mid-session transition** from
  `whole_conversation` both report `turn_not_durable` with coverage unchanged on disk; and (iii) a
  turn on an **already `legacy_uncovered`/`needs_rebaseline` session** (not `invalid` — that refuses
  resume before a turn runs) whose write also fails reports `ledger_unavailable` (on-disk coverage
  already broken, precedence wins) — all escalate (below), none silently proceed.
- **`src/prompt.rs`** — the **machine-block contract renders on turn 1 *and* every resumed turn**
  with the current nonce, not only in the turn-1 preamble; the resumed prompt lists
  **all** prior findings (open and resolved) with ids as quoted evidence and the "every id exactly
  once" instruction; a fresh prompt lists none.
- **`src/mcp.rs` / `src/tools.rs`** — a completed result carries both `structuredContent` and the
  server `_OUT` text block, and validates against `outputSchema`; **the reviewer's `_IN` block is
  stripped from the rendered prose, a lookalike `_OUT` marker in prose is stripped/neutralised, and
  the text channel holds exactly one nonce-bearing `_OUT` block** (round-4 major #4); **a running
  result emits the running variant** with the convergence/findings keys **absent, not `null`**
  (round-6 major #3), via the shared renderer, and **both variants validate against the
  discriminated `outputSchema`** (round-5 major #3); a degraded completed review carries a
  well-formed envelope with `structured:false`, `open_count:null`, `converged:false`; **the
  completed envelope carries `ledger_coverage` and `findings_trusted`** (round-12 major #4) —
  asserted for the **four completed-envelope** coverage states: `whole_conversation` trusted with
  findings; `legacy_uncovered`/`needs_rebaseline` trusted with the intact open findings still listed;
  and **`unestablished` (a fresh turn 1 whose `record_turn` failed → `turn_not_durable`) untrusted
  with `findings: []`, never mislabelled `whole_conversation` or `legacy_uncovered`** (round-13 major
  #1); a `turn_not_durable` turn reports the pre-turn on-disk coverage (`whole_conversation` with the
  intact prior findings, or `unestablished` for a fresh turn 1). All validate against the completed
  `outputSchema` variant, whose `ledger_coverage` enum has exactly those **four** values. **`invalid`
  is *not* a completed-envelope value** (round-17 major): it is tested as a **`SESSION_NOT_RESUMABLE`
  resume refusal** (an error result with remediation, no completed envelope and no prose), never as a
  completed envelope.
- **`smoke.ps1`** — the live round trip asserts a parseable envelope on a real review and that
  a resumed turn reports a prior finding by its stable id. This costs tokens; it runs when the
  change touches the protocol or session handling, which this does.

## Documentation

- **README** — a section on the envelope: the two channels, the `converged` loop contract
  (never a bare `open_count == 0`), stable ids across a resume, the `structured:false`
  degradation, and the stated residual injection risk with the Claude direction as the confined
  one. The existing "Re-reviewing after you act on feedback" section gains the ID-keyed status
  story.
- **AGENTS.md** — the merge-gate workflow can now loop on `converged` and reference findings by
  stable id rather than hand-maintaining the map, which is the concrete win the issue was filed
  about.

## Round 1 response

Codex (gpt-5.6-luna, effort=max) returned REQUEST CHANGES. Every finding was accepted; none was
disputed. What changed:

1. **Unsafe `open_count == 0` on a degraded turn** → introduced [`converged`](#the-single-safe-termination-signal-converged)
   as the only loop signal, made `open_count` `null` (not `0`) whenever unstructured, and
   defined `verdict: unknown` for degraded turns. A zero and a convergence are now reachable
   only through the fully-validated path.
2. **Resolved findings dropped from the resumed prompt, making `regressed` unreachable** →
   the injected digest now carries **all** prior findings, open and resolved, so a regression
   reattaches to its original id (Decision 2, and the failure table row).
3. **New findings keeping `resolved`; unknown ids ignored; no merge/split/dup rule** → the
   block is split into status-only `prior_findings` and status-less `new_findings`; unknown and
   duplicate ids and any schema violation now **degrade the whole turn** rather than being
   warned-and-ignored; merges/splits are defined out of existence by the reviewer's inability
   to mint or retire ids (Decision 2).
4. **Echoing an id doesn't prove identity (paraphrase/retarget/injection)** → the server owns
   immutable finding content; the reviewer supplies status only and cannot rewrite what an id
   means. The residual injection risk is stated explicitly, not claimed closed, with the Claude
   direction named as the confined one (Decision 4).
5. **Fenced-JSON extraction underspecified** → exact begin/end sentinel markers, **exactly one**
   block required, fail-closed on zero/many/malformed/over-cap, prior ledger rendered as quoted
   evidence (Decision 4).
6. **Idempotency across a crash between the reviewer turn and the ledger write** → the ledger
   write is part of the existing atomic session-record replace; a persist failure degrades the
   turn and marks the session non-resumable, so no stale-id retry is possible; the residual
   lost-turn window is stated (Decision 5).
7. **Incompatible/missing ledger treated as fresh; whole-file deser can erase the session** →
   "never had a ledger" (safe, convergeable) is now distinguished from "ledger unreadable"
   (non-convergent, session preserved); the ledger field loads tolerantly so corrupt bytes
   cannot erase the session (Decision 3, `src/session.rs` wiring, failure table).
8. **Incomplete verdict truth table** → [full truth table](#verdict-truth-table) added,
   including `BLOCKED`/uncertain never converging and `approve_with_comments` being distinct
   from an open finding.
9. **`outputSchema` must cover running results** → the envelope carries `result_status` and one
   schema validates running and completed results alike (["One schema for every successful
   response"](#one-schema-for-every-successful-response)).

## Round 2 response

Codex (gpt-5.6-luna, effort=max) returned REQUEST CHANGES again, confirming round-1 items #1,
#3-schema, #4, #5-core and #9 resolved and flagging four majors + one minor. All accepted; none
disputed. What changed:

1. **A resolved prior finding could be omitted and silently keep `resolved`** (only omitted
   *open* findings were flagged), so an unmentioned regression could leave every entry resolved
   and converge → **`prior_findings` must now account for every prior id, open and resolved,
   exactly once; any omission degrades the whole turn** (Decision 2; the `converged` conjunction;
   failure table). The `unaddressed_this_turn` silent-keep path is gone entirely.
2. **Decision 5 did not actually prevent stale resumes** — `resumable` is in-memory rendering
   state, `SessionRecord` had no durable non-resumable bit, and a crash before the replace could
   mark nothing → **a durable `pending_turn` write-ahead marker**, written before the reviewer
   call and cleared only by the atomic replace that writes the matching ledger; `resume_block`
   refuses a resume while it is set, forcing `fresh` (Decision 5, wiring).
3. **A pre-feature session with no ledger is not safely convergeable if its conversation is
   resumed** (ungrounded prose findings with no ids) → **the three-way rule**: only a ledger that
   has covered a conversation since its own turn 1 permits convergence; a resumed pre-ledger
   conversation is reviewed but non-convergent (`ledger_unavailable`) until a `fresh` restart
   ([new section](#convergence-requires-a-ledger-over-the-whole-conversation)).
4. **Self-contradiction: `converged` "defined as `open_count == 0`" vs. the truth table needing
   the reviewer's verdict too** → the shorthand is removed; `converged` is only ever the full
   conjunction, and a new machine-readable [`non_convergence_reason`](#non_convergence_reason)
   both resolves the contradiction and adds the livelock guard the reviewer asked for
   (`reviewer_withheld_approve` tells a loop to escalate rather than spin).
5. **(minor) Per-review nonce + honesty about quoted evidence** → the sentinel now carries a
   per-review nonce derived from the review id (defeats a *static* embedded lookalike; stated not
   to defeat an *active* injection reading the current nonce), and the "quoted evidence" claim is
   softened to acknowledge the injected titles/locations are themselves model-authored data
   (Decision 4).

Two of the reviewer's closing notes are adopted directly: the **content hash is dropped** (with
the reviewer no longer resupplying prior content, it would only detect the server's own rendering
drift; if ledger-record integrity is ever wanted, a record-level checksum/version is the right
tool, noted as a future option), and **flag-and-keep-open remains the default with no
auto-resolve** — reinforced now that omission degrades rather than being silently kept, so a
finding cannot be quietly waited out at all.

## Round 3 response

Codex (gpt-5.6-luna, effort=max) returned REQUEST CHANGES, confirming round-1 extraction,
immutable ids, all-history injection, fail-closed schema validation, the truth table, and
running-result coverage resolved, and the durable marker "sound on paper." Four majors + one
minor. All accepted; none disputed. What changed:

1. **Provenance was not durable** — a legacy session could resume ledgerless, persist a ledger
   this turn, and look whole-conversation next turn (and an empty turn-1 ledger was
   indistinguishable) → **persisted `ledger_coverage` (`whole_conversation` | `legacy_uncovered` |
   `invalid`)**, written when a ledger is first attached and immutable thereafter; only
   `whole_conversation` converges; `legacy_uncovered` is sticky across later turns. Tests now span
   *several* resumed turns after the first late-ledger turn
   ([whole-conversation rule](#convergence-requires-a-ledger-over-the-whole-conversation)).
2. **Nonce/block instructions were in the turn-1-only preamble; the follow-up said "each id it
   addresses," contradicting total-accounting** → the **machine-block contract now renders on
   every turn** in the per-turn section with the current nonce, and the resumed instruction is
   "**every prior id exactly once**" (Decision 1; [prompt changes](#prompt-changes-srcpromptrs)).
3. **Two nonce-bearing blocks in the text channel** (reviewer's raw block + server's, same
   markers) → **disjoint marker namespaces** (`CROSS_REVIEW_FINDINGS_IN` for the reviewer,
   `CROSS_REVIEW_ENVELOPE_OUT` for the server) and the **reviewer block is stripped from the
   rendered prose**; the text channel holds exactly one `_OUT` block (Decision 4; the envelope;
   `src/tools.rs` wiring).
4. **A tolerant *field* deserializer does not save a corrupt whole store** — `read()` maps a
   parse failure to an empty set, so corruption poses as absence and the next call starts a clean
   convergeable conversation → **the store load is tri-state** (`absent` | `valid` | `invalid`);
   an `invalid` store refuses resume and forces `state_corrupt` non-convergence, never posing as
   empty; recoverable-vs-poisoning cases are documented (`src/session.rs` wiring; failure table).
5. **(minor) Unbounded ledger/digest growth under total-accounting** → a **bounded ledger/digest
   budget checked before the reviewer runs**, exceeding it is an escalation (`ledger_too_large`),
   never a silent id retirement; `reviewer_withheld_approve`, repeated `unstructured`,
   `state_corrupt`, `ledger_too_large`, and `SESSION_NOT_RESUMABLE` are defined **human-escalation
   outcomes** ([bounded growth](#bounded-growth-and-escalation-outcomes); the reason table).

The two round-3 recommendations that were open questions are now resolved in the design: the
total-accounting cost is bounded (open q1 → the bounded-growth section), and the livelock has a
defined stopping point (open q2 → escalation outcomes).

## Round 4 response

Codex (gpt-5.6-luna, effort=max) returned REQUEST CHANGES, confirming round-3 #2 fully resolved
and the legacy/collision/parse-as-absence *normal* cases addressed. Five majors + one minor. All
accepted; none disputed. What changed:

1. **Provenance not fully durable** — `invalid` was "this load," with no persisted
   `whole_conversation → invalid` poison, and a degraded turn 1 had no anchor → `ledger_coverage`
   is now an explicit **one-way state machine**: a corrupt load poisons durably to `invalid`
   (sticky, never accepts a replacement ledger); a degraded turn 1 is stamped `legacy_uncovered`;
   `whole_conversation` is set only on a clean, structured, atomically-persisted turn 1; only
   `fresh` re-establishes it
   ([whole-conversation rule](#convergence-requires-a-ledger-over-the-whole-conversation)).
2. **Corrupt store contradicted the `fresh` escape** (`fresh` bypasses lookup, so it would write a
   convergeable record over unreadable state) → **an `invalid` store gates `fresh` too**: no
   convergeable state is established on a corrupt store, recovery is an explicit operator action,
   the corrupt file is never auto-replaced, and `invalid` propagates through
   `get`/`record_turn`/`forget`/`list` (`src/session.rs` wiring; failure table).
3. **Write-ahead guarantee was conditional on an unstated successful marker write** (the code does
   `mark_pending().is_ok()` and continues) → **a failed marker write aborts a resumed turn before
   the model call**; fresh turn 1 is the stated, tested exception (nothing to strand)
   (Decision 5).
4. **`_OUT` had no nonce and running results weren't routed through one renderer** → the `_OUT`
   marker now carries the **per-result nonce**, the server **strips lookalike `_OUT` marker lines
   from prose**, and **both running and completed results go through one renderer** emitting exactly
   one nonce-bearing `_OUT` block (the envelope; `src/tools.rs`/`src/mcp.rs` wiring).
5. **A valid block could silently contradict retained prose** (prose lists a major, block says
   approve, server converges) → the block is declared the **sole machine-authoritative source**,
   the prompt makes **block completeness and block/prose verdict agreement a contract**, and
   `converged` is **scoped honestly** to "the structured contract passed," not "a human agreed";
   the residual (undetectable without a prose parser) is stated (Decision 1).
6. **(minor)** `open_count` is now the explicit formula **`count(status != resolved)`** (so
   `regressed` counts), and `non_convergence_reason` has a **deterministic precedence** so a
   permanently-uncovered session always reports `ledger_unavailable`, never `open_findings`
   ([the reason table](#non_convergence_reason)).

The reviewer's answers to the round-3 open questions are folded into the design: the bounded-growth
cap is **finite and non-disableable, digest-bytes primary + finding-count secondary, enforced
before *and* after** reconciliation; `legacy_uncovered` migration stays an **explicit `fresh`
restart carrying human-selected open findings** (no auto re-baseline, which could not prove the old
prose findings exhaustive); and the **review-id-derived nonce is kept** (sufficient against a static
embed; randomness would not defeat an active injection) and **applied to `_OUT` too**.

## Round 5 response

Codex (gpt-5.6-luna, effort=max) returned REQUEST CHANGES, confirming rounds 1–4 resolved on paper
(the failed-marker and `_IN`/`_OUT` namespace fixes explicitly). Five majors + one minor, all
consistency/precision rather than new mechanism. All accepted; none disputed. What changed:

1. **`fresh` recovery semantics contradicted themselves** (Decision 5 said `fresh` replaces a
   poisoned record; the store section said `fresh` is gated on a corrupt store) → the two are now
   explicitly distinguished: a poisoned **record** in a **valid** store is healed by `fresh`; a
   corrupt **whole store** gates `fresh` and needs operator recovery. Decision 5, the store wiring,
   and the generic failure-table `fresh` row all say so.
2. **Post-reconciliation cap had no durability outcome** (discarding the over-cap ledger while
   clearing the marker recreates stale-ledger divergence) → the complete over-cap ledger is
   **persisted atomically (nothing retired)** and the session marked **non-resumable
   `ledger_too_large`**; a persist failure there is a `turn_not_durable`; the reason definition
   now reads "before *or* after."
3. **Running envelope fields were undefined** → the `outputSchema` is now a **discriminated
   union on `result_status`**: the running variant has **no** convergence/findings group (not
   "null"), the completed variant carries the full envelope and the "null iff converged" rule; both
   validate, every running field is pinned and tested.
4. **`blocked` + zero open had no reason row** → added **`reviewer_blocked`** (a human-escalation
   outcome at any open count), placed in the truth table and the precedence order above
   `verdict_contradiction`.
5. **Poison-write failure was undefined** → it **fails closed**: a failed `whole_conversation →
   invalid` persist leaves the record non-convergent and non-resumable (`state_corrupt` /
   `SESSION_NOT_RESUMABLE`), never usable as `whole_conversation`; self-healing toward `invalid` on
   the next load; tested by injecting the failure.
6. **(minor)** The **exact completeness/verdict-agreement contract is now in the per-turn prompt**
   (not only the design prose), and the absolute "never silently dropped" claim is **qualified** to
   "within the structured contract," acknowledging the one reviewer-non-compliance residual. Per the
   reviewer's own note, no self-reported prose-finding count is added (it would not prove
   completeness).

The reviewer's remaining answers are folded in: keep the review-id-derived nonce; keep a degraded
turn 1 permanently `legacy_uncovered` (retries are safe only as separate fresh conversations before
any ledger is attached — the caller can already do that by calling `fresh`); keep
`approve_with_comments`+zero-open non-convergent; and refusing all starts on a corrupt whole store
does not deadlock recovery (move the file / new `--state-dir`). No open questions remain; the two
prior ones are resolved by those answers and are not re-raised.

## Round 6 response

Codex (gpt-5.6-luna, effort=max) returned REQUEST CHANGES, confirming rounds 1–5 resolved and
flagging three majors + three minors — all **internal-consistency gaps introduced by the round-5
edits**, no new mechanism. All accepted; none disputed. What changed:

1. **`ledger_too_large` was not in the `converged` conjunction** though the post-cap path forced
   `converged: false` → the budget/terminal-state condition is now **part of the sole `converged`
   definition** (a zero-open but over-budget turn does not converge), removing the contradiction.
2. **Over-cap persistence had no durable non-resumable field** (clearing the marker left an
   "apparently normal" record) → a **sticky `terminal_reason = ledger_too_large`** is written on
   `SessionRecord` in the *same* atomic replace as the over-cap ledger and cleared marker;
   `resume_block` refuses on it before the session is claimed; and the `ledger_too_large`-vs-
   `turn_not_durable` ambiguity is resolved deterministically **by whether the atomic replace
   succeeded** (success ⇒ `ledger_too_large`, failure ⇒ nothing written ⇒ `turn_not_durable`), so
   precedence never has to choose.
3. **The `render_running` wiring still said the group is emitted as `null`**, contradicting the
   discriminated schema → corrected to **absent, not `null`**, and the generic "the envelope always
   has `converged`" wording is scoped to *completed* results.
4. **(minor)** "trust `converged` to mean everything the reviewer raised" is narrowed to
   **"everything represented in the validated machine contract"**, matching the honestly-stated
   prose-omission residual.
5. **(minor)** Stale wording fixed: "unaddressed flagging" → **missing-id detection/degradation**
   (the silent-keep path is gone), and the failure table's `request_changes`/`blocked` row is
   **split**, with `blocked` reporting `reviewer_blocked`.
6. **(minor)** The poison-write-failure outcome is disambiguated: it is a **pre-model resume
   refusal** reported as `ledger_unavailable` (not `state_corrupt`, which stays reserved for a
   whole-store parse failure), with same-process and restart both tested, and a failure-table row
   added.

No open questions remain.

## Round 7 response

Codex (gpt-5.6-luna, effort=max) returned REQUEST CHANGES, confirming round 6 coherent and rounds
1–5 without regression. One major + two minors. All accepted; none disputed. What changed:

1. **(major) A mid-session degraded turn was not durably non-convergent** — a resumed turn with no
   valid block preserved the old ledger, but nothing stopped a *later* clean turn from resolving all
   the old ids and converging while that turn's ungrounded prose finding sat untracked → a
   **mid-session degrade now transitions `whole_conversation → needs_rebaseline`** durably and
   stickily (non-convergeable, `fresh`-to-clear), so the session can never again converge. Tested as
   valid t1 → malformed t2 (new prose finding) → valid t3 must not converge.
2. **(minor) Degraded-turn reason ambiguity** (`unstructured` "re-review" vs `legacy_uncovered`
   "fresh") → **coverage is now stamped before reason selection**, so any degraded turn breaks
   coverage first and the reported reason is deterministically `ledger_unavailable` ("go `fresh`"),
   never a conflicting bare `unstructured`; the truth/reason/failure tables are aligned.
3. **(minor) The new marker's relationship to the existing Perforce sidecar was unspecified** →
   the write-ahead marker now **reuses the existing `.pending` sidecar**
   (`mark_pending`/`clear_pending`/`is_pending`, already fail-closed) rather than adding a
   `SessionRecord.pending_turn` field; coexistence is trivial (one marker, one meaning), a
   pre-upgrade Perforce-only sidecar is understood unchanged, and this is a strictly smaller change.
   Decision 5, the over-cap path, the wiring, and the tests are updated to the sidecar.

No open questions remain.

## Round 8 response

Codex (gpt-5.6-luna, effort=max) returned REQUEST CHANGES, confirming round-7 #1 (the
`needs_rebaseline` state) and #3 (the sidecar reuse) resolved, and #2 only partially — because the
round-7 edits added `needs_rebaseline` and the "coverage before reason" rule but did not propagate
them through every table. One major + two minors, all propagation/consistency. All accepted; none
disputed. What changed:

1. **(major) `needs_rebaseline` / the coverage-before-reason rule were not threaded through the
   reason contract** → `unstructured` is now **removed as a `non_convergence_reason` value**
   (internal cause only, carried by `structured:false` + warning); a degraded turn's top-level
   reason is always `ledger_unavailable`. Propagated to the reason table, the precedence order, the
   fail-closed reconciliation text, the truth table (new footnote 4), every failure-table degrade
   row, the four-way `ledger_coverage` summary, the `fresh` recovery row, and the tests.
2. **(minor) Persistence-failure reason for the `needs_rebaseline` transition was ambiguous** →
   made **deterministic by result** exactly like the over-cap path: `record_turn` success ⇒ durable
   `needs_rebaseline`, this turn reports `ledger_unavailable`; failure ⇒ nothing persisted, sidecar
   stays set, this turn reports `turn_not_durable`. Either way the break is durable.
3. **(minor) The over-cap failure-table row still described the old "cleared marker in one atomic
   replace" model** → corrected to "ledger + `terminal_reason` atomic via `record_turn`; the
   `.pending` sidecar cleared *afterward*; a clear failure leaves a stale marker that safely forces
   `fresh`," matching the revised Decision 5 atomicity.

No open questions remain.

## Round 9 response

Codex (gpt-5.6-luna, effort=max) returned REQUEST CHANGES, confirming round-8 #1 and #3 resolved
and no regressions. One major + two minors, all edge-case tightening. All accepted; none disputed.
What changed:

1. **(major) `fresh: true` overwriting an existing record was not covered by the write-ahead
   marker** — `fresh` sets `prior = None` but only overwrites on `record_turn`, so a crash between
   advancing the new conversation and `record_turn` left the *old* record on disk with no marker,
   resumable → the fresh review bypassed. Fixed by **marking every turn (fresh and resumed), not
   just resumes**; the fresh-turn-1 exemption is removed. A crash-injected failed `fresh` overwrite
   now blocks a subsequent normal resume (tested).
2. **(minor) The `needs_rebaseline` persist-failure rule appeared to conflict with the general
   "degraded turn → `ledger_unavailable`" rule** → made the **evaluation order explicit**:
   persistence first (failed write → `turn_not_durable`, coverage unchanged, sidecar enforces the
   break), then coverage stamp, then reason selection (successful write → `needs_rebaseline` +
   `ledger_unavailable`). So the two never both fire.
3. **(minor) `fresh` recovery did not explicitly clear `terminal_reason`** — the fresh-recovery row
   listed the coverage states but not `ledger_too_large`, so an implementation could leave a fresh
   record permanently terminal → **`fresh` now clears `terminal_reason` too**, as part of resetting
   all durable non-convergent state; tested that `fresh` after an over-cap yields a clean,
   convergeable record.

No open questions remain.

## Round 10 response

Codex (gpt-5.6-luna, effort=max) returned REQUEST CHANGES, confirming round-9 #3 resolved and the
worker/Decision 5 prose correct, but catching **two stale failure-table rows** the round-9 rewrite
left behind. One major + one minor, both consistency. All accepted; none disputed. What changed:

1. **(major) A stale failure-table row still said the marker applies "before a resumed call" and
   "fresh turn 1 is exempt"** — contradicting the corrected worker and Decision 5, and reintroducing
   the fresh-overwrite hole on paper → the row now says **`mark_pending` must succeed before *every*
   reviewer call (fresh or resumed), or abort**; the fresh/no-record case is harmless, **not** an
   exemption.
2. **(minor) Several rows/footnotes still said "every degraded turn reports `ledger_unavailable`"**
   without the persistence-first qualification, and the **already-non-convergent** case was
   undefined → centralized the rule: `ledger_unavailable`'s precondition is the **on-disk** coverage
   (all prior persisted transitions), so an already-broken session reports `ledger_unavailable`
   regardless of this turn's write (precedence over `turn_not_durable`); `turn_not_durable` is the
   reason in exactly one case — a turn that entered `whole_conversation` and whose own break-write
   failed. Qualified footnote 4, the reason-table row, and the failure-table summary note, and added
   fault-injection tests for fresh turn 1, mid-session, and already-non-convergent sessions.

No open questions remain.

## Round 11 response

Round 11 was a **fresh reviewer** (Codex, gpt-5.6-luna, effort=max): the prior review session hit
its 10-turn resume limit, so this was a clean holistic pass over the whole document with no
accumulated agreement to lean on — which is exactly why it was worth doing. It surfaced four majors,
two of them genuinely new (the undefined before-reviewer over-cap path, and the unsafe autonomous-
`fresh` loop advice) that ten resumed rounds had not. All accepted; none disputed. What changed:

1. **The verdict truth table and the canonical JSON example contradicted the `converged`
   conjunction** — the table's `converged` column read unconditionally and the example showed
   `approve` + open findings with reason `open_findings` → the table now states explicitly that its
   `converged` column holds **only under the rest of the conjunction** (coverage, reconciliation,
   in-budget) and lists each row's reason; the example is now a concrete, internally consistent
   `open_findings` case with a note on how a contradiction differs.
2. **The load-time poison-write and the during-turn `needs_rebaseline` transition were conflated in
   the tests** — separated: the poison-write is a pre-model event, so success *and* failure surface
   as a resume refusal / `ledger_unavailable` (never `turn_not_durable`); only the during-turn
   transition has the success⇒`ledger_unavailable` / failure⇒`turn_not_durable` split.
3. **The before-reviewer over-cap path was undefined** — now specified: an already-over-budget
   ledger on entry invokes no reviewer and advances no turn, persists `terminal_reason` with an
   idempotent single-field replace (no `.pending`), and returns a completed `ledger_too_large`
   envelope; covers a budget lowered between runs. A pre-call over-cap test is added.
4. **`ledger_unavailable` told an autonomous loop to blindly `fresh`, which can converge while
   abandoning the untracked findings that broke coverage** — the deepest catch. `ledger_unavailable`
   (and `turn_not_durable`) are now **human-escalation outcomes**, and recovery is an explicit
   [rebaseline handoff](#recovery-from-a-broken-ledger-is-a-human-rebaseline-not-an-autonomous-fresh):
   the loop stops, the envelope carries the still-known open findings where the ledger is intact,
   and a human decides what to carry into a `fresh` baseline. The loop contract is now "re-review on
   `open_findings`/`verdict_contradiction`, escalate on everything else." Propagated to the loop
   contract, the reason table, the escalation policy, and the failure table.

No open questions remain.

## Round 12 response

Codex (gpt-5.6-luna, effort=max, resumed) confirmed the round-11 truth-table caveat and the
poison/rebaseline test split resolved, and flagged four majors + one minor — mostly places the
round-11 changes had not been carried all the way through, plus one genuinely new field requirement.
All accepted; none disputed. What changed:

1. **The canonical JSON example was still inconsistent** (`open_count: 2`, `total_count: 5`, but one
   finding shown) → rewritten to three findings (one resolved, two open) with `open_count: 2`,
   `total_count: 3`, fully matching the count formulas.
2. **Before- vs after-reviewer over-cap was conflated in the failure table** → split into four rows:
   before-call (no reviewer, no turn, no `.pending`, idempotent field-persist, completed
   `ledger_too_large`; a failed persist is just non-convergent, never `turn_not_durable`) and
   after-reconciliation (`record_turn` writes ledger+`terminal_reason`, sidecar cleared after; a
   failed `record_turn` is `turn_not_durable`). Pre-call tests added.
3. **Active "go `fresh`" guidance still contradicted the escalation/rebaseline rule** → the loop
   footnote, the persistence-first passage, Decision 5's "no stale ids" restart (now explicit that
   identity is fresh but findings are re-supplied via the rebaseline), and the failure-table
   `turn_not_durable`/degraded-turn-1 rows all now say **escalate → human rebaseline carrying the
   selected findings**. Historical round-response text is left as history.
4. **The rebaseline envelope had no machine-readable coverage/trust discriminator** (the recovery
   section promised one) → added **`ledger_coverage`** (`whole_conversation` | `legacy_uncovered` |
   `needs_rebaseline` | `invalid`) and **`findings_trusted`** to the completed envelope and the
   completed `outputSchema` variant; on `invalid`, `findings` is `[]` and `findings_trusted` is
   `false` so an empty list is never mistaken for "no findings." Tested per coverage state.
5. **(minor) Fresh turn 1 conflicted with the persistence-first `turn_not_durable` condition** → the
   condition is now "on-disk coverage not already broken — `whole_conversation` on entry **or a
   fresh turn 1 with no persisted coverage**"; "prior ledger intact" qualified as "if one exists."

No open questions remain.

## Round 13 response

Codex (gpt-5.6-luna, effort=max, resumed) confirmed round-12 #1 (example counts), #2 (over-cap row
split), and #5 (fresh-turn-1 wording) resolved, and flagged two majors + two minors. All accepted;
none disputed. What changed:

1. **(major) `ledger_coverage` could not truthfully represent an unpersisted fresh turn** — a fresh
   turn 1 whose `record_turn` fails has no persisted coverage, but the completed envelope required
   one of four states and `findings_trusted` had no honest value → added a fifth state
   **`unestablished`** ("no durable coverage yet"), with `findings: []` and `findings_trusted:
   false`; a `turn_not_durable` turn now reports the pre-turn on-disk coverage
   (`whole_conversation` with intact findings, or `unestablished` for a fresh turn 1), never a
   mislabel. Added to the state machine, the shape notes, the `outputSchema` enum, and the tests.
2. **(major) Active recovery prose still said "go/forcing `fresh`"** in the worker bullet
   (`record_turn` failure), the fresh-overwrite mechanism, and the poison-write path → all now say
   the resume is refused as an **escalation → rebaseline**, not an autonomous restart. (Historical
   round-response text left as history.)
3. **(minor) The pre-call over-cap "retried" wording** was ambiguous against the escalation rule →
   clarified that `ledger_too_large` is a human-escalation outcome and the terminal-flag write is
   re-attempted only on the next **operator-directed** call, not by an autonomous loop.
4. **(minor) The top "Review history" still opened "Seven rounds"** while documenting twelve →
   corrected to thirteen (and kept in sync with the Status).

No open questions remain.

## Round 14 response

Codex (gpt-5.6-luna, effort=max, resumed) confirmed the canonical counts, truth-table caveat,
poison/`needs_rebaseline` split, four over-cap paths, `findings_trusted` semantics, and the
review-history count all resolved, and flagged two majors + two minors — every one a *stale summary*
left behind when `unestablished` and the escalation model went in, no new mechanism. All accepted;
none disputed. What changed:

1. **(major) `unestablished` was not threaded through the reason contract** — the reason-table note,
   the coverage summary, and the precedence still enumerated four states and implied all
   non-`whole_conversation` states report `ledger_unavailable`. Corrected: **`unestablished` maps to
   `turn_not_durable`, never `ledger_unavailable`** (it only appears as the pre-turn coverage on a
   failed-write turn); only the three *persisted* break states map to `ledger_unavailable`. The
   summary is now explicitly five-way.
2. **(major) A few active sections still said the fix is to go/`fresh`-clear** — the precedence
   rationale, the "permanently-uncovered → go fresh" line, and the `needs_rebaseline`/`invalid`
   state-clearing sentences → all now say **escalate → human-directed rebaseline `fresh`**.
3. **(minor) The persistence-first qualifier was missing from a couple of active statements** — the
   `unstructured`-is-not-a-reason note and Decision 2's fail-closed conditions now say a degraded
   turn reports `ledger_unavailable` **when the coverage-break write persists, else
   `turn_not_durable`**; the "prior id unaccounted" failure row too.
4. **(minor) The pre-call over-cap test still said "retried"** → now "re-attempted only on the next
   operator-directed call, never an autonomous retry," matching the prose and failure row.

No open questions remain.

## Round 15 response

Codex (gpt-5.6-luna, effort=max, resumed) confirmed the five-state definitions, summary, schema, and
coverage tests resolved, and flagged two majors + one minor — again all stale text, no new mechanism.
All accepted; none disputed. What changed:

1. **(major) The formal precedence line still said `ledger_unavailable` "covers any degraded turn"
   and preceded `turn_not_durable`** — a literal reading could misreport a failed-write fresh turn as
   `ledger_unavailable` → added an explicit "**persistence-first runs before precedence**" note, and
   `ledger_unavailable` now covers only **on-disk persisted** breaks (`invalid`/`legacy_uncovered`/
   `needs_rebaseline`), *not* a degraded turn whose own write failed (that is `turn_not_durable`).
2. **(major) Two active passages still said "restart `fresh`" / "go `fresh`"** — the Decision 5
   residual-limitation paragraph and the persistence-first success branch → both now route through
   **escalate → human-directed rebaseline `fresh` carrying the preserved findings**. The over-cap
   recovery message is aligned too. The only remaining "go fresh" strings are in historical Round-N
   response sections.
3. **(minor) Two parallel unconditional degraded-turn statements were missed** — the "reviewer emits
   no block" failure row and the convergence unit-test description now both say `ledger_unavailable`
   **when the coverage-break write persists, else `turn_not_durable`** (both tested).

No open questions remain.

## Round 16 response

Codex (gpt-5.6-luna, effort=max, resumed) confirmed round-15's precedence/persistence-first fix, the
degraded-turn classifications, the five-state model, trust semantics, schema, tests, and over-cap
paths all resolved and internally consistent, and re-flagged the Decision 5 residual-limitation
paragraph as still saying "restarted `fresh`" without escalation. That paragraph had in fact already
been converted to the escalation/rebaseline framing in round 15 (it read `turn_not_durable` →
"the session escalates and is recovered by a human-directed rebaseline `fresh` carrying the preserved
prior findings"), and a full grep confirmed no active "restart(ed) `fresh`" / "must be restarted"
passage remained anywhere — so the re-flag was a stale re-report against the pre-round-15 text.
Rather than dispute it and stop, the paragraph was tightened further to be unambiguous: it now states
`turn_not_durable` explicitly, that **the loop stops and escalates**, that recovery is a
**human-directed rebaseline** carrying the intact prior findings plus anything recovered from the
lost turn's prose, and that **no autonomous caller silently restarts and discards** the unpersisted
findings.

No open questions remain.

## Round 17 response

Codex (gpt-5.6-luna, effort=max, resumed) confirmed round 16 resolved and the rest of the contract
consistent, and flagged one major + one minor, both real. All accepted; none disputed. What changed:

1. **(major) `invalid` was contradictorily both a completed-envelope `ledger_coverage` value (with
   "reconstruct from prose") *and* a pre-model `SESSION_NOT_RESUMABLE` refusal** — impossible, since
   a refusal runs no turn and yields no prose. Resolved by making `invalid` **only** a resume
   refusal, never a completed-envelope value: the completed envelope's `ledger_coverage` is one of
   **four** run-reachable states (`whole_conversation`/`legacy_uncovered`/`needs_rebaseline`/
   `unestablished`); `invalid` (unreadable ledger, caught at load) is a `SESSION_NOT_RESUMABLE`
   error carrying `ledger_unavailable` as its detail, with an operator/`fresh`-re-review remediation
   and no prose. Threaded through the envelope example, the shape notes, the discriminated schema,
   the reason table + precedence, the five-state summary, the coverage bullet, the recovery section,
   and the tests. `findings_trusted` is now `false` only for `unestablished`.
2. **(minor) `last_seen_turn` was meaningless under total-accounting** (every id is reported every
   turn, so "last seen" = current turn) and the example was internally inconsistent → renamed to
   **`last_status_change_turn`** ("last turn the status changed"), defined in the shape notes, and
   the three example entries corrected.

No open questions remain.

## Round 18 response

Codex (gpt-5.6-luna, effort=max, resumed) confirmed `last_status_change_turn` resolved and flagged
one major: the round-17 `invalid`-is-a-refusal split had not reached every active path. All accepted;
none disputed. The remaining `invalid`-as-completed-envelope remnants are now removed:

- **Decision 3 is scoped to degradations of a turn that *ran***; an unreadable/incompatible ledger is
  explicitly called out as the pre-model `SESSION_NOT_RESUMABLE` refusal, *not* one of Decision 3's
  completed degraded-envelope cases.
- **The persistence-first "already-broken session" prose and its test** no longer include `invalid`
  in the set of states a turn can run on (an unreadable ledger refuses resume before a turn begins);
  only `legacy_uncovered`/`needs_rebaseline` remain.
- **The failure-table "incompatible/undeserializable record" row and the corresponding test** now
  classify that path as a **pre-model `SESSION_NOT_RESUMABLE` resume refusal** (record preserved,
  other sessions intact), not a completed `ledger_unavailable` envelope.

`invalid` is now uniformly a resume refusal everywhere active. No open questions remain.

## Round 19 response

Codex (gpt-5.6-luna, effort=max, resumed) confirmed the `invalid`-as-resume-refusal propagation
complete and no blind-`fresh`/invalid-envelope contradiction remaining, and flagged one **minor**: a
few "else `turn_not_durable`" statements didn't carry the qualifier that an *already*-broken
(`legacy_uncovered`/`needs_rebaseline`) session always reports `ledger_unavailable` regardless of the
current turn's write (precedence wins). Accepted; not disputed. Qualified everywhere — the reason
table `ledger_unavailable`/`turn_not_durable` rows, the `unstructured`-not-a-reason note, Decision
2's fail-closed conditions, the failure-table degrade rows and summary note, and the convergence
test — so `turn_not_durable` is stated as the reason **only** when this turn entered
`whole_conversation`/`unestablished` and its own write failed; a pre-existing persisted break is
always `ledger_unavailable`.

No open questions remain.

## Round 20 response

Codex (gpt-5.6-luna, effort=max, resumed) confirmed everything else resolved and flagged the single
remaining unqualified spot: **truth-table footnote 4** still said a failed current coverage write
reports `turn_not_durable` without excluding an already-broken session. Accepted; not disputed.
Footnote 4 now matches the formal rule: `ledger_unavailable` whenever the on-disk break is persisted
(this turn's write landed, or the session was already `legacy_uncovered`/`needs_rebaseline`);
`turn_not_durable` **only** when the turn entered `whole_conversation`/`unestablished` and its own
write failed.

No open questions remain.

## Round 21 response

Round 21 was a **fresh reviewer** (the prior session hit its 10-turn limit again). It confirmed the
normal convergence contract coherent and caught one genuinely new gap the resumed reviews had
missed: the **corrupt-store one-shot path had no valid envelope representation**. The plan had said a
corrupt `sessions.json` could run a one-shot review returning `state_corrupt`, but a completed
envelope requires a defined `ledger_coverage` (one of four), `findings_trusted`, and `open_count`,
and a corrupt store can supply none — so that "one-shot completed envelope" could not validate.
Accepted; not disputed. Resolved by making a corrupt store a **pre-model refusal**, exactly like an
unreadable ledger:

- A corrupt store **refuses every review (resume *and* `fresh`) before the model call**, as a
  `SESSION_NOT_RESUMABLE`-class refusal carrying `state_corrupt` as its detail — **no one-shot
  review, no completed envelope**. `state_corrupt` is therefore an error/refusal outcome, **never a
  completed-envelope `non_convergence_reason`**, and is removed from the completed-envelope
  precedence (the store-load wiring, the reason table, the precedence, and the failure/test rows).
- The reviewer's second ask is specified: a **`fresh`-over-a-bad-record turn 1 whose own persistence
  fails** is not special — it is the existing `unestablished` / `turn_not_durable` turn-1 case
  (completed envelope, `findings: []`, old record untouched), *distinct* from the corrupt-*store*
  case which runs no turn.

No open questions remain.

## Status

**APPROVED by the cross-model gate on round 22** (Codex, gpt-5.6-luna, effort=max): "the design is
now internally consistent," no findings. Twenty-one prior rounds each returned REQUEST CHANGES with
every finding accepted and none disputed; rounds 11 and 21 were fresh-reviewer holistic passes. The
twenty-two rounds carried the design from the block schema and ID reconciliation through the
escalation/rebaseline seam, the five-state coverage machine, `invalid`/`state_corrupt` as pre-model
refusals, the four over-cap paths, the persistence-first reason rule, and the discriminated
`outputSchema`. **Implementation may now begin** against the [wiring](#wiring-the-io-edges),
[tests](#tests), and [module](#new-module--srcfindingsrs) plan above.
