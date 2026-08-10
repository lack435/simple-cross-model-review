# Reviewer fallback chain — design

Status: **proposed — revised after cross-review rounds 1–5.** This document is the plan. Per
this repository's own rule it must go through the `cross-review` gate (Codex, gpt-5.6-luna,
effort=max) and reach APPROVE before implementation begins. Rounds 1–5 each returned REQUEST
CHANGES — seven findings, then six, five, six, and six, all accepted; the sections below fold
each one in, and [Review history](#review-history) records where. It is the plan for [issue #48].

[issue #48]: https://github.com/lack435/simple-cross-model-review/issues/48

## What the issue asks

> Provide a mechanism to configure a fallback model when usage is beyond a certain
> threshold. The order has to be well defined as a command like argument. If the order is
> misconfigured then the tool should reject all requests with an error stating the
> configuration is invalid. Minimum usage remaining is optional, if not specified then it
> will always be valid. For example, if usage remaining is less than 10% then instead of
> Claude Opus use GPT Luna. Explicitly allow same family reviews if configured as such, but
> allow it to go through the same tooling. If no fallback can be found meeting usage minimums
> then reject the review entirely stating as such.

Five requirements are separable:

1. An **ordered list of reviewers** configured as command-line arguments.
2. **Config validation**: a misordered chain rejects all requests with an "invalid
   configuration" error.
3. A **trigger** that moves from one reviewer to the next.
4. A **same-family** case that is honoured when configured.
5. A **hard rejection** when the chain is exhausted with no reviewer able to run.

Four of the five are cleanly buildable on what exists today. The third — the trigger — is
the one that needs a decision, because the signal the issue names does not exist here.

## What exists today

The server runs **exactly one reviewer** for its whole process lifetime. `Config` flattens
the reviewer's identity and behaviour into one struct — `reviewer: ReviewerKind`,
`model: String`, `effort: String`, `bin: Option<PathBuf>` ([config.rs:274]) — and `App`
binds a single `Arc<dyn Reviewer>` and a single cached preflight at construction
([tools.rs:51], [tools.rs:63]). Nothing iterates over reviewers and nothing falls back:
a failing turn becomes a `Failure` the caller must act on.

[config.rs:274]: ../src/config.rs
[tools.rs:51]: ../src/tools.rs
[tools.rs:63]: ../src/tools.rs

A rate or usage limit is detected **reactively, and only after a run has already failed**.
`errors::classify` keyword-matches the reviewer CLI's own stderr and structured error
events — `"429"`, `"rate limit"`, `"quota"`, `"usage limit"`, `"too many requests"`,
`"overloaded"` — into the `RATE_LIMITED` code ([errors.rs:623]). Classification deliberately
ignores the reviewer's prose so that a review *mentioning* 429 is not misread as a limit
([errors.rs:558]); this discipline is load-bearing and the design below keeps it.

[errors.rs:623]: ../src/errors.rs
[errors.rs:558]: ../src/errors.rs

The token accounting in `metrics.rs` measures **consumption** — tokens, cost, timing, per
turn — not **headroom**. It answers "where did my tokens go", never "how much is left". No
code anywhere reads a usage-remaining percentage, a 5-hour or weekly window, or a
rate-limit budget, and neither reviewer CLI is parsed for one: the CLIs surface a limit only
as an *error* once it is already hit.

## The trigger problem, and the decision taken

The issue's headline mechanism — "if usage remaining is less than 10% then instead of
Claude Opus use GPT Luna" — is a **proactive** gate on a **usage-remaining percentage**.
That number is not available to this server. It is not tracked, not parsed from either CLI,
and not obtainable from any command this project runs. This repository's standing discipline
is to *claim only what was verified* (README, AGENTS.md), so a percentage gate must not be
designed on a signal we have not shown exists.

The signal that **does** exist is the reactive `RATE_LIMITED` classification above: the
reviewer itself telling us, by failing, that its account has no capacity right now.

Decision (confirmed with the maintainer):

- **Build the reactive chain now.** The trigger is a reviewer reporting a rate/usage limit.
  This is honest, feasible today, and needs no unverified capability.
- **Spike the proactive signal separately** (see [The usage-remaining spike]). Only if a
  machine-readable usage-remaining figure is *verified* to exist does a proactive
  per-entry threshold get designed on top. If it is not found, that negative result is
  documented and the reactive chain is the whole feature.
- **The chain is fully explicit in the args. There is no automatic fallback.** With one
  reviewer configured (today's setup), behaviour is byte-for-byte unchanged: a
  `RATE_LIMITED` surfaces to the caller exactly as it does now. A fallback happens only
  because the operator wrote a second reviewer entry into the args.
- **Same-family entries are honoured as written, with no special flag.** The chain is
  whatever the operator declared; if two entries share a family, that is the operator's
  choice and the tool follows it.

[The usage-remaining spike]: #the-usage-remaining-spike

## Proposal

### 1. Config: an ordered list of reviewer entries

Lift the four per-reviewer fields out of `Config` into a `ReviewerSpec`, and hold an ordered
non-empty `Vec<ReviewerSpec>`:

```rust
#[derive(Clone, Debug)]
pub struct ReviewerSpec {
    pub reviewer: ReviewerKind,
    pub model: String,
    pub effort: String,
    pub bin: Option<PathBuf>,
}

pub struct Config {
    /// The reviewer chain, in fallback order. Always non-empty; `reviewers[0]` is the
    /// primary and matches the single-reviewer behaviour that predates this field.
    pub reviewers: Vec<ReviewerSpec>,
    // ... all the process-global fields unchanged (cwd, timeout, state_dir, sandbox,
    //     allowed_tools, tools, preamble, isolate_reviewer, metrics, diff, vcs, ...).
}
```

**What stays global, and why.** Only the reviewer's *identity* is per-entry. The
behaviour flags — `--sandbox` (Codex-only), `--tools`/`--allow-tools` (Claude-only),
`--allow-reviewer-config`, `--preamble-file` — stay process-global. This is correct rather
than merely convenient: `sandbox` is read only by the Codex invocation and `tools` only by
the Claude invocation, so a global value already applies to whichever entries are of that
family and is inert for the others. A mixed chain therefore needs no per-entry behaviour
overrides to be correct. (Whether per-entry overrides are ever *wanted* is resolved against
in [Open questions].)

[Open questions]: #open-questions--resolved-in-round-1

### Per-entry adapter selection — every identity read must follow the active entry

Round 2 found that saying "invocation and auth use the active spec" was not enough: `App`
holds a single `Arc<dyn Reviewer>` built once at [tools.rs:66], the `Job` holds one adapter
and one `bin` ([tools.rs:720]), and a scatter of call sites read the *primary* `Config`
identity — `resolve_bin`, `failure_for`, the truncation/spawn error constructors, and
`classify`'s `reviewer` argument. If any of those keeps reading the primary while a fallback
runs, a `Codex → Claude` walk could invoke Claude but classify its output as Codex, or
resolve the wrong binary. So the design is explicit:

- **The adapter is selected per entry, not held once.** `reviewer::for_kind(spec.reviewer)`
  ([reviewer/mod.rs:126]) already builds an adapter from a `ReviewerKind`, and the adapters
  are stateless, so the walk selects `for_kind(spec.reviewer)` for the active entry rather
  than reusing `App`'s single one. `App` no longer needs a fixed `Arc<dyn Reviewer>`; the
  `Job` carries the *active* adapter, bin, and spec for the attempt it is running.
- **Every identity-bearing call takes the active `ReviewerSpec`** (or the fields it needs):
  `resolve_bin`, `auth_check`, `invocation`, `parse`, the truncation and `spawn_failed`
  constructors, `failure_for`, and the `reviewer`/`model`/`effort` arguments to
  `errors::classify`. None may read `self.cfg.reviewer`/`model`/`effort` on the run path.
  This is a mechanical but wide thread-through, and it is the core of the blast radius.
- **`ensure_ready` returns the resolved adapter+bin for the active entry**, so the run path
  uses exactly what preflight validated — no second, possibly-different resolution.

[tools.rs:66]: ../src/tools.rs
[tools.rs:720]: ../src/tools.rs
[reviewer/mod.rs:126]: ../src/reviewer/mod.rs

### The argument grammar

A repeated `--reviewer` **starts a new entry**; the identity flags `--model`, `--effort`,
`--bin` **bind to the most recent `--reviewer`**. Argument order is fallback order. This is
chosen over a delimited compound value (e.g. `--fallback codex:gpt-5.6-luna:max`) precisely
because Windows binary paths contain `:` and `\`, so any single-string grammar drowns in
escaping; the repeated-flag form has no delimiter to escape.

```
--reviewer claude --model claude-opus-4-8 --effort medium \
--reviewer codex  --model gpt-5.6-luna    --effort max
```

is a two-entry chain: try Claude Opus first, fall back to Codex on a rate limit. A single
`--reviewer claude --model …` is a one-entry chain — today's config, unchanged.

Binding rules, all validated at parse time so a slip is caught, not silently mis-bound:

- An identity flag (`--model`/`--effort`/`--bin`) that appears **before any `--reviewer`**
  is a parse error.
- The **same identity flag twice within one entry** (two `--model` between two `--reviewer`,
  say) is a parse error — it is almost always a forgotten `--reviewer`, and guessing which
  wins would hide the mistake.
- Per-entry defaults are applied per entry: an entry with no `--model` takes that reviewer's
  `default_model()`, no `--effort` takes its `default_effort()`, exactly as the single
  reviewer does today.
- The unknown-effort case stays a **non-fatal stderr warning** per entry ([config.rs:514]),
  deferred to surface as `MODEL_UNAVAILABLE` on first use, matching current behaviour.

[config.rs:514]: ../src/config.rs

### 2. Config validation: two tiers, and where each is reported

There are two kinds of "bad config", and they must not be conflated.

**Syntax errors keep failing fast at startup**, exactly as today: unknown flag, non-integer
`--timeout-seconds`, an identity flag before any `--reviewer`, a doubled identity flag within
an entry, `--diff` under Perforce. `Config::from_args` returns `Err(String)`, `main.rs`
prints it and `exit(2)` ([main.rs:55]). These describe a command line that cannot be parsed
into a chain at all; there is no server to run.

[main.rs:55]: ../src/main.rs

**Chain-semantic errors do not exit — the server starts and rejects every request in-band**
with a new `INVALID_REVIEWER_CHAIN` failure. This directly realises the issue's "the tool
rejects all requests with an error stating the configuration is invalid", and it is the more
useful of the two: an MCP server that `exit(2)`s shows the calling agent a generic dead
connection, whereas a running server returns a structured `Failure` whose remediation names
exactly what is wrong — which is this whole project's house style for failures. The round-1
reviewer endorsed keeping these in-band, with two conditions this plan adopts: the check
runs **before any reviewer preflight** (nothing is resolved or auth-checked on a chain that
is already known invalid), and the failure is **not agent-correctable** (`is_agent_correctable`
returns false — the caller cannot fix a server's command line). `--doctor` and
`cross_model_review_status` report the same problem for a human, before anything is billed.

The chain-semantic rule set (the deliberately small, defensible core — narrowed after
round 1):

- **A fully-identical entry is invalid.** Round 1 was right that `(reviewer, model)` is
  neither a complete nor a verifiable identity: `ReviewerSpec` also carries `bin`
  ([config.rs:275]), a distinct binary can be a distinct installation or account, and the
  CLI accepts model *aliases* (`opus` and `claude-opus-4-8` both resolve, [README.md:138])
  that this tool cannot canonicalise without asserting a mapping it has not verified. So the
  rule rejects only a duplicate of the **entire spec** — same `reviewer`, `model`, `effort`,
  *and* `bin`. That entry is unambiguously not a fallback for the one before it, whatever the
  provider's rate buckets turn out to be, and no valid same-model-different-bin (or
  different-effort) fallback is caught by mistake. The tool deliberately does **not** try to
  detect alias-vs-canonical duplicates: it would have to claim the alias mapping, which the
  verified-only discipline forbids.
- **The empty chain is rejected defensively even though it is unreachable.** `--reviewer`
  remains required, so `from_args` cannot produce an empty `reviewers`, and the existing
  "`--reviewer` is required" message ([config.rs:475]) still fires at startup. `validate_chain`
  additionally treats an empty vector as invalid, so a future construction path cannot slip a
  reviewer-less `App` past the guard.

[config.rs:475]: ../src/config.rs
[config.rs:275]: ../src/config.rs
[README.md:138]: ../README.md

Mechanically: `from_args` parses and syntax-validates into `Vec<ReviewerSpec>`. A new
`validate_chain(&[ReviewerSpec]) -> Result<(), Failure>` runs in `App::new`; on `Err`, `App`
holds the `Failure` and every `start_review` / `status` path returns it **first, before the
session lease and before any preflight**, so an invalid chain touches no reviewer. `main.rs`
no longer needs to distinguish the case — the degraded `App` reports itself. (Alternative —
fail fast at startup for chain-semantic errors too — is resolved against in
[Open questions], following the round-1 ruling.)

### 3. The fall-through, reactive and rate-only

The turn already runs on a **background worker thread** ([tools.rs:276]); the fall-through
lives there, so one `review_id` spans the whole walk. A **resume runs exactly one entry** (the
one bound to the session) with no fall-through — so this loop is the **fresh-review** path:

```
deadline = walk_start + chain_budget          # shared across the walk (see Budget below)
registry.set_active(id, chain[0])             # BEFORE capture, so a Capturing snapshot names it
capture = capture_change_if(chain_needs_capture())   # gathered once, capability-neutral
for i in 0 .. N:                              # N = chain length
    if i != 0: registry.set_active(id, chain[i])     # entry 0 already published above
    ready = ensure_ready(chain[i], cancel, deadline) # entry 0 -> shared-cache hit (preflighted
                                                     #   in start_review); fallbacks resolve here
    if ready is Err(f): return f.with_active(chain[i])   # preflight failure names THIS entry
    outcome = run_turn(chain[i], ready.adapter, ready.bin, capture, cancel, deadline)
    match outcome:
        Ok(review)                  -> return review.with_active(chain[i])
        Err(f) if f.code == RATE_LIMITED:
            if N == 1               -> return f          # single entry: plain RATE_LIMITED, no history
            note_attempt(chain[i], f)                    # attempt history for the logical turn
            if i == N - 1           -> return reviewers_exhausted(attempts).with_active(chain[i])
            else                    -> continue          # advance to the next entry
        Err(f)                      -> return f.with_active(chain[i])   # anything else surfaces at once
```

Two round-4 corrections are in this loop. **`N == 1` returns the existing plain
`RATE_LIMITED`** with no attempt history — `REVIEWERS_EXHAUSTED` is only ever a *multi-entry*
outcome, preserving single-reviewer behaviour exactly (the earlier pseudocode returned
`REVIEWERS_EXHAUSTED` for a one-entry chain, contradicting that rule). And **the selected
entry is published *before capture***, not merely before the first attempt, so a snapshot
polled during the `Capturing` phase ([tools.rs:907]) names the selected reviewer rather than a
default.

The active entry is published to the registry before its work, and every terminal result —
success, `REVIEWERS_EXHAUSTED`, *and* a non-rate failure of a fallback entry — carries the
entry that was actually being tried, so a fallback that fails in preflight or with
`NOT_AUTHENTICATED`/`TIMEOUT` is never misreported as the primary.

**No double preflight.** The per-entry preflight cache lives on `App` (shared between the
request thread and the worker), so entry 0 — preflighted in `start_review` before the worker,
as the single reviewer is today — is a cache hit when the loop calls `ensure_ready(chain[0])`.
Only *fallback* entries resolve inside the walk, which is exactly what the budget's `(N-1)`
term accounts for.

[tools.rs:907]: ../src/tools.rs

- **Only `RATE_LIMITED` advances the chain.** Per the maintainer's choice, confirmed by the
  round-1 reviewer, setup and correctness failures — `NOT_AUTHENTICATED`,
  `AUTH_EXPIRED_MIDRUN`, `CLI_NOT_FOUND`, `MODEL_UNAVAILABLE`, `SPAWN_FAILED`, `BAD_REQUEST`,
  `TIMEOUT`, `CANCELLED`, `EMPTY_REVIEW` — surface immediately. Falling back on these would
  mask a real misconfiguration behind a working substitute, which is worse than a clear
  error.
- **Preflight is per entry, lazy, and cancellable.** `ensure_ready` becomes keyed by entry
  (a small per-entry cache in place of the single `Option<Preflight>` at [tools.rs:59]). A
  fallback entry's CLI is only resolved and auth-checked when the walk reaches it, so a
  fallback whose CLI is absent never troubles a healthy primary — and when it *is* reached,
  its `CLI_NOT_FOUND` surfaces (not `RATE_LIMITED`, so it correctly stops the walk: the
  operator configured a fallback that does not exist). Round 1 flagged that `auth_check`
  today blocks up to 30 s behind a private, uncancellable timeout ([claude.rs:36],
  [codex.rs:27]); this plan threads the review's cancellation token and the shared deadline
  *into* `auth_check`, so a cancelled or budget-exhausted walk stops during a fallback
  preflight rather than after it.

**Budget across the walk.** Round 1 noted the advertised collect budget was sized for **one**
attempt: `Config::max_wait_secs` ([config.rs:575]) is the capture budget plus a single
`--timeout-seconds` plus grace, while each attempt gets its *own* deadline
([reviewer/mod.rs:424], invoked at [tools.rs:1253]). Round 2 then corrected my first fix: a
rate limit is **not** guaranteed to be detected quickly. A CLI can run almost to its timeout
and *then* fail in a way `classify` maps to `RATE_LIMITED` — there is no enforced fast path
for rate-limit classification. So "a rate-limited entry returns fast" cannot be leaned on for
the bound, and the earlier `capture + N×preflight_cap + timeout + grace` formula was wrong: N
rate-limited entries can each consume almost a full `--timeout-seconds`.

The sizing keeps the single-entry contract and, following this repository's existing framing,
is a **practical sizing, not a proven ceiling** — the same honesty
`docs/single-blocking-collect.md` already applies to the one-review budget
([single-blocking-collect.md:178]). Today `Config::max_wait_secs()` ([config.rs:575]) is
**capture + `--timeout-seconds` + finalization** — it does *not* include preflight, because
the selected entry's preflight runs in `start_review` *before* the worker and so before the
collect wait even begins. The chain keeps that exactly, and adds sizing only for the extra
work a chain introduces — the *fallback* attempts, whose preflight and run happen inside the
walk:

```
chain_budget = max_wait_secs_single                       # = today's capture + timeout + finalization
             + (N - 1) × (preflight_cap + timeout + drain_grace)   # each fallback attempt
```

- **`N = 1` is byte-for-byte today's budget** — the `(N-1)` term is zero. This is the
  invariant round 3 required; expressing `chain_budget` as *today's budget plus the fallback
  terms* makes it hold by construction rather than by a claim.
- **Lifecycle, stated explicitly.** The *selected* entry (entry 0 for a fresh review, the
  recorded entry for a resume) is preflighted in `start_review`, before the worker, exactly
  as the single reviewer is today — so its preflight is outside the collect wait and outside
  the deadline. The **shared deadline starts when the walk starts** (worker entry) and spans
  capture + the selected entry's turn + every fallback's preflight-and-turn. Only *fallback*
  entries (1…N-1 of them) are preflighted inside the walk, which is why only they add a
  `preflight_cap` term.
- **`preflight_cap` sizes the cancellable auth invocation; `resolve_bin` is an
  uninterruptible residual.** Round 4/5 established that `resolve_bin` scans PATH with
  synchronous `is_file()` calls and no deadline or cancellation ([reviewer/mod.rs:140],
  [reviewer/mod.rs:154]) — PATH can be arbitrarily long, so it is **not** a bounded fixed
  term and the plan no longer claims it is. `preflight_cap` sizes the part that *is*
  bounded and cancellable — `auth_check` (the timeout at [claude.rs:36]/[codex.rs:27], now
  given the cancellation probe below) and its output-drain grace — and the PATH scan is an
  explicit **uninterruptible residual** that sits outside the deadline, exactly the kind of
  residual `single-blocking-collect.md` already documents for the drain grace. `drain_grace`
  is the reviewer turn's own per-invocation drain ([reviewer/mod.rs:472]), counted once per
  attempt because each attempt is its own process.
- **The cancellation bridge is a single probe threaded through both phases.** Round 5 rightly
  said "pass the cancel token" was under-defined: today `auth_check` hands `run` a *fresh,
  uncancellable* `AtomicBool` ([reviewer/mod.rs:359]), and the selected entry's preflight runs
  in `start_review` *before* `try_start`/`attach_owned` exist ([tools.rs:219], [tools.rs:234]),
  so there is no registry stop-flag yet. The plan defines one **cancellation probe** — a
  cheap `Fn() -> bool` (or a `&dyn` cancel trait) — that both `auth_check` and `run` accept in
  place of the fresh `AtomicBool`. Before registry attachment (the selected entry's
  `start_review` preflight) the probe reads `RequestCancel`; after attachment (the worker's
  turns and fallback preflights) it reads the registry stop flag the review already carries.
  One abstraction, two backing sources, chosen by phase — so a `notifications/cancelled` during
  selected-entry auth, *before a review_id even exists*, stops setup, and a cancel mid-walk
  stops the fallback.
- `max_wait_secs`, and the budget shown in the start/running responses and progress text —
  which today all display `cfg.timeout` ([tools.rs:316], [tools.rs:466]) — are all recomputed
  from `chain_budget`. This is deliberately generous (three entries at a 30-minute timeout
  advertise a ~90-minute sizing), but it is the operator's own chain length, and an honest
  large sizing beats a wrong small one.
- An **optional refinement**, not relied on: a separate short cap on an attempt that has
  already produced rate-limit evidence could shrink the practical walk time. Noted as a
  future tightening; the sizing stands without it.

[tools.rs:276]: ../src/tools.rs
[tools.rs:59]: ../src/tools.rs
[tools.rs:219]: ../src/tools.rs
[tools.rs:234]: ../src/tools.rs
[tools.rs:316]: ../src/tools.rs
[tools.rs:466]: ../src/tools.rs
[tools.rs:1253]: ../src/tools.rs
[config.rs:575]: ../src/config.rs
[reviewer/mod.rs:154]: ../src/reviewer/mod.rs
[reviewer/mod.rs:359]: ../src/reviewer/mod.rs
[reviewer/mod.rs:424]: ../src/reviewer/mod.rs
[reviewer/mod.rs:472]: ../src/reviewer/mod.rs
[single-blocking-collect.md:178]: single-blocking-collect.md
[claude.rs:36]: ../src/reviewer/claude.rs
[codex.rs:27]: ../src/reviewer/codex.rs

### Capture in a mixed-family chain — the change must reach whoever runs

Round 1's most important finding: the plan wrongly claimed the capture pipeline was
untouched. It is not, because **what gets captured depends on the reviewer**. Under `--diff
auto`, the working-tree diff is supplied *only when the reviewer has no usable shell*
([config.rs:623], [vcs/mod.rs:89], [vcs/git.rs:445]): Codex always has a shell and is given
nothing to fetch itself, Claude has none and is handed the diff. A `Codex → Claude` chain
under `auto` therefore captures nothing for the primary — and if Codex is rate-limited and
the walk advances to Claude, Claude would receive **no diff and silently review the current
tree**, which is the exact failure the whole capture feature exists to prevent.

Round 2 sharpened this: the capture *decision* and the capability *rendering* are two
different things, and only the first can be an aggregate. The prompt tells the reviewer what
it can do — "you have a shell, run `git diff` yourself" versus "you have no shell, here is the
diff" — and **one rendered preamble cannot be true for both a Codex and a Claude entry.**
Perforce is sharper still: self-serve there needs Codex *and* `--sandbox danger-full-access`
([config.rs:655]), not merely "some entry has a shell". So the two concerns are split:

- **The capture *decision* is an aggregate: `chain_needs_capture()`.** `auto` captures
  whenever *any* entry would need the change — i.e. any entry lacks a usable shell — not
  merely when the primary does. Under-capturing produces a confident review of the wrong
  thing; over-capturing costs a little redundant work and some extra prompt text for a
  shell-capable entry, which is already the supported, harmless `--diff HEAD` mode. The
  asymmetry decides it. The captured change is gathered **once** at start-of-review.
- **The capture render must become capability-neutral.** Round 3 found the real obstacle: the
  retained `CapturedChange` holds only an *already-rendered* string ([shared.rs:44]), and that
  render bakes in the primary's shell posture — `git::render(&change, &cwd,
  cfg.reviewer_has_shell())` at [vcs/mod.rs:89], and the Perforce render likewise
  ([perforce.rs:194]). So a mixed chain would hand the fallback a block written for the
  primary; "render from retained data per entry" was not possible as the boundary stood. The
  fix draws the line where it belongs: the capture render describes **only the change** (the
  diff, status, untracked contents, truncation facts) and says nothing about what the
  *reviewer* can do; every statement about the reviewer's abilities — shell, self-serve, "the
  change was captured for you below" — lives in `reviewer_capabilities` ([config.rs:699]),
  which is rendered per **active** `ReviewerSpec` at turn time. The `has_shell` parameter to
  `git::render` (and its Perforce equivalent) is therefore removed: the captured block is
  rendered once, identically for every entry, and always includes the full change (which is
  exactly what a shell-less entry needs and a shell-capable one can ignore, the `--diff HEAD`
  case). `reviewer_has_shell` ([config.rs:583]) stays a per-reviewer predicate used by
  `chain_needs_capture()` and by `reviewer_capabilities`; nothing that varies by reviewer
  stays inside the retained render.
- **This does touch the VCS boundary**, so the round-2 "capture mechanics untouched" claim is
  narrowed: the git/p4 *commands*, truncation and labelling are untouched, but the **render's
  signature and the split of capability prose out of it** are in scope — `vcs/shared.rs`,
  `vcs/mod.rs`, and the git and Perforce render paths join the blast radius and get mixed-chain
  golden tests.
- **Every `mcp.rs` tool description must describe the chain, not just the primary.**
  `tools/list` today describes the single reviewer's shell/capture behaviour ([mcp.rs:636]),
  and `cross_model_review_status` formats `cfg.reviewer.as_str()` and one CLI/model
  ([mcp.rs:827], [mcp.rs:833]). Round 5 caught that the status description was still
  primary-only. So *all* tool descriptions — including `status` — render the chain honestly
  (the primary, plus that fallbacks exist and may differ), not one fixed posture.
- The classification-evidence and capture-*labelling* boundaries are unchanged: the change is
  still fenced and labelled as evidence, not instructions.

The other `--diff` modes are unaffected in spirit: `none` still supplies nothing, an explicit
range or `HEAD` still captures regardless of shell. Only `auto`'s "does anyone need it?" test
widens from the primary to the whole chain, and only the capability *text* moves from a
one-shot render to a per-attempt one.

[config.rs:623]: ../src/config.rs
[config.rs:583]: ../src/config.rs
[config.rs:655]: ../src/config.rs
[config.rs:699]: ../src/config.rs
[vcs/mod.rs:89]: ../src/vcs/mod.rs
[vcs/git.rs:445]: ../src/vcs/git.rs
[shared.rs:44]: ../src/vcs/shared.rs
[perforce.rs:194]: ../src/vcs/perforce.rs
[mcp.rs:636]: ../src/mcp.rs
[mcp.rs:827]: ../src/mcp.rs
[mcp.rs:833]: ../src/mcp.rs

### 4. Same family: honoured, not enforced

The tool cannot know the *calling* model — direction is set by the human in `.mcp.json` /
`.codex/config.toml`, and no caller identity crosses the MCP boundary. So the server cannot
enforce "reviewer ≠ author"; the operator owns that when composing the chain, exactly as
they own picking the single reviewer today. Consequently:

- A same-family entry (`claude-opus-4-8` → `claude-sonnet-…`, or one Codex model → another)
  is accepted and used as written. No flag gates it.
- The only composition the tool refuses is the *fully-identical* entry above (same reviewer,
  model, effort, and bin), because that one is not a fallback at all.

This is a deliberate narrowing of the issue's "explicitly allow same family reviews if
configured as such": there is nothing to *explicitly allow*, because nothing forbids it —
the explicitness lives entirely in the operator writing the entry.

### 5. Exhaustion: a distinct, honest rejection

When a **multi-entry** chain is walked to the end and every entry reported `RATE_LIMITED`,
the review is rejected with a new `REVIEWERS_EXHAUSTED` code whose detail enumerates each
entry and its outcome (`claude/claude-opus-4-8: rate-limited; codex/gpt-5.6-luna:
rate-limited`). Remediation: wait for a window to reset, or add an entry on an account with
capacity. Like `RATE_LIMITED`, it is not agent-correctable — it stops and tells the user.

A **single-entry** chain that is rate-limited returns plain `RATE_LIMITED`, unchanged: there
was no fallback to exhaust, and minting a new code for the no-fallback case would churn every
existing caller for no gain.

### Sessions and resume — the one correctness trap

A review that fell back to entry *k* runs its conversation on entry *k*'s reviewer. The
session record already stores `reviewer` / `model` / `effort` ([session.rs:42]), so the
identity of the entry that actually ran is persisted for free — the record simply reflects
the active spec instead of a fixed `cfg.reviewer`.

The trap is resume, and round 1 found a second half of it: **preflight ordering**. Today
`ensure_ready` runs at [tools.rs:175], *before* the session record is read ([tools.rs:189])
and matched. With a chain that is wrong twice over — it would preflight the **primary** even
for a resume that belongs to a fallback entry, so a session created by a Codex fallback could
die on the Claude primary's missing CLI or auth before Codex is ever selected. So the order
must change. A re-review resumes the reviewer's own conversation, whose memory lives on **one
specific** reviewer. Therefore:

- **Select the entry first, then preflight only that entry.** Under the session lease, read
  the record and choose the entry: for a resume, the entry whose identity matches the record;
  for a fresh start, entry 0 (then the walk). Preflight is run against the *selected* entry,
  never unconditionally against the primary. The `INVALID_REVIEWER_CHAIN` guard runs ahead of
  even this, so an invalid chain is reported before any record is read.
- **The session record stores the *raw configured* identity, so the match needs no
  preflight.** Round 2 caught that `SessionRecord` holds `reviewer`/`model`/`effort` but **no
  `bin`** ([session.rs:41]); round 3 then caught that persisting the *resolved* path would be
  self-defeating — comparing it would mean resolving other entries' `bin=None` to their paths,
  which is preflighting the very entries the walk must not touch, and a resolved path cannot
  tell a `bin=None` entry from an explicit `--bin` entry that happens to resolve to the same
  executable (both allowed to coexist). So the record persists the **raw configured
  identity**: `reviewer`, `model`, `effort`, and the raw `bin`. Resume matches this raw
  identity **against the configured chain's raw specs — a pure comparison, resolving
  nothing** — so no unselected entry is touched. Today's check compares only reviewer and model
  ([tools.rs:1535]); it now compares the full raw identity, effort included.
- **The raw-bin field is *tagged* so a new PATH entry is not mistaken for a legacy record.**
  Round 4 caught that a bare `Option<PathBuf>` with `#[serde(default)]` conflates "configured to
  resolve from PATH" (`bin=None`) with "field absent because the record predates it" — both
  deserialize to `None`, and the legacy rule below would then wrongly apply to a new
  PATH-backed session. So the field is a tagged `Option<RawBin>` where `RawBin` is
  `PathSearch | Explicit(PathBuf)`: a **new** record always writes `Some(PathSearch)` or
  `Some(Explicit(..))`, and only a **legacy** record deserializes to `None`. `#[serde(default)]`
  still gives the legacy `None`, but new records are never `None`.
- **The resolved path is persisted and verified against an *uncached* resolution.** Alongside
  the raw identity, the record stores the path the selected entry *resolved to*. Round 5 caught
  that the shared process-lifetime preflight cache ([tools.rs:57], [tools.rs:110]) would return
  the *old* `Preflight` without re-running `resolve_bin`, so a session resumed in the same `App`
  after PATH changed could pass the comparison against a stale path. So the resume verification
  **forces an uncached `resolve_bin`** for the selected entry, compares it to the stored path,
  and only then refreshes the cache. A mismatch (PATH now points at a different
  executable/account) is refused with `SESSION_NOT_RESUMABLE` rather than resuming the
  conversation through a different binary. A regression test prepopulates the cache before
  changing the resolved executable.
- **Both new fields are explicitly `#[serde(default)]`, so legacy session files still load.**
  Round 5 caught that a *required* resolved-path field would fail deserialization on existing
  session files, and the store reader treats a failed load as an empty store ([session.rs:362])
  — silently losing every legacy session. So the resolved path is a defaulted
  `Option<PathBuf>` (as the raw-bin field is a defaulted `Option<RawBin>`): a missing path is
  permitted *only* for a legacy `None` raw-bin record, while a new tagged record always writes —
  and on resume verifies — its path. A deserialization test runs against an actual pre-change
  session file.
- **The selected entry is then preflighted, and only it.** After the raw-identity match picks
  the entry, that one entry is preflighted (its `bin` resolved, auth checked); selection never
  depended on resolving anything.
- **Legacy and ambiguous records are refused, not guessed.** A `None` (legacy) record has no
  raw bin to compare; it resumes only if **exactly one** chain entry matches on the fields it
  does carry. If it matches none, or more than one (e.g. two same-model/different-bin entries),
  resume is refused with `SESSION_NOT_RESUMABLE` and the caller starts fresh — never silently
  bound to a guessed executable.
- **Fallback selection happens only on a fresh review start, never on resume.** A resume runs
  its bound entry only; it does not restart the walk from the top, because silently resuming a
  *different* reviewer would hand it a conversation it never had.
- If the resumed entry is itself rate-limited, the turn returns a **resume-specific**
  rate-limit failure whose remediation points at `fresh: true` — which *does* restart chain
  selection, at the known cost of the prior reviewer's memory. Round 1 noted the existing
  `errors::rate_limited` remediation only says "wait or change account" ([errors.rs:180]);
  rather than change that shared message (and with it single-entry behaviour), the resume
  path constructs its own `RATE_LIMITED`-coded failure carrying the `fresh: true` guidance in
  its detail. Same code, resume-aware remediation.
- If the record's identity no longer matches **any** configured entry (the chain was edited
  between runs), resume is refused with the existing `SESSION_NOT_RESUMABLE`, consistent with
  today's reviewer/model-mismatch check ([tools.rs:1535]). The caller retries with
  `fresh: true`.

[session.rs:41]: ../src/session.rs
[session.rs:42]: ../src/session.rs
[session.rs:52]: ../src/session.rs
[session.rs:362]: ../src/session.rs
[tools.rs:57]: ../src/tools.rs
[tools.rs:110]: ../src/tools.rs
[tools.rs:175]: ../src/tools.rs
[tools.rs:189]: ../src/tools.rs
[tools.rs:1535]: ../src/tools.rs
[errors.rs:180]: ../src/errors.rs

### Metrics, the result interface, status, doctor

- **The result must name the reviewer that actually ran — at every stage, not just at the
  end.** The registry's `Outcome` and `Snapshot` carry no reviewer identity
  ([registry.rs:150], [registry.rs:637]), and the completed *and running* responses render
  `self.cfg.describe_reviewer()` ([tools.rs:462], [tools.rs:474]) — the *chain/primary*, not
  the entry that reviewed. Round 2 added that a *running* snapshot must reflect the current
  attempt too, and that `Outcome` is written only at the end. So the running registry state
  holds a **mutable active identity** that the walk sets via `set_active` for the selected
  entry **before capture** and updates before each fallback attempt (see the pseudocode
  above) — round-4 finding 5: capture runs first in the worker ([tools.rs:907]), so a snapshot
  during `Capturing` would otherwise carry a default identity. Every `Outcome` carries the
  attempted-entry identity on success *and* failure, and both the running and completed
  responses render that identity instead of `describe_reviewer()`. A caller polling mid-walk
  sees the fallback; a fallback that fails in preflight is reported as the fallback. The
  earlier "registry untouched" claim is withdrawn.
- **`set_active` obeys the existing lock discipline.** Round 3 noted the mutable identity must
  not race the registry: `set_active` takes the same `State` mutex that `finish` and the
  snapshot path already hold ([registry.rs:432], [registry.rs:637]), stores an owned clone of
  the identity, updates only a review still `Running`, and snapshots copy it out under the same
  lock. No new lock, no new ordering.
- **The rendered identity includes the resolved bin.** Round 3 asked that same-model,
  different-bin entries be distinguishable in the result: the active/result/attempt identity
  carries reviewer, model, effort *and* the resolved binary path, so a caller can tell which
  executable (and thus which account) actually ran, not merely the provider configuration.
- **One logical turn, with an attempt history — turn semantics preserved.** Round 1 wanted a
  rate-limited primary to stay visible; round 2 warned that appending a second `Record` for
  the same review would double the metrics' *turn* contract — two turns, two wall-times, two
  per-session turns ([metrics.rs:304], [metrics.rs:742]). Both are satisfied by keeping
  **exactly one `Record` per logical turn** (the entry that reviewed, or the terminal
  outcome), which drives turn count, wall time and token totals exactly as today, plus a new
  optional `attempts` field on `Record`. Each `Attempt` carries the full identity — reviewer,
  model, effort, **and resolved bin** (round-4 finding 6: without bin, same-model/different-bin
  attempts are ambiguous in the log too) — its `failure_code`, its attempt-local wall time, and
  its own `prompt_bytes`. The rate-limited primary is therefore visible without inflating any
  turn statistic.
- **Prompt-byte accounting is defined, not left to chance.** Today `AttemptFacts` holds one
  prompt size ([tools.rs:775]); a fall-through sends several prompts. `Record.prompt_bytes`
  keeps its current meaning — the **final (terminal) attempt's** prompt size — and each
  `Attempt` carries its own `prompt_bytes`, so the per-attempt sizes are recoverable without
  redefining the top-level field.
- **Every record names its resolved bin, not only fallback ones.** Round 5 noted that a
  successful *single-entry* record has no `attempts`, so with bin only inside `Attempt` a
  same-model/different-bin single run is indistinguishable in the log. So `Record` gains an
  optional top-level resolved-bin field (additive, `#[serde(default)]`; no completeness
  impact, so it needs no version bump and rides on `v1` records too), matching the result
  interface's identity. The executable that ran is recoverable from every record.
- **Failed-attempt token usage is not claimed — and its absence propagates to completeness.**
  The adapter API returns `Result<Parsed, Failure>` and exposes `Parsed.usage` only on a
  successful parse ([reviewer/mod.rs:118]); a rate-limit refusal yields a `Failure` with no
  usage, and the CLI's usage on a refusal is not something this tool has verified it can read.
  So the `attempts` entries record *that* the attempt happened and how it failed, with usage
  left unknown — not a fabricated zero, and not an unverified figure. The adapter API is *not*
  widened to carry usage on failure; that would be surface for a number we cannot trust.
- **The accumulator must not report the logical turn as complete when an attempt's usage is
  unknown.** Round 3's sharpest point: "do not fabricate zero" is not enough if the unknown
  simply disappears. The summary derives completeness from the single `Record`'s usage
  ([metrics.rs:631], [metrics.rs:734]); a successful fallback's own usage looks complete, so a
  rate-limited primary that consumed unreported tokens would vanish and the turn's totals would
  read as complete. So `Accumulator::push` inspects the `attempts` metadata and marks the
  token/cost totals **partial / unknown** whenever any attempt carries usage it could not
  report — the same "not complete" signal the summary already uses for a reviewer that omits a
  field. One `Record` per turn stays; what changes is that the accumulator reads its
  `attempts`. Backward-compatibility fixtures cover records both with and without `attempts`.
- **The record schema version is bumped for attempt-bearing records.** Round 4 caught that
  leaving the version at `1` is unsafe: an *older* binary accepts `v == 1` and Serde silently
  ignores the unknown `attempts` field ([metrics.rs:33], [metrics.rs:496]), so it would read a
  fallback turn's usage as complete — defeating the partial/unknown semantics. Because the
  reader **skips and counts** any record whose version it does not recognise (it never guesses,
  [metrics.rs:497]), a record that carries `attempts` is stamped `RECORD_VERSION = 2`: an old
  reader skips it (under-counts visibly rather than misreporting), and the **new reader accepts
  both `1` and `2`**. A plain single-entry turn with no fallback stays at `v1`, so nothing an
  old reader could correctly read changes. This is a deliberate compatibility decision, stated
  so it is not a silent break.
- **`--doctor` / `status`** enumerate every entry with its resolved bin and auth, so a human
  sees the whole chain and any per-entry setup problem in one read. A degraded
  (`INVALID_REVIEWER_CHAIN`) config reports the validation failure here too.

[metrics.rs]: ../src/metrics.rs
[metrics.rs:33]: ../src/metrics.rs
[metrics.rs:304]: ../src/metrics.rs
[metrics.rs:496]: ../src/metrics.rs
[metrics.rs:631]: ../src/metrics.rs
[metrics.rs:734]: ../src/metrics.rs
[metrics.rs:742]: ../src/metrics.rs
[reviewer/mod.rs:118]: ../src/reviewer/mod.rs
[reviewer/mod.rs:140]: ../src/reviewer/mod.rs
[tools.rs:462]: ../src/tools.rs
[tools.rs:474]: ../src/tools.rs
[tools.rs:775]: ../src/tools.rs
[registry.rs:150]: ../src/registry.rs
[registry.rs:432]: ../src/registry.rs
[registry.rs:637]: ../src/registry.rs

## The usage-remaining spike

Run as a **separate, bounded investigation**, gating any proactive feature:

1. Determine whether `claude` and `codex`, run non-interactively, expose a machine-readable
   usage-remaining / rate-limit-headroom figure anywhere — a subcommand, a flag, a field in
   `--output-format stream-json`, or surfaced response headers. No model call is needed to
   find out; this reads help/status surfaces only.
2. **Decision gate.**
   - *If a signal is verified*: design an optional per-entry `--min-usage-remaining` gate
     that skips an entry whose remaining is below its threshold **before** spawning, and
     rejects with `REVIEWERS_EXHAUSTED` if no entry clears its minimum — realising the
     issue's literal 10% example and its "minimum usage remaining is optional; if unset it
     is always valid". The grammar slot is reserved for it but **not added until proven**,
     to avoid an inert flag.
   - *If no signal is found*: record the negative result with evidence (per the
     verified-only discipline), and the reactive chain stands as the complete feature. The
     issue's percentage semantics are then declared infeasible, on the record, rather than
     faked.

This ordering is why the config carries no `min-usage` field yet: the reactive chain is
correct and shippable on its own, and the proactive layer is additive when — and only
when — it is real.

## What this must not do

- **Must not change single-reviewer behaviour.** One entry ⇒ identical arguments, identical
  `RATE_LIMITED` surfacing, identical sessions, identical capture, identical collect budget.
  The chain is purely additive.
- **Must not let a fallback review the wrong change.** Under `auto`, the diff is captured
  whenever any chain entry needs it, so a shell-less fallback is never handed the working
  tree in place of the change.
- **Must not fall back on anything but a rate/usage limit.** Auth, missing CLI, bad model,
  timeout, empty review, bad request all surface immediately and stop the walk.
- **Must not preflight or bill an entry the review will not use.** An invalid chain is
  reported before any preflight; a resume preflights only its bound entry; fallback
  preflight stays lazy.
- **Must size its budget honestly.** The walk runs under one shared deadline the collect cap
  is sized to match, and auth checks are cancellable — but, as `single-blocking-collect.md`
  already says of the one-review budget, this is practical sizing, not a proven ceiling: the
  uninterruptible `resolve_bin` PATH scan is an acknowledged residual, not a hidden overrun.
- **Must not misattribute the review, at any stage.** The running snapshot and every terminal
  result — success or a fallback's own failure — name the entry actually being tried, and the
  metrics log keeps one logical turn plus an attempt history.
- **Must not tell one entry it has another's capabilities.** The capability preamble is
  rendered from the active entry, so a mixed chain never claims a shell (or Perforce
  self-serve) that the running entry lacks.
- **Must not read the primary's identity on the run path.** Every identity-bearing call —
  bin resolution, invocation, parse, classification, error construction — follows the active
  `ReviewerSpec`.
- **Must not weaken the classification boundary.** The reviewer's prose still never drives
  classification; only its stderr/structured errors do ([errors.rs:558]).
- **Must not resume a session on a reviewer that did not create it.** Fallback is a
  fresh-start decision only.
- **Must not claim a usage-remaining capability that has not been verified.** No proactive
  gate ships before the spike proves the signal exists.
- **Must not add an inert config surface.** `--min-usage-remaining` is designed, not added,
  until the spike lands.

## Blast radius

Larger than the recent features, because it lifts a field out of `Config` — accepted by the
maintainer as the cost of foundational completeness. Touched:

- **`config.rs`**: `ReviewerSpec`; `Config.reviewers: Vec<ReviewerSpec>` replacing the four
  flat fields; per-entry parse + binding validation; `validate_chain`; `chain_needs_capture()`;
  `describe_reviewer` renders the chain; `reviewer_capabilities` renders from an active
  `ReviewerSpec` and absorbs the reviewer-ability prose removed from the VCS render;
  `max_wait_secs` computes the shared `chain_budget` as *today's budget + fallback terms*.
- **`tools.rs`**: `App` holds the chain and a per-entry preflight cache (and, when invalid,
  the `INVALID_REVIEWER_CHAIN` `Failure`) and no longer a single fixed adapter; the `Job`
  carries the *active* adapter+bin+spec; the worker gains the fall-through loop, the shared
  chain deadline, and `set_active` before each attempt; the `INVALID_REVIEWER_CHAIN` guard and
  entry selection move ahead of preflight (fixing the [tools.rs:175] ordering); `ensure_ready`
  is keyed per entry, cancellable, and returns the resolved adapter+bin; `status` enumerates
  entries; resume matches the record's full identity against the chain; running *and*
  completed responses render the active entry rather than `describe_reviewer()`, and the
  displayed budget uses `chain_budget` not `cfg.timeout`.
- **`reviewer/mod.rs`, `reviewer/claude.rs`, `reviewer/codex.rs`**: per-entry adapter via
  `for_kind(spec.reviewer)`; `resolve_bin`, `auth_check`, `invocation`, `parse`, the
  truncation/`spawn_failed` constructors and the `classify` identity arguments all take the
  active `ReviewerSpec` (or its fields) rather than `cfg`; `auth_check` accepts the
  cancellation token + deadline so a fallback preflight is interruptible.
- **`registry.rs`**: a **mutable active identity** on the running state, set per attempt via
  `set_active` under the existing `State` mutex ([registry.rs:432]); `Outcome`/`Snapshot` carry
  the attempted reviewer/model/effort/resolved-bin so both running and terminal results name who
  reviewed. Concurrency, leasing and cancellation otherwise unchanged.
- **`mcp.rs`**: *all* tool descriptions — `tools/list` ([mcp.rs:636]) and
  `cross_model_review_status` ([mcp.rs:827], [mcp.rs:833]) — describe the chain honestly
  (primary plus differing fallbacks) instead of advertising the primary's posture as the only
  one.
- **`vcs/shared.rs`, `vcs/mod.rs`, `vcs/git.rs`, `vcs/perforce.rs`**: the `auto` capture
  *decision* generalises to `chain_needs_capture()` (capture if *any* entry needs it); the
  capture **render is made capability-neutral** — the `has_shell` parameter to `git::render`
  ([vcs/mod.rs:89]) and the Perforce render ([perforce.rs:194]) drop the reviewer-ability
  branch, which moves into `reviewer_capabilities`; the retained `CapturedChange` string is
  then identical for every entry. The git/p4 *commands*, truncation and labelling are
  untouched.
- **`errors.rs`**: two constructors — `invalid_reviewer_chain`, `reviewers_exhausted` — with
  codes, summaries, remediation, and `is_agent_correctable` returning false; a resume-aware
  `RATE_LIMITED` remediation path that does not disturb the shared `rate_limited` message; and
  a `.with_active(spec)` helper so a `Failure` can name the entry it came from.
- **`session.rs`**: `SessionRecord` gains a **tagged raw `bin`** (`Option<RawBin>` =
  `None` legacy / `Some(PathSearch)` / `Some(Explicit(..))`, `#[serde(default)]`) **and** a
  defaulted `Option<PathBuf>` **resolved path** — both `#[serde(default)]` so legacy files at
  [session.rs:362] still deserialize; resume matches the full *raw* identity (reviewer, model,
  effort, raw bin) against the chain **without resolving/preflighting other entries**, then
  verifies against a **forced-uncached** `resolve_bin` of the selected entry, refusing
  legacy-ambiguous or PATH-drifted matches with `SESSION_NOT_RESUMABLE`.
- **`metrics.rs`**: `Record` gains an optional `attempts` list of `Attempt`
  (reviewer/model/effort/resolved-bin, `failure_code`, wall, `prompt_bytes`; additive,
  `#[serde(default)]`); `RECORD_VERSION` bumps to `2` for attempt-bearing records with the new
  reader accepting `1` and `2` ([metrics.rs:33], [metrics.rs:496]); still one `Record` per
  logical turn, but `Accumulator::push` reads `attempts` and marks totals partial/unknown when
  an attempt's usage is unavailable ([metrics.rs:631], [metrics.rs:734]). `Record.prompt_bytes`
  keeps its meaning (final attempt).
- **`main.rs`**: unchanged except that chain-semantic invalidity is now reported by the
  degraded `App`, not `exit(2)`.
- **Docs/config**: `README.md` (Configuration table + a fallback section), `AGENTS.md` if
  the gate workflow is affected (it is not, but the arg grammar is documented), the
  `examples/` configs, and `smoke.ps1` if a chain path is worth an end-to-end check.

Not touched: the capture *mechanics* in `vcs/` (commands, truncation, labelling), the
registry's concurrency and leasing, the cancellation *protocol*, the progress protocol.

## Testing

Unit tests (no network, no model call), extending the existing fakes:

- **Parsing**: single `--reviewer` ⇒ one-entry chain (regression guard on today's
  behaviour); a two-entry chain preserves order; an identity flag before any `--reviewer`
  errors; a doubled `--model` within one entry errors; per-entry defaults fill in per entry.
- **Chain validation**: a fully-identical entry (reviewer, model, effort, bin all equal) ⇒
  `INVALID_REVIEWER_CHAIN`, and a degraded `App` returns it from `start_review` and `status`
  **before any preflight** (assert no bin resolution / auth check happened); a same-family,
  different-model chain is accepted; a same-model, different-bin chain is accepted; an empty
  vector is rejected by `validate_chain`.
- **Fall-through** (fake reviewer scripted to return `RATE_LIMITED` then `Ok`): primary
  limited ⇒ second entry runs and its review is returned; the **result names the second
  entry** (registry `Outcome`/`Snapshot` carry it); the logical-turn `Record` names the
  fallback and its `attempts` list contains the rate-limited primary; turn/wall counts do not
  double.
- **Running attribution**: a snapshot polled while the walk is on the fallback names the
  fallback, not the primary; a fallback that fails in preflight (`CLI_NOT_FOUND`) surfaces as
  the fallback.
- **Per-entry identity on the run path**: a `Codex → Claude` fall-through invokes the Claude
  adapter and classifies Claude's output as Claude (asserted with fakes, no live model) — the
  primary adapter/identity is never used for the fallback.
- **Cross-family capability**: a `Codex → Claude` chain under `--diff auto` captures the diff
  (because Claude needs it) even though Codex is primary; the retained capture block is
  **identical** for both entries (capability-neutral render), while `reviewer_capabilities`
  renders per active entry — the Codex attempt is told it may inspect the change itself, the
  Claude attempt that it has no shell and the change is captured below. A Perforce mixed chain
  asserts self-serve prose appears only for a Codex + `danger-full-access` active entry. All at
  rendering level, no live model.
- **Non-rate error does not fall through**: primary `NOT_AUTHENTICATED` (or `CLI_NOT_FOUND`)
  ⇒ surfaced immediately, chain not advanced.
- **Exhaustion**: every entry `RATE_LIMITED` ⇒ `REVIEWERS_EXHAUSTED` whose detail names each
  entry; a single-entry chain ⇒ plain `RATE_LIMITED` with **no** `attempts` history and a `v1`
  record (the single-entry contract).
- **Capture-phase attribution**: a snapshot polled while the worker is in `Capturing` names the
  selected entry, not a default (set_active precedes capture).
- **Schema compatibility**: an attempt-bearing `v2` record is skipped-and-counted by a reader
  that only knows `v1` (fixture-simulated), and read fully by the new reader; a `v1` record is
  read by both.
- **Prompt bytes**: after a fall-through, `Record.prompt_bytes` is the final attempt's size and
  each `Attempt.prompt_bytes` is its own.
- **Budget**: a single-entry `chain_budget` equals today's `max_wait_secs` exactly (the
  invariant); a multi-entry walk of rate-limited entries, each running near its
  `--timeout-seconds` before being classified, stays within `chain_budget`; the displayed
  budget equals `chain_budget`, not `cfg.timeout`; cancellation during a fallback
  preflight/auth check stops the walk promptly (assert the cancel token reaches `auth_check`).
- **Metrics completeness**: a logical turn whose `attempts` include a rate-limited primary with
  unavailable usage is summarised as **partial/unknown**, not complete, even though the
  successful fallback's own usage is complete; fixtures cover records with and without the
  `attempts` field (backward compatibility).
- **Resume**: a session created on entry *k* resumes on entry *k* only, does not restart the
  walk, preflights only entry *k* (a primary made unavailable *after* the session was created
  does not break the resume — the round-1 regression test), and — when *k* is rate-limited on
  resume — returns a `RATE_LIMITED` failure whose remediation names `fresh: true`; a record
  matching no configured entry ⇒ `SESSION_NOT_RESUMABLE`.
- **Resume identity**: two same-model/different-bin entries — a session created by the second
  resumes on the second (raw-identity match, no other entry preflighted); a legacy (`None`)
  record matching both is refused with `SESSION_NOT_RESUMABLE`, one matching exactly one entry
  still resumes; a `Some(PathSearch)` record is *not* treated as legacy; a resume whose selected
  entry now resolves to a different path than stored is refused (PATH drift) — **including when
  the preflight cache was warmed before the executable changed** (the forced-uncached resolve);
  and an actual pre-change session file still deserializes (both new fields default).
- **Cancellation before a review_id exists**: a `notifications/cancelled` during the selected
  entry's `start_review` auth (before `try_start`) stops setup via the `RequestCancel`-backed
  probe; a cancel mid-walk stops the fallback via the registry-stop-flag-backed probe.
- **Tool descriptions**: `tools/list` *and* `cross_model_review_status` describe the chain
  (primary plus differing fallbacks), not just the primary CLI.

A test seam is required so the worker's per-entry reviewer is injectable (the existing
CLI-free registry tests are the pattern). `smoke.ps1` may gain a real two-entry round trip;
because it bills tokens, that is opt-in and its cost is called out, per `AGENTS.md`.

## Open questions — resolved in round 1

All five were answered by the round-1 reviewer; recorded here with the resolution the plan
now reflects.

1. **In-band vs fail-fast for chain-semantic errors.** *Resolved: in-band.* Keep semantic
   chain errors as a per-request `INVALID_REVIEWER_CHAIN`, matching the issue wording and the
   project's runtime-setup-failure style — provided the invalid state is checked *before*
   reviewer preflight and stays non-agent-correctable. Both conditions are now in the plan.
2. **The invalidity rule set.** *Resolved: narrowed to a fully-identical spec.*
   `(reviewer, model)` was under-specified and alias-fragile; the rule now rejects only an
   entry identical in reviewer, model, effort, *and* bin, and the empty vector defensively.
   Same-family/different-model and same-model/different-bin stay valid; alias-vs-canonical
   duplicates are deliberately not detected (unverifiable mapping).
3. **Grammar.** *Resolved: repeated `--reviewer` is adequate*; an explicit `--fallback` opener
   adds little. Kept.
4. **Per-entry behaviour overrides.** *Resolved: global for now* — the family-scoped
   behaviour/security flags are reasonable as process-global, subject to the reviewer-dependent
   **capture** decision being fixed (it is, above). Per-entry sandbox/tool overrides remain a
   possible later addition, not part of this change.
5. **Fall-through scope.** *Resolved: rate-only.* `MODEL_UNAVAILABLE`, authentication and
   spawn failures can all represent configuration errors and must not be masked, so none join
   `RATE_LIMITED` in the fall-through set.

## Review history

- **Round 1 (Codex, gpt-5.6-luna, effort=max) — REQUEST CHANGES.** Seven findings, all
  accepted. Major: (1) cross-family `auto` capture would hand a shell-less fallback the wrong
  change → capture if *any* entry needs it ([Capture in a mixed-family chain]); (2) preflight
  ran before session selection → select the entry, then preflight only it ([Sessions and
  resume]); (3) the one-turn budget did not bound a walk and auth checks were uncancellable →
  shared chain deadline + cancellable preflight ([The fall-through, budget]); (4) the active
  fallback reviewer was absent from the result → carried through `Outcome → Review → Snapshot`
  ([Metrics, the result interface]). Minor: (5) `(reviewer, model)` was not a sound identity →
  full-spec duplicate rule ([Config validation]); (6) the `fresh: true` resume remediation was
  not wired → resume-aware `RATE_LIMITED` failure ([Sessions and resume]); (7) per-attempt
  billing was claimed as zero without evidence → record every attempt, claim nothing
  ([Metrics]). All five open questions resolved as above.

- **Round 2 (same session, turn 2) — REQUEST CHANGES.** Six findings, all accepted as
  second-order consequences of the round-1 fixes. (1) The single `Arc<dyn Reviewer>` and
  primary-`Config` reads were not threaded to the active entry → per-entry adapter selection
  and a full identity thread-through ([Per-entry adapter selection]). (2) The aggregate "all
  have a shell" flag cannot render a truthful preamble for a mixed chain, and `mcp.rs` was
  missing → split `chain_needs_capture()` from per-active-entry capability rendering, cover
  Perforce self-serve and `tools/list` ([Capture in a mixed-family chain]). (3) A rate limit
  is not guaranteed fast, so the deadline formula was not a worst-case bound → budget every
  attempt for the worst case and drive all displayed budgets from it ([The fall-through,
  budget]). (4) `SessionRecord` has no `bin`, so full-identity resume was unimplementable →
  persist bin, compare full identity, refuse legacy-ambiguous matches ([Sessions and resume]).
  (5) Active attribution was set only on success and could not update a running snapshot →
  publish the active entry before each attempt and carry it on every outcome ([Metrics, the
  result interface]). (6) A `Record` per attempt breaks the turn contract and failed-attempt
  usage is not retrievable → one logical-turn `Record` plus an `attempts` history with usage
  left unknown ([Metrics]).

- **Round 3 (same session, turn 3) — REQUEST CHANGES.** Four major + one minor, each a place a
  round-2 fix had not been carried all the way to the code. (1) The budget formula silently
  broke the single-entry invariant → re-expressed as *today's budget + fallback terms*, with an
  explicit lifecycle (selected entry preflighted before the wait; deadline spans the walk)
  ([The fall-through, budget]). (2) Per-entry capability rendering was infeasible because
  `CapturedChange` retains a render that bakes in `reviewer_has_shell` → make the capture render
  capability-neutral and move all reviewer-ability prose into per-active-entry
  `reviewer_capabilities`; `vcs/shared.rs`/`vcs/mod.rs`/Perforce render join the blast radius
  ([Capture in a mixed-family chain]). (3) Resolved-bin resume matching would preflight
  unselected entries and could not distinguish `bin=None` from an equal explicit path → persist
  and match the **raw** configured identity, resolving nothing ([Sessions and resume]).
  (4) Unknown attempt usage vanished from summary completeness → `Accumulator::push` marks
  totals partial when an attempt's usage is unknown ([Metrics]). Minor: (5) result/attempt
  identity omitted the bin → include the resolved bin. Plus a note adopted: `set_active` uses
  the existing `State` mutex ([Metrics, the result interface]).

- **Round 4 (same session, turn 4) — REQUEST CHANGES.** Four major + two minor, each a detail a
  round-3 fix had not carried fully to the code (the reviewer confirmed VCS rendering, registry
  locking, and per-entry adapter selection resolved, and found no missing adapter call site).
  (1) The pseudocode returned `REVIEWERS_EXHAUSTED` for a one-entry chain, contradicting the
  single-entry `RATE_LIMITED` rule → `N == 1` returns the plain failure with no history ([The
  fall-through, budget]). (2) The lifecycle and loop disagreed (double preflight) and
  `resolve_bin` is an uninterruptible PATH scan → shared `App` preflight cache makes entry 0 a
  cache hit, and the budget claim is narrowed to the cancellable auth invocation ([The
  fall-through, budget]). (3) A bare `Option<PathBuf>` conflated a new PATH entry with a legacy
  absent field → tagged `Option<RawBin>`, plus persist-and-verify the resolved path against PATH
  drift ([Sessions and resume]). (4) `attempts` at schema `v1` would be silently dropped by old
  readers → bump attempt-bearing records to `v2`, new reader accepts both ([Metrics]).
  (5) Attribution was unset during the `Capturing` phase → `set_active` before capture
  ([Metrics, the result interface]). (6) The attempt schema omitted bin and left prompt-byte
  accounting undefined → `Attempt` carries resolved bin and its own `prompt_bytes`; the
  top-level field stays the final attempt's ([Metrics]).

- **Round 5 (same session, turn 5) — REQUEST CHANGES.** Four major + two minor, all residuals;
  the reviewer confirmed round-4 #1/#4/#5/#6, the registry-lock note, the neutral-capture design
  and all five open-question rulings resolved. (1) The resume PATH-drift check could be bypassed
  by the shared preflight cache → force an uncached `resolve_bin` on resume, then refresh
  ([Sessions and resume]). (2) The cancellation bridge was under-defined against the API → one
  cancellation *probe* accepted by `auth_check` and `run`, backed by `RequestCancel` before
  registry attachment and the stop flag after ([The fall-through, budget]). (3) `chain_budget`
  was still called a hard ceiling though `resolve_bin` is an unbounded PATH scan → reframed as
  practical sizing with an explicit uninterruptible residual, matching
  `single-blocking-collect.md` ([The fall-through, budget]). (4) The new resolved-path field
  lacked a legacy serde contract → both new session fields are `#[serde(default)]`, verified
  against a real pre-change file ([Sessions and resume]). Minor: (5) single-entry records lacked
  a bin → optional top-level resolved-bin on every `Record` ([Metrics]); (6) `status` still named
  only the primary → all tool descriptions render the chain ([Capture in a mixed-family chain]).

[Capture in a mixed-family chain]: #capture-in-a-mixed-family-chain--the-change-must-reach-whoever-runs
[Sessions and resume]: #sessions-and-resume--the-one-correctness-trap
[The fall-through, budget]: #3-the-fall-through-reactive-and-rate-only
[Metrics, the result interface]: #metrics-the-result-interface-status-doctor
[Metrics]: #metrics-the-result-interface-status-doctor
[Config validation]: #2-config-validation-two-tiers-and-where-each-is-reported
[Per-entry adapter selection]: #per-entry-adapter-selection--every-identity-read-must-follow-the-active-entry
