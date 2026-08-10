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
- `--ignore-user-config` alone is not a project-isolation control: the official CLI contract
  says it skips `$CODEX_HOME/config.toml`, while trusted `.codex/config.toml` files are a
  separate higher-precedence layer. Adding `--ignore-rules` plus
  `projects.'C:\dev\simple-cross-model-review'.trust_level='untrusted'` skipped that project
  layer. A command-line evidence fixture deliberately named `cross_review` then replaced the
  same-named project MCP server, `repository_scope` succeeded, and
  `cross_model_review_status` was absent from the model's tool list.
- That trust-key probe establishes the bug and one matching path, but path identity across
  Windows processes is not a sound fail-closed boundary. The product design therefore no
  longer depends on a `projects.<path>.trust_level` override. Isolated fresh and resumed
  turns will use one cross-review-owned empty working directory outside the reviewed root,
  `--skip-git-repo-check`, `--ignore-user-config`, and `--ignore-rules`. Before every turn the
  parent must canonicalize that directory, reject reparse points, reject any entry (including
  `.codex`), and verify it is outside the reviewed root. Failure refuses the review. This
  makes case, trailing-separator, short-name, junction, and UNC spelling differences
  irrelevant rather than attempting to predict Codex's project-key normalization.
- The same observation corrects a live product claim: the shipping Codex invocation uses the
  reviewed repository as its working directory and passes only `--ignore-user-config`, so a
  trusted repository's project config and rules are not presently excluded. The sterile-root
  change and corresponding README/AGENTS correction apply to every isolated Codex review,
  not only to evidence-enabled turns.
- The initial supported floor is `codex-cli 0.144.5`, the version these required/approval/
  strict-config/fresh/resume/collision probes exercised. Compatibility is behavior-gated as
  well: a CLI that rejects the strict config or cannot initialize the required server fails
  closed before a review verdict.

Unit coverage verifies the fixture's nonce validation, initialize/tools-list surface,
read-only annotations, approval-control annotations, structured scope result, and rejection
of unknown tools/extra arguments. Formatting, clippy with `-D warnings`, all 525 unit tests,
and a release build passed after the Phase 0 fixture and control were added.

## Still required before product dependency

- Test forced cancel, timeout, parent crash, and provider-child cleanup. Normal EOF is not a
  substitute for those paths.
- Exercise both a fresh and resumed probe from the same verified sterile working directory.
  The final product must perform the empty/outside/non-reparse preflight before every turn.
- Replace this temporary fixture with the real capability-bundle handshake and evidence
  dispatcher; do not ship the fixture as the product service.

If any remaining check fails, amend the architecture and repeat the plan review before making
the evidence service mandatory. Do not fall back to exec-policy allow rules.
