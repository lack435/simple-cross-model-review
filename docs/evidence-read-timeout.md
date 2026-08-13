# Evidence `repository_read` must not hang the request loop — design

Status: **planned.** This document is the plan. It goes through this repository's own
`cross-review` gate before implementation begins, and the implementation goes through it again.

Tracks issue #61 (`EVIDENCE_UNAVAILABLE` after a 30s `tools/call` timeout on `repository_read`).
Filed from dogfooding: in one session `cross_model_review` failed three times (~14%), each time
on a `repository_read` that stalled 30s **after** `repository_scope` and `repository_list` had
already returned in the same run — so the evidence service was provably alive; a single read
round-trip just exceeded a ceiling and took the whole review down with it.

> **Review history.** One round against this repository's own gate (Codex, gpt-5.6-luna,
> effort=max), REQUEST CHANGES with four `major` + two `minor`, each accepted and none disputed:
> f1 (the watchdog must bound the *whole* read path, including the drift stamp, not just the byte
> read), f2 (the worker must return the resolved path, not just bytes), f3 (the evidence server is
> **not** in the reviewer job object, so abandoned workers are bounded only by the evidence
> process lifetime and need an explicit cap), f4 (the read budget and Codex's `tool_timeout_sec`
> must derive from one shared constant under one monotonic total deadline), f5 (retry must cover
> transient fast I/O errors, not only the stall), and f6 (a *timely in-band* read error does **not**
> invalidate the review — only the transport-level 30s abandon does, which reframes the whole fix).
> A second round (REQUEST CHANGES, two `major` + three `minor`, all accepted) confirmed f1–f6
> resolved and sharpened five refinements: f7 (the worker must **preserve** the `observed_stamp`
> cache, not rescan `tree_stamp` every read), f8 (the budget must be measured from request
> *receipt*, and the end-to-end proof honestly scoped, because serial dispatch can delay a read
> after Codex's client clock has started), f9 (`repository_search` reads per-file too, so "every
> other operation is bounded" was an overclaim — route it through the shared bounded-read helper),
> f10 (classify `raw_os_error()` before it is flattened to `read_failed`, or the retry predicate is
> blind to it), and f11 (test the MCP-response layer and the Codex-event layer separately). A third
> round (REQUEST CHANGES) confirmed f7/f9/f10/f11 resolved and drew the boundary of the guarantee
> tighter still: f12 (the response *write-back* is itself unbounded — a client that stops draining
> stdout is transport death, named and excluded, not bounded), f13 (only `search`'s per-file *read*
> is covered, with a no-stamp helper mode; its walk and the sibling walks stay follow-up), and f14
> (the raw-error classifier must span every blocking worker stage, not only `read_bounded`).
>
> **Implementation code-review** (the diff, not the plan) then made three refinements this document
> now reflects: the receipt `Instant` is stamped when the request is read off stdin and threaded
> through the dispatch channel — not captured at `Core::call` entry, which would miss the
> in-channel queue wait (impl finding f1); the **drift stamp moved out of the read worker back to
> the main thread**, computed *after* content validation, because stamping in the worker reordered
> a prompt content error behind a repository-wide walk and folded `tree_stamp`'s unclassified errors
> into the retry path (impl findings f3/f2); and the `GetFinalPathNameByHandleW` verification
> failure is now classified via `GetLastError` (impl f2). A second implementation round then added
> two more: the drift-stamp walk *is* watchdog-bounded after all — but on the **main thread, after
> content validation**, via `current_stamp()` (impl f4), so a first `repository_read`/`scope` whose
> `tree_stamp` stalls still cannot hang the loop; and worker threads are spawned with
> `Builder::spawn` so an OS thread-creation refusal becomes an in-band `read_unavailable` instead of
> panicking the evidence process (impl f5). Net: the read *and* `scope` are fully bounded (file I/O
> in a worker, stamp in a single-attempt bounded walk); only `list`/search-base `walk_files` remain
> the deferred sibling follow-up. A final round settled the ingress channel: the receipt threading
> (impl f1) surfaced that a full *unbounded* queue could OOM the process (impl f8), so the channel
> stays **bounded** for admission control, with the receipt-under-backpressure residual made an
> explicit contract (impl f6) rather than traded for a memory-safety hole — plus doc-consistency
> passes (impl f7/f9). This document reflects the as-implemented design.

## What #61 is, precisely — and the one fact the whole fix turns on

The 30s in the error is **not our constant**. It is Codex's own MCP client-side
`tool_timeout_sec`, which we set to `30` when we generate the reviewer's config
(`src/reviewer/codex.rs:182-185`). When a `tools/call` to our evidence service exceeds it, Codex
**abandons the call at the transport level** and emits an `item.completed` for the `mcp_tool_call`
carrying a non-null top-level `error` and `status: "failed"`.

That top-level transport error is what is fatal. `parse` returns `EVIDENCE_UNAVAILABLE` — **even on
an otherwise-complete review** — the moment `events.evidence_infrastructure_errors` is non-empty, and
`parse_events` fills that vector only from a **non-null top-level `item.error`** on an evidence
`mcp_tool_call`. Two existing tests pin exactly this line:

> **Amended by #71.** The rule above is no longer unconditional. One narrowly-specified shape — the
> abandonment Codex reports as a failed tool call carrying its own `timed out awaiting tools/call`
> phrase — is demoted to a `warnings` entry on a review that otherwise completed, **and only when at
> least one other evidence call in the same turn completed**, which is the evidence that the service
> was answering rather than absent. A turn that does not survive it fails as the separate
> `EVIDENCE_CALL_ABANDONED`. Everything else that reaches `evidence_infrastructure_errors` is still
> fatal exactly as described here, and the demotion deliberately matches an observed shape rather
> than inferring a service's state from error prose — five review findings in a row landed on that
> inference before it was removed.

- `evidence_transport_error_invalidates_an_otherwise_completed_review`
  (`src/reviewer/codex.rs:1189-1200`) — a call with top-level `"error":"connection closed"`,
  `"status":"failed"` → `EVIDENCE_UNAVAILABLE`. **This is the shape Codex's 30s abandon produces.**
- `model_argument_error_is_not_misread_as_service_death` (`src/reviewer/codex.rs:1202-1213`) — a
  call that *completed* with an **in-band** error result (`"result":{…,"is_error":true}`,
  top-level `"error":null`, `"status":"completed"`) → the review **survives**; only the reviewer
  sees the tool error.

And our own `EvidenceError` path already returns the *second* shape: `handle` emits an error as a
tool **result** `{"structuredContent":{"error":{…}},"isError":true}` (`src/evidence.rs:645-648`),
with no top-level transport error. So the read codes we already return (`read_failed`,
`file_too_large`, `binary`) are ordinary in-band tool errors that do **not** fail a review.

**This is the crux of the fix.** The problem is not that a read can fail; a fast in-band read
failure is already survivable and already reviewer-visible. The problem is that a *stalled* read
returns **nothing at all** until Codex tears the call down at 30s, and *that* teardown is the
fatal transport error. **So the entire fix is: make a stalled read return a fast in-band
`read_timeout` error before the 30s abandon fires.** That converts the one fatal shape into the
already-survivable shape, with no change to `parse` and no loosening of any contract (this is
finding f6, and it is why Decision D below leaves propagation untouched).

### The read path has no wall-clock bound — and `search` shares its flaw (f9)

The git operations are genuinely bounded: they shell out through
`crate::reviewer::run(..., Duration::from_millis(limits.operation_timeout_ms), cancel)`
(`src/evidence/git.rs:90-95`) — a hard 15s deadline that *kills* the child. The filesystem
operations are **not**: `tree_stamp`, `walk_files`, and `search` call `deadline()`
(`src/evidence/core.rs:873-882`) only *between* iterations, and `search` calls `read_bounded` for
each candidate file (`src/evidence/core.rs:245-250`, `:741-772`) — so a single blocked per-file
read stalls `search` exactly as it stalls `read`. The earlier draft's "every other operation is
bounded" was an overclaim; the cooperative `deadline()` bounds neither a blocked stat nor a blocked
`read_bounded`. Because `read` and `search` share the `read_bounded` primitive, the fix bounds that
primitive once (below) and routes **both** through it, so `search`'s **per-file reads** are covered
by the same watchdog rather than left as a second unbounded path. Two precise limits on that (f13):
the shared helper takes a **no-stamp mode** so `search` does not recompute a drift stamp for every
candidate file (search does not need per-file drift, and a stamping helper would tree-scan per
hit; as implemented the read's own stamp is bounded on the main thread instead — see the stamp
bullet below). This covers only the per-file *read* — `search`'s own `resolve_existing`/`walk_files`
(`src/evidence/core.rs:234-235`, `:446-510`), like `list`'s `walk_files`,
have their own uninterruptible `read_dir`/`symlink_metadata` and stay the **documented follow-up**
for the same helper, not something this change claims to bound.

`repository_read` is bounded by neither `deadline()` nor a killed child. `Core::read` (`src/evidence/core.rs:290-344`) calls
`resolve_existing` (per-component `symlink_metadata` + `fs::canonicalize`), checks `is_file()`,
then `read_bounded` (`src/evidence/core.rs:741-772`) — `File::open`, `verify_open_file` (a Windows
`GetFinalPathNameByHandleW` syscall plus another `fs::canonicalize`, `:774-819`), `metadata`,
`seek`, `read_to_end` — and finally `current_stamp` (`:346-353`). **None of these is wrapped in a
deadline, and none is on a loop that checks one.** The `operation_timeout_ms` limit (15s,
`src/evidence.rs:103`) is **effectively dead code for reads**: `deadline()` is only ever called
from `search`, `walk_files`, and `tree_stamp`.

### Why a cooperative deadline would not fix it

The obvious patch — "call `deadline()` in `read` too" — does not work, and the plan says why so
the reviewer does not propose it. `deadline()` is a *cooperative* check (`if start.elapsed() >
timeout`) that can only fire *between* operations. The stall is *inside* a single blocking syscall
— `File::open`, `read_to_end`, `fs::canonicalize`, or `GetFinalPathNameByHandleW` — against a file
an on-access AV scanner is holding, an oplock is contending, or a slow/redirected path is backing.
A thread parked in `read_to_end` never returns to a checkpoint. Bounding a blocked syscall requires
a mechanism that runs *concurrently* with it, not a checkpoint between calls.

### The complete read path must be bounded, not just the byte read (f1)

A subtle trap the first review caught: even after bounding the open+read, `Core::read` still calls
`current_stamp()`, which on the **first** read of a run computes `tree_stamp` — a directory walk
whose individual `read_dir`/`symlink_metadata` syscalls are themselves uninterruptible (the
`deadline()` between iterations does not bound a single blocked stat). The earlier draft leaned on
"`repository_scope` ran first and cached the stamp," but scope-first is **not** part of the service
contract — a reviewer may `repository_read` before it ever scopes. So the watchdog must bound the
**whole** blocking portion of a read: resolve → `is_file` → open/verify/stat/read → drift stamp.
`repository_list` and `repository_scope` share this latent class (unbounded stat syscalls); this
change bounds the read path #61 names and factors the watchdog as a reusable helper so list/scope/
walk can adopt it as a documented follow-up, rather than widening this change into all of them.

### One stalled read fails the entire review

When Codex abandons the `tools/call` at 30s (the transport error above), the reviewer's entire
accumulated work is discarded and the caller gets `EVIDENCE_UNAVAILABLE`. There is **no retry
anywhere** near evidence calls (the `tools.rs` dispatcher retries only `RATE_LIMITED` between chain
entries). A single contended file read is a total loss of a multi-minute review.

## Goals

1. **A contended read can never hang the evidence request loop.** The whole read operation —
   resolve, open, verify, stat, read, *and* drift stamp — is bounded by a deterministic wall-clock
   deadline that finishes with margin under Codex's 30s ceiling, so our **in-band** `read_timeout`
   result always wins the race against Codex's transport abandon.
2. **The common transient recovers without surfacing any error.** An AV scan or a momentary lock
   that clears within the budget is absorbed by a bounded internal retry, so the reviewer never
   even sees it.
3. **A genuinely unreadable file fails fast and in-band**, as a normal evidence tool error the
   reviewer can retry or route around — never a 30s transport teardown.
4. **No contract moves.** The reviewer's isolation and read-only posture are unchanged (the read
   still runs in-process under the same restricted token), and `parse`'s fail-closed propagation is
   unchanged (f6: the fast in-band error is already survivable; only the abandon was fatal).

Non-goals: raising Codex's 30s ceiling (that lengthens hangs, it does not bound them); parsing or
re-prompting anything; the findings/verdict path (#62/#63); bounding the `list`/`scope`/`tree_stamp`
directory walks and `search`'s own walk (same helper, documented follow-up); and bounding the MCP
output write (f12) — a client that has stopped reading stdout is transport death, correctly fatal,
with nowhere to deliver an in-band error anyway.

## Design

### A. A wall-clock watchdog around the complete blocking read

The blocking work a read performs — resolve the path, confirm it is a file, open, verify the open
handle, stat, read the bytes, and (only when the drift stamp is not already cached) walk the tree —
is a **pure function of its inputs**: none of it needs `&mut Core`. The one stateful step is
*caching* the stamp into `observed_stamp`, which stays on the main thread.

- The request thread spawns a **detached worker thread** running the resolve→`is_file`→open→
  verify→stat→read sequence, and sending back a typed result over a `std::sync::mpsc` channel. **The
  worker returns the materials the response needs — the canonical resolved path and the bytes — not
  bytes alone (f2)**, so the main thread never re-resolves the path (a second unbounded syscall) to
  build the response. (As implemented the drift stamp is *not* in this worker — it is a separate
  bounded main-thread walk after content validation; see the stamp bullet below, impl f3/f4.)
- **The drift stamp is bounded on the main thread, after content validation (as implemented; impl
  findings f3/f4/f2).** The plan originally bounded the stamp inside the file-read worker with an
  `Option` cached-stamp input (f7). The implementation review found that computing it in the read
  worker runs the repository-wide `tree_stamp` walk *before* the main thread's binary/UTF-8/line
  checks — reordering a prompt content error behind a repo-wide walk — and folds `tree_stamp`'s
  unclassified errors into the retry path (impl f3/f2). But simply moving it out unbounded would let
  a first read's stamp still hang the loop (impl f4). So the final shape is two bounded phases: the
  **read worker** bounds the file I/O (resolve → `is_file` → open → verify → stat → read) and
  returns the bytes; the main thread runs the content checks (returning a prompt content error);
  and *then* `current_stamp()` computes the drift stamp through a **single-attempt bounded walk**
  (`run_bounded_stamp` on the watchdog) and caches it in `observed_stamp`. Order and cache semantics
  match the original; both the read and — since `scope` also calls `current_stamp()` — `scope` are
  now fully bounded. `list`/search-base `walk_files` were the deferred sibling follow-up and are
  bounded as of #71.
- The request thread blocks on `recv_timeout(remaining_budget)`.
  - **Completed in time** → the main thread does the cheap CPU-bound post-processing exactly as
    today (UTF-8/binary/line-cap checks, response assembly) and caches the returned stamp into
    `observed_stamp`. No behaviour change on the happy path.
  - **Timed out** → stop waiting. Return a typed `read_timeout` `EvidenceError` (an in-band tool
    error, §C). The worker is **abandoned** (see f3 below for why that is safe and how it is
    capped): it holds only its own local `File` handle and owned `PathBuf`s — no `Core` state, no
    lock — so it cannot block or corrupt any later request. It exits if its syscall ever returns.

Abandoning a thread is deliberate and is the crux of why this is correct where a cooperative
deadline is not: a blocked syscall on Windows std has no portable interruption primitive, so the
only way to *stop waiting* on it is to stop waiting on it. `verify_open_file`'s guarantee is
unaffected — it still runs inside the worker before any bytes are returned, so a path that changes
under the open handle is still rejected; a timed-out read simply returns no bytes.

The generic alternative — bounding the *entire* `Core::call` dispatch on a worker — was considered
and rejected: `Core::call` takes `&mut self` (the `observed_stamp` cache, the `cancel` flag), so a
timed-out, abandoned dispatch would strand that borrowed state and the *next* request would
deadlock on it, re-creating the very hang this fixes. Bounding the pure read, which borrows nothing
shared, avoids that trap — and is why the worker takes owned inputs and returns owned outputs.

### f3 — abandoned workers are bounded by an explicit cap, not the job object

The earlier draft claimed abandoned workers are "reaped when the evidence process exits under the
job object." **That is wrong, and the review was right to flag it.** The reviewer job object is
assigned to the *direct reviewer child*; the evidence server (our binary, re-invoked by Codex over
stdio) runs **outside** that job (Phase 0 observed exactly this — see
`docs/codex-reviewer-evidence-phase0.md`). So a timed-out worker lives until its syscall returns or
**the evidence process itself exits** at review end — and with two attempts per read and a 96-call
budget (`Limits::max_calls`), a pathological run could strand on the order of ~192 blocked threads,
each holding a file handle and a stack.

So accumulation is capped explicitly rather than assumed away. The service holds an atomic counter
of **outstanding abandoned read workers**. When a read times out, the worker is abandoned and the
counter increments; when an abandoned worker's syscall finally returns and it exits, it decrements.
Past a small hard cap (a handful — e.g. 8), `Core::read` **stops spawning** and returns a
deterministic `read_unavailable` in-band error immediately, and the internal retry (§B) is skipped
whenever a fresh abandon would breach the cap. This converts unbounded thread growth into a
bounded, deterministic, fail-closed refusal — which, being in-band, still does not tear down the
review; it just tells the reviewer this read cannot be served right now. (The heavier alternative —
a per-read killable helper *process* under a service-owned job, so a stalled read can be
`TerminateProcess`d — is noted for the reviewer but not proposed: the cap bounds the resource with
far less machinery, and the evidence process lifetime already bounds the absolute worst case.)

### B. One bounded internal retry, over a shared total deadline

On a retryable failure the request thread retries the read **once**, a fresh worker after a short
fixed backoff, so the AV-scan/lock transient the issue describes clears without the reviewer ever
seeing an error — the highest-value part of the fix.

**Retryable means more than the stall (f5).** A contended file often surfaces not as a hang but as
a *fast* OS error — Windows `ERROR_SHARING_VIOLATION` (32) or `ERROR_LOCK_VIOLATION` (33) from
`File::open` — which the retry must also catch. But the classification has to happen **before the
error is flattened (f10).** Today `read_bounded` converts every `io::Error` straight to a public
`EvidenceError` with code `read_failed` (`src/evidence/core.rs:741-764`), and `EvidenceError`
carries only a `code`/`message` (`src/evidence.rs:225-246`) — so by the time the request thread
holds one, the raw OS code (32/33) is gone and no predicate over `EvidenceError` can recover it. So
the worker inspects `io::Error::raw_os_error()` **at the point of failure** and returns an internal
typed distinction (a private `ReadFailure { retryable: bool, .. }`, or a `read_timeout`-style code
chosen there) rather than a bare flattened error. **The classification must span every blocking
stage of the worker, not only `read_bounded` (f14).** A sharing/lock violation can surface at the
resolve, `is_file`, or `verify_open_file` stage too, and each currently erases the raw error before
the request thread sees it — `resolve_existing` maps failures to `not_found`
(`src/evidence/core.rs:299-305`, `:542-557`), `Path::is_file` discards the metadata error, and
`verify_open_file` maps to `read_failed` (`:774-805`). So the worker preserves `raw_os_error()`
through the whole resolve→`is_file`→open→verify→stat→read sequence and classifies retryability
uniformly at each stage; the retry predicate reads that — the watchdog timeout **plus** the
classified sharing/lock cases wherever they arise — and everything else (not-found, too-large,
binary, path-escape, non-UTF-8) is returned immediately, unretried. The *public* `EvidenceError`
the reviewer eventually sees is unchanged; the raw-code carrier is internal to the read helper.

**The retry is budgeted against the ceiling under one monotonic deadline (f4).** Two independent
15s attempts plus a backoff would exceed 30s and hand the race back to Codex — the exact failure
this removes. So:

- **One shared constant is the single source of truth for the ceiling.** Codex's
  `tool_timeout_sec` (`src/reviewer/codex.rs:182-185`) and the evidence read budget are both
  derived from it, so they cannot drift into inversion. The read budget is `ceiling − margin`,
  where the margin covers worker creation, the backoff, the post-worker CPU work, and the
  *expected* MCP response serialisation under a draining client. The margin explicitly does **not**
  claim to bound a stalled output write (f12, below) — a client that has stopped reading stdout is
  transport death, not a slow read.
- **One monotonic total deadline governs the whole read**, measured from **request receipt (f8;
  as implemented, impl f1)**. Requests are read off stdio by a reader thread and handed to the
  serial dispatcher through a **bounded** channel (impl f8: bounded for memory safety — an unbounded
  queue lets a pipelining client grow it until the process OOMs). The receipt `Instant` is stamped
  **when the request is read off the wire** (in `read_requests`) and threaded through the channel →
  `handle` → `Core::call_with_receipt`, so the budget origin includes any wait in that channel
  before dispatch (`received_at.elapsed()` keeps growing, shrinking the remaining budget). (The plan
  first captured it at `Core::call` entry, which the code review corrected — that point is *after*
  the channel wait.) **The explicit ingress/receipt contract (impl f6/f8):** the bounded channel
  backpressures the client through the OS pipe when full; the one residual is a request that sat in
  the pipe *un-read* while the reader was blocked on a full queue — its pre-read wait is not in its
  budget. That needs a client pipelining enough concurrent requests to saturate the queue *and*
  stall the dispatcher; the real MCP client issues evidence calls serially and never does, and the
  residual is the same unobservable class as raw transport latency (we cannot stamp a request before
  we read it), covered by the margin. Attempt 1 waits up to the per-attempt cap or the remaining
  budget, whichever is smaller, and a retry runs **only if** the remaining budget (after backoff)
  still allows a meaningful attempt, bounded by that remainder — not a fresh cap.
- **The end-to-end claim is scoped honestly, not overstated (f8, f12, f13).** State precisely what
  is proved and what is not, because three distinct things sit outside the read's own bound and the
  earlier drafts blurred them:
  - **What is bounded:** the *processing* of a `repository_read` — and the per-file read
    `repository_search` performs — from **request receipt** through resolve→`is_file`→open→verify→
    stat→read→(stamp), across every blocking stage, within the receipt-anchored budget.
  - **Not bounded — the output write (f12).** After the read produces its result, delivering it is
    a synchronous `writer.write_all` + `flush` (`src/evidence.rs:596-600`) with no deadline. If
    Codex has stopped draining stdout, that write blocks regardless of how fast the read was. This
    is **out of scope on purpose**: an unread stdout is genuine transport death — the same class as
    the process dying — and it *should* fail the review; you cannot deliver an in-band error to a
    client that is not reading. So the guarantee is "the read produces its result within budget,"
    not "the result reaches a client that has stopped listening." (Bounding the write would not
    help — there is nowhere for the bytes to go — so it is named and excluded, not attempted.)
  - **Bounded since #71 — the remaining sibling walks (f13).** This was the documented follow-up,
    and it was not optional in the end: with only the read path bounded, two of three reviews on
    this repository still died the #61 death, because the budget bounded one *stage* rather than the
    *request*. Issue #71 closed it — `repository_list`'s enumeration and `repository_search`'s
    `resolve_existing`/`walk_files` now run on the same watchdog (their own worker pool), the
    cooperative `deadline()` takes the tighter of the per-operation timeout and the request budget,
    the Git evidence commands derive their child timeout from the request's receipt instant rather
    than starting a fresh one, and a request whose budget is already spent at dispatch is refused
    in-band. See that issue and the change on `fix/evidence-operation-deadlines`.
  So the guarantee this document describes was, as written, **`repository_read` and
  `repository_scope` bounded from receipt** (file I/O and the drift stamp) plus `search`'s per-file
  reads. #71 widened it to every operation. What remains excluded is the output path above: it is
  explicitly **not** "the review can never see a transport abandon from any cause".
- **The coupling is asserted, not just intended.** A `debug_assert` / `Limits::validate()` clause
  rejects any configuration where the derived read budget plus its margin is not safely below the
  emitted `tool_timeout_sec`. If a serialized `read_budget_ms` field is added to `Limits`, the
  bundle schema version and its `validate()` gain the field and the guard.

Concrete starting numbers, open to the reviewer: ceiling 30s, total read budget 20s, per-attempt
cap ~9s, backoff ~0.5s — worst case two attempts return their typed error near ~18.5s from receipt,
comfortably inside 30s with room for queue wait and post-processing.

### C. Fast, typed, in-band failure

When the budget is exhausted (or the cap is hit), the read returns an `EvidenceError` with a stable
code (`read_timeout` / `read_unavailable`) naming the file and the budget, exactly like the
existing `read_failed` codes. It travels through **two layers that must not be conflated (f11)**:

- **The MCP-response layer** — `handle` serialises the `EvidenceError` into a JSON-RPC *result*
  carrying `"isError": true` (`src/evidence.rs:641-648`). That is all our code emits; it does **not**
  set any `error`/`status` field — those do not exist at this layer.
- **The Codex-event layer** — when Codex records that completed tool call, *it* emits an
  `item.completed` whose `mcp_tool_call` has top-level `"error": null` and `"status": "completed"`
  (an `is_error` result is not a transport failure). `parse_events` keys **only** on a non-null
  top-level `error` (`src/reviewer/codex.rs:691-704`), so this call never enters
  `evidence_infrastructure_errors`, mirroring `model_argument_error_is_not_misread_as_service_death`
  (`src/reviewer/codex.rs:1202-1213`).

So a timely `read_timeout` is an ordinary tool error the reviewer sees and can retry or route
around, and the review is not lost — but the plan is careful to attribute each field to the layer
that actually produces it, rather than claiming `handle` emits Codex's event fields.

### D. Propagation stays fail-closed and unchanged — and that is now provably correct (f6)

The earlier draft treated `parse` propagation as a debatable open question ("should a post-retry
read failure still fail the whole review?"). The first review closed it: **no change to `parse` is
needed or wanted.** The only evidence outcome that fails a review is a **top-level transport
error**, and the sole such shape here was Codex's 30s abandon — which §A–C eliminate by returning
in-band before it fires. A timely `read_timeout` / `read_unavailable` is an in-band tool error that
`parse` already tolerates (test `model_argument_error_is_not_misread_as_service_death`). So the
fail-closed contract is preserved verbatim: a genuine service death (a real transport error, the
process dying) still fails the review, as it should; a bounded, recovered-or-fast-failed read does
not. This is documented as a contract and locked by a new test (below), rather than left implicit.

## Blast radius (as implemented)

- `src/evidence/core.rs` — the substance. The **file-read worker** (`read_job`) is a pure,
  owned-in/owned-out `resolve→is_file→open→verify→stat→read` sequence returning `(resolved, bytes)`,
  with an internal raw-error classifier spanning **every blocking stage**, not just the byte read
  (f14/impl f2). It runs under a generic watchdog (`bounded_attempts<T>`) with the budgeted retry,
  the outstanding-worker atomic counter and cap, `Builder::spawn` (impl f5), and the `read_timeout`
  / `read_unavailable` codes. The **drift-stamp walk** runs separately, on the main thread *after*
  content validation, through a single-attempt bounded walk (`run_bounded_stamp` via
  `current_stamp`) so `read` and `scope` are both bounded without the stamp preceding content errors
  (impl f3/f4). `search`'s per-file reads route through the same file-read watchdog (f9/f13); its
  own walk and `list`'s were the deferred follow-up and are bounded as of #71.
- `src/evidence.rs` — the shared `CODEX_TOOL_TIMEOUT_SECS` ceiling constant (also emitted as
  `tool_timeout_sec` from `src/reviewer/codex.rs`); the receipt `Instant` stamped in `read_requests`
  and threaded through the **bounded** dispatch channel → `serve_requests` → `handle` →
  `Core::call_with_receipt` (impl f1/f6). The read budget lives as `core.rs` constants derived from
  the ceiling with `const _` coupling assertions, not a serialized `Limits` field — so no bundle
  `SCHEMA_VERSION` bump is needed.
- `src/reviewer/codex.rs` — emit `tool_timeout_sec` from the shared constant; `parse` is
  deliberately untouched (a timely in-band `read_timeout` is survivable — Decision D/f6-plan).
- Tests as below.

Per this repository's stated priority, foundational completeness is preferred over minimising the
change — and here that means bounding the *whole* read path (f1) with a *capped* worker pool (f3)
under a *coupled* deadline (f4), not a narrower patch. Widening into list/scope/walk and into a
killable helper process is scoped out on purpose, with the reusable helper and the process-cap
alternative both named so the follow-up is deliberate, not forgotten.

## Testing

- **Unit — the watchdog fires, and strands nothing.** Inject a read whose worker blocks past the
  budget (a seam that sleeps) and assert `Core::read` returns `read_timeout` at ~the budget, and
  that a *subsequent* read on the same `Core` still succeeds — proving the abandoned worker strands
  no shared state (the property that rules out the generic `&mut self` approach).
- **Unit — the retry recovers, silently, at any stage (f5, f14).** A read that fails transiently on
  attempt 1 and succeeds on attempt 2 returns bytes with **no** error surfaced, within the total
  budget — tested for a stall and for a simulated `ERROR_SHARING_VIOLATION` raised **at more than
  one stage** (open *and* verify), proving the classifier spans the whole worker, not just
  `read_bounded`. A non-transient error (not-found) is returned immediately, **unretried**.
- **Unit — the cache is preserved, and the stamp walk is bounded (impl f4).** A first
  `repository_read` (no prior scope) populates `observed_stamp` so a later `scope` returns the same
  cached value — proving `read` participates in the same cache via the unchanged `current_stamp()`.
  The drift-stamp walk runs under `STAMP_WATCHDOG` (single attempt), so a stalled `tree_stamp` is
  bounded like a read; the watchdog-fires unit test above covers the bounding mechanism generically
  (it now applies to any `bounded_attempts<T>` output, reads and the stamp alike).
- **Unit — `search`'s per-file read is bounded (f9).** A `repository_search` whose candidate-file
  read stalls returns through the same watchdog rather than hanging the request loop, proving the
  shared helper covers `search`, not only `read`.
- **Unit — worker cap (f3).** With the abandoned-worker counter at the cap, `Core::read` returns
  `read_unavailable` immediately without spawning, and the retry is skipped; below the cap it
  spawns normally. Asserts bounded accumulation.
- **Unit — budget vs ceiling coupling (f4).** Assert the read budget and `tool_timeout_sec` derive
  from the one shared constant, that the worst-case two-attempt total is `< tool_timeout_sec`, and
  that `Limits::validate()` rejects a config that inverts the two.
- **Unit — the response contract (f2).** The worker-returned resolved path yields the identical
  `path`/`bytes`/`sha256`/`total_lines` response shape as today for a normal small file, with no
  second resolution.
- **Unit — `verify_open_file` still guards.** A path that changes under the open handle inside the
  worker still returns `path_changed`/`path_escape`, not bytes.
- **Unit — propagation contract, per layer (f6, f11).** Two separate tests, one per layer, so the
  MCP response and the Codex event shape are not conflated:
  - *MCP-response layer* — `handle` serialising a `read_timeout` `EvidenceError` yields a JSON-RPC
    result with `isError: true` (and no top-level `error`/`status`), alongside the existing
    `handle` response-shape assertions (`src/evidence.rs` tests near `:1024`).
  - *Codex-event layer* — an event stream whose evidence `mcp_tool_call` completed with top-level
    `error: null` / `status: completed` (the in-band shape), followed by an `agent_message`, does
    **not** yield `EVIDENCE_UNAVAILABLE` (mirroring
    `model_argument_error_is_not_misread_as_service_death`), while a top-level transport `error`
    still does (mirroring `evidence_transport_error_invalidates_an_otherwise_completed_review`).
- **`smoke.ps1 -Reviewer codex`** — a real end-to-end round trip still passes (this touches the
  read path the reviewer exercises constantly). Real model call; note the cost. CI's
  `CLI_NOT_FOUND` contract is untouched.

## Open questions for the reviewer

1. **Budget numbers.** Is ceiling 30s / total 20s / per-attempt ~9s / backoff ~0.5s the right
   shape, given the invariant is `worst-case total < tool_timeout_sec` under one monotonic
   deadline? Or should the first attempt claim most of a single total budget and retry only on the
   remainder?
2. **Worker cap value and refusal (f3).** Is a small hard cap (~8) with a deterministic in-band
   `read_unavailable` refusal the right bound, or is the heavier per-read killable helper process
   (service-owned job, `TerminateProcess`) worth its machinery for a stronger guarantee?
3. **Retryable error set (f5).** Is retrying `ERROR_SHARING_VIOLATION`/`ERROR_LOCK_VIOLATION`
   alongside the stall correct, or should the retry stay stall-only and the recovery claim be
   narrowed to stalls?
4. **Sibling scope (f8, f9).** As implemented this change bounds `read`, `scope` (both via the
   watchdog-bounded stamp), and `search`'s per-file reads. The still-unbounded `list` and
   search-base `walk_files` directory walks are left as documented follow-up — which is also what
   keeps the end-to-end "no call ever hits the abandon" claim honestly scoped.
   Is that the right cut, or should the directory-walk syscalls be wrapped in the same helper now so
   the stronger end-to-end guarantee holds in this change rather than after it?

   **Answered by events (#71).** It was not the right cut. The narrow scope was honestly *stated*,
   but a review does not care which stage held the loop, and two of three reviews on this repository
   still died the same death with this change shipped. Bounding a stage is not bounding a request.
   The follow-up was done as its own issue rather than in this one, which was the correct order —
   but "documented follow-up" turned out to mean "the bug is still there and now has a footnote".
