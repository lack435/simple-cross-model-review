# AGENTS.md

Instructions for coding agents working in this repository. `CLAUDE.md` imports this file,
so it applies to Claude Code and Codex alike.

## What this project is

`cross-review` is a Windows-only MCP server that hands work to a *different* model for
review and returns what it said. Rust, MSVC toolchain, `serde` as the only dependency —
the small self-contained binary is a feature, so do not add crates casually. See
[README.md](README.md) for the full design.

## Pull requests

**Every PR must be reviewed by a different model through this repository's own
`cross-review` MCP server before it is approved to merge.** This is a blocking gate, not a
suggestion. We eat our own dog food: the merge gate for cross-review is cross-review.

- Call `cross_model_review` with a session named for the branch or PR, and collect it with
  `cross_model_review_result`. Both directions are already wired up in this checkout —
  Claude Code gets Codex via [`.mcp.json`](.mcp.json), Codex gets Claude Opus 5 via
  [`.codex/config.toml`](.codex/config.toml) — so the reviewer is always the model that did
  not write the diff.
- Give the reviewer the branch and base to diff itself, say what changed and why, and point
  it at this file and `README.md`. It runs configuration-isolated, so `CLAUDE.md` is not
  auto-loaded; it will read convention files when told to.
- The reviewer reviews; it cannot fix — it has no write access. Bring its findings back and
  act on them yourself.
- After acting on feedback, call `cross_model_review` again with the **same session** so the
  reviewer reports what is resolved, what is still open, and what regressed. Only use
  `fresh: true` when the earlier findings would mislead.
- Never approve, merge, or tell the user a PR is ready to merge without that review having
  run and its findings resolved or explicitly dismissed with a reason.
- If the review fails — `CLI_NOT_FOUND`, `NOT_AUTHENTICATED`, `RATE_LIMITED`, any of the
  codes in the README — hand the user the remediation the tool returned, say the review did
  not run, and stop. Do not substitute your own read of the diff, and do not fall back to a
  same-model subagent: a model reviewing its own work shares its own blind spots, which is
  the entire premise of this project. `cross_model_review_status` checks the reviewer CLI
  and auth for free, before anything is billed.
- Summarise the outcome for the user: what the reviewer flagged, what changed in response,
  and anything deliberately left alone.

Dogfooding is also how the tool gets tested in anger. If the gate itself misbehaves —
a failure code that misreports the reviewer's state, a resumed session that lost context, a
response that reads badly to the calling agent — that is a bug in this repository, so report
it rather than working around it.

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
