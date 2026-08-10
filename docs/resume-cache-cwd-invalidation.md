# Resume cache invalidation from the reviewer's working directory — investigation and proposed fix

Status: **investigation and implementation both merge-gate approved.** This document records a
diagnosed cost regression in the Claude-reviewer direction, the experiments that isolated its
cause, and the fix — now implemented on branch `feat/reviewer-neutral-cwd`. Per this
repository's own rule both went through the `cross-review` gate (Codex, gpt-5.6-luna,
effort=max). The **investigation** took rounds 1–3 REQUEST CHANGES (six, seven, then six
findings — all accepted), rounds 4–5 APPROVE WITH COMMENTS (five, then one), round 6 **APPROVE**.
The **implementation** took rounds 1–2 REQUEST CHANGES (three, then two findings — all accepted)
and round 3 **APPROVE** ("no remaining correctness or security defect"). [Review
history](#review-history) records each round.

## Symptom

A Codex session using the Claude reviewer (`claude-opus-5`, effort `medium`) spent roughly
**40% of a 5-hour subscription window over five review turns on a documentation-only PR**
(`pr-616-remastered-presentation-policy`). That is far more than a docs change should cost,
and the diff being documentation ruled out "the code was just large" as a complete
explanation.

The per-turn usage, from the reviewer's own accounting
(`usage-unknown.jsonl` in the state directory; the `unknown` host tag is how the
Claude-reviewer-under-Codex direction labels itself, since the restricted token does not
inherit `COMPUTERNAME`):

| turn | gap | model calls | diff sent | cache-write | cache-read | output | cost |
|-----:|----:|------------:|----------:|------------:|-----------:|-------:|-----:|
| 1 (fresh) | — | 36 | 368,505 B | 205,674 | 5,546,679 | 39,758 | $5.92 |
| 2 (resume) | 2166 s | 5 | 50,539 B | 249,238 | 489,667 | 17,768 | $3.18 |
| 3 (resume) | 270 s | 4 | 23,944 B | 276,850 | 550,184 | 12,661 | $3.36 |
| 4 (resume) | 381 s | 3 | 14,550 B | 296,282 | 296,316 | 8,216 | $3.32 |
| 5 (resume) | 116 s | 2 | 9,854 B | 307,336 | 310,774 | 4,108 | $3.33 |
| **total** | | 50 | | 1,335,380 | 7,193,620 | 82,511 | **$19.11** |

Two things stand out. Turn 1 is 67% of the session's input — a 36-call agentic exploration
of a 368 KB diff. And on the follow-ups, **`cache-write` climbs (249K → 307K) while the diff
shrinks (50 KB → 10 KB)**: the cost is not tracking the change under review. Each follow-up
is paying to re-cache something the size of the accumulated conversation.

## How resume is supposed to work

The Claude adapter resumes a named session with `claude -p --resume <session_id>`
([`src/reviewer/claude.rs`](../src/reviewer/claude.rs)). The design can avoid re-sending the
diff: the session store keeps the last captured `head_sha`/`base_sha`
([`src/session.rs`](../src/session.rs)), so a follow-up can capture only `<head_sha>..HEAD`
rather than the whole range — "the reviewer, which already holds the earlier full diff in
its resumed conversation, is not re-sent a near-duplicate every turn."

That delta path is **conditional**, not automatic. Incremental resume is on by default
(disabled with `--no-incremental-resume`, [`src/config.rs`](../src/config.rs)), but sending a
delta rather than a full capture requires *several* conditions, not just a stored pair: the
diff mode must be a HEAD-anchored range, and the backend then validates both stored ids, that
the current base still equals the stored base, that HEAD and base resolve this turn, and
ancestry — any of which failing falls back to a full capture
([`src/vcs/git.rs`](../src/vcs/git.rs) decisions `Disabled`/`ModeNotDeltable`, gated in
[`src/tools.rs`](../src/tools.rs)). The controls in this document ran in the default
working-tree mode and logged `disposition: full-by-design:mode-not-deltable` — the delta was
*not* active in them. So in production the shrinking `diff_bytes` across the follow-ups is
not proof the delta fired; it may simply be the change under review shrinking as the PR was
refined. Either way the resumed prompt ([`src/prompt.rs`](../src/prompt.rs)) still carries the
current turn's instructions and context paths, not only a delta.

Whatever the diff's contribution, the **dominant unexplained component** of the follow-up cost
is the reviewer's own replayed conversation — turn 1's 368 KB diff plus its 36-call
exploration — which `--resume` carries forward every turn. (Control B shows the regression
occurs even with an *empty* change, so it is not driven by the diff; it does not prove the
follow-up cost is entirely independent of the current diff and instructions.) Prompt caching
should make carrying the conversation forward cheap: the unchanged prefix should be served as
`cache-read` (a fraction of the input price), not rewritten as `cache-write`.

## Experiments

Controls A, B, and D drove `dist\cross-review.exe --reviewer claude --model claude-opus-5
--effort low` over MCP for three turns on one session, against a throwaway repo, reading the
per-turn `cache-write`/`cache-read` split from the usage log. Control C additionally drove
`claude -p --resume` **directly** (no cross-review) to compare — see its section. The review
instruction was a trivial echo ("reply CACHE-OK / COUNTER=n"; the reviewer correctly treated
it as an injected directive and reported it instead — this does not affect the token
accounting).

Method and limits, stated up front:

- Effort was `low` and the conversation trivial, so these measure the **ratio** of
  cache-write to cache-read and its trend across turns, not production magnitudes.
- Each condition is a **single run (n=1)**, not repeated; the effects are large and
  consistent across the three turns of each run, but no variance is reported.
- Control turns were back-to-back (0–1 s gaps), so all sit inside any prompt-cache TTL. The
  applicable TTL depends on the auth regime — roughly 1 hour on a subscription, 5 minutes on
  an API key or once a subscription draws on usage credits (see the `gap_bucket` note in
  [`src/metrics.rs`](../src/metrics.rs)). Production gaps are in the symptom table above; all
  five were within the 1-hour lifetime and three within the 5-minute one.
- The echo review performs **no file reads**, so the controls exercise the caching of the
  *conversation and prompt*, not the reviewer's exploration. This matters for Control D (see
  its caveat).

### Control A — stable repo (baseline)

Repo left clean and unchanged between turns.

| turn | cache-write | cache-read |
|-----:|------------:|-----------:|
| 1 (fresh) | 3,080 | 2,800 |
| 2 (resume) | 892 | 5,880 |
| 3 (resume) | 962 | 6,772 |

`cache-read` **grows** with the conversation while `cache-write` stays tiny. Resume caching
works: each follow-up reuses the entire accumulated conversation from cache and writes only
the new increment. This refutes an early hypothesis that resume caching was simply broken.

### Control B — repo mutated between turns

Identical to A, except a `git commit` is made in the reviewer's cwd before each resume (only
variable changed).

| turn | cache-write | cache-read |
|-----:|------------:|-----------:|
| 1 (fresh) | 3,186 | 2,800 |
| 2 (resume) | 3,701 | 2,800 |
| 3 (resume) | 4,215 | 2,800 |

`cache-read` is **pinned** at the static prefix (2,800) while `cache-write` **climbs with the
conversation**. This is the production signature — cost that grows with the accumulated
conversation and ignores the (here empty) diff. A commit between turns is enough to defeat
the cache.

### Control C — raw `claude`, and the `--exclude-dynamic-system-prompt-sections` flag

`claude --help` documents a flag aimed squarely at this:

> `--exclude-dynamic-system-prompt-sections` — Move per-machine sections (cwd, env info,
> memory paths, git status) from the system prompt into the first user message. Improves
> cross-user prompt-cache reuse. Only applies with the default system prompt.

cross-review uses the default system prompt (it never passes `--system-prompt`), so the flag
applies. Two ways to test it.

Driving `claude -p --resume` directly (cwd = a git repo, commit between turns, trivial
prompt):

| turn | baseline write / read | with-flag write / read |
|-----:|----------------------:|-----------------------:|
| 1 | 29,049 / 0 | 4,840 / 23,911 |
| 2 | 5,168 / 23,911 | 4,146 / 24,634 |
| 3 | 5,199 / 23,911 | 4,176 / 24,634 |

Raw `claude` does **not** reproduce the pathology — it reuses ~24K of `cache-read` across
mutated turns regardless of the flag. This is a caution: **raw `claude` and cross-review
cache differently**, so the flag must be tested on the cross-review vehicle that actually
shows the problem.

Patching the flag into the Claude adapter and re-running Control B (cross-review, cwd = repo,
commit between turns):

| turn | Control B (no flag) write / read | with flag write / read |
|-----:|---------------------------------:|-----------------------:|
| 1 | 3,186 / 2,800 | 2,914 / 2,800 |
| 2 | 3,701 / 2,800 | 3,078 / 3,523 |
| 3 | 4,215 / 2,800 | 4,269 / 3,523 |

**Insufficient.** The flag moved a fixed ~700-token block into the cached region (read went
from a pinned 2,800 to a pinned 3,523), but `cache-write` still climbs with the conversation
and `cache-read` does not grow the way Control A's does. Read share improved only from ~43%
to ~53% on turn 2 and decays as the conversation grows. This is *consistent with* git/repo
context also reaching the **conversation stream** (a per-turn injection this system-prompt
flag would not relocate), but Control C establishes only that the flag is insufficient — it
does not by itself prove where the residual lives; see the verified/inferred split in
[Root cause](#root-cause--best-supported-hypothesis).

### Control D — neutral working directory (the fix)

Patched the Claude adapter's invocation `current_dir(&cfg.cwd)` to
`current_dir(super::neutral_dir(cfg))` — moving the child's process cwd away from the working
root — and re-ran Control B (cross-review, commit before every turn). The chosen `neutral_dir`
here resolved to a scratch state directory under the system temp path, which was independently
confirmed to have no `.git` ancestor; note that `neutral_dir` itself only does a lexical
containment check and does *not* guarantee this in general (hence the robustness requirement in
the risks below):

| turn | Control B (git cwd) write / read | neutral cwd write / read | Control A target write / read |
|-----:|---------------------------------:|-------------------------:|------------------------------:|
| 1 | 3,186 / 2,800 | 3,066 / 2,800 | 3,080 / 2,800 |
| 2 | 3,701 / 2,800 | **1,089 / 5,866** | 892 / 5,880 |
| 3 | 4,215 / 2,800 | **1,221 / 6,955** | 962 / 6,772 |

Neutral-cwd reproduces Control A almost exactly — `cache-read` grows, `cache-write` stays
small — **despite a commit before every turn**, and cost per follow-up fell from ~$0.05 to
~$0.03. With the reviewer's process cwd not a git repository, Claude Code has no git context
to derive or refresh, so the mutation is invisible and the whole conversation stays cached.

## Root cause — best-supported hypothesis

The evidence best supports this account. The reviewer runs with `current_dir(&cfg.cwd)` = the
**live working root** ([`src/reviewer/claude.rs`](../src/reviewer/claude.rs)). Claude Code, in
a git repo, is understood to regenerate a per-invocation context block on each `claude -p`
call. The parent agent (here Codex) commits to that repository *between* review turns, so that
context changes every turn, and the change invalidates the prompt cache from its position
onward — which includes the replayed conversation. The follow-up then pays `cache-write` to
reconstitute a conversation it should have read from cache. As the conversation grows across
turns, so does the re-cached amount, which matches the production signature.

What is **verified** versus **inferred**:

- Verified: repo mutation between turns is what flips the cache from read-dominated (Control
  A) to write-climbing (Control B) — that is a controlled single-variable result.
- Verified: neutral-cwd restores Control A economics under mutation (Control D).
- Inferred, not directly observed: the *contents* of the context block Claude Code regenerates
  (working directory, HEAD, git status, recent commits — listed from the flag's help text, not
  from the prompt itself) and *where* in the prompt the cache invalidation falls. These were
  not read off the actual prompt Claude Code sent.
- Inferred: that the residual after `--exclude-dynamic-system-prompt-sections` (Control C) lies
  in the conversation stream. Control C establishes the flag is *insufficient*; the data is
  *consistent with* a conversation-stream residual but does not by itself rule out another
  cache segment.
- Scope of Control D: the patch changed *only* the child's process `current_dir`. The
  "Working directory" line the reviewer is *told* is `parts.cwd` = `cfg.cwd`
  ([`src/prompt.rs`](../src/prompt.rs), [`src/tools.rs`](../src/tools.rs)), which the patch
  left unchanged — so D held the told-cwd constant and varied only the process cwd and git-repo
  discovery. That makes D clean evidence for the process-cwd/git-discovery effect. It does
  *not* test the path-rewrite half of the proposed fix, and because the echo review reads no
  files it does not exercise the reviewer's own file access at all.

This is a **cost** regression. Review *correctness* was not evaluated here — the controls used
an echo review that reads nothing, so nothing in this investigation speaks to whether the
production reviews were right. On a subscription window the cost is large, and it scales with
the number of follow-ups and the size of turn 1.

## Rejected alternatives

- **Shrink turn 1 / cap exploration.** Trades review quality for cost. A documentation change
  to a system still requires understanding that system to judge whether the docs are
  *correct*, not merely well written. A capped reviewer is a worse reviewer, which defeats the
  premise of the tool.
- **Skip `--resume` for "trivial" follow-ups.** A fresh turn must re-read the code and re-read
  the convention files (`AGENTS.md`, `README.md`) from cold to judge whether the delta is
  acceptable — so it re-pays the turn-1 exploration every turn. Strictly worse than replaying a
  cached conversation.
- **`--exclude-dynamic-system-prompt-sections` alone.** Necessary-looking but insufficient
  (Control C): it moves the system-prompt sections but the re-caching persists, consistent with
  a residual reaching the conversation stream (see the verified/inferred split in
  [Root cause](#root-cause--best-supported-hypothesis)).

## Proposed fix

Run the Claude reviewer from a **neutral, non-git working directory** and give it repository
access explicitly rather than through its cwd. This is more involved than a one-line cwd
change, because the current design *depends* on cwd being the configured working root in
several places at once. Concretely, in the Claude adapter's `invocation`:

1. **Set `current_dir(super::neutral_dir(cfg))` instead of `cfg.cwd`, but only when the
   reviewer is isolated *and* shell-less.** Git discovery walks *up* from cwd, so no
   subdirectory of the working root escapes it — only a cwd outside the repo tree does. Two
   conditions gate it:
   - **Isolation.** `--allow-reviewer-config` (which sets `isolate_reviewer = false`,
     [`src/config.rs`](../src/config.rs)) is documented in the README as loading project *and*
     user configuration, which needs the project as cwd. Keep `cfg.cwd` when isolation is off.
   - **Shell-less.** A Claude reviewer given a shell — which requires *both* `--tools` to
     include `Bash` *and* an allow rule permitting it ([`src/config.rs`](../src/config.rs),
     [README](../README.md)) — is expected to run git itself, and in `--diff auto` the server
     *withholds* the captured git diff on that basis. From a neutral cwd that git access no
     longer reaches the project. Gate on this too —
     but with the **active-entry** predicate `reviewer_has_shell_of(spec.reviewer)`, not the
     primary-only `reviewer_has_shell()` ([`src/config.rs`](../src/config.rs)): in a
     Codex→shell-less-Claude fallback the primary (Codex) always has a shell, so the generic
     predicate would wrongly keep Claude in the repo cwd. Test mixed reviewer chains. Otherwise,
     define and test explicit `git -C <root>` / absolute-path behavior for shell-enabled configs.
   - **Default read rules only.** The two conditions above still leave an isolated, shell-less
     reviewer with a *caller-supplied relative* `--allow-tools` rule (e.g. `Read(./src/**)`),
     which is passed through unchanged ([`src/config.rs`](../src/config.rs),
     [`src/reviewer/claude.rs`](../src/reviewer/claude.rs)) and would resolve against the neutral
     dir and silently lose access. The fix must decide the policy for such rules — reject,
     normalize to absolute, or keep `cfg.cwd` for that config — and test it. Only the default
     `scoped_claude_rules` path is safe to move blindly.
2. **Re-scope the reviewer's read grant — the hard part.** Today the allow-list is a
   *deliberately relative* `Read(./**)`/`Grep(./**)`/`Glob(./**)`, and
   [`src/config.rs`](../src/config.rs) (`scoped_claude_rules`, and the comment above it)
   explicitly rejects absolute interpolation as unsafe: these are gitignore-style globs, so a
   working-root path containing glob metacharacters (its example is `C:\work\[ab]`, a character
   class), a UNC root, or a drive root would mis-scope — *simultaneously failing to read the
   real project and granting reads outside it*. The relative rule sidesteps all of that by
   relying on cwd being the working root. So moving cwd neutral means building a **new, tested
   escaping mechanism** for all three tools that scopes to exactly the configured working root
   `cfg.cwd` — which **may be a subdirectory of the git repository** ([README](../README.md),
   "the capture is scoped to the working root, not the whole repository") — and no broader. Not
   the git top-level. This is the highest-risk part of the change; see the security note below.
3. **Make every path-bearing prompt element absolute.** With a neutral cwd, anything the
   reviewer receives as a cwd-relative path breaks. This is more than `context_paths`: the
   prompt renders `context_paths`, the caller's `instructions`, and the "Working directory"
   line all verbatim ([`src/prompt.rs`](../src/prompt.rs)), and the captured listings carry
   **different origins**: `git status --porcelain` paths are relative to the *repository root*
   (stated as such in the prompt, [`src/vcs/git.rs`](../src/vcs/git.rs)), whereas the
   `git diff --relative` and untracked listings are relative to the *working root* — and when
   `--cwd` is a subdirectory the two roots differ. So each listing must be resolved from its own
   actual origin, not a single assumed root. Note the server does **not** currently separate the
   logical working
   root from the child process cwd — both are `cfg.cwd` today ([`src/tools.rs`](../src/tools.rs)
   passes `cfg.cwd` as the prompt's `cwd`, and [`src/reviewer/claude.rs`](../src/reviewer/claude.rs)
   sets the same as the process cwd); introducing that separation *is* part of this change. The
   fix must establish a **mandatory absolute-root prompt contract** — resolve `context_paths`,
   the "Working directory" line, and the status/diff/untracked path references from their actual
   origins to absolute paths, and instruct the reviewer (in the default preamble, and accounting
   for `--no-preamble`/custom preambles where that instruction would be absent) to read by
   absolute path. Caller `instructions` cannot be rewritten safely, so the contract must make the
   absolute root unambiguous regardless of what they say. Cover it with an end-to-end test.

An **alternative worth investigating first**, because it would avoid the item-2 rework
entirely: is there a way to stop Claude Code discovering the repo while keeping cwd = the repo
(so the relative scoping and relative paths keep working)? `--exclude-dynamic-system-prompt-sections`
was the obvious candidate and is insufficient (Control C). A git-discovery suppression (an env
var, a ceiling directory, or a CLI option) was not found in `claude --help`, but confirming
its absence — or finding one — would decide between the two approaches.

### Requirements and risks the implementation must address

- **The read scope is a security boundary (highest risk).** The rewrite from a relative
  `./**` to an absolute scope must keep the scope *exactly* the configured working root
  `cfg.cwd` — which may be a subdirectory of the git repo, so it is **not** the git top-level —
  no broader, across glob-metacharacter paths, UNC roots, and drive roots, which is the
  specific hazard `config.rs` documents and avoids today. It needs its own tests proving all
  three: the real working root stays readable, paths outside it (and metacharacter-crafted
  siblings) are denied, and a subdirectory `--cwd` is scoped to that subdirectory rather than
  the enclosing repository. The existing `..`/junction escape-denial tests
  ([README](../README.md)) must be preserved, not regressed.
- **The neutral directory must be verified non-repository, robustly.** `neutral_dir` /
  `is_within` ([`src/reviewer/mod.rs`](../src/reviewer/mod.rs)) compare paths *lexically*, so a
  `--state-dir` reaching into `cfg.cwd` through `..` or a junction/symlink can look textually
  outside while being physically inside — reintroducing the exact problem. The fix must
  canonicalize/resolve the chosen directory, scan for a `.git` file *or* directory through all
  ancestors, distinguish an access error from "not found", and **fail closed** (refuse rather
  than guess) if it cannot confirm a non-repository directory.
- **Pre-change sessions must migrate safely.** This applies to exactly the sessions that
  *switch* cwd, which depends on the custom-rule policy chosen in item 1: isolated, shell-less
  Claude sessions always switch; sessions using `--allow-reviewer-config` or a shell never do.
  A custom-relative-rule session switches only if that policy is "normalize"; under "keep
  `cfg.cwd`" or "reject" it does not. So the migration set must be defined *together with* the
  item-1 policy — do not state one without the other. For a switching session created before the change, its stored conversation and
  verbatim instructions assume the old cwd (the configured working root, which may be a repo
  subdirectory — not necessarily the git top-level), while the child would now run from the
  neutral cwd. The server's resume check compares only the *logical* recorded cwd, not the
  child process cwd ([`src/tools.rs`](../src/tools.rs)), so it would not catch this; and if the
  reviewer's conversation is lost, resuming against a stored delta baseline can yield an
  incomplete review, a limitation the resume design already documents
  ([`docs/incremental-resume-disposition.md`](incremental-resume-disposition.md)). The fix must
  define and test a migration policy — e.g. force **both a fresh Claude conversation and a full
  capture** for pre-change switching sessions (a full diff alone does not repair lost
  conversation context) — or otherwise prove cross-cwd resume compatibility.
- **End-to-end validation with a real diff is required.** Control D used an echo review that
  reads no files, so it proved the *caching mechanism* only. Before shipping, an end-to-end run
  (a real change, the reviewer actually reading code and `AGENTS.md` by absolute path) must
  confirm the reviewer still explores correctly from a neutral cwd with the absolute read
  grant, and that a re-review is cache-read-dominated.
- **Isolation-off and shell-enabled paths stay correct.** With `--allow-reviewer-config`, or
  when the reviewer has a shell, the reviewer keeps `cfg.cwd` and its current relative scoping;
  the fix must not change those paths.
- **Codex direction is unaffected.** This is Claude-adapter-specific; the Codex reviewer uses a
  read-only shell and a different mechanism, and its cwd handling is out of scope here.

### Expected impact

On the production shape (`pr-616`), the **server-side diff capture** for turn 1 is unchanged
(it is a fresh turn and legitimately explores); the fresh turn's own cache and review-quality
effects are *not* established here — Control D's turn-1 numbers differ slightly and the change
also touches permissions and prompt paths, so treat fresh-turn impact as unverified (see Open
questions). The follow-ups, which re-cached 250K–307K each, are where the bulk of the
follow-up cost is. **If** the causal hypothesis holds and the end-to-end validation above
succeeds, they would instead read the conversation from cache and write only their small
increment — the Control A/D economics. That projection is conditional: Control D used an
echo review, so it has not been shown for a run that actually explores, applies absolute read
rules, and resumes across the migration.

## Open questions

- Exactly which per-turn context Claude Code injects into the conversation stream (versus the
  system prompt) is inferred from the Control C result, not directly observed. The neutral-cwd
  fix does not depend on the answer (it removes all git context at the source), but confirming it
  would sharpen the explanation.
- Whether the same cwd change benefits the fresh turn's cross-session cache reuse at all, or only
  the follow-ups. The data here only establishes the follow-up win.
- Whether Claude Code's git-repo discovery can be suppressed while keeping cwd = the repo (see
  the alternative under [Proposed fix](#proposed-fix)). If so, it avoids the read-scope rework
  and is likely the smaller, safer change.

## Review history

Reviewed through this repository's own `cross-review` gate (Codex, gpt-5.6-luna, effort=max).

**Round 1 — REQUEST CHANGES (six findings, all accepted).** All were verified against the code
and folded in:

1. (major) The claim that `config.rs` already handles absolute-path Read escaping was **wrong** —
   `scoped_claude_rules` is deliberately *relative* and the code documents absolute interpolation
   as unsafe. Rewrote the fix to require a new, tested escaping mechanism and reframed it as the
   highest-risk part.
2. (major) An unconditional cwd change would break `--allow-reviewer-config` (project-config
   loading needs cwd = project). Made the neutral cwd conditional on `isolate_reviewer`.
3. (major) Relative `context_paths` and captured path references would fail from a neutral cwd.
   Added a requirement to normalize them to the absolute root and test it.
4. (major) The neutral-dir check was too weak — `is_within` is lexical, so `..`/junctions defeat
   it. Required canonicalization, ancestor `.git` scanning, access-error handling, and fail-closed.
5. (minor) Controls C/D do not prove the precise causal story, and an intro line wrongly said all
   controls used cross-review when C used raw `claude`. Recast the root cause as the best-supported
   hypothesis with an explicit verified/inferred split, fixed the contradiction, and added a
   method/limits note (n=1, TTL regime, echo-review-does-no-reads).
6. (minor) The resume-delta explanation was too absolute. Qualified it: the delta is conditional,
   the controls ran in a non-deltable mode, and the shrinking production diff is not proof it fired.

**Round 2 — REQUEST CHANGES (round-1 findings 1/2/4 confirmed resolved; 3/5/6 substantially
addressed; seven residual/deeper findings, all accepted).** Verified against the code and folded
in:

1. (major) Conditioning neutral cwd on isolation alone is not enough — a shell-enabled Claude
   (`--allow-tools "Bash(...)"`) runs git itself and `--diff auto` withholds the capture on that
   basis (`reviewer_has_shell`), which a neutral cwd would break. Added a shell-less condition.
2. (major) "Repository root" / "exactly the repository" was the wrong scope: `--cwd` may be a
   subdirectory of the git repo, and the scopes use `cfg.cwd`. Replaced with "configured working
   root `cfg.cwd`" throughout and added a subdirectory-`--cwd` test requirement. Also corrected
   the false claim that `tools.rs` already separates logical cwd from child cwd — both are `cfg.cwd`
   today; that separation is part of the change.
3. (major) Added a pre-change-session migration risk: a session recorded under the repo cwd could
   resume with the child now at a neutral cwd, old relative paths, and an omitted Working-directory
   section; the resume check compares only the logical cwd. Requires a tested migration policy.
4. (minor) Wrong flag name: there is no `--resume-incremental-diff`; incremental resume is on by
   default and disabled with `--no-incremental-resume`, and the delta gate validates more than a
   stored pair (base equality, HEAD/base resolution, ancestry). Corrected and reframed as necessary
   (not complete) conditions.
5. (minor) Softened the causal story further: the context block's contents and the invalidation
   point were not directly observed, and the conversation-stream residual is stated as "consistent
   with", not established.
6. (minor) Scoped the conclusions to the experiment: review *correctness* was not evaluated (echo
   review reads nothing), and the replayed conversation is the "dominant unexplained component"
   rather than the diff being provably irrelevant.
7. (minor) Limited "turn 1 unchanged" to the server-side diff capture; fresh-turn cache and quality
   impact are explicitly unverified.

**Round 3 — REQUEST CHANGES (round-2 findings 2/4/7 confirmed resolved; 1/3/5/6 substantially
addressed; six residual findings, all accepted).** Verified and folded in:

1. (major) The shell gate named the primary-only `reviewer_has_shell()`; for a fallback chain it
   must use the active-entry `reviewer_has_shell_of(spec.reviewer)`, or a Codex→shell-less-Claude
   chain would wrongly keep Claude in the repo cwd. Corrected; added a mixed-chain test note.
2. (major) Custom relative `--allow-tools` rules (e.g. `Read(./src/**)`) in an isolated,
   shell-less config are passed through unchanged and would break under a neutral cwd. Added a
   "default read rules only" condition requiring an explicit policy (reject/normalize/keep cwd),
   and required preserving the existing `..`/junction escape-denial tests.
3. (minor) The Control D confound was wrong: the patch changed only the child's process cwd; the
   *told* Working-directory line (`cfg.cwd`) was held constant. Corrected — this makes D cleaner
   evidence for the process-cwd effect, and leaves the path-rewrite half untested.
4. (minor) Control C's section still stated the conversation-stream residual as a conclusion;
   aligned it with the "consistent with" hypothesis wording.
5. (minor) Scoped the migration risk to switching (isolated, shell-less, default-rules) sessions,
   noted the stored cwd is the working root (not necessarily the git top-level), and defined
   "fresh/full" as a fresh conversation *and* a full capture.
6. (minor) Made the expected follow-up savings explicitly conditional on the causal hypothesis and
   a successful end-to-end validation.

**Round 4 — APPROVE WITH COMMENTS (all six round-3 findings resolved; five minor residuals,
all accepted).** Folded in:

1. Broadened item 3 into a "mandatory absolute-root prompt contract": it now also covers the
   caller's verbatim `instructions`, the status/diff/untracked paths (which are relative to the
   working root, itself relative to the git top-level under a subdir `--cwd`), and
   `--no-preamble`/custom-preamble cases.
2. Made the migration set explicitly conditional on item 1's custom-rule policy (normalize
   switches; keep/reject does not), so the two are defined together.
3. Corrected the shell example: a shell requires *both* `--tools` containing `Bash` and a
   permitting allow rule; qualified the `--diff auto` withholding as git-specific.
4. Replaced a stale "cwd being the repository" with "configured working root".
5. Described Control D as moving the child process cwd away from the working root, recorded that
   its `neutral_dir` was independently confirmed to have no `.git` ancestor, and noted
   `neutral_dir` does not guarantee that in general.

**Round 5 — APPROVE WITH COMMENTS (all five round-4 findings resolved; one new minor,
accepted).** The round-4 broadening of item 3 had introduced an error: it lumped git status,
diff, and untracked listings as all working-root-relative. Corrected — `git status --porcelain`
paths are *repository-root*-relative while `git diff --relative` and untracked listings are
*working-root*-relative, and under a subdirectory `--cwd` those roots differ, so each listing
must be resolved from its own origin. The reviewer also independently confirmed the code-fidelity
claims (`reviewer_has_shell_of` as the active-entry predicate, custom-rule passthrough, the told
Working-directory line remaining `cfg.cwd`).

**Round 6 — APPROVE.** "The round-5 path-origin correction is accurate, and the document is
ready to merge." No findings; the reviewer re-confirmed the code-fidelity claims (git status
repository-root-relative vs `git diff --relative`/untracked working-root-relative) and that no
new issue was introduced.

### Implementation (`feat/reviewer-neutral-cwd`)

The implementation went through the gate separately, reviewing only its own diff.

**Round 1 — REQUEST CHANGES (three findings, all accepted).** (1) The cwd-mode migration ran
before `resume_block`, so a session that should be refused could be silently rebound — reordered
to run after, judged on the real resume entry (no default index). (2) The neutral read instruction
told the reviewer to prefix every listing path, which is wrong for git-diff `a/`/`b/` paths —
corrected. (3) `absolute_scoped_rules` accepted `.`/`..` segments — now fails closed on them.

**Round 2 — REQUEST CHANGES (two findings, all accepted).** (1) The neutral gate keyed only on a
git top-level, so a Perforce review inside a git repo would wrongly switch, and the diff guidance
did not cover rename/copy/binary/mode-only diffs — gated to the git backend and made the
instruction format-agnostic (use the underlying project-relative path). (2) The read-scope tests
checked only the emitted string — ran a live matcher deny-probe (allow in-root; deny outside,
same-prefix sibling, and `..` traversal; allow case-variant) and recorded it beside the builder.

**Round 3 — APPROVE.** "The accepted fixes are present, and I found no remaining correctness or
security defect." No findings.
