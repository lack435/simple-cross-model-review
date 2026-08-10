# Single blocking collect — design

Status: **plan, under cross-model review** (see [Review history](#review-history)).

Addresses [#39](../../issues/39): `cross_model_review_result` caps `wait_seconds` at 300, but
reviews in this project routinely run 5–25 minutes, so every review forces the caller into a
manual polling loop. One 25-minute review in a recent dogfooding session needed six
`cross_model_review_result` calls in a row; thirteen review rounds produced ~40 no-op
round-trips whose only purpose was to keep waiting.

The goal: **a ~20-minute review is started and collected with one blocking call**, and polling —
when it still happens — degrades gracefully instead of destroying the review.

## Why the cap is 300 today, and why that is the real problem

The `wait_seconds` cap is not an arbitrary throttle. It is load-bearing for a *safety*
property, and that coupling is what this change has to break before the cap can move.

`cross_model_review_result` currently binds the review to the polling request
([`tools.rs:387`](../src/tools.rs), `request.attach_review(&id)`). A
`notifications/cancelled` for that request therefore **kills the running review**
([`mcp.rs:440`](../src/mcp.rs), `app.cancel_review`). The README states the consequence
plainly:

> the client's tool timeout must exceed `MAX_WAIT_SECS` (300s), or a client giving up on a
> poll will destroy a review that was still coming.

A client does not only send `notifications/cancelled` when a user cancels. It also sends it
automatically when its own MCP **tool timeout** fires. The server cannot tell the two apart.
So the cap has to sit comfortably below the client tool timeout, or an ordinary long wait
trips the client timeout and the auto-cancellation destroys a review that was still running.
The example configs pin the client timeout at 600s / 400s, so the cap must stay well under
that — hence 300.

Raising the cap to the review budget (1800s) under the *current* cancellation semantics would
require every client to pin a tool timeout above 1800s **and** would widen the window in which
an accidental client-side timeout destroys an in-flight review from a few minutes to half an
hour. That is the wrong direction. The cap is a symptom; the destructive coupling is the
disease.

## The fix: decouple "stop waiting" from "cancel the review"

The load-bearing change is to make abandoning a `cross_model_review_result` poll **non-destructive**.
Three lifecycle events, three distinct behaviours:

| Event | Today | After |
| --- | --- | --- |
| `notifications/cancelled` on a **`cross_model_review`** (start) call | kills the review | **kills the review** (unchanged) |
| `notifications/cancelled` on a **`cross_model_review_result`** (poll) call | kills the review | **detaches the poll; review keeps running and stays collectible by `review_id`** |
| `cross_model_review_cancel` | kills the review | kills the review (unchanged) |

The asymmetry between the two `notifications/cancelled` rows is already justified in the code
and README, it is just not currently *acted on*:

- A cancelled **start** call is unarguable — the `review_id` was never delivered to the caller,
  so nobody can ever collect the review. Killing it is the only sane outcome, and it stays.
- A cancelled **poll** is "the real trade" (README's words). The caller *does* hold the
  `review_id` and *can* come back for it. The current code guesses "won't come back" and kills.
  That guess is wrong in exactly the flow #39 is about: the agent fully intends to collect, it
  was just cut off by a client-side timeout. So the poll cancellation should stop the *wait*,
  not the *review*.

Once a poll cancellation no longer destroys the review, the invariant "client tool timeout must
exceed the cap or you lose the review" disappears. A client timeout shorter than the wait then
degrades to polling — the caller re-issues `cross_model_review_result` and resumes waiting —
rather than destroying anything. That is what makes raising the cap safe.

### What this trades, and why it is acceptable

The README's original argument for kill-on-abandon: a review nobody waits on bills against its
whole timeout budget and holds its session lease for that whole time. That cost is real, and it
does not vanish. For a **single** detached review it is bounded and mitigated:

- **The budget caps the spend.** A detached review runs at most `--timeout-seconds` (default
  30 min) before the server stops it. It cannot bill unbounded.
- **The session lease releases when the review finishes**, not when it is collected. A detached
  review that runs to completion frees its session; a later `cross_model_review` on that session
  is refused as `SESSION_BUSY` only while the review is genuinely still running — which is
  correct, since you should not start a second review of a session mid-review anyway.
- **Explicit cancellation still exists.** An agent that truly wants to stop the spend calls
  `cross_model_review_cancel`. The kill path is not removed; it is moved to the tool whose *job*
  is to kill, and off the tool whose job is to wait.
- **The raised cap removes most reasons to abandon a poll at all.** The dominant flow becomes one
  blocking call that returns the finished review. Abandonment stops being routine.

The net for one review: we give up an automatic cost-saving guess that fired mostly on false
positives (client timeouts, not real user cancellations), and in exchange the main flow stops
being able to destroy its own review.

#### The per-review bound is not a server-wide bound

Kill-on-abandon was also, incidentally, a *server-wide* limiter: because abandoning a poll killed
the review, a caller that started reviews and walked away could not accumulate them. Removing it
removes that limiter too. `Registry::try_start` refuses only a **second review of the same
session** ([`registry.rs`](../src/registry.rs), `find(|r| r.session == session && r.status ==
Running)`); different session names run concurrently, running reviews are never evicted, and the
terminal-retention caps bound only *finished* reviews' text, not live reviewer processes, worker
threads, session leases, or prompt buffers. So under the new semantics a caller that invents a
fresh session name each time and detaches every poll can accumulate arbitrarily many full-budget
reviews at once. That is a real exposure this change introduces, and "bounded by the budget" is
true only per review.

So this PR adds an explicit **global cap on concurrently-running reviews** as the backstop:

- A new `--max-concurrent-reviews <n>` config knob (default a small figure — proposed **8** — that
  no legitimate flow reaches; a serial dogfooding session runs one at a time).
- `Registry::try_start` gains a third refusal: alongside `Busy(existing)` and `ShuttingDown`, a
  `TooManyRunning { limit }` returned when the count of `Status::Running` reviews is already at the
  cap, mapped to a plain agent-facing error (`errors::too_many_running`) that tells the caller to
  collect or cancel an outstanding review rather than escalating — the same class as `SESSION_BUSY`,
  since it is the agent's own call to make.
- The check is inside `try_start`, under the same state lock that inserts the review, so two
  concurrent starts cannot both slip past a cap of *n* into *n+1*.

What this deliberately does **not** bound: the count of persisted **session records**
([`session.rs`](../src/session.rs) `record_turn` writes one per distinct session name, uncapped
today). A detached review that runs to completion calls `record_turn` just as any completed review
does, so a caller minting unique session names grows that map — but that is the pre-existing shape
of the feature (every finished review persists its session), not something decoupling changed, and
the live-resource cap above is what stops the dangerous accumulation (processes and threads). The
plan states this rather than claiming a fix it does not make.

## The cap change

With the coupling broken, `MAX_WAIT_SECS` is raised to track the review budget. The value must
cover the review's **whole lifecycle**, not just the reviewer's turn — a subtle point the first
draft got wrong:

- **`cfg.timeout` alone is not an upper bound on time-to-terminal-state.** The reviewer turn is
  bounded by `--timeout-seconds`, but the worker also spends time *before* and *after* that clock:
  - **before** — capturing the change runs under a separate 60s `CAPTURE_BUDGET`
    ([`vcs/shared.rs`](../src/vcs/shared.rs)), which elapses before the reviewer timeout even starts;
  - **after** — when the reviewer times out, there is still the bounded output-drain grace, then
    parsing the transcript and persisting the session record, all before `Registry::finish` flips
    the review to a terminal state ([`tools.rs`](../src/tools.rs)).

  So a collect issued the instant a review starts, waiting exactly `cfg.timeout`, can still return
  `running` at the boundary. The cap has to account for the full path or it fails the very
  "one blocking call catches the result" property it exists to provide.
- **Effective cap = `CAPTURE_BUDGET + cfg.timeout + FINALIZATION_GRACE`.** `FINALIZATION_GRACE` is
  a single new named constant (proposed ~30s) covering output drain + transcript parse + session
  persistence — the bounded tail after the reviewer clock stops. Computed with saturating
  arithmetic so a pathological `--timeout-seconds` near `u64::MAX` cannot overflow. This is the
  maximum wall-clock from review start to terminal state, so a single blocking call issued at or
  after start reliably observes the terminal result (a completed review, or a `TIMEOUT` failure)
  rather than a `running` snapshot.
- **Derived, not a fixed 1800.** If an operator sets `--timeout-seconds 3600`, the cap tracks it.
  The cap follows the configured budget instead of a second hard-coded number that would silently
  disagree with it.
- **Default `wait_seconds` blocks to completion.** When `wait_seconds` is omitted, the call waits
  the full effective cap. The ergonomic no-argument path — `cross_model_review_result` with just a
  `review_id` — becomes the one blocking call the issue asks for. A caller that wants a quick "is
  it done yet?" peek passes `wait_seconds: 0`, which returns the current snapshot immediately.
- **The tool schema reflects the real cap.** `inputSchema.wait_seconds.maximum` and its description
  are already built from `cfg` at `tools/list` time ([`mcp.rs:782`](../src/mcp.rs)), so they will
  report the configured budget rather than a stale `300`.

## Sequencing (why the two changes ship together)

The cap change is only safe *after* the decoupling. If the cap rose first, a client with an
unchanged short tool timeout would auto-cancel a long wait and — under the old semantics — destroy
the review. So the decoupling lands with, or before, the cap. They are one PR here because each
half is incomplete without the other: decoupling alone leaves the ergonomic tax in place; raising
the cap alone reintroduces the destruction hazard the README warns about.

## Concrete implementation

### 1. `RequestCancel` learns ownership (`src/cancel.rs`)

Today `cancel()` returns `Option<String>` — the review to kill, or `None`. That conflates "a
start call that owns the review" with "a poll that is merely waiting." Replace the review binding
with an explicit intent so the cancellation path can tell them apart:

- `attach_owned(review_id)` — used by the **start** call. A cancellation kills this review.
- `attach_wait(review_id)` — used by the **poll** call. A cancellation detaches only.
- `cancel()` returns an enum: `Kill(review_id)` | `Detach` | `Nothing`.
  - `Kill` → the start-call case (and the mid-setup attach race). `handle_cancellation` kills.
  - `Detach` → the poll case. `handle_cancellation` does **not** kill; it wakes the parked wait
    so the handler thread returns promptly instead of parking out the (now much larger) budget.
  - `Nothing` → no review bound, or a response already committed. As today.

The `responded`/`try_claim_response` race machinery is unchanged: the ownership discriminator lives
in the same `State` behind the same mutex as `cancelled`/`responded`/`review_id`, and the
pending-map removal ordering in `handle_cancellation` is untouched, so response suppression still
works identically for both attach kinds. Only the *kill decision* becomes conditional on ownership —
which is what the reviewer confirmed is safe provided all attach, cancel, and response-claim
operations stay under that one mutex.

### 2. The parked wait wakes on its own cancellation (`src/registry.rs`, `src/tools.rs`)

Today a poll cancellation kills the review, the review reaches a terminal state, the condvar
wakes, and `Registry::wait` returns. If we stop killing the review, nothing wakes the parked
`wait` — it would sit until `wait_seconds` (up to the full budget) elapses. That is a lingering
handler thread per abandoned poll, which in a heavy session accumulates.

So `Registry::wait` gains awareness of the caller's cancellation, mirroring how it already
re-checks `shutdown`. **The wake must be published under the registry state lock**, exactly as
`begin_shutdown` is — a bare `changed.notify_all()` has a lost-notification race (a waiter that
read its flag as clear but has not yet reached `wait_timeout` still holds no barrier against the
notify, so the wake lands before it joins the wait set and is lost). `begin_shutdown` already
documents this and sets `shutdown` inside `State` before notifying for precisely that reason.

- `wait` takes the request's cancellation handle (the `Arc<RequestCancel>`), and each iteration —
  **while holding the state lock** — re-reads its cancelled flag alongside `shutdown` and the
  terminal-state test, returning immediately when set. Whatever snapshot it returns is discarded:
  the handler's `try_claim_response()` loses to the cancellation and sends nothing.
- `handle_cancellation`, on `Detach`, calls `registry.wake()`, which **acquires the state lock,
  drops it, then `notify_all()`** — the `begin_shutdown` spelling. `notify_all` wakes every waiter;
  each re-checks *its own* flag under the lock, so only the cancelled one returns and the rest
  re-park.
- **The happens-before that makes reading a foreign flag safe:** `handle_cancellation` calls
  `entry.cancel()` (which sets `RequestCancel.cancelled` under *its* mutex) *before* it calls
  `registry.wake()` (which takes the state lock). The state lock is the barrier — a waiter cannot
  be between "checked the flag clear" and "parked" without holding it, and `wake()` cannot notify
  without acquiring it — so either the waiter parks first and the notify wakes it, or `wake()` runs
  first and the waiter's next check under the lock sees the already-set flag. This is the same
  argument `begin_shutdown` relies on, with the flag set one lock earlier.
- **Lock ordering:** the waiter acquires `state` then reads `RequestCancel` (nested); no other path
  takes them in the reverse order (`cancel`/`attach`/`try_claim_response` touch only
  `RequestCancel`; `wake` touches only `state`), so there is no deadlock. This ordering is stated
  in the code so a later change cannot silently invert it.

Shutdown behaviour is unchanged: `begin_shutdown` already wakes parked waits, so a park up to the
new (larger) cap is still released the instant stdin closes.

### 3. The cap and the concurrency backstop (`src/config.rs`, `src/registry.rs`, `src/tools.rs`, `src/mcp.rs`, `src/errors.rs`)

- `src/config.rs`: replace the fixed `MAX_WAIT_SECS = 300`. Introduce a derivation
  `Config::max_wait()` = `CAPTURE_BUDGET + timeout + FINALIZATION_GRACE`, saturating (see the cap
  section above for why all three terms are needed). `FINALIZATION_GRACE` is a new named constant.
  Keep `DEFAULT_WAIT_SECS`'s *meaning* as "block to completion" — i.e. an omitted `wait_seconds`
  resolves to `max_wait()`.
- `src/config.rs`: add `--max-concurrent-reviews <n>` (default 8), parsed like the other numeric
  flags, `0` disables the check (consistent with `--session-max-turns`/`--session-max-idle-seconds`).
- `src/registry.rs`: `try_start` counts `Status::Running` reviews under the state lock and returns
  the new `StartRefused::TooManyRunning { limit }` when at the cap. `src/errors.rs`: a plain
  `too_many_running` correction (not an escalation code), telling the caller to collect or cancel an
  outstanding review. `src/tools.rs`/`src/mcp.rs`: map it on the start path next to `Busy`/`ShuttingDown`.
- `src/tools.rs` `review_result`: `wait = args.wait_seconds.unwrap_or(max_wait()).min(max_wait())`.
- `src/tools.rs` `render_start`: the "Use wait_seconds=300; if it returns status=running, call it
  again" guidance becomes "collect it with one call; it blocks until the review is done."
- `src/mcp.rs` tool schema + `cross_model_review_result` description: report the derived cap and
  describe the single blocking call. The "call it again with the same review_id" line stays as the
  *fallback* for a client whose tool timeout is shorter than the wait, not as the expected path.

### 4. Example configs and README

- `.mcp.json` and `examples/claude-code-reviewed-by-codex/.mcp.json`: raise `timeout` from
  `600000` to comfortably above the 1800s cap (e.g. `1920000` = 32 min) so the single blocking call
  completes in one round-trip rather than being cut into two polls.
- `.codex/config.toml` and `examples/codex-reviewed-by-claude/.codex/config.toml`: raise
  `tool_timeout_sec` from `400` to ~`1920`, and rewrite the comment. The old comment justifies the
  floor with "the server stops the reviewer, so a client that gives up first would discard a review
  that was still coming." After this change a client that gives up first does **not** discard the
  review — it detaches. The new reasoning: keep the timeout above the cap so a single call
  *completes*; below it, you degrade to polling, you do not lose work.
- README:
  - Tools table / async section: describe the single blocking call; progress notifications
    unchanged as the liveness signal (the issue explicitly wants them unchanged).
  - The "A cancelled request cancels its review" design note: rewrite to the three-way table above.
    Remove the "client tool timeout must exceed `MAX_WAIT_SECS` or destroy the review" warning;
    replace with the non-destructive-degradation invariant.
  - **State the MCP spec deviation explicitly.** MCP's cancellation utility says a receiver SHOULD
    stop processing and *free the associated resources*; this design deliberately keeps the reviewer
    process and session lease alive on a `_result` cancellation and frees only the poll handler.
    That is a defensible SHOULD-level exception, but it must be named where a reader would otherwise
    assume conformance: the README design note **and** the `cross_model_review_result` /
    `cross_model_review_cancel` tool descriptions say plainly that cancelling a poll detaches only
    the wait, and that `cross_model_review_cancel` is the operation that frees the reviewer.
  - Add `--max-concurrent-reviews` to the Configuration table, noting the default and that `0`
    disables it, alongside the other resume/limit knobs.
  - The example-config READMEs (`examples/*/README.md`) carry the same warning — update both.
- `AGENTS.md`: the dogfooding steps describe a poll loop; note that a single collect call now
  blocks to completion, re-reviews included.

### 5. Tests

- `src/cancel.rs`: rework the unit tests for the `attach_owned` / `attach_wait` / `cancel()`-enum
  split. Add: an owned attach yields `Kill`; a wait attach yields `Detach`; the response-suppression
  race is unchanged for both.
- `src/tools.rs`:
  - `wait_seconds_is_capped` — the cap is now derived from `cfg.timeout`; assert against
    `cfg.max_wait()` rather than the literal 300, and assert an omitted `wait_seconds` resolves to
    the cap (blocks to completion).
  - New: a poll cancellation leaves the review `Running`/collectible (not `Failed`/`CANCELLED`);
    a start cancellation still finishes it `CANCELLED`.
- `src/registry.rs`: new test — a parked `wait` returns promptly when its request is cancelled,
  **without the review leaving `Running`**, and other waiters on the same review are not disturbed
  (they re-park). Use the existing `parks`-counter barrier to force the cancellation into the
  window *between* the flag check and parking (the reviewer's finding-1 test), so a lost-wakeup
  regression fails the test rather than passing on timing luck.
- `smoke.ps1` — **contract change, called out loudly.** Step 7 today cancels a `_result` poll and
  asserts the review is `CANCELLED`. Under the new semantics a `_result` cancellation must leave the
  review *alive*, so the assertion is split across **two independent reviews** — reusing one review
  for both halves is nondeterministic, since the poll-cancellation review may finish naturally
  before the explicit cancel and would then report `completed`, not `CANCELLED`:
  1. **Review A** — start it, poll, cancel the poll → assert the poll is never answered (unchanged)
     **and** a fresh collect returns the review still running *or* completed and collectible (i.e.
     not `CANCELLED`, not gone). This proves poll-cancellation is non-destructive.
  2. **Review B** — a separate review, still running → call `cross_model_review_cancel` explicitly →
     assert it is now `CANCELLED` and the reviewer is dead. This is the "cancellation stops the
     reviewer" assertion; it moves to the tool that actually owns killing.

## What #39 asks for that this does not do

The issue's preferred option 2 is "truly async collection … completion delivered via a
background-task/push notification, so the agent does other work and is woken when it lands."

**The server cannot deliver a server-initiated result push under the MCP versions it speaks**
(`2025-06-18` and earlier, per [`mcp.rs:21`](../src/mcp.rs)). MCP has no server→client "here is your
finished tool result, resume the turn" message outside an open request. The "background task /
push" the issue names is a property of the *client harness* (the Agent SDK), not something this
stdio JSON-RPC server can initiate. Claiming otherwise would be a capability the protocol does not
have.

What the server *can* do — and does here — is make the single blocking call the norm, so the agent
issues one `cross_model_review_result` and gets the finished review back, with progress
notifications proving liveness throughout. That satisfies acceptance criterion 1 ("one blocking
call") directly, and leaves criterion 2 ("zero polling via a completion notification") to the
client, which is the only layer that can implement it. The plan states this rather than pretending
the server can wake the agent on its own.

## Acceptance mapping

- **"A ~20-minute review can be started and collected with one blocking call"** — yes: raised cap +
  default-to-completion + updated client timeouts. One `cross_model_review`, one
  `cross_model_review_result`.
- **"…or with zero polling via a completion notification"** — out of the server's reach under the
  MCP versions it speaks; documented as a client-harness capability, not silently dropped.
- **"Progress notifications during the wait are unchanged"** — untouched. The 30s
  `notifications/progress` cadence and its snapshot content are not modified.

## Review history

- **Round 1** (Codex, gpt-5.6-luna, effort=max) — REQUEST CHANGES. Five findings, all accepted:
  1. *major* — the detach wake had a lost-notification race; a bare `notify_all()` (or an
     `AtomicBool`) is insufficient. Fixed: `wake()` publishes under the state lock like
     `begin_shutdown`, with the happens-before and lock ordering now spelled out, plus a
     `parks`-barrier test.
  2. *major* — `cfg.timeout + grace` is not a bound on time-to-terminal-state; capture (60s
     `CAPTURE_BUDGET`) precedes the reviewer clock and parse/persist follow it. Fixed: cap =
     `CAPTURE_BUDGET + cfg.timeout + FINALIZATION_GRACE`, saturating.
  3. *major* — the "bounded" trade was only per-review; decoupling lets a caller accumulate
     unbounded concurrent full-budget reviews across distinct sessions. Fixed: added a global
     `--max-concurrent-reviews` cap enforced in `try_start`; narrowed the written claim and
     acknowledged the pre-existing (unchanged) session-record growth.
  4. *minor* — the smoke-test split was nondeterministic (the poll-cancel review could finish
     before the explicit cancel). Fixed: two independent reviews, one per assertion.
  5. *minor* — the deliberate deviation from MCP's "free associated resources" cancellation
     guidance must be stated explicitly. Fixed: README design note and tool descriptions now say so.

  The reviewer also **confirmed** two load-bearing claims: the `RequestCancel` ownership split
  preserves the existing response/kill race provided everything stays under one mutex; and the
  narrow protocol claim — that a stdio tools-only MCP server has no message to inject a finished
  result into an ended agent turn — is correct.
