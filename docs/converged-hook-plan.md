# Converged-review hook — plan

Status: **draft plan**, on `plan/converged-hook`, for issue #142. Cross-review session
`plan-converged-hook` (Codex, gpt-5.6-luna, effort=xhigh). This is r4: **converged at round 4**
(7 findings raised, all accepted and resolved; none disputed).

**Revision note (r1 → r2).** Round 1 raised four findings. All four were checked against the code
and accepted:

- **f1** (major): r1 said the serve-record aggregate supplies the digest and base commit. It does
  not. `CanonicalServe` keeps only `base_source` and the counts, and `aggregate_serve_records`
  uses the digest for agreement and then drops it. The binding check now says plainly that the
  aggregate is **extended** to retain the agreed `digest`, the full `base` token and
  `base_source`. A canonical op missing any of them fails closed.
- **f2** (major): r1 recomputed `effective_autocrlf` at hook time. The evidence bundle fixes it
  once per turn, so a config or attributes change mid-review could make the two compositions use
  different options. That only ever causes a false *skip*: different options give different
  bytes, and different bytes never fire. Still, it is a hole in the "same code, same options"
  claim, and closing it is cheap. The parent built the bundle, so it already holds the value, and
  the check now reuses that exact `Option<AutoCrlf>` instead of recomputing it.
- **f3** (major): r1 presented job-object reaping as guaranteed. `JobObject::new()` can return
  `None`, and the reviewer runner proceeds uncontained when it does. The hook runner now **fails
  closed**: no job, or a failed `spawn_in_job` (which is already fail-closed: it kills the
  suspended child rather than resume it uncontained), means the hook is reported `failed` and
  never launched. Timeout terminates the job, then does a bounded quiescence check.
- **f4** (minor): r1 did not say piped output is drained concurrently. It now reuses the reviewer
  runner's pattern: stdin written on its own thread, and both streams drained on their own
  threads with a bounded tail. A large-output test is added.

**Revision note (r2 → r3).** Round 2 resolved f1–f4 and raised two more, both accepted. The
requester's feedback on the draft also lands here.

- **f5** (major): r2 quiesced the job only on timeout. A hook whose direct process exits 0 while
  a helper is still posting would have been reported `succeeded` before the post finished. Now
  **every** exit, not just a timeout, is followed by a bounded wait for the job to quiesce.
  `succeeded` requires exit 0 **and** an empty job. Otherwise the job is terminated and the
  hook reported `failed` (`not_quiesced`).
- **f6** (minor): r2 promised `hook: skipped (cancelled)`. A cancelled or failed review returns
  an error, not a completed result, so it has no result context to carry a `hook` field. The row
  is removed: a cancelled review never runs the hook and has no `hook` field, like any failed
  review.
- **Requester: committed is not enough, it must be pushed.** GitHub rejects a status on a
  commit it does not have (HTTP 422). A review that converges on a committed-but-unpushed
  change would report `hook: failed`, and the only recovery would have been another model
  call. So the no-model **`cross_model_review_attest`** tool moves from open question to scope
  (see "Re-attesting without a model call"). It also covers "converged on uncommitted work,
  then committed exactly that". The README tells authors to push before the final turn.
- **Requester: fallback reviewers.** When the primary reviewer is rate-limited the chain can
  fall back to an entry from the *calling* agent's own model family. Parsing the `reviewer`
  string to detect that is fragile. The payload now carries `chain_index`, `reviewer_kind`,
  `model` and `effort` as separate fields, so a hook can refuse a same-family review. The server
  does not decide this: it does not know the caller's family.
- **Requester confirmations**, recorded as data, not as resolved review findings: firing only
  on `converged` with the binding in the server is agreed; deferring carry-forward is fine for
  them (their ruleset does not require up-to-date branches); and on their repository the
  isolated worktree diff of a committed, non-LFS binary (`.uasset`) matched the
  commit-to-commit diff exactly. That last point corroborates the byte-identity assumption on
  one real repository. It does not replace the tests that pin it.

**Revision note (r3 → r4).** Round 3 resolved f5 and f6, and confirmed three things: the
configured-index `chain_index` is correct, the binding stays fail-closed through the attest
tool, and the attest tool is proportionate to the 422 case. It raised **f7** (major), accepted:
the attest tool's latest-turn check used the process-local registry and was not atomic with the
hook. A later turn in another server process, or one that started mid-attest, could be missed.
The fix reuses existing machinery and adds none. Attest takes the **same per-session lease** a
review takes, checks the **durable** `SessionRecord`'s `cli_session_id` and `turns` under it,
and holds the lease through the hook.

## Implementation status

Implemented on `feat/converged-hook`: `src/hook.rs` (payload, binding check, contained runner,
turn-end gating); `committed_change_digest` / `resolve_head` in `src/evidence/core.rs`, built on a
`compose_diff` → `compose_resolved` split, so the served text and the committed text come from
the same code; the flags and their resolution in `src/config.rs`; `turn_end_hook` /
`attestable_for` and `App::attest` in `src/tools.rs`; the `hook` result field (envelope v4) in
`src/findings.rs`; and the conditional tool in `src/mcp.rs`.

**The byte-identity assumption is verified**, not just assumed. On a clean checkout, the worktree
composition and the `base..H` composition produce equal digests for a modification, an
addition, a deletion, a binary file and non-ASCII content
(`a_clean_checkout_serves_exactly_the_committed_change`). They also agree on a CRLF checkout
under its own `core.autocrlf` (`a_crlf_checkout_matches_only_under_its_own_autocrlf`).

Where the implementation differs from the plan text, and why:

- **A cancel that lands after the turn completed** reports `hook: skipped (cancelled)`. f6
  removed `cancelled` only for reviews that end as a *failure*, which have no result to carry
  the field. A completed turn whose cancel flag was set by then still has a result, and not
  firing is the honest reading of a cancel.
- **`core.autocrlf` has no observable effect on the committed side.** A commit-to-commit diff
  never touches the working tree, so `autocrlf` cannot change it. The setting only shapes the
  *served* worktree diff, and that effect is already captured in the served digest. The value is
  still passed through, as f2 asked: it is harmless and keeps "same code, same options" literally
  true. The planned test that attest "uses the retained value, not a fresh lookup" was not written
  because nothing observable separates the two.
- **Two more skip reasons**, both fail-closed: `session_not_recorded`, for a converged turn
  whose durable record cannot be read back under the lease (which should not happen, since
  `converged` implies durable), and `review_not_available` for attest, covering an evicted,
  unknown or pre-restart review.
- **Hook output**: the retained tail of each stream is logged to stderr, not the whole stream,
  so the bounded-memory drain stays bounded.
- **`chain_index`**: `turn_end_hook` is tested with a fallback entry (index 1), and the payload
  says `1` / `claude`. The walk's `ran_index = Some(i)` assignment is one line and is not
  exercised through a full walk, which would need a real reviewer. `smoke.ps1` covers the
  primary path end to end.

Found in passing, filed separately: git reviews always report `captured: null`, because the
serve record is deleted before the `captured:` line is built from it (issue #143). This feature
avoids that by reading its binding inputs inside `attempt`, while the record still exists.

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
5. A no-model `cross_model_review_attest` tool that re-runs the binding check for a session's
   latest converged turn against the current `HEAD` and fires the hook. It is for the
   push-after-convergence and commit-after-convergence cases.

Out (each is a deliberate cut, per AGENTS.md "How much rigor, and where"):

- **Any GitHub API, App, JWT or credential handling.** That belongs to the hook script.
- **Keychain / libsecret.** The project is Windows-only.
- **`failure` / `pending` statuses for other outcomes** (issue item 4). The hook fires only on
  `converged`. A required check that has not been posted already blocks the merge, so posting
  `failure` mid-iteration adds noise without adding enforcement.
- **Carry-forward across a clean sync** (issue item 5). After a merge from the default branch,
  the fork point moves and the diff text shifts even when the branch's own change is unchanged.
  Re-attesting that needs a second, weaker binding (`git patch-id --stable` over
  `merge-base..HEAD`) layered onto `cross_model_review_attest`. That is a follow-up issue, and arguably
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
   at least one was complete and paged to its end. **Today the aggregate does not keep what
   the check needs** (f1): `CanonicalServe` (`src/tools.rs`) holds only `base_source` and the
   counts, and the digest is used for agreement and then dropped. Extend it to retain the
   agreed `digest`, the `base` token (the full merge-base object id the evidence server wrote,
   `src/evidence/core.rs` `first_diff_page`) and `base_source`, taken from the
   complete+terminal canonical op. The `base` must also be a valid full object id. If any of
   them is missing or malformed, skip with `no_canonical_diff`. Fail closed.
2. Resolve `HEAD` to a full object id `H` (the existing `git::resolve_commit`).
3. Compose the `base..H` diff text with the **same composition code** the evidence server used
   for the served text: same header line (`# repository_diff base {base_source} = {base}`),
   same `git::diff` options, and the **same `core.autocrlf` value the turn's evidence bundle
   was built with** (f2). The parent creates the bundle (`Bundle::create` in the
   `evidence_setup` block of `src/tools.rs`), so it keeps that `Option<AutoCrlf>` and passes it
   through, rather than calling `effective_autocrlf` again at hook time. Hash it with the same
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
| the review completed (not cancelled, not failed) | (no completed result, so no `hook` field; f6) |
| the run is a review, not a consult | (consults carry no `hook` field) |
| `vcs == git` | `not_git` |
| `outcome == converged` | `not_converged` |
| binding check passed | `no_canonical_diff`, `head_does_not_match_review`, `binding_check_failed` |

### Running the hook

- **Containment is required, not best-effort** (f3). The hook needs a `JobObject::new()`
  (`KILL_ON_JOB_CLOSE`) and is started with `JobObject::spawn_in_job`. That creates the child
  suspended, assigns it, and only then resumes it; if assignment fails, it kills the suspended
  child rather than resume it uncontained. If either step fails, the hook is reported `failed`
  (`no_containment`) and **never runs**. This deliberately differs from the reviewer runner,
  which warns and proceeds uncontained. A hook that escapes its timeout could post a stale
  status long after the result said `timed_out`, and `pwsh`, `gh` and `git` all spawn helpers.
  (`spawn_in_job` and `active_processes` are currently `#[allow(dead_code)]`, waiting on the
  login runner. This feature becomes their first caller.)
- **stdin**: the JSON payload, written on its own thread, then closed. **stdout and stderr**:
  piped and captured, never inherited. The server's stdout is MCP protocol traffic, and an
  inherited handle would let any `Write-Output` in a hook script corrupt it. This is the one
  mistake in this feature that would break the server outright, so a test pins it.
- **Both streams are drained concurrently** (f4), on their own threads, with a bounded
  retained tail, following `reviewer::run` and its `drain` helper. Draining after `wait` would
  deadlock as soon as a chatty hook fills a pipe buffer. After the child exits, a short drain
  grace lets a straggler that still holds a pipe finish, and then the readers are abandoned.
  They own nothing the result needs.
- Working directory: the working root.
- Environment: the server's, unchanged. The payload is on stdin, so no new environment
  variables.
- **Quiescence on every exit** (f5). When the direct child exits, the runner waits, bounded
  by what remains of the timeout, for `active_processes` to reach zero. A helper the hook
  started (a backgrounded `gh api`, say) is still part of the hook's work, and the result must
  not report success before it finishes. If the job does not empty in the bound, terminate it.
- Timeout: `--on-converged-timeout-seconds` covers the whole hook, direct child and
  descendants. On expiry, terminate the job, then wait briefly (bounded) for it to empty.
- Status:
  - `succeeded`: exit code 0 **and** the job emptied on its own.
  - `failed`: a non-zero exit (with the code); exit 0 with descendants still running at the
    deadline (reason `not_quiesced`, the job then terminated); or no containment
    (`no_containment`).
  - `timed_out`: the timeout fired. If the job still did not empty after termination, the
    reason is `not_quiesced`, so the caller knows a straggler may still act.
  - `KILL_ON_JOB_CLOSE` remains the backstop on server exit.
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
  "trigger": "review",
  "reviewer": "codex gpt-5.6-luna (effort=xhigh)",
  "reviewer_kind": "codex",
  "model": "gpt-5.6-luna",
  "effort": "xhigh",
  "chain_index": 0,
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
  ran, including a fallback. It is for humans. **Hooks should key on the separate fields**
  (requester feedback):
  - `reviewer_kind`: `codex` or `claude`.
  - `model` and `effort`: the full pinned id and the effort.
  - `chain_index`: the position in the configured `--reviewer` chain of the entry that ran.
    `0` is the primary. It is the configured index (`i` in the `walk` loop in `src/tools.rs`),
    not the walk position, so it is honest in two cases: when the proactive usage gate skipped
    the primary before the walk started, and when a resumed session is bound to an entry that
    was itself a fallback on turn 1. Either way a non-zero value means "not the primary".
  
  A repository whose rule is "the other model, never the caller's own family" refuses on
  `chain_index != 0`, or on `reviewer_kind`. The server cannot make that decision: it does not
  know the calling agent's family.
- `trigger` is `review` when the hook fires at the end of a review turn and `attest` when it
  fires from `cross_model_review_attest`.
- **The payload carries no reviewer-authored content.** It has no findings, no prose and no
  titles. Every field is server-derived (git object ids, digests, configuration) or
  caller-supplied (`session`). A hook script therefore cannot be steered by anything the
  reviewer wrote. `session` is caller-supplied text, so the docs tell hook authors to treat it
  as data, not interpolate it into a shell command.
- The payload's own `schema_version` is independent of the envelope's, so the hook contract can
  evolve on its own.

## Re-attesting without a model call

A review that converges on a commit the forge does not have yet (committed, not pushed) gets a
`hook: failed` from a hook that posts a status: GitHub answers 422. A review that converges on
uncommitted work gets `hook: skipped (head_does_not_match_review)`. In both cases the reviewed
change is fine, and paying for another model call to re-fire the hook is waste. The requester
hit the first case in practice.

`cross_model_review_attest` takes a `review_id` (or a `session`, meaning that session's most
recent review). It:

1. **Requires the review to be the session's latest turn, and converged, against durable
   state and under the session lease** (f7). This matters: if turn 3 converged on content X and
   turn 4 re-reviewed the same X and raised a finding, attesting via turn 3 would launder turn
   4's finding. The in-process registry cannot answer "is this the latest turn". It is
   process-local (`src/registry.rs`), while sessions are shared across server processes
   through `--state-dir`, so a later turn run by another process is invisible to it. Instead:
   1. Acquire the **existing per-session lease**, the same `ExclusiveLock` on
      `session::session_lock_path` that a review takes before it reads the session record
      (`src/tools.rs`), with the same `SESSION_LEASE_WAIT`. A failure to acquire it is the
      existing `SESSION_LEASED` error.
   2. Under the lease, read the durable `SessionRecord`. Require that its `cli_session_id` and
      `turns` equal the ones the review's turn recorded, both retained on the registry record
      with the binding inputs. Any mismatch is `not_latest_turn`. That covers a later turn from
      any process, a `fresh` rebind (new `cli_session_id`) and an expired or forgotten session.
      *(Added in implementation review, f1.)* Also require the session's **findings write-ahead
      marker** to be absent. A later turn that ran but was never durably recorded leaves the
      record unchanged, and the marker is the existing durable fact for that case: it is written
      before every reviewer turn and cleared only after a durable record. A set or unreadable
      marker is `not_latest_turn`, the same fail-closed rule the resume gate applies.
   3. **Hold the lease through the binding check and the hook**, releasing it only after the
      hook's quiescence. No turn can start or finish on the session while an attest is deciding
      and firing, in any process. A review that arrives meanwhile waits on the lease exactly as
      it waits on another review today, bounded by the hook's timeout plus the check.

   It also requires that the review completed, was a git review, and has
   `outcome == converged`. Otherwise it reports `not_converged` or `not_git`. These are read
   from the registry record, which is authoritative for its own turn's outcome. The durable
   check above is what establishes that the turn is still the latest.
2. **Re-runs the binding check** against the **current** `HEAD`, using the binding inputs
   retained from that turn: the agreed digest, `base`, `base_source` and the turn's
   `Option<AutoCrlf>`. Same function, same fail-closed rules. So "converged on uncommitted work,
   then committed exactly that" passes, and "committed something else" does not.
3. **Fires the hook** with `trigger: "attest"`, under the same runner and the same containment,
   draining, quiescence and timeout rules, and **returns the same `hook` report object** as the
   tool's result.

The binding inputs live on the completed review's in-memory registry record (`src/registry.rs`)
next to the rest of its result. They are **not persisted**: no session-record or ledger change,
and nothing to migrate. When the record is evicted (the per-session cap on finished reviews) or
the server restarts, the tool reports `review_not_available`, and the recovery is a re-review on
the same session, which is exactly today's cost. That is a deliberate trade-off: persisting the
inputs would let an attest outlive the server, but at the cost of a session-record change for a
case that is already cheap to recover.

What this tool does not do: it does not change the stored review's `hook` field. Collecting
the review again shows the original turn-end report, and the attest call's own result is the
record of the re-fire. It is not cancellable, and it makes no model call, so it is not billed.
It does not handle carry-forward: after a sync the base moves and the digests differ, by
design.

The tool is listed only when a hook is configured. Called without one, it returns the
standard invalid-arguments error naming `--on-converged`.

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
  converged review" section. It covers the binding: **commit and push before the final review
  turn**, or push afterwards and call `cross_model_review_attest`. It also covers the payload
  (including keying a same-family refusal on `chain_index` / `reviewer_kind`), and a minimal
  example script that posts a status with `gh api`. It notes honestly
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
  - A serve record with no canonical digest, no `base`, or a malformed `base`:
    `no_canonical_diff`.
  - The aggregate retains the agreed digest, base and base_source from the complete+terminal
    canonical op (extends the existing `serve_record_aggregate_*` tests).
  - The check composes with the autocrlf value it is handed, not a fresh lookup. A test
    changes the repository's `core.autocrlf` between "serve" and check and asserts the
    turn's value is used.
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
  - Timeout is `timed_out`, and the grandchild is gone (job reaping, quiescence observed).
  - A hook writing several MiB to both stdout and stderr completes without deadlock. Only
    the bounded tail is retained, and it is marker-swept.
  - Containment failure (the job-creation seam forced to fail) reports `failed`
    (`no_containment`) and the program never starts, which a side-effect file proves.
  - A hook whose direct process exits 0 while a spawned child sleeps and then writes a marker
    file (f5): the runner waits for the child, and the marker exists before `succeeded` is
    reported. With the child outliving the bound, the result is `failed` (`not_quiesced`),
    and the child is gone afterwards.
- **Attest tool**, with the recording runner fake:
  - It fires on the latest converged turn after a matching commit, with `trigger: "attest"`.
  - It refuses `not_latest_turn` when the durable session record has moved on, whether a
    later `turns` value or a different `cli_session_id` from a `fresh` rebind. Each is written
    by a *separate* store instance, standing in for another server process, so the test
    proves the registry is not what decides.
  - It holds the session lease from validation through hook quiescence: a review started
    while the hook runs blocks on the lease until the attest finishes.
  - It reports `not_converged`, `not_git`, and `review_not_available` (an evicted or unknown
    id).
  - `head_does_not_match_review` after a non-matching commit.
  - It uses the retained `Option<AutoCrlf>`, not a fresh lookup.
  - It never changes the stored review's `hook` field.
- **Payload**: `chain_index` is the configured index. The tests cover the primary, a
  rate-limit fallback, a pre-start usage-gate skip, and a resume bound to a fallback entry.
  `reviewer_kind`, `model` and `effort` match the entry that ran.
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
   every hook script. The plan keeps the rule in the server, and the requester has agreed.
2. **Carry-forward** (issue item 5) would naturally extend `cross_model_review_attest` with a
   second binding (patch-id over `merge-base..HEAD`). It is still deferred; the requester
   confirmed they do not need it.
