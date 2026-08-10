# Proactive usage-remaining gate — design

Status: **plan (round 7).** This document is the plan for the piece of [issue #48] that the
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
incidentally observed. **The probes were exploratory — trivial prompts, `claude-haiku-4-5`,
and (in one probe) `--include-partial-messages` — and so differ from the server's production
invocations ([claude.rs:85], [codex.rs:66]).** The design does not assume the probe command
equals the production command; the exact **armed** production invocation is specified in §4,
and its fixtures must be captured from that invocation, not from these probes (round-2
finding f13).

- **CLI versions**: `claude` **2.1.210 (Claude Code)**; `codex` **codex-cli 0.146.0**. On a
  different version any of the surfaces below may move; the design fails open when they do
  (see [Fail-open](#fail-open-is-the-load-bearing-property)).
- **Commands run** (trivial "reply OK" prompts, `claude-haiku-4-5-20251001`, a few cents):
  - `claude -p "…" --output-format json` — inspected the buffered result document's keys.
  - `claude -p "…" --output-format stream-json --verbose` — **and** the same with
    `--include-partial-messages` — inspected the JSONL event stream.
  - `codex exec --json --skip-git-repo-check -s read-only "…"` — inspected stdout events, and
    the rollout log written under `~/.codex/sessions/…` for the same run.
- **Verified (the design depends on these):** the buffered Claude `json` document carries
  **no** rate-limit field; the Claude `stream-json` stream carries a `rate_limit_event` whose
  terminal `result` event is otherwise the same document; **the `rate_limit_event` appears
  without `--include-partial-messages`** (10 lines, ~6.9 KB for a trivial reply — 6 `system`,
  2 `assistant`, 1 `rate_limit_event`, 1 `result`), so the armed invocation does not need
  that flag; the Codex `exec --json` **stdout** carries **no** `rate_limits`; the Codex
  **rollout log** does, in a `token_count` event; the `thread_id` on Codex stdout equals the
  id embedded in the rollout filename; a stable account identifier is readable **cheaply from a
  current local file** for **both** reviewers — Codex `tokens.account_id` in
  `$CODEX_HOME/auth.json`, Claude `oauthAccount.accountUuid`/`organizationUuid` in
  `~/.claude.json` — with no CLI call (the OAuth credentials themselves live elsewhere and are
  not read).
- **Assumed, and *not* depended on (labelled so):**
  - *"Once per turn."* Each probe emitted the event once, but the design never assumes a
    count — it takes the **last** such event in the stream/log, which is correct for zero, one,
    or many.
  - *Claude status values.* Only `status: "allowed"` was observed live (both probe accounts
    had headroom). `allowed_warning` and `rejected` are taken from Anthropic's documented
    enum, **not** observed here; a status this tool does not recognise maps to `Unknown`
    (fail-open), so an unlisted future value cannot mis-gate.
  - *Codex multiple windows.* The probe's `secondary` window was `null`; primary/secondary
    handling below is defensive, driven by the field shape, not by an observed two-window case.

[claude.rs:85]: ../src/reviewer/claude.rs
[codex.rs:66]: ../src/reviewer/codex.rs

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
  "info":{"total_token_usage":{...},"model_context_window":258400},
  "rate_limits":{"limit_id":"codex","plan_type":"pro",
    "primary":{"used_percent":83.0,"window_minutes":10080,"resets_at":1786826027},
    "secondary":null}}}
```

So Codex gives a **numeric** figure — `remaining % = 100 − used_percent` (here 17%) — **per
window**, each window an independent `(used_percent, window_minutes, resets_at)`. The
`thread_id` printed on stdout **equals the id in the rollout filename** (verified: stdout
`019fecc8-…` ↔ file `rollout-…-019fecc8-….jsonl`), so the server can locate the exact rollout
from what it already reads.

[codex.rs:265]: ../src/reviewer/codex.rs

### Claude — a categorical status, only in `stream-json`

The server invokes `claude -p --output-format json` ([claude.rs:86]) and parses one buffered
document. Measured, its keys are `type, subtype, is_error, …, usage, modelUsage,
permission_denials, terminal_reason, fast_mode_state, uuid` — `usage`/`modelUsage` are
**consumption only**; there is **no** rate-limit field. Switching to **`--output-format
stream-json --verbose`** adds a first-class `rate_limit_event` alongside a terminal `result`
event whose fields match today's buffered document:

```json
{"type":"rate_limit_event","rate_limit_info":{
  "status":"allowed","rateLimitType":"five_hour","resetsAt":1786393200,
  "overageStatus":"rejected","isUsingOverage":false},"uuid":"…","session_id":"…"}
```

So Claude gives a **categorical** signal — `status` ∈ {`allowed`, `allowed_warning`,
`rejected`} — plus the window kind (`rateLimitType`, e.g. `five_hour`) and a `resetsAt`. It
does **not** expose a numeric remaining percentage. This shape difference drives the config
grammar: **a numeric threshold is honest only for Codex.**

[claude.rs:86]: ../src/reviewer/claude.rs

### Fail-open is the load-bearing property

Both surfaces are **CLI internals**, not documented contracts. An absent, unparseable,
unrecognised, stale, or identity-mismatched signal is **`Unknown`**, and `Unknown` **never
gates**. If a CLI changes format, the gate silently reverts to today's reactive-only
behaviour rather than misfiring. This is relied on throughout.

## What the issue asks of the proactive layer

> … configure a fallback model when usage is beyond a certain threshold. … Minimum usage
> remaining is optional, if not specified then it will always be valid. For example, if usage
> remaining is less than 10% then instead of Claude Opus use GPT Luna. … If no fallback can be
> found meeting usage minimums then reject the review entirely stating as such.

A **per-entry minimum** on the command line; **optional** (unset ⇒ never gated); a **proactive
skip before spawning**; and a **hard rejection** when no entry clears its minimum.

## Proposal

### 1. Config — two family-appropriate flags, because the signals differ

A single numeric `--min-usage-remaining` cannot honestly gate Claude, which exposes no
percentage (round-1 finding f8). So the grammar is **two flags, each valid only for the
reviewer family whose signal fits it**, and a family mismatch is a **parse error**:

- **`--min-usage-remaining <1..=100>`** — Codex only (numeric). "Skip when last-observed
  remaining across all windows is known and below N %." `0`/`101`/non-integer are parse
  errors; `0` is rejected rather than silently arming the machinery for nothing (finding f10).
- **`--min-usage-status <ample|warning>`** — Claude only (categorical). Names the **lowest
  acceptable** status: `ample` skips on `allowed_warning` **or** `rejected`; `warning` skips
  only on `rejected`. `exhausted` is not offered (a minimum that never gates is meaningless).

Both bind to the most recent `--reviewer`, like `--model`/`--effort`/`--bin` ([config.rs:565]);
before any `--reviewer`, twice within one entry, an out-of-range number, or an unknown status
word are parse errors. Applying the Codex flag to a Claude entry (or vice-versa) is a parse
error naming the mismatch. `ReviewerSpec` carries the policy as one field:

```rust
pub enum UsageMinimum { None, Remaining(u8), Status(HeadroomLevel) }
pub struct ReviewerSpec { /* reviewer, model, effort, bin, */ pub usage_minimum: UsageMinimum }
```

**The minimum is a *gating policy*, not part of reviewer identity** — excluded from
`validate_chain`'s fully-identical-duplicate rule (two entries differing only by minimum are
still the same account, still not a fallback, still rejected) and from the session record's
resume identity (editing a threshold between runs must not break resume).

### 2. A normalized headroom signal both adapters feed

```rust
pub enum Headroom {
    Unknown,                                                    // fail-open: never gates
    Fraction { remaining_pct: f64, resets_at: Option<u64> },    // Codex, the limiting window
    Level    { level: HeadroomLevel, resets_at: Option<u64> },  // Claude
}
pub enum HeadroomLevel { Ample, Warning, Exhausted }
```

**Codex → `Fraction`, tied to the *limiting* window** (finding f3): `remaining_pct = 100 −
max(used_percent over windows)`, and `resets_at` is the `resets_at` of *that same* max-used
window (not the nearest reset). **Claude → `Level`** from `status`
(`allowed`→`Ample`, `allowed_warning`→`Warning`, `rejected`→`Exhausted`, anything else →
`Unknown`), `resets_at` from `resetsAt`.

**The "clears the minimum?" decision** compares each shape only against its own-shaped minimum
(the grammar guarantees a Codex entry carries `Remaining(_)`, a Claude entry `Status(_)`):

- `Unknown` ⇒ **clears** (fail-open), always.
- `Fraction { remaining_pct }` vs `Remaining(min)` ⇒ clears iff `remaining_pct >= min`.
- `Level { level }` vs `Status(min)` ⇒ clears iff `level` is at least as ample as `min`.

`HeadroomLevel` carries an **explicit rank** (`Exhausted = 0 < Warning = 1 < Ample = 2`) used
by a `match`/rank comparison — **not** a `#[derive(PartialOrd)]`, whose declaration-order
default would rank `Ample` lowest and invert the decision (round-2 finding f19). No numeric
threshold is ever applied to a categorical signal, and no fabricated Claude "%" is produced.

### 3. Observing the signal — a channel that survives the failure path

Round-1 finding f1, sharpened in round 2: the observation is needed most on a `RATE_LIMITED`
**failure**, but `Reviewer::parse` returns `Parsed` only on success ([reviewer/mod.rs:138]),
and `Job::attempt` owns the raw `RunOutcome` yet returns only a bare `Failure` on the error
arm ([tools.rs:1734]) — so the walk's `Err` arm never sees the `RunOutcome` to observe from.
The fix is explicit: **headroom is extracted where the raw `RunOutcome` still exists, and
travels out on *both* arms.**

```rust
// attempt/run yield the observation next to the result, computed from the raw RunOutcome:
struct AttemptResult { outcome: Result<Parsed, Failure>, headroom: Headroom }
```

- Headroom is computed from the raw `RunOutcome` **before** it is converted to a `Failure`, so
  a rate-limited turn is observed exactly like a successful one.
- It reads only the CLI's own machine output — Codex's rollout `token_count.rate_limits`,
  Claude's `rate_limit_event` — treated as **CLI evidence, exactly like stderr and structured
  error events, never the model's prose** ([errors.rs:558] discipline).
- The **walk stores the observation after every attempt**, on both arms. A refusal with no
  parseable signal yields `Unknown` and writes nothing.
- Produced **only when the chain is armed** (§4), so a non-gating chain pays nothing.

### 4. Observing costs nothing unless armed — and the armed reads are bounded and safe

Observation is **armed per chain** iff some entry carries a non-`None` `UsageMinimum`;
`0`/unset never arm (finding f10). `Config` exposes `chain_gates_on_usage() -> bool`, mirroring
`chain_needs_capture()`. When disarmed, both adapters behave exactly as today.

**Codex, when armed** — a **bounded, post-turn** rollout read, no invocation change: take
`thread_id` from the `thread.started` stdout event; construct the path directly from **today's**
date (and the previous day, for a midnight rollover) —
`$CODEX_HOME/sessions/<yyyy>/<mm>/<dd>/rollout-*-<thread_id>.jsonl`, **not** a recursive
`sessions/**` walk (finding f12); at most one file matches the `thread_id` suffix; read a
**bounded tail** and take the **last** `token_count.rate_limits`. Any miss → `Unknown`.

**Claude, when armed** — the exact production invocation is **today's argument vector with
`--output-format json` replaced by `--output-format stream-json --verbose`, and nothing else
added** (in particular *not* `--include-partial-messages`; verified unnecessary). Two
consequences must be handled, or the switch would change a review's *outcome*, not just add an
observation:

- **JSONL-aware parse and failure classification (finding f14, still open after round 3).**
  The stream is not one JSON document, so the current whole-stdout `serde_json::from_str` and
  the generic `failure_for` path — whose diagnostics fold in raw stdout ([reviewer/mod.rs:710])
  and could let event or model text drive classification — **must not** be reused as-is for the
  armed case. Two distinctions are load-bearing:
  - **`result` is the model's review *content*, not classification evidence.** Round 3 caught
    that an earlier draft listed `result` among the "CLI-owned" fields; it is model prose and
    is used **only** as the review text, never as evidence — otherwise a review that *mentions*
    "rate limit" could drive classification, the exact failure [errors.rs:558] forbids. Failure
    is classified **only** from the CLI's own structured metadata: `type`, `is_error`,
    `subtype`, `api_error_status`, `stop_reason`, `terminal_reason`, `permission_denials`,
    `usage`, `session_id`. The armed parser extracts content and metadata from separate,
    named fields — it never runs free text through the classifier.
  - **A `rejected` rate-limit event classifies the turn as `RATE_LIMITED` (round-5 finding
    f14).** A `rate_limit_event` with `status: "rejected"` is the CLI telling us, in its own
    structured metadata, that the account has no capacity — so the armed parser maps it to the
    **`RATE_LIMITED`** code, exactly the trigger the reactive fall-through advances on. Mapping
    it to a *generic* failure would strand the chain (no fall-through, no `REVIEWERS_EXHAUSTED`),
    which is the whole point of the reactive layer this feature sits on. This holds whether or
    not a terminal `result` accompanies the rejection.
  - **The no-terminal-`result` case is defined.** If the stream carries events but **no**
    terminal `result` — the CLI died or was rejected mid-stream — the turn is a **failure**:
    a `rejected` `rate_limit_event` → `RATE_LIMITED` (above); otherwise classified from whatever
    CLI-owned metadata the stream carried (a `result`/error event with `is_error:true`), or,
    absent any, a defined "no result event" failure. The `rate_limit_event`, if present, is
    still captured as headroom by `observe_headroom` regardless. A missing `result` is never
    treated as an empty successful review.
- **One truncation contract, on raw stdout bytes, with explicit bounds (finding f15).** Round
  5 was right that the earlier "cap the retained result text" framing was a category error:
  buffered mode caps **raw stdout bytes** at `MAX_OUTPUT_BYTES` ([reviewer/mod.rs:31],
  [reviewer/mod.rs:600]) and *then* parses that possibly-truncated buffer ([claude.rs:136]), so
  a cap on the *parsed result text* is a different quantity and cannot be equated with it. The
  two modes read fundamentally different byte streams — a buffered JSON document vs a JSONL
  event stream — so there is **no** byte-level or content-level truncation *equivalence* between
  them, and none is claimed (this is the honest consequence of finding f11, not a gap). Instead
  the armed path has **one** truncation contract, the *same shape* as buffered mode's — **the
  raw bytes read from the child's stdout are bounded, and overrun before the terminal `result`
  is a defined `OUTPUT_TRUNCATED`** — with its own explicit constant because the stream is
  inherently larger than the document:
  - Buffered: `MAX_OUTPUT_BYTES` (today's `8 * 1024 * 1024` = 8 MiB).
  - Armed: `MAX_ARMED_STREAM_BYTES` = **`4 * MAX_OUTPUT_BYTES`** (32 MiB), plus a
    `MAX_ARMED_STREAM_LINES` = **500,000** event-line cap, plus the collect wall-deadline the
    walk already enforces. Concrete named constants, stated here so the implementation is not
    free to leave them implicit.

  These armed bounds are **practical sizing, not a proven ceiling** (the discipline
  `single-blocking-collect.md` already applies to the collect budget): they exist to stop an
  unbounded or pathological stream, and are set far above any realistic review's stream (a
  review whose text approaches 8 MiB is already extraordinary; its ~2× stream still sits well
  under 32 MiB). Because the two modes cap different streams, a *pathological* review right at
  the multi-MiB boundary could truncate in one mode and not the other — that residual is
  **documented, not hidden behind a false equivalence claim**, and it is bounded to sizes no
  real review reaches.

  **The contract is enforced by a cap-aware armed *runner*, at read time — not a post-run parse
  (round-6 finding f15).** The existing collector retains bytes only up to `MAX_OUTPUT_BYTES`
  but **keeps reading and discarding** past it ([reviewer/mod.rs:572]), so a parser that runs
  *after* the fact over a `RunOutcome` buffer can neither observe a 32 MiB overrun nor recover
  the discarded JSONL — the bound has to be enforced *while reading*. So the armed path does
  **not** reuse the shared post-run collector: it reads the child's stdout through its own
  reader that, incrementally as bytes arrive, (a) counts **raw bytes** and **event lines**
  against `MAX_ARMED_STREAM_BYTES`/`MAX_ARMED_STREAM_LINES`, (b) parses each JSONL line and
  retains only the latest `result`/`rate_limit_event` (the memory optimisation — it does not
  decide truncation, which is on bytes *read*), and (c) the moment either bound is exceeded
  **before** a terminal `result`, stops and returns the defined truncation outcome. This is a
  new armed reader/`RunOutcome` path in `reviewer/mod.rs`, distinct from the disarmed buffered
  read, and is in the blast radius below.

  **The `OUTPUT_TRUNCATED` failure must name the bound it hit (round-6 finding f15).**
  `errors::output_truncated` today hardcodes byte-oriented wording ([errors.rs:362]); a
  *line*-count or *deadline* breach reported with a byte message would misdescribe the cause.
  So the failure carries which bound tripped — bytes, lines, or the wall-deadline — either by a
  reason parameter or a per-cause detail, so the remediation is accurate. Tests exercise **both
  boundary cases** of the one contract: a stream just under both bounds whose terminal `result`
  is delivered and parsed, and one just over each bound (bytes, and separately lines) that fails
  as an `OUTPUT_TRUNCATED` whose message names the bound that tripped.

For a turn that completes **within bounds in both modes**, the behaviour-preservation invariant
is **field-level `Parsed` equivalence** — identical `result` text, `session_id`,
`denials`/`denial_count`/`denial_count_is_floor`, `warnings`, `usage`, `usage_is_cumulative`
(not byte-equivalence — a JSONL line is not a buffered document, finding f11). Truncation is
governed by the explicit contract above, not by this equivalence; the buffered `json` path is
retained unchanged for the disarmed case.

### 5. Persisting the observation — cross-process safety and verified account identity

The gate decides **before** spawning, but the signal is observed **after** a turn. The bridge
is a persisted store, `usage-headroom.json` in the state dir, that **reuses `SessionStore`'s
exact discipline** ([session.rs:247], [session.rs:318]): an `ExclusiveLock` across a
read-modify-write, atomic rename, corrupt-file preservation, and stale-write rejection (an
entry advances only when the incoming `observed_at` is newer, finding f5). Any lock/IO error
degrades that one operation to `Unknown`/no-op.

**Keyed by resolved identity *and* a verified account fingerprint, read from a *current local*
source — never a preflight, never a cache (findings f4 and the round-3 f2 regression).** A
resolved binary path is not enough: the same binary and home can be re-authenticated as a
*different account*, so `$CODEX_HOME`/`bin` names storage, not the logged-in identity. Round 3
rejected two tempting shortcuts: obtaining the fingerprint from `auth_check` **re-introduces a
preflight before the gate** on a cache miss (the f2 regression), and a **process-lifetime
cache goes stale** across an account switch (f4). Both are avoided by reading the identity from
the **same current, local, cheap file the CLI itself keeps its logged-in account in** — a plain
file read, no CLI call, no preflight, and *current* (it changes on re-login, so a switch is
detected):

- **Codex** — `tokens.account_id` from `$CODEX_HOME/auth.json`. (Verified present; the
  *identifier*, never the OAuth tokens.)
- **Claude** — `oauthAccount.accountUuid` (with `organizationUuid`) from the CLI's account file
  `~/.claude.json` (honouring `CLAUDE_CONFIG_DIR`/`HOME` as the CLI does). (Verified present;
  the account *identifier*, never the credentials in the separate `~/.claude/.credentials.json`,
  which this tool does not read.)

**One read per review, at launch, carried through — no read-time/observe-time TOCTOU (round-5
finding f4).** The fingerprint is read **once, at the start of the review (selection/launch)**,
and that single launch-time value is carried through the whole review: it is what the gate
compares against the stored snapshot, *and* it is what a fresh observation is written under
when the turn completes. Reading it a second time at observation would open a
time-of-check/time-of-use gap — an account switch mid-review could file the observation under a
different identity than the gate decision used. Binding both to the one launch-time read closes
that: within a review the identity is fixed, and the turn's own auth (the preflight that runs
right after launch) executes under that same account. **Across reviews, the next launch reads
the fingerprint afresh; if it differs from the stored one the snapshot is `Unknown`** and does
not gate. **When the fingerprint cannot be read at all, the observation is `Unknown` and the
entry is not skipped** (fail-open — the reviewer's "current local identity source, or treat as
Unknown"). Proactive gating is therefore only ever a skip on a *positively matched* launch-time
identity; an account switch between reviews flips it immediately and the stale snapshot stops
gating. A genuine auth failure is still surfaced by the normal preflight when the selected entry
runs — the gate neither triggers nor masks it.

**Actionable only while fresh and unreset (finding f9).** A snapshot gates only if `now <
resets_at` **and** `now − observed_at < TTL` (the window's own length when known —
`window_minutes` for Codex, `rateLimitType`→duration for Claude — else a conservative default
cap), **and** the account fingerprint matches. A missing `resets_at` still expires via TTL.

This is the honest form of "proactive": act on the freshest matched-identity observation; a
first review against a never-seen (or identity-changed) entry is **ungated**.

### 6. The gate in the walk — skip before publishing active, select before capture

Round-1 finding f2 (gate must precede primary preflight and capture) and round-2 findings f7,
f17, f18 together pin the exact placement:

- **Selection (fresh review), before capture, the Perforce pending-marker, and *any*
  auth preflight or model spawn.** Walk the chain from entry 0; for each entry with a minimum,
  read its launch-time account fingerprint from the **local file** (Codex `auth.json`, Claude
  `~/.claude.json`) — **not** an `auth_check`, so selection never triggers a preflight (the
  round-3 f2 regression) — resolve its binary with the cheap `resolve_bin` PATH scan to form
  the store key, read the store, and skip the entry if its observation is matched-identity,
  actionable, and below-minimum. The first entry that clears is the start entry. If **every**
  entry is gated out, return `REVIEWERS_EXHAUSTED` (gated variant) with **no auth preflight, no
  capture, no pending-marker, no spawn, and no billing**. (Round-5 finding f21: the store key
  is `resolved-bin` + fingerprint, so `resolve_bin` — a PATH scan, no auth, no model — *does*
  run to form it; "nothing resolved" would be inaccurate. It is only the *auth* preflight,
  capture, marker, spawn, and billing that the all-gated path avoids. A `resolve_bin` failure
  during selection yields no key → `Unknown` → the entry is not gated and takes the normal
  path, where its `CLI_NOT_FOUND` surfaces.)
- **Skipped-entry metadata is carried into the `Job` and seeded into its `metrics_attempts`
  (finding f17)**, because selection runs before the worker while the attempt history is built
  inside the walk. A pre-start skip of entry 0 therefore still appears as its promised
  non-billed `Attempt` (§7).
- **In the worker loop, the gate check is the *first* action of an iteration — before
  `set_active` (finding f18)** — so a skipped fallback is never published as the active running
  reviewer, and before the lazy auth-preflight so a skip spawns and resolves nothing.
- **A gated skip spawns nothing and bills nothing.**
- **Terminal handling never falls through to `WORKER_PANICKED` (finding f7).** When the *last*
  entry in the walk is gated (whether earlier entries were rate-limited or gated), the walk
  sets the terminal `REVIEWERS_EXHAUSTED` outcome explicitly rather than leaving `outcome =
  None` and hitting the panic fallback ([tools.rs:1584]). The terminal outcome's active
  attribution names the last entry that actually ran (or, for an all-gated chain, carries no
  active entry — the failure detail enumerates the gated entries).
- **The gate applies to fresh walks only, never a resume** ([reviewer-fallback-chain.md,
  Sessions and resume]); a resumed entry that is genuinely out still hits the reactive
  `RATE_LIMITED` whose remediation points at `fresh: true`.
- **Exhaustion is honest per cause (finding f7).** `errors::reviewers_exhausted` is
  **parameterised by cause**: the pure all-rate-limited path keeps **today's exact text**
  ([errors.rs:225]); an all-gated chain says the entries were skipped because last-observed
  usage was below the configured minimum (remediation: wait for the window to reset, or
  lower/remove the minimum); a mixed chain enumerates each entry with its actual reason. All
  stay non-agent-correctable.

### 7. Metrics — a gated skip is *not* a billed attempt

Finding f6: the accumulator marks totals partial whenever `record.attempts` is non-empty
([metrics.rs:811]), assuming a fall-through attempt burned unreported tokens. A gated skip
burned none, so `Attempt` gains an explicit **billed/non-billed** distinction — a skip is
tagged non-billed (`USAGE_BELOW_MINIMUM` reason, no spawn, no usage) — and the accumulator's
`attempt_free` check becomes "no **billed** attempt with unknown usage": non-billed skips are
recorded for visibility but do not taint completeness. Fixtures cover billed, non-billed, and
no attempts. One `Record` per logical turn is unchanged.

### 8. Attribution, status, doctor

- **Attribution** is handled by the walk's existing `set_active`/terminal machinery
  ([tools.rs:1467]); §6 ensures a skipped entry is never published active.
- **`status` / `--doctor` gain a headroom column** per entry: its configured minimum (numeric
  Codex, categorical Claude, or "no gate"), and its **last-observed** headroom with
  `observed_at`/`resets_at` and whether that observation is still actionable or aged out to
  `Unknown`. **This adds no *model* call and no *extra* CLI call: the headroom is a store
  read, and `status`/`--doctor` already run per-entry auth checks today** ([tools.rs:894],
  [README.md:25]) — the plan does not claim `status` is CLI-free, only that the headroom
  column itself costs nothing beyond the store read (round-2 finding f16). This is where
  Claude's categorical-only limit is spelled out for a human.

[reviewer/mod.rs:31]: ../src/reviewer/mod.rs
[reviewer/mod.rs:572]: ../src/reviewer/mod.rs
[reviewer/mod.rs:138]: ../src/reviewer/mod.rs
[reviewer/mod.rs:600]: ../src/reviewer/mod.rs
[reviewer/mod.rs:662]: ../src/reviewer/mod.rs
[reviewer/mod.rs:710]: ../src/reviewer/mod.rs
[claude.rs:136]: ../src/reviewer/claude.rs
[tools.rs:894]: ../src/tools.rs
[tools.rs:1467]: ../src/tools.rs
[tools.rs:1584]: ../src/tools.rs
[tools.rs:1734]: ../src/tools.rs
[config.rs:565]: ../src/config.rs
[session.rs:247]: ../src/session.rs
[session.rs:318]: ../src/session.rs
[metrics.rs:811]: ../src/metrics.rs
[errors.rs:225]: ../src/errors.rs
[errors.rs:362]: ../src/errors.rs
[errors.rs:558]: ../src/errors.rs
[README.md:25]: ../README.md
[reviewer-fallback-chain.md, Sessions and resume]: reviewer-fallback-chain.md

## What this must not do

- **Must not change behaviour for a chain with no minimum configured.** Observation disarmed,
  Claude on buffered `json`, no rollout read, no store, no `observe_headroom` — byte-for-byte
  today's walk. `0` never arms.
- **Must not gate on an absent, unparseable, unmatched-identity, or stale signal.** `Unknown`
  always clears; a record gates only while `now < resets_at`, within TTL, **and** the account
  fingerprint matches; every lock/read/parse/identity error fails open. The gate skips only on
  positively matched knowledge that an entry is below its minimum.
- **Must not apply a numeric threshold to Claude** (parse error), nor produce an invented
  Claude "%".
- **Must not couple hard to a CLI's internal format** — a format change degrades to
  reactive-only.
- **Must not lose the observation on a failed turn**, nor read it from model prose:
  `observe_headroom` runs on both arms, from the raw `RunOutcome`, on CLI-owned fields only.
- **Must not change a Claude review's outcome when armed** — field-equivalent `Parsed` for a
  turn within bounds in both modes, JSONL-aware failure classification on CLI-owned fields
  (`result` is content, not evidence; a `rejected` event → `RATE_LIMITED`), and one explicit
  raw-byte truncation contract (`MAX_ARMED_STREAM_BYTES`/`_LINES`) sized far above any realistic
  review so the format switch does not truncate a review the buffered path would accept.
- **Must not run capture, the pending-marker, or a model spawn for an all-gated review**, must
  never publish a skipped entry as active, must never fall through to `WORKER_PANICKED`, and
  must not gate a resume.
- **Must not corrupt metrics** — a gated skip is non-billed; the shared store uses the
  lock/atomic/merge/stale-reject discipline.
- **Must not treat the minimum as reviewer identity** (excluded from the duplicate rule and
  resume matching).
- **Must not overstate `status`** — the headroom column adds no model call and no extra CLI
  call, but `status`/`--doctor` keep their existing per-entry auth checks.
- **Must not weaken the reactive chain** — `RATE_LIMITED` fall-through, single-entry
  `RATE_LIMITED`, capture, sessions, budget, and the pure all-rate-limited exhaustion text all
  stand.

## Blast radius

- **`config.rs`**: `UsageMinimum` on `ReviewerSpec`; parse + family/range validation for both
  flags; `chain_gates_on_usage()`; exclude the field from the duplicate rule and identity
  rendering; status/doctor rendering.
- **`reviewer/mod.rs`**: `Headroom` + `HeadroomLevel` (explicit rank) + `clears`; headroom
  carried out of `attempt`/`run` on both arms (the `AttemptResult` shape); a **new cap-aware
  armed reader/runner** — distinct from the buffered post-run collector ([reviewer/mod.rs:572])
  — that counts raw bytes and event lines incrementally at read time against
  `MAX_ARMED_STREAM_BYTES`/`MAX_ARMED_STREAM_LINES`, retains only the latest
  `result`/`rate_limit_event`, and emits the truncation outcome the moment a bound is exceeded.
- **`reviewer/codex.rs`**: armed rollout read (date-scoped, suffix-matched, tailed, fail-open);
  `tokens.account_id` fingerprint read from `$CODEX_HOME/auth.json`. No invocation change.
- **`reviewer/claude.rs`**: armed invocation (`json`→`stream-json --verbose`, nothing else
  added); JSONL-aware parse and failure classification on CLI-owned metadata only (`result`
  is content, not evidence); the total-input + retention bounds; the `oauthAccount.accountUuid`
  fingerprint read from `~/.claude.json`. Buffered `json` retained when disarmed.
- **New `usage.rs`**: the `usage-headroom.json` store — resolved-bin + account-fingerprint key,
  TTL/reset actionability, and the `SessionStore` lock/atomic/merge/corrupt-preserve/stale-reject
  discipline (shared with, or factored out of, `session.rs`).
- **`tools.rs`**: gated **selection ahead of capture/marker/preflight** in `start_review`, with
  skipped-entry metadata carried into the `Job`; the all-gated early `REVIEWERS_EXHAUSTED`;
  fallback gating as the first loop action (before `set_active`); explicit terminal exhaustion
  when the last entry is gated (no `WORKER_PANICKED`); `observe_headroom`-and-store after every
  attempt (both arms); non-billed gated-skip `Attempt`. Resume path unchanged.
- **`errors.rs`**: `reviewers_exhausted` parameterised by cause (rate-limited / gated / mixed),
  preserving the exact existing text for the pure-reactive case; `USAGE_BELOW_MINIMUM` reason;
  and `output_truncated` ([errors.rs:362]) generalised so its message names the bound that
  tripped (bytes / lines / deadline) rather than hardcoding byte wording.
- **`metrics.rs`**: the billed/non-billed distinction on `Attempt` and the accumulator's
  completeness check; fixtures for all three attempt shapes.
- **`mcp.rs` / `--doctor`**: the per-entry minimum + last-observed-headroom column and the
  Claude-categorical caveat; the wording that the column adds no model/extra-CLI call.
- **Docs/config**: `README.md` (a section + both flags), this doc, `examples/`, an **opt-in**
  `smoke.ps1` path (bills tokens; cost called out per `AGENTS.md`), and resolving the
  fallback-chain doc's "usage-remaining spike" decision gate to *signal verified → this plan*.

Not touched: the reactive fall-through logic, the capture pipeline, the session resume identity
match, the budget sizing, the cancellation protocol, the registry concurrency.

## Testing

Unit tests, no network and no model call, extending the existing fakes:

- **Config**: both flags bind to the last `--reviewer`; before any `--reviewer`, twice, out of
  range, unknown status word, and family mismatch (Codex flag on Claude entry and vice-versa)
  all error; unset ⇒ `None`; two entries differing only by minimum are still a rejected
  duplicate; `chain_gates_on_usage()` true iff some entry has a positive/categorical minimum.
- **Headroom parse**: Codex rollout fixture → `Fraction` with the limiting window's remaining
  **and that window's own `resets_at`** (the f3 two-window case); Claude `stream-json` fixture
  → `Level` for each status; unrecognised status → `Unknown`; malformed/absent → `Unknown`.
- **`clears` / ordering**: `Unknown` clears everything; `Fraction` clears iff `remaining ≥
  min`; `Level` clears by explicit rank (`Ample>Warning>Exhausted`) — a direct test that the
  rank is not the derived declaration order (f19); a record past `resets_at`, beyond TTL
  (incl. `resets_at=None`), or with a mismatched fingerprint reads as `Unknown`.
- **Observation channel (f1)**: `observe_headroom` returns a value on a **failed**
  (`RATE_LIMITED`) turn, not only success; reads CLI-owned fields, never model prose; disarmed
  ⇒ never called.
- **Armed Claude (f13/f14/f15)**: the armed invocation is exactly today's args with
  `json`→`stream-json --verbose` and no `--include-partial-messages` (assert the argv);
  failure classification uses only CLI-owned event fields (a `result` event with model prose
  containing "rate limit" is **not** misclassified); a `rejected` `rate_limit_event` classifies
  the turn as `RATE_LIMITED` (with, and without, a terminal `result`); a stream just **under**
  `MAX_ARMED_STREAM_BYTES`/`_LINES` yields its terminal `result` parsed, and one just **over**
  fails as a defined `OUTPUT_TRUNCATED` (both boundary cases of the one truncation contract);
  for a within-bounds turn, `Parsed` from the armed path is field-equal to the buffered parse
  (denials, usage, warnings, CLI events), with fixtures captured from the armed production
  invocation.
- **Account identity (f4)**: a snapshot recorded under account A is `Unknown` when the current
  fingerprint is B (Codex via a swapped `auth.json`; Claude via a swapped `~/.claude.json`
  result); an unreadable/absent fingerprint fails open (entry runs); a matching fingerprint
  gates.
- **Store lifecycle (f5)**: write/read round-trip; two same-model/different-`bin` (or
  different-`$CODEX_HOME`) entries keep separate records; a concurrent writer cannot regress a
  newer observation; a corrupt file is preserved, not overwritten; a lock/IO error fails open
  without panic.
- **Gate/selection (f2/f7/f17/f18)**: entry 0 below minimum ⇒ skipped **without preflight,
  `set_active`, or spawn** (assert the fake was not invoked and the entry was never published
  active) and entry 1 selected/attributed; the skipped entry 0 appears as a **non-billed**
  `Attempt` in the record (metadata carried from selection); all entries gated ⇒
  `REVIEWERS_EXHAUSTED` **before capture / marker / preflight** (assert those did not run);
  the *last* entry gated after an earlier rate-limit ⇒ a terminal `REVIEWERS_EXHAUSTED` (mixed
  detail), **not** `WORKER_PANICKED`; the pure all-rate-limited detail is byte-for-byte today's.
- **Metrics (f6)**: a turn with a non-billed gated skip plus a successful fallback is
  **complete**; a turn with a billed rate-limited attempt of unknown usage is still **partial**.
- **Resume**: a resume whose bound entry's stored headroom is below its minimum still runs
  (not gated); a rate-limited resume still returns the `fresh: true` remediation.
- **No-gate invariant**: a chain with no minimum leaves Claude on buffered `json` (assert the
  argv), reads no rollout, consults no store, calls no `observe_headroom` — a byte-for-byte
  today walk.
- **Surfaces (f16)**: `status`/`--doctor` render each entry's minimum and last-observed
  headroom; the test asserts the headroom column triggers no additional CLI call beyond the
  auth checks `status` already performs.

An **opt-in** `smoke.ps1` path may arm a two-entry chain and assert a real skip; it bills
tokens, so it is opt-in and its cost is called out per `AGENTS.md`.

## Open questions — resolved in round 1

1. **Codex rollout coupling** — kept, but **armed, fail-open, bounded** (§4).
2. **Armed-only observation, `0` disarmed** — adopted (§4, §1).
3. **Claude numeric threshold** — **rejected as a config error**; Claude gets the categorical
   `--min-usage-status` (§1).

## Review history

- **Round 1 (Codex, gpt-5.6-luna, effort=max) — REQUEST CHANGES.** 13 findings (**9 major,
  4 minor**), all accepted. Major: headroom lost on the failure path (f1); gate after primary
  preflight/capture (f2); Codex reset not tied to the limiting window (f3); raw identity missed
  PATH/account drift (f4); store lacked cross-process guarantees (f5); gated skips corrupted
  metrics completeness (f6); `REVIEWERS_EXHAUSTED` false for gated chains (f7); a numeric Claude
  threshold was silently categorical (f8); a missing `resets_at` could gate forever (f9). Minor:
  `0` still armed the feature (f10); "byte-equivalent" was the wrong invariant (f11); the Codex
  rollout scan was unbounded (f12); spike evidence did not establish every claimed contract
  (f13).

- **Round 2 (same session, turn 2) — REQUEST CHANGES.** Confirmed **resolved**: f2, f3, f5, f6,
  f8, f9, f10, f11, f12. **Still open** and carried into round 3: f1 (the `RunOutcome` is owned
  by `attempt`, which returns only `Failure` — observation must move before the failure
  conversion → the `AttemptResult` shape, §3); f4 (`$CODEX_HOME`/executable is storage, not
  account identity → a verified account fingerprint, §5); f7 (a *last-entry* gated skip after an
  earlier rate limit could still reach `WORKER_PANICKED` → explicit terminal exhaustion, §6);
  f13 (probe commands differ from production; capture fixtures from the exact armed invocation →
  Provenance, §4). **New** (all accepted): f14 (armed Claude must parse JSONL and classify
  failure on CLI-owned fields, not whole-stdout → §4); f15 (verbose stream can hit
  `MAX_OUTPUT_BYTES` before the terminal result → incremental bounded retention, §4); f16
  (`status` already runs auth checks; the no-CLI wording was wrong → §8); f17 (pre-start skips
  must be seeded into the walk's metrics history → §6); f18 (gate before `set_active` so a
  skipped fallback is never published active → §6); f19 (`HeadroomLevel` needs an explicit rank,
  not derived `Ord` → §2); f20 (round-1 severity counts were wrong → corrected above to 9 major /
  4 minor). f20 is a doc-accounting fix, applied here.

- **Round 3 (same session, turn 3) — REQUEST CHANGES.** Confirmed **resolved**: f1, f3, f5–f13,
  f16–f20 (16 of 20). **Open/regressed** and addressed in round 4:
  - **f2 regressed** — sourcing Claude's fingerprint from the cached `auth_check` re-introduced
    a preflight before the gate on a cache miss. Fixed by reading the fingerprint from a
    **current local file** (`~/.claude.json`), never `auth_check`, so selection triggers no
    preflight (§5, §6).
  - **f4** — a process-lifetime cache could reuse account A's fingerprint after a switch to B.
    Fixed by the same current-local-file read, which changes on re-login and so detects the
    switch immediately; unreadable/mismatched → `Unknown` (§5).
  - **f14** — `result` is model prose and must not be classification evidence; failure is now
    classified only from CLI-owned structured metadata, and the no-terminal-`result` case is
    defined (§4).
  - **f15** — the retention idea did not resolve the cap's dual role; round 4 defines **two
    independent bounds** (retention ≈ one document; a separate total-input bound ≥ 2×
    `MAX_OUTPUT_BYTES` whose breach before `result` is a defined `OUTPUT_TRUNCATED`), so a
    runaway stream is bounded and a review buffered mode would accept is not truncated (§4).

- **Round 4 (same session, turn 4) — REQUEST CHANGES**, restated on turn 5 (the client that
  collected turn 4 crashed on a text-encoding bug before its verdict was read; the doc was
  unchanged, so turn 5 re-reported the same assessment). Confirmed **resolved**: f1–f3, f5–f13,
  f16–f20 (no regressions). **Still open**, addressed in round 5:
  - **f4** — reading the identity file at *both* selection and observation is a TOCTOU gap if
    the account changes mid-review. Fixed: the fingerprint is read **once at launch** and that
    single value is carried through the review for both the gate decision and the observation
    write; across reviews a changed launch read invalidates the snapshot (§5).
  - **f14** — a `rejected` `rate_limit_event` was captured as headroom but not mapped to a
    failure code; a generic failure would block reactive fall-through. Fixed: a `rejected`
    event classifies the turn as **`RATE_LIMITED`**, with or without a terminal `result` (§4).
  - **f15** — "≥ 2× raw bytes" does not *prove* every buffered-acceptable stream fits. Fixed:
    the review-truncation threshold is pinned to the **retained result content** at
    `MAX_OUTPUT_BYTES` (content-equivalent to buffered), and the raw-read guard is reframed as a
    concrete *runaway* bound (bytes + line/event count + the collect wall-deadline), practical
    sizing rather than a proven ceiling; the unprovable raw-multiple claim is withdrawn (§4).
  - **f21 (new, minor)** — the all-gated path must `resolve_bin` to form the store key, so
    "nothing resolved" was inaccurate. Fixed: clarified that `resolve_bin` (a PATH scan, no
    auth, no model) runs to build the key, while auth preflight, capture, the marker, spawning,
    and billing are what the all-gated path avoids (§6).

- **Round 5 (same session, turn 6) — REQUEST CHANGES.** Confirmed **resolved**: f1–f14,
  f16–f21 (20 of 21). One open:
  - **f15** — the round-4 "pin the cap to retained *result content*" framing was itself a
    category error: buffered mode caps raw stdout **bytes** and then parses that buffer, so a
    content-text cap is a different quantity and cannot claim `Parsed` equivalence, and the
    armed bounds had no concrete values. Fixed by defining **one** truncation contract of the
    same shape as buffered mode — *raw bytes read* are bounded; overrun before the terminal
    `result` is a defined `OUTPUT_TRUNCATED` — with explicit constants (`MAX_ARMED_STREAM_BYTES
    = 4× MAX_OUTPUT_BYTES = 32 MiB`, `MAX_ARMED_STREAM_LINES = 500,000`, plus the collect
    deadline), framed as practical sizing; the equivalence claim is dropped entirely (there is
    none between two different byte streams, per f11), the residual boundary divergence is
    documented, and retention is separated out as a memory optimisation that does not decide
    truncation. Both boundary cases are tested (§4).

- **Round 6 (same session, turn 7) — REQUEST CHANGES.** Confirmed the conceptual part of f15
  resolved (explicit constants, false-equivalence claim removed) and f1–f14, f16–f21 still
  resolved — one implementation-mechanics gap left, now closed:
  - **f15** — the existing collector retains only up to `MAX_OUTPUT_BYTES` but keeps
    reading/discarding past it ([reviewer/mod.rs:572]), so a *post-run* parse could neither
    enforce the 32 MiB armed bound nor recover discarded JSONL; and `errors::output_truncated`
    ([errors.rs:362]) hardcodes byte wording, wrong for a line-limit breach. Fixed by specifying
    a **cap-aware armed reader/runner** that counts raw bytes and lines incrementally at read
    time (not a post-run parse), and generalising `output_truncated` to name the bound that
    tripped (bytes / lines / deadline) (§4, blast radius).
