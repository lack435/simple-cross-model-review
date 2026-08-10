# Codex reviewer evidence service: Phase 0 record

Date: 2026-08-10

Branch: `codex/issue-47-reviewer-evidence`

Plan: [`codex-reviewer-evidence-service.md`](codex-reviewer-evidence-service.md)

This records observed external behavior separately from the approved design. It is not a
claim about other Codex releases. The probes used `codex-cli 0.144.5` on Windows 11 with the
full model id `gpt-5.6-luna`, low effort, `--sandbox read-only`, `--ignore-user-config`, and
`--strict-config`. Successful probes made real model calls and cost tokens.

## Fixture implemented

`cross-review --evidence-probe-server <nonce> [approval-control]` is a temporary hidden MCP
mode handled before normal server configuration. It exposes exactly one closed-schema tool,
`repository_scope`, which returns only its nonce, PID, schema version, and whether the process
belongs to any Windows job. It constructs no `App`, exposes no review tool, and performs no
filesystem, shell, write, or network operation.

`approval-control` changes only MCP annotations so the otherwise harmless tool requires an
approval under Codex's default `auto` mode. It exists to test whether an `approve` setting on
the evidence server leaks to a second server.

## Verified on the installed CLI

- An explicit `mcp_servers.<name>` entry loads under `--ignore-user-config` on a fresh turn.
- The same entry is recreated and callable on `codex exec resume`; the returned nonce changed
  from the fresh turn's value to the resume turn's configured value.
- The JSONL stream reports `mcp_tool_call` start/completion items with `server`, `tool`,
  `arguments`, `result`, `error`, and `status`. Structured output is exposed as
  `result.structured_content` in Codex's JSONL spelling.
- A nonexistent server with `mcp_servers.<name>.required=true` aborts session creation with
  `required MCP servers failed to initialize`. No `thread.started`, `turn.started`, or usage
  event appeared, so the prompt did not reach a model.
- `mcp_servers.approved_probe.default_tools_approval_mode="approve"` allowed the evidence
  tool, while an approval-control tool on a second server was cancelled. The setting is
  therefore observed to be server-scoped in this CLI/configuration.
- A second ordinary read-only MCP tool without an explicit approval setting also ran. That is
  Codex's `auto` behavior based on read-only annotations, not leakage from the first server;
  the approval-control probe above distinguishes the cases.
- The MCP server reports `current_process_in_job=false` when Codex launches it. It does **not**
  inherit cross-review's outer reviewer job, so implementation must not rely on that job for
  service cleanup.
- After the Codex turn ended, the reported evidence-server PID no longer existed. The normal
  stdio-EOF path therefore reaped this fixture even though it had broken away from the job.

Unit coverage verifies the fixture's nonce validation, initialize/tools-list surface,
read-only annotations, approval-control annotations, structured scope result, and rejection
of unknown tools/extra arguments. Formatting, clippy with `-D warnings`, all 525 unit tests,
and a release build passed after the Phase 0 fixture and control were added.

## Still required before product dependency

- Prove command-line precedence over a colliding user/project server entry.
- Prove `--ignore-user-config` suppresses both user and trusted-project MCP entries, not only
  the base user config.
- Run the fixture against the eventual lowest supported Codex CLI version, not only 0.144.5.
- Test forced cancel, timeout, parent crash, and provider-child cleanup. Normal EOF is not a
  substitute for those paths.
- Replace this temporary fixture with the real capability-bundle handshake and evidence
  dispatcher; do not ship the fixture as the product service.

If any remaining check fails, amend the architecture and repeat the plan review before making
the evidence service mandatory. Do not fall back to exec-policy allow rules.
