# Proactive usage-remaining gate — design

Status: **plan (round 2).** This document is the plan for the piece of [issue #48] that the
reviewer fallback chain deliberately deferred: the **proactive** gate — "if usage remaining
is less than 10% then instead of Claude Opus use GPT Luna". [`docs/reviewer-fallback-chain.md`]
built the *reactive* chain (fall back only after a reviewer fails with `RATE_LIMITED`) and
recorded that the proactive gate depended on a signal it had **not verified existed**,
reserving a grammar slot but adding nothing until a spike proved the signal real. This plan
**runs that spike, records the verified result, and — because the signal is real — designs
the proactive gate on top of it.** Per this repository's rule it goes through the
`cross-review` gate before it is implemented; [Review history](#review-history) records what
each round changed.

[issue #48]: https://github.com/lack435/simple-cross-model-review/issues/48
[`docs/reviewer-fallback-chain.md`]: reviewer-fallback-chain.md

## The spike, and its result — the signal is real for both reviewers

The fallback-chain doc stated, honestly for what was known then, that a usage-remaining
figure "is not available to this server … not obtainable from any command this project
runs". **That is now falsified by measurement.** Both reviewer CLIs expose a
machine-readable usage-remaining signal on a normal turn — before any limit is hit — but each
in a different shape and on a different surface.

### Provenance — what was measured, and what is assumed

Per the verified-only discipline, the exact provenance is recorded so a later reader can
re-run it, and every claim the design *depends on* is separated from behaviour that was only
incidentally observed.

- **CLI versions**: `claude` **2.1.210 (Claude Code)**; `codex` **codex-cli 0.146.0**. On a
  different version any of the surfaces below may move; the design fails open when they do
  (see [Fail-open](#fail-open-is-the-load-bearing-property)).
- **Commands run** (trivial "reply OK" prompts, cheapest model, a few cents total):
  - `claude -p "…" --output-format json --model claude-haiku-4-5-20251001` — inspected the
    buffered result document's keys.
  - `claude -p "…" --output-format stream-json --include-partial-messages --verbose --model
    claude-haiku-4-5-20251001` — inspected the JSONL event stream.
  - `codex exec --json --skip-git-repo-check -s read-only "…"` — inspected stdout events, and
    the rollout log written under `~/.codex/sessions/…` for the same run.
- **Verified (the design depends on these):** the buffered Claude `json` document carries
  **no** rate-limit field; the Claude `stream-json` stream carries a `rate_limit_event` whose
  terminal `result` event is otherwise the same document; the Codex `exec --json` **stdout**
  carries **no** `rate_limits`; the Codex **rollout log** does, in a `token_count` event; the
  `thread_id` on Codex stdout equals the id embedded in the rollout filename.
- **Assumed, and *not* depended on (labelled so):**
  - *"Once per turn."* Each probe emitted the event once, but the design never assumes a
    count — it takes the **last** such event in the stream/log, which is correct for zero, one,
    or many.
  - *Claude status values.* Only `status: "allowed"` was observed live (both probe accounts
    had headroom). `allowed_warning` and `rejected` are taken from Anthropic's documented
    enum, **not** observed here; a status this tool does not recognise maps to `Unknown`
    (fail-open), so an unlisted future value cannot mis-gate.
  - *Codex multiple windows.* The probe's `secondary` window was `null`; primary/secondary
    handling below is defensive, driven by the field shape, not by an observed two-window
    case.

### Codex — a numeric percentage, in the rollout log (not on stdout)

`codex exec --json` writes JSONL **events to stdout** in the `thread`/`turn`/`item` schema
the server already parses ([codex.rs:265]); measured, that stream carries **no** rate-limit
field:

```
{"type":"thread.started","thread_id":"019fecc8-bcdd-76e3-b7dd-dc8ad78ec428"}
{"type":"turn.started"}
{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"OK"}}
{"type":"turn.completed","usage":{"input_tokens":15488,"cached_input_tokens":5888,...}}
```

Codex *also* writes a **rollout log** per session at
`$CODEX_HOME/sessions/YYYY/MM/DD/rollout-<ts>-<thread_id>.jsonl`; **that** file carries the
headroom, in a `token_count` event, refreshed every turn:

```json
{"type":"event_msg","payload":{"type":"token_count",
  "info":{"total_token_usage":{...},"last_token_usage":{...},"model_context_window":258400},
  "rate_limits":{"limit_id":"codex","plan_type":"pro",
    "primary":{"used_percent":83.0,"window_minutes":10080,"resets_at":1786826027},
    "secondary":null,"rate_limit_reached_type":null}}}
```

So Codex gives a **numeric** figure — `remaining % = 100 − used_percent` (here 17%) — **per
window**, each window an independent `(used_percent, window_minutes, resets_at)`. The
`thread_id` printed on stdout **equals the id in the rollout filename** (verified: stdout
`019fecc8-…` ↔ file `rollout-…-019fecc8-….jsonl`), so the server can locate the exact rollout
from what it already reads.

[codex.rs:265]: ../src/reviewer/codex.rs

### Claude — a categorical status, only in `stream-json`

The server invokes `claude -p --output-format json` ([claude.rs:86]) and parses one buffered
document. Measured, its keys are `type, subtype, is_error, api_error_status, duration_ms, …,
usage, modelUsage, permission_denials, terminal_reason, fast_mode_state, uuid` — `usage` /
`modelUsage` are **consumption only**; there is **no** rate-limit field. (A transcript grep
had turned up a `usage_window`/`consumed_percent` object; probing proved it is *not* emitted
by this surface — it was agent-authored state. That is why it was checked, not assumed.)

Switching to **`--output-format stream-json --verbose`** changes that: the stream emits a
first-class `rate_limit_event` alongside a terminal `result` event whose fields match today's
buffered document:

```json
{"type":"rate_limit_event","rate_limit_info":{
  "status":"allowed","rateLimitType":"five_hour","resetsAt":1786393200,
  "overageStatus":"rejected","isUsingOverage":false},"uuid":"…","session_id":"…"}
```

So Claude gives a **categorical** signal — `status` ∈ {`allowed`, `allowed_warning`,
`rejected`} — plus the window kind (`rateLimitType`, e.g. `five_hour`) and a `resetsAt`. It
does **not** expose a numeric remaining percentage. This shape difference drives the config
grammar below: **a numeric threshold is honest only for Codex.**

[claude.rs:86]: ../src/reviewer/claude.rs

### Fail-open is the load-bearing property

Both surfaces are **CLI internals**, not documented contracts. The design therefore treats an
absent, unparseable, unrecognised, or stale signal as **`Unknown`**, and `Unknown` **never
gates**. If a CLI changes format, the gate silently reverts to today's reactive-only
behaviour rather than misfiring. This is stated once here and relied on throughout.

## What the issue asks of the proactive layer

> … configure a fallback model when usage is beyond a certain threshold. … Minimum usage
> remaining is optional, if not specified then it will always be valid. For example, if usage
> remaining is less than 10% then instead of Claude Opus use GPT Luna. … If no fallback can be
> found meeting usage minimums then reject the review entirely stating as such.

Four requirements, all now buildable: a **per-entry minimum** on the command line; **optional**
(unset ⇒ never gated); a **proactive skip before spawning**; and a **hard rejection** when no
entry clears its minimum.

## Proposal

### 1. Config — two family-appropriate flags, because the signals differ

A single numeric `--min-usage-remaining` cannot honestly gate Claude: Claude exposes no
percentage, so `10`, `40`, and `90` would all collapse to the same "allowed vs not" decision
(round-1 finding f8). So the grammar is **two flags, each valid only for the reviewer family
whose signal fits it**, and a family mismatch is a **parse error** — the operator is told the
signal does not exist rather than handed a knob that silently lies:

- **`--min-usage-remaining <1..=100>`** — Codex only (numeric). "Skip this entry when its
  last-observed remaining across all windows is known and below N %." Range is `1..=100`; `0`
  and `101` are parse errors (`0` would never gate, so it is rejected rather than silently
  arming the machinery for nothing — round-1 finding f10).
- **`--min-usage-status <ample|warning>`** — Claude only (categorical). Names the **lowest
  acceptable** status: `ample` skips the entry on `allowed_warning` **or** `rejected`;
  `warning` skips only on `rejected` (tolerating a warning). `exhausted` is not offered — a
  minimum that never gates is meaningless.

Both bind to the most recent `--reviewer`, exactly as `--model`/`--effort`/`--bin` do
([config.rs:565]); before any `--reviewer`, twice within one entry, an out-of-range numeric,
or an unknown status word are all parse errors, matching the existing binding-error style.
Applying `--min-usage-remaining` to a Claude entry, or `--min-usage-status` to a Codex entry,
is a parse error naming the mismatch. `ReviewerSpec` carries the resulting policy as one
field:

```rust
pub enum UsageMinimum {
    None,                 // unset ⇒ never gated
    Remaining(u8),        // Codex: percent in 1..=100
    Status(HeadroomLevel) // Claude: the lowest acceptable level
}
pub struct ReviewerSpec { /* reviewer, model, effort, bin, */ pub usage_minimum: UsageMinimum }
```

**The minimum is a *gating policy*, not part of reviewer identity.** It is **excluded from
`validate_chain`'s fully-identical-duplicate rule** (two entries differing only by minimum are
still the same account, so the second is still not a fallback — the duplicate rule keeps
rejecting it) and **excluded from the session record's resume identity** (a resume binds to the
reviewer that holds the conversation; editing a threshold between runs must not break resume).

### 2. A normalized headroom signal both adapters feed

The two signals unify behind one type, a deliberate **three-state** value with an explicit
`Unknown` — never a number that defaults to zero:

```rust
pub enum Headroom {
    Unknown,                                         // fail-open: never gates
    Fraction { remaining_pct: f64, resets_at: Option<u64> }, // Codex, the limiting window
    Level    { level: HeadroomLevel, resets_at: Option<u64> }, // Claude
}
pub enum HeadroomLevel { Ample, Warning, Exhausted }
```

**Codex maps to `Fraction`, tied to the *limiting* window.** With multiple windows the adapter
picks the window with the **lowest remaining** and stores **that window's own `resets_at`**,
not the nearest reset (round-1 finding f3: storing the nearer reset would invalidate the
snapshot before the limiting window actually resets, letting fail-open run a still-throttled
entry). Concretely: `remaining_pct = 100 − max(used_percent over windows)`, and `resets_at` is
the `resets_at` of *that same* max-used window. **Claude maps to `Level`** from `status`
(`allowed`→`Ample`, `allowed_warning`→`Warning`, `rejected`→`Exhausted`, anything else→
`Unknown`), with `resets_at` from `resetsAt`.

**The "clears the minimum?" decision** is the one place the shapes meet, and each side is only
ever compared against its own-shaped minimum (the grammar guarantees a Codex entry carries
`Remaining(_)` and a Claude entry `Status(_)`):

- `Unknown` ⇒ **clears** (fail-open), always.
- `Fraction { remaining_pct }` vs `Remaining(min)` ⇒ clears iff `remaining_pct >= min`.
- `Level { level }` vs `Status(min)` ⇒ clears iff `level >= min` on the order
  `Ample > Warning > Exhausted`.

No numeric threshold is ever applied to a categorical signal, and no fabricated Claude "%"
is stored, gated on, or displayed.

### 3. Observing the signal — an explicit channel that survives failures

Round-1 finding f1 is decisive: `Reviewer::parse` returns `Parsed` **only on success**
([reviewer/mod.rs:138]), but the observation is needed most on a `RATE_LIMITED` **failure**
(that is precisely the account the gate should skip next time), and `Job::attempt` forwards
only `parsed.usage` into the outcome ([tools.rs:2221]). Headroom cannot ride on `Parsed`. So
observation is a **separate channel keyed off the raw `RunOutcome`, independent of parse
success**:

```rust
// on the Reviewer trait, alongside parse():
fn observe_headroom(&self, cfg: &Config, spec: &ReviewerSpec, out: &RunOutcome) -> Headroom;
```

- It reads only the CLI's own machine output — Codex's rollout `token_count.rate_limits`,
  Claude's `rate_limit_event` — which are **CLI evidence, classified exactly like stderr and
  structured error events, never the model's prose** ([errors.rs:558] discipline). Round-1
  finding f1 also flagged that Claude's failure path today ignores stdout-owned events; the
  observation channel therefore reads the `rate_limit_event` off stdout explicitly rather than
  going through the failure classifier.
- The **walk calls it after every attempt, on both the `Ok` and the `Err` arm**, and writes
  the result to the store (below). A refusal with no parseable signal yields `Unknown` and
  writes nothing — never a fabricated value.
- It is **only invoked when the chain is armed** (next section), so a non-gating chain pays
  nothing for it.

### 4. Observing costs nothing unless the gate is armed

Reading headroom costs a rollout read (Codex) or a change of output format (Claude). Neither
is paid, and **no output-format change touches a chain that does not use the feature**.
Observation is **armed per chain** iff some entry carries a non-`None` `UsageMinimum`
(`Remaining(≥1)` or `Status(_)`); `0`/unset never arm (round-1 finding f10). `Config` exposes
`chain_gates_on_usage() -> bool`, mirroring `chain_needs_capture()`. When disarmed, both
adapters behave exactly as today and no `Headroom` is ever produced.

- **Codex, when armed** — a **bounded, post-turn** rollout read, no invocation change: take
  `thread_id` from the `thread.started` stdout event; construct the candidate path directly
  from **today's** date (and the previous day, for a midnight rollover) —
  `$CODEX_HOME/sessions/<yyyy>/<mm>/<dd>/rollout-*-<thread_id>.jsonl` — **not** a recursive
  `sessions/**` walk (round-1 finding f12); at most one file matches the `thread_id` suffix;
  read a **bounded tail** of it and take the **last** `token_count.rate_limits`. Any miss —
  `$CODEX_HOME` unset, no file, oversize, unexpected JSON — yields `Unknown`. `$CODEX_HOME`
  defaults to `~/.codex`, read from the same env the CLI honours.
- **Claude, when armed** — invoke `--output-format stream-json --verbose`, read the JSONL,
  parse the terminal `type == "result"` event with **today's logic** and take the last
  `rate_limit_event`. The switch is **behaviour-preserving for the review itself**: the fields
  today's parser reads off the buffered document (`result` text, `session_id`, `denials`/
  `denial_count`/`denial_count_is_floor`, `warnings`, `usage`, `usage_is_cumulative`) are the
  **same fields** on the stream's `result` event. The invariant asserted is **field-level
  `Parsed` equivalence**, not byte-equivalence — a JSONL line is not byte-identical to a
  buffered document (round-1 finding f11) — and the tests exercise truncation, denials, usage,
  warnings, and CLI-owned events across the two paths.

### 5. Persisting the observation — the "before spawning" mechanism, with real cross-process safety

The gate decides **before** spawning, but the signal is observed **after** a turn. The bridge
is a persisted store, `usage-headroom.json` in the state dir. Round-1 finding f5 was right
that "best-effort" is not good enough here: the state dir can be shared across processes, so
the store **reuses `SessionStore`'s exact discipline** ([session.rs:247], [session.rs:318]) —
an `ExclusiveLock` held across a **read-modify-write**, an **atomic rename** on write,
**corrupt-file preservation** (a bad read never triggers a destructive overwrite), and
**stale-write rejection** (an entry's observation only advances when the incoming
`observed_at` is newer). Losing or skipping the store is still non-fatal: a lock or IO error
degrades that one operation to `Unknown`/no-op, which just means "ungated this once."

**Keyed by resolved runtime identity, not raw config.** Round-1 finding f4: keying by the raw
`bin=None`→`PathSearch` tuple would let a later PATH change, or a different `$CODEX_HOME`
account, reuse an observation belonging to a **different executable/account**. So the key is
the **resolved** binary path plus the account context — for Codex, the resolved `$CODEX_HOME`;
for Claude, the resolved binary (its auth home is not separately identifiable, a documented
limit bounded by the TTL below). The stored record carries this identity, and a read whose
resolved identity does not match is treated as `Unknown`, exactly as `SessionRecord` validates
its persisted `resolved_bin` ([session.rs:131]).

**Actionable only while both fresh and unreset** (round-1 finding f9: a record with no
`resets_at` could otherwise gate forever). A stored observation gates only if **`now <
resets_at`** *and* **`now − observed_at < TTL`**. The TTL is the window's own length when known
(`window_minutes` for Codex; `rateLimitType` mapped to a duration for Claude, e.g.
`five_hour`→5h), else a conservative default cap. A missing `resets_at` therefore cannot make
data actionable indefinitely — the TTL still expires it to `Unknown`.

This is the honest form of "proactive": the gate acts on the **freshest observation it has**,
and a first review against a never-seen entry is **ungated** because there is nothing yet to
gate on. It is strictly better than reactive-only — an account known from a prior turn to be
below threshold is skipped before it is billed again — without pretending to a live headroom
query, which neither CLI offers.

### 6. The gate in the walk — select before preflight *and* before capture

Round-1 finding f2 caught a lifecycle error in the round-0 sketch: the start (primary) entry
is preflighted in `start_review` **before** the worker loop ([tools.rs:323]), and `Job::run`
does **capture** and the Perforce pending-marker **before** the loop ([tools.rs:1281], loop at
[tools.rs:1454]). A gate check placed *inside* the loop would let a known-gated primary die in
preflight instead of falling through, and would run capture side-effects for an all-gated
review. So gating is folded into **entry selection, which moves ahead of both**:

- **Selection (fresh review), before any preflight or capture.** Walk the chain from entry 0;
  for each entry that carries a minimum, `resolve_bin` it (a cheap PATH scan — **not** an auth
  check, so it is not the preflight f2 warns about) to compute the store key, read the store,
  and skip the entry if its observation is known-and-below-minimum. The **first entry that
  clears** becomes the start entry, preflighted and run exactly as today. If **every** entry is
  gated out, return `REVIEWERS_EXHAUSTED` (gated variant, below) **before** capture, the
  pending-marker, or any preflight — nothing is resolved, captured, or billed.
- **Fallbacks in the worker loop** are gated the same way, immediately before their existing
  lazy auth-preflight: a fallback whose stored headroom is below its minimum is skipped without
  resolving auth or spawning, and the walk advances.
- **A gated skip spawns nothing and bills nothing** — its whole point.
- **The gate applies to fresh walks only, never a resume** — mirroring the settled rule that
  "fallback selection happens only on a fresh review start" ([reviewer-fallback-chain.md,
  Sessions and resume]). A resume continues a conversation that lives on one reviewer; a
  resumed entry that is genuinely out still hits the **reactive** `RATE_LIMITED` whose
  remediation points at `fresh: true`, and a fresh restart *does* run the gate.
- **Exhaustion is honest per cause** (round-1 finding f7). The existing
  `errors::reviewers_exhausted` hard-codes "every reviewer reported a rate/usage limit" and
  "wait for a limit to reset" ([errors.rs:225]) — false for an all-gated or mixed chain. So the
  constructor is **parameterised by cause**: the pure all-rate-limited path keeps **today's
  exact text** (single-entry `RATE_LIMITED` behaviour is untouched); an all-gated chain says
  the entries were skipped because their last-observed usage was below the configured minimum,
  with remediation "wait for the window to reset, or lower/remove `--min-usage-remaining` /
  `--min-usage-status`"; a mixed chain enumerates each entry with its actual reason
  (`claude/opus: usage below minimum (status=warning < ample); codex/luna: rate-limited`). All
  stay non-agent-correctable.

### 7. Metrics — a gated skip is *not* a billed attempt

Round-1 finding f6: the accumulator marks a turn's totals partial whenever `record.attempts`
is non-empty ([metrics.rs:811]), on the sound assumption that a fall-through attempt consumed
tokens the CLI did not report. A gated skip consumed **nothing**, so recording it as an
ordinary `Attempt` would wrongly mark a successful fallback's usage partial. So `Attempt` gains
an explicit **billed/observation** distinction — a skip is tagged `not billed` (it has a
`USAGE_BELOW_MINIMUM` reason, no `resolved_bin` spawn, no usage) — and the accumulator's
`attempt_free` check becomes "no **billed** attempt with unknown usage": non-billed skips are
recorded for visibility but **do not** taint completeness. Backward-compatibility fixtures cover
records with billed attempts, non-billed attempts, and none. One `Record` per logical turn is
unchanged.

### 8. Attribution, status, doctor

- **Attribution is already correct.** The walk publishes the active entry per iteration via
  `set_active` and attributes the terminal outcome to the entry that produced it
  ([tools.rs:1467]). A gated skip never becomes the active *running* entry; the entry that runs
  is named exactly as a rate-limit fall-through names it today.
- **`status` / `--doctor` gain a headroom column** for each entry: its configured minimum
  (numeric for Codex, categorical for Claude, or "no gate"), and its **last-observed** headroom
  with `observed_at`/`resets_at` and whether that observation is still actionable or has aged
  out to `Unknown`. This is where Claude's categorical-only limit is spelled out for a human.
  These surfaces read the store, call no CLI, and bill nothing.

[reviewer/mod.rs:138]: ../src/reviewer/mod.rs
[tools.rs:323]: ../src/tools.rs
[tools.rs:1281]: ../src/tools.rs
[tools.rs:1454]: ../src/tools.rs
[tools.rs:1467]: ../src/tools.rs
[tools.rs:2221]: ../src/tools.rs
[config.rs:565]: ../src/config.rs
[session.rs:131]: ../src/session.rs
[session.rs:247]: ../src/session.rs
[session.rs:318]: ../src/session.rs
[metrics.rs:811]: ../src/metrics.rs
[errors.rs:225]: ../src/errors.rs
[errors.rs:558]: ../src/errors.rs
[reviewer-fallback-chain.md, Sessions and resume]: reviewer-fallback-chain.md

## What this must not do

- **Must not change behaviour for a chain with no minimum configured.** Observation is
  disarmed, Claude keeps buffered `json`, Codex reads no rollout, no store is consulted, no
  `observe_headroom` runs — the walk is byte-for-byte today's. `0` never arms.
- **Must not gate on an absent, unparseable, unmatched, or stale signal.** `Unknown` always
  clears; a record is actionable only while `now < resets_at` **and** within TTL; a resolved
  identity that does not match the stored key is `Unknown`; every lock/read/parse error fails
  open. The gate skips only on *positive knowledge* an entry is below its minimum.
- **Must not apply a numeric threshold to Claude.** The grammar makes a numeric min on a Claude
  entry a parse error; Claude uses the categorical `--min-usage-status`; no invented Claude "%"
  is ever produced.
- **Must not couple hard to a CLI's internal format.** The Codex rollout log and Claude
  `rate_limit_event` are undocumented; a format change degrades the gate to reactive-only.
- **Must not lose the observation on a failed turn**, nor read it from the model's prose:
  `observe_headroom` runs on both arms and reads only CLI-owned machine output.
- **Must not change a Claude review's outcome when armed.** The `stream-json` `result` event
  yields a field-equivalent `Parsed`; the switch adds an observation, nothing else.
- **Must not run capture, the pending-marker, or preflight for an all-gated review**, and must
  not gate a resume: selection precedes those side-effects and gating is fresh-walk-only.
- **Must not corrupt metrics.** A gated skip is a non-billed attempt and does not taint
  completeness; the shared store uses the lock/atomic/merge/stale-reject discipline so
  concurrent writers cannot regress an observation.
- **Must not treat the minimum as reviewer identity.** It is excluded from the duplicate rule
  and from resume matching.
- **Must not weaken the reactive chain.** `RATE_LIMITED` fall-through, single-entry
  `RATE_LIMITED`, capture, sessions, budget, and the pure all-rate-limited `REVIEWERS_EXHAUSTED`
  text all stand; the gate runs ahead of the spawn, not instead of the reactive handling.

## Blast radius

Additive to the fallback chain; the reactive walk's structure is reused, not rewritten.

- **`config.rs`**: `UsageMinimum` on `ReviewerSpec`; parse + family/range validation for
  `--min-usage-remaining` (Codex, `1..=100`) and `--min-usage-status` (Claude, `ample|warning`);
  `chain_gates_on_usage()`; exclude the field from `validate_chain`'s duplicate rule and from
  identity rendering; status/doctor rendering.
- **`reviewer/mod.rs`**: `Headroom` + `HeadroomLevel` + `clears`; the `observe_headroom` trait
  method (default `Unknown`); no change to `Parsed`.
- **`reviewer/codex.rs`**: when armed, capture `thread_id`, build the date-scoped rollout path,
  read a bounded tail, parse the last `token_count.rate_limits`, pick the limiting window, fail
  open. No invocation change.
- **`reviewer/claude.rs`**: when armed, invoke `--output-format stream-json --verbose`, parse
  the JSONL, reuse the `result`-event parse (field-equivalent), read the `rate_limit_event`.
  Buffered `json` retained for the disarmed case.
- **New `usage.rs`**: the `usage-headroom.json` store — resolved-identity-keyed, TTL/reset
  actionability, and the `SessionStore` lock/atomic/merge/corrupt-preserve/stale-reject
  discipline (shared with, or factored out of, `session.rs`).
- **`tools.rs`**: gated **entry selection ahead of preflight and capture** in `start_review`;
  the all-gated early `REVIEWERS_EXHAUSTED` before capture; fallback gating in the walk;
  `observe_headroom`-and-store after every attempt (both arms); non-billed gated-skip `Attempt`.
  Resume path unchanged (not gated).
- **`errors.rs`**: `reviewers_exhausted` parameterised by cause (rate-limited / gated / mixed),
  preserving the exact existing text for the pure-reactive case; `USAGE_BELOW_MINIMUM` as an
  attempt reason.
- **`metrics.rs`**: the billed/non-billed distinction on `Attempt` and the accumulator's
  completeness check; fixtures for all three attempt shapes.
- **`mcp.rs` / `--doctor`**: the per-entry minimum + last-observed-headroom column and the
  Claude-categorical caveat.
- **Docs/config**: `README.md` (a "Usage-remaining gate" section + both flags in the config
  table), this doc, `examples/` (a gated two-entry example), an **opt-in** `smoke.ps1` path
  (bills tokens; cost called out per `AGENTS.md`), and resolving the fallback-chain doc's
  "usage-remaining spike" decision gate to *signal verified → this plan*.

Not touched: the reactive fall-through logic, the capture pipeline, the session resume identity
match, the budget sizing, the cancellation protocol, the registry concurrency.

## Testing

Unit tests, no network and no model call, extending the existing fakes:

- **Config**: `--min-usage-remaining 10` binds to the last `--reviewer` (Codex); before any
  `--reviewer` errors; twice errors; `0`/`101`/non-integer error; on a Claude entry errors
  (family mismatch). `--min-usage-status warning` binds (Claude); an unknown word errors; on a
  Codex entry errors. Unset ⇒ `None`. Two entries differing only by minimum are still a rejected
  duplicate. `chain_gates_on_usage()` true iff some entry has a positive/categorical minimum,
  false for `0`/unset.
- **Headroom parse**: Codex rollout fixture → `Fraction` with the limiting window's remaining
  **and that window's `resets_at`** (primary+secondary → the lower-remaining window's reset is
  kept, the round-1-f3 case); Claude `stream-json` fixture → `Level` for
  `allowed`/`allowed_warning`/`rejected`; an unrecognised Claude status → `Unknown`;
  malformed/absent → `Unknown`; the `result` event parsed from `stream-json` yields a `Parsed`
  field-equal to the buffered-`json` parse (truncation, denials, usage, warnings, CLI events
  covered).
- **`clears`**: `Unknown` clears everything; `Fraction` clears iff `remaining ≥ min`; `Level`
  clears iff `level ≥ min` on `Ample>Warning>Exhausted`; a record past its `resets_at`, and a
  record beyond TTL (including one with `resets_at=None`), both read as `Unknown`.
- **Observation channel**: `observe_headroom` returns a value on a **failed** (`RATE_LIMITED`)
  turn, not only success; it reads CLI-owned events, never model prose; disarmed ⇒ never called.
- **Gate/selection** (scripted fakes, no live model): a store showing entry 0 below its minimum
  ⇒ entry 0 is **skipped without preflight or spawn** and entry 1 is selected, preflighted, and
  attributed; entry 0 clearing ⇒ entry 0 runs; **all** entries gated ⇒ `REVIEWERS_EXHAUSTED`
  **before capture / pending-marker / preflight** (assert those side-effects did not run) whose
  detail says "usage below minimum" per entry; a mix of gated + rate-limited ⇒ the parameterised
  detail names both causes; the pure all-rate-limited detail is byte-for-byte today's.
- **Store lifecycle**: an attempt writes its identity's observation; a later fresh walk reads
  it; two same-model/different-`bin` (or different-`$CODEX_HOME`) entries keep separate records;
  a resolved-identity mismatch reads as `Unknown`; a concurrent writer cannot regress a newer
  observation (stale-write rejection); a corrupt file is preserved, not overwritten; a lock/IO
  error fails open with no panic.
- **Resume is not gated**: a resume whose bound entry's stored headroom is below its minimum
  still runs (assert not skipped); a rate-limited resume still returns the `fresh: true`
  remediation.
- **Metrics**: a turn with a non-billed gated-skip attempt plus a successful fallback is
  summarised **complete** (the skip does not taint it); a turn with a billed rate-limited
  attempt of unknown usage is still **partial**; fixtures cover billed, non-billed, and no
  attempts.
- **No-gate invariant**: a chain with no minimum leaves Claude on buffered `json` (assert the
  invocation), reads no rollout, consults no store, calls no `observe_headroom`, and produces a
  byte-for-byte-today walk.
- **Surfaces**: `status`/`--doctor` render each entry's minimum and last-observed headroom
  (numeric Codex, categorical Claude, "unknown" before first observation or after TTL).

An **opt-in** `smoke.ps1` path may arm a two-entry chain and assert a real skip; it bills
tokens, so it is opt-in and its cost is called out per `AGENTS.md`.

## Open questions — resolved in round 1

The round-1 reviewer's recommendations are adopted:

1. **Codex rollout coupling** — kept, but **only as an armed, fail-open, bounded** post-turn
   read, after the persistence (§5), identity (§5), and bounds (§4) fixes. Not a live query;
   degrades to `Unknown` on any change.
2. **Armed-only observation, `0` disarmed** — adopted (§4, §1): the default path is untouched
   and `0` is a parse error rather than an inert arm.
3. **Claude numeric threshold** — **rejected as a config error**; Claude gets the categorical
   `--min-usage-status` instead (§1), so no numeric minimum is ever silently categorical.

## Review history

- **Round 1 (Codex, gpt-5.6-luna, effort=max) — REQUEST CHANGES.** 13 findings (8 major, 5
  minor), all accepted. Major: headroom was lost on the failure path (→ a parse-independent
  `observe_headroom` channel, §3); the gate sat after primary preflight and capture (→ gated
  selection moved ahead of both, §6); the Codex reset was not tied to the limiting window (§2);
  the raw identity missed PATH/account drift (→ resolved-identity keying, §5); the store lacked
  cross-process guarantees (→ `SessionStore` discipline, §5); gated skips would corrupt metrics
  completeness (→ non-billed attempt distinction, §7); `REVIEWERS_EXHAUSTED` was false for gated
  chains (→ cause-parameterised constructor, §6); a numeric Claude threshold was silently
  categorical (→ two family-appropriate flags, §1). Minor: `0` still armed the feature (§1);
  "byte-equivalent" was the wrong invariant (→ field-level `Parsed` equivalence, §4); the Codex
  rollout scan was unbounded (→ date-scoped, suffix-matched, tailed read, §4); a missing
  `resets_at` could gate forever (→ TTL, §5); the spike evidence did not establish every claimed
  contract (→ provenance + observed-vs-assumed labels, "Provenance").
