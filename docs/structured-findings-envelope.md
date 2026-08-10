# Structured findings + machine-readable verdict — design

Status: **planned.** This document is the plan. It goes through this repository's own
`cross-review` gate before implementation begins, and the implementation goes through it again.

Tracks issue #40. The issue asks for a machine-readable envelope so that an
autonomous "re-review until converged" loop stops re-deriving structure the tool could
provide: a top-level verdict, an `open_count`, stable finding IDs carried across a resumed
session, and per-finding status (resolved / open / regressed) on a re-review.

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
search.

## What "done" means (from the issue's acceptance)

1. An agent can loop "re-review until `open_count == 0`" **without string-matching the body**.
2. A resumed review reports **each prior finding's status by stable ID**.

Everything below serves those two sentences. Where a choice is open, the tie-breaker is
*foundational completeness and robustness* over minimising blast radius — the explicit
priority for this work.

## The three hard parts

Structure is easy to ask for and hard to make trustworthy. Three problems have to be solved
together, or the envelope is a liability rather than a convenience:

1. **Where does the structure come from?** The reviewer is a model emitting text. Either the
   server parses its prose into structure, or the reviewer emits the structure directly.
2. **Who owns finding identity, and how does it stay stable across rounds?** A stable ID is
   only stable if *something* remembers it between turns and matches this turn's findings to
   last turn's.
3. **What happens when the structure is missing or malformed?** A review that produced good
   prose but no valid machine block must still come back as a review, not an error — but the
   loop must be able to tell it apart from a clean structured one.

### Decision 1 — the reviewer emits the structure; the server does not parse prose

The reviewer is instructed to emit, in addition to its prose, a single fenced machine block
containing its findings and verdict in a defined schema. The server locates and validates
that block. It does **not** reverse-engineer structure from the `## Findings` markdown.

Why not parse the prose:

- Two different models (Codex reviewing Claude, Claude reviewing Codex) format prose
  differently, and both are free to reword their own headings. A parser tuned to one drifts
  against the other and against the next model version. This project pins models by full id
  precisely because model behaviour moves; a prose parser would re-introduce exactly that
  fragility one layer up.
- Severity and status are *judgements*, and the model is the authority on them. Asking it to
  state `"severity": "major"` in a field is more faithful than inferring "major" from where a
  sentence sat in a bulleted list.
- A fenced block with a sentinel key is trivial to locate and cheap to validate; the prose
  stays untouched as the human-readable rendering and as each finding's `detail`.

Why not a *second* model call to extract structure from the prose: this project's whole
character is one call, one response, `serde` as the only dependency. A structuring pass would
double the model cost of every review and add a second failure surface. The reviewer emits
the block in the same turn it writes the prose.

### Decision 2 — the server owns finding IDs and status reconciliation, keyed by ID, not by fuzzy match

This is the core of the design and the part most likely to be got wrong.

The server keeps a **per-session findings ledger**, persisted in the session record next to
the existing resume state. Each ledger entry is a finding with a **server-assigned stable
id** (`f1`, `f2`, … monotonic per session, never reused), its last-known severity / title /
location, its current status, and the turns it was first and last seen on.

**Turn 1 (fresh session).** The reviewer's block carries findings with *no* IDs — it cannot
know them. The server assigns `f1…fn` in block order, records them all in the ledger with the
reviewer's stated per-finding status (defaulting to `open`), and returns them.

**Turn N (resumed session).** The server already holds the ledger. Before the review, it
injects the prior **non-closed** findings — *with their IDs, titles, and locations* — into
the prompt, and instructs the reviewer to, for each listed ID, report `resolved`, `open`, or
`regressed`, and separately to report any **new** findings (which it emits without IDs). After
the review, the server reconciles:

- **Prior findings** keyed by the ID the reviewer echoed back: status updated to what the
  reviewer reported for that ID.
- **A prior open finding the reviewer did not mention at all** stays `open` and is flagged
  `unaddressed_this_turn` — never silently dropped. Silence is not resolution; treating it as
  resolution is the one failure mode that could let the loop merge an unfixed bug.
- **New findings** (no ID in the block) get fresh monotonic IDs and are appended.
- **An ID the reviewer cites that the ledger never issued** is ignored with a warning — the
  reviewer does not get to mint IDs, only to report status on ones the server owns.

Reconciliation is therefore **keyed by explicit server-owned IDs the reviewer is handed and
hands back**, not by fuzzy text matching of one turn's titles against another's. There is no
"is this the same finding as last time?" heuristic to be wrong. The model, which raised the
concern, is the authority on whether it is resolved; the server, which owns the counter, is
the authority on identity. Neither guesses at the other's job.

This also means the reviewer is *told* its earlier findings and their IDs on every resume,
which the prompt already gestures at ("state which previous findings are resolved") but
without stable handles. Giving it the IDs is what lets the caller stop maintaining the map by
hand — the whole point of the issue.

### Decision 3 — the envelope always exists; it degrades, it never fails the review

A review that produced usable prose must never be turned into a tool error because its
machine block was absent or malformed. The failure codes in this project are reserved for the
reviewer being *unavailable* (`NOT_AUTHENTICATED`, `RATE_LIMITED`, `TIMEOUT`, …); "the review
happened but was unstructured" is not that.

So the envelope is always returned, with a `structured` boolean:

- **Valid block present** → `structured: true`, findings and verdict from the block, verdict
  cross-checked against `open_count` (below).
- **No block, or an unparseable one** → `structured: false`, `findings: []`, verdict inferred
  best-effort from the prose `## Verdict` line (`verdict_source: "prose"`), and a warning
  telling the caller the per-finding structure is unavailable this turn. The prose is returned
  in full regardless, exactly as today. An autonomous loop can still read a verdict; it just
  cannot rely on per-finding status for that turn, and it is told so rather than being handed
  an empty findings list that reads like "nothing wrong."

Robustness rule that makes the loop *safe* rather than merely convenient: **`verdict` and
`open_count` are kept consistent by the server, deterministically.** `open_count == 0` iff
`verdict == "approve"`. If the reviewer's block says `approve` but lists findings still
`open`, that is a contradiction; the server resolves it in favour of `open_count` (the machine
verdict becomes `changes`) and emits a warning naming the disagreement. A loop that terminates
on `open_count == 0` then cannot be talked into merging by a reviewer that wrote "APPROVE" at
the top and "major: still broken" three lines down.

## The envelope

Delivered on **two channels at once**, because the client population is mixed and the change
must not break the prose channel anything already depends on:

1. **`structuredContent`** on the MCP tool result. The negotiated protocol here is
   `2025-06-18`, which defines `structuredContent`; clients that ignore it are unaffected, and
   the tool definition gains an `outputSchema` describing the envelope.
2. **A fenced `json` block appended to the existing text content**, so a client that only
   reads `content[].text` still gets the machine envelope without a second round trip. The
   human prose (`--- BEGIN REVIEW --- … --- END REVIEW ---`) and the existing status/usage/
   warning header stay exactly as they are, above it.

```jsonc
{
  "schema_version": 1,
  "session": "auth-refactor",
  "turn": 3,
  "structured": true,            // false when no valid reviewer block was found this turn
  "verdict": "approve" | "changes",
  "verdict_source": "structured" | "prose" | "inferred",
  "verdict_detail": "approve" | "approve_with_comments" | "request_changes" | "blocked",
  "open_count": 2,               // findings whose status is open or regressed
  "total_count": 5,              // all findings ever raised on this session, any status
  "findings": [
    {
      "id": "f3",
      "severity": "critical" | "major" | "minor",
      "status": "open" | "resolved" | "regressed",
      "title": "Race between refresh and revoke",
      "file": "src/auth/token.rs",
      "line": 129,               // optional; omitted when the finding is not line-scoped
      "detail": "<the reviewer's prose for this finding>",
      "first_seen_turn": 1,
      "last_seen_turn": 3,
      "unaddressed_this_turn": false  // true if a prior-open finding the reviewer didn't mention
    }
  ],
  "warnings": [ "reviewer marked verdict approve but 2 finding(s) are still open; treated as changes" ]
}
```

Notes on the shape:

- **`verdict` is the two-value loop signal** the issue asked for (`approve` / `changes`), and
  it is defined as `open_count == 0`. **`verdict_detail`** preserves the reviewer's richer
  four-level judgement (the existing `APPROVE / APPROVE WITH COMMENTS / REQUEST CHANGES /
  BLOCKED`) so nuance is not lost — a reviewer can approve-with-comments and the loop still
  sees `approve` with a non-empty findings list of `minor`s.
- **`total_count` vs `open_count`.** The loop terminates on `open_count`; `total_count` lets a
  caller show "2 of 5 still open" without recomputing. Resolved findings stay in the ledger and
  in the envelope (with `status: resolved`) rather than vanishing, so the caller can see the
  arc of the session, and a *regression* has a prior entry to point at.
- **`schema_version`** on the envelope, and a matching version on the persisted ledger, follow
  this project's existing discipline (the metrics log versions its records and skips-and-counts
  foreign ones rather than guessing). A ledger written by an incompatible version is treated as
  absent — the session reconciles as if fresh, with a warning — never misread.
- **`detail` is the prose**, so nothing is lost by adopting the structure: the block is a
  spine, the prose is the body, and the human rendering is unchanged.

## Prompt changes (`src/prompt.rs`)

The preamble gains a section defining the machine block: its sentinel, its schema, that it is
**in addition to** the prose (not a replacement), and that severity/status must be its own
honest judgement. The block is specified as a fenced ```` ```json ```` region containing a
single object with a sentinel key so the extractor can locate it unambiguously even if the
model also emits other code fences.

The follow-up guidance (`FOLLOW_UP_GUIDANCE`, rendered on resumed turns) changes from "state
which previous findings are resolved" to the ID-keyed contract: the resumed prompt lists the
prior open findings *with their IDs*, and asks the reviewer to report a status for each listed
ID and to emit new findings without IDs. This is a new `PromptParts` input — the prior-findings
digest — rendered only on resume, after the change and before the follow-up instruction, in the
slot the resumed-capture note already establishes as "context about the prior turn."

The block format is documented to the reviewer as evidence-producing output, and the extractor
treats the reviewer's block as *trusted-ish model output* but still validates defensively
(size caps, enum validation, no unbounded strings) — the reviewer is more trusted than the
repository under review, but "more trusted" is not "unvalidated."

## New module — `src/findings.rs`

Pure logic, no I/O, exhaustively unit-tested, in the spirit of `src/digest.rs`:

- The types: `Verdict`, `Severity`, `Status`, `Finding`, `Envelope`, `LedgerEntry`, `Ledger`.
- **Extraction**: locate and parse the reviewer's fenced block out of the raw review text;
  tolerant of surrounding prose and of the model wrapping it in ```` ```json ````; returns
  `None` (→ degraded envelope) rather than erroring on anything malformed.
- **Reconciliation**: `(prior ledger, this turn's parsed block, turn number) → (new ledger,
  envelope)`, implementing Decision 2 exactly — ID assignment, status carry-over, unaddressed
  flagging, unknown-ID rejection, new-ID minting.
- **Verdict resolution**: `(reviewer verdict, open_count) → (machine verdict, warnings)`,
  implementing Decision 3's consistency rule.
- **Rendering**: `Envelope → serde_json::Value` for `structuredContent`, and `Envelope →
  String` for the fenced text block.

Keeping all of this in one pure module means the entire ID/status/verdict contract is testable
without a model, a network, or a filesystem — which is where the correctness risk actually
lives.

## Wiring (the I/O edges)

- **`src/session.rs`** — `SessionRecord` gains an optional, versioned `findings_ledger`.
  `TurnFacts` carries the reconciled ledger a completed turn produced; `record_turn` persists
  it under the existing exclusive-lock read-modify-write, so a crash between turns cannot leave
  a ledger that disagrees with the resume state. A session recorded before this field existed
  reconciles as fresh (the `#[serde(default)]` pattern already used for every other added
  field).
- **The worker** (in `src/reviewer/mod.rs` / `src/tools.rs`, wherever a turn is currently
  assembled) gains four steps around the existing review call: load the prior ledger from the
  session store → hand it to the prompt builder (for the resumed injection) → after the review
  text is collected, extract + reconcile into an envelope and a new ledger → stash the envelope
  on the `Review`/`Outcome` and persist the new ledger via `record_turn`.
- **`src/registry.rs`** — `Review`, `Outcome`, and `Snapshot` carry the `Envelope` (or its
  serialised form) so the result renderer can emit both channels. The envelope is computed once,
  when the turn finishes, and travels with the snapshot like `usage` and `warnings` already do.
- **`src/tools.rs`** — `render_completed` appends the fenced-JSON envelope block after the
  existing prose and guidance. A new field on the result carries the envelope object.
- **`src/mcp.rs`** — `text_result` (or a new `tool_result` beside it) is extended to attach
  `structuredContent` alongside `content`; the dispatch path threads the envelope through. Tool
  definitions gain `outputSchema` for `cross_model_review_result`.

## What deliberately does not change

- **The prose.** Every existing line of the completed-review rendering stays. The envelope is
  additive on both channels. Anything currently reading the prose keeps working.
- **The single-call, single-dependency character.** No second model call, no new crate — the
  block is emitted in the same turn and parsed with `serde`.
- **The failure contract.** No new failure code. "Unstructured this turn" is a `structured:
  false` envelope with a warning, not a `REVIEWER_FAILED`.
- **The reviewer's isolation and read-only posture.** Nothing here touches the tool policy,
  the sandbox, or the capture; the reviewer emits more structured *output*, and its inputs and
  permissions are unchanged.

## Failure modes and how each is handled

| Situation | Handling |
| --- | --- |
| Reviewer emits no block | `structured: false`, verdict inferred from prose, warning; prose returned in full. |
| Block present but invalid JSON / wrong schema | Same as no block; the malformed text is not partially trusted. |
| Reviewer says `approve` but lists open findings | Machine verdict forced to `changes` (open_count wins); warning names the disagreement. |
| Reviewer cites an ID the ledger never issued | ID ignored; warning; the reviewer cannot mint identity. |
| Prior open finding the reviewer never mentions | Stays `open`, flagged `unaddressed_this_turn`; never auto-resolved. |
| Ledger persisted by an incompatible schema version | Treated as absent; session reconciles as fresh; warning. |
| Session recorded before ledgers existed | Reconciles as a fresh turn-1; no error. |
| Degraded turn (no structure) mid-session | Prior ledger is preserved untouched; that turn contributes no status updates and says so, rather than wiping known findings. |

The through-line: **every uncertain signal degrades toward "still open / say so," never toward
"resolved / silently dropped."** An autonomous loop can trust `open_count == 0` to mean the
reviewer actually cleared everything it raised, because nothing in the pipeline can manufacture
that zero.

## Tests

- **`src/findings.rs` unit tests** — extraction (clean block, block among other fences, no
  block, truncated block, oversized fields); reconciliation (turn-1 assignment; resolve; still-
  open; regressed; new finding mid-session; unaddressed prior finding; unknown-ID citation;
  monotonic non-reuse of IDs across many turns); verdict resolution (agreement; approve-with-
  open-findings contradiction; degraded inference).
- **`src/session.rs`** — ledger round-trips through persistence; a pre-ledger record loads; an
  incompatible version is ignored not misread.
- **`src/prompt.rs`** — the resumed prompt lists prior findings with their IDs; a fresh prompt
  does not; the block-format instruction is present on turn 1.
- **`src/mcp.rs` / `src/tools.rs`** — a completed result carries both `structuredContent` and
  the fenced text block; a degraded review still carries a well-formed envelope with
  `structured: false`.
- **`smoke.ps1`** — the live round trip asserts a parseable envelope on a real review and that
  a resumed turn reports a prior finding by its stable ID. This costs tokens; it runs when the
  change touches the protocol or session handling, which this does.

## Documentation

- **README** — a section on the envelope: the two channels, the loop contract (`open_count ==
  0`), stable IDs across a resume, and the `structured: false` degradation. The existing "Re-
  reviewing after you act on feedback" section gains the ID-keyed status story.
- **AGENTS.md** — the merge-gate workflow can now loop on `open_count` and reference findings
  by stable ID rather than hand-maintaining the map, which is the concrete win the issue was
  filed about.

## Open questions for the reviewer

1. **Two-value verdict + `verdict_detail`, or a single richer enum?** The issue specifies
   `approve | changes`; this plan keeps that as the loop signal and adds `verdict_detail` for
   nuance. Is that the right split, or should the loop signal itself be richer?
2. **Should a finding carry a content hash** (of title+file+detail) to help a caller notice a
   reviewer silently rewording a finding under the same ID? Currently the server trusts the
   ID→status mapping and does not diff the content. Worth the complexity, or over-engineered?
3. **Unaddressed prior findings** stay open and flagged. Is flag-and-keep the right default, or
   should a finding the reviewer ignores for K consecutive turns escalate (e.g. force
   `changes` with a distinct warning) so it cannot be quietly waited out?
4. **Degradation visibility.** Is `structured: false` + a warning enough for an autonomous
   loop to do the right thing, or should a degraded turn be surfaced more loudly (e.g. the loop
   should be told explicitly "do not treat this turn's absent findings as convergence")?
