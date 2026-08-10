# Reviewer fallback chain — design

Status: **proposed — revised after cross-review rounds 1 and 2.** This document is the plan.
Per this repository's own rule it must go through the `cross-review` gate (Codex,
gpt-5.6-luna, effort=max) and reach APPROVE before implementation begins. Rounds 1 and 2 each
returned REQUEST CHANGES — seven findings then six, all accepted; the sections below fold each
one in, and [Review history](#review-history) records where. It is the plan for [issue #48].

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
lives there, so one `review_id` spans the whole walk. The worker gains an *active entry
index* and this loop, on a **fresh** review (resume is different — see below):

```
deadline = now + chain_budget          # shared across the whole walk (see Budget below)
for (i, spec) in chain.iter().enumerate():
    registry.set_active(id, spec)       # publish BEFORE preflight, so a running poll and any
                                        # terminal failure name the entry actually being tried
    ready = ensure_ready(spec, cancel, deadline)   # resolve bin + auth for THIS entry, cancellable
    if ready is Err(f): return f.with_active(spec)  # preflight failure names this entry, not the primary
    outcome = run_turn(spec, ready.adapter, ready.bin, cancel, deadline)
    match outcome:
        Ok(review)                  -> return review.with_active(spec)
        Err(f) if f.code == RATE_LIMITED:
            note_attempt(spec, f)       # attempt history for the logical turn (see Metrics)
            if i is last            -> return reviewers_exhausted(attempts).with_active(spec)
            else                    -> continue            # advance to the next entry
        Err(f)                      -> return f.with_active(spec)   # anything else surfaces at once
```

The active entry is published to the registry **before** its preflight, and every terminal
result — success, `REVIEWERS_EXHAUSTED`, *and* a non-rate failure of a fallback entry —
carries the entry that was actually being tried. This is round-2 finding 5: assigning the
active entry only in the `Ok` branch would misreport a fallback that failed in preflight or
with `NOT_AUTHENTICATED`/`TIMEOUT` as the primary, and a running poll during the fallback
attempt would still show the primary.

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

The honest bound budgets **every attempted entry for the worst case**:

```
chain_budget = capture_budget
             + N × (preflight_cap + timeout + drain_grace)
             + finalization_grace
```

where `N` is the chain length, `preflight_cap` bounds `resolve_bin` + `auth_check` (the
auth-check timeout at [claude.rs:36]/[codex.rs:27], now cancellable), `timeout` is
`--timeout-seconds`, and `drain_grace` is the per-invocation output-drain grace that already
exists per turn ([reviewer/mod.rs:472]) — counted **per entry**, because each attempt is its
own process with its own drain. This is deliberately generous (a three-entry chain of
30-minute timeouts advertises a ~90-minute worst case), but it is the operator's own chain
length, and an honest large bound beats a wrong small one.

- `max_wait_secs`, and the budget shown in the start/running responses and progress text —
  which today all display `cfg.timeout` ([tools.rs:316], [tools.rs:466]) — are all recomputed
  from `chain_budget`, so what the caller is told to expect matches what the walk can take.
  For a single-entry chain, `chain_budget` reduces exactly to today's `max_wait_secs`.
- An **optional refinement**, not relied on: a separate short cap on an attempt that has
  already produced rate-limit evidence could shrink the practical walk time. It is noted as a
  future tightening; the worst-case bound above stands on its own without it.

[tools.rs:276]: ../src/tools.rs
[tools.rs:59]: ../src/tools.rs
[tools.rs:316]: ../src/tools.rs
[tools.rs:466]: ../src/tools.rs
[tools.rs:1253]: ../src/tools.rs
[config.rs:575]: ../src/config.rs
[reviewer/mod.rs:424]: ../src/reviewer/mod.rs
[reviewer/mod.rs:472]: ../src/reviewer/mod.rs
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
  asymmetry decides it. The capture *data* (the git/p4 output) is gathered **once** at
  start-of-review and retained.
- **The capability *rendering* is per active entry.** `reviewer_capabilities`
  ([config.rs:699]) and the VCS preamble are rendered from the **active** `ReviewerSpec` at
  turn time, from the retained capture data — so each attempt is told the truth about *its*
  shell and *its* self-serve ability. A Codex attempt is told it may fetch the diff; if the
  walk advances to Claude, Claude's attempt is rendered the retained diff with "you have no
  shell". `reviewer_has_shell` ([config.rs:583]) stays a per-reviewer predicate; what
  generalises to the chain is only `chain_needs_capture()`.
- **`mcp.rs` tool descriptions must not advertise the primary as if it were the whole
  chain.** `tools/list` today describes the single reviewer's shell/capture behaviour
  ([mcp.rs:636]); with a chain it must describe the chain honestly (the primary, plus that
  fallbacks exist and may differ) rather than imply one fixed posture. This file was missing
  from the round-1 blast radius and is added.
- Nothing about the classification-evidence or capture-labelling boundaries changes; the
  capture *mechanics* (commands, truncation, labelling) are untouched.

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
[mcp.rs:636]: ../src/mcp.rs

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
- **The session record must store enough to identify the entry unambiguously.** Round 2
  caught that `SessionRecord` holds `reviewer`/`model`/`effort` but **no `bin`**
  ([session.rs:41]), while this plan allows two entries that differ only by `bin` (different
  installation or account). Without persisting `bin`, a session created by the second such
  entry could match the first on resume and run the wrong executable. So `SessionRecord`
  gains the **resolved binary path**, added with `#[serde(default)]` exactly as
  `cumulative_usage` was ([session.rs:52]), and the resume match compares the **full
  operational identity — reviewer, model, effort, and resolved bin** (today's check at
  [tools.rs:1535] compares only reviewer and model, so effort is added too). A record that
  matches a chain entry on every field is resumed on that entry.
- **Legacy and ambiguous records are refused, not guessed.** A record written before the
  `bin` field existed has no bin to compare; it resumes only if **exactly one** chain entry
  matches on the fields it does carry. If it matches none, or more than one (e.g. two
  same-model/different-bin entries), resume is refused with `SESSION_NOT_RESUMABLE` and the
  caller starts fresh — never silently bound to a guessed executable.
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
  holds a **mutable active identity** that the walk updates via `set_active` before each
  attempt's preflight (see the pseudocode above), every `Outcome` carries the
  attempted-entry identity on success *and* failure, and both the running and completed
  responses render that identity instead of `describe_reviewer()`. A caller polling mid-walk
  sees the fallback; a fallback that fails in preflight is reported as the fallback. The
  earlier "registry untouched" claim is withdrawn.
- **One logical turn, with an attempt history — turn semantics preserved.** Round 1 wanted a
  rate-limited primary to stay visible; round 2 warned that appending a second `Record` for
  the same review would double the metrics' *turn* contract — two turns, two wall-times, two
  per-session turns ([metrics.rs:304], [metrics.rs:742]). Both are satisfied by keeping
  **exactly one `Record` per logical turn** (the entry that reviewed, or the terminal
  outcome), which drives turn count, wall time and token totals exactly as today, plus a new
  optional `attempts` field on `Record` listing each earlier fallback attempt — its
  reviewer/model/effort, its `failure_code`, and its attempt-local wall time. The
  rate-limited primary is therefore visible without inflating any turn statistic.
- **Failed-attempt token usage is not claimed.** The adapter API returns
  `Result<Parsed, Failure>` and exposes `Parsed.usage` only on a successful parse
  ([reviewer/mod.rs:118]); a rate-limit refusal yields a `Failure` with no usage, and the
  CLI's usage on a refusal is not something this tool has verified it can read. So the
  `attempts` entries record *that* the attempt happened and how it failed, with usage left
  unknown — not a fabricated zero, and not an unverified figure. This keeps round 1's "claim
  nothing about billing" while dropping round 2's double-counting. The adapter API is *not*
  widened to carry usage on failure; that would be surface for a number we cannot trust.
- **`--doctor` / `status`** enumerate every entry with its resolved bin and auth, so a human
  sees the whole chain and any per-entry setup problem in one read. A degraded
  (`INVALID_REVIEWER_CHAIN`) config reports the validation failure here too.

[metrics.rs]: ../src/metrics.rs
[metrics.rs:304]: ../src/metrics.rs
[metrics.rs:742]: ../src/metrics.rs
[reviewer/mod.rs:118]: ../src/reviewer/mod.rs
[tools.rs:462]: ../src/tools.rs
[tools.rs:474]: ../src/tools.rs
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

- **`config.rs`**: new `ReviewerSpec`; `Config.reviewers: Vec<ReviewerSpec>` replacing the
  four flat fields; per-entry parse loop and binding validation; `validate_chain`;
  `describe_reviewer` renders the chain.
- **`config.rs`**: `ReviewerSpec`; `Config.reviewers: Vec<ReviewerSpec>`; per-entry parse +
  binding validation; `validate_chain`; `chain_needs_capture()`; `describe_reviewer` renders
  the chain; `reviewer_capabilities` renders from an active `ReviewerSpec`; `max_wait_secs`
  computes the shared `chain_budget`.
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
- **`registry.rs`**: a **mutable active identity** on the running state (set per attempt), and
  `Outcome`/`Snapshot` carry the attempted reviewer/model/effort so both running and terminal
  results name who reviewed. Concurrency, leasing and cancellation otherwise unchanged.
- **`mcp.rs`**: `tools/list` descriptions ([mcp.rs:636]) describe the chain honestly (primary
  plus differing fallbacks) instead of advertising the primary's posture as the only one.
- **`vcs/` and `config.rs` capability logic**: the `auto` capture *decision* generalises to
  `chain_needs_capture()` (capture if *any* entry needs it); the capability *rendering* is
  per active entry (git shell text; Perforce self-serve, which needs Codex +
  `danger-full-access`). The capture *mechanics* (git/p4 commands, truncation, labelling) are
  untouched.
- **`errors.rs`**: two constructors — `invalid_reviewer_chain`, `reviewers_exhausted` — with
  codes, summaries, remediation, and `is_agent_correctable` returning false; a resume-aware
  `RATE_LIMITED` remediation path that does not disturb the shared `rate_limited` message; and
  a `.with_active(spec)` helper so a `Failure` can name the entry it came from.
- **`session.rs`**: `SessionRecord` gains the resolved `bin` (`#[serde(default)]`); resume
  match compares full identity (reviewer, model, effort, bin) against the chain, refusing
  legacy-ambiguous matches with `SESSION_NOT_RESUMABLE`.
- **`metrics.rs`**: `Record` gains an optional `attempts` list (schema addition, additive and
  `#[serde(default)]`); still exactly one `Record` per logical turn, so turn/wall/token
  accounting is unchanged.
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
  (because Claude needs it) even though Codex is primary; the Codex attempt's preamble says it
  may fetch the diff while the Claude attempt's preamble says it has none — both rendered from
  the retained capture data. A Perforce mixed chain asserts self-serve is rendered only for a
  Codex + `danger-full-access` active entry. All at rendering level, no live model.
- **Non-rate error does not fall through**: primary `NOT_AUTHENTICATED` (or `CLI_NOT_FOUND`)
  ⇒ surfaced immediately, chain not advanced.
- **Exhaustion**: every entry `RATE_LIMITED` ⇒ `REVIEWERS_EXHAUSTED` whose detail names each
  entry; a single-entry chain ⇒ plain `RATE_LIMITED`.
- **Budget**: a walk of several rate-limited entries, each running near its `--timeout-seconds`
  before being classified, stays within `chain_budget`; the displayed budget equals
  `chain_budget`, not `cfg.timeout`; cancellation during a fallback preflight/auth check stops
  the walk promptly (assert the cancel token reaches `auth_check`).
- **Resume**: a session created on entry *k* resumes on entry *k* only, does not restart the
  walk, preflights only entry *k* (a primary made unavailable *after* the session was created
  does not break the resume — the round-1 regression test), and — when *k* is rate-limited on
  resume — returns a `RATE_LIMITED` failure whose remediation names `fresh: true`; a record
  matching no configured entry ⇒ `SESSION_NOT_RESUMABLE`.
- **Resume identity**: two same-model/different-bin entries — a session created by the second
  resumes on the second (full-identity match), and a legacy record with no `bin` that matches
  both is refused with `SESSION_NOT_RESUMABLE` rather than guessing; a legacy record matching
  exactly one entry still resumes.

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

[Capture in a mixed-family chain]: #capture-in-a-mixed-family-chain--the-change-must-reach-whoever-runs
[Sessions and resume]: #sessions-and-resume--the-one-correctness-trap
[The fall-through, budget]: #3-the-fall-through-reactive-and-rate-only
[Metrics, the result interface]: #metrics-the-result-interface-status-doctor
[Metrics]: #metrics-the-result-interface-status-doctor
[Config validation]: #2-config-validation-two-tiers-and-where-each-is-reported
[Per-entry adapter selection]: #per-entry-adapter-selection--every-identity-read-must-follow-the-active-entry
