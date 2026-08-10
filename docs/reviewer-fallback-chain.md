# Reviewer fallback chain — design

Status: **proposed.** This document is the plan. Per this repository's own rule it must go
through the `cross-review` gate (Codex, gpt-5.6-luna, effort=max) and reach APPROVE before
implementation begins. It is the plan for [issue #48].

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
exactly what is wrong — which is this whole project's house style for failures. `--doctor`
and `cross_model_review_status` report the same problem for a human, before anything is
billed.

The chain-semantic rule set (the deliberately small, defensible core):

- **Exact-duplicate entries are invalid.** Two entries with the same `(reviewer, model)`
  cannot function as a fallback for each other: a rate limit is per account-and-model, so if
  the first is limited the identical second is too. This is the canonical "misordered chain"
  the issue points at, and it is caught here rather than wasting a spawn at runtime.
  (`effort` and `bin` do not distinguish them — same account, same model, same bucket.)
- The empty chain is impossible by construction: `--reviewer` remains required, so
  `reviewers` is always non-empty, and the existing "`--reviewer` is required" message
  ([config.rs:475]) still fires at startup as a syntax error.

[config.rs:475]: ../src/config.rs

Mechanically: `from_args` parses and syntax-validates into `Vec<ReviewerSpec>`. A new
`validate_chain(&[ReviewerSpec]) -> Result<(), Failure>` runs in `App::new`; on `Err`, `App`
holds the `Failure` and every `start_review` / `status` path returns it instead of touching
a reviewer. `main.rs` no longer needs to distinguish the case — the degraded `App` reports
itself. (Alternative — fail fast at startup for chain-semantic errors too — is called out in
[Open questions].)

### 3. The fall-through, reactive and rate-only

The turn already runs on a **background worker thread** ([tools.rs:276]); the fall-through
lives there, so one `review_id` spans the whole walk and the registry, cancellation and
progress reporting are untouched. The worker gains an *active entry index* and this loop, on
a **fresh** review (resume is different — see below):

```
for (i, spec) in chain.iter().enumerate():
    ensure_ready(spec)              # resolve bin + auth, lazily, for THIS entry
    outcome = run_turn(spec)        # spawn the child, drain, parse
    match outcome:
        Ok(review)                  -> record used = spec; return review
        Err(f) if f.code == RATE_LIMITED:
            if i is last            -> return REVIEWERS_EXHAUSTED (detail: every entry tried)
            else                    -> continue            # advance to the next entry
        Err(f)                      -> return f            # anything else surfaces at once
```

- **Only `RATE_LIMITED` advances the chain.** Per the maintainer's choice, setup and
  correctness failures — `NOT_AUTHENTICATED`, `AUTH_EXPIRED_MIDRUN`, `CLI_NOT_FOUND`,
  `MODEL_UNAVAILABLE`, `SPAWN_FAILED`, `BAD_REQUEST`, `TIMEOUT`, `CANCELLED`,
  `EMPTY_REVIEW` — surface immediately. Falling back on these would mask a real
  misconfiguration behind a working substitute, which is worse than a clear error.
- **Preflight is per entry and lazy.** `ensure_ready` becomes keyed by entry (a small
  per-entry cache in place of the single `Option<Preflight>` at [tools.rs:59]). A fallback
  entry's CLI is only resolved and auth-checked when the walk actually reaches it, so a
  fallback whose CLI is absent never troubles a healthy primary — and when it *is* reached,
  its `CLI_NOT_FOUND` surfaces (it is not `RATE_LIMITED`, so it stops the walk, correctly:
  the operator configured a fallback that does not exist).
- **Cost is bounded and honest.** Each exhausted entry costs one spawn that the CLI refuses
  with a rate limit; a refused turn bills nothing. A successful entry bills once, as today.

[tools.rs:276]: ../src/tools.rs
[tools.rs:59]: ../src/tools.rs

### 4. Same family: honoured, not enforced

The tool cannot know the *calling* model — direction is set by the human in `.mcp.json` /
`.codex/config.toml`, and no caller identity crosses the MCP boundary. So the server cannot
enforce "reviewer ≠ author"; the operator owns that when composing the chain, exactly as
they own picking the single reviewer today. Consequently:

- A same-family entry (`claude-opus-4-8` → `claude-sonnet-…`, or one Codex model → another)
  is accepted and used as written. No flag gates it.
- The only composition the tool refuses is the *exact-duplicate* entry above, because that
  one is not a fallback at all.

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

The trap is resume. A re-review resumes the reviewer's own conversation, whose memory lives
on **one specific** reviewer. Therefore:

- **Fallback selection happens only on a fresh review start, never on resume.** A resume
  binds to the entry that created the session — found by matching the record's
  `(reviewer, model)` against the configured chain — and runs *that* entry only. It does not
  restart the walk from the top; silently resuming a *different* reviewer would hand it a
  conversation it never had.
- If the resumed entry is itself rate-limited, the turn returns `RATE_LIMITED` (not
  `REVIEWERS_EXHAUSTED`, and no fall-through), with remediation pointing at `fresh: true` —
  which *does* restart chain selection, at the known cost of the prior reviewer's memory.
- If the record's `(reviewer, model)` no longer matches **any** configured entry (the chain
  was edited between runs), resume is refused with the existing `SESSION_NOT_RESUMABLE`,
  consistent with today's reviewer/model-mismatch check ([tools.rs:1535]). The caller
  retries with `fresh: true`.

[session.rs:42]: ../src/session.rs
[tools.rs:1535]: ../src/tools.rs

### Metrics, status, doctor

- **Metrics** need no schema change: `Record` already carries `reviewer`/`model`/`effort`
  ([metrics.rs]); a fallback turn records the active entry's identity, so `--usage` already
  attributes cost to the reviewer that actually ran.
- **`--doctor` / `status`** enumerate every entry with its resolved bin and auth, so a
  human sees the whole chain and any per-entry setup problem in one read. A degraded
  (`INVALID_REVIEWER_CHAIN`) config reports the validation failure here too.

[metrics.rs]: ../src/metrics.rs

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
  `RATE_LIMITED` surfacing, identical sessions. The chain is purely additive.
- **Must not fall back on anything but a rate/usage limit.** Auth, missing CLI, bad model,
  timeout, empty review, bad request all surface immediately and stop the walk.
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
  the `INVALID_REVIEWER_CHAIN` `Failure`); the worker gains an active-entry index and the
  fall-through loop; `ensure_ready` keyed per entry; `status` enumerates entries; resume
  matches the record against the chain.
- **`reviewer/mod.rs`, `reviewer/claude.rs`, `reviewer/codex.rs`**: `invocation` and
  `auth_check` take the active `ReviewerSpec` (or its `model`/`effort`) instead of reading
  `cfg.model`/`cfg.effort`.
- **`errors.rs`**: two constructors — `invalid_reviewer_chain`, `reviewers_exhausted` — with
  codes, summaries, remediation, and `is_agent_correctable` placement (both are
  stop-and-tell-user, like `RATE_LIMITED`).
- **`session.rs`**: resume match generalised from "equals the one reviewer" to "matches some
  entry in the chain"; the record reflects the active spec.
- **`main.rs`**: unchanged except that chain-semantic invalidity is now reported by the
  degraded `App`, not `exit(2)`.
- **Docs/config**: `README.md` (Configuration table + a fallback section), `AGENTS.md` if
  the gate workflow is affected (it is not, but the arg grammar is documented), the
  `examples/` configs, and `smoke.ps1` if a chain path is worth an end-to-end check.

Not touched: the capture pipeline (`vcs/`), the registry's concurrency and leasing, the
cancellation paths, the progress protocol, the metrics schema.

## Testing

Unit tests (no network, no model call), extending the existing fakes:

- **Parsing**: single `--reviewer` ⇒ one-entry chain (regression guard on today's
  behaviour); a two-entry chain preserves order; an identity flag before any `--reviewer`
  errors; a doubled `--model` within one entry errors; per-entry defaults fill in per entry.
- **Chain validation**: exact-duplicate `(reviewer, model)` ⇒ `INVALID_REVIEWER_CHAIN`, and
  a degraded `App` returns it from `start_review` and `status` without touching a reviewer;
  a same-family, different-model chain is accepted.
- **Fall-through** (fake reviewer scripted to return `RATE_LIMITED` then `Ok`): primary
  limited ⇒ second entry runs and its review is returned; the metrics record names the
  second entry.
- **Non-rate error does not fall through**: primary `NOT_AUTHENTICATED` (or `CLI_NOT_FOUND`)
  ⇒ surfaced immediately, chain not advanced.
- **Exhaustion**: every entry `RATE_LIMITED` ⇒ `REVIEWERS_EXHAUSTED` whose detail names each
  entry; a single-entry chain ⇒ plain `RATE_LIMITED`.
- **Resume**: a session created on entry *k* resumes on entry *k* only, does not restart the
  walk, and — when *k* is rate-limited on resume — returns `RATE_LIMITED` with a
  `fresh: true` hint; a record matching no configured entry ⇒ `SESSION_NOT_RESUMABLE`.

A test seam is required so the worker's per-entry reviewer is injectable (the existing
CLI-free registry tests are the pattern). `smoke.ps1` may gain a real two-entry round trip;
because it bills tokens, that is opt-in and its cost is called out, per `AGENTS.md`.

## Open questions for the reviewer

1. **In-band vs fail-fast for chain-semantic errors.** The plan starts the server and
   returns `INVALID_REVIEWER_CHAIN` per request, on the argument that an agent-legible
   in-band failure beats a silent `exit(2)`. The alternative — treat a bad chain as a syntax
   error and `exit(2)` at startup like every other config fault — is more *consistent* with
   today. Which does the reviewer prefer?
2. **The invalidity rule set.** Is exact-duplicate `(reviewer, model)` the right — and
   sufficient — definition of "misordered"? Candidates deliberately left *valid*:
   same-family different-model (the issue says honour it), and an unreachable entry after a
   never-limited one (the tool cannot know an account is unlimited). Anything missing, or
   anything over-reached?
3. **Grammar.** Repeated `--reviewer` as the entry delimiter with positional identity-flag
   binding — acceptable, or is an explicit `--fallback` opener (e.g. a second flag name that
   starts each additional entry) clearer and less error-prone, despite adding a flag?
4. **Per-entry behaviour overrides.** The plan keeps `--sandbox` / `--tools` /
   `--allow-reviewer-config` global, arguing they are already family-scoped in effect. Is
   there a real chain where an entry needs its *own* sandbox or tool policy — enough to
   justify per-entry overrides now rather than as a later addition?
5. **Fall-through scope.** Rate-only is the maintainer's choice. Does the reviewer see a
   failure class that is *clearly* "this reviewer can't, try the next" and *clearly* not a
   maskable misconfiguration — such that it belongs alongside `RATE_LIMITED` in the
   fall-through set?
