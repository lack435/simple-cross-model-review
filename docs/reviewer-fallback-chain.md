# Reviewer fallback chain — design

Status: **proposed — revised after cross-review round 1.** This document is the plan. Per
this repository's own rule it must go through the `cross-review` gate (Codex, gpt-5.6-luna,
effort=max) and reach APPROVE before implementation begins. Round 1 returned REQUEST CHANGES
with seven findings, all accepted; the sections below fold each one in, and
[Review history](#review-history) records where. It is the plan for [issue #48].

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
overrides to be correct. (Whether per-entry overrides are ever *wanted* is left open for the
reviewer, [Open questions].)

[Open questions]: #open-questions-for-the-reviewer

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
lives there, so one `review_id` spans the whole walk. The worker gains an *active entry
index* and this loop, on a **fresh** review (resume is different — see below):

```
deadline = now + chain_budget          # shared across the whole walk (see Budget below)
for (i, spec) in chain.iter().enumerate():
    ensure_ready(spec, cancel, deadline)   # resolve bin + auth for THIS entry, cancellable
    outcome = run_turn(spec, cancel, deadline)   # spawn child, drain, parse
    match outcome:
        Ok(review)                  -> active = spec; return review
        Err(f) if f.code == RATE_LIMITED:
            record_attempt(spec, f)     # each attempt is accounted for (see Metrics)
            if i is last            -> return REVIEWERS_EXHAUSTED (detail: every entry tried)
            else                    -> continue            # advance to the next entry
        Err(f)                      -> return f            # anything else surfaces at once
```

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

**Budget across the walk.** Round 1 correctly noted that the advertised collect budget was
sized for **one** attempt: `Config::max_wait_secs` ([config.rs:575]) is the capture budget
plus a single `--timeout-seconds` plus grace, while each attempt gets its *own* deadline
([reviewer/mod.rs:424], invoked at [tools.rs:1253]) — so several rate-limited entries could
outlive what a `cross_model_review_result` caller was told to expect. The fix is an explicit
**shared chain deadline**, computed once when the walk starts and passed to every preflight
and turn, and the collect cap widened to match it. Its size:

- A rate/usage refusal returns **fast** — the CLI rejects before doing real work — so an
  exhausted entry costs roughly a spawn plus its preflight, not a full `--timeout-seconds`.
  The plan does not *rely* on that for correctness, but it is why the walk is not `N ×
  timeout` in practice.
- The bound that matters is the worst case, so the shared deadline is
  `capture + (N × preflight_cap) + timeout + grace`, where `N` is the chain length and only
  the one entry that actually reviews consumes the full `--timeout-seconds` (a rate-limited
  entry cannot, by definition, run to timeout and still be classified `RATE_LIMITED`). The
  collect cap (`max_wait_secs`) is recomputed from the same formula so the server-side wait
  and the walk agree. Progress notifications already report elapsed time and liveness across
  the wait; they simply span the attempts.

[tools.rs:276]: ../src/tools.rs
[tools.rs:59]: ../src/tools.rs
[tools.rs:1253]: ../src/tools.rs
[config.rs:575]: ../src/config.rs
[reviewer/mod.rs:424]: ../src/reviewer/mod.rs
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

The fix, and the reason it is conservative rather than clever:

- **`auto` captures whenever *any* entry in the chain would need the change** — i.e. when any
  entry lacks a usable shell — not merely when the primary does. Handing a diff to a
  shell-capable reviewer is already a supported, harmless mode (it is exactly what `--diff
  HEAD` does), so the cost of over-capturing for a `Codex → Claude` chain is a little
  redundant work and some extra prompt text for Codex, never a wrong review. Under-capturing,
  by contrast, produces a confident review of the wrong thing. The asymmetry decides it.
- This keeps capture a **single start-of-review decision** feeding the prompt, rather than
  re-capturing mid-walk per active entry — simpler, and it matches the current architecture
  where the capture is built once into the turn. The `reviewer_capabilities` /
  `reviewer_has_shell` computation ([config.rs:583]) is generalised from "the one reviewer"
  to "the chain": has-a-shell becomes *all* entries have a shell; needs-the-diff becomes
  *any* entry needs it.
- The reviewer's prompt still describes the capture honestly. A shell-capable entry that
  receives a diff is told what it was shown, exactly as `--diff HEAD` does today; nothing
  about the classification-evidence or capture-labelling boundaries changes.

The other `--diff` modes are unaffected in spirit: `none` still supplies nothing, an explicit
range or `HEAD` still captures regardless of shell. Only `auto`'s "does the reviewer need
it?" test widens from the primary to the whole chain.

[config.rs:623]: ../src/config.rs
[config.rs:583]: ../src/config.rs
[vcs/mod.rs:89]: ../src/vcs/mod.rs
[vcs/git.rs:445]: ../src/vcs/git.rs

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
  the record and choose the entry: for a resume, the entry whose full identity matches the
  record; for a fresh start, entry 0 (then the walk). Preflight is run against the *selected*
  entry, never unconditionally against the primary. The `INVALID_REVIEWER_CHAIN` guard runs
  ahead of even this, so an invalid chain is reported before any record is read.
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

[session.rs:42]: ../src/session.rs
[tools.rs:175]: ../src/tools.rs
[tools.rs:189]: ../src/tools.rs
[tools.rs:1535]: ../src/tools.rs
[errors.rs:180]: ../src/errors.rs

### Metrics, the result interface, status, doctor

- **The result must name the reviewer that actually ran.** Round 1 found this missing: the
  registry's `Outcome` and `Snapshot` carry no reviewer identity ([registry.rs:150],
  [registry.rs:637]), and the completed/running responses render `self.cfg.describe_reviewer()`
  ([tools.rs:474], [tools.rs:511]) — the *chain/primary*, not the entry that reviewed. A
  successful fallback would be misattributed. The fix threads the active entry's
  `reviewer`/`model`/`effort` through `Outcome → Review → Snapshot` and renders *that* in the
  result, so a caller always sees who did the review. This is the registry change the round-1
  finding requires; the earlier "registry untouched" claim is withdrawn.
- **Metrics account for every attempt, and claim nothing about billing.** `Record` already
  carries `reviewer`/`model`/`effort` ([metrics.rs]), so a successful turn attributes cost to
  the entry that ran with no schema change. Round 1 was right that the earlier "a refused turn
  bills nothing" was unverified and that a rate-limited primary would otherwise vanish from
  the log: the worker records usage once after the final outcome ([tools.rs:1038],
  [tools.rs:1092]). So each **attempt** — including a rate-limited one that the walk handled
  internally — appends its own `Record` with `status`/`failure_code` set and whatever usage
  the CLI reported (left as "not reported" when the CLI reported none, which the schema
  already distinguishes from zero). No claim is made that a refusal is free; the log shows
  what the CLI actually reported.
- **`--doctor` / `status`** enumerate every entry with its resolved bin and auth, so a human
  sees the whole chain and any per-entry setup problem in one read. A degraded
  (`INVALID_REVIEWER_CHAIN`) config reports the validation failure here too.

[metrics.rs]: ../src/metrics.rs
[tools.rs:474]: ../src/tools.rs
[tools.rs:511]: ../src/tools.rs
[tools.rs:1038]: ../src/tools.rs
[tools.rs:1092]: ../src/tools.rs
[registry.rs:150]: ../src/registry.rs
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
- **Must not outrun its advertised budget.** The walk runs under one shared deadline that the
  collect cap is sized to match; preflight and auth checks are cancellable.
- **Must not misattribute the review.** The result names the entry that actually ran, and
  metrics record each attempt.
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

- **`config.rs`**: new `ReviewerSpec`; `Config.reviewers: Vec<ReviewerSpec>` replacing the
  four flat fields; per-entry parse loop and binding validation; `validate_chain`;
  `describe_reviewer` renders the chain.
- **`tools.rs`**: `App` holds the chain and a per-entry preflight cache (and, when invalid,
  the `INVALID_REVIEWER_CHAIN` `Failure`); the worker gains an active-entry index, the
  fall-through loop, and the shared chain deadline; the `INVALID_REVIEWER_CHAIN` guard and
  entry selection move ahead of preflight (fixing the [tools.rs:175] ordering); `ensure_ready`
  is keyed per entry and cancellable; `status` enumerates entries; resume matches the record
  against the chain; the completed/running responses render the active entry rather than
  `describe_reviewer()`.
- **`reviewer/mod.rs`, `reviewer/claude.rs`, `reviewer/codex.rs`**: `invocation` and
  `auth_check` take the active `ReviewerSpec` (or its `model`/`effort`) instead of reading
  `cfg.model`/`cfg.effort`, and `auth_check` accepts the cancellation token + deadline so a
  fallback preflight is interruptible.
- **`registry.rs`**: `Outcome` and `Snapshot` gain the active reviewer/model/effort so the
  result can name who reviewed. Concurrency, leasing and cancellation are otherwise
  unchanged.
- **`vcs/` and `config.rs` capability logic**: the `auto` capture decision and
  `reviewer_capabilities` / `reviewer_has_shell` generalise from the single reviewer to the
  chain (capture if *any* entry needs it; "has a shell" only if *all* do). The capture
  *mechanics* (git/p4 commands, truncation, labelling) are untouched.
- **`errors.rs`**: two constructors — `invalid_reviewer_chain`, `reviewers_exhausted` — with
  codes, summaries, remediation, and `is_agent_correctable` returning false (both are
  stop-and-tell-user, like `RATE_LIMITED`); plus a resume-aware `RATE_LIMITED` remediation
  path that does not disturb the shared `rate_limited` message.
- **`session.rs`**: resume match generalised from "equals the one reviewer" to "matches some
  entry in the chain"; the record reflects the active spec.
- **`metrics.rs`**: no schema change, but the worker now appends a `Record` per attempt, not
  only for the final outcome.
- **`main.rs`**: unchanged except that chain-semantic invalidity is now reported by the
  degraded `App`, not `exit(2)`.
- **Docs/config**: `README.md` (Configuration table + a fallback section), `AGENTS.md` if
  the gate workflow is affected (it is not, but the arg grammar is documented), the
  `examples/` configs, and `smoke.ps1` if a chain path is worth an end-to-end check.

Not touched: the capture *mechanics* in `vcs/` (commands, truncation, labelling), the
registry's concurrency and leasing, the cancellation *protocol*, the progress protocol, the
metrics schema.

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
  entry** (registry `Outcome`/`Snapshot` carry it); a `Record` is appended for the
  rate-limited primary *and* the successful fallback.
- **Cross-family capture**: a `Codex → Claude` chain under `--diff auto` captures the diff
  (because Claude needs it) even though Codex is primary; asserted at capture-decision level
  so it does not require a live model.
- **Non-rate error does not fall through**: primary `NOT_AUTHENTICATED` (or `CLI_NOT_FOUND`)
  ⇒ surfaced immediately, chain not advanced.
- **Exhaustion**: every entry `RATE_LIMITED` ⇒ `REVIEWERS_EXHAUSTED` whose detail names each
  entry; a single-entry chain ⇒ plain `RATE_LIMITED`.
- **Budget**: a walk of several rate-limited entries stays within the shared chain deadline,
  and cancellation during a fallback preflight/auth check stops the walk promptly (assert the
  cancel token reaches `auth_check`).
- **Resume**: a session created on entry *k* resumes on entry *k* only, does not restart the
  walk, preflights only entry *k* (a primary made unavailable *after* the session was created
  does not break the resume — the round-1 regression test), and — when *k* is rate-limited on
  resume — returns a `RATE_LIMITED` failure whose remediation names `fresh: true`; a record
  matching no configured entry ⇒ `SESSION_NOT_RESUMABLE`.

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

[Capture in a mixed-family chain]: #capture-in-a-mixed-family-chain--the-change-must-reach-whoever-runs
[Sessions and resume]: #sessions-and-resume--the-one-correctness-trap
[The fall-through, budget]: #3-the-fall-through-reactive-and-rate-only
[Metrics, the result interface]: #metrics-the-result-interface-status-doctor
[Metrics]: #metrics-the-result-interface-status-doctor
[Config validation]: #2-config-validation-two-tiers-and-where-each-is-reported
