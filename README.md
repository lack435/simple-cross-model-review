# cross-review

An MCP server that hands your work to a **different model** for review, and gives you back
what it said.

Claude Code asks Codex. Codex asks Claude. One request, one response — the calling agent
decides what to do with the feedback. No orchestration, no multi-agent choreography.

Windows only. A single self-contained executable — no Node, no Python, no DLLs. Vendor the
`.exe` into a repository and a fresh clone works.

## Why

An agent reviewing its own work shares its own blind spots. A different model, with a
different training run and different failure modes, finds things the author could not. This
makes that a single tool call.

## Requirements

On the machine running the calling agent:

- The **reviewer's** CLI, installed and signed in — Codex (`codex login`) if Claude Code is
  the caller, or Claude Code (`claude auth login`) if Codex is the caller.

If the reviewer CLI is missing, signed out, rate-limited, or rejects the pinned model, the
tool call fails loudly with an explicit message telling the agent to stop and telling you how
to fix it. It never silently falls back to a same-model review.

## Setup

Pick your direction and follow its guide — each is a two-step setup (drop the `.exe` in
`tools\`, add one entry to a project config file), so a fresh clone of *your* project needs no
setup at all:

- **[Claude Code reviewed by Codex](examples/claude-code-reviewed-by-codex/)** — project
  `.mcp.json`.
- **[Codex reviewed by Claude](examples/codex-reviewed-by-claude/)** — project
  `.codex\config.toml`.

This repository has both wired up for itself, so it can be dogfooded either way:
[`.mcp.json`](.mcp.json) (Claude asks Codex) and [`.codex/config.toml`](.codex/config.toml)
(Codex asks Claude).

Get the executable from the [Releases page](../../releases) — download `cross-review.exe`
and check it against `SHA256SUMS.txt`:

```bash
sha256sum -c SHA256SUMS.txt
```

`cross-review.exe --version` reports the commit it was built from. It is not committed to
this repository; releases are built and published by CI.

> Codex only loads project-level config for **trusted** folders — the
> `[projects.'c:\path'] trust_level = "trusted"` entry it writes when you first approve the
> folder. And `codex mcp list` / `codex doctor` report only global config, so they never show
> a project-level server; use `cross_model_review_status` from inside a session to verify.

## The tools

| Tool | Purpose |
| --- | --- |
| `cross_model_review` | Start a review of a change. Returns a `review_id` immediately. |
| `cross_model_review_result` | Wait for and return the review — one blocking call, with live progress when the client supports it. |
| `cross_model_review_cancel` | Stop a review that is still running. This is what frees the reviewer; abandoning a `cross_model_review_result` wait does not. |
| `cross_model_review_status` | Is the reviewer CLI installed and signed in? Costs nothing, calls no model. |
| `cross_model_consult` | Ask the reviewer an informal question — a lightweight second opinion, no findings ledger and nothing to converge on. Returns a `review_id`. |
| `cross_model_consult_result` | Wait for and return a consult's prose answer. |
| `cross_model_setup_profile` | One-time, human-approved step to authorize this repository to review under a dedicated reviewer account (see [account profiles](docs/reviewer-account-profiles.md)). |

### Reviews are asynchronous

A serious review of real work takes time — in this project's usage, commonly at least five
minutes, and complex changes 20 minutes or longer. A running review in that window is normal.
Starting and collecting are separate calls, so the harness is never blocked before it chooses
to wait.

**A single `cross_model_review_result` collects a whole review in one blocking call** — omit
`wait_seconds` to block to completion. If the wait budget elapses server-side the call returns
`status=running`; if the client's own tool timeout fires first the response is suppressed and
the caller sees a client-side timeout. Either way, just call again with the same `review_id` —
abandoning a collect leaves the reviewer running and the result collectible (only
`cross_model_review_cancel` stops it). Set your client's per-server `timeout` above the collect
cap (~1890s at the defaults) so one blocking call completes; below it you poll, you do not lose
work. Design notes: [`docs/single-blocking-collect.md`](docs/single-blocking-collect.md).

While a collect is open the server emits MCP `notifications/progress` every 30 seconds when the
client supplied a progress token — the current phase, elapsed time, how recently the reviewer
was confirmed alive, and how much output has arrived.

### Consults

`cross_model_consult` is a lighter path for "does this direction look right?", "where is X
handled?", or "am I missing a simpler approach?". The reviewer reads the repository through the
same read-only evidence service and answers in prose — no findings, no verdict, nothing to act
on mechanically. By default it reads the tree and captures no diff; pass `include_change: true`
to also show it the configured change. It requires the evidence service, so it runs only on a
reviewer that provides one (Codex, or a profile-pinned shell-less Claude).

## Configuration

Everything is a CLI argument on the MCP server entry, so the project's config file is the
single source of truth — there is no config file of our own to drift out of sync.

The essentials:

```
--reviewer <claude|codex>   Which CLI reviews. Required. Pick the model that is NOT the
                            calling agent. Repeat it to configure a fallback chain.
--level NAME:MODEL:EFFORT   A caller-selectable (model, effort) preset. Required — at least one
                            per --reviewer. Repeatable; the caller picks one per review via the
                            `level` argument. effort: claude low|medium|high|xhigh|max
                                                       codex  low|medium|high|xhigh|max|ultra
--default-level NAME        Which --level an omitted `level` uses. Required with 2+ levels;
                            defaults to the sole level otherwise.
--bin <path>                Reviewer CLI path, if not on PATH.
```

> Pin models by full id, never an alias. `opus` resolves to whatever the provider maps that
> alias to and can move as releases ship. Pinning the exact id keeps the reviewer fixed.

<details>
<summary>All options</summary>

```
--claude-profile <name>     Pin the reviewer to a named account/config home rather than the
--codex-profile <name>      ambient login, so the review bills that account and loads none of
                            your config. Provisioned once with cross_model_setup_profile.
--claude-config-dir <path>  Explicit config-home path instead of a managed profile label.
--codex-home <path>
--min-usage-remaining <n>    Proactive gate (codex): skip this entry when last-observed usage
                            remaining is below n% (1..=100).
--min-usage-status <lvl>     Proactive gate (claude): 'ample' skips on warning-or-worse,
                            'warning' skips only when rejected.
--timeout-seconds <n>        Per-turn budget. Default 1800. Max 86400 (24h).
--max-concurrent-reviews <n> Per-process cap on reviews running at once. Default 8. 0 disables.
--session-max-turns <n>      Refuse to resume a session past this many turns. Default 10.
--session-max-idle-seconds <n>
                            Refuse to resume a session idle longer than this. Default 3300.
--stagnant-session-turns <n> End a session going this many turns without raising or resolving
                            a finding while findings are open. Default 3. 0 disables.
--block-repair-attempts <n>  When a reviewer omits its machine-readable findings block, ask
                            once more for the block alone. Default 1. Max 3. 0 disables.
--block-repair-timeout-seconds <n>   Per-attempt timeout for the above. Default 180.
--max-policy-denials <n>     Codex: after this many command-policy refusals plus an idle stdout
                            window, end the turn early as POLICY_BLOCKED. Default 4. 0 disables.
--max-policy-idle-seconds <n>   The idle window for the above. Default 300.
--no-incremental-resume      Disable the incremental-diff optimization on resumed turns.
--cwd <path>                 Review root. Defaults to the server's working directory.
--state-dir <path>           Where named sessions live.
--sandbox <mode>             Codex sandbox policy. Default read-only.
--vcs <auto|git|perforce>    Which version control the change backend uses. Default auto.
--tools / --allow-tools      Override the Claude reviewer's read-only tool policy.
--review-preamble-file <path>  Replace the built-in review preamble (cross_model_review).
--consult-preamble-file <path> Replace the built-in consult preamble (cross_model_consult).
--no-preamble                Send the caller's instructions with nothing added (both paths).
--allow-reviewer-config      Let the reviewer load project and user configuration.
--no-metrics                 Stop recording per-turn token usage. On by default.
--doctor                     Check CLI and auth from a terminal, then exit.
--usage                      Print the recorded usage summary, then exit.
```

Deeper behaviors have their own design docs: the
[reviewer fallback chain](docs/reviewer-fallback-chain.md),
[proactive usage-remaining gate](docs/usage-remaining-gate.md),
[review levels](docs/review-levels-plan.md), and
[reviewer account profiles](docs/reviewer-account-profiles.md).

</details>

## The change under review is derived live

Most reviews are reviews *of a change*, not of a tree. Rather than making the caller paste a
diff into `instructions` — spending the caller's context, missing untracked files, and, when
forgotten, getting back a review of the current tree instead of the change — the reviewer derives
the change itself, live, through the read-only evidence service.

For **git**, there are no capture modes: the reviewer diffs the live working tree on demand through
the `repository_diff` evidence tool. Its default scope — `base: "branch-base"`, `head: "worktree"` —
is the whole change: the working tree against the branch's fork point
(`merge-base(HEAD, refs/remotes/origin/{HEAD,main,master})`), including committed work, uncommitted
edits, and untracked files. There is nothing to commit first and nothing to paste. A fail-closed
gate ensures a formal *approve* was actually served that complete diff, end to end, so an approval
cannot rest on less than the whole change. Git reviews require the evidence path — the Codex reviewer
always has it; the Claude reviewer needs a pinned `--claude-profile` — and a git review without it is
refused before it runs. `git` is resolved from PATH and run with a hostile repository's config
disabled (`diff.external`, textconv, `core.fsmonitor`); `repository_diff` accepts only full object
ids or a closed sentinel set, never a raw ref or option. Full behavior:
[`docs/retire-capture-modes.md`](docs/retire-capture-modes.md).

For **Perforce** (`--vcs perforce`, or `auto` in a workspace with no `.git`), the change is an
explicit list of changelists named per call in the `change` argument (`"43650"`,
`"43650,43651"`, or `["43650","43651"]`); it is required, with no default. Details, including
the client-derivation rules and read-confinement by basis, are in
[`docs/perforce-resume-delta.md`](docs/perforce-resume-delta.md).

## Re-reviewing after you act on feedback

Sessions are named, and you choose the names. Calling `cross_model_review` again with the same
`session` resumes that reviewer's conversation, so it still remembers what it told you and
reports what is now resolved and what is still open.

```
cross_model_review(session: "auth-refactor", instructions: "First pass at the token refresh
                   path. Focus on the race between refresh and revoke.")
... act on the findings ...
cross_model_review(session: "auth-refactor", instructions: "Addressed findings 1 and 3.
                   I disagree with 2 because the lock is already held; check that.")
```

The name-to-session mapping is stored on disk, so a review survives a server restart. Pass
`fresh: true` when earlier findings would only mislead.

Resuming is not free: a resumed turn re-sends the whole conversation, and the reviewer runs an
agentic loop that re-reads everything accumulated so far, so later turns cost more than the
first. The server records per-turn token usage (`usage-<machine>.jsonl` in the state directory),
summarised on every completed review, in `cross_model_review_status`, and by `--usage`.
`--no-metrics` turns recording off. How the two CLIs' token counts are normalised is documented
in [`docs/structured-findings-envelope.md`](docs/structured-findings-envelope.md).

## Reading a completed result

Switch on `outcome`:

| `outcome` | What it means | What to do |
| --- | --- | --- |
| `converged` | The machine contract passed. | Stop. (It certifies the structured contract, not that a human read the prose.) |
| `changes_requested` | Findings are open, the verdict and count disagree, or the reviewer was not shown the whole current change (`evidence_incomplete`). | Act on `findings`, re-review the same session. |
| `escalate` | The reviewer blocked. | A person decides; re-reviewing keeps producing this. |
| `rebaseline` | This session cannot continue — coverage broke, the turn was not durable, the ledger is over budget, or the session stalled. | A person decides, then starts a fresh review carrying the still-open findings. |

Every review carries a machine-readable envelope (verdict, findings with stable ids, a
`converged` signal) alongside prose. Read `review_prose`, not only `findings` — it is where the
reviewer explains why a finding it is holding open is still open, and it is present on every turn
that ran. A client reading only `structuredContent` still gets everything that bears on how much
weight the review deserves: `review_prose`, `captured`, `denial_count`, `warnings`, `resumable`,
`reviewer`, `usage`. The full contract is in
[`docs/structured-channel-parity.md`](docs/structured-channel-parity.md); the findings model and
how carried-vs-re-examined findings work are in
[`docs/finding-liveness.md`](docs/finding-liveness.md).

If a reviewer writes a good review but omits its machine block, the server asks once more for the
block alone rather than discarding the turn (opt out with `--block-repair-attempts 0`). See
[`docs/unstructured-turn-recovery.md`](docs/unstructured-turn-recovery.md).

## When the reviewer is unavailable

The tool call fails loudly with a machine-readable code, an explanation, and the exact
remediation to hand the user:

```
CROSS-MODEL REVIEW FAILED
code: NOT_AUTHENTICATED

The 'codex' CLI is installed but not signed in, so it cannot review.

=== ACTION REQUIRED ===
The external review did not run, so there is no review feedback. Stop the current task now.
Do not review the work yourself in place of the external reviewer.

Report this to the user:
  To fix it, run this in a terminal:
    codex login
```

Codes: `CLI_NOT_FOUND`, `NOT_AUTHENTICATED`, `AUTH_EXPIRED_MIDRUN`, `MODEL_UNAVAILABLE`,
`RATE_LIMITED`, `REVIEWERS_EXHAUSTED`, `REVIEWER_ACCOUNT_CHANGED`, `INVALID_REVIEWER_CHAIN`,
`INVALID_LEVEL`, `INVALID_LEVEL_ON_RESUME`, `PROFILE_IDENTITY_MISMATCH`, `PROFILE_NOT_AUTHORIZED`,
`TIMEOUT`, `POLICY_BLOCKED`, `SPAWN_FAILED`, `REVIEWER_FAILED`, `EMPTY_REVIEW`, `OUTPUT_TRUNCATED`,
`OUTPUT_INCOMPLETE`, `EVIDENCE_UNAVAILABLE`, `EVIDENCE_CALL_ABANDONED`, `SESSION_NOT_FOUND`,
`SESSION_NOT_RESUMABLE`, `CANCELLED`, `SERVER_SHUTTING_DOWN`, `INTERNAL_ERROR`. Bad arguments, a
busy session, a non-resumable session, and too many reviews already running (`TOO_MANY_RUNNING`)
get a plain correction rather than an escalation, since each is the agent's own call to make.

`cross_model_review_status` and `--doctor` check the reviewer CLI and auth before anything is
billed.

## Security posture

By default the reviewer cannot modify anything, and the table below is that **default posture** —
several supported overrides weaken it, called out beneath the table. Whether the reviewer can
*read* outside the project differs by direction — worth knowing before you point either one at
code you do not trust:

| | writes | reads outside the project | shell |
| --- | --- | --- | --- |
| **Claude reviewer** | denied (no such tool) | **denied** | none |
| **Codex reviewer** | denied (OS sandbox) | **not confined** | yes |

- The **Claude reviewer** runs with `--tools Read,Grep,Glob`, each path-scoped to the project
  (`Read(./**)`). No write tools and no shell are in the session at all. Point this direction at
  a repository you do not trust.
- The **Codex reviewer** runs `--sandbox read-only` — writes are refused by the OS (a Windows
  restricted-token sandbox, verified with no model in the loop). It keeps a shell, so it can read
  anything your account can and quote it into the review, which is returned to the caller
  verbatim. Its reads are **not** confined; that was investigated and there is no CLI surface that
  confines them.

Both reviewers run configuration-isolated, so a committed hook, plugin, or MCP server in the
reviewed repository cannot execute during a review. The Codex reviewer — and a profile-pinned,
shell-less Claude — read the repository through a read-only, path-scoped evidence service
(`repository_scope`/`list`/`search`/`read`/`change`/`diff`, plus Git `history`/`revision`) rather than by
loading project config; an ambient Claude reviewer instead uses its scoped native Read/Grep/Glob
tools.

**Overrides that weaken this default**, for repositories you already trust: `--sandbox` set to a
Codex write mode lets the reviewer write; adding `Bash` to the Claude reviewer's `--tools` and
`--allow-tools` gives it a shell, a soft boundary that read-oriented commands can escape to write;
and `--allow-reviewer-config` turns configuration isolation off, so a committed project hook *can*
then execute. Change these only for code you trust.

Everything above is verified behavior against specific CLI versions on Windows; the full evidence,
the isolation design, and what was ruled out are in
[`docs/claude-reviewer-evidence-service.md`](docs/claude-reviewer-evidence-service.md) and
[`docs/codex-reviewer-evidence-service.md`](docs/codex-reviewer-evidence-service.md). Re-check it
against your own CLI versions rather than inheriting it.

## Contributing

Design decisions and their verified evidence live in [`docs/`](docs/). Build and test
instructions for working on cross-review itself are in [`AGENTS.md`](AGENTS.md).
