# Codex reviewer evidence service: Phase 0 record

Date: 2026-08-10

Branch: `codex/issue-47-reviewer-evidence`

Plan: [`codex-reviewer-evidence-service.md`](codex-reviewer-evidence-service.md)

This records observed external behavior separately from the approved design. It is not a
claim about other Codex releases. The probes used `codex-cli 0.144.5` on Windows 11 with the
full model id `gpt-5.6-luna`, low effort, `--sandbox read-only`, `--ignore-user-config`, and
`--strict-config`. Successful probes made real model calls and cost tokens.

## Phase 0 fixture (now replaced)

`cross-review --evidence-probe-server <nonce> [approval-control]` was a temporary hidden MCP
mode handled before normal server configuration. It exposes exactly one closed-schema tool,
`repository_scope`, which returns only its nonce, PID, schema version, and whether the process
belongs to any Windows job. It constructs no `App`, exposes no review tool, and performs no
filesystem, shell, write, or network operation.

The product implementation removed that flag after these facts were established. Its hidden
mode is now `--evidence-server <bundle> <nonce>` with the real seven-tool dispatcher.

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
- A fresh turn and an explicit-ID resume ran from the same newly created empty directory
  outside any repository with `--skip-git-repo-check`, `--ignore-user-config`, and
  `--ignore-rules`. Each turn loaded a newly injected evidence fixture and returned its new
  nonce (`sterile-fresh`, then `sterile-resume`). The directory contained zero entries after
  each turn, and both reported fixture PIDs had exited afterward. The CLI warning about an
  unsupported PowerShell shell snapshot went to stderr and created no working-directory
  artifact. These two real model turns used 96,654 input tokens (86,016 cached) and 295 output
  tokens in total.

The Phase 0 fixture had unit coverage for nonce validation, initialize/tools-list, annotations,
structured results, and malformed calls. The replacement service now covers bundle/schema/nonce
validation, closed input and output schemas, bounded pagination and counters, path/device/ADS/
junction rejection, UTF-8 and digest handling, hardened Git providers, sterile-root validation,
fresh/resume argv identity, and evidence-specific failure classification.

A final real product smoke on `codex-cli 0.144.5` passed the no-model status handshake, fresh and
explicit-ID resume evidence calls, detached-poll collection, explicit cancellation, evidence-child
reaping, and the zero-shell-policy-denial assertion. The completed fresh/resume turns reported
127,747 input tokens (108,032 cached) and 538 output tokens; the detached-poll review also completed
and reported 267,547 input tokens (218,112 cached) and 843 output tokens.

The smoke found two lifecycle defects before that pass. First, a process-ID watcher treated the
anonymous pipe owner as the durable Codex parent and closed a longer turn; a concurrent stdin reader
now makes EOF the authoritative parent-death signal and can cancel an active provider while it is
running. Second, a maximum 196 KiB change page was duplicated into both MCP `content` and
`structuredContent`, producing a 398,516-byte envelope that crossed the 256 KiB transport cap and
terminated the service. Results now put the complete page only in `structuredContent` with concise
text, as the MCP contract intends, and any remaining envelope overflow is an in-band retryable tool
error rather than transport death. A unit test fixes the maximum-page envelope at the real cap.

## Product dependency gates

- Keep the real smoke's explicit cancel and detached-poll cases, its evidence-child process check,
  and the provider runner's kill-on-close job/cancellation tests green. The MCP stdin reader runs
  concurrently with provider work, so parent EOF sets the same cancellation flag the provider
  runner observes instead of waiting for the operation deadline.
- Do not restore the temporary probe or make model-selected tool use an availability gate. The
  no-model handshake plus Codex `required=true` startup are the deterministic gate.

If any remaining check fails, amend the architecture and repeat the plan review before making
the evidence service mandatory. Do not fall back to exec-policy allow rules.
