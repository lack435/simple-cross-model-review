# Retire git capture modes — a formal review is the live working tree

Status: **draft plan**, on `plan/retire-capture-modes`. Two cross-review rounds so far (session
`plan-retire-capture-modes`, Codex, gpt-5.6-luna, effort=xhigh). Round 1 raised six findings; round 2
resolved f2/f5/f6 and held f1/f3/f4 open. This is r3, addressing the three held findings.

**Revision note (r1 → r2).**

- **f2, f5, f6** accepted outright and **resolved at round 2**: `git diff` omits untracked files
  (f2 → mechanism 1 composes untracked in); removing `diff_mode` does *not* fail-close old session
  records because `SessionRecord` lacks `deny_unknown_fields` (f5 → mechanism 7 handles it
  explicitly); the tracked dogfood config still passes the deleted `--diff` flag (f6 → the dogfood
  section makes the config/doc sweep atomic).
- **f1, f3, f4** revised at round 2 but **held open**, then reworked here in r3 (below).

**Revision note (r2 → r3).**

- **f1** (critical): my round-2 dispute was **wrong, and I concede it.** I argued every coverage
  requirement is adversarial; the reviewer produced a concrete *cooperative-but-fallible* failure —
  a reviewer reads "focus on auth" as the whole scope, pulls a path-restricted diff, gets
  `complete: true`, and approves while another changed file goes unseen. No gaming needed. The fix is
  the reviewer's own minimal form: **a formal approval requires the reviewer was served the complete
  canonical working-tree diff** (mechanism 4). "Focus" prose steers *attention* within that diff, not
  the approval scope; a genuinely partial look is a `consult`, not an approving review. This restores
  a *minimal* server-defined canonical scope — justified by fallibility, not anti-gaming.
- **f3**: the serve-record needed a defined parent-readable channel (the round-2 "receipt path" was
  undefined, and the two CLI streams differ). r3 specifies a **nonce-bound side-channel file** the
  evidence server writes and the parent reads (mechanism 3).
- **f4**: reframed per the maintainer's principle that **the base is the branch's fork point and the
  author owns it** — the tool computes the fork point from the author's own refs and must not
  second-guess it by fetching. The base is `merge-base(HEAD, upstream)` (always an ancestor, never a
  diverged tip), recorded in `captured:`, with a **no-fetch staleness check** that fails closed on the
  detectable Case B (local ref behind its remote-tracking ref). Full prevention of a never-fetched
  base is deliberately *not* done — it would require the tool to override the author's refs
  (mechanism 1). **Resolved at round 3.**

**Revision note (r3 → r4).** Round 3 resolved f4; f1 and f3 narrowed to one shared gap — **paged
canonical diffs.**

- **f1** (critical): the canonical-diff approval floor closed the path-restriction hole (confirmed),
  but a large canonical diff is *paged*, and a fallible reviewer could read page 1, miss the cursor,
  and approve. **f3**: the side-channel appended per call but did not tie the pages of one canonical
  diff together. r4 fixes both with one mechanism: the canonical diff is a **logical operation with an
  operation id**, its pages are associated in the serve-record, and the approval floor requires the
  server observed that operation **paged to its terminal page** (cursor exhausted). This stays
  fallibility-calibrated — it checks the reviewer *requested* the whole canonical diff, not that it
  *comprehended* it (a page-then-lie reviewer is still out of model) — and it is the honest completion
  of the serve-record contract, not new coverage attestation. **Round 4 resolved f1 and f3**
  (verdict: approve-with-comments).

**Revision note (r4 → r5).** Round 4 raised one minor finding, **f7**: mechanism 4 still described the
canonical diff as a "single `repository_diff` call," contradicting the multi-call logical operation of
mechanism 3 and risking an implementation that excludes cursor-follow pages from the operation. Fixed:
the canonical diff is one logical operation that may span cursor-follow page calls, all retaining the
operation's canonical identity.

## Why this plan exists

This is a deliberate re-opening of a goal that was cut, not a new idea. Recording the provenance
so the frame that narrowed it the first time does not silently reassert itself.

Capture modes — the `--diff auto|none|staged|HEAD|<rev>|<range>` axis — exist for exactly one
reason: the Claude reviewer could not see the repository. `--safe-mode` gave it no shell and no MCP,
so the server had to pre-render "the change" into a static string and embed it in the prompt. Every
fragility of the current design descends from that one static pre-capture:

- `--diff main...HEAD` is **empty until the work is committed**, and **silently widens** when the
  local `main` ref is stale — the failure AGENTS.md's commit-first / fetch-`main` preflight ritual
  exists to prevent by hand.
- `--diff auto` (working tree) is **empty exactly when a PR is ready** (already committed).
- The reviewer reviews the static capture but reads live files through other tools, so it can be
  looking at **two different revisions at once** — the reason the ritual also demands a clean
  `git status`.

Issue #47 gave the Codex reviewer a read-only evidence service; PR #102 extended it to the in-scope
Claude reviewer. That removed the *root cause* — the reviewer is no longer blind. The original aim
was that the capture would then become **unnecessary**. It did not, because the #102 plan framed
itself as "give Claude the evidence service Codex already has" (parity), and under a parity frame the
capture stays, because that is how the Codex side worked too. In its design review, finding **f1**
correctly narrowed the plan's Goal 2 from "capture fragility **eliminated**" to "**mitigated, not
eliminated**", justified by two things — the evidence service has no live range-diff operation, and
Goal 4 ("reuse the evidence server **unchanged**, no new tools"). Neither was a requirement anyone
set for the *project*; both were artifacts of the parity frame. The gate then hardened "mitigate,
not eliminate" across five more rounds, exactly the ratchet AGENTS.md warns about ("nothing in the
loop ever argues for less"), because the original goal was never written down for anyone to defend.

This plan writes it down: **eliminate git capture modes.**

## The goal

A formal git review no longer rests on a static, server-pinned pre-capture. The change under review
is the **live working tree**, read on demand through the evidence service by the reviewer, which
controls exactly how to diff it. The `--diff` axis, the `DiffMode` type, and the up-front git
capture step are removed. The commit-first / fetch-`main` / clean-`git status` ritual dissolves
because there is no static range to be empty, stale, or out of step with the live tree.

Non-goal: informal, exploratory second opinions. Those are what `cross_model_consult` is for. This
plan is about **formal review**, and a formal review is the working tree.

## Decision

**Single owner of "what is the change": the evidence service, evaluated live at review time. The
parent captures nothing for a git review.**

This is not a new delivery model. The **tree-only consult path already works this way today** —
`should_capture_change()` returns false, `vcs::capture` is skipped, and the reviewer reads the
workspace live through the evidence service (`src/tools.rs:2481`, `src/vcs/shared.rs:119`). The
capture-pipeline investigation identified this as "the existing precedent for the live-evidence
model." Formal review becomes that same live-evidence model, plus the formal apparatus a consult
does not have (the findings ledger, the fail-closed gate, named resumable sessions).

Concretely:

- The reviewer works from the **live working tree**. It diffs the branch as it judges best — against
  `HEAD` to see uncommitted work, against the branch's fork point to see the whole branch — and
  reports issues on what it finds. It explores freely, with one floor: to *approve*, it must have
  been served the complete canonical working-tree diff (mechanism 4), so a review cannot pass on a
  self-narrowed slice.
- The caller steers with **prose**, not a structured range: "focus on the auth changes",
  "everything through commit `abc123…` was already approved, review what's after it". The caller's
  `instructions` already reach the reviewer; no new structured argument is added.
- The parent does **no ref resolution and no range selection**. What little resolution is needed
  (merge-base, a commit id the caller's prose named) happens either in the evidence service's own
  trusted Rust or by the reviewer walking `repository_history` for full object ids. Untrusted
  symbolic refs never reach git — the existing `valid_object_id` hardening is preserved.
- `captured:` becomes **descriptive** — built from the server's record of the diff it *served the
  reviewer on demand* (mechanism 3), not from a static pre-render. Its guarantee flips from "what the
  server sent up front" to "what the server served when the reviewer asked", which stays a
  server-attested fact rather than a claim about the reviewer's judgement.

Scope is **git only**. Perforce has no `--diff` (it is driven by a per-call changelist argument that
is already per-call and does not suffer this fragility); converting Perforce's `repository_change` to
a live `p4 describe`/`diff` is a noted follow-on, not part of this plan.

## Threat model — the reviewer is fallible, not adversarial

The gate design (mechanism 4) rests on this, so it is stated explicitly rather than left implicit.

The reviewer is **our own reviewer**: a model we select, pin, pay for, and prompt to review. It is
**cooperative but fallible**, not adversarial. It has no incentive to game the gate, and its actual
failure mode is doing a *worse* job than intended — this reviewer (gpt-5.6-luna) says blatantly
wrong things at low effort and becomes highly effective at high effort, which is exactly why the
dogfood config pins it to `max`/`xhigh`. The mitigation for a fallible reviewer of our own choosing
is **effort-pinning plus honest tooling**, not a gate that treats it as an attacker.

Two consequences shape mechanism 4:

- **We do not defend against a reviewer that games the gate.** A reviewer that would deliberately
  diff an empty or unrelated scope to fake a pass is not our threat, and defending against it is
  self-defeating: a coverage gate only proves the reviewer *looked*, never that it *judged honestly*.
  A reviewer dishonest — or prompt-injected by hostile repository content — enough to diff nothing is
  equally able to pull the whole diff and then report "approve, no issues" with fabricated findings,
  which sails through any coverage attestation. Coverage machinery therefore defends against neither
  the cooperative reviewer (wrong problem) nor the dishonest one (does not work). The untrusted-repo
  vector is handled where it belongs — the reviewer's isolation and read-only posture — not here.
- **We do defend against a fallible reviewer being silently handed, or silently settling for, a
  partial change.** A well-meaning reviewer given an incomplete diff without being told — untracked
  files missing, a truncated page, a wrong base — or one that honestly under-scopes ("focus on auth"
  read as the whole task) can approve in good faith on less than the whole change. That is real, and
  the fix is cheap and fallibility-shaped: make the tool *honest* about completeness, and floor a
  formal approval on the reviewer having been served the complete canonical diff (mechanism 4) — not
  a coverage proof against gaming, just a guarantee it was shown the whole tree.

## Mechanisms

Seven pieces. The load-bearing one is the first; the rest fall out of removing the static capture,
or close a gap round-1 review found.

### 1. A live diff operation in the evidence service

Add `repository_diff` (equivalently, repurpose `repository_change`, which in a no-capture world has
no pre-rendered blob left to serve). It runs live against the real repository root, using the same
hardened pattern `repository_revision` already uses — bounded child-process worker, argv-only (no
shell), isolated git config, `current_dir(root)` (`src/evidence/git.rs:178-252`).

- **The result is a whole working-tree change, not a bare `git diff` (f2).** `git diff` omits
  untracked files, and a new file is a common and important part of a change — the current capture
  includes untracked contents for exactly this reason, and this very plan doc was reviewed as an
  untracked file that a bare `git diff HEAD` reports as zero changes. So the operation **composes**
  the tracked diff with bounded untracked-file enumeration and contents, reusing the existing
  `git ls-files --others --exclude-standard` + capped-read logic already in the git backend
  (`src/vcs/git.rs:1456-1566`), and carries the same completeness/omission metadata that logic
  already produces.
- **Arguments:** `base` and `head`, each either a full 40/64-hex object id (validated by the existing
  `valid_object_id`, `src/evidence/core.rs:2097`) or a member of a **closed sentinel enum** —
  `worktree`, `index`/staged, `head`, and `branch-base` — never a raw symbolic ref. Optional `path`,
  routed through the existing `validate_relative` and passed after `--` exactly as `history`/
  `revision` do. The closed sentinel set is what keeps model input off git's ref/option surface.
- **`branch-base` is the branch's fork point, computed from the author's own refs — the tool does
  not second-guess it (f4).** The base a formal review diffs against is the branch's *fork point*, and
  which fork point that is belongs to the author: if the branch was cut from an older commit, that
  older commit is the base until the author rebases or fast-forwards. So `branch-base` resolves, in
  trusted Rust, to `merge-base(HEAD, <upstream>)` — the branch's configured upstream when set, else
  the detected default branch. This is **always an ancestor of `HEAD`** (three-dot semantics); the
  op never diffs against a commit that is not in the branch's history (a two-dot diff against a
  diverged tip), which is a category the reviewer should never be shown.
  - It **fails closed** when the base is unresolvable — no upstream/default branch, ambiguous, or no
    merge-base — with a typed error, never a silent fallback to `HEAD`.
  - It runs a **no-fetch staleness check** for the one case where a resolved base is wrong rather than
    merely old-by-the-author's-choice: the branch was cut from a recent upstream but the *local* base
    ref lags behind its own remote-tracking ref (Case B — the "1707 insertions instead of 208"
    incident). The check compares the base ref to its remote-tracking ref *using refs already on
    disk* (no network); if the tracking ref is ahead, it fails closed with "your `main` is N commits
    behind `origin/main` — update it or name a base," rather than reviewing others' merged work as
    the branch's own.
  - It is honest about its limit: if the repository was **never fetched**, both refs are equally stale
    and the check cannot fire. Full prevention there would require the tool to `fetch` and thereby
    override the author's ref state — deliberately *not* done, because per the principle above the
    base is the author's to set. The resolved base object id and its source are always recorded in the
    serve-record, so the scope is visible even where it cannot be auto-verified.
  - **Implementation note (superseding the `@{upstream}` design above).** The implementation's own
    cross-review (session `impl-retire-capture-modes`, f1/f10) showed `@{upstream}` is *wrong* as the
    base — under the common push-to-same-name workflow it is the branch's own remote ref, so
    `merge-base(HEAD, @{upstream})` can equal `HEAD` and omit all committed work while the gate still
    accepts an approval. So `branch-base` resolves the **default branch** instead —
    `refs/remotes/origin/HEAD`, then `refs/remotes/origin/main`, then `refs/remotes/origin/master`,
    *fully qualified* so a local branch named `origin/main` cannot shadow it — and fails closed if
    none resolves. Because all candidates are remote-tracking, a stale *local* branch can never be the
    base **by construction**, which makes the separate no-fetch staleness check above unnecessary; it
    was not implemented. The never-fetched residual stands as described.
- **The op emits a structured serve-record (f3), because the server computes the diff to serve it.**
  Each `repository_diff` call records what it actually served: the resolved scope (which sentinel /
  which endpoints), the resolved base object id and its source, file/insertion/deletion counts, the
  untracked count, and the existing completeness/truncation/cursor flags — and, for the gate, whether
  this call was the **complete canonical working-tree diff** (mechanism 4). This is server-side truth,
  delivered to the parent through the channel defined in mechanism 3 — not reconstructed from the
  reviewer's stream.
- **Plumbing:** the evidence-ops investigation enumerated the six sites a new tool touches — the
  `TOOLS` array, `tool_definitions()`, `output_schema()`, the `call_with_receipt` dispatch match,
  `Limits`, and `SCHEMA_VERSION` (`src/evidence.rs`, `src/evidence/core.rs`). The paged diff body
  reuses the `text_page` shape and the `max_change_bytes` budget; the serve-record is a small typed
  addition to the call's structured result.

### 2. Stop capturing for git reviews

- `should_capture_change()` (`src/tools.rs:2481`) becomes **VCS-keyed, not consult-keyed**: false for
  everything git (formal review *and* `include_change:true` consult — resolved decision 3), true only
  for Perforce. The git branch of `vcs::capture` is no longer reached at all, so the git capture path
  deletes **entirely** rather than only for reviews — no git caller is left using it.
- Remove `--diff` parsing and the `DiffMode` type: `src/config.rs:1016-1019,1238,1271-1291,1372`,
  the `Config.diff` field (`src/config.rs:928`), and `enum DiffMode` with its `parse`/`Display`/
  `diff_args` (`src/vcs/git.rs:123-248`). The `vcs` and `resume_incremental_diff` fields stay
  (Perforce and resume still exist).
- The evidence `Bundle` no longer carries a pre-filled `change` string (`src/evidence.rs:291`); the
  server derives diffs live.
- `supplies_change_of` / `chain_needs_capture` / the Codex-only prompt suppression at
  `src/tools.rs:3744` collapse: with nothing captured, no change is embedded in any prompt, for any
  reviewer. This is a net deletion.

### 3. `captured:` from the server's serve-record, not from parsing the reviewer's stream

The draft claimed `captured:` could be reconstructed by extending the existing stream parse. f3
showed that does not hold: `call_with_receipt` today receives only a timestamp and emits no receipt
(`src/evidence/core.rs:550-555`), the Claude health parse keeps only success/error booleans
(`src/reviewer/claude.rs:582-663`), the Codex parse keeps only call counts (`src/reviewer/codex.rs:832-847`),
and a `text_page` body carries no file/line metrics. There is no metrics channel to reconstruct
from, and inventing a cross-stream one for two CLI formats would be fragile machinery.

- **The server already has the numbers, because it ran the diff to serve it** — the question f3
  correctly forced is *how they reach the parent*. The evidence server and the reviewer talk over
  MCP; the parent does not sit on that channel and the Claude/Codex CLI streams expose different
  slices of it, so "surface it through the receipt path" was underspecified. r3 defines a concrete,
  reviewer-independent channel: the evidence server **appends each `repository_diff` serve-record to a
  nonce-bound side-channel file** in the same capability directory it already owns (beside the bundle,
  under the same `<nonce>` the parent generated), and the parent reads that file when the reviewer
  exits. This reuses the bundle's existing lifecycle — `create_new`, nonce-checked, RAII-deleted at
  turn end (`src/evidence.rs:296-415`) — and is authoritative regardless of which reviewer ran,
  because it is written by the server, not echoed by the model. `call_with_receipt` (today just a
  timestamp, `src/evidence/core.rs:550-555`) is the natural write point.
- **The record is typed, append-only, and keyed by a logical operation (f3 aggregation).** A single
  canonical diff can span several `repository_diff` page calls, so a flat per-call append is not
  enough. Each `repository_diff` request is assigned an **operation id** (a diff for a given scope;
  its cursor-follow page calls share the id), and each appended record carries: operation id, page
  cursor and whether this page is **terminal** (no further cursor), the resolved scope, the
  `canonical` flag, base id + source, and the counts/completeness flags. The parent aggregates records
  by operation id to reconstruct each logical diff and whether it ran to its terminal page.
- **`captured:` is built from that file**, reporting the **one complete canonical operation** — the
  canonical-flagged diff that reached its terminal page — with narrower exploratory diffs available as
  detail. The aggregation is **fail-closed**: no canonical operation, a canonical operation missing
  its terminal page, or *conflicting* canonical operations (differing counts for the same tree state,
  which should not happen) all resolve to "no trustworthy canonical diff" → no `captured:` line and no
  approval (mechanism 4), never a best-guess. A small, typed, single-source contract; no
  per-CLI-stream heuristic.
- **The `CaptureSummary` invariant is rewritten honestly.** Today it is "what the server *sent*, never
  what the reviewer received" (`src/vcs/capture_summary.rs:1-17`). The live model reports **what the
  server served to the reviewer on demand** — still a server-attested fact, not a claim about the
  reviewer's judgement. Update `docs/capture-summary.md` to match.

### 4. One fail-closed gate, always on, both directions — a fallibility floor, not a coverage proof

This mechanism is where f1 lands, and its shape follows directly from the threat model above: the
gate protects a *cooperative but fallible* reviewer from being silently handed a partial change. It
does **not** attempt to prove full-scope coverage against a reviewer that games it — that defends
against a non-threat and cannot work anyway (see "Threat model").

- **The canonical working-tree diff, defined.** The server has one distinguished scope it calls
  *canonical*: the whole working tree (tracked diff against `branch-base` — the fork point of
  mechanism 1 — plus staged and untracked changes on top), served complete. It is mode-less and
  server-defined; it is *not* a `--diff` axis the caller pins, and it is the same thing for every
  review of a given tree state. The reviewer obtains it as **one canonical logical operation, possibly
  spanning cursor-follow page calls** (mechanism 3): the first `repository_diff` call opens the
  operation and every follow-up page call retains its operation id and `canonical` identity, so the
  terminal-page gate sees the whole operation, not just its first page.
- **Honesty comes first, in the tool.** Because that canonical diff includes untracked files
  (mechanism 1, f2) and carries explicit completeness/truncation flags, a fallible reviewer is not
  quietly handed a partial change: incompleteness is *visible* in the evidence it reads.
- **The approval floor (f1, conceded).** A formal **approval** is accepted only if the reviewer was
  served the **complete canonical diff, paged to its terminal page** — the serve-record file holds one
  canonical operation (mechanism 3), the server flagged it complete, and the operation reached its
  terminal page (no cursor left unfollowed). This closes both halves of f1: a reviewer that pulls only
  a path-restricted diff is caught (a narrowed diff is not the canonical one), *and* a reviewer that
  starts the canonical diff but stops after page 1 of many is caught (the operation never reached its
  terminal page). Crucially this is a floor on *approval*, not a leash on the reviewer: it may pull as
  many narrowed/exploratory diffs as it likes for focus, and "focus on X" prose still steers its
  *attention* — it just cannot **approve** without having been served the whole working tree, end to
  end. A genuinely partial review is a `consult`, not an approving review.
- **The rest is a sanity floor**, re-keyed from the static summary (`capture_thin`,
  `src/tools.rs:3629`; enforced `:3895`) to the serve-record and unified across both reviewers. It
  fails closed when an approval would rest on nothing or on something the server itself marked partial:
  - *the reviewer never pulled a diff* → `EVIDENCE_UNAVAILABLE`;
  - *the canonical diff was server-flagged incomplete* (truncated at the byte budget, unresolved/stale
    base per f4) or *left mid-pagination* (a cursor remained unfollowed) and the reviewer approved
    anyway → fail closed, because it approved on evidence known to be partial.
- **What it still does not do — the anti-gaming line stays cut.** The floor checks that the server
  *served* the complete canonical diff and the reviewer *requested* it end to end. It does **not** try
  to prove the reviewer *read or comprehended* any page — that is the distinction that keeps this
  fallibility-calibrated: requesting the terminal page is server-observable and catches an honest
  early-stop; comprehension is not observable and a reviewer that pages through and then lies in its
  verdict is a dishonest/manipulated reviewer, out of threat model and uncatchable by any coverage
  check (see "Threat model"), handled by isolation, not here. It also does not reject narrowed
  exploratory diffs. The concession is a *minimal* delivery-completeness floor; it is not a slide back
  to full coverage attestation.
- **A legitimately empty change is a distinct, honest outcome**, not a gate failure: the reviewer
  pulled the canonical diff, the server computed it as genuinely empty (clean tree, nothing to
  review), and that is reported as "nothing to review" — not a false approval, not an error.

### 5. Reviewer preamble guidance

- Tell the reviewer plainly: the material is the live working tree; here is the `repository_diff`
  op; **pull the canonical diff (`base: "branch-base"`) — the whole working tree against the branch's
  fork point, including untracked files — before approving**, since an approval requires it
  (mechanism 4); narrow with `path` or `base: "head"` for focused exploration on top; honor any focus
  or base the caller's instructions name. Keep it short — the tool descriptions carry the detail, as
  they do for the other evidence tools.

### 6. Caller prose steering — no new argument

- The caller's existing `instructions` field is the steering channel. Document the two conventions
  the user named — "focus on X" and "treat commit `<id>` as the approved baseline, review after it".
  Nothing structured is added; the reviewer interprets prose and drives the diff.

### 7. Explicit session handling for the removed `diff_mode` (f5)

The draft assumed removing the `diff_mode` field would make old session records fail closed on load,
letting the loader's rebaseline behavior be the whole migration story. f5 showed this is false:
`SessionRecord` deserializes with ordinary `serde` and **no `deny_unknown_fields`**
(`src/session.rs:129-130`), unlike the evidence `Bundle`. So an old record still loads fine after the
field is gone — `diff_mode` is simply ignored — while the resume guard that used it
(`src/tools.rs:4677-4691`) disappears. An `include_change:true` consult started under static capture
could then resume the *same* conversation under live-diff semantics, silently.

- **Handle it explicitly, don't rely on field removal.** Either (a) add `deny_unknown_fields` plus a
  session schema version to `SessionRecord` so a record written under the old contract is refused and
  rebaselined, or (b) keep `diff_mode` as a recognized legacy marker whose presence forces a
  rebaseline for a change-capturing session. Option (a) is the cleaner fit and aligns the session
  record with the `Bundle`'s existing strictness. This is bounded — a session record is small and
  rebaselining is the normal fail-closed path — but it must be *written*, not assumed.

## What changes for the dogfood PR gate

This is a first-class consequence, not a footnote: the whole reason this repo exists is its own
merge gate, and this plan changes how that gate is driven.

Today AGENTS.md pins the Claude direction to `--diff main...HEAD` and spends a long preflight
(commit-first, clean `git status`, fetch + fast-forward `main`) keeping a human from handing the
reviewer the wrong static range. Under this plan there is no pinned range: the reviewer reviews the
current state of the branch and diffs it against its base itself. That entire AGENTS.md section is
rewritten and mostly deleted — the ritual it describes is machinery for a problem this plan removes.

**The tracked config must move in the same change, or reviews break before they start (f6).**
`.codex/config.toml` still passes `--diff main...HEAD` (`.codex/config.toml:51`). Once `--diff`
parsing is removed, that argument is unknown and the server **fails configuration before a review
runs** — the dogfood gate would be dead on arrival. So this plan lands, atomically:

- `.codex/config.toml` — drop `--diff main...HEAD` (and any `--diff` in `.mcp.json` if present).
- AGENTS.md — the PR-gate section rewrite/deletion above.
- README, its capture/`--diff`/`--vcs` reference and the `repository_change` description, and the
  smoke documentation — rewritten to the live model.
- The affected design docs whose subject is static capture (`docs/capture-summary.md`, and pointers
  in the two evidence-service docs).
- Startup and a real review invocation tested in **both** directions, so a stale flag or a doc that
  still describes `repository_change` cannot ship silently.

## What boundary moves — the part that gets the rigor

Per AGENTS.md "How much rigor, and where": most of this plan is delivery mechanics, which get the
light touch. Two things touch a boundary and get the scrutiny.

- **Ref resolution and the read-only posture.** The security property that must not move is that
  **untrusted (model-supplied) input never reaches git as a ref or an option**. This plan preserves
  it exactly: `repository_diff` accepts only full-hex object ids (existing `valid_object_id`) or a
  closed sentinel enum; paths go through `validate_relative` and after `--`; the git invocation
  reuses the isolated-config, no-shell, `current_dir(root)` hardening. No new symbolic-ref parser is
  introduced — that was the tempting expansion, and it is explicitly rejected. The reviewer remains
  read-only and shell-less; a diff is a read.
- **The caller's proof-of-review.** The guarantee flips from proof-of-offer ("the server sent the
  change") to a report of what the server *served the reviewer on demand* (mechanism 3) — still a
  server-attested fact. The property that must not weaken is **no false approval on a change the
  reviewer could not have seen**. It does not, but note precisely *which* threat this closes:
  against a *fallible* reviewer, honest tool completeness (f2) plus the mechanism-4 sanity floor
  prevent a silent partial-change approval; against a *dishonest or manipulated* reviewer, no gate in
  this layer helps (a reviewer can always pull the full diff and lie in its verdict), which is why
  that vector is carried by the reviewer's isolation and read-only posture, not by this guarantee.
  The guarantee is honest about its own scope rather than overclaiming coverage it cannot enforce.

## Scope — deliberately out

Stating these so the plan does not accrete them under review (the failure that produced this plan):

- **Perforce.** Its `repository_change` could also go live; it is a separable follow-on. P4's per-call
  changelist path is already per-call and is left untouched. `vcs::capture` remains for the P4
  dispatch branch.
- **A structured caller range override.** The user's decision is prose steering, not a typed range
  argument. No `--diff`-shaped per-call parameter is added.
- **Migration / compatibility machinery for the findings *ledger*.** A findings ledger made
  unreadable by the change is disposable: the loader's existing fail-closed behavior is the migration
  story (refuse the resume, rebaseline). No version dispatch, no compatibility types for the ledger.
  Note this does **not** extend to the *session record*, which carries binding state and does *not*
  fail closed on the removed field on its own — that is handled explicitly in mechanism 7 (f5), and
  is the one place a bounded version marker is warranted rather than assumed.
- **A symbolic-ref validator in the evidence service.** Rejected above; closed sentinels + full-hex
  ids only.

## Resolved decisions

These three were open in draft; settled with the maintainer before implementation.

1. **Ambient (evidence-less) Claude — unsupported for git review (option a).** An ambient Claude
   (`ProfileSelector::Ambient`, the no-`--claude-profile` default at `src/config.rs:542`) inherits
   `~/.claude` and runs off the evidence path under `--safe-mode`. With capture removed it can see
   nothing, so **reviewing git with the Claude reviewer requires a profile**; an ambient Claude git
   review fails with a clear error directing the user to pin one, rather than reviewing blind. This
   is not a real narrowing in practice: both dogfood directions already pin `--claude-profile work`,
   and the Codex reviewer is unaffected (its evidence path uses sterile-root isolation, not a
   profile). No capture fallback is kept — keeping one would reintroduce, for a single unused config
   mode, exactly what this plan deletes.
2. **The default diff base — server-computed.** Folded into mechanism 1 as the `branch-base`
   sentinel: the evidence service resolves merge-base with the detected/configured default branch in
   its own trusted Rust, so the reviewer never derives it by hand and no ref resolution touches the
   model's input path.
3. **`consult` converges on live delivery.** `include_change:true` consult drops its added static
   capture and diffs live through the evidence service it already has attached. Delivery is now
   identical to review; the review/consult distinction is *only* the formal apparatus (review keeps
   the findings ledger and the fail-closed gate; consult stays ledger-free and un-gated). This is
   why the git capture path deletes entirely (mechanism 2) — after it, no git caller captures.

## Verification

- **`smoke.ps1` in both directions** is protocol-level evidence and non-optional here: this plan
  changes the evidence service (a new op) and the delivery model. Run `-Reviewer codex` and
  `-Reviewer claude`, and assert: a formal review with an uncommitted working-tree change is reviewed
  (the case that is empty-capture today); **a review whose only change is an untracked new file is
  reviewed** (the f2 case — and the literal case of this plan doc); a clean-tree review reports
  "nothing to review" rather than approving nothing; and the fail-closed gate fires when the reviewer
  is prevented from pulling a diff.
- **Unit tests.** The `capture_summary` cluster and the `DiffMode` tests (`src/vcs/git.rs`,
  `src/config.rs`) are rewritten, not merely edited — they pin behavior this plan removes. The
  byte-for-byte golden of the rendered capture prompt (`src/vcs/mod.rs:110`) is retired with the
  render path. New tests cover: `repository_diff` argument validation (sentinels + hex only, symbolic
  refs and options rejected); untracked-file composition and its completeness metadata (f2);
  `branch-base` resolving to `merge-base(HEAD, upstream)`, failing closed on an unresolvable base, and
  the no-fetch staleness check firing when the base ref is behind its remote-tracking ref (f4); the
  nonce-bound serve-record file and its operation-id aggregation — page association, terminal-page
  detection, one-complete-canonical-operation selection, and fail-closed on missing/partial/conflicting
  records (f3); the mechanism-4 approval floor (approval refused when the reviewer pulled only a
  path-narrowed diff and never the canonical one; **refused when the canonical operation stopped
  before its terminal page**; refused when the canonical diff was flagged incomplete; allowed when the
  canonical diff was complete-and-empty → "nothing to review") (f1); and `SessionRecord`
  refusing/rebaselining a record written under the old `diff_mode` contract (f5).
- **CI** remains Windows-only and keeps re-verifying the `CLI_NOT_FOUND` contract.
