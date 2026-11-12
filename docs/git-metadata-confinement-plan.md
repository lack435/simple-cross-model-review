# Git metadata that resolves outside the served root — plan

Status: **implemented** for issue #218, including its follow-up comment (check-then-use on the served root).
Cross-review session `plan-issue-218-git-metadata` (Codex, gpt-6-luna, xhigh): **converged at
round 2**. Round 1: three minor findings, all accepted and confirmed resolved. f1 extends the env scrub list; f2 adds the submodule layout; f3 is answered
by narrowing the claim and accepting file-level object redirects as a stated residual.

**Code review.** Session `feat-issue-218-git-metadata-confinement` (Codex, gpt-6-luna, xhigh):
**converged at round 3**. Four minor findings were raised.
- **f1:** the pin registry is keyed on the root's spelling, so a moved root is never served unpinned.
- **f2:** an oversized metadata file is refused rather than read as a prefix.
- **f3:** the README claim was narrowed to what is actually checked.
- **f4:** a root replaced by a junction mid-turn is accepted as a documented residual (see "Residual,
  accepted"). The reviewer agreed after the dispute.

**Implementation notes.** Built as planned, with these differences:

- **No bundle field, no schema bump.** The evidence child does not take the git directory from the
  bundle: it runs `validate_layout` again at startup, in the process that serves the root, and pins
  that result. The pin is a small registry in `src/evidence/git.rs` keyed by root. `Core` holds a
  guard that unpins on drop, and every git spawn in that module consults the registry. This keeps the
  check closer to use and leaves the bundle format alone.
- **A root inside a repository is validated through its top level.** A Codex review's working root
  need not be the git top level, so the layout checks run on the top level git reports, and the root
  must sit at or under it. `GIT_WORK_TREE` is pinned to that top level.
- **The moved-root check refuses only a path that goes through a reparse point.** Comparing spellings
  alone would refuse CI's 8.3 temp paths, which canonicalize to their long form but were never moved.
- **Reparse points are refused among *all* direct children** of the git and common directories and of
  `objects`, files included. It costs the same walk, and nothing legitimate puts one there.
- **Tests.** A Git bundle over a non-repository can no longer start a service. The four core tests that
  use one to exercise degraded reads now build their `Core` through a test-only constructor that skips
  the layout check. The `evidence.rs` test helper makes its directory a repository first.

## The problem, verified

The evidence service's git runner turns off system and global config, but it still finds the
repository by discovery from the served root and follows that repository's own metadata. Checked
on git 2.55.0.windows.2, in a throwaway repository `r` beside an unrelated repository `other`:

- With `core.worktree` in `r/.git/config` pointing at `other`, `git status` run in `r` lists
  `other`'s files (`?? s`) and `git ls-files` lists `r`'s index against `other`'s tree. A
  `repository_diff` of `r` would serve `other`'s untracked files as new content.
- Running the same commands with `GIT_DIR=r/.git` and `GIT_WORK_TREE=r` ignores `core.worktree` and
  serves `r` correctly. The same pinning works for a linked worktree (`GIT_DIR=<common>/worktrees/<id>`).

From the issue and git's documented behaviour (not re-run here): `objects/info/alternates`, a `.git`
file naming an arbitrary gitdir, its `commondir`, and a junction inside `.git` (for example at
`objects`) each let history and object reads resolve outside the served root.

The follow-up comment's gap: `worktree::recheck` and `companion::recheck` check a root, then
`Bundle::create` (`src/evidence.rs:449`) canonicalizes it again and writes whatever *that* resolves
to into the bundle. The evidence child does refuse a bundle root that no longer canonicalizes to
itself (`src/evidence/core.rs:647-661`), but by then the bundle already names the redirected path.

**Realistic risk is low** (see the issue). Planting any of this needs write access to the working
tree, which the reviewer does not have; the calling agent has it but can already read everything.
This is defence in depth for the Claude direction, whose reads all go through the evidence service.
The plan is sized to that: one validator, one pinning change, no handle-holding machinery.

## Scope

Every root the evidence service serves goes through `Bundle::create`: the primary working root, a
per-call `root` worktree (`src/tools.rs:4451-4454`), each companion — configured, nested or
`./`-declared (`src/tools.rs:6477-6484`) — and the status preflight (`src/evidence.rs:1263`). So the
fix lives in `Bundle::create` and the evidence child, and covers all of them at once. Perforce bundles
are untouched.

## Design

### 1. Validate the git layout in `Bundle::create` (git only)

A new `evidence::git::validate_layout(root) -> Result<Layout, EvidenceError>`, where
`Layout { git_dir, common_dir }` holds physical (canonical) paths. It runs before anything else in
`Bundle::create`, including `git_staging`, which already runs git in the root. The bundle is refused,
with code `unsafe_git_layout` and a message naming the cause and the fix, unless all of these hold:

1. **`.git` is one of the three shapes git itself creates**, each recognised by a back-pointer from
   the git directory to this root:
   - **a plain repository:** `<root>/.git` is a real directory (no reparse point). Then
     `git_dir = common_dir = <root>/.git`;
   - **a linked worktree:** `<root>/.git` is a file `gitdir: <X>` (absolute or relative to the root;
     git 2.48+ can write relative paths). `G = physical(X)` must be exactly
     `<C>/worktrees/<id>`, `G/commondir` must resolve to that same `C`, and `G/gitdir` must resolve
     back to `<root>/.git`. Then `git_dir = G`, `common_dir = C`;
   - **a submodule** (review f2; supported today, since `reviewer::is_git_toplevel` accepts its `.git`
     file): `<root>/.git` is a file whose `G = physical(X)` has `.git/modules/` in its path (nested
     submodules repeat `modules/<name>`), has no `commondir`, and its own `G/config` explicitly sets
     `core.worktree` resolving to the root — the back-pointer git writes for every submodule. Then
     `git_dir = common_dir = G`. A submodule's linked worktree is caught by the linked-worktree rule,
     with `C` being the submodule's git directory.

   Anything else is refused, for example a `.git` junction, or a `.git` file naming another
   repository's git directory with no back-pointer to this root. Without the explicit back-pointer,
   a `.git` file naming an unrelated repository's `.git` would pass item 2, because git defaults the
   working tree to the gitfile's directory.

   **Why a linked worktree may point outside the root.** `C` is that worktree's own repository, so
   its objects are this root's history, and git reads no other worktree's files from it. That is
   what keeps the layouts supported today working without an allowlist of authorised roots: a
   per-call `root` (`C` is `<server>/.git`), a linked-worktree companion (whose `C` may sit
   anywhere), and a primary working root that is itself a linked worktree (a Claude Code worktree
   session). The structural test is what stops a hand-written `.git` file from borrowing an
   unrelated repository.
2. **git agrees.** `git rev-parse --show-toplevel --git-dir --git-common-dir`, run by discovery in the
   root under the hardened environment (item 3), resolves to the root, `G` and `C`. This catches
   `core.worktree` (top level elsewhere), including one set through `include.path` or
   `config.worktree`. A repository whose own git thinks its working tree is somewhere else is
   refused, not silently served as the root. The fix the message suggests: unset `core.worktree`.
3. **No alternates.** `C/objects/info/alternates` is missing or has no entries. A
   `clone --shared`/`--reference` repository is refused, and the message gives the documented way
   out: `git repack -a -d`, then delete the alternates file. Allowing alternates inside the
   authorised roots is not worth the machinery.
4. **No directory reparse points where git reads objects or metadata:** at `G` and `C`, each of their
   direct child directories, and each direct child directory of `C/objects` (fan-out, `pack`,
   `info`) — about 260 `symlink_metadata` calls, using the `FILE_ATTRIBUTE_REPARSE_POINT` test from
   `winsec`. Deeper directories under `refs`/`logs` are not walked: a redirected ref is useless
   without the objects it names, and those are confined by this check and item 3.

**Not covered, said plainly (review f3): file-level redirects inside the object store.** The
check covers *directories* only, so the confinement claim is: git metadata cannot redirect a
**directory** git reads outside the repository. A foreign pack or loose-object file can still be
brought in at the file level, by a **hard link** (not a reparse point, and impossible to tell apart
from a real file) or a **file symlink** (needs Developer Mode or elevation on Windows). Checking file
reparse points in `objects/pack` alone would be cheap, but it would close nothing: a hard link does the
same job with fewer privileges. Checking every loose object file is a walk of up to hundreds of
thousands of files per turn. Neither is proportionate to a low risk whose precondition, write access
to the working tree, the reviewer lacks. So both are accepted as residual.

### 2. Bundle the validated root and layout; refuse a moved root

- `Bundle::create` refuses with `invalid_root` when the root it is given does not canonicalize to
  itself (after `normalize_dir`). Every caller already passes a canonical, just-checked root (the
  normalized `--cwd`, the per-call root from `worktree::resolve`, `Companion::root`), so a junction
  swapped in between the recheck and this point is refused rather than followed. This closes the
  follow-up comment's gap without holding directory handles open for the turn.
- The bundle gains `git_dir` (the validated `G`), so the child can pin it (item 3). The child already
  re-checks the root at startup (`core.rs:647`); it also re-checks that `git_dir` canonicalizes to
  itself. `SCHEMA_VERSION` goes to 4: parent and child are the same binary, so nothing else changes.

### 3. Pin and scrub git's environment in the runner

At the evidence child's two git spawn sites (`run_classified`, `git.rs:985`, and the `cat-file`
streamer, `git.rs:~405`):

- set `GIT_DIR=<bundle git_dir>` and `GIT_WORK_TREE=<bundle root>`. Discovery and `core.worktree` then
  play no part for the rest of the turn, so swapping `.git` or adding `core.worktree` mid-turn has
  no effect;
- `env_remove` the inherited variables that relocate a repository or inject config: `GIT_DIR`,
  `GIT_WORK_TREE`, `GIT_COMMON_DIR`, `GIT_OBJECT_DIRECTORY`, `GIT_ALTERNATE_OBJECT_DIRECTORIES`,
  `GIT_INDEX_FILE`, `GIT_NAMESPACE`, `GIT_CEILING_DIRECTORIES`, `GIT_DISCOVERY_ACROSS_FILESYSTEM`,
  `GIT_GRAFT_FILE`, `GIT_SHALLOW_FILE` (review f1: both redirect history metadata),
  `GIT_REPLACE_REF_BASE`, `GIT_CONFIG`, `GIT_CONFIG_PARAMETERS`, `GIT_CONFIG_COUNT` (and so its
  `KEY_n`/`VALUE_n`). The list is one shared constant, used by every git spawn in the crate's
  evidence and validation paths.
  A profiled reviewer's environment is already cleared (`apply_controlled_env`), but an ambient one
  and the server itself inherit the caller's environment.

Parent-side git calls on the server root (`list_worktrees`, `tracks_under`, `ignores_untracked`,
`git_config_status`) only read metadata and listings, never file or object content. They get the
`env_remove` scrub but no pinning, so their signatures do not change.

**Residual, accepted: changes made during the turn, after the service validated at startup.** This
covers two cases:
- metadata edited *in place* (alternates added, a junction made under `objects`);
- the root itself renamed and its path replaced by a junction to another repository (code review
  f4). The pinned `GIT_DIR`/`GIT_WORK_TREE` are paths, so they resolve through the new junction.

Both checks run when the bundle is built and when the service starts, and pinning removes discovery
and `core.worktree`, so neither can redirect anything after that. Closing the turn-long window itself
would mean binding git to directory handles held for the whole turn. Re-checking before each spawn
would only narrow it, since the gap between check and spawn stays. Either is machinery out of
proportion here. Every one of these cases needs write access to the working tree mid-turn, which the
reviewer lacks. The only party that has it, the calling agent, can already read everything the
reviewer could. And the Codex reviewer's own shell reads are unconfined anyway (README).

## Not changed

- No allowlist of authorised roots, and no new flag. The linked-worktree shape test replaces both.
- `companion::check_root`'s existing `reject_reparse_on_ancestors` and the per-call `root` rules stay
  as they are. Neither is needed for this fix, and layout problems in a companion surface at its first
  turn through `Bundle::create`. (Running `validate_layout` at startup as well would be a convenience,
  not a boundary; left out.)
- Startup does not validate the primary root's layout either: the status preflight
  (`cross_model_review_status`) builds a real bundle, so a bad layout shows up there first.

## Tests (unit, `cargo test`)

Each case goes through `Bundle::create`, the one place every root kind passes:

- **refused:** `core.worktree` set to a sibling repository; alternates pointing at a sibling;
  a junction at `.git/objects` and one at `.git/objects/ab`; `.git` itself a junction; a `.git` file
  naming a sibling repository's `.git` (not a `worktrees/<id>` layout); a `.git` file naming a
  `worktrees/<id>` whose `gitdir` points at a different worktree; a `.git` file naming a
  `.git/modules/<name>` directory whose `core.worktree` is unset or points elsewhere; a root whose
  path no longer canonicalizes to itself (junction swapped on it).
- **accepted, and `repository_diff` serves the right change:** a plain repository; a nested
  per-call worktree (`.claude/worktrees/w`); a sibling linked worktree as a companion; a worktree
  made with `--relative-paths`; a primary root that is itself a linked worktree; a submodule
  checkout as a root.
- **runner:** the built `Command` carries `GIT_DIR`/`GIT_WORK_TREE` from the bundle and removes every
  scrubbed variable (inspected with `Command::get_envs`, so no process-wide env mutation in tests).
- Existing companion, nested-companion and per-call-root tests pass unchanged.

The evidence service is protocol, so `smoke.ps1 -Reviewer codex` and `-Reviewer claude` both run
before the PR is handed back (real model calls; costs tokens).

## Size

One validator (~120 lines), a bundle field, two spawn-site changes, a shared scrub helper, and
tests. One PR.
