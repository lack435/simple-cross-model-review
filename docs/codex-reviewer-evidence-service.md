# Codex reviewer evidence service

Status: proposed implementation plan for [issue #47](https://github.com/lack435/simple-cross-model-review/issues/47)

## Decision

Do not add exec-policy allow rules for `git grep`, `git ls-files`, or a PowerShell
prefix. Instead, give the isolated Codex reviewer a small, repository-scoped MCP server
implemented by `cross-review` itself. It will expose bounded read, list, search, diff, and
history operations without asking Codex's non-interactive shell router to approve a
command.

This deliberately solves the larger problem rather than only the two commands named in
the issue. A code reviewer needs a dependable evidence plane. It should not have to know
which harmless-looking shell spellings a particular Codex release recognizes, and a
security boundary should not depend on a prompt asking the model to avoid the wrong
spelling.

The existing Windows restricted-token sandbox remains in force for every shell command
the Codex reviewer might still request. The evidence server adds no write or network tool,
does not load repository code or configuration, and runs in the reviewer's existing
kill-on-close job object.

## Why an allow-list is the wrong boundary

The failure is reproducible on Windows 11 with `codex-cli 0.144.5`. Under the same
`--sandbox read-only`, `--ignore-user-config`, explicit sandbox override, and full model id
used by the reviewer, this request was declined before execution:

```text
"C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe" -Command
  'git grep -n POLICY_DENIAL_MARKER'
rejected: blocked by policy
```

Two narrower hypotheses were ruled out:

- Explicit `approval_policy="never"` produced the same refusal.
- Enabling Codex's stable `unified_exec` feature changed the wrapper to
  `powershell.exe -NoProfile -Command`, but produced the same refusal.

The replacement path was also probed before this plan was committed. An explicit stdio MCP
entry added with `-c mcp_servers.issue47_probe.*` loaded under `--ignore-user-config` and
`--strict-config` on both a fresh turn and `codex exec resume`. With `required=true` and
`default_tools_approval_mode="approve"`, the model called the server tool successfully. The
JSONL stream produced paired `item.started` / `item.completed` events of type
`mcp_tool_call`, carrying server, tool, arguments, result, error, and status -- enough for
the parent to attest a completed evidence call without parsing model prose. Without the
explicit tool approval mode, the non-interactive call was cancelled, so that setting is a
required part of the design rather than an incidental test detail.

The official Codex rules documentation says that a matching `allow` rule runs the command
*outside* the sandbox. It also documents safe decomposition for `bash -lc` and related
POSIX wrappers, not PowerShell. `codex execpolicy check` found no matching rule for either
the observed PowerShell argv or direct `git grep` argv. A rule broad enough to match
`powershell.exe -Command` would therefore authorize an arbitrary script outside the
restricted-token boundary. A direct `git` rule does not match the observed Windows wrapper.

That is not an acceptable trade: fixing missing read evidence by weakening the verified
write boundary would make the product less trustworthy. The relevant current references
are the [Codex rules documentation](https://developers.openai.com/codex/rules) and
[configuration reference](https://developers.openai.com/codex/config-reference).

## Goals

1. An isolated Codex reviewer can list repository files, search content, read files, inspect
   the configured change, and request bounded history without shell-policy denials.
2. The default review path preserves configuration isolation, the OS-enforced no-write
   boundary, no-network behavior, process-tree cleanup, and stdout-as-protocol-only.
3. Evidence operations are path-confined, deterministic, resource-bounded, observable, and
   testable without a model call.
4. A missing or incompatible evidence service fails visibly. It must not silently turn a
   full review into a thinner review that can still report approval.
5. Resume turns get the same evidence contract as fresh turns and cannot inherit a stale
   repository root or stale service instance.
6. The abstraction is VCS-neutral at the MCP layer. Git and Perforce may implement different
   history/change providers underneath it, but the reviewer gets one vocabulary.

## Non-goals

- Do not make general shell execution policy configurable through `cross-review`.
- Do not claim the Codex reviewer's filesystem reads are confined; its existing shell can
  still read outside the repository. The new service itself will be confined.
- Do not evaluate arbitrary regex engines, shell strings, Git options, revision expressions,
  pathspec magic, repository hooks, filters, pagers, or external diff drivers supplied by
  the reviewer or repository.
- Do not add a network-capable evidence operation.
- Do not remove policy-denial reporting. Denials remain useful evidence for shell calls that
  occur outside the new service.

## Architecture

### 1. A second, internal MCP mode

Add a hidden `--evidence-server` entry mode before normal `Config` parsing in `main.rs`.
It speaks MCP over stdio but exposes only evidence tools; it does not construct `App`, create
review state, load reviewer configuration, authenticate a model, or expose any
`cross_model_review*` tool. Keeping the dispatcher separate makes recursion impossible by
construction rather than by tool description.

The parent process launches its own absolute executable path as this server through explicit
`-c mcp_servers.<reserved-name>.*` overrides on `codex exec`. Those overrides are passed on
fresh and resumed turns. Default isolation still passes `--ignore-user-config`, so user and
project MCP servers, hooks, skills, and settings remain absent; the server-owned MCP entry is
added afterward at invocation precedence. Add `--strict-config` so an older Codex CLI that
does not accept the generated MCP configuration fails before a billed review can masquerade
as fully equipped. Set `required=true`, `enabled=true`, an exact `enabled_tools` list, and
`default_tools_approval_mode="approve"`. Auto-approval is safe only because this is a
server-owned entry whose complete tool surface is read-only; it must never be applied to a
user or repository MCP server.

The internal server receives an opaque, per-turn capability file path rather than a root and
options assembled from model input. The parent creates that file in the existing state/temp
area with the canonical root, VCS kind, configured diff description, limits, and a unique
turn nonce. The child validates schema version, nonce, file ownership assumptions available
on Windows, and canonical root before serving. The file contains no auth token or reviewer
credential and is deleted with the other turn temporaries.

### 2. One evidence vocabulary

Expose the following tools with closed JSON schemas (`additionalProperties: false`) and
small defaults:

- `repository_scope()` returns the canonical root, VCS kind, configured change label,
  current status summary, service schema version, limits, and per-turn nonce. The reviewer
  must call this first.
- `repository_list(path?, cursor?, limit?)` returns repository-relative entries and type,
  with deterministic ordinal pagination. It never follows directory links or descends into
  VCS metadata, build output, or the service's state directory.
- `repository_search(query, path?, cursor?, limit?)` performs literal UTF-8 text search and
  returns path, line number, and a capped line excerpt. Literal search is the secure and
  dependency-free base contract; regex can be proposed later as a separate reviewed feature.
- `repository_read(path, start_line?, line_count?)` returns numbered UTF-8 text lines plus
  byte size and a content digest. Binary, non-UTF-8, over-limit, link-escaped, device, and
  alternate-data-stream paths return explicit typed errors.
- `repository_change(cursor?, limit_bytes?)` returns the already selected/captured change
  and omission metadata in pages rather than asking the reviewer to reconstruct it with
  `git diff` or `p4 describe`.
- `repository_history(path?, before?, cursor?, limit?)` returns a normalized, bounded commit
  or changelist summary. A subsequent `repository_revision(id, path?, cursor?)` reads a
  validated revision through fixed provider-owned arguments. Reviewer input is data, never
  spliced into an option or shell command.

Every result reports `complete`, `truncated`, and a continuation cursor. A cap can reduce an
answer, but it cannot silently label a prefix as the whole result. Cursors are opaque,
server-held state bound to the per-turn nonce and normalized operation arguments, so they
cannot be reused for a different root, query, or revision.

### 3. Shared evidence core, VCS-specific providers

Create an `evidence` module independent of MCP JSON. Unit-test it directly, then adapt its
typed requests/results in the internal MCP dispatcher.

Filesystem operations canonicalize the root once and validate every requested component.
Reject absolute paths, `..`, reserved device names, alternate data streams, and any existing
component that resolves through a symlink or junction outside the root. Open handles, then
verify final paths before reading to close path-check/path-open races as far as the standard
library and existing Win32 surface allow. Listing and search do not follow reparse points.

Reuse the repository's existing capture hardening instead of inventing a second Git/Perforce
runner. Git subprocesses are resolved from `PATH`, never the application directory, receive
fixed argv, `--no-pager`, `--no-ext-diff`, `--no-textconv`, disabled fsmonitor, and isolated
Git config/pager environment. Perforce history/change access reuses the current client and
network policy decisions. No provider invokes a shell. Unsupported provider capabilities
return `unsupported`, not guessed output.

The configured change is captured once by the parent before the reviewer starts and stored
in the capability bundle with the same omission accounting used in the prompt today. The
service pages that immutable snapshot. Live file reads include a digest and stat metadata;
if the working tree changes after capture, `repository_scope` and subsequent reads surface
drift rather than presenting two revisions as one coherent snapshot.

### 4. Invocation and attestation

Extend `Invocation` with an evidence expectation. For an isolated Codex review, parsing is
successful only if the Codex JSON event stream contains a completed call to
`repository_scope` whose result carries the expected turn nonce and schema version. This
cheap first call proves that:

- the generated `-c mcp_servers...` override was accepted;
- the internal server started and completed MCP initialization;
- the reviewer could call the service; and
- the service was bound to this turn and root.

The preamble tells the reviewer to use evidence tools for repository discovery and to reserve
the shell for information the service explicitly does not provide. It removes the current
advice to avoid `git grep`/`git ls-files`; tool descriptions, not a release-specific list of
forbidden spellings, become the primary route.

If attestation is absent, malformed, from another nonce, or reports a narrower schema than
required, return a dedicated `EVIDENCE_UNAVAILABLE` failure with remediation. Do not return
the review text or a machine `approve` verdict. A service tool failure after attestation is
included in the review warnings and structured result; whether it is fatal is based on the
operation's typed error (`invalid request` is not an infrastructure failure, `service died`
is). Continue collecting ordinary shell policy denials independently.

Non-isolated `--allow-reviewer-config` reviews still receive the server-owned evidence entry
at command-line precedence. Tests must show that a user entry with the reserved name cannot
replace its command, args, enabled state, or tool set. If Codex's merge semantics cannot
guarantee that, reject the name collision visibly rather than running a user-defined server
under a trusted tool name.

### 5. Limits, cancellation, and cleanup

Use the existing capture budget as the parent ceiling and add per-call limits below it:

- maximum returned bytes, matches, files, line length, and history records;
- maximum request count and cumulative returned bytes per turn;
- a short operation deadline checked during walks/searches;
- bounded MCP request and response sizes; and
- cancellation propagated from review cancellation and job termination.

All counters use checked arithmetic. Directory walks are iterative and deterministic. The
child logs diagnostics only to stderr. The evidence server and any provider subprocess stay
inside the existing reviewer job object and die on cancel, timeout, server shutdown, or
parent exit. Temporary capability/snapshot files are best-effort deleted on every terminal
path and are bounded/expired so a crash cannot grow state forever.

## Implementation sequence

### Phase 0: compatibility proof

The installed development CLI has already proved that explicit `-c mcp_servers...` entries
load after `--ignore-user-config` on both fresh and `exec resume` forms, and its actual JSONL
tool-call event shape is recorded above. Turn that manual probe into a tiny test fixture,
then complete the compatibility proof by capturing CLI behavior when a required server
fails, checking command-line precedence over a colliding user config, and testing the lowest
supported Codex CLI version. If any remaining premise fails, stop and amend this plan; do
not silently fall back to exec-policy rules.

### Phase 1: evidence core

Implement typed scope/list/search/read/change/history APIs, canonical path confinement,
pagination cursors, caps, drift metadata, provider hardening, and direct unit tests. Keep
serialization and MCP concerns out of this layer.

### Phase 2: internal MCP server

Add the hidden entry mode, closed schemas, dispatcher, typed error rendering, nonce-bound
capability loading, protocol tests, stdout-purity tests, and malformed/fuzz-style request
tests using only the existing `serde` dependencies.

### Phase 3: Codex integration

Generate strict MCP overrides for fresh/resume invocations, create and clean turn bundles,
extend JSONL parsing for evidence events, enforce attestation, update capabilities/preamble,
and expose service readiness/schema in `cross_model_review_status`.

### Phase 4: result contract and documentation

Add evidence-service warnings/usage to the structured result without breaking its current
schema guarantees, update README security claims and the reviewer matrix, document the new
failure code, and retain policy-denial reporting for remaining shell calls. State separately
what was verified from what is only designed.

### Phase 5: end-to-end verification

Run `build.ps1`, then a real `smoke.ps1 -Reviewer codex` because this changes reviewer
spawning, MCP protocol, session resume, and isolation. The smoke costs model tokens and must
cover both a fresh and resumed turn.

## Test matrix

The implementation is not complete until all of these are automated or, where a real CLI is
unavoidable, captured as a named smoke artifact:

| Area | Required proof |
| --- | --- |
| Invocation | Fresh and resume argv include the same strict, isolated evidence entry; TOML escaping covers spaces, quotes, backslashes, Unicode, and an unrepresentable executable path fails visibly. |
| Isolation | Hostile user/project MCP servers, hooks, rules, skills, and config do not load by default; the evidence server exposes no review, shell, write, or network tool. |
| Attestation | Correct nonce succeeds; missing, replayed, truncated, malformed, collision-replaced, or wrong-schema evidence fails closed. |
| Paths | Absolute, parent, UNC, device, ADS, case-folding, symlink, junction, deleted/replaced, and root-edge cases cannot escape the canonical root. |
| Search/list/read | Deterministic pagination, literal matching, UTF-8/binary handling, long lines, large files, empty repos, subdirectory roots, ignored/untracked files, and cap/timeout accounting. |
| VCS | Git worktree, staged, HEAD and range changes; hostile pager/ext-diff/textconv/fsmonitor/config; submodules; Perforce offline/networked and restricted changelists. |
| Lifecycle | Cancel, timeout, parent crash, MCP EOF, provider hang, output cap, and handler-thread failure reap the complete process tree and clean bounded temporaries. |
| Regression | Existing review capture, findings envelope, fallback chain, usage gate, policy-denial reporting, and Claude reviewer behavior remain unchanged. |
| Security | Model attempts to write inside/outside the repo and reach network still fail; evidence tools reject every write-shaped or option-shaped input; an evidence read succeeds. |
| Acceptance | During a real Codex review, file listing and repository search complete through evidence tools with zero `blocked by policy` denials; the reviewer can cite their results and return a structured verdict. |

## Rollout and compatibility

Make the evidence service mandatory for isolated Codex reviews once Phase 0 establishes the
supported CLI floor. Older/incompatible CLIs receive `EVIDENCE_UNAVAILABLE` with an upgrade
remediation; they do not fall back to an evidence-thinned approval. Claude reviews are
unchanged. Keep the current shell guidance behind a temporary compatibility switch only
during development, remove it before release, and do not expose a permanent flag that turns
the service off while still claiming an equivalent review.

The README may claim success only after the no-model boundary probes and real Codex smoke
both pass. The final claim should be narrow: the reviewer has a verified, repository-scoped
evidence service for listing, searching, reading, change capture, and bounded history; its
separate Codex shell remains read-unconfined and subject to Codex policy.

## Delivery checkpoints

1. Compatibility evidence and core path/security tests.
2. Internal MCP server with protocol and resource-limit tests.
3. Fresh-turn Codex integration and attestation.
4. Resume, collision, cancellation, and failure-contract coverage.
5. Documentation, full build, real Codex smoke, and cross-model review of the exact final
   implementation diff.

Each checkpoint should be independently reviewable. Do not batch a weakened boundary with a
later promise to harden it: any checkpoint that launches the service must already be
path-confined, bounded, isolated, and fail-closed.
