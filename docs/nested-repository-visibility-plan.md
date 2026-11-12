# Nested repositories the reviewer cannot see — plan

Status: **plan** for issue #217. Cross-review session `plan-issue-217-nested-repos` (Codex,
gpt-6-luna, effort=xhigh): **converged at round 3**. Eight findings raised: f1 and f3–f8 accepted
and resolved; f2 resolved by a scope cut (the overclaim and partial checks removed, git-metadata
confinement for all roots deferred to its own issue), which the reviewer accepted.

**Revision note (r1 → r2).** Round 1 raised six findings, all accepted:

- **f1** (major): r1 let any calling agent declare a nested path, exposing that repository's git
  history without an operator decision, against the documented contract that a caller can only name
  a configured companion. Per-review paths are now behind a default-off startup opt-in,
  `--allow-nested-companions` (gap 3). One restart enables it for the server's lifetime, not one per
  repository.
- **f2** (major): r1's containment checks constrained the nested *directory* but not its git
  *metadata*. A `.git` file (linked worktree), a `commondir`, or `objects/info/alternates` can point
  git at an object store outside the working root. A per-review path now requires `.git` to be a
  real directory with no `commondir` and no alternates (gap 3).
- **f3** (major): r1 treated an unresolvable branch state and any detection failure as "no
  changes", which is silent in exactly the issue's case (committed on a branch). Both now produce a
  warning saying the state is unknown, and `@{upstream}` is tried before giving up (gap 1).
- **f4** (minor): r1's candidate cap bounded repositories found, not work done. Output lines,
  directories inspected and entries per directory are now capped, and hitting a cap is itself a
  detection-incomplete warning (gap 1).
- **f5** (minor): r1 said ignored files are unreachable by `repository_list`/`repository_search`.
  Wrong: `repository_list` uses `fs::read_dir` and lists ignored entries; `repository_search` searches
  an explicitly named file whether or not it is ignored. Only directory searches are git-scoped.
  Corrected below; the conclusion (no diff, so uncertifiable) stands.
- **f6** (minor): r1 resolved a bare `companions` entry as a configured name first and a path
  second, so the warning's `["engine"]` could select a *different* configured companion named
  `engine`. Paths are now syntactically distinct (`./engine`), and path companions get an `n-`
  name prefix (gap 3).

**Revision note (r2 → r3).** Round 2 resolved f1 and f3–f6, held f2 open, and raised f7 and f8:

- **f2** (held, answered by a **scope cut**): r2's gitfile/`commondir`/alternates checks were not a
  sandbox. The evidence git runner honours the repository's local `.git/config`
  (`src/evidence/git.rs:941-951` overrides only `core.fsmonitor`/`core.hooksPath` and disables
  system/global config), so `core.worktree`, or a junction *inside* `.git`, can still point git
  outside the nested directory. Closing that properly means overriding or validating every
  git-resolved path, and it is **equally true today of the primary working root and of every
  configured companion**, nested or sibling. Rather than build a git-metadata sandbox for one kind of
  root, r3 **removes the partial checks and the claim they supported**. A path companion gets
  exactly the trust a configured companion gets, and the boundary statement in gap 3 now says so.
  Planting such metadata needs write access to the working tree, which the reviewer does not have;
  the calling agent does, but it already reads everything the reviewer could. Git-metadata
  confinement for all roots at once is a separate, pre-existing hardening issue, to be filed on its
  own.
- **f7** (minor): the *unknown* warning blamed a missing base even when a git call had failed. It
  now carries the actual cause.
- **f8** (minor): detection skipped the children of a candidate that was itself a repository
  (`vendor/` a repo, `vendor/sdk/` another). The one-level descent now applies to every kept
  directory, candidate or not, within the same caps.

## The problem, in one paragraph

A git repository nested inside the working root and ignored by it (the #170 layout, e.g. an
`engine/` fork) contributes nothing to the primary's canonical diff. If it is not a declared
companion, its change is never served, so the reviewer cannot certify it. In the reported case the
change's substance was ~10 lines of C++ in `engine/`, the primary diff was 7 doc lines, and the only
way to get a verdict was to paste the code into `instructions`. That converged, and the converged
hook posted success on the primary head. Nothing told the caller, at any point, that part of the
change was unreachable.

## What the code does today (verified by reading, not by running)

- **The server does not know about unconfigured nested repositories.** The issue assumes it does
  ("the #170 checks"), but `companion::check_against_root` only inspects repositories already
  configured with `--companion`. Nothing enumerates nested toplevels. Gap 1 needs new detection,
  not reuse of existing state.
- **A nested repository's working-tree files are visible to the primary's evidence tools, its
  history is not** (r1 f5). `repository_read` refuses only the `.git`/`.hg`/`.svn`/`target`/`dist`
  components and reserved names (`validate_relative_path`, `src/evidence/core.rs:2174`), not a nested
  toplevel. `repository_list` enumerates with `fs::read_dir` (`core.rs:977-1006`), so it lists
  ignored entries, `engine/` included. `repository_search` searches an explicitly named file whether
  or not it is ignored (`core.rs:1802-1808`); only a *directory* search is scoped through `git
  ls-files`. So the turn-1 reviewer could have reached `engine/<file>`, but there is no diff for it,
  and the change stays uncertifiable.
- **Per-review companions already exist for configured names** (`parse_companions_arg`,
  `src/tools.rs:6843`), with session binding by name *and* root, a per-turn `recheck`
  (`src/companion.rs:254`) that re-validates a nested companion against the server root, the
  evidence floor, and hook binding. Gap 3 reuses all of it.

## Order and sizing

| Gap | Change | Size | Boundary moves? |
| --- | --- | --- | --- |
| 1. No signal | detect nested repos with unserved changes; warn | small/medium | no |
| 3. Restart to add | opt-in: `companions` accepts `./<nested path>` | medium | **yes, behind an operator flag** |
| 2. No remediation | warning text and reviewer prompt name the fix | small | no |
| 4. Pasted code converges | prompt rule only, no new detection | tiny | no |

Two PRs: 1+2+4, then 3, so the boundary move is reviewed on its own. Gap 1's remediation text
mentions the flag from day one; it only stops being hypothetical when 3 lands.

## 1. Detection and warning

**Enumeration.** At `start`, for a git review or consult, one call through the existing isolated
runner (`evidence::git::run`, same `Limits`):

```
git ls-files -z --others --directory --no-empty-directory   # untracked + ignored, dirs collapsed
```

Keep the entries ending in `/` (individual untracked files are skipped). For each kept directory:
if it contains `.git`, it is a candidate; **in either case** list its immediate subdirectories with
`read_dir` and keep any that contain `.git` (r2 f8). This catches `vendor/sdk` under an ignored
`vendor/`, and under a `vendor/` that is itself a nested repository.
Nested repositories under a *non-ignored* untracked directory are covered by the same rule, since
git collapses a nested repository to `engine/` whether or not it is ignored.

**Bounds** (r1 f4), every one of which, when hit, stops detection and adds one warning:
"nested-repository detection stopped early (`<which cap>`); a nested repository with changes may
not be listed":

- `ls-files` output: at most 4 096 entries read (the runner's byte limit already applies);
- directories inspected (top-level plus descent): at most 512;
- entries read per directory during descent: at most 256;
- candidates: at most 32.

Depth stays one level below a collapsed directory. A nested repository deeper than that is missed
without a warning; the README says so. That is a deliberate limit, not a scan to chase (#86 removed
the recursive walk for good reason).

**State of each candidate not declared on this review** (r1 f3):

- **changed** — `git status --porcelain` is non-empty, **or** `HEAD` differs from its fork point,
  where the fork point is `resolve_branch_base` (`src/evidence/core.rs:3170`, made `pub(crate)`) or,
  when no `refs/remotes/origin/{HEAD,main,master}` resolves, `merge-base(HEAD, @{upstream})`;
- **unknown** — status is clean and neither base resolves (a local-only branch, a detached HEAD
  with no upstream), or any git call for this candidate failed;
- **clean** — status is clean and `HEAD` equals the resolved fork point. No warning.

**Warnings**, one per non-clean candidate:

- *changed, ignored, not configured:* "`engine/` is a separate git repository inside the working
  root, with changes (2 commits ahead of its fork point; 1 modified file) that this review cannot
  see or certify. If the change under review includes it, configure it as a companion
  (`--companion engine=engine`), or, on a server started with `--allow-nested-companions`, re-review
  with `companions: ["./engine"]`."
- *changed, configured as companion `<name>` (same root) but not declared:* "...re-review with
  `companions: ["<name>"]`."
- *changed, not ignored by the working root:* "...is not ignored by the working root, so it cannot
  be a companion; add `engine/` to `.gitignore`, then declare it."
- *unknown:* "`engine/` is a separate git repository inside the working root whose changes could not
  be determined: `<cause>`. If the change under review includes it, declare it as above." The cause
  is the real one (r2 f7): "no default or upstream branch to compare against" when no base
  resolved, or the failing git command and its error otherwise.

Wording names the path and the remediation, never the configured-name lookup by basename: a
configured companion is matched to a candidate by **root**, not by name (r1 f6).

**Where it appears.**

- On the `cross_model_review` / `cross_model_consult` **start response** as `warnings`, so the
  caller can cancel and re-declare before waiting a review out.
- On **every turn's result** `warnings` (via `capture_warnings`), recomputed each turn, so a gate
  reading only the result sees it and a re-review after declaring the companion drops it.
- Not a refusal and not a non-convergence reason (see gap 4).

**Cost.** One `ls-files`, bounded `read_dir`s, and per candidate one `status` plus up to four
`rev-parse` and one `merge-base`, all under `Limits`. A failure of the enumeration itself is the
detection-incomplete warning, never a failed review.

## 2. Remediation on escalate

No escalate-specific machinery:

- Gap 1's warning carries the exact remediation and rides on the escalate result like any other.
- **Reviewer prompt:** when detection found changed or unknown undeclared nested repositories, the
  review prompt gets one paragraph listing them (relative path only): their changes are not served
  to you; if the change under review depends on them, name the path in your prose and give verdict
  `blocked` rather than reviewing around them.

## 3. Per-review declaration of a nested repository (opt-in)

**Operator opt-in** (r1 f1). `--allow-nested-companions`, default off. Without it, the contract is
unchanged: the calling agent can name only configured companions. With it, the operator has decided
once that any repository physically nested in, and ignored by, the working root may be served on a
caller's request. Refused beside `--allow-reviewer-config`, like `--companion`, and on a Perforce
working root.

**Surface** (r1 f6). A `companions` entry that starts with `./` (or `.\`) is a **path relative to the
server working root**; any other entry is a configured name, exactly as today. No fallback from one
to the other, so a name never resolves to a path and a path never to a name.

- With the flag on, the `companions` schema property is advertised on every git server; its
  description lists the configured names and the `./<path>` form. With the flag off, the schema and
  the "names only" error are unchanged, and a `./` entry gets a correction naming the flag.
- A path companion's name is `n-` plus the sanitized last component (lowercased, characters outside
  `[a-z0-9-]` become `-`, truncated to `MAX_NAME_LEN`). The prefix keeps it out of the configured
  namespace in practice; an exact collision with a configured or another declared name is refused.
- A path that resolves to the **same root** as a configured companion is that configured companion
  (its configured name is used), so declaring by name or by path binds the same session.

**Accepted only in the #170 shape**, using exactly the checks a configured nested companion already
passes (no additional git-metadata checks; see r3's f2 note):

- relative, starts with `./`, no `..`, no root or prefix component; joined to the **server** working
  root, never a per-call `root` worktree (#167's rule that companion roots come from the server);
- `reject_reparse_on_ancestors` on the joined path, then canonicalized and required to be still
  inside the server working root;
- `check_root` (a git toplevel, disjoint from the state and temp directories);
- `check_against_root` with the `Separate` arm refused: outside the working root, not ignored by it,
  or anything tracked under it is refused.

Everything after acceptance is the configured-companion path unchanged: `CompanionEvidence::build`,
the per-turn `recheck` against the server root (which covers start-to-turn TOCTOU the same way it
does for a configured nested companion), the floor, the in-turn repair, hook binding and the v2
payload. The existing per-call-`root` overlap check in `start` applies unchanged. Session binding
(`CompanionBinding { name, root }`) already binds the root, so a resume re-declaring the same path
resumes and a different one is `SESSION_NOT_RESUMABLE`.

**The boundary move, stated plainly.** With the flag on, a calling agent can serve to the reviewer,
through a companion evidence server, any git repository whose working directory is nested in and
ignored by the working root, which today the reviewer reaches only as working-tree files. That
server reads whatever that repository's git metadata resolves to: its own history, and also anything
its local config or `.git` layout points at (a linked worktree's common directory, alternates,
`core.worktree`, a junction inside `.git`), which **can lie outside the working root**. This is
exactly the trust a configured companion and the primary working root already get today; the flag
extends it from operator-named repositories to every nested, ignored one. Planting metadata that
points outside needs write access to the working tree, which the reviewer does not have. Write
access is unchanged: the reviewer has none. With the flag off, nothing moves.

Not done here: confining git metadata for *any* root (overriding or validating `core.worktree`,
`commondir`, alternates, reparse points under `.git`). It is pre-existing for the primary and
configured companions and should be fixed for all roots at once, in its own PR: issue #218.

## 4. Converging on pasted code

**Cut to a prompt rule; no detection.** Gap 3 removes the reason to paste, and gap 1 tells the
caller that part of the change was unserved, on the converged result too. Detecting "code in
instructions" would fire on every re-review that quotes a finding, and detecting "the reviewer
relied on excerpts" from prose is unreliable, as the issue says.

The prompt rule, added to the formal review prompt: code quoted in the instructions is the caller's
claim, not evidence; an approval may rest only on what the evidence tools served; if the substance
of the change is visible to you only as quoted code, give verdict `blocked` and say which repository
it lives in.

**Not done: refusing convergence when gap 1 fires.** Round 1 agreed this would be disproportionate:
a nested repository often has unrelated work in progress, and every such review would stop
converging until the caller declared a repository the change does not touch. It relies on gap 1
telling "clean" from "unknown", which r2 now does. Easy to add later as `evidence_incomplete` if the
warning proves to be ignored in practice.

## Verify

Unit tests (`cargo test`):

- Detection: with `engine/` nested and ignored, a warning names it when it has (a) an uncommitted
  change and (b) a commit ahead of its origin fork point; (c) a commit ahead of `@{upstream}` with no
  origin default branch; an *unknown* warning with neither base resolvable and a clean status; none
  when clean and on base; none when declared; an *unknown* warning naming the failing command when a
  candidate's git call fails. The "configured but not declared" (matched by root,
  including a configured companion of the same basename at a *different* root, which must not be
  suggested) and "not ignored" wordings. `vendor/sdk` is found under an ignored `vendor/` and under a `vendor/` that is itself a
  nested repository. Each cap
  produces the detection-incomplete warning. A failing `ls-files` yields that warning, not an error.
- Path declaration, flag off: a `./engine` entry is refused naming the flag; schema unchanged.
- Path declaration, flag on: `companions: ["./engine"]` serves and certifies `engine/`'s diff exactly
  like a configured companion (floor, repair prompt, hook binding). Refused when not ignored, tracked
  under, not a toplevel, outside the root (`..`, absolute, a junction on the path), or overlapping a
  per-call `root`; the recheck refuses the same when introduced after start. A bare `engine` is still a configured
  name only. Same root as a configured companion binds under its configured name. Exact name
  collision refused. Resume with the same path resumes; a different path is refused.
- Existing configured-companion tests pass unchanged.

`smoke.ps1`, **both** `-Reviewer codex` and `-Reviewer claude` (gap 3 changes what spawns evidence
servers, which AGENTS.md treats as protocol; it costs tokens): a nested, ignored scratch repository
with a committed change, declared by `./` path on a server started with `--allow-nested-companions`,
and an assertion that the reviewer reached its `repository_diff` and the review converged only with
both diffs served.
