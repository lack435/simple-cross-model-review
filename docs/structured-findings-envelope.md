# Structured findings + machine-readable verdict — design

Status: **planned.** This document is the plan. It goes through this repository's own
`cross-review` gate before implementation begins, and the implementation goes through it again.

Tracks issue #40. The issue asks for a machine-readable envelope so that an
autonomous "re-review until converged" loop stops re-deriving structure the tool could
provide: a top-level verdict, an `open_count`, stable finding IDs carried across a resumed
session, and per-finding status (resolved / open / regressed) on a re-review.

> **Review history.** Three rounds against this repository's own gate (Codex, gpt-5.6-luna,
> effort=max), each REQUEST CHANGES, each finding accepted and none disputed: round 1 (seven
> `major`), round 2 (four `major` + one `minor`, five round-1 items confirmed resolved), round 3
> (four `major` + one `minor`, round-1 extraction/immutability/truth-table/running-result
> confirmed resolved). Recorded in [Round 1](#round-1-response), [Round 2](#round-2-response), and
> [Round 3](#round-3-response) responses. The sections they touched — the block schema, ID
> reconciliation (now total-accounting), degradation and the `converged` signal, extraction (now
> nonce-marked, dual-namespace), durability (write-ahead marker), the whole-conversation ledger
> rule (now durable `ledger_coverage` provenance), tri-state store load, bounded growth with
> escalation outcomes, and the verdict truth table — were rewritten rather than patched. This
> document reflects the post-round-3 design.

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
  clean approve; numeric zero is necessary but not sufficient.

If any one is false, `converged` is `false`, and the envelope carries a machine-readable
`non_convergence_reason` (below) naming which one, so an autonomous loop can tell "keep working,
findings still open" from "the reviewer withheld a clean approve, a human should look" and
**cannot livelock silently**. `open_count` is a number **only** when the block is structured,
the ledger is valid, and reconciliation was clean; otherwise it is `null`, never `0`. A caller
loops `while (!converged)`; it never has to reason about *why* a turn was not convergent to stay
safe, only to report the reason it is handed.

### `non_convergence_reason`

A non-convergent turn is not necessarily a *broken* turn, and an autonomous loop must be able
to act differently on the two. The envelope carries one machine-readable reason (or `null` when
`converged`):

| `non_convergence_reason` | Meaning | What a loop should do |
| --- | --- | --- |
| `open_findings` | Structured, valid, but `open_count > 0`. | Act on the findings, re-review. |
| `reviewer_withheld_approve` | `open_count == 0` but `verdict_detail` is `approve_with_comments`. | The reviewer left only comments and declined a clean approve. Do **not** auto-stop; surface to a human. |
| `verdict_contradiction` | Reviewer verdict and `open_count` disagree (e.g. `approve` with open findings, or `request_changes` with none). | Treat as changes; re-review. |
| `unstructured` | No valid block this turn (missing/duplicate/extra id, malformed block, wrong nonce, …). | Re-review; do not read absence as convergence. |
| `ledger_unavailable` | Ledger unreadable, incompatible, or `ledger_coverage != whole_conversation`. | Start a `fresh` session to establish a convergeable ledger. |
| `turn_not_durable` | This turn's ledger could not be persisted (pending marker left set). | Start a `fresh` session; do not resume (stale-id risk). |
| `state_corrupt` | The session store itself did not parse; this name's history cannot be trusted. | **Escalate to a human**; do not silently start fresh (below). |
| `ledger_too_large` | The ledger/digest exceeded the bounded budget before this turn ran. | **Escalate to a human**; the session cannot keep growing (below). |

Three of these are **explicit human-escalation outcomes**, not "re-review and hope":
`reviewer_withheld_approve` (the reviewer will keep withholding), `state_corrupt`, and
`ledger_too_large`. An autonomous loop that treats every non-convergence as "try again" would
spin on these; the reason field exists so it stops and surfaces them instead. A caller should
also escalate on **repeated `unstructured`** (a reviewer that cannot produce a valid block N
turns running is not going to) and on `SESSION_NOT_RESUMABLE` — both are covered in
[the escalation policy](#bounded-growth-and-escalation-outcomes).

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
- **Fail-closed conditions that degrade the *entire* turn** (→ `structured: false`,
  `non_convergence_reason: unstructured`, see Decision 3): the set of ids in `prior_findings` is
  not *exactly* the ledger's id set (a missing id, an extra/unknown id, or any duplicate); a
  `prior_findings` object carrying any field other than `id`/`status`; a `new_findings` object
  carrying a `status`; any enum out of range. The block is trusted as a whole or not at all — a
  block the server had to *partially* discard is a block it cannot reason about, and round 1 was
  right that "ignore the bad entry with a warning" is exactly how a serious finding silently
  leaves `open_count`. Whole-turn is the correct granularity precisely because per-finding
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

So the envelope is always returned, with a `structured` boolean and the `converged` signal
from the top of this document:

- **Exactly one valid block, ledger present and compatible, reconciliation clean** →
  `structured: true`, findings and verdict from the block, `open_count` numeric, `converged`
  computed as defined.
- **Any degradation** — no block, more than one candidate block, unparseable/invalid block, a
  fail-closed reconciliation condition, a ledger that is present-but-unreadable or at an
  incompatible version, or a turn whose ledger could not be persisted (Decision 5) → the
  envelope still returns, but with `structured: false`, `open_count: null`, `verdict` **never
  `approve`** (it is reported as `unknown`), `converged: false`, and a warning naming which
  degradation occurred. The prose is returned in full regardless, exactly as today.

The invariant round 1 demanded, made structural: **a numeric `open_count` and a `true`
`converged` are reachable only through the fully-validated path.** No degraded path can
produce a zero or a convergence, because both are computed from ledger state that the degraded
paths mark unavailable. An autonomous loop can trust `converged == true` to mean the reviewer
actually cleared everything it raised, because nothing in the pipeline can manufacture that
boolean without the structure behind it.

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
   - **The two marker namespaces are disjoint.** `CROSS_REVIEW_FINDINGS_IN` (with the per-review
     nonce) is what the *reviewer* emits and the extractor parses; `CROSS_REVIEW_ENVELOPE_OUT` is
     what the *server* emits into the text channel. "Exactly one block" is enforced per namespace:
     exactly one input block from the reviewer (or degrade), and exactly one output block from the
     server (always). A client parses only the `_OUT` block.

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
  "verdict": "approve" | "changes" | "unknown",   // "unknown" only when structured == false
  "verdict_source": "structured" | "prose" | "none",
  "verdict_detail": "approve" | "approve_with_comments" | "request_changes" | "blocked" | null,
  "open_count": 2,               // NUMBER only when structured+valid+clean; otherwise null
  "total_count": 5,              // findings ever raised on this session, any status; null when unstructured
  "findings": [
    {
      "id": "f3",
      "severity": "critical" | "major" | "minor",
      "status": "open" | "resolved" | "regressed",
      "title": "Race between refresh and revoke",
      "file": "src/auth/token.rs",
      "line": 129,               // optional; omitted when the finding is not line-scoped
      "detail": "<the reviewer's prose for this finding, as first captured>",
      "first_seen_turn": 1,
      "last_seen_turn": 3
    }
  ],
  "warnings": [ "reviewer marked verdict approve but 2 finding(s) are still open; treated as changes" ]
}
```

Notes on the shape:

- **`converged` is the loop signal.** `verdict` is retained as the human-facing two-value
  summary the issue named, but the *machine* stop condition is `converged`, which folds in
  structured-ness and ledger validity so a caller cannot accidentally stop on a bare zero.
- **`verdict_detail`** preserves the reviewer's richer four-level judgement (`APPROVE / APPROVE
  WITH COMMENTS / REQUEST CHANGES / BLOCKED`) so nuance is not lost — a reviewer can
  approve-with-comments and the loop still sees the `minor`s in the findings list. See the
  [verdict truth table](#verdict-truth-table) for the complete mapping.
- **`open_count` / `total_count` are `null`, not `0`, when unstructured.** This is the
  round-1 fix in the wire shape itself: absence is distinguishable from zero.
- **`detail` is the reviewer's prose, captured when the finding was first raised**, and not
  rewritten on later turns (Decision 2). Nothing is lost by adopting the structure: the block
  is a spine, the prose is the body, and the human rendering is unchanged.
- **`schema_version`** on the envelope, and a matching version on the persisted ledger, follow
  this project's existing discipline (the metrics log versions its records and skips-and-counts
  foreign ones rather than guessing).

### One schema for every successful response

Round 1 noted that `cross_model_review_result` also returns `status=running` snapshots, so an
advertised `outputSchema` must describe those too or it is a lie. The envelope therefore
carries `result_status` (`running` | `completed`) and the schema makes the findings/verdict
group **present-and-populated for `completed`, null/absent for `running`**, so a single
`outputSchema` validates every successful result the tool returns. Failure results
(`isError: true`) are out of scope for `outputSchema`, as they are today.

### Verdict truth table

Completing the mapping round 1 flagged as underspecified. `converged` is `true` only in the
one row that says so; every uncertain or blocked state is non-convergent by construction.

| reviewer `verdict_detail` | open findings after reconciliation | machine `verdict` | `converged` |
| --- | --- | --- | --- |
| `approve` | 0 | `approve` | **true** |
| `approve` | ≥1 (contradiction) | `changes` + warning | false |
| `approve_with_comments` | 0 | `approve` | false¹ |
| `approve_with_comments` | ≥1 | `changes` | false |
| `request_changes` | ≥1 | `changes` | false |
| `request_changes` | 0 (contradiction) | `changes` + warning | false² |
| `blocked` | any | `changes` | false |
| *unstructured (no valid block)* | unknown (`null`) | `unknown` | false |

¹ `approve_with_comments` with zero *open* findings means the reviewer left only informational
comments. Those are not open findings and do not block convergence *numerically*, but the
reviewer explicitly declined a clean `approve`, so the server honours that: `converged` stays
false until the reviewer itself returns a clean `approve`. Convergence follows the reviewer's
own top-level judgement, never overrides it toward "done."

² `request_changes` with zero open findings is a contradiction in the other direction (the
reviewer wants changes but named none as open). Fail safe: treat as `changes`, warn, do not
converge. Uncertainty never resolves to done.

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
  judgement, and that it must emit **exactly one** such block.
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
  assignment, status carry-over, unaddressed flagging, and the fail-closed conditions that
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

- **`src/session.rs`** — `SessionRecord` gains three optional, versioned fields: a
  `findings_ledger`, the durable `ledger_coverage` provenance
  ([whole-conversation rule](#convergence-requires-a-ledger-over-the-whole-conversation)), and a
  durable `pending_turn` intent marker (Decision 5). `TurnFacts` carries the reconciled ledger a
  completed turn produced; `record_turn` persists it — and clears the pending marker — under the
  existing exclusive-lock read-modify-write. `resume_block` is extended to refuse a resume while a
  pending marker is set.
- **`src/session.rs` — the store load itself becomes tri-state, which round 3 showed is the real
  fix, not a tolerant field.** Today `read()` (`src/session.rs:342`) maps *both* "file missing"
  and "file present but unparseable" to `StoreFile::default()` — an empty set — so a corrupt
  `sessions.json` silently makes every named session look absent, and the next call starts a
  fresh (and, worse, `whole_conversation`-convergeable) conversation under a name whose real
  history was lost. So the load distinguishes three states: **absent** (no file → genuinely empty,
  normal), **valid** (parsed), and **`invalid`** (file present, did not parse). An `invalid`
  store does **not** masquerade as absent: a `get(name)` against it returns an `Invalid` sentinel,
  not `None`, so `start_review` refuses a resume and any review it does run is stamped
  non-convergent (`non_convergence_reason: state_corrupt`) rather than silently starting a clean
  conversation. The corrupt file is never overwritten blindly; it is preserved for recovery and
  the operator is told. The tolerant-*field* idea still applies *within* a valid store — one bad
  ledger record degrades that one session, not the file — but it is the store-level tri-state that
  closes the "corruption looks like absence" hole. Which corruption cases are recoverable
  field-by-field versus which poison the whole store is documented explicitly rather than left to
  luck; a journal/per-session-file layout is noted as the more robust option if field-level
  recovery of arbitrary corruption is wanted later.
- **The worker** (in `src/reviewer/mod.rs` / `src/tools.rs`) gains, around the existing review
  call: **write the durable `pending_turn` marker before the reviewer is invoked** → load the
  prior ledger → hand it to the prompt builder → after the review text is collected, extract +
  reconcile into an envelope and a new ledger → **persist the new ledger and clear the marker in
  the one atomic replace before the result is delivered**. If that persist fails, degrade this
  turn's envelope (`structured: false`, `non_convergence_reason: turn_not_durable`); the marker
  stays set on disk, so the next call is refused a resume and must go `fresh` — see Decision 5.
- **`src/registry.rs`** — `Review`, `Outcome`, and `Snapshot` carry the `Envelope` so the
  result renderer can emit both channels. Computed once, when the turn finishes, travelling with
  the snapshot like `usage` and `warnings` already do.
- **`src/tools.rs`** — `render_completed` **strips the reviewer's `CROSS_REVIEW_FINDINGS_IN`
  block from the prose** it renders, then appends the server's canonical `CROSS_REVIEW_ENVELOPE_OUT`
  block after the existing prose and guidance; the result carries the envelope object. The two
  markers never collide, and the text channel holds exactly one `_OUT` block (major #3).
- **`src/mcp.rs`** — `text_result` (or a new `tool_result` beside it) attaches
  `structuredContent` alongside `content`; the dispatch path threads the envelope through. Tool
  definitions gain `outputSchema` for `cross_model_review_result`, covering running and
  completed per the single-schema rule above.

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
marker**, not a rendering flag:

- **A pending-turn marker is written durably into the session record *before* the reviewer is
  called**, while the lease is held, in the same atomic-replace discipline the record already
  uses. It records that a turn is in flight against this session's reviewer conversation. (This
  is new persisted state on `SessionRecord`, versioned like every other added field.)
- **On a clean turn**, the atomic replace that writes the new ledger and resume state also
  clears the marker — one write, so a reader never sees a new ledger beside a stale marker or
  vice versa.
- **On any crash, timeout, or persistence failure**, the marker survives on disk because it was
  written first and never cleared. The next call finds it. `resume_block` is extended to refuse
  a normal resume while a pending marker is present, returning `SESSION_NOT_RESUMABLE` and
  pointing the caller at `fresh: true` (or a new session name) — exactly the existing
  refuse-don't-silently-restart path this project already prefers. A `fresh: true` call ignores
  the stored record entirely (`start_review` sets `prior = None`), so it starts a new
  conversation with a new ledger and no stale ids; the poisoned record is replaced.
- **The reviewer turn and the ledger cannot silently diverge**, because the only way to a
  *clean* (marker-cleared) record is the atomic replace that also wrote the matching ledger; any
  path that advanced the reviewer conversation but did not reach that replace leaves the marker
  set, which blocks resume.

The residual honest limitation, unchanged and now genuinely the *only* one: a crash in the
narrow window after the reviewer accepted a turn and before the atomic replace loses that turn's
status updates, and the session must be restarted `fresh`. That is the safe direction — a lost
turn re-reviews from scratch rather than resuming on a ledger that disagrees with the reviewer —
and it costs one re-review, not a corrupted convergence. It is stated rather than papered over.

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
carries a `ledger_coverage` enum, written the first time a ledger is attached and thereafter
immutable for the life of that conversation:

- **`whole_conversation`** — the ledger was created on a genuine turn 1 (a new session or
  `fresh: true`). **Convergeable.** An empty `whole_conversation` ledger legitimately means "the
  reviewer raised nothing," which *can* converge — the provenance is what distinguishes it from
  the legacy-empty case below, which looks identical without it.
- **`legacy_uncovered`** — a ledger was first attached to a conversation that already had turns
  before this feature. **Never convergeable**, for the life of that conversation, even as the
  ledger fills up over later turns: those later findings are real but the *early* prose findings
  were never grounded, so the ledger can never be trusted as total. `converged: false`,
  `non_convergence_reason: ledger_unavailable`; the caller must go `fresh` to get a
  `whole_conversation` ledger. This flag is sticky — a legacy conversation cannot launder itself
  convergeable by accumulating turns.
- **`invalid`** — the ledger bytes were present but unreadable/incompatible this load (below).
  Non-convergent; session preserved; never reset to a fresh zero-open ledger.

A legacy resumed conversation is therefore recorded `legacy_uncovered` on its first ledgered
turn and stays there. The implementation may equivalently choose **not to persist a ledger for a
legacy conversation at all** and treat "resumed, no ledger, turns > 0" as permanently
non-convergent — either way the invariant is the same and must be tested across *several* resumed
turns after the first late-ledger turn, not just the first.

The rule in one line: **a session can converge only if its `ledger_coverage` is
`whole_conversation`** — a durable, persisted fact, not a per-turn inference from whether a
ledger happens to be present.

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

- The reconciler enforces a **maximum ledger size** (finding count and total serialized bytes of
  the injected digest). The cap is checked **before the reviewer is invoked**, so an over-budget
  session does not spend a billed turn only to degrade.
- Exceeding it yields `converged: false`, `non_convergence_reason: ledger_too_large`, and a
  message telling the operator the session has outgrown a single review conversation and should be
  split or restarted `fresh` with the still-open findings carried into new instructions. **No id
  is ever silently retired** to make room — that would be exactly the silent drop the whole design
  refuses.
- The same escalation discipline covers the other "trying again won't help" states, so an
  autonomous loop has a defined stopping point rather than spinning: **repeated `unstructured`**
  (a configurable small N of consecutive unstructured turns → escalate: the reviewer cannot
  produce a valid block), **`reviewer_withheld_approve`** (the reviewer keeps declining a clean
  approve), **`state_corrupt`**, **`ledger_too_large`**, and **`SESSION_NOT_RESUMABLE`** are all
  surfaced as human-escalation outcomes. The loop contract is therefore not just "while
  (!converged)" but "while (!converged) unless the reason is an escalation outcome," and the
  reason field is what makes that expressible without string-matching.

This is the honest counterpart to total-accounting: it is the safe choice for correctness, and
its cost (an unbounded id list) is bounded here rather than allowed to become a slow, silent
failure.

## What deliberately does not change

- **The prose.** Every existing line of the completed-review rendering stays. The envelope is
  additive on both channels. Anything currently reading the prose keeps working.
- **The single-call, single-dependency character.** No second model call, no new crate — the
  block is emitted in the same turn and parsed with `serde`.
- **The failure contract.** No new failure code. "Unstructured this turn" is a `structured:
  false`, non-convergent envelope with a warning, not a `REVIEWER_FAILED`.
- **The reviewer's isolation and read-only posture.** Nothing here touches the tool policy,
  the sandbox, or the capture; the reviewer emits more structured *output*, and its inputs and
  permissions are unchanged.

## Failure modes and how each is handled

Every uncertain signal degrades toward "still open / say so / not converged," never toward
"resolved / silently dropped / done."

| Situation | Handling |
| --- | --- |
| Reviewer emits no block | `structured:false`, `open_count:null`, `converged:false`, verdict `unknown` + warning; prose returned in full. |
| Reviewer emits more than one block | Fail-closed ambiguity → degrade as above; the server does not pick one. |
| Block present but invalid JSON / wrong schema / over size cap | Degrade as above; the block is not partially trusted. |
| `prior_findings` names an id the ledger never issued | Whole-turn degrade + warning; the reviewer cannot mint identity. |
| Same id twice in `prior_findings` | Whole-turn degrade + warning. |
| `new_findings` object carries a `status` field | Whole-turn degrade + warning; new findings are server-forced `open`. |
| Reviewer says `approve` but ledger has open findings | Machine verdict `changes`, `converged:false`, warning names the disagreement. |
| Reviewer says `request_changes`/`blocked` with zero open | Machine verdict `changes`, `converged:false`; uncertainty never converges. |
| Prior id the reviewer failed to account for | Whole-turn degrade (`non_convergence_reason: unstructured`); every prior id must appear exactly once — silence on any id is never an implied status. |
| Regression of a previously resolved finding | Reviewer sends `f1: regressed` (resolved findings are in the injected digest, which lists every prior id); `f1` reopens, no new id. |
| One ledger *record* incompatible/undeserializable, store otherwise valid | Field-level tolerant load → that session non-convergent (`ledger_unavailable`); other sessions unaffected; **not** reset to a fresh zero-open ledger. |
| The whole `sessions.json` store fails to parse | Tri-state load returns `invalid`, **not** empty: resume refused, any review non-convergent (`state_corrupt`), corrupt file preserved, operator told — never silently a clean fresh conversation. |
| New session, or `fresh: true` | `ledger_coverage = whole_conversation`, covers the conversation from turn 1 → convergeable normally (even with zero findings). |
| Resumed conversation predating the ledger | `ledger_coverage = legacy_uncovered` (sticky, durable): reviewed but non-convergent (`ledger_unavailable`) for the life of the conversation; caller must go `fresh`. Later turns cannot launder it convergeable. |
| Turn's ledger could not be persisted | Envelope degrades (`turn_not_durable`); durable pending marker stays set, so the next call is refused a resume and must go `fresh` (Decision 5). |
| Ledger/digest exceeds the bounded budget | Checked **before** the reviewer runs → `converged:false`, `ledger_too_large`, human-escalation outcome; **no id silently retired** ([bounded growth](#bounded-growth-and-escalation-outcomes)). |
| Degraded turn mid-session | Prior ledger preserved untouched; that turn contributes no status updates, says so, and cannot converge. |

The distinction is durable and three-way, keyed on the persisted `ledger_coverage`, not inferred
per turn: **`whole_conversation`** (ledger from turn 1 → convergeable) ≠ **`legacy_uncovered`**
(ungrounded prose history → non-convergent, sticky, must go `fresh`) ≠ **`invalid`**
(present-but-unreadable ledger → non-convergent, session preserved, never reset). Separately, the
*store* load is tri-state (`absent`/`valid`/`invalid`) so file-level corruption cannot pose as
absence. Only `whole_conversation` can reach `converged`.

## Tests

- **`src/findings.rs` unit tests** — extraction (one clean block bearing the review nonce; a
  block among other fences and quoted examples; a block with a *stale/foreign* nonce → not
  matched; zero blocks; two blocks → degrade; unterminated block; over-cap field);
  reconciliation (turn-1 assignment; resolve; still-open; regressed-from-resolved; new finding
  mid-session; **a prior id the reviewer omitted → whole-turn degrade**; unknown-id → degrade;
  duplicate-id → degrade; `new_findings` with a status → degrade; monotonic non-reuse of ids
  across many turns); convergence + verdict (every row of the truth table; approve-with-open
  contradiction → `verdict_contradiction`; request-changes-with-none → `verdict_contradiction`;
  approve-with-comments + zero open → `reviewer_withheld_approve`, not converged; unstructured →
  `converged:false`, `open_count:null`, `non_convergence_reason:unstructured`).
- **`src/session.rs`** — ledger round-trips through persistence; a genuinely fresh turn-1
  converges (including with zero findings); a **resumed pre-ledger conversation is stamped
  `legacy_uncovered` and stays non-convergent across *several* later turns** even as its ledger
  fills (round 3 #1), not just on the first late-ledger turn; a corrupt-ledger *record* degrades
  only its own session; **a corrupt whole store loads `invalid`, refuses resume, and never poses
  as an empty/fresh session** (round 3 #4); an incompatible version is not misread; a
  **pending-turn marker blocks resume** and a `fresh` call clears it.
- **`src/prompt.rs`** — the **machine-block contract renders on turn 1 *and* every resumed turn**
  with the current nonce (round 3 #2), not only in the turn-1 preamble; the resumed prompt lists
  **all** prior findings (open and resolved) with ids as quoted evidence and the "every id exactly
  once" instruction; a fresh prompt lists none.
- **`src/mcp.rs` / `src/tools.rs`** — a completed result carries both `structuredContent` and the
  server `_OUT` text block, and validates against `outputSchema`; **the reviewer's `_IN` block is
  stripped from the rendered prose and the text channel holds exactly one `_OUT` block** (round 3
  #3); a running result validates against the same schema; a degraded review carries a well-formed
  envelope with `structured:false`, `open_count:null`, `converged:false`.
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

## Open questions for the reviewer

1. **Bounded-growth thresholds.** The design bounds the ledger/digest and escalates rather than
   silently retiring ids. What is a sane *default* cap (finding count / digest bytes) before
   escalation, given real sessions here have reached 13 rounds? Should it be a CLI knob
   (`--max-findings` / a digest-byte budget) with a conservative default, and should it ever be
   disableable given that disabling it reinstates the slow-failure mode?
2. **`legacy_uncovered` migration ergonomics.** A pre-feature session in the act-on-feedback loop
   becomes permanently non-convergent until `fresh`, which discards the reviewer's accumulated
   context. Is "go `fresh`, carry open findings into new instructions" an acceptable one-time
   migration cost, or is a one-shot re-baseline (ask the reviewer to enumerate its current
   findings once, seed the ledger, then stamp `whole_conversation`) worth the added complexity and
   the risk that the re-baseline itself is trusted without a turn-1 anchor?
3. **Per-review nonce strength.** Ids are `rv-<pid>-<counter>` and therefore guessable. For the
   threat it defends — a *static* embedded lookalike — unguessability seems unnecessary, but is
   there a reason to prefer a random per-review token anyway (defence against an active injection
   that predicts rather than reads the nonce)?
