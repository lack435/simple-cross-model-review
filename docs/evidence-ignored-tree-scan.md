# Evidence scans in repositories with large ignored trees

Tracks issue #86: on v0.10.0 every Codex-direction review in a repository with a large
gitignored working tree fails immediately with

```
code: EVIDENCE_UNAVAILABLE
--- diagnostic output from the reviewer CLI ---
limit_exceeded: drift scan exceeded file budget
```

before any model call. v0.9.0 reviewed the same tree because it had no evidence service.

## 1. What actually fails

`Bundle::create` (`src/evidence.rs:179`) computes the capture-time drift stamp by calling
`core::initial_stamp` → `tree_stamp` (`src/evidence/core.rs:1416`). `tree_stamp` walks the whole
working root breadth-first, excluding only the five hard-coded names in `excluded_name`
(`.git`, `.hg`, `.svn`, `target`, `dist`), and pushes one row per entry — directories included.
At `limits.max_files` (20 000, `src/evidence.rs:97`) it returns `limit_exceeded`. Nothing above
it treats that as anything other than fatal: `Bundle::create` propagates, `readiness()`
propagates, and the review is refused with `EVIDENCE_UNAVAILABLE`.

So the failure is not a service defect at all. The reporter's repository has ~1 700 tracked
files and 400 000+ files on disk — a vendored engine tree, a cooked-content tree and a data
pipeline, every one of them gitignored and none of them reviewable content. The scan walks all
of them, blows the budget in the first fraction of the tree, and the review never starts.

Two consequences beyond the headline:

- **The same defect is in `walk_files`** (`src/evidence/core.rs:999`), the recursive walk behind
  `repository_search`. It uses the same `excluded_name` filter and the same `max_files` ceiling.
  Even with the drift stamp fixed, a reviewer that searches from the repository root in such a
  tree spends its whole request budget walking vendored content and gets `limit_exceeded` or
  `deadline_exceeded` back. Fixing only the stamp makes the review start and leaves its main
  discovery tool useless.
- **A merely large repository is indistinguishable from a broken service.** `EVIDENCE_UNAVAILABLE`
  says "the service could not be proved available". Here the service was fine; an advisory
  signal could not be computed. The caller is told to check its installation.

Measured on the reporter's repository (a private checkout, used here only as a repro):

| probe | result |
| --- | --- |
| `git ls-files` | 1 705 files |
| `git ls-files -c -o --exclude-standard` | 1 705 files, 79 130 bytes, 63 ms |
| `git status --porcelain` | clean, 38 ms |
| filesystem walk under `excluded_name` | 400 000+ entries, refuses at 20 000 |

Git already knows the answer, costs 63 ms to ask, and its answer is 235× smaller than the walk's.

## 2. What this change does

Three things, in descending order of how much they matter:

1. **Scope the recursive scans to reviewable files.** In a Git working root, ask `git ls-files`
   what is tracked (including submodule contents) and what is untracked-but-not-ignored, instead
   of walking the filesystem, and put every path git returns through the *same* validation the
   walk performs today (§4.1). Both the drift stamp and the `repository_search` walk use it.
2. **Make an uncomputable drift signal *unknown* rather than fatal.** Any failure to compute the
   stamp — budget, deadline, watchdog, missing git, git error — yields a nullable `drifted` with a
   reason, instead of discarding a review that was otherwise fine. Separately, `max_files`
   exhaustion in `repository_list` and the search walk truncates the result with `complete: false`
   rather than erroring.
3. **Say so out loud.** `--doctor` reports whether drift tracking is available, the scope response
   tells the reviewer which file set it is looking at and why drift is unknown when it is, and the
   search tool description says ignored files are not searched.

Nothing here relaxes a security boundary. Path confinement, reparse-point refusal, the read-only
tool surface, the sterile root, the request watchdogs and the byte budgets are all untouched.
The change narrows *which paths a scan enumerates*, keeps every check those paths already had,
and makes *one advisory signal* nullable.

## 3. Scope decisions, including what is deliberately not done

**Not adding a budget-override or exclusion-list flag** (issue suggestion 2). With the scan
scoped to git's own answer, the 20 000-file budget stops being reachable for any repository whose
*tracked* tree is under 20 000 files, and for the ones above it the degrade path in §4.3 keeps the
review running. A flag would add a documented surface, a validation path and a support answer
("raise the budget") to buy a case the fix already covers. If a real repository turns up whose
tracked tree exceeds the budget *and* which needs exact drift tracking, that is the moment to add
the flag, with a real number to size it against.

**Not reimplementing `.gitignore` matching.** Nested ignore files, negation, precedence between
`.gitignore`/`.git/info/exclude`/`core.excludesFile`, directory-vs-file semantics and Windows path
casing are a large amount of subtle machinery to get wrong. `git ls-files` is a bounded child
process that this repository already knows how to run safely (`src/evidence/git.rs`), against a
`git` binary the Codex direction already requires for `repository_history`.

**Not using `git status --porcelain` as the stamp.** It is cheaper still, but it weakens the
signal: a file that is already modified relative to HEAD produces the same porcelain line however
many times it is edited during the review, so a second edit to a file the reviewer already read
would go undetected. `ls-files` + `stat` keeps today's size/mtime sensitivity and changes only the
set of paths it applies to.

**`repository_list` keeps listing ignored entries.** It reads one directory, does not recurse, and
cannot reach the budget in any realistic tree. Hiding ignored entries there would also hide the
vendored trees from a reviewer trying to understand the project's shape. Only its budget behaviour
changes (truncate, not refuse).

**`repository_read` keeps reading ignored files.** A reviewer may legitimately need a generated or
vendored file by path. Only *enumeration* is scoped; resolution is not.

**An explicitly named file stays searchable, ignored or not.** `walk_files` already short-circuits
when its base resolves to a regular file (`src/evidence/core.rs:1006`), returning that one file
without walking anything. That path is kept: naming a file is the same opt-in as reading it, it
cannot cost more than one file, and removing it would make `repository_search` weaker than
`repository_read` for no benefit. Scoping applies to *directory* bases, where the cost is
unbounded and the noise is the problem. The tool description must say this exactly, because "we
do not search ignored files" and "you can search this ignored file if you name it" are both true
and a reviewer needs to know which it is holding. A consequence worth stating: searching a
directory that is entirely ignored returns no matches with `complete: true`, which is honest but
easy to misread — `scan_scope` in `repository_scope` is what explains it.

**Perforce is unchanged in kind.** There is no `git ls-files` equivalent that does not require a
network call, and this service's Perforce posture is deliberately local-only. Perforce roots keep
the filesystem walk and gain the degrade path, which is what makes a large Perforce workspace
reviewable rather than refused.

**Not making watchdog timeouts return partial results** (§4.4). A timed-out walk is abandoned by
the watchdog, so there is nothing partial to return without threading a shared buffer out of a
detached worker. That is real machinery for a case the enumeration change mostly removes.

## 4. Design

### 4.1 One enumeration helper, with the walk's checks preserved

Add to `src/evidence/git.rs`:

```rust
pub struct Enumeration { pub paths: Vec<String>, pub complete: bool }

pub fn reviewable_paths(root, limits, cancel, received_at) -> Result<Enumeration, EvidenceError>
```

`complete` rather than a bare `Vec` because three cases produce a short-but-useful list that an
`Err` would throw away (§4.4): the `max_files` ceiling, paths dropped by the validation below, and
the partial outcomes of the two-command rule. The 8 MiB stdout cap is deliberately *not* one of
them — see "Truncated output is an error" below.

It runs, through the existing hardened `run()` helper, two fixed commands **in this order** and
takes their union:

```
git … ls-files -z --others --exclude-standard      # 1: also the probe
git … ls-files -z --cached --recurse-submodules    # 2: retried once as plain --cached on failure
```

(`…` being the existing `--no-pager -c core.fsmonitor= -c core.hooksPath=NUL` prefix), with
`current_dir(root)` and the same isolated Git environment (`GIT_CONFIG_NOSYSTEM`,
`GIT_CONFIG_GLOBAL=NUL`, `GIT_OPTIONAL_LOCKS=0`, empty pagers). Fixed argv, no shell, no
caller-supplied fragment — neither command takes an argument derived from model input.

**Two commands rather than one, because of submodules.** `ls-files --cached --others` lists a
submodule as the single gitlink path `sub` and never its contents, while today's filesystem walk
descends into it and reads the files — so the obvious one-liner would silently drop every
submodule file from search and drift, against a coverage requirement this design already carries
(`docs/codex-reviewer-evidence-service.md:363`, "VCS … submodules"). `--recurse-submodules`
replaces the gitlink with the submodule's tracked files, but git refuses it together with
`--others` (`fatal: ls-files --recurse-submodules unsupported mode`, verified on this machine), so
the two halves are asked for separately and merged. Both outputs are sorted; the union is a
sort-and-dedup.

**The aggregate rule for two commands, because "one failed" is not one situation.** Command 1 uses
only flags that have existed for as long as `ls-files` has, so it doubles as the probe for "is git
usable and is this a work tree".

`git::run` cannot express the distinction this needs: it maps *everything* non-success to
`provider_failed` before it looks at output (`src/evidence/git.rs:129`, `:132`), so a timed-out
probe and a not-a-work-tree probe arrive identically — and under the round-4 rule a *timeout*
would have sent us into the 400 000-file walk. Nor is `RunOutcome` alone enough to classify on:
`reviewer::run` returns `io::Result<RunOutcome>` and flattens spawn and observe failures
(`src/reviewer/mod.rs:1590`, `:1601`), and `git::run` can refuse before spawning at all when the
request budget is already gone (`src/evidence/git.rs:115`) — three paths with no exit code, no
`timed_out`, and no `cancelled` to read.

So the enumeration classifies into **three** outcomes, which is fewer moving parts than the
round-5 table and covers strictly more:

| outcome | what it is | what it means |
| --- | --- | --- |
| `NoGit` | `on_path("git")` found no binary — decided once, before either command, no spawn involved | the **only** route to the filesystem fallback (§4.2) |
| `OutOfTime` | `timed_out`, `cancelled`, or the pre-spawn budget refusal | never retried: the budget that ran out is the budget a retry would spend |
| `Failed` | everything else — nonzero exit, spawn error, observe error, `run()`'s truncated/lossy refusal | command 2 gets one plain `--cached` retry; command 1 does not |

Command 1 failing as `OutOfTime` or `Failed` is an `Err` with **no** fallback. Command 2 failing
either way keeps command 1's paths with `complete: false`.

Three things this rule is built to prevent. A **mixed** outcome must never reach the filesystem
walk: git demonstrably works here, so falling back would walk the 400 000 ignored files this
change exists to avoid, for a repository that had already given us a usable list. A **timeout or a
transient failure** must not be read as "there is no git here" — round 5's rule fell back on any
nonzero exit, which lumps a corrupt index, a permissions failure and a bad config in with
not-a-work-tree, and the walk is the expensive wrong answer to all four. Requiring a *missing
binary* is the only condition that can be verified rather than inferred, and it is the only one
where the filesystem walk is genuinely the better answer; note also that this enumeration is
attempted only for a root the caller has already declared to be Git, so "git says this is not a
work tree" is a misconfiguration worth surfacing rather than papering over. And an **old git**
that rejects `--recurse-submodules` (added in 2.11, 2016) must not cost the entire tracked file
set — hence the one retry with a flag that predates it.

**What `--exclude-standard` honours here is narrower than git's default, deliberately.** The
evidence runner disables user and system config (`GIT_CONFIG_NOSYSTEM`, `GIT_CONFIG_GLOBAL=NUL`,
`src/evidence/git.rs:106`), so a `core.excludesFile` set in the user's global config is not read.
Verified on this machine, in a repo whose `vendored/` is ignored only by that global file:

| environment | `ls-files -o --exclude-standard` |
| --- | --- |
| global config loaded | `top.txt` — `vendored/` ignored |
| `GIT_CONFIG_GLOBAL=NUL` (this runner) | `top.txt`, `vendored/big.txt` — **not** ignored |
| `GIT_CONFIG_GLOBAL=NUL` + default `~/.config/git/ignore` | `top.txt` — ignored |

So per-directory `.gitignore`, `$GIT_DIR/info/exclude` and the default `~/.config/git/ignore` all
still apply; only a *custom* excludes path does not. That is not relaxed to fix this, for a reason
worth stating: the enumeration runs at capture time **and** inside the service, and drift compares
the two. The capture-side git runner (`src/vcs/git.rs:1325`) does *not* disable global config, so
borrowing its environment for one of the two call sites and not the other would produce two
different file sets under one `StampMethod::Git` label — a false drift report, which §4.3 exists
to make impossible. Both call sites use the isolated environment, and the result is consistent.
The cost is a repository whose large tree is ignored *only* by a custom global excludes path: it
is enumerated, and if that pushes past `max_files` the result is a truncated search and unknown
drift (§4.4) rather than a refused review. Such a user's remedy is a line in `.gitignore` or
`.git/info/exclude`, both of which are honoured.

What remains uncovered is *untracked* files **inside** a submodule: git does not descend a
submodule boundary for `--others`, and reaching them needs one child process per submodule. They
are out of scope, and §4.4 makes the omission visible at runtime rather than only in prose.

`-z` is load-bearing twice: it disables git's `core.quotePath` escaping, and it makes the
separator unambiguous for paths containing newlines. Output paths are relative to `root` because
git prints them relative to the process working directory.

**Git's answer is untrusted input, and gets exactly the checks a caller-supplied path gets.** A
tracked symlink, an in-root junction, a path under a hard-excluded directory name (`dist/` is
excluded by `excluded_name` but nothing stops a repository tracking files in it), or a corrupt
index entry must not widen what a scan reaches. Every returned segment therefore goes through:

1. `validate_relative_path` (`src/evidence/core.rs:1244`) — length, absolute/device/ADS refusal,
   reserved names, and the same `excluded_name` component rejection the walk applies. Paths that
   fail are **skipped**, not fatal: git listing a path this service will not look at is not an
   error, and one bad index entry must not lose the whole scan. The skip count is carried into the
   scan's completeness so a scan that dropped paths does not report as whole.
2. A reparse-point check on the file itself (`symlink_metadata` — which the stamp needs anyway for
   size and mtime) and on every **ancestor directory**, memoised in a set as the sorted path list
   is walked so each directory is stat-ed once. This is what reproduces `walk_files`' guarantee:
   it skips reparse-point children and never descends through one, and a per-file check alone
   would still let `root/junction/file` be reached. Both kinds of hit are skipped, exactly as the
   walk skips them.
3. A **regular-file** check from that same `symlink_metadata`. `walk_files` collects only
   `meta.is_file()` (`src/evidence/core.rs:1057`) and its results are handed to reads as
   already-`Resolved` targets, which skip the `is_file` guard (`src/evidence/core.rs:246`) — so
   anything not a regular file must be filtered here or it becomes a read of a directory. An
   uninitialised submodule's gitlink is the concrete case: git lists the path, the disk has an
   empty directory or nothing at all.

That is the whole difference between "git says this path exists" and "this service will look at
it". Nothing downstream is asked to trust the index.

Outcomes not covered by the per-command table above:

| condition | result | falls back? |
| --- | --- | --- |
| more than `limits.max_files` paths in the union | `Ok`, first `max_files`, `complete: false` | no |
| a path skipped by the checks above | `Ok`, `complete: false` | no |

**Truncated output is an error, not a prefix to parse.** Round 2 had this case returning the
parsed prefix; round 3 pointed out the contradiction, and it is worse than a wording problem.
`run()` already refuses truncated, incomplete or lossy stdout, and a multibyte filename split by
the 8 MiB cap makes the *whole* stream lossy through the `String` API
(`src/reviewer/mod.rs:1885`), so honouring that promise would mean giving the shared hardened
runner a byte-level path — for a repository whose path list alone exceeds 8 MiB, i.e. upwards of
130 000 paths, six times past the `max_files` budget where the answer is truncated anyway. The
case is deleted rather than engineered: such a repository gets `drifted: null` and a search that
fails with `limit_exceeded` naming the cap, and the review still runs. `Enumeration.complete`
survives because `max_files` and skipped paths still need it.

**Where the child runs — one bounded path, both call sites.** The enumeration runs on the **child
worker pool** (`run_bounded_walk(&CHILD_WATCHDOG, …)`), the same way `repository_history` and
`repository_revision` already do (`src/evidence/core.rs:909`, `:956`). An earlier draft put it on
the read pool and claimed `reviewer::run`'s own timeout bounded it; that was wrong. `reviewer::run`
kills an overrunning child and then calls `child.wait()` (`src/reviewer/mod.rs:1713`), which has no
bound — a process that will not die holds the caller indefinitely, which is precisely why
`CHILD_WORKER_CAP` exists (`src/evidence/core.rs:80`). A wedged `ls-files` on the read pool could
have made every read of the turn refuse with `read_unavailable`.

The stamp is therefore computed in **two bounded stages, sequentially, not nested**: enumerate on
the child pool, then stat the returned paths on the read pool under `STAMP_WATCHDOG` where the
stamp already lives. No worker ever waits on another pool's worker, so the pools stay independent.

**`Bundle::create` uses the same two stages**, anchored at `Instant::now()` instead of a request
receipt. A second draft argued this was unnecessary because the parent already runs `reviewer::run`
for the diff capture on every review, so the enumeration added no new *class* of hazard. That
argument does not hold: under `--diff off` (`DiffMode::None`), `supplies_change_of` is false
(`src/config.rs:1455`), `chain_needs_capture` is false, no capture runs (`src/tools.rs:1963`), and
the enumeration would be the **first** git child of the review — with nothing above
`Bundle::create` (`src/tools.rs:2889`) to bound it. Reusing the same helper costs one argument and
removes the special case entirely: capture time and service time now have identical bounds, and a
capture-time stall degrades to *unknown* (§4.3) instead of hanging a review before it starts.
That also newly bounds the *filesystem* stamp walk at capture time, which today is bounded only
cooperatively by `operation_timeout_ms` between directories.

### 4.2 The drift stamp

`tree_stamp` becomes:

- **Git root, complete enumeration**: rows from `reviewable_paths` after the §4.1 checks, one per
  surviving file, `"{relative}\0{len}\0{mtime_nanos}"`, sorted. A path that disappears between
  `ls-files` and `stat` — a real race, and a new one this change introduces — records
  `"{relative}\0missing"` instead of aborting the scan. Directories no longer produce rows: a new
  empty directory is not drift worth failing over, and the file rows already carry every path that
  has one.
- **Git root, `complete: false`**: `Drift::Unavailable`. A hash of an arbitrary prefix of the tree
  is not a stamp — two scans that truncate at different points would compare unequal and report
  drift that did not happen. Partial is useful for *search* (§4.4) and useless here.
- **Perforce root, or `NoGit`** (§4.1 — no git binary on PATH): today's filesystem walk,
  unchanged. `OutOfTime` and `Failed` do **not** come here; they are errors, and §4.3 turns them
  into unknown drift.

**Fallback happens in exactly one case** — `NoGit` in §4.1: there is no git binary to ask.
Everything else, timeouts and failures alike, keeps whatever git produced: once `ls-files` has
answered, that answer is the answer, short or not, and a failure to answer is not a reason to try
the other method. Falling back would do the exact thing this change exists to prevent — walk
400 000 ignored files — and then fail anyway.

The stamp is therefore still "size and mtime of every file the reviewer might care about, hashed",
just over git's file set instead of the disk's.

### 4.3 Unknown is a state, with provenance

Stamps produced by the two methods are not comparable, and "we could not compute one" is not the
same as "we have not computed one yet". Both distinctions need to be in the type, or a later
change re-introduces a false *not-drifted*. So:

```rust
enum StampMethod { Git, Filesystem }          // serde: "git" | "filesystem"

enum Drift {
    Stamp { method: StampMethod, sha256: String },
    Unavailable { reason: String },           // carries the failing code + a short message
}
```

- `Bundle.initial_stamp: Drift` replaces `String`. `Bundle::validate` keeps today's
  64-hex-digit check for the `Stamp` arm and bounds `reason`'s length for the other.
- `Bundle::create` never fails on a stamp error. **Any** error — `limit_exceeded`,
  `deadline_exceeded`, `provider_unavailable`, `provider_failed`, `read_failed`, a watchdog's
  `read_timeout`/`read_unavailable` — becomes `Unavailable { reason }`. One rule, no per-code
  table to get wrong and no list to keep in sync as the watchdogs grow codes. The single
  exception is `cancelled`, which stays an error: a cancelled request is not an observation.
- `Core.observed_stamp: Option<String>` becomes `Option<Drift>`, so an unavailable observation is
  cached once per service turn like a successful one, instead of re-spawning git on every read.

Drift is then a total function of two `Drift` values:

| baseline | observation | `drifted` | `drift_unavailable` |
| --- | --- | --- | --- |
| `Stamp{m, a}` | `Stamp{m, b}` (same method) | `a != b` | `null` |
| `Stamp{m1, _}` | `Stamp{m2, _}` (m1 ≠ m2) | `null` | "drift baseline and observation used different scan methods" |
| `Unavailable{r}` | anything | `null` | `r` |
| anything | `Unavailable{r}` | `null` | `r` |

Wire changes, in both responses that carry drift — `repository_read` gets the reason too, because
`repository_scope` is recommended to the reviewer, not mandatory, and a bare `null` with no
explanation is a worse signal than the boolean it replaced:

- `repository_scope`: `initial_stamp`/`current_stamp` become `string|null`; `drifted` becomes
  `boolean|null`; new `drift_unavailable` (`string|null`) and `scan_scope` (`string`, the
  enumeration method in words).
- `repository_read`: `drifted` becomes `boolean|null`; new `drift_unavailable` (`string|null`).

`SCHEMA_VERSION` goes to 2. The bundle is written and read by one process tree of one binary, and
its file is deleted when the review ends, so there is no migration to write — the version bump is
the reviewer-visible declaration that the contract changed, not a compatibility mechanism.

### 4.4 Budget exhaustion truncates; a timeout is still an error

Three call sites turn a ceiling into a hard error today. Two of them sit behind a response shape
that can already say "there was more":

- `walk_files` (`repository_search`): stop collecting at `max_files`, return what was gathered with
  `source_complete: false`, which flows into the existing `complete`/`truncated` fields. Paths
  skipped by the §4.1 checks, and an enumeration that came back `complete: false`, clear the same
  flag — that is the whole reason `Enumeration` carries it rather than erroring.
- `list` (`repository_list`): the same, over directory entries.
- `tree_stamp`: no partial answer is meaningful — a hash of half a tree is not a stamp — so it
  keeps returning `limit_exceeded`, which §4.3 turns into *unknown*.

**A repository with submodules marks its searches incomplete.** Today's filesystem walk descends
into a submodule and sees untracked files there; the enumeration cannot (§4.1). Left at that, a
root search would return `complete: true` while a whole class of file was never looked at, and
"no matches" is exactly the answer a reviewer turns into "this code does not exist". So when the
enumeration contains a root `.gitmodules`, search results carry `source_complete: false`. The
detection is one string comparison over a list that is already in hand, and it is accurate in
every repository where a submodule was added with `git submodule add`; a gitlink staged by hand
with no `.gitmodules` is a false negative, and is named here rather than engineered around.

**The stamp is deliberately *not* degraded by that same condition.** Marking drift unknown
whenever a repository has submodules would trade a small hypothetical loss for a large certain
one: every submodule-using repository would permanently lose a drift signal that works today, to
avoid reporting `drifted: false` when an *untracked file inside a submodule* changed mid-review —
a file that is in neither the captured change nor any plausible reviewed diff. Search's
completeness flag answers "did I look everywhere", where the omission actually misleads; drift's
boolean answers "did the reviewed thing move underneath us", where it does not. If that trade is
wrong, it is wrong in one line and can be flipped.

**Timeouts are deliberately not included.** `deadline_exceeded` inside a walk, and the watchdog's
`read_timeout`/`read_unavailable` around it, still fail their call. The watchdog abandons the
worker thread rather than collecting from it, so returning a partial result would mean threading a
shared buffer out of a detached worker and reasoning about what a half-finished walk means — real
machinery, for a case that ignore-scoping largely removes and that a retry already handles. §5's
matrix says which failures degrade and which do not; the claim in §2 is bounded to match.

This is the one place the change touches behaviour that is not broken today. It is included
because "a scan that hit its ceiling" and "a scan that found nothing" must not be the same
response, and because leaving search to hard-fail would mean the fallback path (Perforce, or git
unavailable) still loses the tool in exactly the repositories this issue is about.

### 4.5 Telling the caller and the reviewer

- `evidence::readiness` returns the drift-tracking state alongside success, and `--doctor` prints
  `evidence: ready (schema 2, 7 read-only tools; no-model handshake passed; drift tracking: on)`
  or `… drift tracking: unavailable - <reason>`. That is the line that makes "large repository"
  distinguishable from "broken service" without running a review.
- `repository_scope`'s `scan_scope` names the enumeration method, so a reviewer that searches for a
  vendored symbol and finds nothing knows why, and knows `repository_read` can still reach it.
  When the enumeration was short of whole — a truncation, skipped paths, a failed second command,
  or the submodule case — `scan_scope` says which, so the machine-readable `complete: false` on
  the search beside it has an explanation attached.
- The `repository_search` tool description says that a directory search covers only the enumerated
  file set and that naming a file searches it regardless (`src/evidence.rs`, §3). The Codex
  capability preamble (`src/config.rs`) gains one clause about drift possibly being unknown.
- README: note that `--allow-reviewer-config` does **not** disable the evidence service. The
  reporter expected it to, and the current text is silent rather than wrong.

## 5. Behaviour matrix

| situation | before | after |
| --- | --- | --- |
| small git repo | works | works; stamp over tracked + untracked-unignored files |
| git repo, ~1 700 tracked, 400 000 ignored | **review refused** | works; stamp and search cover 1 705 files |
| git repo, >20 000 tracked files | **review refused** | review runs; `drifted: null` + reason; search uses the first 20 000 enumerated paths with `complete: false`, and does **not** fall back to the filesystem |
| git repo, path list over the 8 MiB stdout cap (≳130 000 paths) | **review refused** | review runs; `drifted: null` + reason; search fails with `limit_exceeded` naming the cap, and does not fall back |
| git repo with submodules | walked, contents covered | tracked submodule contents covered via `--recurse-submodules`; untracked files inside a submodule are not enumerated, and searches report `complete: false` because of it |
| git older than 2.11 (no `--recurse-submodules`) | walked | retried as plain `--cached`: everything except submodule contents, `complete: false`, no filesystem fallback |
| one `ls-files` command fails, the other succeeds | n/a | union of what succeeded, `complete: false`, **no** filesystem fallback |
| the `ls-files` probe times out, fails to spawn, or is refused for budget | n/a | `Err`, `drifted: null`, search fails — **not** read as "no git here", so no 400 000-file walk |
| tree ignored only by a custom global `core.excludesFile` | walked, refused if large | enumerated (the isolated git config does not read it); large enough, that means truncation and unknown drift, not a refusal |
| git binary missing | **review refused** | filesystem walk (today's behaviour) if it fits, otherwise `drifted: null` + reason |
| git present but the root is not a work tree, or the index is corrupt | **review refused** | `Err`, `drifted: null` + reason, no filesystem walk — a Git-declared root that git rejects is surfaced, not papered over |
| search naming an ignored file explicitly | searched | searched, unchanged |
| search naming a wholly ignored directory | searched | no matches, `complete: true`; `scan_scope` explains why |
| tracked symlink / junction / index path under an excluded name | n/a (walk never saw it) | skipped; scan reports not-complete |
| Perforce workspace | works under budget, else refused | works; over budget it degrades instead of refusing |
| walk or stamp **times out** | error | error, unchanged (stamp's becomes `drifted: null`) |
| genuinely broken evidence service | `EVIDENCE_UNAVAILABLE` | `EVIDENCE_UNAVAILABLE`, unchanged |

## 6. Tests

Unit tests, no network and no model calls:

1. `reviewable_paths` parsing and the §4.1 outcome table, one case per row: `-z` splitting, empty
   tail dropped, the union of the two commands sorted and deduped, `max_files` truncating to
   `complete: false` rather than erroring, and truncated/incomplete/lossy stdout, `OutOfTime` and
   `Failed` each erroring with the code §4.1 names. Driven through a fake enumerator so parsing is
   testable without a git repository.
2. §4.1 validation: a path with a `..`/absolute/device/ADS/reserved component is skipped; a path
   under an `excluded_name` component (e.g. `dist/x.txt`) is skipped; a reparse-point file is
   skipped; a file whose **ancestor directory** is a reparse point is skipped; a path that is not
   a regular file (a directory, an uninitialised submodule's gitlink) is skipped; each of these
   clears the completeness flag rather than failing the scan.
3. Stamp behaviour over a fake enumeration: a change to an enumerated file moves the stamp; a file
   outside the enumeration does not; a path that vanishes before `stat` records `missing` rather
   than erroring.
4. Degrade: `initial_stamp` with `max_files: 1` over a two-file tree yields
   `Drift::Unavailable`; `Bundle::create` succeeds with it; a *cancelled* stamp still errors.
5. The §4.3 table as a table test, including **method mismatch** (git baseline, filesystem
   observation) → `null`, and read-before-scope → `drifted: null` with a non-null reason.
6. The unavailable observation is cached: two reads in one turn enumerate once.
7. Declared output schemas accept the nullable shapes, `additionalProperties` stays `false`, and
   the seven-tool allow-list is unchanged (the handshake assertion must still pass).
8. `list` and `walk_files` at their ceiling return a truncated page with `complete: false`; a
   walk driven to its **watchdog timeout** (via the existing injected-watchdog seam,
   `Core.walk_watchdog`) still errors — the §4.4 boundary asserted in both directions.
9. Fallback boundary: a git root whose enumeration returns `complete: false` searches the
   enumerated prefix and **never** runs the filesystem walk; only `NoGit` runs it, and a `Failed`
   root (git present, non-zero exit) does not. Asserted by observing which enumeration ran, not by
   timing.
10. An enumeration that is `complete: false` yields `Drift::Unavailable`, not a stamp over the
    prefix.
11. Explicit-path search: naming an ignored regular file returns its matches; naming a wholly
    ignored directory returns no matches with `complete: true`.
12. Git-backed integration test, skipped when `git` is not on PATH: init a temp repo, commit a
    file, add an ignored file **and an untracked non-ignored file**. Assert the ignored file is
    absent from the enumeration and does not move the stamp, and that the untracked file *is*
    enumerated and *does* move it — without the second half, an implementation that passed
    `--cached` alone would pass the test.
13. Git-backed submodule test, same skip condition: a superproject with one committed submodule.
    Assert the submodule's tracked file is enumerated as `sub/a.txt`, that the bare gitlink path
    `sub` is not in the enumeration, that editing the submodule's file moves the stamp, and that a
    search in that repository reports `complete: false`.
14. The §4.1 outcomes as a table test over a fake two-command runner: `NoGit` is the only case
    that reaches the filesystem walk; command 1 timing out, exiting nonzero, **failing to spawn**,
    **failing to observe**, and **refused pre-spawn for budget** each stay `Err` without falling
    back; command 2 `Failed` then succeeding on the plain `--cached` retry gives the union with
    `complete: false`; command 2 `OutOfTime` is not retried; both failing gives command 1's paths
    with `complete: false`.
15. Ignore-source scope, git-backed and skipped without `git`: a tree ignored only by a custom
    global `core.excludesFile` **is** enumerated under the isolated environment, while one ignored
    by `.git/info/exclude` or the default `~/.config/git/ignore` is not — the narrowing in §4.1
    asserted rather than assumed, and a canary if a future change borrows the capture-side
    environment for one call site only.

Then, per AGENTS.md:

- `.\build.ps1` — fmt, clippy `-D warnings`, unit tests, release build.
- `smoke.ps1 -Reviewer codex` — required, because this changes the evidence service, which is
  protocol; the Claude direction does not exercise it. Costs tokens.
- Manual: `cross-review.exe --reviewer codex --model gpt-5.6-luna --effort max --doctor` in the
  reporter's repository, which must report `evidence: ready`, and a real review there that starts.

## 7. Files touched

| file | change |
| --- | --- |
| `src/evidence/git.rs` | `reviewable_paths`, fixed-argv `ls-files`, parsing/validation tests |
| `src/evidence/core.rs` | `Drift` type and comparison; `tree_stamp` over the enumeration; §4.1 path checks; child-pool enumeration stage; scope/read drift fields; `walk_files`/`list` truncate at the ceiling |
| `src/evidence.rs` | `Bundle.initial_stamp: Drift`, `SCHEMA_VERSION` 2, output schemas, tool descriptions, `readiness` returns drift state |
| `src/tools.rs` | `--doctor` evidence line reports drift tracking |
| `src/config.rs` | one clause in the Codex capability preamble |
| `README.md` | evidence section: scan scope, unknown drift, `--allow-reviewer-config` clarification |
| `docs/evidence-ignored-tree-scan.md` | this document |

## 8. Risks

- **`git ls-files` cost in a pathological tree.** Git skips wholly-ignored directories when
  enumerating `--others`, which is why the reporter's 400 000-file tree answers in 63 ms. A tree
  whose ignore rules are per-file rather than per-directory would make git walk it too. Inside the
  child is bounded by the child watchdog at both call sites and its failure degrades to *unknown*,
  so the worst case is a lost drift signal, not a lost review.
- **The ancestor-reparse memo is per-scan, not a live guarantee.** A junction created between the
  ancestor check and the file's `stat` is not caught by it. The read path's final-handle
  verification (`verify_open_file`, `within`) still refuses anything that resolves outside the
  root, so the residual is an in-root junction appearing mid-scan — the same race the existing
  walk has, neither widened nor narrowed here.
- **Untracked files inside a submodule are not enumerated.** Tracked ones are
  (`--recurse-submodules`, §4.1); reaching the untracked ones would need a child process per
  submodule. They stay reachable by `repository_read` and visible to `repository_list`, searches
  in such a repository report `complete: false` (§4.4), and drift does not see them — a stated
  trade, not an oversight.
- **Drift over ignored files is no longer detected.** If a reviewer reads an ignored file and that
  file changes mid-review, `drifted` stays false. Accepted: drift exists to stop the captured
  change and the live tree being presented as one snapshot, and ignored files are in neither.
- **Nullable `drifted` reaches a model.** A reviewer that ignores `null` and assumes "not drifted"
  is a slightly worse outcome than today's hard failure — but only in the repositories where today
  there is no review at all.

## 9. Review history

Round 1 (`gpt-5.6-luna`, session `issue-86-plan`) requested changes with four major findings, all
accepted:

- **f1** — the enumeration child was placed on the read worker pool with a claim that
  `reviewer::run`'s timeout bounded it. It does not: the kill path calls an unbounded
  `child.wait()`, which is why the child pool exists. Moved to `CHILD_WATCHDOG` as two sequential
  bounded stages, and the capture-time bound is now stated accurately rather than overclaimed
  (§4.1).
- **f2** — git-supplied paths were validated only lexically, losing the walk's reparse-point and
  excluded-component guarantees before being handed to `search` as already-`Resolved` reads.
  §4.1 now reapplies both, including ancestor directories, with tests.
- **f3** — the degrade story covered only `max_files` and silently implied watchdog failures too.
  The stamp now degrades on *any* error (one rule, not a code table), and §4.4/§5 state plainly
  that timeouts still fail their call, with the reason.
- **f4** — nullable drift had no provenance: `Option<String>` cannot distinguish unobserved from
  observed-unavailable or record which method produced a stamp. Replaced with an explicit `Drift`
  type carrying method and reason, cached as such, with the reason exposed on `repository_read`
  as well as `repository_scope`.

Round 2 resolved f2, f3 and f4, held f1 open, and raised three more. All accepted:

- **f1** (held) — the capture-time half was not answered. The argument that the parent already
  runs a git child for the diff capture fails under `--diff off`, where no capture runs and the
  enumeration would be the first git child of the review, unbounded. `Bundle::create` now uses the
  same two bounded stages as the service, anchored at `Instant::now()`; the special case is gone
  and the filesystem walk at capture time gains a bound it did not have (§4.1).
- **f5** — `reviewable_paths` returned `Result<Vec<String>>`, so the two ceilings that produce a
  perfectly usable short list threw it away, and the fallback rule then sent a large git tree into
  the 400 000-file walk it was meant to avoid. Now returns `Enumeration { paths, complete }`;
  fallback happens only when git *did not answer* (§4.1, §4.2).
- **f6** — the explicit-file search path was left undefined. It stays searchable, which is now
  stated in §3, in the tool description, and in tests 11 and the matrix.
- **f7** — the integration test would have passed a `--cached`-only implementation. It now asserts
  an untracked non-ignored file is enumerated and moves the stamp (test 12).

Round 3 resolved f1, f6 and f7, held f5 open on a residual, and raised f8. Both accepted:

- **f5** (held) — the round-2 fix promised a parsed prefix from capped stdout while also rejecting
  lossy stdout, and those cannot both hold: a multibyte filename split at the cap makes the whole
  stream lossy through `run()`'s `String` API. Resolved by **deleting the prefix case** rather
  than giving the shared runner a byte-level path — see §4.1. This makes the plan smaller.
- **f8** — `ls-files --cached --others` lists a submodule as a bare gitlink and never its
  contents, so the enumeration would have silently dropped submodule files that today's walk
  covers, against `docs/codex-reviewer-evidence-service.md:363`. §4.1 now issues two commands and
  unions them, adds a regular-file filter (which also catches the gitlink itself), and §8 names
  untracked-inside-a-submodule as the remaining uncovered case. Tests 2 and 13.

Round 4 resolved f5 and f8 and raised three more, all accepted:

- **f9** — two commands had no aggregate failure policy, so a mixed outcome could re-enter the
  filesystem fallback the change exists to avoid. §4.1 now orders the commands (the flag-free one
  first, as the probe), gives the outcome table, and confines fallback to a command-1
  no-git/non-worktree failure. The same rule closes an old-git cliff the plan had not noticed:
  `--recurse-submodules` is retried once as plain `--cached` rather than costing the whole tracked
  set. Test 14.
- **f10** — untracked files inside submodules were omitted with only prose to say so, leaving a
  root search able to report `complete: true` over a class of file it never looked at. §4.4 now
  clears `source_complete` when the enumeration contains a root `.gitmodules`. The same paragraph
  declines to degrade *drift* on that condition and gives the reasoning, because that would cost
  every submodule repository a working signal to cover a file that is in neither the capture nor
  the diff — flagged explicitly as a trade to challenge rather than a silent choice.
- **f11** — the `Enumeration` rationale still cited the 8 MiB cap after the case was deleted.
  Corrected.

Round 5 resolved f10 and f11, held f9 open on a residual, and raised f12. Both accepted:

- **f9** (held) — the round-4 table keyed on `provider_failed`, which `git::run` produces for
  *every* non-success including a timeout, so a timed-out probe would still have fallen into the
  400 000-file walk, and every command-2 failure would have been retried. The enumeration now
  classifies from `RunOutcome` (`timed_out`/`cancelled`/`exit`) into four cases per command;
  fallback is one cell, and the retry is the nonzero-exit cell only. Test 14 covers each.
- **f12** — `--exclude-standard` does not read a custom global `core.excludesFile` under this
  runner's isolated config, verified with a three-case experiment now quoted in §4.1. Not
  relaxed: the capture-side runner *does* load global config, so borrowing its environment for one
  of the two enumeration call sites would give two file sets one `StampMethod::Git` label and a
  false drift report. The narrowing is documented, its cost is degradation rather than refusal,
  and test 15 pins it.

Round 6 resolved f12 and held f9 open once more, on a third residual — accepted, and the fix is a
cut rather than an addition:

- **f9** (held) — classifying on `RunOutcome` still left three failures unclassified
  (`reviewer::run` flattens spawn and observe errors into `io::Error`, and `git::run` can refuse
  before spawning when the budget is gone), and "nonzero exit" lumped corrupt-index, permission
  and config failures in with not-a-work-tree. The 2×4 table is replaced by three outcomes —
  `NoGit`, `OutOfTime`, `Failed` — and fallback now requires `NoGit`, the only condition that is
  *verified* (no binary on PATH, decided before any spawn) rather than inferred from an exit code.
  A Git-declared root that git rejects now surfaces as an error instead of quietly becoming a
  400 000-file walk. Test 14 covers the spawn, observe and budget paths explicitly.
