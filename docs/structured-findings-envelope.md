# Structured findings + machine-readable verdict — design

Status: **planned.** This document is the plan. It goes through this repository's own
`cross-review` gate before implementation begins, and the implementation goes through it again.

Tracks issue #40. The issue asks for a machine-readable envelope so that an
autonomous "re-review until converged" loop stops re-deriving structure the tool could
provide: a top-level verdict, an `open_count`, stable finding IDs carried across a resumed
session, and per-finding status (resolved / open / regressed) on a re-review.

> **Review history.** Round 1 (Codex, gpt-5.6-luna, effort=max) returned REQUEST CHANGES with
> seven `major` findings. All seven are addressed below and the changes are recorded in
> [Round 1 response](#round-1-response); the sections they touched — the block schema, ID
> reconciliation, degradation, extraction, idempotency, ledger loading, and the verdict truth
> table — were rewritten rather than patched. This document reflects the post-round-1 design.

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
told to stop on. `converged == true` requires **all** of:

- the reviewer emitted exactly one valid, well-formed machine block this turn
  (`structured == true`), and
- the session's finding ledger is present and readable at a compatible version, and
- reconciliation completed without a fail-closed condition (no unknown/duplicate IDs), and
- every finding in the ledger has `status == resolved`, so `open_count == 0`, and
- the reviewer's own verdict is `approve` (it agrees nothing is open).

If any one is false, `converged` is `false`. `open_count` is a number **only** when the first
three hold; otherwise it is `null`, never `0`. A caller loops `while (!converged)`; it never
has to reason about *why* a turn was not convergent to stay safe, only to report it.

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

- **Each `prior_findings` entry** must name an id the ledger owns. Its status is applied to
  that ledger entry. The ledger entry's content is untouched.
- **A prior ledger finding the reviewer did not mention** stays at its current status and, if
  it was open, is flagged `unaddressed_this_turn` — never auto-resolved. Silence is not
  resolution.
- **`new_findings`** get fresh monotonic ids, `status: open`, and are appended.
- **Fail-closed conditions that degrade the *entire* turn** (→ `structured: false`, see
  Decision 3): a `prior_findings` id the ledger never issued; the same id appearing twice in
  `prior_findings`; a `prior_findings` object carrying any field other than `id`/`status`; a
  `new_findings` object carrying a `status`; any enum out of range. The block is trusted as a
  whole or not at all — a block the server had to *partially* discard is a block it cannot
  reason about, and round 1 was right that "ignore the bad entry with a warning" is exactly
  how a serious finding silently leaves `open_count`.

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
kept consistent by the server, deterministically.** In the structured path, `converged` (and
thus the two-value `verdict`) is defined as `open_count == 0`. If the reviewer's block claims
`verdict: approve` while the ledger still has open findings, that is a contradiction; the
server resolves it in favour of the ledger (machine verdict `changes`, `converged: false`) and
warns, naming the disagreement. A loop cannot be talked into stopping by a reviewer that wrote
`approve` at the top and left a `major` open below.

### Decision 4 — extraction is fail-closed on ambiguity, and the ledger is injected as quoted evidence

Round 1 was right that "a fenced ```json block" is underspecified: a review can contain
several JSON fences, quoted examples, or a sentinel copied out of the repository under review.
So:

- The block is delimited by **exact begin/end sentinel lines** that are unlikely to occur in
  ordinary prose or code, on their own lines — not by a bare ```` ```json ```` fence.
- Extraction requires **exactly one** complete, well-formed sentinel block. **Zero → degrade**
  (no structure this turn). **More than one → degrade** (fail-closed on ambiguity; the server
  does not pick "the first" or "the last"). A block that opens and never closes → degrade.
- The block body has a **strict size cap** and per-field length caps; over-cap → degrade. This
  is defence against a block that is technically parseable but pathological.
- The prior-findings digest the server injects on a resumed turn is rendered as **quoted
  evidence, not instructions**, exactly as the change capture and CLAUDE.md already are
  (README's "labelled as evidence, not instructions"). It carries ids, titles, locations and
  statuses — never anything that reads as a directive to the reviewer.

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
2. **A block appended to the existing text content**, delimited by the same sentinel markers,
   so a client that only reads `content[].text` still gets the machine envelope without a
   second round trip. The human prose (`--- BEGIN REVIEW --- … --- END REVIEW ---`) and the
   existing status/usage/warning header stay exactly as they are, above it.

```jsonc
{
  "schema_version": 1,
  "session": "auth-refactor",
  "turn": 3,
  "result_status": "completed",  // present so one schema covers running/completed (see below)
  "structured": true,            // false on any degradation (Decision 3)
  "converged": false,            // the ONLY safe loop-termination signal
  "verdict": "approve" | "changes" | "unknown",   // "unknown" only when structured == false
  "verdict_source": "structured" | "prose" | "none",
  "verdict_detail": "approve" | "approve_with_comments" | "request_changes" | "blocked" | null,
  "open_count": 2,               // NUMBER only when structured; otherwise null
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
      "last_seen_turn": 3,
      "unaddressed_this_turn": false
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

The preamble gains a section defining the machine block: its exact begin/end sentinel markers,
the two-array schema (`prior_findings` status-only, `new_findings` content-only-no-status), a
statement that it is **in addition to** the prose (not a replacement), that severity/status
must be its own honest judgement, and that it must emit **exactly one** such block.

The follow-up guidance (`FOLLOW_UP_GUIDANCE`, rendered on resumed turns) changes from "state
which previous findings are resolved" to the ID-keyed contract: the resumed prompt lists **all
prior findings, open and resolved, with their ids**, as quoted evidence, and asks the reviewer
to report a status for each id it addresses (via `prior_findings`) and to emit anything new via
`new_findings`. This is a new `PromptParts` input — the prior-findings digest — rendered only
on resume, after the change and before the follow-up instruction, in the slot the
resumed-capture note already establishes as "context about the prior turn."

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

- **`src/session.rs`** — `SessionRecord` gains an optional, versioned `findings_ledger`. Round
  1 flagged that the store loads by whole-file deserialization (`src/session.rs:342`), so a
  malformed ledger must not erase the session: the field deserializes tolerantly (into a
  raw/optional value that degrades to "ledger unreadable" rather than failing the record), so a
  bad ledger becomes a field-level *degradation of that session's convergence*, not a lost
  session. `TurnFacts` carries the reconciled ledger a completed turn produced; `record_turn`
  persists it under the existing exclusive-lock read-modify-write.
- **The worker** (in `src/reviewer/mod.rs` / `src/tools.rs`) gains, around the existing review
  call: load the prior ledger → hand it to the prompt builder → after the review text is
  collected, extract + reconcile into an envelope and a new ledger → **persist the new ledger
  before the result is delivered**, and if that persist fails, degrade this turn's envelope
  (`structured: false`, non-convergent) and mark the session non-resumable — see Decision 5.
- **`src/registry.rs`** — `Review`, `Outcome`, and `Snapshot` carry the `Envelope` so the
  result renderer can emit both channels. Computed once, when the turn finishes, travelling with
  the snapshot like `usage` and `warnings` already do.
- **`src/tools.rs`** — `render_completed` appends the sentinel-delimited envelope block after
  the existing prose and guidance; the result carries the envelope object.
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

This project already has the mechanism to lean on: a turn whose session state could not be
persisted is reported as a warning and **the response stops inviting a resume** (README, "A
session that could not be persisted is reported as a warning with the review, and the response
then stops inviting a resume that would silently start over"). The ledger is persisted as part
of that same session record under the same lock, so it inherits that behaviour, and the design
tightens it for findings specifically:

- The ledger write is part of the **same atomic session-record replace** as the existing
  resume state, so the two never diverge on disk: either both this turn's resume state and its
  ledger land, or neither does.
- **If persistence fails after the reviewer turn**, the turn's envelope degrades
  (`structured: false`, `open_count: null`, `converged: false`, warning), and the session is
  marked non-resumable — so the next call is a fresh session with a fresh ledger rather than a
  resume that reuses ids the remote conversation has moved past. No stale-id retry is possible
  because no resume is offered.
- **A resume is admitted only when the local ledger and the reviewer session agree** on turn
  count / identity, which the existing resume-guard (`SESSION_NOT_RESUMABLE` on a turn/idle/
  identity mismatch) already enforces at the session level; the ledger travels inside that same
  guarded record, so a resume that passes the guard has a ledger consistent with the turn being
  resumed.

The residual honest limitation: a crash in the narrow window *after* the reviewer accepted the
turn and *before* the atomic replace commits loses this turn's status updates, and the session
falls back to fresh. That is the safe direction — a lost turn re-reviews from scratch rather
than resuming on a ledger that disagrees with the reviewer — and it is stated rather than
papered over.

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
| Prior open finding the reviewer never mentions | Stays `open`, flagged `unaddressed_this_turn`; never auto-resolved. |
| Regression of a previously resolved finding | Reviewer sends `f1: regressed` (resolved findings are in the injected digest); `f1` reopens, no new id. |
| Ledger persisted by an incompatible schema version | Treated as unreadable → non-convergent + warning; **not** reset to a fresh zero-open ledger on an existing session. |
| Ledger bytes corrupt / undeserializable | Field-level tolerant load → session survives, but that session is non-convergent + warning until a fresh session is started. |
| Session recorded before ledgers existed | No ledger ever ≠ corrupt ledger: reconciles as a genuine fresh turn-1; no error, and it can converge normally. |
| Ledger could not be persisted this turn | Envelope degrades, session marked non-resumable (Decision 5); no stale-id retry. |
| Degraded turn mid-session | Prior ledger preserved untouched; that turn contributes no status updates, says so, and cannot converge. |

The distinction round 1 sharpened: **"this session never had a ledger" (pre-feature, safe to
treat as fresh and convergeable) is not the same as "this session's ledger is now unreadable"
(unsafe — block convergence, do not reset).** The loader tells them apart; only the first can
reach `converged`.

## Tests

- **`src/findings.rs` unit tests** — extraction (one clean block; block among other fences and
  quoted examples; zero blocks; two blocks → degrade; unterminated block; over-cap field);
  reconciliation (turn-1 assignment; resolve; still-open; regressed-from-resolved; new finding
  mid-session; unaddressed prior finding; unknown-id → whole-turn degrade; duplicate-id →
  degrade; `new_findings` with a status → degrade; monotonic non-reuse of ids across many
  turns); convergence + verdict (every row of the truth table; approve-with-open contradiction;
  request-changes-with-none contradiction; unstructured → `converged:false`, `open_count:null`).
- **`src/session.rs`** — ledger round-trips through persistence; a pre-ledger record loads and
  can converge; a corrupt-ledger record loads *without erasing the session* and is
  non-convergent; an incompatible version is not misread.
- **`src/prompt.rs`** — the resumed prompt lists prior findings (open and resolved) with ids as
  quoted evidence; a fresh prompt does not; the block-format instruction (exact markers,
  two-array split) is present on turn 1.
- **`src/mcp.rs` / `src/tools.rs`** — a completed result carries both `structuredContent` and
  the sentinel text block, and validates against `outputSchema`; a running result validates
  against the same schema; a degraded review carries a well-formed envelope with
  `structured:false`, `open_count:null`, `converged:false`.
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

## Open questions for the reviewer

1. **Convergence on `approve_with_comments` with zero open findings.** The truth table keeps
   this non-convergent (footnote 1): the reviewer declined a clean `approve`, so the loop keeps
   going until it gets one. The alternative is to converge on zero open findings regardless of
   the reviewer's four-level nuance. Is deferring to the reviewer's own top-level judgement the
   right call, or should numeric zero win?
2. **Should a prior finding the reviewer ignores for K consecutive turns escalate** (e.g. a
   distinct warning, or forcing attention) so it cannot be quietly waited out across many
   rounds, or is flag-and-keep-open per turn sufficient?
3. **Is the content hash still worth adding** now that the reviewer no longer resupplies prior
   findings' content at all? With content server-owned and immutable, the paraphrase/retarget
   vector is closed structurally; a hash would only detect the server's own rendering drift. Is
   there a remaining case it earns its complexity for?
4. **Sentinel design.** Is an exact begin/end marker pair plus exactly-one-block plus size caps
   sufficient against a repository that embeds a lookalike sentinel in its own source (which the
   Codex reviewer can read), or is a per-review nonce in the marker warranted?
