# Recovering a turn whose machine block never arrived — design

Status: **implemented** on `fix/unstructured-turn-recovery`. This document is the plan; it went
through this repository's own `cross-review` gate before implementation began (see the review
history below), and the implementation goes through the gate again as its own review.

> **Review history.** Ten rounds against this repository's own gate (Codex, gpt-5.6-luna,
> effort=max), every one REQUEST CHANGES: 21 findings raised, all 21 accepted and acted on, **none
> disputed**. Round 1 (five `major` + one `minor`) reshaped the repair's identity, headroom and
> budget handling and rejected the test-hook design; round 2 (three `major`) added sticky coverage,
> the single commit point, and the account pin; rounds 3–5 closed the `evaluate_turn` API
> contradiction and — after issue [#62](https://github.com/lack435/simple-cross-model-review/issues/62)
> froze three findings at stale statuses — restated the remainder as new findings, which is how f11
> surfaced: the pinned account could not be honoured at the `Reviewer::invocation` boundary at all.
> Rounds 6–10 worked outward from that into the account-profile surface: the pre-spawn probe (f16),
> the "never launched" overclaim (f17), the marker/commit contradiction (f18), and finally the
> recorded identity being a fresh read rather than the pin (f19) plus two claim-scoping fixes
> (f20, f21).
>
> **Implementation review.** The code went back through the gate as its own review (session
> `issue-63-impl`). The first attempt failed with `TIMEOUT` — the reviewer's CLI refused four
> composed shell commands by policy and spent the turn on variants, filed as
> [#68](https://github.com/lack435/simple-cross-model-review/issues/68). The retry returned four
> findings, all accepted: **f1 (`major`)** the post-repair switch-guard refusal was committed as an
> ordinary repair failure, and the guard sat behind the parse; **f2** the cumulative usage fold
> dropped the main run's per-invocation call count; **f3** the repair child reported as `Finalizing`;
> **f4** the collect cap omitted the repair's pre-spawn probe. Turn 2 confirmed all four resolved and
> raised three more, all following from the f1 fix and all accepted: **f5 (`major`)** the repair's
> headroom reading was observed and *stored* before the guard ran, so an A→B switch could persist an
> unverified reading under A and steer later entry selection with it; **f6** a `RunError::Spawn` (no
> child ever created) was classified as a security refusal; **f7** a refused, non-durable turn
> rebuilt its envelope from pre-turn state and lost the fact that a repair had been attempted, so
> both the caller and the metrics record reported none. Each is marked in place below.
>
> **This document did not receive an explicit APPROVE.** The session reached the configured
> `--session-max-turns` limit of 10 on the turn that would have judged the final three fixes, and per
> the maintainer's instruction the plan was not re-opened as a fresh session. So f19, f20 and f21 are
> **fixed but unreviewed**, and rounds 1–9's findings are reviewer-confirmed resolved. The
> implementation goes through the gate as its own review against real code, which is where those three
> — and anything else here that is argued rather than proven — get checked.

Tracks issue [#63](https://github.com/lack435/simple-cross-model-review/issues/63). It is a delta
against [`structured-findings-envelope.md`](structured-findings-envelope.md), which remains the
contract document; every decision below either extends that contract or is explicitly scoped as
leaving it untouched. Where the two disagree after implementation, the envelope document is wrong
and must be amended — see [Documentation](#documentation).

## Problem

A completed review came back with no machine block. The envelope, verbatim from the issue:

```json
{"converged":false,"findings":[],"findings_trusted":true,
 "ledger_coverage":"legacy_uncovered","non_convergence_reason":"ledger_unavailable",
 "open_count":null,"result_status":"completed","schema_version":1,
 "structured":false,"total_count":null,"turn":1,"verdict":"unknown",
 "verdict_detail":null,"verdict_source":"none",
 "warnings":["no valid machine block this turn: no machine block was emitted; the review is returned unstructured"]}
```

The reviewer ran to completion, wrote prose, and skipped its own output contract. Three separate
defects follow from that, and they are independent — fixing any one leaves the others.

**1. The turn is thrown away.** Today a turn runs a reviewer once *per chain entry* — single-shot run
→ parse → `evaluate_turn` → return — and there is no in-conversation re-request of the block on any
path. (The fallback chain can invoke a *second reviewer entry* when the first is rate-limited, so a
turn is not literally one invocation; but that is a different reviewer and a different conversation,
never a second ask for the block that is missing.) Decision A is the change to that.

So a 5–20 minute review at `effort=max` yields no machine result, and the only recovery available to the
caller is to run the whole review again — after escalating, because a degraded turn also breaks
ledger coverage to `legacy_uncovered`/`needs_rebaseline`, which is a human-escalation outcome by
design. The entire cost of the reviewer skipping one instruction is borne by the caller, and the
cheapest thing in the system — asking again for a block the reviewer already has the material for —
is never tried.

**2. The prose is missing from the structured channel.** The reviewer's prose *is* preserved: it is
rendered in the text body between `--- BEGIN REVIEW ---` and `--- END REVIEW ---`.
`Envelope::to_structured_value()` builds `findings` from the ledger only — empty on a degraded turn
1 — and carries no pointer to the prose at all. A client consuming `structuredContent` sees
`verdict: "unknown"`, `findings: []`, and nothing to read. The review exists; the structured channel
says the review is empty. That is the issue's "not surfaced in a usable form", and it is a real
inconsistency between two channels the design says carry the same envelope.

**3. There is no single field to act on.** `result_status` is hardcoded `"completed"` and its only
other value is `"running"`, so a degraded turn is not distinguishable from a clean one by that field.
A caller must instead reconstruct its next action from four secondary fields (`structured`,
`verdict`, `verdict_source`, `non_convergence_reason`) plus the precedence rules and the reason
table in the design document. The project already rejected this shape of ambiguity once: `converged`
exists precisely so a loop does not derive "am I done?" from `open_count`. The same argument applies
to "what do I do now?", and it has not been applied.

### What is not a defect, and will not change

Stated first, because the fix must not be mistaken for a licence to relax any of it:

- **Degrading to a review rather than an error is deliberate** (Decision 3). Failure codes are
  reserved for the reviewer being unavailable. An unstructured review is still a review.
- **The server never infers a verdict from prose.** `verdict_source` stays `structured | none`,
  never `prose`. Nothing below adds a prose parser, and nothing below reads the prose for meaning —
  it is transported, not interpreted.
- **The block is the sole machine-authoritative source** of findings and verdict (Decision 1).
- **A genuinely unrecovered degraded turn still breaks coverage** and still reports
  `ledger_unavailable`, still cannot converge. Recovery below is recovery *of the turn*, not a
  softening of what an unrecovered degrade means.

## Decision A — one bounded in-turn block repair, inside the same reviewer conversation

When a turn degrades for a reason the reviewer itself can fix, the server sends **one** short
follow-up message in the **same reviewer conversation** asking it to re-emit just the machine block,
naming exactly what was wrong. If that produces a valid block that reconciles, the turn is
structured and the review is not wasted. If anything at all goes wrong, the turn degrades exactly as
it does today.

This is the seam the issue named ("the `Err(cause)` arm of `evaluate_turn` together with the
single-shot invocation"), and it is worth taking because the economics are lopsided: the repair is a
few hundred prompt tokens against a re-review that costs a full max-effort turn plus a rebaseline
handoff.

### Which causes are repairable

| Cause | Repairable | Corrective instruction the repair prompt carries |
| --- | --- | --- |
| `NoBlock` | yes | The block was absent; emit exactly one, with these markers. |
| `MultipleBlocks` | yes | More than one block bore this token; emit exactly one. |
| `Unterminated` | yes | The end marker was missing; both marker lines, each on its own line. |
| `Malformed` | yes | The body was not valid JSON for the schema; re-emit it, schema restated. |
| `FieldTooLong` | yes | A `title`/`detail` exceeded its cap; **shorten the prose, do not drop findings.** |
| `OverCap` | yes | The block or its `new_findings` count exceeded the cap; **shorten `detail` fields, do not drop findings.** |
| `ReconcileError::UnknownId(id)` | yes | Name the ids the ledger never issued or has already resolved. |
| `ReconcileError::DuplicateId(id)` | yes | Name the duplicated ids. |
| `ReconcileError::CounterExhausted` | **no** | A server-side ceiling. Re-asking cannot help; degrade. |

> **Amended.** This table listed a seventh row, `ReconcileError::MissingId(id)` — "name the missing
> ids explicitly; restate total accounting" — which no longer exists. Exact-set accounting was
> deleted with the fix for issue #62: an id the reviewer does not restate is carried unchanged, so
> there is nothing to repair and no call to spend repairing it. That row was also the most expensive
> one in the table, because a reviewer that re-examined everything and dropped a single id out of
> twenty-five lost its whole block, prose included, and bought it back with another model call. See
> [`stale-open-findings-fix.md`](stale-open-findings-fix.md).

The two cap cases carry an explicit "shorten, do not drop" clause. Without it the obvious way for a
reviewer to satisfy a size cap is to drop findings — which is precisely the silent-loss failure the
whole fail-closed design exists to prevent, re-introduced through a prompt.

### The repair prompt

A new pure renderer, `prompt::block_repair(cause, nonce, digest)`, which:

- states plainly that **this is not a re-review**: do not re-read the code, do not revise, add, or
  withdraw findings, do not change the verdict — re-emit the machine record of the review already
  given;
- quotes the specific cause from the table above;
- **reuses `machine_block_section` verbatim** for the markers, schema, and (on a resumed turn) the
  prior-findings digest and the restatement contract. Sharing the renderer is not a tidiness
  preference: two independently-worded statements of the same contract are two things that drift,
  and a repair prompt that describes a slightly different schema than the turn prompt would produce
  blocks that fail for a new reason;
- asks for the block and nothing else.

### Extraction scope, and what a repaired turn is

Extraction runs against the **repair response alone**, not the concatenation of the two responses.
Concatenating would re-inherit the original failure in the `MultipleBlocks` case (original block +
repair block = two blocks bearing this nonce) and would make the ambiguity rule depend on which
failure preceded it. Reconciliation runs against the **same prior ledger and the same turn number**:
a repair is part of turn N, not a new turn.

A repaired turn is an ordinary structured turn — `structured: true`, with `block_repair: "recovered"`
on the envelope and a warning naming what happened.

**What a repair does *not* do is heal coverage.** `coverage_after_turn` is a one-way state machine:
`(Some(broken), _) => broken`. A repair changes this turn's `degraded` input from `true` to `false`,
which is what stops the turn from breaking a `whole_conversation` session — but on a session that
entered already `legacy_uncovered` or `needs_rebaseline`, coverage stays broken no matter how clean
the repaired block is. Such a turn is structured (its `findings`, `open_count` and `total_count` are
real and usable) and still non-convergent, still `ledger_unavailable`, still `outcome: rebaseline`.
That is correct and must stay so — a repair recovers *this turn's* machine record, it does not
retroactively cover the conversation's ungrounded history — and the first draft's blanket "coverage
unbroken, convergence reachable" was wrong for exactly those sessions. A test pins a repaired turn on
an already-broken session as structured-but-still-`rebaseline`.

**Allowing a repaired turn to converge — where the rest of the conjunction already holds, and never
otherwise — is a deliberate call, and here is the argument.** The block
came from the same reviewer, in the same conversation, under the same block contract,
reconciled against the same ledger by the same fail-closed code. Every guarantee the structured path
makes is intact. Refusing convergence would leave the caller exactly where it is today (escalate,
rebaseline, re-review) while having spent the repair — the fix would buy nothing. The residual is
that the prose was written *before* the repair, so a repair block that disagrees with the prose is
undetectable — but that is Decision 1's standing residual, identical in kind and no wider: the
server cannot detect prose/block disagreement on any turn without re-introducing a prose parser. The
repair neither widens nor narrows it, and the `block_repair` field plus the warning mean a human
reading the response can always see that a repair happened.

### Failure handling — the load-bearing rule

**A failed repair never fails the review.** Spawn failure, timeout, rate limit, cancellation, a
second unusable block, a repair that answers under a different conversation id — every one of them
discards the repair and returns the degraded envelope the server would have returned anyway, with
`block_repair: "failed"` and a warning naming the repair-side failure code. The review in hand is
good prose; a bookkeeping retry must not be able to destroy it.

**And a failed repair changes nothing about how the turn commits.** The first draft said repair
failures "leave the write-ahead marker set", which contradicted the single-commit-point rule and left
three policies undecided. The policy, stated once:

- **Every ordinary repair-side failure** — pre-spawn probe failure, invocation-build failure, spawn
  failure, timeout, cancellation, a second unusable block, a repair answering under a different
  conversation id — leaves the turn a **plain degraded turn**, which then goes through the single
  finalize → record → clear transaction *exactly as a degraded turn does today*: the ledger is
  persisted with its coverage break, `record_turn` runs, the marker is cleared in its `Ok` arm,
  resumability is decided by the existing rules, and the Perforce sidecar is handled by the existing
  path. The envelope carries `block_repair: "failed"` and a warning. Nothing about the commit is
  special-cased, and `turn_not_durable` is reported only if that *record* actually fails — never
  because a repair did.
- **With one thing that must be threaded, not inherited: the recorded account identity.**
  `TurnFacts.profile_identity` is currently filled from `current_profile_identity(...)` — a *fresh
  read* at record time. On the A→B probe-failure path that read returns **B**, while the conversation
  being recorded was produced under **A**, so the session would be persisted as B's and a later resume
  would be allowed to continue A's history under B — inverting the guard's whole purpose. So the
  record takes the **pinned main-run identity** (`authorized_start`) rather than re-reading, on every
  path, which is the same "pin once per attempt, thread it, never re-resolve" rule the invocation
  change applies. A test asserts that an A→B probe-failure record preserves A and that a later resume
  under B is refused.
- **The repair may never call `clear_findings_marker_after_pre_launch_failure`.** That is the
  f8 rule and it is about *who* clears: the clear happens in the one commit point, not on a
  repair-side path whose "nothing has advanced" premise is false by then.
- **The one exception is a `switch_guard` trip after the repair**, which is a security refusal rather
  than a repair failure. Three things the implementation review made concrete. It must be **carried
  in the type, not merely intended** — the first implementation returned it as one more `Failure`
  down the same arm as a timeout, so the refusal was committed exactly like a failed retry, recorded
  and marker cleared, the opposite of the contract (f1). It must run **before every side effect of
  its own run**: before the parse is unwrapped, since an unreadable answer still advanced the
  conversation, and before the headroom reading is stored, since that store keys on the pinned
  account while the reading comes from mutable state under the home (f5). And it must run **only
  when a child could have started** — `RunError::Spawn` means none ever was, so nothing was billed
  and nothing could have answered under another account (f6). With those, the existing refusal
  semantics apply unchanged — do not record, leave
  the marker set, session non-resumable — and the degraded review is still returned. A refused turn
  rebuilds its envelope from pre-turn state, so the repair status has to be carried across
  explicitly or both the caller and the metrics record report that no repair was attempted on a turn
  that made a billed one (f7). It is the only
  **repair-side** path that deliberately skips the single commit transaction. It is not the only way
  a marker can end up set: a main-run guard refusal returns before recording, and a failed
  `record_turn` or a failed `clear_findings_pending` leave it set too. Those are existing paths this
  plan does not change.

Tests pin all three: a probe-failed repair records and clears like any degraded turn; a repair that
fails before launch does not clear the marker itself; a guard-tripped repair records nothing and
leaves the marker set.

Two cases need naming individually because they are not simple failures:

- **Cancellation.** `self.cancel` is checked before the repair is started; if the caller has
  cancelled, the repair is skipped and the degraded review is returned (`block_repair` absent — no
  attempt was made). A cancel that lands *during* the repair kills that child through the existing
  `run_observed` path and is treated as a repair failure. Either way the caller gets its review; a
  cancel arriving after the reviewer already answered has never discarded the answer and must not
  start now.
- **The account switch guard, pinned to the main run's identity — which requires threading the pin
  through `Reviewer::invocation`.** `authorized_start` is resolved once per attempt, before the child
  starts, precisely so the post-run check can tell A→B from B→B; a fresh self-read cannot. The repair
  runs inside the same attempt, with that value still in scope and the same shared per-home setup
  lock still held, so it must **reuse the pinned identity and never re-resolve one**. The repair
  child also reports `Phase::Reviewing` while it runs, rather than leaving the turn's progress saying
  `Finalizing` through a live model call (implementation review, f3).

  Requiring that is not enough, because the current boundary cannot honour it. `Reviewer::invocation`
  takes `cfg`/`spec` but no pinned identity, and **each adapter resolves the account itself** —
  `cfg.resolve_authorized_home(spec)` in `src/reviewer/codex.rs` and `src/reviewer/claude.rs` —
  immediately before applying `CODEX_HOME`/`CLAUDE_CONFIG_DIR`. So a profile that moves A→B between
  the main run and the repair would have the repair *launch and bill* under B; the post-run guard
  would discard the response, but the call has already happened under an account this repository
  never authorised. Discarding output is not the same as not making the call.

  So `Reviewer::invocation` gains the pinned `AuthorizedHome` as a parameter and the adapters apply
  it instead of resolving their own. Both call sites — main run and repair — pass the one value
  resolved at the top of the attempt. This is a signature change through the `Reviewer` trait and
  both adapters, which is the right shape: "the account is pinned once per attempt" becomes a fact of
  the type rather than a convention two adapters happen to follow.

  **It also closes a narrower pre-existing window**, stated because this project distinguishes what
  was verified from what was assumed: even with no repair, the account is resolved twice on the main
  path — once for `authorized_start`, once inside `invocation` — so a re-login landing between them
  today launches the *review itself* under B. The existing guard catches that after the fact and
  refuses to deliver or record, which is why it has not been a correctness hole; but the call is
  billed to B either way, and threading the pin removes the window rather than continuing to detect
  it afterwards.

  **And the pin alone is still not enough, because a pinned home is a path, not a credential.**
  `AuthorizedHome` carries a home path and the account it was authorised for; it does not freeze what
  is *inside* that home. If the home re-logs from A to B between the two runs, launching the repair
  with the pinned home still launches under B. The existing defence against exactly this is the
  per-spawn identity-and-method probe that runs before every non-ambient child
  (`resolve_home_identity` + `assert_profile_identity` against `authorized_start.account`), and it is
  a **pre-spawn** check — while `switch_guard` is a post-output one. So the repair runs that same
  probe, against the same pinned home and the same authorised account, immediately before its child,
  **without re-resolving authorisation**. A probe failure skips the repair and is an ordinary
  repair-side failure — see the marker policy below, which it follows exactly: the turn is degraded,
  recorded, and its marker cleared like any other degraded turn. It is a failed repair, not a failed
  review, and not a refusal.

  **What this does and does not guarantee, stated at the precision this repository requires.** The
  probe closes the *deterministic between-runs case*: a home that re-logged while the main run was
  executing is caught before the repair child starts, so the repair is not launched and nothing is
  billed to B. It is **not** atomic with process creation, and it cannot be — an external `codex
  login` takes none of our locks, which is exactly why `switch_guard` exists as a post-output
  backstop and why the code says pre-spawn checks cannot close that race. A login landing in the
  window between the probe and the spawn can still incur a call under B; the post-run guard then
  prevents delivery and recording, as it does today for the main run. So `switch_guard` is retained
  after the repair, unchanged, and no wording anywhere in this plan claims the repair "is never
  launched under a moved account" without that qualification. Nothing short of an atomic
  credential-or-session mechanism the current code does not have would make the unqualified claim
  true, and inventing one is not in scope here.

  **And the backstop itself has a residual worth naming rather than implying away.** `switch_guard`
  is a single `fingerprint_at` comparison after the child's output. A home that moves A→B before the
  spawn and back to A before that final read passes the guard, so a response produced under B can be
  delivered and recorded. That is true of the main run today and the repair inherits it; this plan
  neither introduces nor fixes it. The honest scope of the claim is therefore: **swaps still visible
  at the final check are refused** — not "a review can never be delivered from a run under another
  account". Closing the transient case needs the same atomic mechanism as above and is out of scope
  here; it belongs to the account-profile work, not to a fix for issue #63.

  **The A→B test asserts the deterministic case**: with the home re-logged between the two runs, the
  probe fails and the repair child is not launched — not merely that the invocation received the
  pinned home.

  Ambient (`authorized_start == None`) is unprobed and unguarded today and stays that way — the
  repair inherits the main run's posture rather than inventing one.

  `switch_guard` runs after the main run and passes before the repair is considered. If it trips
  after the *repair* run — the profile home re-logged to a different account mid-review — the repair
  response is discarded unread, the session is left unrecorded and
  non-resumable (the findings write-ahead marker stays set, exactly as the existing guard leaves
  it), and the degraded review is returned with a warning. The security property is preserved:
  nothing answered under an unauthorised account is delivered or recorded. But the *main* review was
  answered under the pinned account and verified so — turning it into an error would discard
  verified-good work to punish a later, discarded call. This is the one place where the repair
  changes the shape of an existing security path, so it is called out here rather than buried.
### Repair conversation identity

A repair is only meaningful if it lands in the conversation that produced the review. The existing
`resumed_session_id_mismatch` check is **not** sufficient cover, because it compares the reported id
against the id *this turn resumed* — and on a fresh turn there is no resume id, so it never fires.
Left there, a repair could start or land in an unrelated conversation, emit a block for a review it
never saw, and have that block recorded as a convergeable turn. The rules, therefore:

- **The repair target id** is the main run's effective conversation id: `parsed.session_id`,
  falling back to `resume_id` on a resumed turn. It is resolved *before* the repair is considered.
- **No target id, no repair.** A fresh main run that reported no session id has no conversation to
  resume; the repair is skipped and the turn degrades as today. Starting a new conversation to ask
  for a block is worse than not asking.
- **A main run that already failed the identity check does not get a repair.** If
  `resumed_session_id_mismatch` fired, the turn is already unrecordable and the conversation identity
  is broken; repairing it would be building on a conversation the server has decided not to trust.
- **The repair must answer under the id it resumed.** If the repair run reports a different nonempty
  id, discard the repair, keep the degraded review, and record under the main run's id exactly as
  today. `record_under` must never prefer a repair id that differs from the target — the later run's
  id is used only when it *matches*.

These four rules are part of `plan_repair`/`apply_repair` (pure) rather than inline conditions, so
each is a unit test rather than a comment.

### Budget, timeout, and configuration

- `--block-repair-attempts <n>`, default **1**, range 0–3; `0` restores exactly today's behaviour.
  One is the default because this is a contract slip, not a capability gap: a reviewer that ignored
  the block instruction once and is told precisely what was wrong either complies immediately or is
  unlikely to comply on a third telling, and each attempt adds tail latency to every degraded turn.
  The range allows 2–3 for the reconciliation cases, where the corrective feedback is specific
  enough (exact ids) that a second attempt is plausibly different from the first.
- `--block-repair-timeout-seconds <n>`, default **180**, clamped to `--timeout` (the per-run budget).
  A repair is a re-emission, not a review; a reviewer that cannot restate a block it already
  computed within three minutes is not going to.
- **`Config::max_wait_secs()` must grow — this is a code change, not a documentation note.** That
  function computes the collect deadline as `capture + turn + finalization`, plus a
  `preflight + turn + drain` term for each fallback entry, and it is what caps `wait_seconds` and
  what the tool text advertises as "covers a whole review". A degraded turn can now run the normal
  turn budget *plus* `attempts × repair_timeout`, so the repair term is added to the single-entry
  budget **and** to each per-fallback term (a fallback entry's turn can degrade and repair too).
  Leaving this to documentation would mean a blocking collect that is advertised as covering a whole
  review times out on exactly the turns this feature exists to rescue. Each attempt's term is the
  child's timeout **plus `PREFLIGHT_CAP_SECS`**: the pre-spawn identity probe in front of it is a
  real CLI invocation with its own 30-second auth-status timeout, not a free check. (Implementation
  review, f4.)
- Attempts are consumed per turn, not per session.

### Accounting and plumbing

These are the details that make a second run inside one turn correct rather than merely working:

- **Both runs go through one post-run path.** Today the code observes usage headroom from the raw
  `RunOutcome` *before* it becomes a `Parsed` or a `Failure`, records it under `usage_key`, then
  parses and removes the `last_message_file`. That headroom store is what the reviewer chain's
  proactive gate reads when choosing an entry next time, so a repair run that consumed real headroom
  without being observed would leave the gate reading a stale figure and selecting an account that is
  closer to its limit than the store believes. A `metrics::fold_runs` helper alone does not fix this:
  the *observation*, not just the arithmetic, has to happen for both runs. So the main run and the
  repair run are routed through one shared helper that observes headroom under the active usage key,
  parses, and cleans up — with the repair's own failure handling layered on top of it.
- **Usage folding.** Claude reports per-turn usage; Codex reports the conversation's running total.
  So the two runs fold differently: `usage_is_cumulative ? take the later reading : sum the two` —
  **except the per-invocation counters, which are summed either way**. "Cumulative" is true of
  Codex's token and cost readings, not of everything in `Usage`: `api_calls` counts the
  `turn.completed` events seen in *that* invocation, so taking the later reading wholesale drops the
  main run's model calls — the figure that exists to explain why a turn costs more than its prompt.
  (Implementation review, f2.)
  This becomes a pure `metrics::fold_runs` with tests, next to `reconcile_cumulative`, rather than an
  inline conditional — mis-folding here reproduces exactly the bug that function already exists to
  prevent (a thread total recorded as a turn's cost). A repair run that reports no usage leaves the
  main run's reading untouched.
- **`prompt_bytes`** gains the repair prompt's bytes: it is prompt this server sent and paid for.
- **The recorded CLI session id** is the repair run's, falling back to the main run's, then the
  resume id — the same precedence the single-run path already uses, applied to the later run.
- **Evidence and invocation.** The Codex evidence bundle, handshake nonce, and sterile directory are
  built once per turn and reused for the repair invocation; each invocation gets its own
  `last_message_file`, removed after parse, exactly as today.
- **Denials and warnings** from the repair run are merged into the turn's, with repair-run warnings
  prefixed so their origin is legible. A repair should make no tool calls; if it does, they are
  counted rather than hidden.
- **Any non-block prose in the repair response** — after the block and marker lines are stripped —
  is appended to the rendered review under a clearly labelled `--- BEGIN BLOCK REPAIR NOTE ---`
  section, bounded to 2,000 characters. Silently discarding reviewer output because it arrived on a
  message the server considers transport is how a "I have reconsidered finding f2" line disappears.
- **Turn numbering and session limits are untouched.** A repair adds a message to the reviewer
  conversation; it does not advance the server's turn counter, and `--session-max-turns` counts
  server turns.
- **One commit point for the whole turn, and the repair may never touch the marker.** The findings
  write-ahead marker is set before the main run and cleared only in the `record_turn` Ok arm. The
  *pre-launch* failure paths (`clear_findings_marker_after_pre_launch_failure`, called when building
  the invocation fails or the child never started) exist because on a first attempt nothing has
  advanced, so the marker is safe to withdraw. **That reasoning does not survive being reused for a
  repair:** by then the main run has already advanced the reviewer conversation, so clearing the
  marker on a repair's pre-launch failure would leave the session resumable with a ledger that no
  longer matches the conversation — the precise staleness the marker exists to catch. So the repair
  runs on a path that **cannot clear the marker under any outcome**; a repair that fails before
  launch is simply a failed repair. The whole turn keeps exactly one finalize → record → clear
  transaction, after both runs, driven by the final assessment. For the same reason the Perforce
  `.pending` sidecar is not touched by a repair: the change was captured once, for the turn.

### Keeping the logic testable

The repository has no fake-CLI harness — the correctness discipline is "pure module, exhaustive unit
tests". The repair keeps that shape:

- `findings::assess_turn(review_text, nonce, prior) -> TurnAssessment`, where a degraded assessment
  carries a typed `RepairAdvice { kind, cause }` (or `None` for `CounterExhausted`).
- `findings::plan_repair(&assessment, attempts_remaining, cancelled) -> Option<RepairRequest>` — the
  whole decision, pure.
- `findings::apply_repair(assessment, repair_text) -> TurnAssessment` — extraction and reconciliation
  of a repair response against the same prior state, pure.
- `findings::finalize_turn(assessment, budget, …) -> TurnEvaluation` — builds ledger and envelope.
- `evaluate_turn` **keeps its signature and semantics**, implemented as `assess + finalize`, so no
  call site has to learn the new seam to keep working.

To be precise about one thing the first draft left contradictory: `TurnEvaluation` **does** gain a
`review_prose: String` field (the reviewer's text with its own `_IN` block stripped), and
`src/tools.rs` reads it instead of calling `strip_reviewer_block` on its own after the fact. That is
a change to the returned struct, so every site that constructs or exhaustively destructures a
`TurnEvaluation` — the tests in `src/findings.rs` and `src/session.rs` — is updated. "Unchanged"
above means the function's signature and meaning, not that the type it returns is frozen.
`strip_reviewer_block` stays public: it is still the tested primitive, now called from one place
instead of two, which is the point — a single owner for "what prose do we render and store".

`src/tools.rs` is then only orchestration: run the CLI, hand the text back, fold usage, merge
warnings. Every branch of the decision is unit-testable without a model. **What that does not
cover** is the two-run orchestration itself, and this document does not claim otherwise — see
[Verification](#verification) for the one honest option for exercising it against a real CLI.

## Decision B — the prose travels on the structured channel when the machine channel is incomplete

> **Superseded by [`structured-channel-parity.md`](structured-channel-parity.md) (issue #73).** The
> *condition* below — attach the prose only when the machine channel does not represent the turn —
> was wrong, and the table of four situations no longer describes the code: `review_prose` is now
> attached on **every turn that ran**, and is `null` only when no reviewer ran. The reasoning that
> follows is kept as the record of #63, and its analysis of capping and marker safety still holds.
>
> Why it was wrong, in one line: it made a *content* decision (is there more to read?) a function of
> the *action* axis, which [Decision C](#decision-c--one-field-a-caller-switches-on) below
> deliberately separates from the content axis — so on a clean structured turn, everything the
> reviewer said outside its findings list was unreachable to a `structuredContent`-only client. That
> included `approve_with_comments`, a verdict whose entire content is the comments.

The completed envelope gains:

- **`review_prose`** — `string | null`. The reviewer's prose with its own machine block already
  stripped, capped at 16,000 characters, truncated from the tail with an explicit trailing note
  naming how much was dropped and pointing at the text channel for the whole thing.
- **`review_prose_truncated`** — `boolean`.

`review_prose` is non-null **iff a turn actually ran and the machine channel does not represent it**:

| Situation | `review_prose` | Why |
| --- | --- | --- |
| Clean structured turn | `null` | `findings` *is* the machine record of the review; duplicating the prose on every turn doubles the response for nothing. |
| Degraded turn (`structured: false`) | the prose | The #63 case. Nothing else in the envelope carries the review. |
| `turn_not_durable` | the prose | The design document already says the human reconstructs from this turn's returned prose; the structured channel must therefore contain it. This turn's increment is not in `findings` by construction. |
| Over-budget on entry | `null` | No reviewer ran. There is no prose, and a `null` says so. |

Both channels carry the identical value — the `structuredContent` object and the `_OUT` text block
come from one renderer and must not diverge, or "the same envelope on two channels" stops being
true. The cost is that a degraded turn's text body carries the prose twice, bounded by the cap; a
typical review is 2–8 KB, so a degraded body roughly doubles, and only degraded bodies do.

**Marker safety is preserved and must be tested.** The `_OUT` block is appended after
`strip_marker_lines` has swept the assembled body, so the embedded prose is not swept. It does not
need to be: a sentinel is only a delimiter when it is a whole line, and JSON string escaping renders
every embedded newline as `\n` inside one string value, so a lookalike marker inside `review_prose`
can never form its own line. A test pins this: prose containing a literal `_OUT` marker line still
yields exactly one parseable block bearing this result's nonce.

This adds no interpretation of the prose. The server transports it; nothing reads it for a verdict.

## Decision C — one field a caller switches on

The completed envelope gains **`outcome`**: what the caller should do next, as a **total function of
`non_convergence_reason` alone**.

| `outcome` | Reason | What a caller does |
| --- | --- | --- |
| `converged` | `null` (converged) | Stop. (Still only the machine contract, not a human sign-off.) |
| `changes_requested` | `open_findings`, `verdict_contradiction` | Act on `findings`, re-review the same session. |
| `escalate` | `reviewer_blocked`, `reviewer_withheld_approve` | Stop; the reviewer's own judgement needs a person. Re-reviewing will keep producing this. |
| `rebaseline` | `ledger_unavailable`, `turn_not_durable`, `ledger_too_large` | Stop; this session cannot continue. A human decides, then starts a fresh review carrying the preserved findings — and reads `review_prose` when it is non-null, because it holds what the machine record does not. |

Deriving it from the reason means the mapping is total and non-overlapping *by construction*: the
reason is already chosen by a deterministic precedence over the whole conjunction, so `outcome` adds
no second precedence that could disagree with the first. That is a direct correction of the first
draft, which keyed a fifth value (`unstructured`) off `structured == false` and gave it precedence
over `escalate`. It overlapped: a `turn_not_durable` turn can also be `structured: false`, and the
draft's precedence would have reported "read the prose" while suppressing the rebaseline action and
the instruction to carry the preserved prior findings — hiding the more consequential of the two.

**`outcome` is the action axis; it is deliberately not the content axis.** Whether this turn produced
a machine record is `structured`, and what to read when it did not is `review_prose`. The two are
orthogonal, and collapsing them was the draft's mistake. The #63 case reads:

```jsonc
{ "outcome": "rebaseline", "structured": false, "verdict": "unknown",
  "non_convergence_reason": "ledger_unavailable", "review_prose": "## Verdict\n…" }
```

— one field saying what to do, one saying there is no machine record, one holding the review. None of
those can be mistaken for a clean completion, which is what the issue asked for.

`outcome` is `required` in the advertised schema, so it cannot be silently absent, and it is derived
in one place with an exhaustive test over every reason value including `null` — a derived field that
can disagree with its inputs is worse than no field.

### Why not a new `result_status` value

The issue suggests "a distinct status so it's not mistaken for a completed review". `outcome` is that
signal; `result_status` is deliberately left alone, for three reasons:

1. `result_status` is the `oneOf` discriminator in the advertised `outputSchema`, matched as a
   `const` per branch. A third value needs either a third branch describing an otherwise-identical
   completed object, or the `const` widened to an `enum` — at which point it no longer discriminates
   and the union stops being the thing that keeps running and completed shapes disjoint.
2. The review *did* complete. `result_status` describes whether the server has a result, and it is
   not wrong.
3. Callers correctly matching `"completed"` would start seeing an unrecognised status. That is a
   worse failure mode than an additive field an old caller ignores — and an old caller that ignores
   `outcome` is no worse off than it is today, while every field it already keys on keeps its
   meaning.

## Decision D — envelope and ledger schema versions are separated

Adding three required keys to the completed variant is a wire change, so the envelope version should
go to **2**. But `SCHEMA_VERSION` in `src/findings.rs` is currently *shared*: it stamps the envelope
**and** the persisted ledger, and `src/session.rs` gates ledger load on exact equality with it. Bumping
the shared constant would mark every ledger on disk foreign, turning every in-flight session into a
resume refusal — a fix for a reporting gap that breaks resume for everyone mid-flight.

So the constant splits into `ENVELOPE_SCHEMA_VERSION` (→ 2; **3 since issue #73**, which is this
decision paying off a second time — a second wire-format bump, again with no ledger on disk marked
foreign) and `LEDGER_SCHEMA_VERSION` (stays 1).
They describe different artifacts with different compatibility rules: one is a wire format
renegotiated on every response, the other is persisted state that must survive an upgrade. That they
were ever one constant is the latent bug here, and the fix is foundational rather than incidental.
A test pins that a ledger written before this change still loads after it.

## Decision E — a closing reminder in the prompt

The machine-block contract renders after the change and before `FOLLOW_UP_GUIDANCE`, so on a resumed
turn the last thing the reviewer reads is the follow-up instruction, not the block contract. Append
one line — "End your response with the machine-readable findings block described above" — rendered
only when a nonce is present.

This is a mitigation, not a fix, and its effect is **unmeasured**: it is a plausible improvement at
zero cost, and the metrics field below is what will actually tell us whether unstructured turns
recur. It is listed as its own decision so that it cannot later be described as the thing that fixed
this.

## Decision F — unstructured turns become measurable

The issue closes with "possibly a one-off … filing with evidence in case it recurs". Nothing in the
system would let anyone answer that. The metrics record gains two optional fields:

- `structured: Option<bool>` — whether the turn produced a trusted machine record.
- `block_repair: Option<String>` — `"recovered"` | `"failed"`; absent when no repair was attempted.

Both use `skip_serializing_if`, and the record version follows the existing precedent in
`src/metrics.rs`: a record carrying the new fields is written at a higher version, and the reader
accepts both, so old records still read and old readers skip-and-count rather than misreading.

## What changes, by file

| File | Change |
| --- | --- |
| `src/findings.rs` | `assess_turn` / `plan_repair` / `apply_repair` / `finalize_turn` split; `RepairAdvice`/`RepairKind`/`RepairRequest`; envelope fields `outcome`, `review_prose`, `review_prose_truncated`, `block_repair`; prose capping; `outcome` derivation; `output_schema()` updates; version constant split; `evaluate_turn` returns the stripped prose so `tools.rs` stops stripping separately. |
| `src/prompt.rs` | `block_repair()` renderer reusing `machine_block_section`; the closing reminder. |
| `src/tools.rs` | One shared post-run helper (observe headroom under the usage key → parse → clean up `last_message_file`) used by both runs; the repair run on top of it: invocation reuse, timeout, cancel check, switch guard, conversation-identity rules, usage fold, denial/warning merge, repair-note append; attach prose per Decision B to the degraded, not-durable, and over-budget envelopes; thread `block_repair` into the envelope and the metrics record. |
| `src/reviewer/mod.rs`, `codex.rs`, `claude.rs`, `argv_tests.rs` | `Reviewer::invocation` takes the attempt's pinned `AuthorizedHome`; both adapters apply it instead of calling `resolve_authorized_home` themselves. `argv_tests.rs` is compiled in and calls `.invocation` directly, so it is updated with the rest: its ambient cases pass an absent pin (unchanged behaviour), and new cases pass a pin and assert the controlled environment follows it. |
| `src/metrics.rs` | `fold_runs`; the two record fields; record version handling. |
| `src/config.rs` | `--block-repair-attempts`, `--block-repair-timeout-seconds`: parsing, validation, clamping, `--doctor` output, help text; **`max_wait_secs()` grows by the repair budget** in both the single-entry and per-fallback terms. |
| `Cargo.toml`, `smoke.ps1` | The non-default `repair-test-hook` feature, and the smoke switch that builds an instrumented binary into a scratch directory (never `dist\`) for one real repair round trip. |
| `src/mcp.rs` | Tool description states the loop rule in terms of `outcome` and says a degraded turn returns its prose in `review_prose`. `outputSchema` flows from `findings::output_schema()`. |

### Documentation

- `docs/structured-findings-envelope.md` — the contract document, amended **in place** where it is
  now wrong (the JSONC envelope example, the field notes, the `one schema for every successful
  response` section, and the truth table's unstructured row, which now has a repair path in front of
  it), with a short amendment section pointing here for the reasoning. A stale contract document is
  worse than none. Two specific claims in it must be corrected rather than left standing:
  - **"one call, one response"** (Decision 1's "why not a *second* model call", and the
    single-call/single-dependency summary). The repair *is* a second model call, and the paragraph
    that rejects one must say what is and is not different about this one: it is a short re-emission
    in the same conversation on degraded turns only, it introduces no second model and no structuring
    pass, and it cannot fail the review — but it is a second call, and the document must not keep
    implying there is never one.
  - **"the prose is returned in full regardless."** Still true of the text channel, which is what
    that sentence is about, but the structured copy added by Decision B is capped. Say which channel
    each claim covers rather than leaving one sentence to cover both.
- `README.md` — the two new flags; a short caller-contract note naming `outcome` and `review_prose`;
  and the same qualification of "One request, one response" in the opening summary. The README's
  framing (no orchestration, no choreography) survives intact — a bounded re-ask inside one
  conversation is not orchestration — but the sentence as written is now inaccurate on degraded
  turns, and this project's rule is that a claim says only what was verified. The user-visible
  consequences are stated with it, not left to be discovered: a degraded turn can bill **up to
  `--block-repair-attempts` further short calls** (default 1, configurable to 3) to the same
  conversation, and can take up to `attempts × --block-repair-timeout-seconds` longer;
  `--block-repair-attempts 0` turns the whole thing off.
- `AGENTS.md` — the gate instructions tell agents to loop on this tool; one line stating that the
  loop switches on `outcome`, and that a `rebaseline` outcome with a non-null `review_prose` means
  read the prose and decide, rather than re-running blind.

## Verification

**Unit (no model, no network — the bulk of the coverage):**

- `plan_repair` over every `ExtractError` and `ReconcileError`, including `CounterExhausted` → no
  repair, and attempts-exhausted / cancelled → no repair.
- `apply_repair`: a valid repair block reconciles and produces a structured turn that does not break
  coverage — on a session entering `whole_conversation` that means coverage stays whole; on one
  entering broken it stays broken (the sticky-arm case below). A second unusable block degrades with
  the original cause preserved and `block_repair: "failed"`.
- Repair-prompt content: the cause, the exact nonce-bearing markers, the digest and the restatement
  clause on a resumed turn, the "do not re-review" framing, and the "shorten, do not drop" clause on
  both cap causes.
- The pinned account reaches the child: an invocation built with a pinned `AuthorizedHome` applies
  that home even when the resolvable account has moved underneath it, for both adapters and both runs.
- The A→B case, deterministic form: with the home re-logged between the two runs, the pre-spawn probe
  fails and the repair child is not launched; the turn returns the degraded review and commits like
  any degraded turn (recorded, marker cleared). (A move inside the probe-to-spawn window is not
  closable and is not asserted; the retained post-run `switch_guard` covers it, as it does for the
  main run — and that path, being a refusal, is the one that records nothing and leaves the marker
  set.)
- Repair conversation identity: no target id on a fresh run → no repair; a main run that already
  failed the identity check → no repair; a repair answering under a different id → discarded, and
  `record_under` still resolves to the main run's id.
- `outcome` derivation: exhaustive over every `non_convergence_reason` value including `null`,
  pinning `turn_not_durable` and over-budget-on-entry (`ledger_too_large`) as `rebaseline` — the two
  cases the first draft got wrong.
- `max_wait_secs()`: the advertised collect budget covers `turn + attempts × repair_timeout`, in the
  single-entry case and with fallbacks configured.
- A repaired turn on a session that entered `legacy_uncovered` / `needs_rebaseline` is
  `structured: true` with real counts, and still `converged: false`, `ledger_unavailable`,
  `outcome: rebaseline` — coverage is sticky and a repair does not heal it.
- `review_prose` presence over all four rows of the Decision B table; truncation boundary and the
  truncation note; a lookalike `_OUT` marker inside the prose still yields exactly one parseable
  block. (Issue #73 replaced the Decision B condition — see the note on that section — so the
  presence test now walks the whole outcome matrix instead of those four rows.)
- Schema/renderer parity: the existing test that pins every emitted key against the advertised schema
  is extended to the new keys, both variants.
- `fold_runs`: cumulative-reporter and per-turn-reporter folds, and a repair run reporting nothing.
- Version split: an on-disk ledger written at `LEDGER_SCHEMA_VERSION` still loads while the envelope
  reports its own, higher version. That was 2 when this document was written and is **3** since issue
  #73 — which is the split earning its keep a second time, exactly as intended.
- Config: parsing, range rejection, and clamping of the repair timeout to `--timeout`.

**End-to-end.** `smoke.ps1 -Reviewer codex|claude` exercises the normal path; it cannot *make* a real
reviewer omit its block, so the repair orchestration would otherwise ship unexercised against a real
CLI. The first draft proposed an environment variable that dropped the first run's block, off by
default and loudly warned about. That is rejected: an ambient variable is inherited state, and a
shipped binary that can be told to degrade real reviews and spend real tokens is a backdoor no
warning makes safe.

Instead the hook is **compiled out of every shipped binary**: a non-default Cargo feature
(`repair-test-hook`) whose code is `#[cfg(feature = ...)]`-gated, so `build.ps1`, the `dist\` binary,
and the release workflow contain none of it. `smoke.ps1` gains a switch that builds an instrumented
binary **into a scratch directory, never `dist\`**, points a temporary MCP entry at it, and runs one
real round trip in which the first response's block is dropped and the repair recovers it. A test
asserts the hook is inert without the feature. This keeps "the repair path works against a real
reviewer" a verified claim rather than an assumed one, without the shipped artifact being able to do
it at all.

CI stays Windows-only and its `CLI_NOT_FOUND` contract check is untouched.

## Non-goals and residuals

- **No prose parsing, and no second *model*.** `verdict_source` stays `structured | none`, and
  nothing here adds a structuring pass by another model. It is **not** a no-second-call design any
  more: a degraded turn makes up to `--block-repair-attempts` further short calls to the same
  conversation. That is the exception this plan introduces deliberately, and every document that
  currently claims otherwise is listed for correction above.
- **The completeness contract remains unenforceable.** A reviewer whose prose raises a finding it
  omits from the block is still undetectable. The repair does not change this in either direction.
- **A repaired turn may converge — only where the incoming coverage and every other term of the
  `converged` conjunction already permit it**, never by virtue of having been repaired. On a session
  that entered `legacy_uncovered`/`needs_rebaseline` it stays non-convergent and `rebaseline`. The
  residual is argued in Decision A rather than hidden.
- **Nothing here makes the reviewer more likely to emit the block.** Decision E is an unmeasured
  mitigation; Decision F is how the question gets answered with data instead of anecdote.
- **The repair costs tokens and time on degraded turns.** Bounded by two flags, disableable to
  exactly today's behaviour with `--block-repair-attempts 0`.
