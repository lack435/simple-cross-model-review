# Single blocking collect — design

Status: **implemented; merge gate pending.** The plan went through six rounds of cross-model
review ending in a clean **APPROVE, no findings** (see [Review history](#review-history)). The
implementation is built and passes `cargo fmt --check`, clippy `-D warnings`, and the unit tests
(including the new cancellation, concurrency-cap, wait-cap, and Codex read-cap tests); the
implementation diff still needs its own pass through this repository's cross-review merge gate.

## Implementation status

Built across `src/cancel.rs` (the `attach_owned`/`attach_wait` ownership split and the
`CancelAction` enum), `src/registry.rs` (`wake()`, the cancellation-aware `wait()`, the
per-process `TooManyRunning` cap), `src/config.rs` (`max_wait_secs()`, `--max-concurrent-reviews`,
the `--timeout-seconds` 24h bound, `FINALIZATION_GRACE_SECS`), `src/reviewer/codex.rs` (the capped
final-message read → `OUTPUT_TRUNCATED`), `src/errors.rs` (`too_many_running`), `src/tools.rs`
(default-to-completion wait, the non-destructive result-poll detach, the `TooManyRunning` mapping),
`src/mcp.rs` (the `cfg`-derived schema cap, the `Detach`/`Kill`/`Nothing` cancellation split), and
`src/vcs/mod.rs` (the `read_capped`/`CAPTURE_BUDGET` re-exports). Docs and configs updated:
`README.md`, `AGENTS.md`, both `examples/*` configs and READMEs, `.mcp.json`, `.codex/config.toml`,
and the `smoke.ps1` step-7 two-review split. The deterministic cancellation contract lives in unit
tests; `smoke.ps1` is best-effort e2e liveness and has not been run here (it bills a real model and
needs `dist\` unloaded).

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
- **The session lease releases when the worker finishes**, not when the review is collected.
  Precisely: the lease is the `_lease` field on the `Job` (`src/tools.rs`, "dropping the job
  releases the session"), so it is freed when the worker thread returns — which is just *after*
  `Registry::finish` flips the terminal state and its post-finish accounting runs, not exactly at
  `finish`. Either way it is bounded by the worker's own lifetime, not by whether a caller ever
  collects. A later `cross_model_review` on that session is refused as `SESSION_BUSY` only while
  the worker is genuinely still running — which is correct, since you should not start a second
  review of a session mid-review anyway.
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

So this PR adds an explicit **per-process cap on concurrently-running reviews** as the backstop:

- A new `--max-concurrent-reviews <n>` config knob (default a small figure — proposed **8** — that
  no legitimate flow reaches; a serial dogfooding session runs one at a time). `0` disables the
  check, consistent with the other limit knobs — but `0` is an explicit opt-out, **not** the
  default: a finite backstop shipped by default is the point.
- `Registry::try_start` gains a third refusal: alongside `Busy(existing)` and `ShuttingDown`, a
  `TooManyRunning { limit }` returned when the count of `Status::Running` reviews is already at the
  cap, mapped to a plain agent-facing error (`errors::too_many_running`) that tells the caller to
  collect or cancel an outstanding review rather than escalating — the same class as `SESSION_BUSY`,
  since it is the agent's own call to make.
- The check is inside `try_start`, under the same state lock that inserts the review, so two
  concurrent starts cannot both slip past a cap of *n* into *n+1*.
- **It is a per-*process* cap, and the plan says so plainly rather than calling it "global."** `App`
  holds one in-memory `Registry` per process (`src/tools.rs`, `src/registry.rs`), and the README
  supports two server processes sharing one state directory. The cap lives in that in-memory
  registry, so *N* server processes admit up to *N × limit* concurrent reviews; the cross-process
  session lease bounds duplicate work on the *same* session name but does not coordinate a shared
  running-count. A genuinely cross-process slot lease would need a crash-safe shared counter — the
  same complexity the README notes was deliberately avoided for session state — and is
  disproportionate to a backstop against a runaway *caller* (the trusted agent), which is a
  per-process actor. The single-server case is the norm and is fully covered; the multi-server
  multiplier is documented, not hidden.
- Admission tests: the cap refuses at the limit, two concurrent starts cannot exceed it, and `0`
  disables the check.

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
  a single new named constant (proposed ~30s) covering the tail after the reviewer clock stops:
  the output-drain grace, transcript parse, and session persistence.
- **Honest about what this is: a practical sizing, not a proven ceiling.** Two parts of the tail
  are not themselves deadline-enforced in the code, so `max_wait()` is a generous cover for the
  realistic lifecycle, not a mathematical upper bound on time-to-terminal-state:
  - The Codex **final-message file read** is currently unbounded (`src/reviewer/codex.rs:138`,
    `std::fs::read_to_string`). This PR **caps that read** at the existing 8 MiB stream cap, so the
    one adversarially-controllable term becomes bounded and consistent with the pipe cap the README
    already documents. **A capped read must never be parsed as an authoritative review.** The
    project already has `read_capped` (`src/vcs/shared.rs`), which returns the truncated bytes *and*
    an over-cap flag; the Codex path uses it and, when the flag is set, surfaces the existing
    `OUTPUT_TRUNCATED` failure — which is exactly the right code, since the README already defines it
    as "the CLI wrote far too much, a retry will do the same again" as opposed to `EMPTY_REVIEW`.
    **The over-cap flag is checked *before* any decode.** `read_capped` returns raw `Vec<u8>`
    (`src/vcs/shared.rs`) and the boundary/decode helpers take `String`; truncating 8 MiB of bytes
    can land mid-codepoint, so the path must not `from_utf8`/lossy-decode the truncated bytes at all
    on the over-cap branch — it returns `OUTPUT_TRUNCATED` straight from the flag, and only decodes
    when the read was within cap. (`read_capped` is a `pub(crate)` *item*, but its
    module is declared `mod shared;` — **private** — at [`src/vcs/mod.rs:17`](../src/vcs/mod.rs),
    which today re-exports only `Capture, CapturedChange`. A `pub(crate)` item in a private module is
    not nameable from `reviewer/codex.rs`, so this PR adds an explicit re-export —
    `pub(crate) use shared::read_capped;` from `vcs/mod.rs` (preferred), or makes the module
    `pub(crate) mod shared;` — rather than assuming the path already resolves. The same plumbing
    applies to `CAPTURE_BUDGET`, which `Config::max_wait()` now needs: re-export it from `vcs/mod.rs`
    the same way, or lift the constant somewhere already reachable from `config.rs`.) An over-limit test asserts the read yields
    `OUTPUT_TRUNCATED` rather than a partial review presented as complete. This tightens today's
    behaviour, where an over-8-MiB final message would otherwise have been returned as a valid review.
  - **Session persistence has no wall-clock deadline** (`src/session.rs`), so a stalled disk write
    could in principle push the terminal state past `max_wait()`. This PR does **not** add a
    persistence timeout: the code deliberately treats session-mapping durability as load-bearing (a
    session that cannot be persisted is reported as a warning and the response stops inviting a
    resume), and a timeout there risks dropping a mapping that is mid-write. It is a small JSON
    write of local state, not attacker-controlled latency, so it is left as documented residual.
    **The "one extra poll" degradation assumes persistence eventually completes.** A *transiently*
    slow write costs a poll; a *permanently* stalled one is worse than one poll — the review's entry
    stays `Running` and its session lease stays held until the process is restarted, so the affected
    session cannot be re-reviewed and repeated collects keep returning `running`. That is the honest
    worst case of declining the deadline, and it is judged acceptable because a local JSON write that
    never returns is a failed machine, not a normal operating condition, and the blast radius is one
    session on one process rather than lost review work. One qualification on "never lost work": that
    holds *while the process stays alive* — a completed review's text is retained in memory and
    collectible. Restarting the process while a `record_turn` is permanently stalled discards that
    in-memory result along with everything else the process held, which is the general property that
    review ids and running reviews do not survive a restart, not a new failure mode of this change.
  - **Why the residual is acceptable here specifically:** because the whole design is now
    non-destructive, a wait that expires one moment before the terminal state simply returns a
    `running` snapshot and the caller polls once more — cheap, and it cannot lose the review. Under
    the *old* destructive semantics that same boundary miss would have been fatal, which is exactly
    why a proven ceiling would have mattered then and is only a nicety now. So the claim is
    weakened to: **`max_wait()` catches the terminal state in one call in every realistic case, and
    a boundary miss degrades to a single extra poll, never to lost work.** "One blocking call" is
    the norm, not an asserted invariant.
- **Derived, not a fixed 1800.** If an operator sets `--timeout-seconds 3600`, the cap tracks it.
  The cap follows the configured budget instead of a second hard-coded number that would silently
  disagree with it.
- **`--timeout-seconds` gets a sane upper bound (fixes a latent overflow).** Today it accepts any
  `u64` and several deadlines are computed as `Instant::now() + timeout` without checking
  (`src/registry.rs:523`, `src/reviewer/mod.rs:424`), which panics on overflow. Deriving the wait
  cap from `timeout` makes that latent bug easier to reach, so this PR rejects an out-of-range
  `--timeout-seconds` at parse time (a defined maximum — proposed 24h — well above any real review)
  rather than only saturating the cap sum, which would leave the `Instant + Duration` sites able to
  panic. Saturating arithmetic on the cap computation stays as belt-and-braces.
- **Default `wait_seconds` blocks to completion.** When `wait_seconds` is omitted, the call waits
  the full effective cap. The ergonomic no-argument path — `cross_model_review_result` with just a
  `review_id` — becomes the one blocking call the issue asks for. A caller that wants a quick "is
  it done yet?" peek passes `wait_seconds: 0`, which returns the current snapshot immediately.
- **The tool schema must be changed to report the real cap.** Today `inputSchema.wait_seconds.maximum`
  and its description use the compile-time constant `crate::config::MAX_WAIT_SECS`
  ([`mcp.rs:782`](../src/mcp.rs)) — *not* `cfg`, despite the surrounding `format!` reading other
  `cfg` fields. This is an edit, not a free consequence: schema generation must take the configured
  `max_wait()` (the `cfg` is in scope where the tool list is built), and a test must cover a
  non-default `--timeout-seconds` so the schema and the runtime cap cannot silently diverge.

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

**The pre-attach cancellation race must also honour ownership — this is the easiest place to
reintroduce the core bug.** Today both sites handle a cancellation that arrived *before* the attach:
the result path at [`tools.rs:387`](../src/tools.rs) does `if request.attach_review(&id) {
self.registry.cancel(&id); return Err(cancelled()); }`, and the start path at
[`tools.rs:222`](../src/tools.rs) does the same with `registry.finish(cancelled)`. Renaming
`attach_review` to `attach_wait` on the result path while leaving the `self.registry.cancel(&id)`
call would **destroy the review on the pre-attach race** — exactly the behaviour this PR removes,
just on a narrower window. So the result path's pre-attach branch must return `CANCELLED`
*without* calling `registry.cancel` (the review stays running and collectible); the start path's
pre-attach branch keeps finishing the review, because there the id was never delivered. A
`cancel-before-attach_wait` unit test asserts the review remains `Running`.

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

**What "returns promptly" does and does not cover.** `wake()` guarantees the parked `Registry::wait`
returns. It does not guarantee the whole handler unwinds instantly: when a progress token was
supplied, the handler then drops the `ProgressReporter`, which joins the progress thread
(`src/mcp.rs:342`), and that thread can be mid-`send_progress` blocked on a stdout the client has
stopped draining (`src/mcp.rs:324`). That join-on-a-blocked-stdout hazard already exists for **every**
completed result call today — it is not introduced here — so this PR narrows the claim to "the wait
is woken promptly" rather than "the handler returns promptly," notes the pre-existing progress-join
caveat, and adds a progress-enabled cancellation test to pin the wake behaviour. Making the progress
send itself time-bounded/interruptible is a separate, broader change to the stdout path and is out
of scope.

### 3. The cap, the concurrency backstop, and the finalization bound (`src/config.rs`, `src/registry.rs`, `src/reviewer/codex.rs`, `src/tools.rs`, `src/mcp.rs`, `src/errors.rs`)

- `src/config.rs`: replace the fixed `MAX_WAIT_SECS = 300`. Introduce a derivation
  `Config::max_wait()` = `CAPTURE_BUDGET + timeout + FINALIZATION_GRACE`, saturating (see the cap
  section above for why all three terms are needed). `FINALIZATION_GRACE` is a new named constant.
  Keep `DEFAULT_WAIT_SECS`'s *meaning* as "block to completion" — i.e. an omitted `wait_seconds`
  resolves to `max_wait()`.
- `src/config.rs`: validate `--timeout-seconds` against a defined maximum (proposed 24h) as well as
  the existing `> 0` check, so the `Instant::now() + timeout` deadline sites cannot overflow.
- `src/config.rs`: add `--max-concurrent-reviews <n>` (default 8), parsed like the other numeric
  flags, `0` disables the check (consistent with `--session-max-turns`/`--session-max-idle-seconds`).
- `src/registry.rs`: `try_start` counts `Status::Running` reviews under the state lock and returns
  the new `StartRefused::TooManyRunning { limit }` when at the cap. `src/errors.rs`: a plain
  `too_many_running` correction (not an escalation code), telling the caller to collect or cancel an
  outstanding review. `src/tools.rs`/`src/mcp.rs`: map it on the start path next to `Busy`/`ShuttingDown`.
- `src/reviewer/codex.rs`: cap the final-message file read at the existing 8 MiB stream cap instead
  of an unbounded `read_to_string`, using `read_capped` (`src/vcs/shared.rs`) so the over-cap flag is
  observed. On overflow, return `OUTPUT_TRUNCATED` rather than parsing the truncated bytes as a
  review; truncate on a UTF-8 boundary. Add an over-limit test.
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
- **The authoritative cancellation contract moves to deterministic unit tests, not `smoke.ps1`.**
  A real-model smoke run cannot *guarantee* a review is still mid-flight when the cancellation is
  sent, so any assertion that depends on that timing is inherently flaky (Review A could finish
  before the poll cancellation, letting the old destructive behaviour pass unnoticed; Review B could
  finish before the explicit cancel, making `CANCELLED` flaky). So the load-bearing assertions live
  where lifecycle is fully controlled — registry-level tests driving a scripted/fake reviewer:
  - poll cancellation (`attach_wait` + `cancel()` → `Detach`) leaves the review `Running` and
    collectible; the parked wait is woken (finding-1 barrier test);
  - explicit `cross_model_review_cancel` drives the review to `Failed`/`CANCELLED`;
  - start-call cancellation (`attach_owned` → `Kill`) finishes the review `CANCELLED`.
- `smoke.ps1` — **contract change, called out loudly, but scoped to best-effort e2e liveness.** Step
  7 today cancels a `_result` poll and asserts the review is `CANCELLED`; that assertion is wrong
  under the new semantics. The rewrite cancels a real review ~2s after starting it — practically
  certain to be mid-flight for a multi-minute review, the same timing the current step already
  relies on — and asserts **tolerantly**:
  1. **Review A** — poll, cancel the poll → assert the poll is never answered (unchanged) **and** a
     fresh collect returns *running-or-completed-and-collectible* (i.e. **not** `CANCELLED`, not
     gone). Then, so the smoke run does not leak a billing reviewer, explicitly
     `cross_model_review_cancel` Review A if it is still running.
  2. **Review B** — a separate review → `cross_model_review_cancel` ~2s in → assert `CANCELLED`
     **or** already `completed`. Even a 2s-in cancel of a multi-minute review can, in principle,
     race a review that finished unusually fast, so the smoke assertion tolerates natural completion
     rather than asserting `CANCELLED` unconditionally; the deterministic proof that explicit cancel
     kills a *still-running* review is the unit test, not this timing-dependent e2e check.
  The smoke test proves the wiring end to end; the *guarantee* is the unit tests. `README.md`'s
  smoke summary (the "a cancellation that must leave the request unanswered and the reviewer dead"
  line) is updated to describe the two-review split and that a poll cancellation now leaves the
  reviewer alive.

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

- **Round 2** (same session) — REQUEST CHANGES. Round-1 #1 (wake race) and #5 (MCP deviation)
  confirmed resolved; the ownership split and protocol claim reaffirmed sound. The new findings were
  about over-claimed *hard guarantees*; all accepted, resolved mostly by weakening claims to the
  honest *non-destructive-degradation* story plus two real hardenings:
  1. *major* — `max_wait()` still is not a *proven* whole-lifecycle ceiling: the Codex final-message
     file read is unbounded and persistence has no wall-clock deadline. Resolved by capping the
     Codex read at 8 MiB, documenting persistence latency as accepted residual (a deadline there
     risks session-mapping durability), and weakening the claim to "catches the terminal state in
     every realistic case; a boundary miss is one cheap extra poll, never lost work" — acceptable
     precisely because the design is now non-destructive.
  2. *major* — `--max-concurrent-reviews` is per-*process*, not global (one `Registry` per process;
     README supports two servers sharing a state dir). Resolved by calling it a per-process cap,
     documenting the *N × limit* multiplier, and arguing a cross-process slot lease is
     disproportionate to a runaway-caller backstop. Default stays finite (8); `0` is opt-out.
  3. *minor* — the smoke split is still real-model-timing-dependent. Resolved by moving the
     authoritative contract to deterministic registry unit tests; `smoke.ps1` becomes best-effort
     e2e liveness with explicit Review-A cleanup, and the README smoke summary is updated.
  4. *minor* — saturating the cap sum does not stop `Instant + timeout` overflow elsewhere. Resolved
     by rejecting an out-of-range `--timeout-seconds` at parse (defined 24h max), fixing a latent
     pre-existing panic the raised cap made easier to reach.
  5. *minor* — `wake()` guarantees the *wait* wakes, but the handler can still linger on the
     progress-thread join if the client stopped draining stdout. Resolved by narrowing the claim to
     "the wait is woken promptly," noting the pre-existing progress-join caveat, and adding a
     progress-enabled cancellation test; the lease-release-vs-`finish` distinction is now stated
     precisely.

  The reviewer confirmed the three scoped-out residuals (persistence deadline, cross-process slot
  counting, interruptible stdout) are legitimate out-of-scope choices given the revised claims, not
  correctness blockers.

- **Round 3** (same session) — REQUEST CHANGES. All five round-2 findings confirmed resolved; three
  smaller items, all accepted:
  1. *major* — the new 8 MiB Codex read cap did not specify over-limit behaviour, so a truncated
     final message could be parsed as a valid review. Resolved: use `read_capped`, and on the
     over-cap flag return `OUTPUT_TRUNCATED` (never parse the partial), truncate on a UTF-8 boundary,
     add an over-limit test. This tightens today's unbounded behaviour.
  2. *minor* — the "one extra poll" degradation assumes persistence completes; a permanently stalled
     write holds the `Running` entry and lease until process restart. Resolved: documented as the
     honest worst case of declining the persistence deadline.
  3. *minor* — the plan wrongly said the schema `maximum` was already built from `cfg`; it uses the
     compile-time `MAX_WAIT_SECS` const. Resolved: corrected to require the schema edit, with a
     non-default-timeout test so schema and runtime cap cannot diverge.

- **Round 4** (same session) — APPROVE WITH COMMENTS: "approved for implementation; all prior
  blocking findings are resolved." Four minor implementation clarifications, all folded in before
  coding:
  1. the *pre-attach* cancellation race on the result path must return `CANCELLED` without calling
     `registry.cancel`, or a mechanical rename reintroduces the core bug on that narrow window;
     added a `cancel-before-attach_wait` test requirement.
  2. `read_capped` returns bytes and the decode/boundary helpers take `String`, so the over-cap flag
     must be checked *before* any decode (truncated bytes can split a codepoint); module visibility
     noted.
  3. smoke Review B tolerates "already completed before cancel"; the deterministic kill proof is the
     unit test.
  4. "never lost work" qualified to "while the process stays alive."

- **Round 5** (same session) — APPROVE WITH COMMENTS; all round-4 findings confirmed resolved, "no
  correctness or security blocker remains." One plumbing detail: `mod shared;` is private in
  `vcs/mod.rs`, so `pub(crate)` items in it are not nameable from `reviewer/codex.rs`/`config.rs`.
  Folded in: the PR adds explicit `pub(crate)` re-exports of `read_capped` and `CAPTURE_BUDGET` from
  `vcs/mod.rs`. No behavioural change.

- **Round 6** (same session) — **APPROVE, no findings.** The reviewer confirmed the round-5
  visibility plumbing is resolved and every earlier finding is resolved or honestly scoped; nothing
  new introduced. Plan approved for implementation (the reviewer notes the eventual implementation
  still needs its own compile / unit / smoke verification — and, per AGENTS.md, its own pass through
  this gate on the code diff).

- **Round 7** (same session — the *implementation* diff) — APPROVE WITH COMMENTS. All correctness
  findings resolved (wake race, ownership/pre-attach races, lifecycle sizing, overflow validation,
  per-process cap, Codex over-cap handling, smoke split, visibility re-export); no security or core
  concurrency blocker. Three minor doc/test items, all folded in:
  1. the docs implied a *client-timeout* cancellation returns `status=running`, but a cancellation
     suppresses the response — only a normal `wait_seconds` expiry returns that. Corrected in the
     `cross_model_review_result` and `cross_model_review_cancel` tool descriptions, README, and
     AGENTS.md, and the descriptions now state plainly that a poll cancellation detaches only the
     wait and that `cross_model_review_cancel` is the resource-freeing operation.
  2. `smoke.ps1` Review A's "no line arrives" assertion was timing-sensitive (a trivial review can
     finish inside the 2s window); made tolerant, with suppression left to the unit tests.
  3. added a `tools/list` test asserting the advertised `wait_seconds.maximum` tracks a non-default
     `--timeout-seconds`.
