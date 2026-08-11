# Codex reviewer evidence service

Status: approved plan, implemented on `codex/issue-47-reviewer-evidence`

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
and does not load repository code or configuration. Whether Codex's MCP child inherits the
reviewer's kill-on-close job object is a compatibility premise to verify, not a boundary this
plan assumes; the service also owns cleanup for any provider process it starts.

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
`--strict-config` on both a fresh turn and `codex exec resume`. With
`mcp_servers.issue47_probe.required=true` and
`mcp_servers.issue47_probe.default_tools_approval_mode="approve"`, the model called the
server tool successfully. The latter is documented as a per-server key, not a global MCP
default; Phase 0 still verifies that it has no effect on a second server. The
JSONL stream produced paired `item.started` / `item.completed` events of type
`mcp_tool_call`, carrying server, tool, arguments, result, error, and status -- enough for
the parent to observe completed evidence calls without parsing model prose. Without the
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
   the configured change, and (for Git) request bounded history without shell-policy denials.
2. The default review path preserves configuration isolation, the OS-enforced no-write
   boundary, no-network behavior, process-tree cleanup, and stdout-as-protocol-only.
3. Evidence operations are path-confined, deterministic, resource-bounded, observable, and
   testable without a model call.
4. A missing or incompatible evidence service fails visibly. It must not silently turn a
   full review into a thinner review that can still report approval.
5. Resume turns get the same evidence contract as fresh turns and cannot inherit a stale
   repository root or stale service instance.
6. The abstraction is VCS-neutral at the MCP layer. Provider-specific gaps are explicit
   `unsupported` results; they are never papered over by network access or shell fallback.

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

## Scope choice

This is intentionally larger than an issue-specific prompt workaround because the rejected
commands are symptoms of a release-specific shell router, while dependable repository
evidence is a product requirement. The project remains one Windows executable with its
existing `serde` dependencies; the evidence mode is a second dispatcher in that executable,
not a new daemon or package.

The first implementation is nevertheless bounded: scope/list/search/read/change for local
workspaces, plus Git history/revision because the issue explicitly includes Git discovery
and history. It does not add regex, Perforce history/network access, general command
execution, write tools, or a plugin system. Those omissions are conscious stopping points,
not deferred claims. This balance accepts internal blast radius to remove the policy-router
dependency without turning `cross-review` into a general repository service.

## Architecture

### 1. A second, internal MCP mode

Add a hidden `--evidence-server` entry mode before normal `Config` parsing in `main.rs`.
It speaks MCP over stdio but exposes only evidence tools; it does not construct `App`, create
review state, load reviewer configuration, authenticate a model, or expose any
`cross_model_review*` tool. Keeping the dispatcher separate makes recursion impossible by
construction rather than by tool description.

The parent process launches its own absolute executable path as this server through explicit
`-c mcp_servers.<reserved-name>.*` overrides on `codex exec`. Those overrides are passed on
fresh and resumed turns. Default isolation passes `--ignore-user-config`, `--ignore-rules`,
and `--skip-git-repo-check`, and runs Codex from a canonical, cross-review-owned empty working
directory outside the reviewed repository. The parent creates that directory without links,
verifies before every fresh or resumed turn that it is still a directory, contains no
`.codex` layer or other entries, and is not within the reviewed root, and refuses the review
if any invariant fails. The directory is stable for the lifetime of a reviewer session so
resume uses the same working root. Empty means empty: if the Codex CLI or anything else
creates a file there, the next-turn preflight refuses the review. Phase 0 must demonstrate
that the supported CLI keeps session/history/log state elsewhere on both fresh and resume;
if it does not, stop and redesign the sterile-root lifecycle rather than weakening the check.

This sterile-root design removes reviewed-project path spelling from the isolation boundary:
Codex has no reviewed-project `.codex/` layer to discover regardless of how Codex normalizes
drive letters, case, trailing separators, short names, junctions, or UNC paths. The first flag
skips `$CODEX_HOME/config.toml`, the second skips user/project exec-policy rules, and the empty
working root removes the reviewed repository's config, hooks, agents, and other project layer
even when the caller has trusted that checkout. `--ignore-user-config` alone does **not**
suppress trusted-project config. System/managed policy remains in force. This is also a repair
to the currently shipping Codex reviewer isolation boundary, which today runs in a trusted
reviewed checkout with only `--ignore-user-config`; it is not conditional on the evidence
service being used and its README/AGENTS claims must be corrected in the same implementation.

Moving the shelled Codex reviewer out of the repository deliberately changes its repository
access model. Evidence tools become the complete, primary path for scope, discovery, file
reads, the configured change, and Git history/revisions. The prompt supplies the canonical
reviewed root for exceptional absolute-path shell reads, but neither correctness nor change
delivery may depend on a repo-relative shell command succeeding from the sterile directory.
README capability text must stop advertising the shell as the normal way Codex obtains
`git log`, `git show`, a truncated file, or the branch diff. The shell remains available for
information outside the evidence vocabulary and retains the same OS-enforced no-write
posture; its reads remain unconfined, as documented today.

The server-owned MCP entry is then added at command-line precedence. Add `--strict-config`
so an older Codex CLI that does not accept the generated MCP configuration fails before a
billed review can masquerade as fully equipped. Set
`mcp_servers.<reserved-name>.required=true`, `.enabled=true`, an exact `.enabled_tools` list,
and `.default_tools_approval_mode="approve"`. Auto-approval is safe only because the setting
is scoped to this server-owned entry whose complete tool surface is read-only; it must never
be applied at top level or to a user/repository MCP server. Phase 0 tests that a second server
still follows its own approval policy.

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
  current status summary, service schema version, limits, and per-turn nonce. The preamble
  recommends it first, but review validity does not depend on the model choosing to call it.
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
- `repository_history(path?, before?, cursor?, limit?)` returns a normalized, bounded Git
  commit summary. A subsequent `repository_revision(id, path?, cursor?)` reads a validated
  Git revision through fixed provider-owned arguments. Reviewer input is data, never spliced
  into an option or shell command. Perforce returns `unsupported` for both tools in this
  implementation; its configured change remains available through `repository_change`.

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

Factor the repository's existing executable resolution, cancellation, output caps, and Git
environment/argv hardening into shared helpers rather than copying them. `repository_change`
pages the existing captured result. History and revision are **new** `git log` / `git show`
subprocess surfaces, so each gets its own fixed-argv implementation and tests for `--no-pager`,
`--no-ext-diff`, `--no-textconv`, disabled fsmonitor, PATH-only executable resolution, and
isolated Git config/pager environment; none of that hardening is assumed to arrive for free
from the diff runner. No provider invokes a shell.

Perforce list/search/read operate on the local workspace, and `repository_change` pages the
snapshot the parent already captured under the existing Perforce policy. Perforce history or
arbitrary revision reads would require new network calls, so this plan does not implement
them. They return `unsupported` regardless of whether the reviewer's sandbox could reach the
server. Adding them later requires a separate plan that names and secures each permitted p4
operation.

The configured change is captured once by the parent before the reviewer starts and stored
in the capability bundle with the same omission accounting used in the prompt today. The
service pages that immutable snapshot. Live file reads include a digest and stat metadata;
if the working tree changes after capture, `repository_scope` and subsequent reads surface
drift rather than presenting two revisions as one coherent snapshot.

This capture is unconditional for an isolated Codex reviewer, including `--diff auto`.
Phase 3 must change the existing `reviewer_has_shell && !supplies_change` decision: the shell
no longer counts as delivery of the selected change merely because it exists. The bundle's
`repository_change` snapshot is the supplied change for capability text and runtime omission
accounting. An isolated Codex review must never occupy the old state where no change is
captured because a repo-relative shell was assumed to provide it. Explicit `--diff off`
remains an intentional request for no selected change and is reported as such by
`repository_scope`/`repository_change`, not silently upgraded to `auto`.

### 4. Invocation and deterministic availability

Do not make a paid review's validity depend on a discretionary model tool call. Before
launching Codex, the parent starts the exact evidence-server executable and capability bundle
with a no-model MCP handshake, checks `initialize` plus `tools/list`, validates the schema and
tool allow-list, then closes it. This proves the internal mode and this turn's bundle work.
Codex independently receives the same bundle with
`mcp_servers.<reserved-name>.required=true`; the verified CLI contract must abort startup or
resume when that server cannot initialize. `--strict-config` makes a rejected config shape a
startup error as well. Together these are deterministic availability signals, independent of
what the model decides to inspect.

The preamble tells the reviewer to use evidence tools for repository discovery and to reserve
the shell for information the service explicitly does not provide. It removes the current
advice to avoid `git grep`/`git ls-files`; tool descriptions, not a release-specific list of
forbidden spellings, become the primary route.

If the parent handshake fails, the required server fails Codex startup/resume, or strict
configuration is rejected, return a dedicated `EVIDENCE_UNAVAILABLE` failure with
remediation. A completed review with no evidence-tool call is still a completed review; the
service was available and the model may reasonably not need it. Completed JSONL
`mcp_tool_call` events remain useful observability: if the model attempts an evidence call
and receives an infrastructure/service-death error, return `EVIDENCE_UNAVAILABLE` rather
than a possibly evidence-thinned verdict. Invalid model arguments are not infrastructure
failure and remain visible as ordinary tool errors. Continue collecting ordinary shell
policy denials independently.

Non-isolated `--allow-reviewer-config` reviews deliberately keep the reviewed repository as
Codex's working directory and still receive the server-owned evidence entry at command-line
precedence. Tests must show that a user entry with the reserved name cannot replace its
command, args, enabled state, or tool set. If Codex's merge semantics cannot guarantee that,
reject the name collision visibly rather than running a user-defined server under a trusted
tool name.

### 5. Limits, cancellation, and cleanup

Use the existing capture budget as the parent ceiling and add per-call limits below it:

- maximum returned bytes, matches, files, line length, and history records;
- maximum request count and cumulative returned bytes per turn;
- a short operation deadline checked during walks/searches;
- bounded MCP request and response sizes; and
- cancellation propagated from review cancellation and job termination.

All counters use checked arithmetic. Directory walks are iterative and deterministic. The
child logs diagnostics only to stderr. Phase 0 verifies whether Codex's MCP child inherits
the outer reviewer job. Independently, the evidence service puts every provider subprocess
in its own kill-on-close job, watches stdio EOF/parent death, and terminates its provider job
before exit. Cancel, timeout, shutdown, and parent-exit tests must observe both the service
and provider gone; if Codex can break away and EOF/parent-death cleanup is not reliable, the
architecture must change before release. Temporary capability/snapshot files are best-effort
deleted on every terminal path and are bounded/expired so a crash cannot grow state forever.

## Implementation sequence

### Phase 0: compatibility proof

The installed development CLI has already proved that explicit `-c mcp_servers...` entries
load after `--ignore-user-config` on both fresh and `exec resume` forms, and its actual JSONL
tool-call event shape is recorded above. Turn that manual probe into a tiny test fixture,
then complete the compatibility proof by capturing CLI behavior when a required server
fails, proving the exact per-server approval key leaves a second server untouched, checking
command-line precedence over a colliding user config, observing whether the MCP child can
break away from the reviewer job, and testing the lowest supported Codex CLI version. The
probe that exposed trusted-project loading invalidates the old isolation claim and motivates
the sterile root; verify the parent-side empty/outside/non-reparse checks and prove fresh and
resumed Codex turns work from that root with `--skip-git-repo-check`, leave it empty, and can
obtain the selected `--diff auto` change through `repository_change` without repo-relative
shell access. If any remaining premise fails, stop and amend this plan; do not silently fall
back to a trust-key spelling guess, repo-relative shell assumption, or exec-policy rules.

### Phase 1: evidence core

Implement typed scope/list/search/read/change APIs plus Git-only history/revision, canonical
path confinement, pagination cursors, caps, drift metadata, provider hardening, and direct
unit tests. Perforce history/revision return `unsupported`. Keep serialization and MCP
concerns out of this layer.

### Phase 2: internal MCP server

Add the hidden entry mode, closed schemas, dispatcher, typed error rendering, nonce-bound
capability loading, protocol tests, stdout-purity tests, and malformed/fuzz-style request
tests using only the existing `serde` dependencies.

### Phase 3: Codex integration

Generate strict per-server MCP overrides for fresh/resume invocations, add the no-model
handshake, create and clean turn bundles, parse evidence events for observability, classify
required-server startup failures, make evidence-bundle capture supersede the shelled
`--diff auto` shortcut, update capabilities/preamble for evidence-first repository access,
and expose service
readiness/schema in `cross_model_review_status`.

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
| Isolation | User config and rules are skipped, and isolated turns run from a verified empty cross-review-owned root outside the reviewed repository, so its `.codex/` layer is not a candidate for loading; a colliding config entry cannot replace the command-line evidence server; the evidence server exposes no review, shell, write, or network tool. |
| Availability | Parent handshake and required Codex startup succeed with the correct bundle; missing, replayed, truncated, malformed, collision-replaced, wrong-schema, or dead services fail before a review verdict. A model that simply makes no tool call does not fail. |
| Paths | Absolute, parent, UNC, device, ADS, case-folding, symlink, junction, deleted/replaced, and root-edge cases cannot escape the canonical root. |
| Search/list/read | Deterministic pagination, literal matching, UTF-8/binary handling, long lines, large files, empty repos, subdirectory roots, ignored/untracked files, and cap/timeout accounting. |
| VCS | Git worktree, staged, HEAD and range changes plus the new fixed `log`/`show` runners; hostile pager/ext-diff/textconv/fsmonitor/config; submodules; Perforce local evidence and captured offline/networked/restricted changelists; Perforce history/revision explicitly unsupported. |
| Lifecycle | Cancel, timeout, parent crash, MCP EOF, provider hang, output cap, and handler-thread failure reap the complete process tree and clean bounded temporaries. |
| Regression | Existing review capture, findings envelope, fallback chain, usage gate, policy-denial reporting, and Claude reviewer behavior remain unchanged. |
| Security | Model attempts to write inside/outside the repo and reach network still fail; evidence tools reject every write-shaped or option-shaped input; an evidence read succeeds. |
| Acceptance | During a real Codex review, file listing and repository search complete through evidence tools with zero `blocked by policy` denials; the reviewer can cite their results and return a structured verdict. |

## Rollout and compatibility

Make the evidence service mandatory for isolated Codex reviews once Phase 0 establishes the
supported CLI floor (initially the verified `codex-cli 0.144.5`). Older/incompatible CLIs
receive `EVIDENCE_UNAVAILABLE` with an upgrade remediation; they do not fall back to an
evidence-thinned approval. Claude reviews are
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
3. Fresh-turn Codex integration and deterministic availability checks.
4. Resume, collision, cancellation, and failure-contract coverage.
5. Documentation, full build, real Codex smoke, and cross-model review of the exact final
   implementation diff.

Each checkpoint should be independently reviewable. Do not batch a weakened boundary with a
later promise to harden it: any checkpoint that launches the service must already be
path-confined, bounded, isolated, and fail-closed.
