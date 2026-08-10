# Proactive usage-remaining gate — design

Status: **plan.** This document is the plan for the piece of [issue #48] that the reviewer
fallback chain deliberately deferred: the **proactive** gate — "if usage remaining is less
than 10% then instead of Claude Opus use GPT Luna". [`docs/reviewer-fallback-chain.md`]
built the *reactive* chain (fall back only after a reviewer fails with `RATE_LIMITED`) and
recorded that the proactive gate depended on a signal it had **not verified existed**,
reserving a grammar slot but adding nothing until a spike proved the signal real. This plan
**runs that spike, records the verified result, and — because the signal is real — designs
the proactive gate on top of it.** Per this repository's rule it goes through the
`cross-review` gate before it is implemented.

[issue #48]: https://github.com/lack435/simple-cross-model-review/issues/48
[`docs/reviewer-fallback-chain.md`]: reviewer-fallback-chain.md

## The spike, and its result — the signal is real for both reviewers

The fallback-chain doc stated, honestly for what was known then, that a usage-remaining
figure "is not available to this server … not obtainable from any command this project
runs" and that "the CLIs surface a limit only as an *error* once it is already hit"
([reviewer-fallback-chain.md, "What exists today"]). **That is now falsified by
measurement.** Both reviewer CLIs expose a machine-readable usage-remaining signal on a
normal turn — before any limit is hit — but each in a different shape and on a different
surface. The evidence below was captured with trivial prompts (a few cents), inspected from
on-disk logs where possible; no capability is claimed that a command did not print.

[reviewer-fallback-chain.md, "What exists today"]: reviewer-fallback-chain.md

### Codex — a numeric percentage, in the rollout log (not on stdout)

`codex exec --json` writes JSONL **events to stdout** in the new `thread`/`turn`/`item`
schema, and the server already parses `turn.completed.usage` from it ([codex.rs:265]).
Measured, that stdout stream carries **no** rate-limit field:

```
{"type":"thread.started","thread_id":"019fecc8-bcdd-76e3-b7dd-dc8ad78ec428"}
{"type":"turn.started"}
{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"OK"}}
{"type":"turn.completed","usage":{"input_tokens":15488,"cached_input_tokens":5888,...}}
```

But Codex *also* writes a **rollout log** per session under
`$CODEX_HOME/sessions/YYYY/MM/DD/rollout-<ts>-<thread_id>.jsonl`, and **that** file carries
the headroom, in a legacy `event_msg`/`token_count` event, updated every turn:

```json
{"type":"event_msg","payload":{"type":"token_count",
  "info":{"total_token_usage":{...},"last_token_usage":{...},"model_context_window":258400},
  "rate_limits":{"limit_id":"codex","plan_type":"pro",
    "primary":{"used_percent":83.0,"window_minutes":10080,"resets_at":1786826027},
    "secondary":null,
    "credits":{"has_credits":false,"unlimited":false,"balance":"0"},
    "rate_limit_reached_type":null}}}
```

So Codex gives a **numeric** figure: `remaining % = 100 − used_percent` (here 17%), per
window (`primary`/`secondary`, `window_minutes: 10080` = a 7-day window), with a
`resets_at`. The `thread_id` printed on stdout **is the same id embedded in the rollout
filename** — verified: the stdout `thread.started` above and the file
`rollout-…-019fecc8-bcdd-76e3-b7dd-dc8ad78ec428.jsonl` match — so the server can locate the
right rollout deterministically from what it already reads.

[codex.rs:265]: ../src/reviewer/codex.rs

### Claude — a categorical status, only in `stream-json`

The server invokes `claude -p --output-format json` ([claude.rs:86]) and parses one buffered
result document. Measured, that document's keys are `type, subtype, is_error,
api_error_status, duration_ms, …, usage, modelUsage, permission_denials, terminal_reason,
fast_mode_state, uuid` — `usage`/`modelUsage` are **consumption only**; there is **no**
rate-limit field. (An earlier grep of transcripts turned up a `usage_window` /
`consumed_percent` object; probing proved it is *not* emitted by this surface — it was
agent-authored state. The verified-only discipline is why this was checked rather than
assumed.)

Switching the output format to **`--output-format stream-json --verbose`** changes that. The
stream then emits a first-class `rate_limit_event`, once per turn, alongside a final
`result` event whose shape is **identical** to today's buffered document:

```json
{"type":"rate_limit_event","rate_limit_info":{
  "status":"allowed","rateLimitType":"five_hour","resetsAt":1786393200,
  "overageStatus":"rejected","overageDisabledReason":"org_level_disabled",
  "isUsingOverage":false},"uuid":"…","session_id":"…"}
```

So Claude gives a **categorical** signal — `status` is `allowed`, `allowed_warning`, or
`rejected` — plus the window kind (`rateLimitType`, e.g. `five_hour`) and a `resetsAt`. It
does **not** expose a numeric remaining percentage.

[claude.rs:86]: ../src/reviewer/claude.rs

### What the spike settles

- The proactive signal issue #48 asks for **exists and is readable today**, for both
  reviewers, on a normal turn — the fallback-chain doc's "decision gate" resolves to the
  *"signal verified"* branch, and this plan is that branch.
- **The two signals are differently shaped** — Codex numeric, Claude categorical — so a
  single numeric threshold cannot be applied to both without a documented mapping. The design
  centres on that mismatch.
- **Neither signal is a free pre-flight query.** Both are observed *during/after* a turn
  (Codex's `token_count` events and Claude's `rate_limit_event` arrive mid-stream, and
  Codex's live in a post-hoc log). There is no command that reports headroom without running
  a turn. So a *proactive* gate — one that skips an entry **before** spending a model call —
  must gate on the **most recent prior observation**, persisted per entry. This is the one
  honesty constraint the whole design is built around, and it is stated plainly rather than
  dressed up as a live check.
- **Both surfaces are CLI internals.** The Codex rollout-log format and the Claude
  `rate_limit_event` are not documented contracts. The design therefore **fails open**: an
  absent, unparseable, or unrecognised signal is treated as *unknown*, never as "empty", and
  unknown never gates. If a CLI changes its format, the gate degrades to today's reactive-only
  behaviour rather than misfiring.

## What the issue asks of the proactive layer

> Provide a mechanism to configure a fallback model when usage is beyond a certain
> threshold. … Minimum usage remaining is optional, if not specified then it will always be
> valid. For example, if usage remaining is less than 10% then instead of Claude Opus use
> GPT Luna. … If no fallback can be found meeting usage minimums then reject the review
> entirely stating as such.

Four requirements, all now buildable:

1. A **per-entry minimum** usage-remaining, configured on the command line.
2. **Optional** — an entry with no minimum is always valid (never gated).
3. A **proactive skip**: an entry whose known remaining is below its minimum is passed over
   **before** it is spawned, advancing to the next entry.
4. A **hard rejection** when no entry clears its minimum (and none can otherwise run):
   `REVIEWERS_EXHAUSTED`, stating that the chain was gated out.

## What exists today, and where the gate attaches

The reactive walk is a `for` loop over the chain on the worker thread
([tools.rs:1454]): it publishes the active entry, preflights a fallback lazily, runs the
turn via `self.attempt(…)`, and on `RATE_LIMITED` records an `Attempt` and advances; any
other failure stops the walk; exhaustion yields `REVIEWERS_EXHAUSTED` ([tools.rs:1547]). A
**resume** runs exactly its one bound entry and never falls through ([tools.rs:1437]). The
per-entry identity — `ReviewerSpec { reviewer, model, effort, bin }` ([config.rs:85]) —
already threads through the run path, and each turn's `Parsed` ([reviewer/mod.rs:38]) is where
adapter output is normalised. Metrics already carry a per-turn `Attempt` history at schema
`v2` ([reviewer-fallback-chain.md, Metrics]).

The proactive gate slots into this without disturbing the reactive path: it adds **(a)** a
per-entry configured minimum, **(b)** an observed-headroom value produced by each adapter,
**(c)** a small persisted store of the last observation per entry, and **(d)** one check at
the top of each fresh-walk iteration, before `self.attempt`. When no entry configures a
minimum, none of it runs and behaviour is byte-for-byte today's.

[tools.rs:1437]: ../src/tools.rs
[tools.rs:1454]: ../src/tools.rs
[tools.rs:1547]: ../src/tools.rs
[config.rs:85]: ../src/config.rs
[reviewer/mod.rs:38]: ../src/reviewer/mod.rs
[reviewer-fallback-chain.md, Metrics]: reviewer-fallback-chain.md

## Proposal

### 1. Config — a per-entry `--min-usage-remaining`

`ReviewerSpec` gains one optional field; the flag binds to the most recent `--reviewer`,
exactly as `--model`/`--effort`/`--bin` do ([config.rs:565]):

```rust
pub struct ReviewerSpec {
    pub reviewer: ReviewerKind,
    pub model: String,
    pub effort: String,
    pub bin: Option<PathBuf>,
    /// Skip this entry proactively when its last-observed usage-remaining is *known* and
    /// below this percentage (0..=100). `None` = never gated. See docs/usage-remaining-gate.md.
    pub min_usage_remaining: Option<u8>,
}
```

Parse rules, all validated at parse time like the other identity flags:

- `--min-usage-remaining` **before any `--reviewer`** is a parse error (as with `--model`).
- The **same flag twice within one entry** is a parse error (forgotten `--reviewer`).
- A value outside `0..=100`, or non-integer, is a parse error.
- Unset ⇒ `None` ⇒ the entry is never gated (requirement 2). A value of `0` is also never
  gated — "remaining ≥ 0" is always true and, for the categorical mapping below, `0` clears
  every status — so `0` and unset are behaviourally identical, and that is documented rather
  than made a special case.

**`min_usage_remaining` is a *gating policy*, not part of reviewer identity.** This is the
one subtle interaction with the fallback chain, and it is decided deliberately:

- It is **excluded from `validate_chain`'s fully-identical-duplicate rule**
  ([reviewer-fallback-chain.md, Config validation]): two entries identical in reviewer,
  model, effort, and bin but differing only in `min_usage_remaining` are still the *same
  account*, so the second is still not a real fallback for the first — the duplicate rule
  keeps rejecting it. Threshold does not rescue a duplicate.
- It is **excluded from the session record's resume identity** ([session.rs], the raw
  reviewer/model/effort/bin match): a resume binds to the reviewer that holds the
  conversation, and the gating policy has no bearing on which reviewer that is. Editing an
  entry's threshold between runs must not break resume.

### 2. A normalized headroom signal both adapters feed

The two CLIs' signals are unified behind one type produced by the adapter and consumed by
the gate. It is deliberately a **three-state** value with an explicit `Unknown`, never a
number that defaults to zero:

```rust
pub enum Headroom {
    /// No signal was read (absent, unparseable, unrecognised format, or stale past reset).
    /// Always valid — never gates. This is the fail-open state.
    Unknown,
    /// Codex: a numeric remaining percentage for the *worst* (lowest-remaining) window,
    /// with when that window resets.
    Fraction { remaining_pct: f64, resets_at: Option<u64> },
    /// Claude: a categorical level, with when the window resets.
    Level { level: HeadroomLevel, resets_at: Option<u64> },
}

pub enum HeadroomLevel { Ample, Warning, Exhausted }
```

**The "clears the minimum?" predicate** is the single place the shape mismatch is resolved,
and its mapping is documented, not fudged:

- `Unknown` ⇒ **clears** any minimum. Fail-open, always.
- `Fraction { remaining_pct }` (Codex) ⇒ clears iff `remaining_pct >= min`. When Codex
  reports **more than one** window (`primary` and `secondary`), the adapter takes the
  **minimum remaining across them** (the most-constrained window) — over-gating on the
  tighter window is the safe direction, matching the fallback chain's "under-capturing is
  worse than over-capturing" reasoning.
- `Level { level }` (Claude) ⇒ clears iff `level == Ample`, **or** `min == 0`. Claude exposes
  no percentage, so a numeric threshold is interpreted ordinally: `Ample` (`status: allowed`)
  clears any minimum; `Warning` (`allowed_warning`) and `Exhausted` (`rejected`) clear only
  `min == 0`. The rationale is faithful to the signal: a *warning* status **is** Anthropic
  telling us headroom is low, so any operator who set a positive minimum wanted exactly this
  fallback. This coarseness — a Claude entry cannot distinguish "fall back below 10%" from
  "below 40%" — is a real limitation of Claude's categorical signal and is **documented as
  such** (in the README and `--doctor`), not hidden behind an invented number. This is the
  verified-only discipline applied to a threshold: we will not print a Claude "remaining %"
  we did not measure.

`Parsed` ([reviewer/mod.rs:38]) gains one field, defaulted so an adapter that reads nothing
degrades to `Unknown`:

```rust
pub headroom: Headroom,   // defaults to Headroom::Unknown
```

### 3. Observing the signal — per adapter, and only when the gate is armed

Reading headroom costs a rollout-log read (Codex) or a change of output format (Claude).
Neither should be paid, nor should any output-format change touch a chain that does not use
the feature. So **headroom observation is armed per chain**: it is enabled iff **any** entry
in the chain sets `--min-usage-remaining`. When disarmed, both adapters behave exactly as
today and `Parsed::headroom` stays `Unknown`. `Config` exposes a
`chain_gates_on_usage() -> bool` for this, computed once, mirroring `chain_needs_capture()`.

**Codex — read the rollout log by `thread_id`, fail open.** When armed, the Codex adapter:
1. captures the `thread_id` from the `thread.started` stdout event it already sees;
2. locates `$CODEX_HOME/sessions/**/rollout-*-<thread_id>.jsonl` (`$CODEX_HOME` defaults to
   `~/.codex`; the adapter reads the same env the CLI honours, and the search is scoped to
   the filename suffix so it is a bounded directory walk, not a content scan);
3. parses the **last** `token_count.rate_limits`, computes `remaining_pct = 100 − max(
   primary.used_percent, secondary.used_percent)` and the nearer `resets_at`.

Any failure at any step — env unset, file absent, unexpected JSON, `rate_limits` missing —
yields `Headroom::Unknown`. The coupling to Codex's on-disk format is **explicit and
fail-open**: verified against Codex 0.146.0 today, and if a future Codex drops or moves the
event the gate silently reverts to reactive-only. No invocation flag changes; this is a pure
post-turn read, so it cannot break a review.

**Claude — switch to `stream-json`, parse the `rate_limit_event`, reuse the `result`
parse.** When armed, the Claude adapter invokes `--output-format stream-json --verbose`
instead of `--output-format json`, reads the JSONL, and:
- finds the terminal `type == "result"` event and parses it with **exactly today's logic**
  (its shape is identical — verified), preserving all existing behaviour (text, denials,
  usage, session id, warnings);
- captures the last `rate_limit_event.rate_limit_info`, mapping `status` →
  `Ample`/`Warning`/`Exhausted` and `resetsAt` → `resets_at`.

Because the switch happens **only when the chain is armed**, a default single-Claude setup —
the common case — keeps the buffered `json` path untouched. A regression test asserts the
`result` event parsed out of `stream-json` is byte-equivalent to the buffered document for
the same turn, so the format switch cannot silently change a review's outcome.

### 4. Persisting the observation — the "before spawning" mechanism

The gate must decide **before** spawning an entry, but the signal is only observed **after**
a turn. The bridge is a small persisted store, `usage-headroom.json` in the state dir
(alongside the session store and metrics), keyed by **entry identity** — the same raw
`reviewer/model/effort/bin` tuple the session record already uses to distinguish accounts,
so two same-model/different-bin entries keep separate headroom:

```
{ "<identity-key>": { "headroom": <serialized Headroom>, "observed_at": <unix>, "resets_at": <unix?> } }
```

Lifecycle:

- **After every attempt that observed a signal** — success *or* a `RATE_LIMITED` refusal
  (Codex's rollout still carries `rate_limits` at the limit; Claude's `rate_limit_event`
  carries `status: rejected`) — the walk writes the fresh observation for that entry's
  identity.
- **Before spawning an entry** (fresh walk only, see below), the walk reads the store for
  that identity. A record whose `resets_at` is in the **past** is treated as `Unknown` (the
  window has rolled over; the old percentage is stale — fail open, do not gate on it). An
  absent record is `Unknown`.
- The store is **best-effort**: a read or write error is logged to stderr and treated as
  `Unknown`/no-op. It is an optimisation of proactivity across turns and process restarts,
  never a correctness dependency — losing it degrades to "first turn on this entry is
  ungated", which is exactly the honest floor (we have not observed this account yet).

This is the honest form of "proactive": the gate acts on the **freshest observation it
has**, and states plainly that the first review against an entry the server has never run is
ungated because there is nothing yet to gate on. It is strictly better than reactive-only —
an account known to be over threshold from a prior turn is skipped before it is billed again
— without pretending to a live headroom query that does not exist.

### 5. The gate in the walk — fresh walks only

One check is added at the top of each fresh-walk iteration in `run_review_walk`
([tools.rs:1454]), **after** publishing the active entry (so a poll still names it) and
**before** the fallback preflight and `self.attempt`:

```
if entry.min_usage_remaining is Some(min):
    headroom = headroom_store.get(entry.identity)          # Unknown if absent/stale/error
    if not headroom.clears(min):                            # known-and-below-minimum
        note gated attempt (USAGE_BELOW_MINIMUM, no usage, no spawn)
        if last entry -> REVIEWERS_EXHAUSTED (detail: gated + any rate-limited)
        else           -> advance to next entry
        continue                                            # nothing spawned, nothing billed
```

- A **gated skip spawns nothing and bills nothing** — its whole point. It records a
  `metrics::Attempt` with a new `failure_code = "USAGE_BELOW_MINIMUM"` and no usage (there
  was no turn), reusing the existing `v2` attempt history verbatim, so the log shows *why*
  the chain advanced.
- **Exhaustion is unified and honest.** If every entry is either gated out or rate-limited,
  the terminal `REVIEWERS_EXHAUSTED` detail enumerates each with its reason
  (`claude/opus: usage below minimum (10%); codex/luna: rate-limited`), realising the issue's
  "reject the review entirely stating as such". The existing `REVIEWERS_EXHAUSTED`
  constructor ([errors.rs, reviewers_exhausted]) is reused; only its detail string composes
  the two reasons.
- **The gate applies to fresh walks only, never a resume.** This mirrors the fallback
  chain's settled rule that "fallback selection happens only on a fresh review start"
  ([reviewer-fallback-chain.md, Sessions and resume]): a resume continues a conversation that
  lives on one specific reviewer, so silently gating it out and doing nothing would strand
  the caller. A resumed entry that is genuinely out of capacity still hits the **reactive**
  `RATE_LIMITED`, whose resume remediation already points at `fresh: true` — and a fresh
  restart *does* run the gate. So a stale account cannot trap a resume; it just takes the
  reactive path, which is the correct one for a resume.
- **A gate that removes the primary still preserves single-entry semantics elsewhere.** With
  one entry that gates itself out and no fallback, the walk has nothing to run, so it returns
  `REVIEWERS_EXHAUSTED` with a single-entry detail — the issue's "reject entirely" for the
  degenerate chain. (A one-entry chain with **no** minimum is, as ever, byte-for-byte today.)

### 6. Attribution, metrics, status, doctor

- **Attribution is already correct.** The walk publishes the active entry per iteration via
  `set_active` and attributes the terminal outcome to the entry that produced it
  ([tools.rs:1467], [reviewer-fallback-chain.md, Metrics]). A gated skip simply does not
  become the active *running* entry; the entry that actually runs is named exactly as a
  rate-limit fallthrough names it today.
- **Metrics.** Gated skips are `Attempt`s with `USAGE_BELOW_MINIMUM` and no usage; no new
  schema version is needed (the `v2` attempt list already tolerates arbitrary
  `failure_code`s and absent usage). One `Record` per logical turn is unchanged.
- **`status` / `--doctor` gain a headroom column.** For each entry they render its configured
  `min_usage_remaining` (or "no gate") and its **last-observed** headroom with `observed_at`
  and `resets_at` — numeric for a Codex entry, categorical for a Claude entry, "unknown
  (never observed)" before the first turn. This is the operator's window into *why* a chain
  would fall back before it does, and it is where the Claude-is-categorical limitation is
  spelled out. These surfaces are read-only and bill nothing (they read the store, not the
  CLI).

[tools.rs:1467]: ../src/tools.rs
[session.rs]: ../src/session.rs
[config.rs:565]: ../src/config.rs
[errors.rs, reviewers_exhausted]: ../src/errors.rs
[reviewer-fallback-chain.md, Config validation]: reviewer-fallback-chain.md
[reviewer-fallback-chain.md, Sessions and resume]: reviewer-fallback-chain.md

## What this must not do

- **Must not change behaviour for a chain with no minimum configured.** Observation is
  disarmed, Claude keeps buffered `json`, Codex reads no rollout, no store is consulted, the
  walk is byte-for-byte today's. The whole feature is inert until an operator sets a minimum.
- **Must not gate on an absent, unparseable, or stale signal.** `Unknown` always clears; a
  window past its `resets_at` is `Unknown`; every read/parse/store error fails open. The gate
  can only ever *skip on positive knowledge* that an entry is below its minimum.
- **Must not invent a number the CLI did not report.** Claude's categorical status maps to an
  ordinal decision, documented as coarse; no fabricated Claude "remaining %" is ever stored,
  gated on, or displayed.
- **Must not couple hard to a CLI's internal format.** The Codex rollout log and the Claude
  `rate_limit_event` are undocumented surfaces; a format change degrades the gate to
  reactive-only, never breaks a review.
- **Must not let the format switch change a Claude review's outcome.** `stream-json`'s
  terminal `result` event is parsed with today's logic and asserted byte-equivalent; the
  switch adds an observation, nothing else.
- **Must not gate a resume.** Proactive selection is a fresh-walk decision; a resume takes the
  reactive path and its `fresh: true` remediation.
- **Must not treat the minimum as reviewer identity.** It is excluded from the
  fully-identical-duplicate rule and from resume matching; a duplicate is still a duplicate
  and a resume still binds by reviewer/model/effort/bin.
- **Must not spend a model call on a gated entry.** A gated skip spawns nothing and records a
  no-usage attempt.
- **Must not weaken the reactive chain.** `RATE_LIMITED` fallthrough, `REVIEWERS_EXHAUSTED`
  on all-rate-limited, single-entry `RATE_LIMITED`, capture, sessions, and budget all stand;
  the gate is additive and runs ahead of the spawn, not instead of the reactive handling.

## Blast radius

Additive to the fallback chain; the reactive walk's structure is reused, not rewritten.

- **`config.rs`**: `ReviewerSpec.min_usage_remaining`; `PendingEntry` + parse + range
  validation for `--min-usage-remaining`; `chain_gates_on_usage()`; exclude the field from
  `validate_chain`'s duplicate rule; extend `describe`/status rendering to name the minimum.
- **`reviewer/mod.rs`**: `Headroom` + `HeadroomLevel` + the `clears(min)` predicate;
  `Parsed.headroom` (defaulted `Unknown`).
- **`reviewer/codex.rs`**: when armed, capture `thread_id`, locate and parse the rollout
  log's last `token_count.rate_limits`, fail open to `Unknown`. No invocation change.
- **`reviewer/claude.rs`**: when armed, invoke `--output-format stream-json --verbose`, parse
  the JSONL, reuse the `result`-event parse unchanged, and read the `rate_limit_event`. Buffered
  `json` path retained for the disarmed case.
- **New `usage.rs`** (or a section of `session.rs`): the `usage-headroom.json` store —
  identity-keyed, `#[serde(default)]`-tolerant, best-effort read/write, stale-by-`resets_at`
  handling.
- **`tools.rs`**: the pre-spawn gate check in the fresh walk; the post-attempt store write;
  gated-skip `Attempt` recording; the composed `REVIEWERS_EXHAUSTED` detail. Resume path
  unchanged (not gated).
- **`errors.rs`**: `USAGE_BELOW_MINIMUM` as an `Attempt` `failure_code` (metrics only) and the
  composed exhaustion detail; no new agent-correctable failure surface.
- **`metrics.rs`**: none beyond reusing the `v2` `Attempt` list for gated skips.
- **`mcp.rs` / `--doctor`**: the per-entry headroom column and the Claude-categorical caveat.
- **Docs/config**: `README.md` (a "Usage-remaining gate" section + the flag in the config
  table), this doc, `examples/` (a gated two-entry example), and an **opt-in** `smoke.ps1`
  path that arms the gate and asserts a skip (bills tokens; cost called out per `AGENTS.md`).
- **`docs/reviewer-fallback-chain.md`**: its "usage-remaining spike" decision gate is resolved
  to *signal verified → this plan* with a back-reference.

Not touched: the reactive fallthrough logic, the capture pipeline, the session resume
identity match, the budget sizing, the cancellation protocol, the registry's concurrency.

## Testing

Unit tests, no network and no model call, extending the existing fakes:

- **Config**: `--min-usage-remaining 10` binds to the most recent `--reviewer`; before any
  `--reviewer` errors; twice within one entry errors; `101` / non-integer errors; unset ⇒
  `None`; two entries differing only by minimum are still rejected as a duplicate;
  `chain_gates_on_usage()` true iff some entry sets it.
- **Headroom parse**: a Codex rollout fixture → `Fraction` with the worst-window remaining
  and `resets_at`; primary+secondary → the lower remaining wins; a Claude `stream-json`
  fixture → `Level` for each of `allowed`/`allowed_warning`/`rejected`; malformed/absent →
  `Unknown`; the `result` event parsed from `stream-json` equals the buffered-`json` parse.
- **`clears(min)`**: `Unknown` clears everything; `Fraction` clears iff `remaining ≥ min`;
  `Level::Ample` clears all, `Warning`/`Exhausted` clear only `min == 0`; a `Fraction` whose
  `resets_at` is in the past is read from the store as `Unknown`.
- **Gate in the walk** (scripted fakes, no live model): a store showing entry 0 below its
  minimum ⇒ entry 0 is **skipped without spawning** (assert the fake was not invoked) and
  entry 1 runs and is attributed; a store showing entry 0 clearing ⇒ entry 0 runs; every
  entry gated out ⇒ `REVIEWERS_EXHAUSTED` whose detail says "usage below minimum" per entry;
  a mix of gated + rate-limited ⇒ the composed detail names both reasons.
- **Store lifecycle**: an attempt writes the observation for its identity; a later fresh walk
  reads it; two same-model/different-bin entries keep separate records; a store read/write
  error fails open (entry runs, no panic); a legacy/absent store file loads as empty.
- **Resume is not gated**: a resume whose bound entry's stored headroom is below its minimum
  still runs the entry (assert not skipped); a rate-limited resume still returns the
  `fresh: true` remediation.
- **No-gate invariant**: a chain with no minimum leaves Claude on buffered `json` (assert the
  invocation), reads no rollout, consults no store, and produces a byte-for-byte-today walk.
- **Surfaces**: `status`/`--doctor` render each entry's minimum and last-observed headroom
  (numeric for Codex, categorical for Claude, "unknown" before first observation).

An **opt-in** `smoke.ps1` path may arm a two-entry chain and assert a real skip; it bills
tokens, so it is opt-in and its cost is called out per `AGENTS.md`.

## Open questions — for the reviewer

1. **Codex rollout coupling.** Reading `$CODEX_HOME/sessions/**/rollout-*-<thread_id>.jsonl`
   couples to an undocumented on-disk layout. The design fails open, but is a post-turn
   filesystem read of another tool's session log an acceptable dependency, or should the
   Codex direction be *documented as headroom-unavailable* until Codex surfaces `rate_limits`
   on `exec --json` stdout? (The plan keeps the read, fail-open; happy to drop it to
   Claude-only proactive if the coupling is judged too fragile.)
2. **Always-on vs armed observation.** The plan arms observation only when a minimum is
   configured, to keep the default path untouched. The alternative — always observe and show
   headroom in `status` even with no gate — is more visible but changes the default Claude
   output format. Armed-only is chosen for the no-change-by-default guarantee; is that the
   right call?
3. **Claude categorical mapping.** `Warning`/`Exhausted` gate for any positive minimum. Is
   collapsing Claude to "Ample vs not" the right honest reading of a signal with no
   percentage, or should a Claude entry with a numeric minimum be a **config error** (forcing
   the operator to acknowledge the mismatch) rather than silently coarse?
