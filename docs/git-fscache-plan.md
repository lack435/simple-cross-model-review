# Isolated git: restore Git for Windows' `core.fscache` (issue #226)

Status: **implemented**. The plan review (cross-review session `issue-226-git-fscache`, Codex
gpt-6-luna, effort=xhigh) converged at round 4 with four findings, all resolved:

- f1: the boundary description omitted metadata reads and the existing user-level inputs.
- f2: parity covered only `--stat`, and real symlinks; narrowed to the measured cases.
- f3: the argument relied on reviewer read confinement, which differs by direction.
- f4 (major): on a cache miss, fscache lists the parent directory, following reparse points. Its
  facts were conceded and the widened metadata read accepted, with the rationale below; the
  reviewer agreed no concrete harm remains.

The implementation review (same session) converged at round 6 after one more minor finding, f5:
the README and code comment claimed output parity beyond the measured cases. Both are now
qualified. `smoke.ps1` passed for both reviewers.

## Problem

The evidence git runner disables system and global config (`GIT_CONFIG_NOSYSTEM=1`,
`GIT_CONFIG_GLOBAL=NUL`), so a hostile or merely personal config cannot change what git runs or
reports. Git for Windows turns `core.fscache=true` on in its **system** config, though, not as a
built-in default. On this machine that is `file:C:/Program Files/Git/etc/gitconfig  true`, under
`git version 2.55.0.windows.2`. So every isolated git command runs without the file-system cache
that every user shell has.

On a large working tree, that is the difference between fitting a budget and missing it. The issue
measured `git status` on a 224k-file nested Unreal Engine checkout: ~1.5 s in a user shell, ~6.5 s
under the isolation, and ~1.5 s under the isolation with `-c core.fscache=true`. The nested probe's
per-command ceiling is 3 s (`GIT_OP_TIMEOUT_MS`, `src/nested.rs`), so every review that does not
declare that repository warns that its change "could not be determined". The same slowdown applies
to every worktree-scanning evidence command on a large tree, including the root's own
`repository_diff` and status.

## Measured here

I built a synthetic repository on this machine: 100,000 tracked files in 200 directories, one
modified file, one untracked file, a `.gitignore`, and a junction to a directory outside the
repository. Every command ran with the runner's own environment (`GIT_CONFIG_NOSYSTEM=1`,
`GIT_CONFIG_GLOBAL=NUL`, `GIT_OPTIONAL_LOCKS=0`) and prefix (`--no-pager -c core.fsmonitor=
-c core.hooksPath=NUL`).

| `git status --porcelain --untracked-files=normal` | three runs |
|---|---|
| runner as today (fscache off) | 783, 907, 816 ms |
| runner + `-c core.fscache=true` | 55, 55, 57 ms |

Output parity, compared byte for byte (md5) with and without the cache:

- same: `status --porcelain=v2 -z --untracked-files=all`
- same: `status --porcelain -z --ignored=matching --untracked-files=all`
- same: `diff HEAD --stat`
- same, 413 bytes each: the full patch, `diff HEAD --no-ext-diff --no-textconv --binary --full-index`,
  and the worktree-against-index form of the same. Both carry the modified file and the untracked
  file, added intent-to-add for the comparison (review f2).
- same: `ls-files -z --others --directory --no-empty-directory` (the nested scan's listing)
- same: `ls-files -z --others --ignored --exclude-standard`

The junction gave the same answer both ways: git lists `d9/junc/t.txt` through it with or without
the cache. That is today's behaviour and the cache leaves it unchanged.

**Not verified: a real symlink.** This account lacks the privilege to create one, and fscache has
its own symlink handling, so the junction result is not evidence for symlinks (review f2). The
parity claim covers only the cases above. A tree holding real symlinks stays an open question.
At worst it is a difference in reported output. It cannot be a program run or a write, and Git
for Windows users already run with that behaviour in their own shells.

## Change

Every production git spawn goes through one of three places in `src/evidence/git.rs`:

- `run_classified`, behind `run`: every evidence, capture, nested-probe and worktree git command;
- `cat_blob`: `cat-file blob`;
- `git_config_status`: `git config` lookups, which deliberately read the user's config and never
  scan the work tree.

1. Add `-c core.fscache=true` to the fixed prefix in `run_classified`, next to
   `-c core.fsmonitor=` and `-c core.hooksPath=NUL`.
2. Pull the shared prefix into one constant used by both `run_classified` and `cat_blob`, so the two
   cannot drift. `cat-file` gains nothing from the cache, since it doesn't scan the work tree, but
   one prefix is simpler than two.
3. Leave `git_config_status` alone. It reads config and touches no work tree.

No `cfg(windows)` is needed: the server is Windows-only, and a git build without fscache (MinGit
included) ignores an unknown `core.*` key.

## Why this does not move the isolation boundary

The runner disables **system and global config**. That stops config from choosing what git *runs*:
hooks, fsmonitor, external diff, textconv, filters, pager. It also stops most config from changing
what git *reports*. Two user-level inputs still apply, as documented in `src/evidence/git.rs`
(review f1):

- the default `~/.config/git/ignore`, which git reads without any config telling it to; and
- `core.autocrlf`, which the parent resolves deliberately from the user's own config, so the
  attributes check matches the checkout.

This change touches neither.

`core.fscache` is an in-process cache that does neither of those things. Instead of calling `lstat`
path by path, git enumerates a whole directory once and caches every entry's **metadata**. So it can
look at the metadata (name, size, times, attributes) of sibling entries that a narrow pathspec would
not have stat'ed. It reads no file **contents** the command would not otherwise read, runs no
program, writes nothing, and lasts only as long as the one git process.

**It can list directories a command would otherwise only stat one entry in** (review f4, conceded).
On a cache miss, `fscache_lstat` lists the whole parent directory (`CreateFileW` with
`FILE_LIST_DIRECTORY`), following a junction or symlink on the way. So a path-limited command,
such as a `repository_diff` with a path, can enumerate the metadata of every entry in that path's
directory, including through a junction to a target outside the work tree, where today it would
`lstat` the one path. This plan accepts that and does not limit the cache to whole-tree commands.
The reasons:

- **Nothing it reads leaves git.** The cache only answers git's own `lstat` calls, and the parity
  measurements above show those answers produce byte-identical output. Sibling metadata in a
  listed directory is never returned to the reviewer, the caller or a log. The server's read
  boundary is about what reaches the reviewer, and that does not change.
- **Whole-tree scans in the same review already list those directories, junction targets
  included.** The canonical `repository_diff` covers untracked files, and the untracked scan descends
  through junctions: measured above, `status` lists `d9/junc/t.txt` with or without the cache. A
  path-limited command adds no directory that the review's own whole-tree scan does not already
  enumerate.
- **The cost is bounded.** A listing of a slow or huge directory reached this way is held to the
  same per-command operation timeout as everything else the runner spawns.
- **It is the state every Git for Windows user's own git runs in,** and the issue's slowdown hits
  exactly the commands that would be split out, so restricting the cache per command would add a
  second prefix to keep in sync for no output difference.

(Review f3: an earlier draft argued from the reviewer's read confinement instead. That differs by
direction, and this argument doesn't need it.)

Its output must be identical, and that is checked below rather than assumed.

One precedence consequence: `-c` outranks the repository's local config. A repository that sets
`core.fscache=false` locally, presumably to dodge an fscache bug, would get the cache anyway. That
is accepted rather than engineered around. Honouring a local opt-out would need a server-written
system config file, `GIT_CONFIG_SYSTEM` with `GIT_CONFIG_NOSYSTEM` lifted. That is new machinery on
the isolation boundary to protect a setting that is rare, and that Git for Windows overrides for
every user by default anyway.

## Tests

- **The setting reaches git through the runner.** `run(root, ["config", "--get", "core.fscache"])`
  returns `true` in a repository with no local setting. Command-line `-c` values are visible to
  `git config --get`, so this pins the prefix without timing anything.
- **Output parity is verified once, here, not in CI.** See "Measured here". Timing tests would be
  flaky and machine-dependent, so none are added.

Verification: `.\build.ps1`. Then `smoke.ps1 -Reviewer codex`, because this changes the evidence
runner's argv, which AGENTS.md counts as evidence-service protocol. Is the Claude round trip needed
too? It uses the same runner. AGENTS.md asks for it on changes to "the evidence service or the
Claude reviewer's isolation/spawn", and this is the evidence service, so run both.
