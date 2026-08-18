# AGENTS.md

Instructions for coding agents working in this repository. `CLAUDE.md` imports this file,
so it applies to Claude Code and Codex alike.

## What this project is

`cross-review` is a Windows-only MCP server that hands work to a *different* model for
review and returns what it said. Rust, MSVC toolchain, `serde` as the only dependency —
the small self-contained binary is a feature, so do not add crates casually. See
[README.md](README.md) for what it is and how to set it up, and [`docs/`](docs/) for the
full design and its verified evidence.

## Pull requests

**Every PR must be reviewed by a different model through this repository's own
`cross-review` MCP server before it is approved to merge.** This is a blocking gate, not a
suggestion. We eat our own dog food: the merge gate for cross-review is cross-review.

- Call `cross_model_review` with a session named for the branch or PR, and collect it with
  `cross_model_review_result`. A single collect call blocks until the review is done — omit
  `wait_seconds` to block to completion — so re-reviews and first passes alike are one call, not
  a poll loop. If the `wait_seconds` budget elapses the call returns `status=running`; if a
  shorter client tool timeout fires first the response is suppressed and you see a client-side
  timeout. Either way just call again with the same `review_id` — abandoning a collect no longer
  cancels the review (`cross_model_review_cancel` does). Both directions are already wired up in this
  checkout — Claude Code gets Codex via [`.mcp.json`](.mcp.json), Codex gets Claude Opus 4.8 via
  [`.codex/config.toml`](.codex/config.toml) — so the reviewer is always the model that did
  not write the diff.
- **Do not paste a diff into `instructions` in either direction.** The isolated Codex reviewer
  receives the selected change through `repository_change` and reads/searches the tree through
  the other bounded evidence tools; its shell runs from a sterile non-repository directory and
  is not the normal repository interface. The Claude reviewer receives the server capture in
  its prompt. Describe the intent and what you want scrutinised, and let the configured capture
  plus reviewer evidence do the rest.
- **For the Claude direction, the gate reviews what is committed.** What gets captured is
  fixed by `--diff` on the server entry, not chosen per call, and
  [`.codex/config.toml`](.codex/config.toml) pins `main...HEAD` so the reviewer is shown the
  branch against its base rather than the default working-tree capture, which is empty once
  the work is committed. Four things follow, and the first two are pre-flight:
  - **Commit, and check `git status --porcelain` is empty, before every call in that
    direction — the first review and each re-review.** A dirty tree is worse than it looks:
    the capture is the committed range, but the reviewer can read the live files and is
    handed `git status`, so it would be reviewing one revision through a diff and another
    through the tree. The reviewer is now told when that has happened; do not make it rely
    on that.
  - **`main` there is the *local* ref, so fetch and check it is current first.** A stale
    local `main` does not fail — it silently widens the capture to include everything merged
    since, and the reviewer spends its turn on code the PR did not touch. This has happened:
    a review of this PR was handed 1707 insertions instead of 208. Nothing in the response
    distinguishes that from a large PR, so the check is yours to make:

    ```powershell
    git fetch origin
    if ($LASTEXITCODE -ne 0) { throw "fetch failed - origin/main may be stale, stop" }
    git merge-base --is-ancestor main origin/main
    if ($LASTEXITCODE -eq 1) { throw "local main has diverged - sort that out first" }
    if ($LASTEXITCODE -ne 0) { throw "could not compare main to origin/main - stop" }
    git branch -f main origin/main
    if ((git rev-parse main) -ne (git rev-parse origin/main)) { throw "main was not updated - stop" }
    ```

    Every check earns its line. A failed `fetch` leaves `origin/main` at whatever it was
    last time, so the ancestor test passes against a stale ref and reports everything
    current — the exact failure this preflight exists to catch, wearing the costume of a
    passing check. `--is-ancestor` exits 1 for "no" and 128 for an error, so a missing
    `origin` would otherwise be reported as divergence. `git branch -f` on a diverged `main`
    discards the local tip rather than refusing, and it fails outright when `main` is the
    branch you have checked out — so the last line confirms the ref actually moved rather
    than trusting that it did. Use `$LASTEXITCODE`, not `$?`: for native commands in
    Windows PowerShell, `$?` can be set from the error stream.
  - **If your PR is not based on `main`, this entry cannot gate it.** The base is pinned in
    the server arguments, so a review of `main...HEAD` on a stacked branch takes in the PR
    underneath as well and would pass the gate without the PR's own diff ever being reviewed
    alone. Do not describe the mismatch and proceed. Register a second server under a
    different `[mcp_servers.…]` name, with `--diff` pointing at the real base, in your
    **global** `%USERPROFILE%\.codex\config.toml` — not in this repository's
    [`.codex/config.toml`](.codex/config.toml), which is tracked, so editing it would either
    dirty the tree the bullet above requires to be clean or land a config change inside the
    PR being gated.
  - For mid-development review of work that is not committed yet, open a Claude Code session
    against this checkout and call from there — that direction gets the Codex reviewer, whose
    default `--diff auto` capture is exposed through `repository_change` alongside bounded live
    tree evidence. A Codex session cannot reach it by changing arguments; it is a different
    server entry.
- Say what changed and why, and point the reviewer at this file and `README.md`. It runs
  configuration-isolated, so `CLAUDE.md` is not auto-loaded; it will read convention files
  when told to.
- The reviewer reviews; it cannot fix. The Claude direction has no write-capable tool in the
  session at all; the Codex direction runs under a read-only policy whose write refusals are
  enforced by the OS — Windows restricted token, verified with no model in the loop. Its
  *reads* are still unconfined, and no CLI surface was found that would confine them; see
  `README.md`. Bring the findings back and act on them yourself.
- After acting on feedback, call `cross_model_review` again with the **same session** so the
  reviewer reports what is resolved, what is still open, and what regressed. That request
  must carry every finding you dismissed and the evidence for dismissing it: a dismissal the
  reviewer never sees is not a dismissal, it is a bypass. Only use `fresh: true` when the
  earlier findings would mislead. A re-review call can be refused rather than resumed: the
  server returns `SESSION_NOT_RESUMABLE` when the session is past its turn or idle limit
  (`--session-max-turns`, `--session-max-idle-seconds`) or no longer matches the reviewer,
  model and working root that created it, and `SESSION_NOT_FOUND` when the reviewer session
  expired out from under the resume. In every one of those cases the reviewer remembers
  nothing, so retry with `fresh: true` and re-supply the earlier findings and your
  dismissals yourself — nothing is carried over for you.
- Never approve, merge, or tell the user a PR is ready to merge without that review having
  run and its findings either resolved or disputed with concrete evidence the reviewer has
  seen and answered.
- **A finding the reviewer holds open against your argument is usually right.** On three
  occasions across #71 and #62 an agent argued — with quoted code and a passing test it had
  written for the purpose — that a held or regressed finding was resolved, and was wrong every
  time. The error had the same shape each time: it fixed the defect it could see and assumed that
  was the defect being named. Before disputing, go back to the code rather than to your reasoning
  about the code. If you still disagree, **run a fresh control review** (`fresh: true`, no ledger
  to anchor on, ideally pointed at the disputed area): a fresh reviewer that independently raises
  the same thing tells you the finding is real, and one that does not is the evidence a dispute
  needs — but only in one direction. A fresh reviewer that **does** raise it independently is
  strong evidence the finding is real; one that **does not** is weak, because a fresh session
  samples differently and can converge while abandoning concerns. Corroboration, not acquittal:
  a dispute still has to answer the original finding.
- **Read `review_prose`, not only `findings`.** The findings are the machine record; the prose is
  where the reviewer explains itself, including why a finding it is holding open is still open. It
  is present on every turn that ran (issue #73 — it used to be `null` whenever the machine record
  was clean, which made a reviewer that explained itself indistinguishable from one that said
  nothing, and cost two rounds of #71 and five of #73's own review). Read `captured` and
  `denial_count` too, on the same response: a review run on a truncated capture, or one that spent
  its turn on commands its policy refused, is thinner than it looks and does not read that way.
- If the review fails — `CLI_NOT_FOUND`, `NOT_AUTHENTICATED`, `RATE_LIMITED`, any of the
  codes in the README — hand the user the remediation the tool returned, say the review did
  not run, and stop. Do not substitute your own read of the diff, and do not fall back to a
  same-model subagent: a model reviewing its own work shares its own blind spots, which is
  the entire premise of this project. `cross_model_review_status` checks the reviewer CLI
  and auth for free, before anything is billed.
- **Read the result's `outcome` field rather than reassembling one from the other fields.**
  `converged` stop, `changes_requested` act and re-review, `escalate` and `rebaseline` both mean a
  person decides — the second because the session itself cannot continue, so carry the still-open
  findings into a fresh one. `rebaseline` with `session_stagnant` is that case and nothing worse: the
  session went several turns without raising or resolving anything, so it was stopped. Every finding
  comes back unchanged and still open — **it is not a hint that they are stale or fixed**, and the
  warning names them precisely so you can carry them across. Re-read them, address them, and open a
  fresh session; do not treat the stop as licence to drop one. When `structured` is false there is no machine record for that turn:
  the empty `findings` list means nothing was recorded, not that nothing was wrong, and the review
  is in `review_prose` and the text body. Read it; do not re-run blind.
- **`changes_requested` with `non_convergence_reason: verdict_contradiction` and `verdict_detail:
  approve` may be a *deferral*, not a demand — the one case where `outcome` alone is not enough.** The
  reviewer approved the diff but held a finding open because it is real and belongs in another PR.
  `open_count > 0` under an `approve` verdict correctly fails closed to `changes_requested`, because the
  machine record cannot distinguish a *deferred* finding from an *ignored* one (issue #82). So in
  exactly that combination, read `review_prose`: if the reviewer calls the open finding a tracked,
  out-of-scope follow-up rather than a blocker, there is nothing to act on — note where it belongs and
  proceed, rather than spending a round asking it to restate what it already said. Do **not** work
  around this by getting the reviewer to mark a genuinely-deferred finding `resolved` — that corrupts
  the record. If that temptation recurs, that is the signal to revisit #82 (a reviewer-set deferral
  disposition, which removes the ambiguity rather than annotating it) instead of documenting around it.
- Summarise the outcome for the user: what the reviewer flagged, what changed in response,
  and what is still disputed. Keep findings the reviewer has confirmed resolved separate
  from ones you argued against — they are not the same claim.

### When the gate itself is broken

Dogfooding is also how the tool gets tested in anger. If the gate misbehaves — a failure
code that misreports the reviewer's state, a resumed session that lost context, a response
that reads badly to the calling agent — that is a bug in this repository, so report it
rather than working around it.

That leaves one deadlock worth naming: a PR that repairs the gate cannot pass through the
gate it is repairing. There is an exception for exactly that case, and **you cannot invoke
it on your own judgement.** Every condition below is an artifact you must be able to point
at. If any one of them is missing, stop and tell the user which one:

- **A human maintainer authorised the use of this exception, for this named PR**, in the PR
  itself or in a direct instruction to you. Approval to work on, approve, or merge the PR is
  not that: "go ahead with #15" authorises the work, not the bypass. Nor may you infer it
  from the situation being urgent, from the repair being obviously correct, or from a
  previous PR having been authorised. If you are unsure whether you have it, you do not have
  it — ask.
- **The PR is the minimum repair to the gate and nothing else.** Split unrelated work out
  into its own PR, which goes through the normal gate. A repair with a tidy-up bundled into
  it does not qualify; neither does a rate limit, a reviewer that is slow or expensive, or
  findings you would rather not address.
- **The failing output is quoted verbatim in the PR** — the code and the full message, not
  a paraphrase of what went wrong.
- **A different model reviewed it out of band, under the same read-only constraints**, and
  the PR carries the request and the response in full, naming the model and how it was
  confined. A claim that this happened is not the artifact; the transcript is.
- **The repaired gate reviews the exact final diff before the merge.** If the repair works,
  this is possible — so it is required, and it is what actually closes the exception. If it
  does not work, the repair has not been demonstrated and the PR does not qualify: say so
  and stop. A repair that cannot pass through the gate it claims to have fixed is a claim,
  not a repair.

## How much rigor, and where

This project is careful by default, and that is right in the places where being wrong is
expensive. It is not right everywhere, and the difference is worth stating because the review
gate will not state it for you.

- **Rigor belongs where the blast radius is real**: account management and credentials, the
  read-only and write boundaries, the isolation posture, anything that could route a review
  through the wrong account or let a reviewer write. Those are the places to enumerate edge
  cases and fail closed.
- **For a review itself, the worst case is usually that it is lost and re-run** — some tokens and
  some minutes. Avoid that; do not fortify against every edge case to prevent it. A protocol
  change that adds a new way for a turn to degrade is usually paying more than the failure it
  prevents. *Usually*, because one review failure is not merely retryable: a review that is
  quietly thinner than it looks can produce a false approval rather than a lost review, which is
  why the fail-closed rules elsewhere in this file stay exactly as they are.

**Perfect is the enemy of good, and this tool will never be perfect.** A review that is
occasionally lost and re-run is a working tool. A tool that spends a week of edge-case machinery
per fix is not, however correct each piece of that machinery is. Any worst case can be escalated
until it justifies anything; the question to ask is what the realistic cost is, and whether the
machinery costs less than that.

**The gate ratchets toward rigor, and you are the counterweight.** Iterating a design through
review adds machinery every round: each round asks "what about this edge case", each honest
answer adds something, and nothing in the loop ever argues for less. Issue #62's plan went
through six rounds and ten findings — every finding correct — and arrived roughly ten times the
size of the bug, with four of the ten existing only inside a chain of consequences from one early
decision. When a finding's fix adds machinery, ask whether the machinery is proportionate to what
it prevents, and be willing to answer a finding by **removing** the thing that made it reachable.
Cutting scope in response to review is a legitimate resolution; say so plainly when you do it, so
a shrinking plan is not mistaken for one that gave up.

## Before handing work back

```powershell
.\build.ps1
```

Runs `cargo fmt --check`, clippy with `-D warnings`, the unit tests, and a release build,
then stages `dist\cross-review.exe`. Both MCP configs point at `dist\`, so restaging needs
open agent sessions unloaded first; `build.ps1` reports the blocking PIDs rather than
shipping a stale binary.

- `cargo test` — unit tests only, no network and no model calls.
- `smoke.ps1 -Reviewer codex|claude` — real end-to-end MCP round trip. It calls a model for
  real and costs tokens, so run it when the change touches the protocol, spawning, or
  session handling, and mention the cost to the user. **The evidence service is protocol**: a
  change to it needs the round trip. It is now exercised in **both** directions — `-Reviewer codex`
  (the isolated Codex evidence path) and `-Reviewer claude` (the in-scope shell-less Claude evidence
  path added for the Claude direction) — so a change touching the evidence service or the Claude
  reviewer's isolation/spawn needs the Claude round trip too, not only Codex. `build.ps1`
  passing is not a substitute either; it never starts a reviewer. `smoke.ps1` runs against
  `target\release\` rather than `dist\`, so it needs a
  `cargo build --release` but neither a restage nor a session restart.
- CI is Windows-only by design and additionally re-verifies the `CLI_NOT_FOUND` failure
  contract against the shipped binary. Do not weaken that check.

## Conventions that are easy to get wrong

- **Never commit `cross-review.exe`.** `dist\` is gitignored; releases are built and
  published by the tag-driven CI workflow, never from a workstation.
- **Pin models by full id** (`claude-opus-4-8`, `gpt-5.6-luna`). Aliases resolve to older
  models.
- **stdout is protocol traffic only.** All diagnostics go to stderr.
- **A findings ledger is disposable; do not reach for migration machinery to preserve one.** If a
  change makes old ledgers unreadable, the loader's existing fail-closed behaviour *is* the
  migration story: `Invalid`, refuse the resume, and the caller rebaselines. No compatibility
  types, no version dispatch, no provenance flags. Be accurate about what that costs, because it
  is more than a re-run: the ledger holds stable ids, the immutable content of every finding, and
  the dispositions, so a rebaseline means a human carrying the still-open findings into a fresh
  session by hand. Bounded and occasional, and still cheaper than permanent machinery in the
  loader — but say so plainly rather than implying nothing is lost. Note also that the resume
  limits are *defaults*: `--session-max-turns` and `--session-max-idle-seconds` are configurable
  and either can be set to `0` to disable, so do not assume a ledger is stale merely because it
  is old. (None of this is licence to break the *session record* itself, which carries account
  identity and binding state — see the rigor section above for which side of the line a thing
  sits on.)
- **The reviewer's isolation and read-only posture are security boundaries.** The tool
  policy, `--safe-mode`, the Codex sterile-root/evidence service plus
  `--ignore-user-config` / `--ignore-rules`, the path-scoped
  `Read(./**)` grants, and the job-object process reaping all exist for reasons documented
  in [`docs/`](docs/) with verified evidence. Do not relax any of them without saying plainly
  what boundary moves.
- **Claim only what was verified.** The docs distinguish "verified" from "assumed"
  deliberately. Keep that discipline in code comments and in what you tell the user.
