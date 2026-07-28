# cross-review

An MCP server that hands your work to a **different model** for review, and gives you
back what it said.

Claude Code asks Codex. Codex asks Claude. One request, one response — the calling agent
decides what to do with the feedback. There is no orchestration, no multi-agent
choreography, and no attempt to be clever about it.

Windows only. A single 520 KB executable with no runtime dependencies: no Node, no
Python, no DLLs. Vendor the `.exe` into a repository and a fresh clone works.

## Why

An agent reviewing its own work shares its own blind spots. A different model, with a
different training run and different failure modes, finds things the author could not.
This makes that a single tool call.

## The tools

| Tool | Purpose |
| --- | --- |
| `cross_model_review` | Start a review. Returns a `review_id` immediately. |
| `cross_model_review_result` | Wait for and return the review. Long-polls, so one call is usually enough. |
| `cross_model_review_status` | Is the reviewer CLI installed and signed in? Costs nothing, calls no model. |
| `cross_model_review_cancel` | Stop a review that is still running. |

Reviews are asynchronous because a serious review of real work takes minutes. Starting
and collecting are separate calls, so the harness is never blocked on a single
long-running request.

## Setup

Pick the direction you need:

- **[Claude Code reviewed by Codex](examples/claude-code-reviewed-by-codex/)** — project
  `.mcp.json`.
- **[Codex reviewed by Claude](examples/codex-reviewed-by-claude/)** — project
  `.codex\config.toml`.

Both come down to two steps: copy `cross-review.exe` into `tools\` in your project, and
add one entry to a project config file. Both are committed, so a fresh clone works.

> Codex only loads project-level config for **trusted** folders — the
> `[projects.'c:\path'] trust_level = "trusted"` entry it writes when you first approve
> the folder. And note that `codex mcp list` / `codex doctor` report only global config,
> so they will never show a project-level server; use `cross_model_review_status` from
> inside a session to verify.

## Configuration

Everything is a CLI argument on the MCP server entry, so the project's config file is
the single source of truth. There is no config file of our own to drift out of sync.

```
--reviewer <claude|codex>   Which CLI reviews. Required. Pick the model that is NOT
                            the calling agent.
--model <id>                Reviewer model. Pin the full id, never an alias.
--effort <level>            claude: low|medium|high|xhigh|max
                            codex:  low|medium|high|xhigh|max|ultra
--bin <path>                Reviewer CLI path, if not on PATH.
--timeout-seconds <n>       Per-turn budget. Default 900.
--cwd <path>                Review root. Defaults to the server's working directory.
--state-dir <path>          Where named sessions live.
--sandbox <mode>            Codex sandbox policy. Default read-only.
--tools / --allow-tools     Override the Claude reviewer's read-only tool policy.
--preamble-file <path>      Replace the built-in reviewer preamble.
--no-preamble               Send the caller's instructions with nothing added.
--allow-reviewer-mcp        Let the reviewer load its own MCP servers (see below).
--doctor                    Check CLI and auth from a terminal, then exit.
```

Defaults are `claude-opus-5` at `high` and `gpt-5.6-terra` at `xhigh`.

> Pin models by full id. `--model opus` resolves to an older model — verified: it
> reported `claude-opus-4-8`, not Opus 5.

## Re-reviewing after you act on feedback

Sessions are named, and you choose the names. Calling `cross_model_review` again with
the same `session` resumes that reviewer's conversation, so it still remembers what it
told you and reports what is now resolved, what is still open, and what regressed.

```
cross_model_review(session: "auth-refactor", instructions: "First pass at the token
                   refresh path. Focus on the race between refresh and revoke.")
... act on the findings ...
cross_model_review(session: "auth-refactor", instructions: "Addressed findings 1 and 3.
                   I disagree with 2 because the lock is already held; check that.")
```

The name-to-session mapping is stored on disk, so a review survives an MCP server
restart. Pass `fresh: true` when earlier findings would only mislead.

## What the reviewer can and cannot do

The reviewer gets **read-only access** to the repository, so you never need to paste code
into the request. How that is enforced differs by reviewer, and the difference is worth
understanding:

- **Codex reviewer** — `--sandbox read-only`. Enforced by the OS, so it holds regardless
  of what the model tries to run. The Codex reviewer keeps shell access.
- **Claude reviewer** — `--tools Read,Grep,Glob`. Write tools *and* Bash are absent from
  the session entirely, so there is nothing to attempt. Read, Grep and Glob are Claude
  Code's own tools and have no write or execute capability.

The Claude reviewer has no shell by default, and that is a deliberate reversal. Claude's
permission patterns match by **command prefix**, so `Bash(git diff:*)` permits *any*
arguments to `git diff` — including `--output=<file>`, which writes. This was verified,
not theorised: with that pattern allow-listed, the reviewer ran
`git diff --output=PWNED_DIFF.txt HEAD~1 HEAD` and created a 354-byte file. `git log`,
`git show` and `git blame` accept `--output` too, and the problem is not specific to git —
read-oriented tools routinely have flags that write files or execute programs (ripgrep's
`--pre` runs an external command). Shell redirection *is* caught by the permission parser
(`git status --short > REDIR.txt` was denied), but that closes only the obvious hole.

A prefix allow-list therefore cannot express "read-only", so the default grants no shell
at all. If you want the Claude reviewer to run commands, opt in explicitly and understand
that it is a soft boundary rather than a guarantee:

```
--tools "Read,Grep,Glob,Bash" --allow-tools "Read Grep Glob Bash(git diff:*)"
```

Any commands the reviewer attempted but was not permitted to run are reported back with
the review, so an analysis thinned by missing evidence is visible rather than silent.

By default the reviewer also loads **no MCP servers**. This matters because `codex exec`
does start configured MCP servers (verified with a marker server that left a file behind),
so without isolation a reviewer that also has cross-review registered could call it
recursively.

For the Claude reviewer this costs nothing but MCP servers (`--strict-mcp-config`). For
the Codex reviewer it is `--ignore-user-config`, which skips the whole of the user's
`config.toml` — the blunter option, chosen because `-c mcp_servers={}` does not work
(dotted overrides merge into the existing table rather than replacing it). Auth still
resolves from `CODEX_HOME`, and model, effort and sandbox are all passed explicitly, so a
review is unaffected. `--allow-reviewer-mcp` turns isolation off if you would rather keep
the reviewer's own configuration.

## When the reviewer is unavailable

If the reviewer CLI is missing, signed out, rate-limited, or rejects the pinned model,
the tool call **fails loudly** with a machine-readable code, an explanation, and the
exact remediation to hand the user:

```
CROSS-MODEL REVIEW FAILED
code: NOT_AUTHENTICATED

The 'codex' CLI is installed but not signed in, so it cannot review.

=== ACTION REQUIRED ===
The external review did not run, so there is no review feedback. Stop the current task
now. Do not review the work yourself in place of the external reviewer, and do not
continue as if the review had passed.

Report this to the user:

  To fix it, run this in a terminal:

    codex login
...
```

Codes: `CLI_NOT_FOUND`, `NOT_AUTHENTICATED`, `AUTH_EXPIRED_MIDRUN`, `MODEL_UNAVAILABLE`,
`RATE_LIMITED`, `TIMEOUT`, `SPAWN_FAILED`, `REVIEWER_FAILED`, `EMPTY_REVIEW`,
`SESSION_NOT_FOUND`, `CANCELLED`. Bad tool arguments get a plain correction instead,
since that is the agent's own mistake and not something to escalate.

An expired reviewer session is handled rather than escalated: the stale mapping is
dropped, the review runs in a fresh session, and the response says so.

Preflight runs before any model call, so a misconfigured machine fails in seconds
instead of after a minute of billed work.

## Building

Requires Rust (stable, MSVC). Everything else is vendored.

```powershell
.\build.ps1
```

That runs `cargo fmt --check`, clippy with warnings as errors, the tests, and a release
build, then stages `dist\cross-review.exe` — the copy committed for vendoring.

Check a setup from a terminal without starting an agent:

```powershell
.\dist\cross-review.exe --reviewer codex --doctor
```

## Testing

```powershell
cargo test          # 77 unit tests: no network, no model calls
.\smoke.ps1 -Reviewer codex     # end to end against the real CLI
.\smoke.ps1 -Reviewer claude
```

`smoke.ps1` speaks real MCP over stdio to the built executable and checks the whole
round trip: initialize, `tools/list`, a live review, a resumed follow-up review that
proves the reviewer retained context, the error paths, and that session state landed on
disk. It calls the reviewer model for real, so it costs tokens — it defaults to
`--effort low` for that reason.

Both directions pass against live CLIs.

## Design notes

- **Hand-rolled MCP.** The protocol surface needed is four methods of JSON-RPC 2.0 over
  newline-delimited stdio. Keeping dependencies at `serde` is what makes a 520 KB
  self-contained binary possible.
- **Prompts go over stdin**, not the command line, so a large review request cannot hit
  the Windows command-line length limit or a quoting bug.
- **Sessions on disk, in-flight reviews in memory.** Review ids are per-process;
  the session mapping outlives the process. Because two servers can share a project's
  state directory, mutations take a cross-process lock file and every write goes to a
  pid-unique temp file before an atomic replace. A session that cannot be persisted is
  reported as a warning with the review, since the response otherwise promises a resume
  that would not work.
- **Timeout and cancel kill the process tree**, not just the direct child. On Windows a
  killed parent orphans its descendants, and an orphan holding an inherited pipe would
  keep our reader threads blocked forever. Output collection is bounded too, so a stuck
  pipe degrades diagnostics instead of hanging the review.
- **stdout is protocol traffic only.** Diagnostics go to stderr.
