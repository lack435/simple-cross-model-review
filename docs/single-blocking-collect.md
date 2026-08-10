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
does not vanish. It is bounded and mitigated:

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

The net: we give up an automatic cost-saving guess that fired mostly on false positives (client
timeouts, not real user cancellations), and in exchange the main flow stops being able to destroy
its own review.

## The cap change

With the coupling broken, `MAX_WAIT_SECS` is raised to track the review budget:

- **Effective cap = `cfg.timeout` plus a small finalisation grace.** A review can never run longer
  than its own `--timeout-seconds` budget, so waiting longer than that is pointless. The grace
  (the existing output-drain window) lets a review that ends *right at* the budget be caught by
  the same call as a terminal `TIMEOUT` failure rather than a `running` snapshot — strictly better
  ergonomics.
- **Derived, not a fixed 1800.** If an operator sets `--timeout-seconds 3600`, a single call can
  wait the full 3600. The cap follows the configured budget instead of a second hard-coded number
  that would silently disagree with it.
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

The `responded`/`try_claim_response` race machinery is unchanged: response suppression still works
identically for both attach kinds. Only the *kill decision* becomes conditional on ownership.

### 2. The parked wait wakes on its own cancellation (`src/registry.rs`, `src/tools.rs`)

Today a poll cancellation kills the review, the review reaches a terminal state, the condvar
wakes, and `Registry::wait` returns. If we stop killing the review, nothing wakes the parked
`wait` — it would sit until `wait_seconds` (up to the full budget) elapses. That is a lingering
handler thread per abandoned poll, which in a heavy session accumulates.

So `Registry::wait` gains awareness of the caller's cancellation, mirroring how it already
re-checks `shutdown`:

- `wait` takes the request's cancellation flag (an `Arc<AtomicBool>` or the `Arc<RequestCancel>`).
- Each wake, it checks the flag alongside `shutdown` and the terminal-state test, returning
  immediately when set. Whatever snapshot it returns is discarded — the handler's
  `try_claim_response()` loses to the cancellation and sends nothing.
- `handle_cancellation`, on `Detach`, calls a cheap `registry.wake()` (`changed.notify_all()`) so
  parked waiters re-evaluate. `notify_all` wakes every waiter; each re-checks *its own* flag, so
  only the cancelled one returns and the rest re-park. This is exactly the `begin_shutdown`
  pattern already in the file.

Shutdown behaviour is unchanged: `begin_shutdown` already wakes parked waits, so a 1800s park is
still released the instant stdin closes.

### 3. The cap (`src/config.rs`, `src/tools.rs`, `src/mcp.rs`)

- `src/config.rs`: replace the fixed `MAX_WAIT_SECS = 300`. Introduce a derivation
  `Config::max_wait()` = `timeout + finalisation grace` (grace = the existing output-drain
  constant). Keep `DEFAULT_WAIT_SECS`'s *meaning* as "block to completion" — i.e. an omitted
  `wait_seconds` resolves to `max_wait()`.
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
- `src/registry.rs`: new test — a parked `wait` returns promptly when its cancellation flag is set,
  without the review leaving `Running`, and other waiters on the same review are not disturbed
  (they re-park). Mirror the existing `parks`-based timing tests.
- `smoke.ps1` — **contract change, called out loudly.** Step 7 today cancels a `_result` poll and
  asserts the review is `CANCELLED`. Under the new semantics a `_result` cancellation must leave the
  review *alive*. Split step 7 into two:
  1. Cancel a `_result` poll → assert the poll is never answered (unchanged) **and** the review is
     still running/collectible (was: dead).
  2. Then call `cross_model_review_cancel` explicitly → assert the review is now `CANCELLED` and the
     reviewer is dead. This is the assertion that "cancellation stops the reviewer" — it just moves
     to the tool that actually owns killing.

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

(to be filled in as cross-model review rounds run)
