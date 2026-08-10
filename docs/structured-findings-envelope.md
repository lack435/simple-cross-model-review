# Structured findings + machine-readable verdict — design

Status: **planned.** This document is the plan. It goes through this repository's own
`cross-review` gate before implementation begins, and the implementation goes through it again.

Tracks issue #40. The issue asks for a machine-readable envelope so that an
autonomous "re-review until converged" loop stops re-deriving structure the tool could
provide: a top-level verdict, an `open_count`, stable finding IDs carried across a resumed
session, and per-finding status (resolved / open / regressed) on a re-review.

> **Review history.** Round 1 (Codex, gpt-5.6-luna, effort=max) returned REQUEST CHANGES with
> seven `major` findings; round 2 returned REQUEST CHANGES with four `major` + one `minor`,
> confirming five round-1 items resolved. Both rounds are recorded in
> [Round 1 response](#round-1-response) and [Round 2 response](#round-2-response); every finding
> was accepted, none disputed. The sections they touched — the block schema, ID reconciliation
> (now total-accounting), degradation and the `converged` signal, extraction (now nonce-marked),
> durability (now a write-ahead marker), the whole-conversation ledger rule, and the verdict
> truth table — were rewritten rather than patched. This document reflects the post-round-2
> design.

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

- **A ledger that covers the whole reviewer conversation.** The session either began its
  reviewer conversation on this feature (a ledger exists from its own turn 1) or is a genuinely
  fresh turn 1 now. A conversation that predates the ledger is explicitly *not* convergeable —
  see [the pre-feature rule](#convergence-requires-a-ledger-over-the-whole-conversation).
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
| `unstructured` | No valid block this turn. | Re-review; do not read absence as convergence. |
| `ledger_unavailable` | Ledger unreadable, incompatible, or the conversation predates the ledger. | Start a `fresh` session to establish a convergeable ledger. |
| `turn_not_durable` | This turn's ledger could not be persisted. | Start a `fresh` session; do not resume (stale-id risk). |

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

- The block is delimited by **exact begin/end sentinel lines carrying a per-review nonce**, on
  their own lines — not by a bare ```` ```json ```` fence, and not by a fixed marker either. The
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

- **`src/session.rs`** — `SessionRecord` gains two optional, versioned fields: a
  `findings_ledger`, and a durable `pending_turn` intent marker (Decision 5). Round 1 flagged
  that the store loads by whole-file deserialization (`src/session.rs:342`), so a malformed
  ledger must not erase the session: the ledger field deserializes tolerantly (into a
  raw/optional value that degrades to "ledger unreadable" rather than failing the record), so a
  bad ledger becomes a field-level *degradation of that session's convergence*, not a lost
  session. `TurnFacts` carries the reconciled ledger a completed turn produced; `record_turn`
  persists it — and clears the pending marker — under the existing exclusive-lock read-modify-
  write. `resume_block` is extended to refuse a resume while a pending marker is set.
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

So convergence is gated on the ledger covering the **whole** conversation, and the three "no
current ledger" cases are *not* equivalent:

- **Genuinely fresh turn 1** (new session, or `fresh: true`) — the ledger is created now and
  covers the conversation from its first turn. **Convergeable.**
- **Resumed session whose reviewer conversation predates the ledger** (turns > 0, no ledger on
  record) — the conversation carries ungrounded prose findings the ledger does not know.
  **Not convergeable.** The envelope reports normally but with `converged: false`,
  `non_convergence_reason: ledger_unavailable`; the caller is told to start `fresh` to establish
  a ledger that covers the whole conversation from turn 1. It is never silently treated as a
  clean turn-1.
- **Resumed session whose ledger is present but unreadable/incompatible** — same outcome,
  `non_convergence_reason: ledger_unavailable`, session preserved (the record is not erased, per
  the tolerant load below), never reset to a fresh zero-open ledger.

The rule in one line: **a session can converge only if a ledger has covered it since its own
turn 1.** Everything else can still be reviewed, and still gets an envelope, but cannot report
`converged: true` until a fresh conversation gives it a whole-conversation ledger.

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
| Ledger persisted by an incompatible schema version | Treated as unreadable → non-convergent (`ledger_unavailable`); **not** reset to a fresh zero-open ledger on an existing session. |
| Ledger bytes corrupt / undeserializable | Field-level tolerant load → session survives, but is non-convergent (`ledger_unavailable`) until a fresh session is started. |
| New session, or `fresh: true` | Ledger created now, covers the conversation from turn 1 → convergeable normally. |
| Resumed session whose conversation predates the ledger (no ledger on record, turns > 0) | Reviewed, but non-convergent (`ledger_unavailable`): old prose findings have no ids and could be silently omitted. Caller must go `fresh` to gain a whole-conversation ledger. |
| Turn's ledger could not be persisted | Envelope degrades (`turn_not_durable`); durable pending marker stays set, so the next call is refused a resume and must go `fresh` (Decision 5). |
| Degraded turn mid-session | Prior ledger preserved untouched; that turn contributes no status updates, says so, and cannot converge. |

The distinction round 2 sharpened, and it is three-way, not two: **a genuinely fresh turn 1**
(ledger from turn 1 → convergeable) is not the same as **a resumed conversation that predates the
ledger** (ungrounded prose history → non-convergent, must go `fresh`), which is not the same as
**a present-but-unreadable ledger** (corrupt/incompatible → non-convergent, session preserved,
never reset). The loader tells all three apart; only the first can reach `converged`.

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
  converges; a **resumed pre-ledger conversation is non-convergent** (`ledger_unavailable`), not
  treated as a clean turn-1; a corrupt-ledger record loads *without erasing the session* and is
  non-convergent; an incompatible version is not misread; a **pending-turn marker blocks resume**
  and a `fresh` call clears it.
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

## Open questions for the reviewer

1. **Is requiring the reviewer to re-state a status for *every* prior id on every resumed turn
   the right cost trade?** It is the fix for the round-2 omission hole and it is cheap per entry
   (`{id, status}` pairs), but on a long session the id list grows unbounded across turns. Is
   total-accounting-or-degrade correct, or should resolved findings eventually be retired from
   the required set (and if so, by what rule that does not reopen the regression hole)?
2. **Does the `reviewer_withheld_approve` reason fully close the livelock**, or should there be
   an explicit turn-count ceiling after which the loop is told to stop regardless, so an
   autonomous caller cannot spin indefinitely against a reviewer that never returns a clean
   `approve`?
3. **Is the per-review nonce derived from the review id strong enough**, given ids are
   `rv-<pid>-<counter>` and therefore guessable, or should the nonce be a random token generated
   per review (at the cost of one more thing to thread through and log)? The threat it defends —
   a *static* embedded lookalike — does not obviously need unguessability, but I want your read.
