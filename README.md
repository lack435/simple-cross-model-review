# cross-review

An MCP server that hands your work to a **different model** for review, and gives you
back what it said.

Claude Code asks Codex. Codex asks Claude. One request, one response — the calling agent
decides what to do with the feedback. There is no orchestration, no multi-agent
choreography, and no attempt to be clever about it. (One exception, bounded and opt-out: when
a reviewer answers without the machine-readable findings block it was asked for, the server
asks it once more for the block alone rather than throwing the whole turn away. See
[block repair](#when-the-reviewer-skips-its-machine-block) below.)

Windows only. A single 520 KB executable with no runtime dependencies: no Node, no
Python, no DLLs. Vendor the `.exe` into a repository and a fresh clone works.

## Why

An agent reviewing its own work shares its own blind spots. A different model, with a
different training run and different failure modes, finds things the author could not.
This makes that a single tool call.

## The tools

| Tool | Purpose |
| --- | --- |
| `cross_model_review` | Start a review. Returns a `review_id` immediately. |
| `cross_model_review_result` | Wait for and return the review — one blocking call, with live progress when the client supports it. |
| `cross_model_review_status` | Is the reviewer CLI installed and signed in? Costs nothing, calls no model. |
| `cross_model_review_cancel` | Stop a review that is still running. This is the tool that frees the reviewer; abandoning a `cross_model_review_result` poll does not. |

Reviews are asynchronous because a serious review of real work takes time. In this
project's usage, reviews commonly take at least five minutes, and complex changes can take
20 minutes or longer; a running review in that window is normal, not a reason to cancel or
start over. Starting and collecting are separate calls, so the harness is never blocked
before it chooses to wait. **A single `cross_model_review_result` collects a whole review in
one blocking call** — omit `wait_seconds` to block to completion — so the mandatory
poll-again loop is gone. The wait cap tracks the review budget rather than a fixed 300s: it
is the capture budget plus `--timeout-seconds` plus the block-repair budget plus a
finalization grace. The default
per-turn hard limit is 30 minutes and can be changed with `--timeout-seconds` (bounded at
24h). Raising it also lets a wedged reviewer bill and hold its session lease for longer
before the server stops it, and it widens the collect cap to match.

Two things can end a collect before the review does, and they look different to the caller.
If the `wait_seconds` budget elapses server-side, the call *returns* `status=running` — call
again with the same `review_id`. If the client's own tool timeout is shorter and fires first,
the client sends `notifications/cancelled` and the response is suppressed — the caller sees a
client-side timeout, not a `status=running` result — so issue a fresh collect with the same
`review_id`. Either way polling still works as a fallback, and crucially it is no longer
destructive: abandoning a collect leaves the reviewer running and the result collectible (see
[A cancelled request](#a-cancelled-request), and `docs/single-blocking-collect.md` for the
design).

While `cross_model_review_result` is open, the server emits standard MCP
`notifications/progress` every 30 seconds when the client supplied a progress token. The
updates report observed facts rather than a guessed percentage: the current pipeline phase,
elapsed time, how recently the reviewer process was confirmed alive, and how much streamed
output has arrived. Some reviewers emit nothing until completion, so zero output is called
out without treating it as a stall. Clients without MCP progress support still receive the
same snapshot whenever a long poll returns `status=running`.

## Setup

Pick the direction you need:

- **[Claude Code reviewed by Codex](examples/claude-code-reviewed-by-codex/)** — project
  `.mcp.json`.
- **[Codex reviewed by Claude](examples/codex-reviewed-by-claude/)** — project
  `.codex\config.toml`.

This repository has both wired up for itself, so it can be dogfooded in either direction:
[`.mcp.json`](.mcp.json) (Claude asks Codex) and [`.codex/config.toml`](.codex/config.toml)
(Codex asks Claude).

Both come down to two steps: put `cross-review.exe` in `tools\` in your project, and add
one entry to a project config file. Commit both, so a fresh clone of *your* project needs
no setup at all.

Get the executable from the [Releases page](../../releases) — download `cross-review.exe`
and check it against `SHA256SUMS.txt`:

```bash
sha256sum -c SHA256SUMS.txt
``` It is not committed to this repository: a committed
binary cannot be kept honest, because a running MCP server locks the file, CI cannot tell
a stale copy from a current one while the version is pinned, and it embedded the builder's
home directory. `cross-review.exe --version` reports the commit it was built from. To
build it yourself instead, see [Building](#building).

> Codex only loads project-level config for **trusted** folders — the
> `[projects.'c:\path'] trust_level = "trusted"` entry it writes when you first approve
> the folder. And note that `codex mcp list` / `codex doctor` report only global config,
> so they will never show a project-level server; use `cross_model_review_status` from
> inside a session to verify.

## Configuration

Everything is a CLI argument on the MCP server entry, so the project's config file is
the single source of truth. There is no config file of our own to drift out of sync.

```
--reviewer <claude|codex>   Which CLI reviews. Required. Pick the model that is NOT
                            the calling agent. Repeat it to configure a fallback chain
                            (see below); --model/--effort/--bin bind to the --reviewer
                            before them.
--model <id>                Reviewer model. Pin the full id, never an alias.
--effort <level>            claude: low|medium|high|xhigh|max
                            codex:  low|medium|high|xhigh|max|ultra
--bin <path>                Reviewer CLI path, if not on PATH.
--level NAME:MODEL:EFFORT    Declare a named review-depth preset the caller can pick per review
                            (e.g. fast:gpt-5.6-luna:high). Repeatable; binds to the --reviewer
                            before it. The caller selects one with the `level` tool argument at
                            start; it is fixed for the session. See "Review levels" below.
--default-level NAME        Which declared --level applies when a review omits `level`. Optional;
                            unset falls back to this entry's --model/--effort.
--min-usage-remaining <n>   Proactive gate (codex only): skip this entry when its last-observed
                            usage remaining is known and below n% (1..=100), before spending a
                            call. Optional; unset never gates. See the usage-remaining gate below.
--min-usage-status <lvl>    Proactive gate (claude only): claude reports a categorical usage
                            status, not a percentage. 'ample' skips this entry when the status is
                            a warning or worse; 'warning' skips only when rejected. Optional.
--timeout-seconds <n>       Per-turn budget. Default 1800. Max 86400 (24h); an over-range
                            value is rejected at startup so the deadline arithmetic cannot
                            overflow.
--max-concurrent-reviews <n>
                            Per-process cap on reviews running at once. A backstop against a
                            caller that starts reviews and abandons the polls, not a normal
                            limit; a serial flow runs one at a time. Two servers sharing a
                            state directory admit up to 2x this. Default 8. 0 disables.
--session-max-turns <n>     Refuse to resume a review session past this many turns; the
                            caller starts fresh (fresh=true) or uses a new session name.
                            Default 10. 0 disables.
--session-max-idle-seconds <n>
                            Refuse to resume a review session idle longer than this many
                            seconds. Default 3300 (55m). 0 disables.
--stagnant-session-turns <n>
                            End a session that has gone this many turns without raising or
                            resolving a finding while findings are still open. The turn
                            reports session_stagnant / rebaseline with its findings intact,
                            and later resumes are refused. Default 3. 0 disables.
--block-repair-attempts <n> When a reviewer answers without a usable machine-readable
                            findings block, ask it once more -- same conversation, block
                            only -- instead of discarding the turn. Costs up to n further
                            short calls on a degraded turn. Default 1. Max 3. 0 disables.
--block-repair-timeout-seconds <n>
                            Per-attempt timeout for the above, clamped to --timeout-seconds.
                            Default 180.
--cwd <path>                Review root. Defaults to the server's working directory.
--state-dir <path>          Where named sessions live.
--sandbox <mode>            Codex sandbox policy. Default read-only.
--vcs <auto|git|perforce>   Which version control the capture backend drives. Default
                            auto: git if a .git entry is at/above the working root, else
                            Perforce. Filesystem-only — it never runs p4 to decide.
--diff <spec>               git only. What to capture as "the change".
                            auto|none|staged|HEAD|<rev>. Default auto: supply a diff to a
                            shell-less reviewer and to an isolated Codex evidence service.
--tools / --allow-tools     Override the Claude reviewer's read-only tool policy.
--preamble-file <path>      Replace the built-in reviewer preamble.
--no-preamble               Send the caller's instructions with nothing added.
--allow-reviewer-config     Let the reviewer load project and user configuration.
--no-metrics                Stop recording per-turn token usage. On by default.
--doctor                    Check CLI and auth from a terminal, then exit.
--usage                     Print the recorded usage summary, then exit.
```

Defaults are `claude-opus-4-8` at `medium` and `gpt-5.6-luna` at `max`.

> Pin models by full id, not an alias. `--model opus` resolves to whatever the provider
> currently maps that alias to and can move as releases ship — verified once as
> `claude-opus-4-8`. Pinning the exact id keeps the reviewer fixed to the one you chose.

### Reviewer fallback chain

Repeat `--reviewer` to configure an ordered fallback chain. `--model`, `--effort` and
`--bin` bind to the `--reviewer` before them, and argument order is fallback order:

```
--reviewer codex  --model gpt-5.6-luna    --effort max \
--reviewer claude --model claude-opus-4-8 --effort medium
```

A **fresh** review runs the first entry; if it reports a rate or usage limit
(`RATE_LIMITED`), the server automatically advances to the next entry, and so on. Only a
rate/usage limit falls through — an auth error, a missing CLI, a bad model or a timeout
surfaces immediately, so a real misconfiguration is never masked behind a working
substitute. If **every** entry is rate-limited the review is refused with
`REVIEWERS_EXHAUSTED`, naming each entry tried. A **single** `--reviewer` behaves exactly as
before: one reviewer, and a rate limit surfaces as `RATE_LIMITED`.

There is no automatic fallback — the chain is only ever what the arguments declare. The
reactive trigger is the reviewer actually reporting a limit; a **proactive** trigger is
available on top of it (see the usage-remaining gate below). Same-family fallbacks
(`gpt-5.6-luna` → `gpt-5.6-sol`, say) are honoured as written; the only chain the tool
refuses is one with an *identity-equivalent* entry (same reviewer, model, effort and bin, with
the bin path compared case- and separator-insensitively, as Windows paths are), which
could never be a fallback for the one before it. A misconfigured chain does not stop the
server — it starts and refuses every review with `INVALID_REVIEWER_CHAIN` until fixed.

A re-review resumes the entry that created the session (which may be a fallback), never the
primary, and never falls through — the reviewer's memory lives on one specific reviewer, so a
rate-limited resume is reported as such and the caller chooses `fresh: true` to restart chain
selection. The full design is in
[docs/reviewer-fallback-chain.md](docs/reviewer-fallback-chain.md).

### Review levels

Declare named `(model, effort)` presets with `--level NAME:MODEL:EFFORT`, and the caller can
pick one per review with the `level` tool argument — a fast/standard/thorough knob without an
operator editing config and restarting. Each server entry drives one reviewer direction, so each
config declares that direction's levels:

```
--reviewer codex --model gpt-5.6-luna --effort xhigh \
  --level fast:gpt-5.6-luna:high \
  --level standard:gpt-5.6-luna:xhigh \
  --level thorough:gpt-5.6-luna:max \
  --default-level standard
```

When levels are declared, `cross_model_review` advertises a `level` argument (an enum of the
declared names). The level is resolved **at start** and **fixed for the session**: `--default-level`
(or, unset, the entry's `--model`/`--effort`) applies when `level` is omitted, and a review omitting
`level` is unaffected. Collecting and re-reviewing never take `level`; a re-review keeps the level the
session started on. Passing a *different* `level` on a resume is refused (`INVALID_LEVEL_ON_RESUME`) —
use `fresh: true` to start a new session at another level. An unknown level on a fresh review is
`INVALID_LEVEL`. The start response and `--doctor`/status name the effective pair and the level menu,
so an omitted-level default that differs from the base `--effort` is never silently misreported.

Levels are a per-review selection knob, not reviewer identity: two entries differing only in their
level menu are still one identity, and a session started at a non-default level still resumes (its
persisted pair binds back to the entry that produced it). A level's effort is validated the same way
`--effort` is (a warning, then passed through). On a multi-entry chain, a level applies to the entry
that **starts** the review — which the proactive usage gate may select. A mid-run **rate-limit
fallback always runs at its own base `--effort`**, never the requested level (even if that fallback
happens to declare the same level name); the response and metrics report the pair that actually ran.

### Proactive usage-remaining gate

The fallback chain above is *reactive*: it advances after a reviewer fails. A reviewer entry
can also be gated **proactively** — skipped before it is spawned — when its last-observed
usage remaining is below a configured minimum, so a review lands on an account with capacity
without first spending a call to discover the primary is throttled.

```
--reviewer codex  --model gpt-5.6-luna    --min-usage-remaining 10 \
--reviewer claude --model claude-opus-4-8
```

The two reviewer CLIs expose usage headroom in different shapes, so the flag is
family-specific and a mismatch is a startup error:

- **`--min-usage-remaining <1..=100>`** — Codex, which reports a numeric remaining percentage
  (read from its rollout log). Skips the entry when the last observation is below N %.
- **`--min-usage-status <ample|warning>`** — Claude, which reports a categorical status, not a
  percentage (read from its `stream-json` `rate_limit_event`). `ample` skips on a warning or
  worse; `warning` skips only when rejected.

It is **optional and fail-open**. An entry with no minimum is never gated; and an entry whose
headroom is *unknown* — never observed, aged past its window reset, or an account whose
identity can't be confirmed — is never gated either. The signal is observed during a turn and
persisted per account — and only once that account has been verified to be the one the turn was
authorized for, so a profile home re-logged mid-review files nothing under the account it left. The
gate therefore acts on the freshest prior *verified* observation; the first review
against a never-seen account is ungated because there is nothing yet to gate on. Nothing
proactive happens unless at least one entry sets a minimum: with none, Claude keeps its
buffered output and no usage store is touched. A gate *skip* is likewise provisional on the
account it was measured for: if the chain would be reported exhausted but a skipped reviewer's
profile home now presents a different account, or none that can be read, the skip is stale and the
review is refused as the retryable `REVIEWER_ACCOUNT_CHANGED` rather than a false
`REVIEWERS_EXHAUSTED` (retry re-checks against the current account — plain if nothing ran, with
`fresh: true` if a reviewer had already run; see the failure-code list). If every entry is gated out
(or rate-limited) and no skip is stale, the review is refused with `REVIEWERS_EXHAUSTED`, naming each
entry's reason. `status` /
`--doctor` show each entry's minimum and last-observed headroom. Full design, including the
verified spike that found these signals, is in
[docs/usage-remaining-gate.md](docs/usage-remaining-gate.md).

## The change under review is fetched for you

Most reviews are reviews *of a change*, not of a tree. The isolated Codex reviewer gets the
captured change through its read-only repository evidence service; the Claude reviewer gets
the same capture in its prompt. Left alone, the earlier shell asymmetry pushed work onto the
caller, which had to paste a diff into
`instructions` — spending the *caller's* context on it, missing untracked files entirely,
and, when it is forgotten, getting back a confident review of the current tree rather than
of the change.

So the server fetches it. It is already a process on your machine with a known working
root, so running `git` (or `p4`) here costs the calling agent nothing. Which one it runs is
[`--vcs`](#perforce), `auto` by default; the table below is the **git** backend, and the
Perforce backend is described [below](#perforce).

| `--diff` (git) | What the reviewer is shown |
| --- | --- |
| `auto` *(default)* | `git diff HEAD` + `git status --porcelain` + untracked file contents — for shell-less reviewers and isolated Codex; a non-isolated Codex opt-out still self-serves from its project cwd |
| `none` | nothing; supply your own in `instructions` |
| `staged` | `git diff --cached` + status |
| `HEAD` | as `auto`, regardless of whether the reviewer has a shell |
| *a range* | `git diff <a>...<b>` + status, e.g. `main...HEAD` — two commits, so no working tree and no untracked files |
| *a bare revision* | `git diff <rev>` + status + untracked contents, e.g. `HEAD~3` — that commit **against the working tree** |

The full command is `git diff --no-ext-diff --no-textconv --relative <rev> -- .`, and it is
named verbatim in the reviewer's prompt so it can report what it was shown.

For an isolated Codex review, the capture is immutable evidence paged through
`repository_change`; the existence of a shell no longer suppresses it, because that shell
runs from a sterile non-repository directory. Claude has a usable shell only when `--tools`
puts Bash in the session *and* `--allow-tools` permits it. `--allow-reviewer-config` is the
explicit Codex opt-out: it keeps the reviewed repository as process cwd and restores the
old `auto` self-serve decision. It does **not** disable the evidence service, which stays
required in both modes.

The Codex evidence server also exposes `repository_scope`, `repository_list`, literal
`repository_search`, numbered/digested `repository_read`, and bounded Git-only
`repository_history`/`repository_revision` (the last two return `unsupported` for Perforce).
Paths are repository-relative, absolute/parent/device/ADS paths are rejected, and traversal
does not follow symlinks or junctions. Results carry completeness and pagination state.

The recursive scans — the drift stamp and a directory search — cover what Git reports as
tracked (submodule contents included) or untracked-and-not-ignored, rather than everything on
disk. A repository can hold hundreds of thousands of ignored files that are not reviewable
content, and walking them used to exhaust the scan's file budget and refuse the review
outright. Every path Git returns is put through the same component, reparse-point and
regular-file checks the filesystem walk applies, so the narrower scan is not a weaker one.
`repository_read` still reads any file by path, and naming a file as a search `path` searches
it whether or not Git ignores it; `repository_scope` reports the scan's scope in words.

A scan that cannot be completed no longer fails the review. `drifted` is `null` — with
`drift_unavailable` saying why — when there is no comparable stamp, and a listing or search
that hits its file budget truncates with `complete: false` rather than erroring. Drift is an
advisory signal, and losing it costs less than discarding a review. `--doctor` reports whether
drift tracking is available, so a repository that is merely large is distinguishable from a
service that is broken. What still fails a call is a scan that *timed out*: the watchdog
abandons its worker, so there is no partial result to hand back.
Complete page data is returned once in `structuredContent`, with only a concise text summary;
an envelope that still exceeds the transport cap fails that call in-band and leaves the service
available for a smaller-page retry.
Before every model call the parent starts the exact current executable in evidence-only mode,
checks its identity and exact seven-tool closed-schema allow-list, and closes it. Codex then
starts the same entry with `required=true`; handshake, startup, transport, or strict-config
failure returns `EVIDENCE_UNAVAILABLE`, never an evidence-thinned review.

One case is deliberately outside that rule, and it is a narrow one. Codex applies its own
per-call `tool_timeout_sec` to every evidence call, and abandons the call when it expires.
A review that *completed* despite one is returned with the shortfall named in `warnings` --
the same channel a truncated capture uses -- rather than discarded, on two conditions that
are checked rather than assumed: the message must not carry a startup-failure marker, and
**at least one other evidence call in the same turn must have completed**, which is what
distinguishes a service that was answering from one that was never usable. Note the exact
strength of that second check: it proves the service answered *somewhere* in the turn, not
that it answered either side of the abandoned call, and nothing here verifies what the
reviewer did about the gap it was told about in-band. A turn that did not survive the
abandon fails as `EVIDENCE_CALL_ABANDONED`. Everything else -- a closed connection, a dead
channel, or any abandon with no successful call beside it -- is still an unusable service
and still invalidates the review whether or not it completed.

The last two rows are one `--diff` value apart and are not the same thing, because that is
git's own semantics: `A..B` and `A...B` compare two commits, while a bare `A` compares A to
the working tree. So `--diff HEAD~3` carries your uncommitted edits and `--diff main...HEAD`
cannot. Both the caller's description and the reviewer's prompt follow the endpoint rather
than the spelling.

Notes on the edges, because they are where this would otherwise mislead:

- **The capture is scoped to the working root**, not to the whole repository —
  `--relative` and the trailing `-- .`. That matters when `--cwd` is a subdirectory: the
  reviewer's reads are scoped there too, so without it the diff would name `sub/file.rs`
  to a reviewer that can only open `file.rs`. (`git status --porcelain` has no
  `--relative`; its paths stay repository-root-relative, and the prompt says so.)
- **The reviewed repository does not get to choose what runs.** `--no-ext-diff` and
  `--no-textconv` are there because a repository's own `.git/config` can name a command
  for git to execute during a diff. Verified: with `diff.external` set, `git diff HEAD -- .`
  ran the configured command *and* exited 0 having printed nothing — so the capture would
  have reported a clean tree, which is the exact failure this feature exists to remove.
  With the flags, the command did not run and the real diff appeared.

  This is a boundary worth being plain about: git has no "ignore this repository's config"
  switch, so unlike the reviewer — which runs configuration-isolated — the server does read
  the reviewed repository's git config when it captures.

  What is closed is the vectors that *can* be closed by name: `diff.external`, textconv
  drivers, and `core.fsmonitor` (also verified — with `core.fsmonitor` pointing at a
  missing path, `git status` reported `cannot spawn`; `-c core.fsmonitor=` removed the
  attempt). **That list is not a completeness proof.** `filter.<driver>.clean` still runs —
  verified against the hardened command line — and cannot be closed the same way, because
  the driver name comes from `.gitattributes` rather than from a fixed key. All of these
  need write access to `.git/config`, not merely a committed file, so a plain `git clone`
  of a hostile repository does not reach them; an archive or a zip does. If that is not a
  trade you want, `--diff none`.
- **git is resolved from PATH, never from beside the executable.** Windows program
  resolution searches the calling executable's own directory *first*, and this binary is
  meant to be vendored into the repository it reviews (`tools\cross-review.exe`). Verified:
  a stand-in `git.exe` next to the caller was run in preference to the real git; one in the
  child's working directory was not. So a committed `tools\git.exe` would otherwise have
  executed as you.
- **Untracked files ride with every mode whose diff endpoint is the working tree** — `auto`,
  `HEAD`, and a bare revision such as `HEAD~3`, which is why that row of the table differs
  from the range above it. They are the case a diff structurally cannot cover, so a review
  against the tree needs them; a `staged` diff or a two-endpoint range named a specific set
  of changes, and an untracked file is not in either. An untracked symlink or junction
  resolving outside the working root is skipped and reported, so this cannot route around
  the reviewer's read confinement.
- **An empty diff is still reported**, explicitly, as an empty diff — and, for the
  working-tree modes, with the reason it might be empty. A reviewer told nothing reviews
  the current code and calls that a review of the change; a reviewer told only "empty"
  reports there was no change, which is wrong in the commonest flow of all, where the work
  has already been committed. Use `--diff <range>` for that.
- **Truncation is stated in the prompt**, for the diff, the status listing and each
  untracked file. Caps: 400 KB diff, 200 KB untracked contents total, 60 KB per file, 50
  files included, 200 paths examined, 20 per-file lines of "what was left out". Binary
  files are not included, and are named individually as far as those 20 lines reach — past
  that they are counted rather than named. Files are read up to their cap rather than read
  whole and then cut, and a truncated file names the cap that actually cut it — the read is bounded
  by whatever is smaller, so calling it the per-file cap would claim a 200-byte file is
  over 60 KB. That last cap covers the per-file lines only: the statements about the
  capture *itself* — the listing stopped early, files were dropped for want of budget —
  are established only after every per-file line has had its chance at a slot, so they are
  carried separately and the cap cannot suppress them. A silently short diff would be
  worse than none.
- **The capture is labelled as evidence, not instructions**, for the same reason CLAUDE.md
  is — a diff from a repository you do not trust is a prompt injection surface. File
  contents go inside a fence long enough that nothing in them can close it early, and
  untracked *filenames* are stripped of backticks and control characters before they are
  interpolated into a heading.
- **A `--diff` value can never become a git option.** Anything starting with `-` is
  rejected at startup, because `git diff --output=<file>` writes. Git's revision-*set*
  shorthand — `^!`, `^@`, `^-` — is rejected there too: those are two-endpoint ranges
  containing no `..` to detect, so they would be read as working-tree comparisons and change
  what the reviewer is shown. Verified: with a tracked file dirty, `git diff HEAD^!` reported
  5 files and `git diff HEAD~1` reported 6. Parent notation (`HEAD^`, `HEAD^^`) is untouched.
  So is the reverse case, `:/<pattern>` containing `..`: git splits a revision on the first
  `..`, so `:/fix..HEAD` is a range whose left endpoint is a commit-message search, and
  nothing distinguishes it from a search for a pattern that contains `..`. Verified too —
  `git rev-parse ':/fix..HEAD'` returned two endpoints, and `git diff ':/fix..HEAD'` ignored
  a dirty file that `git diff ':/fix'` picked up. A brace-scoped search (`HEAD^{/a..b}`) is
  unambiguous and stays legal, because the braces say where the pattern ends.
- **The whole capture shares a 60-second budget**, not one timeout per command, so a wedged
  repository cannot spend four independent timeouts. The budget bounds the git commands
  themselves; each invocation can additionally spend up to the 10-second output drain grace
  after its child is reaped, which is outside the shared deadline. In practice that is
  unreachable — the job object closes the pipes and collection returns at once — so it bites
  only if the job object could not be created or something outside it holds a handle.
- **A capture that was configured and did not happen is reported to the caller**, as a
  warning alongside the review: no local `main` for a pinned `main...HEAD`, a working root
  that is not inside a git work tree, git missing from PATH, or any **capture-level** part of
  it that ran short — the qualifier is load-bearing, and the next two paragraphs are what it
  means. The reviewer is told it has no diff, but the caller is the party that asked for a
  review of a change, and a review of the current tree returned in silence reads exactly
  like the review it asked for. Nothing was ever promised for `--diff none`, or for `auto`
  with a reviewer that has its own shell, so those stay silent.

  So does a capture-level *bound*, not just a part that failed to run. If the untracked
  listing stopped early — at either the included cap or the examined one — or files were
  dropped because the total content cap ran out, the caller is told, because that is the one
  thing it cannot infer from what came back: a review made against a listing that stopped at
  path 200 otherwise reads exactly like one made against all of them. A truncated diff or
  status listing warns for the same reason, and the status one is the sharper: under
  `--diff staged` the dirty-tree check reads it line by line, so a path past the cut cannot
  report the tree as differing from the diff. Neither fires on an ordinary call — it takes
  400 KB of diff or of status to reach them.

  What is bounded *per file* stays in the prompt only: the omission notes — this file is
  binary, unreadable, resolves outside the root — and an untracked file cut short at the
  60 KB cap or at what was left of the total, which is marked where its contents are shown
  and nowhere else. That is a policy judgement rather than a claim about the code: those
  files were all reached by the listing, so the reviewer has their shape, and keeping them
  out of the warnings is what makes a warning mean the capture itself was short.

### Perforce

`--vcs perforce` (or `auto` in a workspace with no `.git`) captures an **explicit list of
changelists**, named **per call** in the `cross_model_review` `change` argument —
`"change": "43650"`, `"43650,43651"`, or `["43650","43651"]`. There is deliberately no
default and no "all opened": Perforce workspaces are large and a reconcile of one is slow,
so the change is always the changelists you name. `change` is **required** for a Perforce
review — a call without it is refused, not silently turned into a review of the current
tree — and it is named per call rather than baked into the server entry precisely because
the changelist under review moves from one review to the next. A review session binds to
its changelist *set* on first use; a resume that names a different set is refused (use
`fresh: true` to switch), while the pending contents are re-captured every turn because a
pending changelist mutates.

**Client and charset are derived, not assumed.** `p4` is resolved from PATH and run in the
working root. If `P4CONFIG`/`P4CLIENT` already resolve a client whose root contains the
working root, it is used as-is; otherwise the client is derived the way the Perforce MCP
does — `p4 info` for the host and user, then `p4 clients -u`, keeping the clients whose
`Host` matches this machine (an empty `Host` is a wildcard) and whose `Root` is an ancestor
of the working root, longest root winning and a tie refused. Every capture command then runs
bound to that client with a global `-c <client>` (in the global slot, so it does not collide
with a subcommand's own `-c`, e.g. `opened -c <changelist>`) and `-C utf8` — the `-C utf8`
is mandatory, not hygiene: the server is unicode-mode and the machine charset is `auto`,
which corrupts `unicode`-filetype files. The resolved client and root are printed in the
reviewer's prompt so a wrong one is visible.

> **Known limitation — `AltRoots` and `Root: null`.** Derivation ranks on the *primary*
> `Root` reported by `p4 clients`, so a client whose effective root on this host comes from
> an `AltRoots` entry (or a `Root: null` workspace) is not matched. Usually that just fails
> to find a client — a loud error pointing you at `P4CLIENT`/`P4CONFIG`, whose `p4 info`
> path *does* resolve the effective (AltRoot) root and is checked first. The one case it does
> not cover is two of the user's clients whose effective roots overlap at the working root
> (one via a primary root, one via an AltRoot): derivation would pick the primary-root one
> and could pick the wrong workspace. Set `P4CLIENT`/`P4CONFIG` explicitly in that setup.

Per changelist the reviewer is shown a **basis banner**, the changelist description (fenced
as evidence, never as instructions), a diff, a listing of the affected files with their
depot and working-root-relative paths, and the contents of files opened for add:

| Changelist | Basis | Diff | New-file contents |
| --- | --- | --- | --- |
| pending, unshelved | **workspace** — the diff (`p4 diff -du` of opened edits) compares the workspace to the depot, so it matches the files you can read | opened edits | files opened for add, read from disk |
| pending, shelved *(opt-in)* | **shelved** — `p4 describe -S -du`, the server-side shelf, which need not match the tree; requires `include_shelved: true` | the shelved files, filtered per file | (none) |
| submitted | **server revision** — `p4 describe -du`; the live tree may be a *different* revision, and the reviewer is told so | the whole changelist, filtered per file | (none) |

**Shelved content is opt-in.** A pending changelist with nothing open in this workspace (it
is shelved, or belongs to another client) is reported as incomplete by default, because
shelved files are often work-in-progress checkpoints rather than the change to review. Pass
`include_shelved: true` to pull the shelf with `p4 describe -S -du` instead — which also
makes a teammate's shelved changelist reviewable, since the shelf is server-side.

Two things are stated plainly rather than glossed, because getting them wrong would let the
reviewer trust the wrong tree:

- **Completeness is separate from basis.** A pending diff matches the files you can read,
  but says nothing about files edited without `p4 edit` (not detected — reconcile is too
  slow to run), files opened in *other* changelists, or any other workspace change. Any
  changelist can also be **incomplete** — permission-limited, an out-of-root file dropped,
  truncated output, or a pending changelist with nothing open here and `include_shelved` off.
  Each is labelled per changelist, and the requested / captured / skipped changelists are
  listed in the prompt, not only in the caller's warnings.
- **Read confinement differs by basis.** A Perforce client view can map depot files to disk
  outside the working root, so out-of-root content is dropped — the capture never contains
  what the reviewer's own `Read(./**)` scope could not. For a *pending* (unshelved)
  changelist that is process-level: paths are filtered *before* any file is read. For a
  *submitted* or *shelved* one it is prompt-level only — `p4 describe` returns the whole
  changelist server-side, so out-of-root bytes reach this process and are dropped before
  rendering, not before being read. The guarantee there is "not shown to the reviewer", not
  "not read by the server".
- **The reviewer is told the truth about its own reach.** The server-side capture is the
  source of truth because the reviewer usually *cannot* fetch a Perforce change itself: `p4`
  needs to reach the server, which a read-only Codex sandbox denies (and the reviewer's own
  `p4` would be client-less here anyway). So the "you can inspect the history yourself"
  prose is gated — it is shown only when the sandbox grants network — and otherwise the
  reviewer is told to rely on the captured change. git history is local, so it is unaffected.

The Perforce threat surface is **narrower** than git's, and the README says so rather than
claiming parity: there is no committed-config execution analog. `p4 diff -du` forces p4's
internal diff, so `P4DIFF` (the external-diff variable, Perforce's `diff.external`) is not
consulted, and `P4MERGE`/`P4DIFFHTML`/`P4EDITOR` are removed from the child's environment as
defence in depth; no `p4 resolve`/`merge`/`print` is ever run. That is not general config
isolation — a `P4CONFIG` file in a parent directory still governs which server and client
`p4` talks to, which is unavoidable because it is how the client resolves at all. Every
filespec handed back to `p4` comes from `p4`'s own canonical output, so nothing is
constructed from a literal name and there is nothing to escape; the only external input is
the changelist *numbers*, validated numeric.

## Re-reviewing after you act on feedback

Sessions are named, and you choose the names. Calling `cross_model_review` again with
the same `session` resumes that reviewer's conversation, so it still remembers what it
told you and reports what is now resolved and what is still open.

```
cross_model_review(session: "auth-refactor", instructions: "First pass at the token
                   refresh path. Focus on the race between refresh and revoke.")
... act on the findings ...
cross_model_review(session: "auth-refactor", instructions: "Addressed findings 1 and 3.
                   I disagree with 2 because the lock is already held; check that.")
```

The name-to-session mapping is stored on disk, so a review survives an MCP server
restart. Pass `fresh: true` when earlier findings would only mislead.

### Resuming is not free, and the cost is now visible

A resumed turn re-sends the whole conversation, so turn six of a session costs more than
turn one did — and a "turn" is not one model call. The reviewer runs an agentic loop, and
every iteration of that loop re-reads everything accumulated so far. Measured on this
project's own reviews: around nine model calls per turn, with the context those calls read
growing from ~190k tokens on turn one to ~970k by turn six.

Both CLIs report their token usage and the server used to discard it. It is now recorded,
one JSON object per finished turn, in `usage-<machine>.jsonl` in the state directory — and
summarised on every completed review, in `cross_model_review_status`, and by `--usage`:

```
cross-review.exe --reviewer claude --usage
```

```
turns:         5 (0 failed, 0 re-run after an expired session)
input tokens:  2,320,250 total = 1,470,000 cache-write + 850,000 cache-read + 250 fresh
output tokens: 97,000
model calls:   43 over 5 turn(s) (8.6 per reporting turn)
wall time:     75m total (15m per turn)
reported cost: $14.07 over 5 turn(s) ($2.81 per reporting turn)

resumed turns by gap since the previous turn:
     2  5m to 1h (past the 5m lifetime, inside the 1h one)
     1  under 5m (inside either lifetime)
```

Three columns earn their place:

- **Cache-write against cache-read.** Cache reads are billed at a fraction of the input
  rate; cache writes are billed *above* it. A large cache-write figure is the expensive
  kind of traffic, and it is what a total spend number hides.
- **The gap since the previous turn.** Only interpretable next to the cache split, which
  is why they sit together: a turn that re-read its history cheaply and one that paid to
  write the whole conversation back are indistinguishable in a cost total. The buckets sit
  on both documented cache lifetimes — an hour on a subscription, five minutes on an API
  key or cloud provider — because the server cannot see which is in force.
- **Model calls per turn.** The multiplier between "one review" and what it actually cost.

Each machine writes `usage-<machine>.jsonl`, and the reader takes every `usage*.jsonl` in
the state directory. Rolling several machines up together is therefore a matter of copying
their logs into one directory and pointing `--state-dir` at it. The machine name in the
filename is a convenience, not a guarantee: two machines sharing a hostname — or both
falling back to `usage-unknown.jsonl` when the environment does not supply one — produce
the same filename, and copying one over the other loses data. Check for a name clash
before copying, and rename if you find one. Nothing is uploaded and no CLI is launched: `--usage` only reads
the local logs, and does not even create the directory it reads from.

The log is unbounded on purpose. Its value is the comparison over time, and a rotation that
dropped the oldest records would quietly break exactly that — so reports stream it rather
than loading it: the number of records makes no difference to how much is held in memory,
though the list of log files in the directory does. The per-session ranking is capped for
the same reason; turns past the cap still count toward every total and the report says how
many lost their own row. Delete it
yourself when you no longer want the history; `--no-metrics` turns the recording off.

Records carry a schema version. One written by a different version is skipped and counted
rather than guessed at, and the report says how many were left out — reading an older
record would turn figures that were never reported into asserted zeroes.

The two reviewer CLIs count differently, and the server normalises rather than assuming.
Claude reports per turn, with `input_tokens` meaning the *uncached remainder* so the three
input figures sum to the prompt. Codex reports the whole thread's running total on every
turn, with `cached_input_tokens` as a *subset* of `input_tokens` — so its figures are
converted to Claude's convention and differenced against the previous turn before being
recorded. Both were verified against `codex exec --json`, not assumed; getting them wrong
inflated this project's own recorded usage by about elevenfold.

Unreported figures stay unreported rather than becoming zeroes — Codex publishes no
cache-write count, so that column reads `not reported` for Codex turns and any total drawing
on one is shown as a floor (`at least ...`). A zero there would be an assertion sitting next
to Claude's measured value. Averages divide by the turns that actually reported the figure,
and name that denominator, so a rollup mixing the two reviewers cannot read as though every
turn had been measured.

## When the reviewer skips its machine block

Every review carries a machine-readable envelope alongside its prose: a verdict, findings with
stable ids, and a `converged` signal a loop can stop on. The reviewer produces that structure
itself, in a delimited block the server extracts and validates — the server never parses prose for
a verdict. Occasionally a reviewer writes a perfectly good review and simply omits the block, or
emits one that will not reconcile against the ids it was handed. That is issue #63, and untreated it
throws away a 5–20 minute review: no verdict, no findings, and a coverage break that the caller can
only recover from by escalating and starting again.

So the server **asks once more, in the same conversation, for the block alone** — naming exactly
what was wrong with the one it got, and telling the reviewer explicitly not to re-read the code or
revise anything. A recovered turn is an ordinary structured turn, flagged `block_repair:
"recovered"` with a warning so nothing is hidden.

What that costs, and what it cannot do:

- Up to `--block-repair-attempts` further short calls (default 1, max 3), each bounded by
  `--block-repair-timeout-seconds` (default 180, clamped to `--timeout-seconds`). Only on a degraded
  turn. `--block-repair-attempts 0` restores the previous single-shot behaviour exactly.
- **A failed repair never fails the review.** Any failure — spawn, timeout, rate limit, cancel, a
  second unusable block, a repair that answered under a different conversation — returns the same
  degraded envelope the turn would have returned anyway, flagged `block_repair: "failed"`.
- **It does not heal a broken ledger.** Coverage is a one-way state machine: a repair stops *this*
  turn from breaking coverage, but a session already `legacy_uncovered` or `needs_rebaseline` stays
  that way, structured counts and all.
- It cannot start a conversation. A fresh run that reported no session id has nothing to resume, and
  the server does not open a new one to ask for a block — a block describing a review that never
  happened is worse than none.

### Reading a completed result

Switch on `outcome`:

| `outcome` | What it means | What to do |
| --- | --- | --- |
| `converged` | The machine contract passed. | Stop. (It certifies the structured contract, not that a human read the prose.) |
| `changes_requested` | Findings are open, or the verdict and the count disagree. | Act on `findings`, re-review the same session. |
| `escalate` | The reviewer blocked, or withheld a clean approve. | A person decides; re-reviewing will keep producing this. |
| `rebaseline` | This session cannot continue — coverage is broken, the turn was not durable, the ledger is over budget, or the session stalled. | A person decides, then starts a fresh review carrying the still-open findings. |

**A stalled session ends.** If a session goes `--stagnant-session-turns` turns without any finding
being raised or resolved while findings are still open, the turn reports
`non_convergence_reason: session_stagnant` and `outcome: rebaseline`, and later resumes are refused.
Read it as *this loop has stopped producing anything*, and nothing more: it is not a claim that
anything went unexamined, and it changes no finding — every open finding is still open, `open_count`
is unchanged, and the full ledger comes back with the turn so it can be carried into a fresh review.
The reviewer is never told this gate exists, so it cannot be tempted to manufacture a resolution to
stay alive. See [`docs/finding-liveness.md`](docs/finding-liveness.md), which also records why the
*per-finding* version of this — forcing a re-examination of a specific carried finding — is not
buildable and was closed won't-fix.

`structured: false` means there is no machine record for this turn: `findings` is empty because
nothing is in it, not because nothing was wrong.

**`structuredContent` is never silently poorer than the text body.** A client that reads only the
structured channel — several MCP clients do — gets everything the rendered text shows that bears on
how much weight the review deserves:

| Field | What it answers |
| --- | --- |
| `review_prose` | What the reviewer said outside its findings list: its reasoning, and why a finding is still open. Present on **every turn that ran**, whatever the outcome; `null` only when no reviewer ran. Capped at 16,000 characters with `review_prose_truncated` saying when the copy is short — the whole thing is always in the text body. |
| `captured` | What change the reviewer was actually shown: the resolved range, the counts, whether the diff was truncated, whether the capture was complete. |
| `denial_count`, `denial_count_is_floor`, `denials` | How many commands the reviewer was refused, and whether that number is exact or a lower bound. |
| `warnings` | Everything qualifying the turn — a partial capture, a turn that could not be saved, an account that moved mid-review — as one list, in the order the text renders it. |
| `resumable` | Whether calling `cross_model_review` again with this session continues it or starts over. |
| `reviewer`, `resumed`, `usage`, `disposition` | Which entry ran, whether it continued an earlier review, what it cost, and what was sent this turn. `reviewer` is `null` when no reviewer ran. |

This exists because the reverse cost a real review: `review_prose` used to be `null` on any turn with
a clean machine record, so an `approve_with_comments` verdict — whose entire content *is* the
comments — returned `outcome: escalate` ("a person decides") with nothing for that person to read
(issue #73). The same gap hid the two failures this README warns about elsewhere: a capture widened
by a stale local `main`, and a reviewer that spent its turn on commands its policy refused. Both read
as an ordinary successful review to a `structuredContent`-only client. A review that is quietly
thinner than it looks produces a false approval rather than a lost review, which is the one failure
here that is not merely retryable.

### Which findings were actually re-examined

On a resumed turn the reviewer is shown the findings it already raised. It reports a status for
**the ones it re-examined**, and nothing more:

- **An omitted finding is carried, not resolved.** Saying nothing about a finding leaves it exactly
  as recorded — same status, same content — and is the correct report when the reviewer did not look
  at it again. It is not an error and does not cost you the turn.
- **`last_verified_turn` says when a finding was last actually looked at.** Compare it with `turn`:
  equal means the reviewer examined it this turn, behind means it was carried. The server derives
  this from whether the reviewer reported the id at all, so it is not something a reviewer can claim
  without doing.

That distinction is the fix for issue #62. Previously the protocol required a status for *every*
finding every turn, so a reviewer with nothing new to say had to restate `open` regardless — and a
finding it had genuinely re-checked was indistinguishable from one it had merely echoed. If a
finding is stuck open, check `last_verified_turn` first: if it is behind, say so in your next
`instructions` and name the id.

**A resolved finding is closed.** It is never asked about again and cannot reopen. If a reviewer
finds it broken again it raises a *new* finding, which may carry `regression_of` naming the closed
id — advisory provenance only, dropped silently if it does not name a finding this session resolved.
There is no `regressed` status.

**`converged` does not mean everything was re-examined on the approving turn.** It means no finding
is open and the reviewer approved. Every finding was examined on the turn it resolved — a status
only moves when the reviewer reports it — but a session can converge without a final sweep, and a
recurrence of something already closed can be missed the same way any defect can be missed.

## What the reviewer can and cannot do

The reviewer cannot modify anything. Whether it can *read* outside the project differs by
direction, and the asymmetry is worth knowing before you point either one at code you do
not trust:

| | writes | reads outside the project | shell |
| --- | --- | --- | --- |
| **Claude reviewer** | denied (no such tool) | **denied** | none |
| **Codex reviewer** | denied (OS sandbox) | **not confined** | yes |

- **Codex reviewer** — `--sandbox read-only`, plus the same policy restated as
  `-c sandbox_mode` on resumed turns, since `-s` exists only on the fresh-session form.
  Verified: a write was refused on turn 1 and again on turn 2 of a resumed session. What
  this does *not* do is confine reads — a read-only sandbox prevents writes, and the Codex
  reviewer keeps shell access, so it can read anything your account can and quote it into
  the review text, which is returned to the caller verbatim. If you are reviewing a
  repository you do not trust, prefer the Claude direction.

  Repository inspection does not depend on that shell. The reviewer is directed to the
  bounded evidence tools for discovery, reads, the selected change, and Git history. The shell
  remains present for exceptional information outside that vocabulary and is still not an
  unrestricted non-interactive command runner; policy refusals remain final and are surfaced.

  **The write boundary is the OS's.** This was previously hedged as "the CLI's, unless you
  have checked further", because only the refusal had been observed and not what refused
  it. It has now been checked, against Codex 0.145.0 on Windows 11. `codex sandbox` runs a
  command under the same sandbox with **no model and no agent policy layer in the loop**,
  which is what makes it a usable probe: under `-c sandbox_mode="read-only"`, a redirect to
  a new file was refused with `Access is denied.` — reported by `cmd` itself, from a Win32
  access-denied, and no file appeared. That held for a target outside the project and for
  one inside it, so it is not the workspace boundary doing the work. Codex's own help calls
  this the "Windows restricted token sandbox", and the refusal survives
  `-c windows.sandbox="unelevated"`, which is the backend the reviewer actually gets —
  `--ignore-user-config` skips the `[windows] sandbox` setting in your own config, so the
  boundary does not depend on it. Seatbelt and Landlock are the macOS and Linux
  implementations; Windows has its own, and it is enforcing.

  **Read confinement was investigated and there is nothing here to enforce it.** Codex
  0.145.0 does have a filesystem permission surface — `[permissions.<name>.filesystem]`
  maps a path, a glob, or a special root such as `:workspace_roots` to `read`, `write` or
  `deny` — and it does narrow the policy the *model* is shown. Verified with
  `codex debug prompt-input`: the default `read-only` mode renders
  `<file_system type="restricted"><entry access="read"><special>:root</special></entry>`,
  i.e. read the whole filesystem, and
  `-c 'permissions.p.filesystem={ ":workspace_roots" = "read" }' -c 'default_permissions="p"'`
  narrows that to a single `<path>` entry naming the project root. It does not follow that
  anything enforces it. The same profile passed to `codex sandbox` as `-P p` still read a
  file outside the project, as did a profile carrying an explicit `deny` entry on that exact
  directory, on both the unelevated and the elevated backend. That is a negative result for
  `codex sandbox -P` specifically; the reviewer runs `codex exec`, which builds its sandbox
  by its own path, and enforcement there was not separately probed. Either way it is not
  wired in: the reachable evidence is a policy that renders and a read that succeeded
  anyway, and wiring that in would make the table above read *confined* over a boundary
  nobody has seen hold — the failure mode this file exists to avoid.

  Two more paths were ruled out rather than left unexamined. `--sandbox-state-readable-root`
  is named for exactly this, but it is a flag on `codex sandbox` that will not run without
  `--sandbox-state-json` — internal plumbing for the app server, not a surface `codex exec`
  exposes, so what it would enforce was never reached. And
  `permissions.filesystem.deny_read`, which the binary describes as requiring the elevated
  backend, belongs to machine-managed enterprise requirements rather than user config; an
  MCP server editing machine-wide policy to confine its own child is not a trade worth
  making. That leaves the option the issue named — take the shell away, as was done for the
  Claude reviewer — and it is not a switch. In 0.145.0, `codex exec` has no flag for it and
  the `[tools]` table carries two keys, `web_search` and `experimental_request_user_input`,
  neither of them the shell (from `--help` and the shipped binary's own config schema, so
  treat it as this version's surface rather than a promise about the next). Closing the
  exposure would therefore mean closing the direction: dropping the Codex reviewer, or
  driving it through something other than `codex exec` that has a tool surface to confine.
  The shell remains a read-exposure trade-off, but it is no longer what buys routine repository
  completeness: the evidence service supplies list/search/read/change/history/revision under
  fixed bounded operations. The Claude direction remains the confined one to point at a
  repository you do not trust. If you are weighing the remaining Codex shell exposure the other
  way, it is the whole of the account you can read, not just this project.
  All of the above is one CLI version on one OS; re-check it rather than inheriting it.
- **Claude reviewer** — `--tools Read,Grep,Glob`. Write tools *and* Bash are absent from
  the session entirely, so there is nothing to attempt. Read, Grep and Glob are Claude
  Code's own tools and have no write or execute capability. Each is further scoped to the
  project (`Read(./**)` and likewise for Grep and Glob), because a bare grant is not
  path-scoped and would let the reviewer read any file you can. Verified for all three
  tools and all three escapes: an absolute path outside the project, a `..`-relative path,
  and a directory junction inside the project pointing out of it were each denied, while
  the project root and its subdirectories stayed readable. The scope is deliberately relative — these
  are gitignore-style globs, so interpolating an absolute path would make the path's own
  characters significant, and a project at `C:\work\[ab]` would turn into a character
  class matching its siblings.

The Claude reviewer has no shell by default, and that is a deliberate reversal. Claude's
permission patterns match by **command prefix**, so `Bash(git diff:*)` permits *any*
arguments to `git diff` — including `--output=<file>`, which writes. This was verified,
not theorised: with that pattern allow-listed, the reviewer ran
`git diff --output=PWNED_DIFF.txt HEAD~1 HEAD` and created a 354-byte file. `git log`,
`git show` and `git blame` accept `--output` too, and the problem is not specific to git —
read-oriented tools routinely have flags that write files or execute programs (ripgrep's
`--pre` runs an external command). Shell redirection *is* caught by the permission parser
(`git status --short > REDIR.txt` was denied), but that closes only the obvious hole.

A prefix allow-list therefore cannot express "read-only", so the default grants no shell
at all. If you want the Claude reviewer to run commands, opt in explicitly and understand
that it is a soft boundary rather than a guarantee:

```
--tools "Read,Grep,Glob,Bash" --allow-tools "Read Grep Glob Bash(git diff:*)"
```

Any commands the reviewer attempted but was not permitted to run are reported back with
the review, so an analysis thinned by missing evidence is visible rather than silent.

What no shell costs the Claude reviewer is the diff, and that is bought back by the server
fetching it — see [the change under review is fetched for you](#the-change-under-review-is-fetched-for-you).
The reviewer is told which of the two situations it is in, so it never claims to have seen
a diff it was not shown, or reports one missing that is sitting in its prompt.

### The reviewer runs without the project's configuration

The tool allow-list is not the only way a repository can get code to run. A committed
`.claude/settings.json` can define a **hook**, and Claude executes that shell command
automatically — no tool call, so no permission check, no allow-list. Verified: a
`SessionStart` hook committed to a project ran on a plain `claude -p` invocation and
created a file. Reviewing a repository would otherwise mean executing whatever that
repository chose to define, which is precisely backwards for a tool whose job is to look
at code you are unsure about.

So the reviewer runs configuration-isolated by default:

- **Claude reviewer** — `--safe-mode`, which disables hooks, settings, plugins, skills,
  commands and MCP servers while leaving auth, model selection and permissions working
  normally. (`--bare` would also do it but redefines authentication as API-key-only,
  breaking subscription sign-in.) Verified end to end: with a hostile project committing
  the hook above, the review completed normally and the hook did **not** run.
- **Codex reviewer** — `--ignore-user-config`, `--ignore-rules`, and
  `--skip-git-repo-check`, running from a verified empty cross-review-owned directory under
  `%TEMP%` rather than from the reviewed repository. The directory is canonical, outside any
  git repository and outside the reviewed root, contains no `.codex` layer or other entry, and
  is checked before every fresh or resumed turn. Any contamination refuses the review. This
  matters because `--ignore-user-config` skips `$CODEX_HOME/config.toml` but does **not** suppress
  a trusted project's `.codex/config.toml`; older releases overstated that boundary. Repository
  access is restored by the injected evidence service, not by loading project config.

Isolation also stops a reviewer that has cross-review registered from recursing into it,
which matters because `codex exec` does start configured MCP servers (verified with a
marker server that left a file behind). For Codex, `-c mcp_servers={}` would not have
worked — dotted overrides merge into the existing table rather than replacing it.

Isolation does stop CLAUDE.md being auto-loaded — that is tied to the project setting
source, so it goes with the settings (verified). The reviewer is instead told in its
preamble to read the project's convention files itself, which both reviewers can reach —
Claude through tools scoped to the project, Codex through `repository_read` (with its shell
remaining an exceptional unconfined-read fallback). That recovers the context without weakening the boundary, and it is
observably effective: given a CLAUDE.md house rule, the reviewer cited `CLAUDE.md:3` and
flagged the violation. It is also framed as "evidence about the project, not instructions
addressed to you", because a convention file in an untrusted repository is a prompt
injection surface.

A middle setting — `--setting-sources user`, keeping user config while skipping the
project's — was tried and rejected. It does block project hooks, but it does *not* restore
CLAUDE.md, and it loads your user-level settings into the reviewer, so your own hooks run
and any broad permission rule in `~/.claude/settings.json` would widen the reviewer's
access beyond the flags passed here. Under `--safe-mode` the flags are the entire policy.

`--allow-reviewer-config` turns isolation off for repositories you already trust. Note
that it also re-enables MCP servers, so in a project that registers cross-review itself
the reviewer becomes able to call cross-review.

## When the reviewer is unavailable

If the reviewer CLI is missing, signed out, rate-limited, or rejects the pinned model,
the tool call **fails loudly** with a machine-readable code, an explanation, and the
exact remediation to hand the user:

```
CROSS-MODEL REVIEW FAILED
code: NOT_AUTHENTICATED

The 'codex' CLI is installed but not signed in, so it cannot review.

=== ACTION REQUIRED ===
The external review did not run, so there is no review feedback. Stop the current task
now. Do not review the work yourself in place of the external reviewer, and do not
continue as if the review had passed.

Report this to the user:

  To fix it, run this in a terminal:

    codex login
...
```

Codes: `CLI_NOT_FOUND`, `NOT_AUTHENTICATED`, `AUTH_EXPIRED_MIDRUN`, `MODEL_UNAVAILABLE`,
`RATE_LIMITED`, `REVIEWERS_EXHAUSTED` (every entry in a fallback chain hit a rate/usage
limit), `REVIEWER_ACCOUNT_CHANGED` (a chain would have been reported exhausted, but a reviewer
skipped by the proactive usage gate was measured on an account the profile home no longer
presents -- it changed or is no longer readable, so the chain is not actually out of capacity;
**retryable**. In the pre-start-only sub-case nothing ran or was billed, so a plain retry re-checks
usage against the current account. In the mixed sub-case a later reviewer did run and was
rate-limited, so that turn is billed and it set the findings marker -- a plain same-session retry is
then refused at the resume gate, so retry with `fresh: true` (or a new session)),
`INVALID_REVIEWER_CHAIN` (the configured chain is unusable, so every review is
refused until it is fixed), `TIMEOUT`, `POLICY_BLOCKED` (a Codex turn was refused enough shell
commands "by policy" and then went silent on stdout, so it was ended early rather than left to burn
the whole budget to a misleading `TIMEOUT`; the remedy is the read-only evidence tools or the other
reviewer direction, never a wider allow-rule -- see `--max-policy-denials`), `SPAWN_FAILED`,
`REVIEWER_FAILED`, `EMPTY_REVIEW`,
`OUTPUT_TRUNCATED`, `EVIDENCE_UNAVAILABLE`, `EVIDENCE_CALL_ABANDONED` (the reviewer CLI gave up
on one evidence call before it was answered and the turn did not survive it -- distinct from
`EVIDENCE_UNAVAILABLE` because the service was up and the remedy is simply to retry),
`SESSION_NOT_FOUND`, `SESSION_NOT_RESUMABLE`, `CANCELLED`,
`SERVER_SHUTTING_DOWN`, `INTERNAL_ERROR`. Bad tool arguments, a session already busy, a
session refused as not resumable, and too many reviews already running (`TOO_MANY_RUNNING`,
the `--max-concurrent-reviews` backstop) get a plain correction instead, since each is the
agent's own call to make and not something to escalate; so does a tool call the server could
not start a thread for -- neither says anything about the reviewer's state.

A Codex turn that accumulates repeated command-policy refusals is handled two ways, by how far it
gets. If it passes the `--max-policy-denials` threshold (default 4) and then produces no stdout for
`--max-policy-idle-seconds` (default 300), the server terminates it early and reports
`POLICY_BLOCKED` rather than letting it run out the clock -- nothing timed out, the reviewer was
blocked, and the remedy differs. The long idle window is deliberate: `codex exec --json` emits
nothing to stdout while the model is reasoning, and measured max-effort turns stay silent for up to
~110s legitimately, so a shorter window would kill working reviews. Set `--max-policy-denials 0` to
disable the fail-fast, or lower the window for faster fails at lower effort. If instead the turn
reaches the hard `--timeout-seconds` deadline first, it remains `TIMEOUT`, but the message names the
refusal count and advises against simply raising the budget. Successful Codex reviews surface
refused commands as a note, so a review that completed after missing evidence is not presented as
fully checked.

A stale session is refused rather than silently restarted. Before a review is resumed the
server checks that the named session still matches this reviewer, model and working root and
is within the configured turn and idle limits (`--session-max-turns`,
`--session-max-idle-seconds`); a session that fails any check returns `SESSION_NOT_RESUMABLE`
so the caller decides to start fresh (`fresh=true`) rather than being handed a review with no
memory of the work it asked to continue. A reviewer session that has expired out from under a
resume mid-run is the same story a step later: the stale mapping is dropped and
`SESSION_NOT_FOUND` is reported, again pointing the caller at `fresh=true`.

Preflight runs before any model call, so a misconfigured machine fails in seconds
instead of after a minute of billed work.

## Building

Requires Rust (stable, MSVC). Everything else is vendored.

```powershell
.\build.ps1
```

That runs `cargo fmt --check`, clippy with warnings as errors, the tests, and a release
build, then stages `dist\cross-review.exe`. `dist\` is gitignored; the staged copy exists
so this repository's own MCP configs resolve, and as the thing you vendor elsewhere.

The build remaps `--remap-path-prefix` over the cargo and rustup homes, because rustc
otherwise embeds the builder's home directory in the binary for panic locations. It fails
if that path survives, so a published artifact cannot carry it.

### Releasing

Releases are built and published by CI, not from a workstation:

```bash
git tag v0.1.0 && git push origin v0.1.0
```

That runs the full checks, builds with paths remapped, re-verifies the `CLI_NOT_FOUND`
failure contract against the exact binary being shipped, and publishes it to the Releases
page with `SHA256SUMS.txt`. It refuses to publish if the tag and the `Cargo.toml` version
disagree. `workflow_dispatch` runs everything except the publish, as a dry run.

Whether a release is marked "Latest" is declared in `Cargo.toml`, not inferred from the
tag, so an ordinary semver tag can still be published provisionally:

```toml
[package.metadata.release]
channel = "prerelease"   # or "stable"
```

Set it in the same edit as `version`. A `prerelease` release stays off "Latest" and its
notes carry a provisional banner. The workflow refuses to publish when the channel is
missing or unrecognised rather than defaulting: a prerelease that reached the Releases
page as "Latest" would be picked up by every project that vendors the binary.

Released binaries report their provenance: `cross-review 0.1.0 (<commit sha>)`. A local
build says `(local build)` rather than implying a provenance it does not have — the
version alone cannot distinguish two builds, since it is pinned in `Cargo.toml`.

Check a setup from a terminal without starting an agent:

```powershell
.\dist\cross-review.exe --reviewer codex --doctor
```

## Testing

```powershell
cargo test          # unit tests only: no network, no model calls
.\smoke.ps1 -Reviewer codex     # end to end against the real CLI
.\smoke.ps1 -Reviewer claude
```

`smoke.ps1` speaks real MCP over stdio to the built executable and checks the whole
round trip: initialize, `tools/list`, a live review, a resumed follow-up review that
proves the reviewer retained context, the error paths, two cancellation cases — a
`cross_model_review_result` poll cancellation that must leave the response unanswered while
the reviewer keeps running, and an explicit `cross_model_review_cancel` that must leave the
reviewer dead — and that session state landed on disk. The deterministic cancellation
contract lives in the unit tests; the smoke checks are best-effort against real-model timing.
It calls the reviewer model for real, so it costs tokens — it defaults to `--effort low` for
that reason.

Both directions pass against live CLIs.

## Design notes

- **Hand-rolled MCP.** The protocol surface needed is four methods of JSON-RPC 2.0 over
  newline-delimited stdio. Keeping dependencies at `serde` is what makes a 520 KB
  self-contained binary possible.
- **Prompts go over stdin**, not the command line, so a large review request cannot hit
  the Windows command-line length limit or a quoting bug.
- **Sessions on disk, in-flight reviews in memory.** Review ids are per-process; the
  session mapping outlives the process. Finished reviews are evicted once a session has
  more than three of them, or the process more than fifty, newest kept — a review holds
  its full text, and a long agent session doing many reviews would otherwise accumulate
  all of them. Running reviews are never evicted: one is still owed to a caller.

  An evicted `review_id` is still recognised as one this process issued, because ids are
  `rv-<pid>-<counter>` and the counter only increases — so recognition is derived from the
  id's own form rather than from a list of what was discarded. That matters: "this
  finished and was discarded" and "this was never issued" call for different advice, and a
  caller told the second has reason to suspect it mangled the id and will go looking for a
  bug that is not there. Deriving it means the distinction holds for the life of the
  process; a list would have to be bounded, and its oldest entry falling off would turn a
  valid id back into "never issued" precisely where a caller is least likely to question
  it.

  Looking a review up by **session name** cannot draw that distinction, and says so rather
  than guessing. Telling the two apart there would mean retaining every session name that
  ever had a review evicted, which is unbounded in the way the caps exist to prevent —
  and caller-controlled, since the names are. So the session-keyed miss reports both
  possibilities and points at `review_id`, which can tell them apart.

  Two servers can share a project's state directory, so a named session is claimed with a
  cross-process lease held for the whole review, and mutations of the state file take an
  exclusive lock across the read-modify-write. Both locks are the OS's: the lock file is
  opened with a share mode of zero, so exclusion is enforced by Windows and released even
  if the holder is killed.
  That deliberately replaces an earlier version which tracked staleness itself — it could
  steal a lock from a merely-paused process and then delete the new owner's lock.
  Writes go to a pid-unique temp file and then an atomic replace, never unlinking the
  live file first. A session that could not be persisted is reported as a warning with
  the review, and the response then stops inviting a resume that would silently start over.
- **The reviewer runs inside a job object**, so timeout, cancel, and even a clean exit
  that leaves a helper behind all reap the whole process tree. On Windows killing a parent
  orphans its descendants, and an orphan holding an inherited pipe would keep our reader
  threads blocked forever. Output collection is bounded in *time* as well, so a stuck pipe
  degrades diagnostics instead of hanging the review: readers append into a shared buffer,
  so whatever arrived before the deadline is still used rather than discarded.

  It is bounded in size too, at 8 MiB per stream. Note what that does *not* mean: the
  reader keeps reading past the cap and throws the bytes away, because a reader that
  stopped would fill the pipe and block the child for ever — trading unbounded memory for
  a hung review, which is the worse bargain.

  Hitting the cap is always reported, but not always as a failure. When the review cannot
  be recovered from what was kept, it is `OUTPUT_TRUNCATED` rather than `EMPTY_REVIEW` —
  an empty review means the CLI wrote nothing and a retry is reasonable, a truncated one
  means it wrote far too much and a retry will do the same again. When the review survives
  anyway — Codex writes its final message to a file, which our pipe cap cannot affect —
  the review is returned with a warning saying the transcript was truncated. Real
  transcripts are kilobytes, so reaching 8 MiB means something has gone wrong either way;
  the point is that it never passes unmentioned.

  Shelling out to `taskkill` was rejected: it cannot help once the direct child has exited
  and the parent/child links are gone, and invoking it by bare name is an execution hazard,
  because Windows resolves an unqualified executable through the current directory — the
  repository under review — before System32.
<a id="a-cancelled-request"></a>
- **A cancelled request: what stops depends on which request.** `notifications/cancelled`
  always suppresses the response, as the spec requires. What it does to the *review* is not
  uniform, and deliberately so:

  | Cancelled call | Effect on the review |
  | --- | --- |
  | `cross_model_review` (start) | **stopped** — its `review_id` was never delivered, so nobody could ever collect it |
  | `cross_model_review_result` (poll) | **left running and collectible** — only the wait detaches |
  | `cross_model_review_cancel` | **stopped** — this is the tool whose job is to stop it |

  The start-call case is unarguable: a review nobody can collect should not keep billing. The
  poll case is the one that changed. It used to stop the review too, on the reasoning that a
  reviewer nobody waits on burns its budget — but that guess fired mostly on *false* positives.
  A client sends `notifications/cancelled` when its own tool timeout fires, which the server
  cannot tell from a real user cancellation, so a long wait that tripped the client timeout
  destroyed a review the caller fully intended to collect. Now the poll detaches instead: the
  reviewer keeps running, the result stays collectible by `review_id`, and a client timeout
  shorter than the review degrades to polling rather than to lost work. That decoupling is what
  lets the collect cap rise to cover a whole review (see [the tools](#the-tools)). The cost —
  a genuinely abandoned poll leaves its reviewer billing to the budget — is bounded per review
  by `--timeout-seconds`, capped across the process by `--max-concurrent-reviews`, and always
  stoppable with `cross_model_review_cancel`.

  This is a deliberate, documented deviation from MCP's cancellation guidance, which says a
  receiver SHOULD stop processing and *free the associated resources*: a cancelled poll frees
  its own handler but keeps the reviewer and session lease alive on purpose, because the caller
  can still collect the result. `cross_model_review_cancel` is the operation that frees the
  reviewer.

  A per-server `timeout` in the client config should exceed the collect cap so one blocking
  call completes; below it you poll, you do not lose work. Pinning it *overrides*
  `MCP_TOOL_TIMEOUT`, making the hard per-call ceiling explicit rather than inherited from that
  variable's ~28-hour default. The 30-minute idle window for a stdio server is unchanged,
  because a per-server `timeout` acts as a floor on it rather than a cap.

  Ending the poll on the detach path is `registry.wake()`'s job, driven from
  `handle_cancellation`: it publishes under the same mutex the condition variable waits on —
  exactly as shutdown does — after the request's cancelled flag is set, so the parked waiter
  re-checks that flag and returns without the review having to reach a terminal state. A
  suppressed response therefore does not park a handler thread, and the review is left alone.
- **Closing stdin ends a long poll immediately.** `serve` joins every in-flight `tools/call`
  before returning, because exiting mid-flight dropped responses a client was still waiting
  on. A poll parked on a review has no deadline worth honouring once the client is gone, so
  stdin closure sets a shutdown flag that waiters observe alongside their own deadline —
  otherwise a long `wait_seconds` call (now up to the full review budget) held the process
  open for the rest of it and then wrote to a stdout nobody was reading. The flag is published under the
  same mutex the condition variable uses; set outside it, a waiter that had read it but not
  yet parked would miss the wake and sleep out its budget regardless. A review that reaches
  a terminal state in that same moment still returns its result rather than a running
  snapshot, and a poll cut short says the server is shutting down instead of inviting a
  retry that nothing will answer.

  A start call still in flight is refused rather than granted a reprieve: `try_start`
  rejects with `SERVER_SHUTTING_DOWN` once the flag is up, under the same lock that raises
  it, because a review registered after stdin closed could never be collected — the server
  drains the calls already running and then exits, so starting one would bill a reviewer
  turn for a result with nowhere to go and answer "review started" to a caller that will
  never see it. A handler can genuinely arrive there that late: it may have spent the
  interval in the auth preflight or waiting for the session lease.

  What it costs: an in-flight review is abandoned rather than allowed to finish. Worker
  threads are never joined, so one that was seconds from persisting its session mapping
  loses it. Its reviewer process is left to the job object, which is best-effort rather
  than guaranteed — job creation and assignment both continue with a warning when they
  fail, so a reviewer that never joined a job is not reaped by the handle closing. Nothing
  cancels the worker on this path, so no child is explicitly killed either. Holding a
  process open for minutes on the chance of salvaging a session mapping is still the worse
  trade.
- **stdout is protocol traffic only.** Diagnostics go to stderr.
