# Converged-review hook — plan

Status: **draft plan**, on `plan/converged-hook`, for issue #142. Not yet reviewed.

## What the issue asks, and what this plan does instead

Issue #142 asks the server to post a GitHub commit status (`cross-review: success`) on the
reviewed commit when a review converges. Then a downstream repository can make "a converged
cross-model review of this exact code" a required status check in a branch ruleset, and use
GitHub auto-merge behind it. The goal is to move the merge gate from a convention to something
GitHub enforces. Today a skipped review, or a stale one (a push after the review converged),
merges as easily as a reviewed one.

The issue proposes that the server talk to GitHub itself: a GitHub App, an RS256-signed JWT,
an HTTPS client, and the App's private key read from Windows Credential Manager. **This plan
does not do that.** Instead the server runs a **repository-configured hook command** when a git
review converges on exactly a committed change. The server passes the facts to the hook as
JSON on stdin, and the repository does whatever it wants with them. For the requester that
means posting the status with its own App credentials.

Why the hook instead of a built-in GitHub client:

- **Dependencies and attack surface.** `serde` is the only dependency, and the small
  self-contained binary is a feature (AGENTS.md). With no new crate, a built-in client means
  hand-written WinHTTP, CNG RSA signing, PEM parsing and Credential Manager FFI: a few hundred
  lines of crypto and network code in exactly the area where AGENTS.md says rigor belongs. The
  hook moves all of that into a script the repository owns. That script can use `gh`, an App
  token helper, or anything else.
- **Generality.** The status context name, the target URL, GitHub vs. another forge, and
  whether to post at all are all the repository's choices.
- **The issue's core argument still holds.** It wants the server, not the calling agent, to
  trigger the attestation, "so no agent step can be skipped or run against the wrong commit."
  The server spawns the hook itself, and decides **which commit** to name. The agent does not
  supply the SHA.

The part that must stay in the server is the **binding**: the hook fires only when the change
the reviewer approved is byte-for-byte the committed change of a specific commit. A naive hook
then cannot post `success` for code the reviewer never saw. That gate is the security-relevant
core of this feature, and the plan spends its rigor there (see "The binding").

## Scope

In:

1. An opt-in hook, configured on the server command line. Off by default; nothing changes for
   existing users.
2. A server-side binding check: the converged turn's served canonical diff must equal the
   committed change `base..HEAD`.
3. A JSON payload on the hook's stdin, in the shape the requester asked for.
4. The hook's result reported in the review result (a new `hook` field and a text-body line).

Out (each is a deliberate cut, per AGENTS.md "How much rigor, and where"):

- **Any GitHub API, App, JWT or credential handling.** That belongs to the hook script.
- **Keychain / libsecret.** The project is Windows-only.
- **`failure` / `pending` statuses for other outcomes** (issue item 4). The hook fires only on
  `converged`. A required check that has not been posted already blocks the merge, so posting
  `failure` mid-iteration adds noise without adding enforcement.
- **Carry-forward across a clean sync** (issue item 5). After a merge from the default branch,
  the fork point moves and the diff text shifts even when the branch's own change is unchanged.
  Re-attesting that needs a second, weaker binding (`git patch-id --stable` over
  `merge-base..HEAD`) and a new no-model tool call. That is a follow-up issue, and arguably
  something the hook-owning repository can do itself, since the payload carries `base` and
  `captured_head`. Until then, the workaround after a sync is to re-review on the same
  session. An unchanged change re-converges in one turn, at the cost of one model call.
- **Perforce.** Commit statuses are a git-forge concept. A Perforce review with a hook
  configured reports `hook: skipped (not_git)`.
- **Consults.** A consult has no outcome and never fires the hook.

## Configuration

Three new server flags, parsed in `src/config.rs` alongside the existing ones:

```
--on-converged <program>              The hook program. Absent = feature off.
--on-converged-arg <arg>              One argument to the hook. Repeatable, in order.
--on-converged-timeout-seconds <n>    Kill the hook's process tree after n seconds. Default 60.
```

Example (`.mcp.json`):

```json
"args": [
  "--reviewer", "codex", "...",
  "--on-converged", "pwsh",
  "--on-converged-arg", "-NoProfile",
  "--on-converged-arg", "-File",
  "--on-converged-arg", "scripts\\post-review-status.ps1"
]
```

- **Program and arguments are separate strings**, never one command line that we split. This
  follows the same reasoning as `allowed_tools`: each argument reaches the child as its own
  argument, and a path with spaces cannot be mis-split.
- **Program resolution must not reintroduce the bare-name execution hazard** documented on
  `reviewer::on_path`, where Windows resolves an unqualified name through the application
  directory first:
  - A bare name (no path separator) is resolved with `reviewer::on_path`.
  - A relative path with a separator is joined to the working root (`Config::cwd`).
  - An absolute path is used as is.
  - Resolution happens at startup. An unresolvable program is a startup error, like any other
    bad flag: a gate that silently never fires is worse than a server that will not start. A
    program that disappears after startup is reported per review as `hook: failed`.
- `--on-converged-arg` or `--on-converged-timeout-seconds` without `--on-converged` is a
  startup error, so a half-configured hook is not silently inert.
- `cross_model_review_status` reports whether a hook is configured and its resolved program
  path. This is free and pre-billing, so the caller can see the gate is wired up before
  spending a review on it.

**What trust boundary moves: none for the reviewer.** The hook runs as the server's user, with
the server's environment, and with no sandbox, because it is the repository owner's code doing
the repository owner's work. That is the same trust as the MCP entry's own `command` field.
Anyone who can add `--on-converged` to the config can already change `command` to run
anything. The reviewer's isolation, read-only posture and evidence service are untouched. The
reviewer cannot influence whether the hook runs, which commit it names, or (see below) any byte
of its payload.

## The binding

### The property

The hook fires for commit `H` only when this holds:

> On the converged turn, the canonical diff the reviewer was served (the whole
> `branch-base..worktree` diff the approval floor already requires, identified by its content
> `digest` in the serve record) is **byte-identical** to the diff `base..H` composed by the same
> code, with the same header and base, where `base` is the served base commit and `H` is `HEAD`
> at hook time.

When it holds, the reviewer was shown exactly `H`'s committed change against its fork point,
and attesting `H` is true. Both sides of the comparison are immutable: the served bytes are
recorded by digest, and `base` and `H` are commits. So the comparison has **no time-of-check /
time-of-use window**. Whatever happens to the working tree before, during or after the check
does not change whether `H`'s change is what was reviewed.

### Why this, and not "HEAD unchanged and the tree was clean"

The issue proposes recording `HEAD` and a clean-tree flag at capture time, then requiring
`HEAD` to be unchanged at post time. That is weaker and racier than it looks:

- A git review is live. The evidence server re-reads the working tree on each
  `repository_diff`, so "the capture point" is a series of reads, not one instant.
- A clean check and a diff are separate git calls. An edit that lands between them (dirty
  during the diff, reverted before the clean check, or the reverse) attests a commit whose
  content differs from what was served. Closing that with before-and-after checks still leaves
  a window.
- Comparing the served bytes against the commit's bytes needs none of that machinery. It also
  subsumes "tree was clean": uncommitted tracked edits or any untracked file (the canonical
  diff composes untracked files in) make the texts differ, and the hook is skipped.

It also removes the need to touch the evidence service. The serve record already carries
everything the parent needs per canonical operation: `digest`, `base`, `base_source`. The check
runs entirely in the parent after the turn is finalized. **The evidence service, which
AGENTS.md treats as protocol, does not change.**

### What the check does

In the parent, after a turn finalizes with `outcome == converged`, for a git review:

1. Read the turn's serve-record aggregate (`read_serve_record_aggregate`, already used by the
   floor). The floor guarantees every canonical op on a converged turn shares one digest, and
   at least one was complete and paged to its end. Take that digest `D`, plus that op's
   `base` and `base_source`. If any is missing, which a converged git turn should never
   produce, skip with `no_canonical_diff`. Fail closed.
2. Resolve `HEAD` to a full object id `H` (the existing `git::resolve_commit`).
3. Compose the `base..H` diff text with the **same composition code** the evidence server used
   for the served text: same header line (`# repository_diff base {base_source} = {base}`),
   same `git::diff` options, same `effective_autocrlf` override. Hash it with the same
   `digest::Fingerprint`. This is a refactor of `compose_diff` in `src/evidence/core.rs` so the
   header and tracked-diff part can be called from the parent with an explicit base commit
   and head commit, instead of a parallel reimplementation that could drift from it.
4. If the digests are equal, fire the hook for `H`. Otherwise skip with
   `head_does_not_match_review`, and say what to do in the reason: commit everything,
   including untracked files, then re-review on the same session.

The git calls use the same isolated runner as the evidence server (`GIT_CONFIG_NOSYSTEM`,
`core.hooksPath=NUL`, and so on), bounded by the evidence `Limits`. A git failure, such as a
diff over the byte cap, skips the hook with `binding_check_failed: <reason>`. It never fails
the review.

**Fail-closed by construction.** Any divergence between the two compositions, whether a real
difference or an unforeseen formatting difference, makes the digests differ and the hook skip.
The only way to fire falsely is two different changes yielding byte-identical diff text,
`index` blob ids included. The residual is an abbreviated-blob-id collision, accepted.

**Assumed, to be verified in implementation:** that for a clean checkout `git diff <base>`
(worktree) and `git diff <base> <H>` produce byte-identical output, including the `index`
lines and including a checkout where the #135 `core.autocrlf` override applies. If this does
not hold in some case, the effect is a hook that skips when it should fire. That is safe, but
it would make the feature useless in that case, so the tests below pin it.

## When the hook runs

At the end of the review job, in the `Ok(o)` arm of the attempt loop in `src/tools.rs`, right
after the `captured:` summary is built from the serve record. That point is:

- **After the turn is durably recorded.** `converged` excludes `TurnNotDurable`, which is
  `rebaseline`.
- **Before the result is published.** `cross_model_review_result` returns the hook's outcome
  with the review, and a caller never has to poll for it.

Conditions, all required; otherwise the `hook` field reports `skipped` with the reason in
parentheses:

| condition | skip reason |
| --- | --- |
| a hook is configured | (field is `null`, not `skipped`) |
| the review was not cancelled | `cancelled` |
| the run is a review, not a consult | (consults carry no `hook` field) |
| `vcs == git` | `not_git` |
| `outcome == converged` | `not_converged` |
| binding check passed | `no_canonical_diff`, `head_does_not_match_review`, `binding_check_failed` |

### Running the hook

- Spawned with `winjob::JobObject::spawn_in_job`, so the whole process tree is reaped on
  timeout and on server exit (`KILL_ON_JOB_CLOSE`). That matters here because `pwsh`, `gh`
  and `git` all spawn helpers.
- **stdin**: the JSON payload, then closed. **stdout and stderr**: piped and captured, never
  inherited. The server's stdout is MCP protocol traffic, and an inherited handle would let
  any `Write-Output` in a hook script corrupt it. This is the one mistake in this feature
  that would break the server outright, so a test pins it.
- Working directory: the working root.
- Environment: the server's, unchanged. The payload is on stdin, so no new environment
  variables.
- Timeout: `--on-converged-timeout-seconds`. On expiry, terminate the job and report
  `timed_out`.
- Exit code 0 is `succeeded`; anything else is `failed`, with the code.
- Output: the last 2 KiB of combined stdout and stderr is kept for the result, passed through
  `strip_marker_lines` like every other string in the result context. The full output also
  goes to the server's stderr log.

**The hook never changes the review.** `outcome`, `converged`, the ledger and resumability are
untouched by any hook result. A hook failure adds one run warning, so it is visible in the
text body as well as the `hook` field. It is never a failure code. The review happened; the
attestation did not.

Cancellation: the hook is not separately cancellable. Its timeout bounds it, and
`cross_model_review_cancel` arriving while it runs leaves it to finish or time out, which is
simpler than threading cancel into a child that is already posting. Two reviews converging
together run two hooks concurrently. That is the hook author's concern, and the payload
identifies each.

## The payload

JSON on stdin, one object, UTF-8, no trailing data. It uses the requester's suggested shape,
plus the fields a status poster needs to be precise:

```json
{
  "schema_version": 1,
  "event": "converged",
  "outcome": "converged",
  "session": "feat/horizon-fill",
  "turn": 3,
  "review_id": "rv-...",
  "reviewer": "codex gpt-5.6-luna (effort=xhigh)",
  "captured_head": "<full HEAD object id>",
  "tree_clean": true,
  "base": "<full merge-base object id>",
  "base_source": "merge-base(HEAD, refs/remotes/origin/HEAD)",
  "diff_sha256": "<served canonical diff digest>",
  "repo_root": "C:\\dev\\some-repo",
  "server_version": "0.16.0"
}
```

- `outcome` is always `converged` and `tree_clean` always `true` today, because the hook only
  fires then. They are kept because the requester asked for them and because they make the
  payload self-describing if a later version adds events. `tree_clean` is documented
  precisely: **the reviewed change was exactly `captured_head`'s committed change**, which is
  what the binding check proves. It is stronger than "the working tree was clean at some
  instant".
- `reviewer` is the same string as the result's `reviewer` field: the chain entry that actually
  ran, including a fallback.
- **The payload carries no reviewer-authored content.** It has no findings, no prose and no
  titles. Every field is server-derived (git object ids, digests, configuration) or
  caller-supplied (`session`). A hook script therefore cannot be steered by anything the
  reviewer wrote. `session` is caller-supplied text, so the docs tell hook authors to treat it
  as data, not interpolate it into a shell command.
- The payload's own `schema_version` is independent of the envelope's, so the hook contract can
  evolve on its own.

## The result

A new `hook` field in the completed result's context group (`ResultContext`,
`Envelope::core_value`), on both channels:

```json
"hook": null
"hook": {"status": "succeeded", "reason": null, "captured_head": "<sha>", "exit_code": 0, "output": "..."}
"hook": {"status": "skipped",   "reason": "head_does_not_match_review", "captured_head": null, "exit_code": null, "output": null}
```

`status` is one of `succeeded`, `failed`, `timed_out` or `skipped`. The field is `null`
exactly when no hook is configured, so its presence says the gate is wired up. The text body
gets one line, for example `hook:      succeeded (head 1a2b3c4d)` or
`hook:      skipped (head_does_not_match_review: commit everything, then re-review)`.

The schema change follows the issue #63 and #73 precedent: `hook` joins the declared output
schema and its `required` list, and `ENVELOPE_SCHEMA_VERSION` goes 3 → 4. The ledger schema
does not change, so no session or ledger is invalidated by the upgrade.

## Docs

- README "Configuration": the three flags, the example, and a short "Gating merges on a
  converged review" section. It covers the binding (commit before the final review turn), the
  payload, and a minimal example script that posts a status with `gh api`. It notes honestly
  that `gh` uses a personal token, so a ruleset that pins the required check to a GitHub App
  needs the script to mint an App installation token instead. The README also carries the
  issue's trust limit: this catches skipped and stale reviews, not a determined actor running
  as the same OS user, who can run the hook script by hand.
- AGENTS.md: no change. This repository does not gate itself on GitHub statuses.

## Tests

All unit tests, no network and no model calls (the issue's "fake status sink"):

- **Binding check**, on temporary git repositories:
  - A clean committed change: the served-style worktree composition and the `base..H`
    composition have equal digests, so the hook fires. This test pins the byte-identity
    assumption.
  - Same, with a non-ASCII file and with `core.autocrlf=true` applied through the #135
    override. This also pins the assumption.
  - An uncommitted tracked edit: digests differ, so `head_does_not_match_review`.
  - An untracked file: digests differ, same reason.
  - `HEAD` moved to a different commit after the serve record: digests differ, same reason.
  - `HEAD` moved to a different commit with the **same** change (amended message): fires for
    the new `H`. This is correct: the reviewed change is that commit's change.
  - A serve record with no canonical digest: `no_canonical_diff`.
- **Gating**, through a hook-runner seam (a trait or closure the job calls, replaced by a
  recording fake in tests): the hook is invoked exactly when vcs is git, the outcome is
  converged, the review is not cancelled and the binding passes; every other combination
  reports the right skip reason and never invokes the runner. A hook result never changes
  `outcome`, `converged` or resumability.
- **Runner**, spawning real `cmd.exe` stand-ins:
  - The payload arrives on stdin, intact.
  - Hook stdout does not reach the server's stdout: the child writes to stdout, and the test
    asserts it lands in the captured tail.
  - A non-zero exit is `failed` with the code.
  - Timeout is `timed_out`, and the grandchild is gone (job reaping).
  - The output tail is bounded and marker-swept.
- **Config**: flag parsing, the argument order is preserved, the half-configured startup
  errors, and the program resolution rules (bare name through `on_path`, relative path joined
  to the working root).
- **Result**: `hook` on both channels, `null` when unconfigured, and the schema's `required`
  list and version bump.

`smoke.ps1`: the evidence service and reviewer spawning are untouched, so this does not
strictly require the round trip. It is still worth one `-Reviewer codex` run with a hook
configured that writes its stdin to a file, as an end-to-end check that a real converged turn
fires it. That costs tokens, and is mentioned to the user before it runs.

## Open questions

1. **Hook on other outcomes.** This plan fires only on `converged`. The requester's payload
   includes `outcome`, which may mean they want every turn. Firing on every turn, with the
   binding fact passed as data, would let a naive hook post `success` on `HEAD` whenever
   `outcome == converged` without checking `tree_clean`, which moves the safety rule into
   every hook script. The plan keeps the rule in the server. Revisit only on a concrete need.
2. **A no-model `attest` tool** that re-runs the binding check against the latest converged
   turn and fires the hook. This covers "converged on uncommitted work, then committed exactly
   that". It is the natural home for carry-forward later. Deferred: committing before the final
   turn costs nothing today.
