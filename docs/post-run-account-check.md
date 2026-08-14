# Nothing from a reviewer run is used before its account is verified — design

Status: **implemented.** This document is the plan, and it describes the code as it now stands. It
went through this repository's own `cross-review` gate before implementation began, and the
implementation goes through the gate too.

> **Review history.** Three rounds against this repository's own gate (Codex, gpt-5.6-luna,
> effort=max), three findings, every one accepted and none disputed. Round 1 (REQUEST CHANGES): f1
> *major* — the plan **moved** the delivery-time account check earlier rather than adding to it,
> which shortens the window the guard covers by however long the parse takes; a widening of the
> identity-switch race, on a security path, in a document whose premise is that no boundary moves.
> The fix keeps both checks, and the guarantee is now "verified before the store write, and verified
> again before delivery" rather than a trade. f2 *minor* — the behaviour table invented the codes
> `REVIEW_CANCELLED`/`REVIEW_TIMEOUT` (they are `CANCELLED`/`TIMEOUT`) and omitted every `parse`
> failure, which the reordering also brings under the refusal; the table is now exhaustive by *arm*
> rather than by code. Round 2 (APPROVE WITH COMMENTS) confirmed f1/f2 resolved, verified the
> delivery-check equivalence against the control flow rather than the description, and raised f3
> *minor* — the `RunError::Spawn` row claimed the findings marker is cleared, when
> `clear_findings_marker_after_pre_launch_failure` clears only if the marker was absent at entry, so
> a `fresh` call keeps one an earlier failed turn set. Round 3: **APPROVE**, no findings, converged.
> Round 2 also answered three questions round 1 had passed over: stopping the chain walk on a moved
> account is correct (only `RATE_LIMITED` falls through, and retrying could conceal a
> security/configuration failure), the shared helper and the `RunFailure` type change are
> proportionate, and the three unit tests plus the structural sole-writer argument are the right
> coverage without a live `Job` harness.
>
> **Implementation review** (the diff, not the plan) then found one *major* the plan had not covered
> and which this document now specifies: the ordering fix made the write verified, but the **key** it
> is written under was still the selection-time one, so a re-login to a *different authorized* account
> between selection and the attempt's pin filed a verified reading under the wrong account. See "The
> key, not only the reading" below.

Tracks issue #69 (the main review path persists a turn's usage headroom *before* the account switch
guard verifies the account). Filed from the #63 implementation review, where the reviewer raised the
identical ordering against the block-repair path as **f5**; that instance was fixed in
`run_block_repair`, and the main run — older than that PR — was deliberately left alone rather than
folded into an unrelated change.

## What the defect is, precisely

`Job::attempt` today does this after the reviewer child returns
([src/tools.rs:2844-2915](../src/tools.rs)):

1. `reviewer::run_observed(...)` returns
2. `observe_headroom(...)` → `self.usage.record(key, headroom, now_unix())` ← **persisted here**
3. `parse(...)`
4. `switch_guard(self.spec.reviewer, authorized_start)?` ← **account verified here**

The usage store keys on the account pinned at the top of the attempt
(`resolve_authorized_home_with_account`, carried in as `usage_key`), while the *reading* comes from
mutable profile state under the home — Codex's rollout log under `$CODEX_HOME`, Claude's
`rate_limit_event` from the run that just happened. So if the profile home re-logs from account A to
account B while the review is running, a reading that belongs to B — or at best one nobody has
verified belongs to A — is persisted under A's key. The proactive usage-remaining gate then reads it
back to choose chain entries on later reviews, which is the one place a wrong figure has a lasting
effect: `docs/usage-remaining-gate.md` §5 promises the store is "keyed by resolved identity *and* a
verified account fingerprint", and step 2 above is the one write in the codebase that does not keep
that promise.

The switch guard catches the account change immediately afterwards and refuses to deliver or record
the review. That is the point of it — but the headroom write has already happened, and nothing rolls
it back.

`run_block_repair` already has the right order for exactly this reason
([src/tools.rs:2480-2518](../src/tools.rs)): guard first, gated on the child having possibly started,
then observe, then parse. The two paths are the same sequence written twice, and one copy is wrong.
That is the fact the fix turns on: this is not a statement-order bug so much as a duplicated
sequence with no single owner.

## The fix

**One shared post-run collection point, with the order fixed inside it.** A new
`Job::collect_run` owns everything that happens between a reviewer child returning and its answer
being usable, and both runs — the main run and the block repair — go through it. The main path's
inline copy and the repair path's inline copy are deleted.

```rust
/// Turn a finished reviewer child into a usable answer, in the one order that is safe:
///   verify the account (if a child could have started) -> observe headroom -> parse.
/// Shared by the main run and the block repair so the order cannot drift between them (#69).
fn collect_run(
    &self,
    run: Result<reviewer::RunOutcome, reviewer::RunError>,
    authorized_start: Option<&crate::config::AuthorizedHome>,
    usage_key: Option<&str>,
    last_message_file: Option<&std::path::Path>,
) -> Result<reviewer::Parsed, RunFailure>
```

The order inside it, and why each step is where it is:

1. **A pre-observation account check, before anything else touches the run.** Nothing from an
   unverified run is stored, parsed, or returned. This is the `run_block_repair` reasoning applied
   unchanged: a run whose output was unreadable still advanced the reviewer's conversation and still
   may have been billed, so a moved home must not slip past the guard merely because its answer could
   not be read.
2. **Gated on a child having possibly started.** `RunError::Spawn` means no process was ever created,
   so there is nothing to verify and nothing was billed; treating it as a security refusal would turn
   an ordinary spawn failure into a non-resumable session for no reason. `RunError::Observe` means the
   child *was* running, so it is guarded.
3. **Then observe and record headroom**, from the raw `RunOutcome`, on both the success and the
   failure arm — unchanged from today except that it is now downstream of the check. This preserves
   `docs/usage-remaining-gate.md` §3 (a rate-limited turn is observed exactly like a successful one)
   while making §5's keyed-by-verified-identity claim true.
4. **Then the cancelled/timed-out branch, then `parse`.**
5. **Then today's delivery-time check, in the position it already occupies** — after the answer has
   been read and before it is returned to be assessed and recorded. It is **kept, not moved**
   (round-1 finding f1). The two checks are not redundant, and neither subsumes the other:
   - The first one exists because a *store write* happens before the parse, so a check after the
     parse is too late for it. It is the only one that runs on the failure arms.
   - The second one exists because the switch guard's coverage is "swaps still visible at the final
     read", and the final read is the one nearest delivery. Deleting it in favour of the earlier one
     would shorten that window by however long the parse takes — a small widening of the
     identity-switch race, but a widening on a security path, which `AGENTS.md` says must not happen
     silently. Adding one local `fingerprint_at` read is much cheaper than arguing the window away.

   So **no boundary moves in this change**: the delivery gate is exactly where it was, and the new
   check is strictly additional coverage in front of the store write. Both paths — main run and
   repair — get both checks, which is the point of one shared helper; a refusal from either is a
   `RunFailure::refusal` and behaves identically.
6. **`last_message_file` is cleaned up on every path, including both refusals, and before the
   delivery check returns.** It has to outlive the `parse` that reads it and be gone before the
   helper returns either way, which is exactly today's order (deleted just before `result?`, which
   precedes the guard at line 2915). An early return added naively above that line would leak the
   temp file, which is why the cleanup moves inside the helper rather than staying at the call site.

**One failure type for a finished run.** `RepairFailure { failure, refusal }`
([src/tools.rs:38-60](../src/tools.rs)) becomes `RunFailure`, carrying the three facts a caller of
`collect_run` needs:

```rust
struct RunFailure {
    failure: Failure,
    /// `true` only for a tripped account switch guard: do not record this turn, leave the
    /// findings write-ahead marker set.
    account_refusal: bool,
    /// `true` only for `RunError::Spawn`: no child existed, so a resumed conversation provably
    /// did not advance and the marker may be cleared.
    child_never_started: bool,
}
```

`account_refusal` is what already stops the repair caller committing the turn as though nothing had
happened, and it keeps that job verbatim. `child_never_started` is what `attempt` needs for
`clear_findings_marker_after_pre_launch_failure` ([src/tools.rs:2384](../src/tools.rs)), which today
is decided in the `run_observed` match arm before the `RunError` is flattened to a `Failure`. Moving
the flattening into `collect_run` means that fact has to travel out; the repair caller ignores it,
which is correct — a repair never withdraws the marker, and the comment on `run_block_repair` already
says why. **The helper itself is untouched**, including its `findings_marker_absent_on_entry` guard
(round-2 finding f3): `child_never_started` decides only whether it is *called*, exactly as the
`run_observed` match arm does today, so a `fresh` call still keeps a marker an earlier failed turn
set rather than clearing it.

`attempt` then reads:

```rust
let run = reviewer::run_observed(...);
self.registry.set_phase(&self.id, Phase::Finalizing);
let mut parsed = match self.collect_run(run, authorized_start.as_ref(), usage_key,
                                        last_message_file.as_deref()) {
    Ok(parsed) => parsed,
    Err(f) => {
        if f.child_never_started {
            self.clear_findings_marker_after_pre_launch_failure();
        }
        return Err(f.failure);
    }
};
```

which is shorter than what it replaces. The guard call at line 2915 does not disappear — it moves
*into* `collect_run` as step 5, keeping its position in the sequence (after the parse, before the
answer is assessed or recorded) and its doc comment ("switch guard [f4], part 2 of 2"), which is
extended to say that a part-1½ check now runs in front of the store write as well.

**The gating rule for the pre-observation check is a small pure function, so it can be tested.**
`collect_run` needs a `Job`, so it is not unit-testable; the decision it makes is:

```rust
/// The post-run account check for a finished run: `Some(refusal)` when the profile home's account
/// moved, `None` when it is still the pinned one, when there is nothing to verify because no child
/// was ever created (`RunError::Spawn`), or when the review is ambient (unpinned, never guarded).
///
/// Deliberately generic over — and therefore blind to — what the run itself produced: whether the
/// turn succeeded, was cancelled, timed out or was rate-limited does not change whether the
/// account has to be verified, and the refusal outranks the run's own failure (#69).
fn post_run_account_refusal<T>(
    reviewer: ReviewerKind,
    start: Option<&crate::config::AuthorizedHome>,
    run: &Result<T, reviewer::RunError>,
) -> Option<Failure>
```

`switch_guard` stays exactly as it is — the inner comparison this wraps, and also the delivery-time
check called directly in step 5, where there is no run to gate on because a child provably produced
the answer being returned. It keeps its existing test.

## The key, not only the reading

Ordering the write after the account check makes the *reading* verified. It does not, on its own, make
the **key** right, and the implementation review found the gap: the key comes from
`usage_headroom_key`, which reads the account currently under the home during chain *selection*, while
`resolve_authorized_home_with_account` pins the account independently at the top of the attempt. If
the home re-logs A→B in between and **B is also authorized**, everything downstream is consistent
about B — the pre-spawn probe asserts B, the run answers under B, the switch guard verifies B — and
the write still says A. That is #69's defect in a different hat: a figure produced by B steering a
later proactive gate for A.

`write_usage_key` closes it by rebinding the write to `authorized_start.account` (and the attempt's
resolved, preflighted binary). That account *is* the verified one — establishing that the live account
still equals this pin is precisely what the switch guard does — so at the moment of the write they are
the same value. Two cases pass through unchanged on purpose: no selection key still means no write (an
unarmed or unkeyable entry gains no store traffic), and ambient keeps the selection key because it has
no pinned account to bind to and is not guarded either.

### The gate decision is a separate defect, and is left as a follow-up

Rebinding the write does not touch the gate **decision**, which is still made against the account
selection read. The implementation review raised that as f2 (*minor*), and it is real — but not by the
mechanism the finding states, and the difference decides what fixing it would cost.

**The decision is sound at the moment it is made.** `usage_headroom_key`'s fingerprint read and the
`usage.get(key).clears(min)` that follows it are adjacent statements, in both gating sites
(`gate_fresh_selection`, and the walk's per-fallback gate). The key *is* the account match: the next
call keys afresh, so a snapshot filed under A stops being reachable once the home holds B. Not
absolutely — a key already in hand still resolves, so an A→B switch landing between
`usage_headroom_key` returning and `usage.get` running does gate B's entry on A's figure (the
reviewer's qualification, and a microsecond window). Beyond that gap no snapshot gates an account it
does not describe. What is unsound is the *inference* from the decision: that the account which was
gated is the
account that would have run. A re-login after the decision breaks that, and because a skipped entry
never reaches an attempt, there is no pin later to compare it against. So the finding's suggested fix —
"re-gate when the selection account differs from the attempt pin" — cannot reach the case that matters:
the entry it would protect is the one that never launches.

**What it costs.** A skipped entry the departed account would have failed and the arrived account would
have served. In a multi-entry chain that is one entry passed over. In a single-entry chain, or when it
is the last unexhausted entry, it is a refused review: `REVIEWERS_EXHAUSTED`, which names each entry's
reason, including "usage below minimum". The next call re-reads the fingerprint, finds no snapshot for
the arrived account, and runs ungated — so the failure states its cause and self-heals on retry, which
is `AGENTS.md`'s "lost and re-run" case rather than a false approval.

**The one variant with a genuinely wide window** is worth naming because it is not the obvious one.
Where the read and the decision are adjacent, the race needs a re-login inside a gap of microseconds.
But a *pre-start* skip is carried in `pre_start_gated_descs` and folded into a terminal
`REVIEWERS_EXHAUSTED` that may be produced many minutes later, after another entry ran and was
rate-limited. There the skip is genuinely stale by the time it is acted on. It still needs the
conjunction of a gated entry, a re-login, and a later rate limit — and it still ends in a retryable
refusal.

**Why it is not fixed here.** Closing it means re-entering chain selection after the walk has already
decided it is exhausted — control flow surgery on the walk, on the account-identity path, in a PR about
the ordering of one store write. This repository's own precedent is the reason to file rather than fold:
issue #69 exists because the same reviewer raised this defect's sibling against the block-repair path
during #63, and the main-run instance was filed as its own issue — "scoped out of that PR deliberately,
not overlooked". This is the same shape, so it gets the same treatment: recorded here in full, with the
mechanism corrected, and filed as its own issue rather than bundled.

## The behaviour choice #69 asks to be made explicit

Moving the check earlier makes combinations reachable that the guard never saw before: today a
cancelled, timed-out or rate-limited turn returns through `result?` *before* the guard runs at all.
So a failure code has to be chosen for those turns on a moved account. **The refusal wins, uniformly.**

The table below is exhaustive **by arm**, not by failure code: the five arms are every way
`collect_run` can leave the run, so a code not named individually falls under the arm that produces
it (round-1 finding f2).

| Arm out of the run | Account | Today | After |
| --- | --- | --- | --- |
| any arm | stable, or ambient (unpinned) | as today | **unchanged, by construction** |
| `RunError::Spawn` — no child ever started | any, incl. moved | `SPAWN_FAILED`; `clear_findings_marker_after_pre_launch_failure` called, which clears *only* when the marker was confirmed absent at entry — a `fresh` call keeps a pending marker an earlier failed turn set | **unchanged** (not guarded: nothing was billed) |
| `RunError::Observe` — child ran, observation failed | moved | `SPAWN_FAILED`, marker left set | `PROFILE_IDENTITY_MISMATCH`, marker left set |
| `out.cancelled` / `out.timed_out` | moved | `CANCELLED` / `TIMEOUT` ([errors.rs:365](../src/errors.rs), [errors.rs:314](../src/errors.rs)), headroom stored | `PROFILE_IDENTITY_MISMATCH`, nothing stored |
| `parse` → `Err` — every classified code the adapters produce: `RATE_LIMITED`, `EMPTY_REVIEW`, `OUTPUT_TRUNCATED`, `OUTPUT_INCOMPLETE`, `MODEL_UNAVAILABLE`, `SESSION_NOT_FOUND`, `EVIDENCE_UNAVAILABLE`, … | moved | that code, headroom stored; on `RATE_LIMITED` a fresh multi-entry walk also **falls through to the next entry** | `PROFILE_IDENTITY_MISMATCH`, nothing stored, walk stops |
| `parse` → `Ok` — a review to deliver | moved | `PROFILE_IDENTITY_MISMATCH`, **but headroom already stored** | `PROFILE_IDENTITY_MISMATCH`, **nothing stored** |

Two things the table does not cover, stated so they are not read into it:

- **The repair path's refusal semantics are unchanged.** A refusal from a *repair* run is not a
  failed call: the repair answer is discarded unread, the main review — which was answered under the
  pinned account and verified so — is still returned, with a warning, and the turn is left unrecorded
  and non-resumable. That is #63's behaviour and this change reuses it verbatim; the uniform
  precedence described here is about which code a *main-run* failure reports.
- **`PROFILE_IDENTITY_MISMATCH` is not a new code on this path.** It is what the existing guard
  already returns on the delivery arm, so no caller, response field or remediation string has to
  learn about it — only the set of arms it can arrive on grows.

Every stable-account and ambient row is unchanged by construction, not by care: on those the guard
returns `Ok` and `collect_run` proceeds down exactly today's statements. That is issue #69's second
acceptance criterion, and it is the reason the fix does not need a compatibility argument for the
common path.

Why the refusal rather than the run's own code:

- **It is the fact that determines what the caller must do next.** A moved account means the turn is
  not recorded, the findings write-ahead marker stays set, and the session cannot be resumed —
  the caller has to rebaseline and probably wants to know why an unexpected re-login happened.
  `TIMEOUT` says none of that. Both codes already leave the turn unrecorded, so the choice is
  purely about which fact is reported.
- **It is one rule instead of a matrix.** A carve-out per run-failure kind ("cancel wins over the
  guard, rate limit does not") is the sort of edge-case machinery `AGENTS.md`'s rigor section warns
  against: it costs a branch and a test per code, forever, to improve the wording of a rare
  environmental fault.
- **A moved account stops the chain walk rather than falling through it.** `PROFILE_IDENTITY_MISMATCH`
  is not `RATE_LIMITED`, so the fresh-review walk ([src/tools.rs:2148](../src/tools.rs)) surfaces it
  at once instead of advancing to the next entry. That is deliberate: an account switching underneath
  a running review is a condition to report, not to route around, and the next chain entry may share
  the same home. The cost of failing closed here is one re-run — the cheap side of `AGENTS.md`'s
  "the worst case is usually that it is lost and re-run".
- **The cancel case is the one that reads oddly, and is accepted as-is.** A user who cancelled a
  review and gets `PROFILE_IDENTITY_MISMATCH` learns something true and more urgent than "you
  cancelled it". The window is also tiny — the guard's account read happens immediately after
  `run_observed` returns, which for a cancel is immediately after the child is killed.

## What is deliberately not in this change

- **No rollback, undo, or compensating write for the store.** Once the write is ordered after the
  check there is nothing to roll back. A "tentative reading, confirmed later" design would add a
  second state to a store whose whole job is to hold one cheap fact per account.
- **No provenance flag on stored headroom** (`verified_account: bool` or similar). The ordering is
  the invariant; a flag would let an unverified write exist and then ask every reader to remember to
  check.
- **Nothing atomic.** The guard remains a single `fingerprint_at` comparison after the child's
  output, and the residual `docs/unstructured-turn-recovery.md` names stays exactly as named: a home
  that moves A→B before the spawn and back to A before the final read passes the guard. This change
  neither introduces nor closes that. Likewise the window between the check passing and Codex's
  rollout read is not closed — and does not need to be, because `find_rollout` keys on the
  `thread_id` this run's own stdout announced, which a re-login does not rewrite.
- **No change to the write or network boundaries** (issue #69's third acceptance criterion): no new
  file is written, no existing write moves to a different path, and one write is removed from one
  path. The reviewer stays read-only and the evidence service is untouched.
- **No change to `switch_guard` itself**, to the pre-spawn probe, to the per-home setup lock, or to
  ambient's unguarded posture.

## Tests

New unit tests in `src/tools.rs`, in the style of the existing
`switch_guard_refuses_a_changed_or_unreadable_account` (a real temp `$CODEX_HOME/auth.json`, so the
live account read is exercised, not just a comparison):

- **`post_run_account_refusal_is_blind_to_what_the_run_produced`** — with the home re-logged from the
  pinned account, a run that succeeded, a run that reports `cancelled`, and a run that reports
  `timed_out` all yield `Some`, and the failure's code is `PROFILE_IDENTITY_MISMATCH`. This is the
  choice made above, pinned at the seam where it is made: the refusal is produced before anything
  looks at `out.cancelled` / `out.timed_out`.
- **`post_run_account_refusal_skips_a_child_that_never_started`** — moved account with
  `Err(RunError::Spawn(...))` yields `None`; the same account with `Err(RunError::Observe(...))`
  yields `Some`. The durability boundary that decides this is `RunError::child_never_started`, which
  already has its own reasoning in `reviewer/mod.rs`.
- **`post_run_account_refusal_passes_a_stable_or_ambient_account`** — the pinned account still in the
  home yields `None` on every run shape; `start: None` (ambient) yields `None` including on a home
  whose account cannot be read at all.
- **`the_write_key_names_the_account_the_attempt_pinned`** — `write_usage_key` follows the pin when it
  disagrees with the selection key, is identity when they agree, stays `None` when there was no
  selection key, and passes the selection key through for ambient.

Not asserted by a unit test, and stated rather than implied: that no store write happens on a
refusal. `collect_run` needs a live `Job`, and building one in a unit test would be a mock harness
larger than the fix. What replaces the assertion is structural — after this change `collect_run` is
the *only* caller of `self.usage.record` in the codebase (today there are two, one per copy of the
sequence), and its refusal path returns before that line. The end-to-end evidence is the smoke run.

Existing coverage kept as-is: `switch_guard_refuses_a_changed_or_unreadable_account` — which is also
the coverage for the delivery-time check, since that check *is* `switch_guard` called directly and
this change does not alter it — and every `findings`/repair test that goes through `RepairFailure`
(renamed, same two-arm behaviour).

## Verification before hand-back

- `.\build.ps1` — fmt, clippy `-D warnings`, unit tests, release build.
- `.\smoke.ps1 -Reviewer codex` — the change is on the shared run/collect path for both reviewers, and
  the Codex direction is the one that exercises the evidence service and the rollout headroom read.
  **This calls a real model and costs tokens**; it will be mentioned to the user when it is run.
  `-Reviewer claude` additionally covers the categorical headroom read on the same helper and is worth
  one run if the first is clean.
- This repository's own review gate on the implementation diff, as `AGENTS.md` requires.

## Files touched

| File | Change |
| --- | --- |
| `src/tools.rs` | `RepairFailure` → `RunFailure` (+ `child_never_started`); new `post_run_account_refusal` and `Job::collect_run` (pre-observation check **and** today's delivery-time check, in that sequence); new `write_usage_key`, binding the write to the attempt's pinned account; `attempt` and `run_block_repair` rewired onto the helper; both inline post-run copies and the standalone line-2915 call site removed, the check itself retained inside the helper; four new tests |
| `docs/usage-remaining-gate.md` | §3/§5: the observation is recorded only once the account it is keyed under has been verified unchanged; a moved account records nothing |
| `docs/unstructured-turn-recovery.md` | the "one shared post-run helper" the #63 plan described (line 613) is now real and shared; the repair's guard-ordering comment moves to the helper |
| `docs/reviewer-account-profiles-impl.md` | note where the post-review guard runs now (before observe/parse, gated on a child having started) |
| `README.md` | one clause in the usage-headroom section: the signal is persisted per account *once that account is verified unchanged* |
