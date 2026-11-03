# Plan: large branch diffs exhaust the reviewer's evidence budget (issue #197)

> **Revision r4 — converged. Reviewed across four rounds by Codex `gpt-5.6-luna` effort=max
> (session `issue-197-evidence-budget-plan`); turn 4 approved with 0 open findings.**
>
> Round 1 raised five findings, four major; all were accepted and none disputed. The shape of
> the plan changed as a result: r1's quantitative root cause was overstated, and r1's
> centrepiece — a per-turn budget grant sized from the composed diff — is **deferred**, because
> three of the four major findings existed only inside its machinery. Round 2 resolved f1, f2
> and f5 and held f3 and f4 open: the commit point has to span the *outer* response path, not
> just `Core`.
>
> r3 accepted that and declined three of the remedies offered alongside it — an acknowledgement
> protocol, a retention limit on cached diffs, and threading request metadata into the paging
> layer — on proportionality grounds argued in "Review round 2". Round 3 closed f3 and f4,
> accepted all three declines, and raised two new findings against r3's own cursor
> simplification; r4 fixes both and records where that line stops.
>
> All seven findings across four rounds are resolved. One of the changes is a **removal**: cursors
> stop carrying a copy of the diff text, which deletes the per-page clone that exists today. The
> per-round record is at the end of this document.

## Context

Reviews of git branches of roughly 35–45 files come back `reviewer_blocked` / `escalate` with
nothing for the caller to act on. The reporter saw five occurrences across two branches in one
day, on v0.18.1 with the Codex reviewer at the default level; branches under ~25 files / ~1,500
lines converge normally. The worst case: a ~45-file branch whose `git diff <fork-point>...HEAD`
is 416 KB, where the reviewer reported the root diff "stopped after 95 pages and 1,966,080 bytes
with `complete: false`" before it reached about a third of the files.

### What the failure actually is

**It is not one expensive canonical pull.** A 416 KB composed diff is about nine 48 KiB pages
and roughly 0.4–0.8 MiB of returned content — comfortably inside a 2 MiB / 96-call pool. At the
Claude reviewer's 24 KiB encoded pages it is about eighteen pages and a similar byte total. One
complete walk is affordable at the reported sizes; r1 of this plan claimed otherwise and was
wrong (round-1 f5).

**It is repeated walks.** The reported 95 pages for a 9-to-18-page diff is four to ten complete
re-walks, and the pool funds every one of them. Three mechanisms drive that:

- **A lost page costs a full re-walk, and cannot be retried.** `diff_cursor`
  (`src/evidence/core.rs:1478`) consumes its cursor via `take_cursor` (`1816`) *before* the
  response-size and cumulative-byte checks at `702-720`, and cursors are single-use. A page
  refused by the byte cap, or whose response is lost to Codex's 30 s client timer, destroys the
  chain. The only recovery is a fresh `repository_diff`, which recomposes from byte 0.
- **The reviewer is told to re-pull.** The approval floor obliges a complete unfiltered canonical
  diff, paged to its terminal page, on every approving turn; the prompt says so
  (`src/prompt.rs:123-127`) and the evidence auto-repair prompt says it again (`400-418`). A
  reviewer that loses its place does the only thing it is told to do.
- **Reads share the same pool.** Every evidence call draws on one per-turn budget —
  `max_calls: 96` and `max_total_bytes: 2 MiB`, hard-coded in `Limits::default()`
  (`src/evidence.rs:161-186`) and built with no reference to the change under review in
  `Bundle::create` (`449`). Enforcement is in `Core::call_with_receipt`: calls at
  `src/evidence/core.rs:651-660`, cumulative bytes at `702-720`.

So the un-scaled budget is the aggravating condition, not the proximate cause. The proximate
cause is that a canonical walk is not restartable at the point it breaks, so it is restarted from
the beginning — repeatedly — until the pool is gone.

**It is not JSON escaping.** Measured on a real 703 KB diff from this repository, escaping
inflates 3.3%, not the reported ~5×. The remaining excess is the re-walks above plus content
`git diff` does not contain: untracked files are inlined in full up to `max_file_bytes` (1 MiB)
each (`src/evidence/core.rs:3248-3273`) and capture-hook generated text is appended (`3282-3316`).

### Why it surfaces as `escalate` with nothing to act on

`reviewer_blocked` is assigned *only* from the reviewer's own `verdict_detail == Blocked`
(`src/findings.rs:990`); the server never infers it, and `Outcome::from_reason`
(`src/findings.rs:255-268`) maps it to `escalate` as the sole reason that does. `limit_exceeded`
appears nowhere in that classification — it exists only inside `src/evidence/*`. The evidence
floor does not rescue the turn either: `clears_floor` (`src/findings.rs:2085-2093`) only fails an
*approve*, and `reviewer_blocked` (rank 4) outranks `evidence_incomplete` (rank 5) anyway. The
parent never receives `Core.calls` or `returned_bytes`, and no refusal is counted anywhere, so
**the server cannot currently tell that a turn was rationed at all.**

### On the numbers themselves

There is no recorded rationale for 96 / 2 MiB / 48 KiB. `git log -S` puts all three in the
initial evidence-service commit (`1144c85`), which has no message body, and no document ties them
to a security property — `docs/codex-reviewer-evidence-service.md:298-304` asks only for
"per-call limits below" the parent capture ceiling. They are a cost and anti-runaway bound. The
constants that *do* have a stated rationale are different ones (`CLAUDE_PAGE_BYTES_CEILING`,
`CODEX_TOOL_TIMEOUT_SECS`).

Per the rigor section of `AGENTS.md`, most of this sits on the "worst case is a lost review,
re-run it" side. Two parts do not, and are treated accordingly: the serve-record ordering in
item 3 is a false-approval path, and item 1's bound has to hold against the real transport
envelope rather than an approximation of it.

**Intended outcome.** A canonical walk that breaks resumes instead of restarting; a page can
never be refused or replaced by the transport; a rationed turn says so; and an operator can raise
the ceiling without a rebuild.

## Design

Three code items (1–3), then telemetry, flags and docs (4–6). Items deferred out of r1 are
recorded in "Deferred" below with the conditions they would have to satisfy.

### 1. A page can never exceed what the transport will send — `src/evidence/core.rs`

`page_end` (`src/evidence/core.rs:1121-1127`) bounds a text page by the requested raw `limit`,
tightened by `bundle.page_bytes_ceiling` when one is set. For Codex that ceiling is `None`
(`src/tools.rs:4210`), so a 192 KiB raw page of escape-heavy diff can serialize past the cap and
be refused — wasting the call *and*, per item 3, the cursor.

Two caps are in play and the second is the one that matters:

- `max_response_bytes` (256 KiB) is checked inside `Core` against the serialized *result*
  (`src/evidence/core.rs:702-709`).
- `serve_requests` separately checks the **full JSON-RPC envelope** and, when it is over,
  *replaces* the response with a `response_too_large` tool error after `handle` has already
  returned (`src/evidence.rs:1036-1055`).

So add an always-on tightening in `page_end` computed against the serialized envelope budget —
`max_response_bytes` less a margin for everything wrapped around the diff body — via the existing
`encoded_bounded_end` (`src/evidence/core.rs:2824`).

**The margin is a reserved worst-case constant, not a measurement.** Everything in the envelope
except the request id has a fixed shape and therefore a computable worst case: the fixed JSON-RPC
and `structuredContent` wrapper (`src/evidence.rs:1066-1102`), the cursor token
`{nonce}-{16 hex}` with its bounded nonce and counter (`src/evidence/core.rs:1803`),
`text_result`'s fixed keys and its numeric fields at their bounds (`2770-2781`), and the trailing
newline — one byte, written *after* the size check (`src/evidence.rs:1057-1060`), so it must be
reserved too.

The request id is the one variable part, and the honest bound on it is `max_request_bytes`
(64 KiB) — a quarter of the whole response budget. Reserving that would shrink every page by 25%
to accommodate a client that does not exist. So reserve a modest id allowance instead, and let an
id past it fall through to the existing `response_too_large` replacement
(`src/evidence.rs:1037-1055`). That is safe *because of item 3*: the replacement path no longer
destroys the walk, so the residual case costs one retryable page rather than a lost review. Item 1
does not have to be airtight once item 3 makes failure cheap.

Round 2 suggested instead passing the real request metadata down into the page-budget
calculation. Declined: `page_end` has no request id, the cursor is minted after it returns, and
the outer object is built later still, so that is an API refactor threading request identity
through the paging layer to recover a fraction of a percent of a 256 KiB budget. The constant is
checkable, and a test on the bytes actually written is what proves it either way.

**Do not reuse `page_bytes_ceiling` for Codex to get this effect.** That field also bounds
`repository_read` windows (`src/evidence/core.rs:1027`, `1047`), so setting it for Codex would
shrink Codex's *reads* and cost more calls — the opposite of the goal. Both review rounds
confirmed this reading and that a separate envelope-aware bound is the right mechanism.

### 2. Raise the default diff page — `src/evidence.rs`

`default_change_bytes: 48 * 1024` → `192 * 1024`, equal to `max_change_bytes`. A Codex reviewer
that passes no `limit_bytes` goes from 9 pages to 3 on a 416 KB diff, which cuts the cost of the
mandatory walk — and of every re-walk — by two thirds. Claude is unaffected; its 24 KiB encoded
ceiling still binds first.

Safe only because item 1 lands with it: 192 KiB is already the validated maximum for this
argument, and `docs/codex-reviewer-evidence-phase0.md:99-102` records the original envelope
defect (a maximum page duplicated into both `content` and `structuredContent`) and the unit test
that pins a maximum page at the real transport cap. The payload has since gone to
`structuredContent` only, asserted at `src/evidence.rs:1849`.

### 3. Make a canonical walk resumable, and stop recording pages that were never delivered — `src/evidence/core.rs`

The core of the fix. The last change is a fail-closed correctness bug rather than a budget one.

**One commit point, after the response is on the wire.** The state a page mutates — the byte
counter, the cursor map, the serve record — must all commit together, and not inside `Core`.
Cancellation and the `response_too_large` replacement both happen after `handle` has returned
(`src/evidence.rs:1028-1061`), and the write itself happens after those, so a page committed
inside `Core` can be charged for and have its cursor consumed while the reviewer receives an
error or nothing at all. Retrying then fails with `invalid_cursor` — the bug this item exists to
fix, reproduced one layer out (round-2 f3, f4).

So `handle` returns the result together with a pending commit, and `serve_requests` commits it
only after the cancellation check, the envelope-size check, and a successful `write_all` and
`flush`. A write error aborts it. Rolled back, a page leaves: the counter unchanged, its own
cursor restored at its original offset, no successor, and no serve record.

What that requires:

- **Transactional byte accounting.** `returned_bytes` is *assigned* and only then checked
  (`src/evidence/core.rs:711-719`), so a refused page leaves the counter permanently over budget
  and every retry fails too. Compute the prospective total, check it, commit on success
  (round-1 f3).
- **Cursors stop carrying the diff text.** Today every diff page clones the whole composed diff
  into a new cursor entry (`src/evidence/core.rs:1449-1457`, `1513-1521`). Store it **once per
  operation** instead, on `Core`:

  ```
  ops: HashMap<String, DiffOp>        // keyed by the op id first_diff_page already mints (1434)
  DiffOp { text: Arc<str>, canonical: bool, complete: bool, digest: Option<String> }
  ```

  `canonical` and `complete` move here because they are properties of the operation, not of a
  page; `diff_cursor` reads them from `ops` and still writes them to every continuation serve
  record, which the gate depends on. `CursorPage` **keeps its `operation` field** — so
  `take_cursor`'s cross-tool validation (`1816-1829`) is untouched — and its `values` become
  `{op, offset}`. The shared `list` / `search` / `history` / `change` pagination and the
  `text_cursor` path used by revisions and Perforce diffs are not migrated and keep their current
  shape (round-3 f6).

  **Operation lifecycle**, so a `DiffOp` does not outlive its use (round-3 f7):

  - terminal page commits → drop the op;
  - a first page rolls back → drop the op;
  - a continuation rolls back → keep it, since the restored cursor still needs it;
  - a fresh canonical pull whose digest matches a live op shares that op's `Arc<str>` rather than
    storing a second copy. The digest is already computed at `1442`, so this is a lookup, not new
    machinery — and it is what bounds retention to one copy **per distinct diff content** rather
    than one per pull, which is the accumulation round 3 was right to flag.

  Net against today: one copy of the composed diff per distinct content per turn instead of one
  clone per page, and rollback becomes a two-field restore.
- **Retry resumes at the same offset.** A refused or lost page resumes instead of forcing a
  recompose from byte 0. A failed attempt consumes a call — so a reviewer cannot loop for free —
  but not bytes it never received (round-1 f3).
- **The serve record is part of the same commit.** `append_serve_record` (`604-611`) currently
  runs inside `first_diff_page` / `diff_cursor`, so a page that trips the byte cap, is cancelled,
  or is replaced by `response_too_large` is still recorded `terminal: true`, and
  `aggregate_serve_records` (`src/tools.rs:5934-6044`) then treats the canonical op as served to
  its end. That is a false-approval path, confirmed exploitable in both rounds (round-1 f4).

**What this claims, and what it does not.** The commit means *the server wrote the complete
response*. It does not mean the reviewer received or read it: if a client-side timer discards a
response without closing the transport or failing the write, the server cannot tell delivery from
loss. That limit is not new and is not papered over — it is the line
`docs/retire-capture-modes.md:351-359` already draws, where the floor "does **not** try to prove
the reviewer *read or comprehended* any page" because only the request is server-observable. The
plan's wording is therefore "the server wrote the response", and item 4's refusal telemetry is
what surfaces a turn that went wrong underneath it.

### 4. Make a rationed turn legible — `src/evidence/core.rs`, `src/tools.rs`

The caller cannot presently distinguish "the reviewer judged itself blocked" from "the reviewer
was refused evidence until it gave up". Close that, minimally:

- Count `limit_exceeded` refusals in `Core` and write one serve-record line for them. The serve
  record is already the child→parent channel and is already aggregated.
- Surface the count from `aggregate_serve_records` and add a warning naming it, beside the
  existing `denial_count` that `AGENTS.md` tells callers to read.
- **Do not add a new `non_convergence_reason`.** `reviewer_blocked` comes only from the reviewer's
  own verdict; reclassifying it server-side would be the server guessing at another model's
  judgement. A count plus a warning gives the caller what issue #197's idea 4 actually needs —
  "this turn was rationed, re-run it" — without inventing an outcome.

This is also the instrumentation that tells us whether the deferred items are needed: a turn that
still reports refusals after items 1–3 is the evidence for reopening them.

For the record, issue #197's idea 5 ("don't have block repair demand the complete diff when the
turn already ran out of requests paging it") needs no code. Evidence auto-repair fires only when
`provisional_approve()` holds or nothing was read (`src/tools.rs:4738-4744`), so it never runs on
a blocked turn; and when it does run it is a fresh child process with a fresh budget
(`src/tools.rs:3893-3917`), which is why the issue's "37 + 15 (block repair)" row converged.

### 5. Configurable ceilings — `src/config.rs`, `src/evidence.rs`, `src/tools.rs`, `README.md`

An operator escape hatch, and what the reporter needs in order to test a build without a rebuild.
Requested by the maintainer and in scope.

Two flags following the `--timeout-seconds` pattern (`src/config.rs:1178-1192`): a named
`DEFAULT_*` / `MAX_*` constant pair beside the others (`781-859`), a local seeded in
`Config::from_args` (`1037-1045`), an arm in the `match key` loop, a `Config` field (`862-890`),
and the `"--flag must be …, got '{v}'"` error form.

- `--evidence-max-calls <n>` — rejects 0. `limits_schema()` pins every field to `minimum: 1`, so
  there is **no** 0-means-unlimited idiom to reuse here, unlike `--session-max-turns`; say so in
  the error text. Cap at the `intake_slots` clamp of 1024 (`src/evidence.rs:835-837`), above which
  the intake queue gains nothing.
- `--evidence-max-bytes <n>` — rejects 0, and rejects anything below `max_response_bytes` because
  `Limits::validate` requires `max_response_bytes <= max_total_bytes` (`src/evidence.rs:213`);
  name that reason in the error. Cap at 64 MiB as a cost bound.

Then:

- Add `Config::evidence_limits() -> Limits`, applying both overrides to `Limits::default()`, so
  the flag→`Limits` mapping lives in one testable place.
- Add a `limits: Limits` parameter to `Bundle::create` (`src/evidence.rs:436-444`) and pass it
  from the three production call sites, all of which already hold a `Config`: `src/tools.rs:4199`
  (the review turn), `src/tools.rs:5856` (`CompanionEvidence::build`), and `src/evidence.rs:1227`
  (the status preflight). Test sites (`src/evidence.rs:1618`, `1652`; `src/evidence/core.rs:4030`,
  `4111`, `4157`) pass `Limits::default()`.
- The other four `Limits::default()` call sites (`src/hook.rs:256`, `src/worktree.rs:130`,
  `src/evidence/generated.rs:406`, `src/companion.rs:149`) are *not* the reviewer's budget — they
  feed git helpers that need a timeout or `max_files`, and `companion.rs:149` runs while
  `Config::from_args` is still mid-construction. Leave them alone.

Lockstep surfaces: the `USAGE` constant (`src/config.rs:2477-2697`), the README "All options"
block (`README.md:139-188`), and the hand-written doctor display (`src/tools.rs:2374-2386`) so a
tester can confirm a build took the flag. `docs/companion-repositories-plan.md:228-229` already
notes that each companion server keeps its own budget; that stays true, now per configured value.

### 6. Docs

- Record the budget rationale that does not exist today — what the numbers bound (cost and
  runaway, not a security boundary) — in `docs/codex-reviewer-evidence-service.md` §5 (`296-313`),
  and state plainly that item 5 lets an operator raise the cost ceiling, which is the one
  allowance this change widens.
- Note in `docs/retire-capture-modes.md` that the approval floor is unchanged, and that a
  canonical walk is now resumable at the page that broke rather than only from byte 0.
- `docs/evidence-read-timeout.md:237-238` sizes an abandoned-worker thread analysis from the
  96-call budget. Update it to say the bound now scales with `--evidence-max-calls`, and that the
  hard 8-worker cap (`:244`) is what actually holds.
- Keep the verified-vs-assumed discipline: the escaping measurement and the smoke result are
  verified; the reporter's branches are not claimed to pass until they do.

## Critical files

| File | What changes |
|---|---|
| `src/evidence/core.rs` | `page_end` envelope-safe bound; transactional accounting in `call_with_receipt`; `ops` map so cursors drop the diff text; `diff_cursor` / `first_diff_page` rollback and retry; pending commit returned from `handle`; refusal counter |
| `src/evidence.rs` | `default_change_bytes`; `Bundle::create` takes `limits`; the commit point in `serve_requests`, after cancellation, the envelope check and a successful write |
| `src/config.rs` | two flags, constants, `Config` fields, `Config::evidence_limits()`, `USAGE` |
| `src/tools.rs` | `Bundle::create` call sites, refusal count through `aggregate_serve_records`, warning, doctor line |
| `README.md`, `docs/` | options list; budget rationale; floor and thread-leak notes |

## Deferred

Not abandoned — deferred, with the conditions they must satisfy recorded so reopening them does
not start from scratch. Item 4's refusal telemetry is what will say whether they are needed.

**A budget grant sized from the composed canonical diff** (r1's item 4, the root-cause fix as
r1 framed it). Three of round 1's four major findings existed only inside this machinery, which
is itself the argument for taking it out rather than hardening it. To be viable it would need:

- *Bootstrap admission.* `call_with_receipt` increments and checks the call budget **before**
  dispatching to `first_diff_page` (`src/evidence/core.rs:651-660`, then the match at `663`), so a
  grant made inside `first_diff_page` can never rescue a turn whose pool is already spent — the
  canonical call is refused before the grant happens (round-1 f1).
- *A reserved budget, not a larger shared one.* Adding to the same counters lets exploratory reads
  and invalid calls spend an allowance granted for the obligation. The canonical pages would have
  to be charged against a separate budget (round-1 f1).
- *A canonical minimum page size, and real serialized costs.* `limit_arg` resolves through
  `positive_arg` (`src/evidence/core.rs:2740-2747`), so `limit_bytes` may be as small as **1 byte**;
  any call estimate assuming 24 or 192 KiB pages under-grants by orders of magnitude. The byte
  counter also charges the whole serialized result including the cursor token and envelope, not
  `json_escaped_len(text)` alone (round-1 f2).
- *A real composed-diff maximum.* r1 asserted the composed diff is bounded near 8 MiB by the
  child-output cap. That bound covers only the **tracked** half (`src/evidence/git.rs:988-994`);
  `compose_resolved` appends untracked files independently at up to 1 MiB each
  (`src/evidence/core.rs:3248-3273`), so ten of them exceed any 4× cap derived from the default
  budget (round-1 f2).

**Returning the change summary to the reviewer** (r1's item 3): adding `bytes`, `files`,
`insertions`, `deletions` and `untracked` to the `repository_diff` result, which `ComposedDiff`
(`src/evidence/core.rs:3010-3019`) already computes and currently writes only to the serve record.
Cheap, but informational: it helps only if reviewers are in fact choosing bad page sizes, and a
canonical minimum page size would address that more directly. Reopen if item 4's telemetry shows
it. Note it would need `repository_diff` split out of the shared `text_page` output schema
(`src/evidence.rs:1401-1414`, `1468-1469`), since `additionalProperties: false` is set and Codex
validates `structuredContent` against the declared schema.

## What this deliberately does NOT do

- **No change to the approval floor, and no path-scoped approval.** `canonical` still requires
  `branch-base`, `worktree` and no `path` (`src/evidence/core.rs:1258-1262`). Issue #197's idea 6
  (letting declared slices add up to an approval) is out of scope: the unfiltered requirement is a
  documented concession in `docs/retire-capture-modes.md:307-323`. Round 1 confirmed the plan
  leaves the floor and the canonical predicate unchanged and moves no read-only or isolation
  boundary.
- **No acknowledgement protocol for served pages.** Offered in both rounds as the way to close
  the client-timeout gap. Declined: proving the reviewer *received* a page is exactly the coverage
  attestation `docs/retire-capture-modes.md:351-359` rules out, and it would mean a new round trip
  per page on the hot path of the very walk this plan is trying to make cheaper. The claim is
  narrowed instead (item 3).
- **No retention limit or eviction policy on cached diffs.** Rounds 2 and 3 both pushed here.
  Answered with a lifecycle and digest sharing (item 3), which bounds retention to one copy per
  distinct diff content per turn. No cap, no eviction, no separate interning map.

  **And this is where that line stops.** If a further edge case appears in the operation
  lifecycle, the answer is to delete the `ops` map and keep today's per-page clone, not to add
  more to it. Memory is not the bug #197 reports; it came up only because a retryable cursor had
  to hold something. The budget fix does not depend on it.
- **No per-level budget table.** A `--level` is only `(model, effort)` (`src/config.rs:110-113`),
  and levels are deliberately excluded from reviewer identity.
- **No migration machinery for the bundle.** One binary writes and reads it
  (`src/evidence.rs:31-39`).
- **No new `non_convergence_reason`**, for the reason given in item 4.

## Verification

### Unit (`cargo test`)

No test exercises either ceiling today, so these are new:

- call-budget and byte-budget exhaustion each return `limit_exceeded`;
- **the rollback table** — one case per failure mode, each asserting the counter, the cursor map
  and the serve record together. A page refused by the Core byte cap; a response suppressed by
  cancellation; a response replaced by `response_too_large`; a write that fails. In every one:
  `returned_bytes` unchanged, exactly one cursor left (its own, at its original offset), no
  successor, no serve record — and the retry of that page then succeeds;
- a failed attempt consumes a call but not bytes;
- a committed page is recorded only after the bytes are actually written;
- cursors hold no diff text: a chain of N pages keeps one copy of the composed diff, not N;
- **the operation lifecycle**, asserting the `ops` map and not only the cursor map: dropped on
  terminal commit, dropped when a first page rolls back, retained when a continuation rolls back,
  and a second canonical pull with the same digest sharing the first op's text rather than
  allocating a second copy;
- a continuation serve record still carries `canonical` and `complete` once they live on the
  operation rather than the cursor, and a cursor from one tool is still refused by another;
- `page_end` never yields a page whose full serialized JSON-RPC envelope exceeds
  `max_response_bytes` — on escape-heavy input, on a continuation page (which carries a cursor
  token the first page does not), with a request id at the reserved worst case, and asserted
  against the **bytes actually written** including the trailing newline rather than against
  `id: 1`;
- `--evidence-max-calls` / `--evidence-max-bytes`: default, override, rejected 0, rejected below
  `max_response_bytes`, rejected above cap, non-integer — following `src/config.rs:4121-4167`;
- a cross-check of `limits_schema()` against the real `Limits` fields — absent today, so a new
  field silently breaks `repository_scope`'s declared output schema.

### Build

```powershell
.\build.ps1
```

`cargo fmt --check`, clippy with `-D warnings`, unit tests, release build, restage. Open agent
sessions must be unloaded first or it reports the blocking PIDs.

### Real round trip (costs tokens)

`AGENTS.md`: "The evidence service is protocol." Both directions are required, because item 1 and
item 2 behave differently per reviewer — Codex has no page ceiling and takes the raised default,
Claude's 24 KiB encoded ceiling still binds.

```powershell
cargo build --release
```

```powershell
.\smoke.ps1 -Reviewer codex
```

```powershell
.\smoke.ps1 -Reviewer claude
```

### The case the issue reports

Neither the unit tests nor the smoke exercises a 40-file branch. The reporter offered to test a
build against the same branches; take them up on it, and confirm from the result that the
canonical pull completed and that item 4 reports no refusals. That is also the gate on the
deferred items: refusals still appearing means the shared pool needs the reserved canonical
budget after all.

## Test-the-gate

Review this change through `cross_model_review` on session `issue-197-evidence-budget-plan`,
pointing the reviewer at `AGENTS.md` and `README.md`, and asking it to scrutinise the accounting
transaction, the cursor retry path, the serve-record commit point, and whether the approval floor
still fails closed. Collect with `cross_model_review_result`.

This PR is **not** a gate repair, so the `AGENTS.md` bypass exception does not apply and must not
be invoked: the change alters how much evidence a reviewer can pull and when a page counts as
served, which the gate itself then has to be shown working across.

## Review round 1

Session `issue-197-evidence-budget-plan`, turn 1, Codex `gpt-5.6-luna` effort=max,
`changes_requested` / `open_findings`, 84 evidence calls, 0 denials, capture complete. Five
findings, four major. **All accepted; none disputed.** Each was re-checked against the code before
acceptance.

| # | Finding | Disposition |
|---|---|---|
| f1 | The canonical allowance cannot bootstrap (the call check precedes dispatch) and, once granted, is not reserved against exploratory calls | Accepted. The grant is deferred; both conditions recorded under "Deferred" |
| f2 | The grant arithmetic assumes 24/192 KiB pages though `limit_bytes` may be 1 byte; the byte counter charges the whole serialized result; the 4× cap is reachable because the 8 MiB child-output cap bounds only the tracked half | Accepted. Verified `limit_arg` → `positive_arg` admits 1. Deferred with the grant |
| f3 | A retryable cursor does not roll back `returned_bytes` (assigned before the check) and leaves a staged successor cursor behind | Accepted. Became item 3's first three bullets — the accounting is now transactional, which is what makes retry work at all |
| f4 | Moving the serve record after Core accounting is not enough: `serve_requests` can suppress or replace a response after `handle` returns | Accepted. Verified at `src/evidence.rs:1028-1055`. Item 3 now stages the record and commits after cancellation and the envelope check; item 1 now bounds the envelope, not the escaped body |
| f5 | The one-pass quantitative diagnosis is overstated — a 416 KB diff is ~9 pages and does not alone exhaust 2 MiB | Accepted. The Context section is rewritten around repeated walks; r1's framing was wrong |

Round 1 also confirmed, against the tree, the plan's claims about `reviewer_blocked` arising only
from the reviewer's own verdict, `limit_exceeded` being invisible to outcome classification,
single-use cursors, Codex's `None` page ceiling, and `page_bytes_ceiling` also bounding
`repository_read` — including that reusing it for Codex would be the wrong mechanism. It confirmed
the floor and the read-only and isolation boundaries are unmoved. It could not reproduce the
reporter's branches, the 703 KB escaping measurement, or run the model smokes.

## Review round 2

Turn 2 on the same session, `changes_requested` / `open_findings`, 33 evidence calls, 0 denials,
capture complete. **f1, f2 and f5 resolved; f3 and f4 held open; no regression.** Round 2 also
agreed the deferral is the right call, and that items 1 and 2 alone would not be robust because a
failed continuation would still restart the walk — which is why item 3 is the core of the plan and
not an optional hardening.

**Accepted.** f3 and f4 were held for the same underlying reason: a commit inside `Core` is the
wrong commit point, because cancellation, the `response_too_large` replacement and the write all
happen after `handle` returns. That is correct and it is now item 3's opening paragraph — one
commit, returned through `handle`, applied after the write succeeds, aborted on a write error.

**Declined, with reasons.** Three remedies offered alongside those findings are not in r3:

- *An acknowledgement protocol*, to close the case where a client-side timer discards a response
  with no server-visible error. Round 2 itself offered the alternative of narrowing the claim, and
  the repository has already settled this question in that direction:
  `docs/retire-capture-modes.md:351-359` says the floor does not try to prove the reviewer read or
  comprehended a page, because only the request is server-observable, and calls that line "not a
  slide back to full coverage attestation". An acknowledgement round trip per page is that slide,
  on the hot path of the walk this plan is making cheaper. The claim is narrowed to "the server
  wrote the response" instead.
- *Bounding the cursor map* against stranded entries from lost pages. Answered by removing the
  cost rather than capping it: cursors now carry `{op, offset}` and the composed diff is stored
  once per operation, so a stranded entry is about a hundred bytes. This deletes code that exists
  today instead of adding more.
- *Threading request metadata into the page-budget calculation.* Round 2 is right that `page_end`
  sees no request id, that the cursor is minted afterwards and that the outer object is built
  later still — which is the argument against the refactor, not for it. Everything but the id has
  a fixed shape and a computable worst case, so r3 reserves a constant and pins it with a test on
  the bytes actually written. Round 2's test list is adopted in full, including the continuation
  page and the worst-case id.

## Review round 3

Turn 3 on the same session, `changes_requested` / `open_findings`, 21 evidence calls, 0 denials,
capture complete. **f3 and f4 resolved**, so all five round-1 findings are now closed, and **all
three declines above were accepted** — the acknowledgement decline explicitly, on the grounds that
`docs/retire-capture-modes.md:351-359` defines a server-attested floor rather than proof of client
receipt. Round 3 also confirmed the envelope-margin constant is derivable without threading
request metadata, that the six-item shape is proportionate, and that no approval-floor, read-only
or isolation boundary moved.

Two new findings, both against r3's own `ops`-map change — the one place r3 made the code smaller.
Both accepted:

| # | Finding | Disposition |
|---|---|---|
| f6 | `{op, offset}` drops `canonical` / `complete`, which `diff_cursor` writes to continuation serve records and the gate depends on, and dropping `CursorPage.operation` would remove `take_cursor`'s cross-tool validation; `CursorPage` is also shared by list/search/history/change and `text_cursor` serves revisions and Perforce diffs | Accepted. `canonical` and `complete` move onto `DiffOp` where they belong (they are per-operation, not per-page); `CursorPage` keeps `operation`; the shared and `text_cursor` paths are not migrated. A description error on my part, not a design one — but a real one |
| f7 | A stranded cursor still pins the whole diff through `ops[op]`, so "about a hundred bytes" was wrong, and the plan specified no operation lifecycle | Accepted. Lifecycle rules added, and the accumulation answered by sharing one `Arc<str>` per distinct digest rather than by a retention limit. The "hundred bytes" claim is withdrawn |

I have also written down where this stops: a further edge case in the operation lifecycle should be
answered by deleting the `ops` map and keeping today's clone, not by adding to it. See "What this
deliberately does NOT do".

## Review round 4 — converged

Turn 4 on the same session: **`converged`, verdict `approve`, 0 open findings**, 7 evidence calls,
0 denials, capture complete. f6 and f7 resolved; no new findings; no regressions across four
rounds.

Three things round 4 checked that the plan had asserted rather than proved:

- **Digest sharing cannot confuse the gate.** `aggregate_serve_records` groups by operation id
  while *separately* requiring canonical operations' digests to agree (`src/tools.rs:5945-6027`),
  and op ids stay unique as `{nonce}-diff-{calls}`. Sharing one text `Arc` between two operations
  with equal digests therefore cannot merge serve records or mix `canonical` / `complete` flags.
  This was the interaction I was least sure of.
- **The oversized-request-id residual is acceptable.** A conforming client can send a large id,
  because the server caps only the whole request (`src/evidence.rs:949-977`). Such an id may
  trigger `response_too_large` — but item 3 rolls the page state back and returns a retryable
  error, so it is a performance residual, not a broken cursor, accounting or approval invariant.
- **Item 3 should not be cut.** Its transaction is what fixes both the false-approval path and the
  retry failure; the `ops` optimisation inside it is scoped, tested and removable if
  implementation proves it disproportionate.

The approval floor, the read-only posture and the isolation boundaries are unchanged, confirmed in
every round. The plan is implementable as written.

### Session cost

Four turns, 4 model calls, roughly 11.3M input tokens (almost all cache reads) and 118k output.
Evidence calls fell 84 → 33 → 21 → 7 as the plan stabilised.
