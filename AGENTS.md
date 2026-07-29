# AGENTS.md

Instructions for coding agents working in this repository. `CLAUDE.md` imports this file,
so it applies to Claude Code and Codex alike.

## What this project is

`cross-review` is a Windows-only MCP server that hands work to a *different* model for
review and returns what it said. Rust, MSVC toolchain, `serde` as the only dependency —
the small self-contained binary is a feature, so do not add crates casually. See
[README.md](README.md) for the full design.

## Pull requests

**Every PR must be reviewed by an Opus subagent before it is approved to merge.** This is a
blocking gate, not a suggestion.

- Spawn the review with the `Agent` tool using `model: "opus"`, giving it the PR's full
  diff (or the branch and base to diff itself) plus this file and `README.md` as context.
- The subagent reviews; it does not fix. Bring its findings back and act on them yourself.
- Never approve, merge, or tell the user a PR is ready to merge without that review having
  run and its findings resolved or explicitly dismissed with a reason.
- If the subagent cannot run, say so and stop. Do not substitute your own read of the diff
  for the Opus review — a model reviewing its own work shares its own blind spots, which is
  the entire premise of this project.
- Summarise the outcome for the user: what the reviewer flagged, what changed in response,
  and anything deliberately left alone.

This gate is separate from the `cross-review` MCP server. That tool is for cross-*model*
feedback during development (Claude asks Codex here, Codex asks Claude via
`.codex/config.toml`); the Opus subagent review is the merge gate. Use both — they catch
different things.

## Before handing work back

```powershell
.\build.ps1
```

Runs `cargo fmt --check`, clippy with `-D warnings`, the unit tests, and a release build,
then stages `dist\cross-review.exe`. Both MCP configs point at `dist\`, so restaging needs
open agent sessions unloaded first; `build.ps1` reports the blocking PIDs rather than
shipping a stale binary.

- `cargo test` — unit tests only, no network and no model calls.
- `smoke.ps1 -Reviewer codex|claude` — real end-to-end MCP round trip. It calls a model for
  real and costs tokens, so run it when the change touches the protocol, spawning, or
  session handling, and mention the cost to the user.
- CI is Windows-only by design and additionally re-verifies the `CLI_NOT_FOUND` failure
  contract against the shipped binary. Do not weaken that check.

## Conventions that are easy to get wrong

- **Never commit `cross-review.exe`.** `dist\` is gitignored; releases are built and
  published by the tag-driven CI workflow, never from a workstation.
- **Pin models by full id** (`claude-opus-5`, `gpt-5.6-terra`). Aliases resolve to older
  models.
- **stdout is protocol traffic only.** All diagnostics go to stderr.
- **The reviewer's isolation and read-only posture are security boundaries.** The tool
  policy, `--safe-mode` / `--ignore-user-config`, the path-scoped `Read(./**)` grants, and
  the job-object process reaping all exist for reasons documented in the README with
  verified evidence. Do not relax any of them without saying plainly what boundary moves.
- **Claim only what was verified.** The README distinguishes "verified" from "assumed"
  deliberately. Keep that discipline in code comments and in what you tell the user.
