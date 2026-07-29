# AGENTS.md

Instructions for coding agents working in this repository. `CLAUDE.md` imports this file,
so it applies to Claude Code and Codex alike.

## What this project is

`cross-review` is a Windows-only MCP server that hands work to a *different* model for
review and returns what it said. Rust, MSVC toolchain, `serde` as the only dependency —
the small self-contained binary is a feature, so do not add crates casually. See
[README.md](README.md) for the full design.

## Pull requests

**Every PR must be reviewed by a different model through this repository's own
`cross-review` MCP server before it is approved to merge.** This is a blocking gate, not a
suggestion. We eat our own dog food: the merge gate for cross-review is cross-review.

- Call `cross_model_review` with a session named for the branch or PR, and collect it with
  `cross_model_review_result`. Both directions are already wired up in this checkout —
  Claude Code gets Codex via [`.mcp.json`](.mcp.json), Codex gets Claude Opus 5 via
  [`.codex/config.toml`](.codex/config.toml) — so the reviewer is always the model that did
  not write the diff.
- **Getting the diff in front of the reviewer depends on which direction you are calling.**
  The Codex reviewer has a read-only shell, so give it the branch and base and let it run
  `git diff` itself. The Claude reviewer still has no shell, but no longer needs one: the
  server captures the change and hands it over with the request. Do not paste a diff into
  `instructions` in either direction — describe the intent and what you want scrutinised,
  and let the reviewer or the server fetch the code.
- **For the Claude direction, the gate reviews what is committed.** What gets captured is
  fixed by `--diff` on the server entry, not chosen per call, and
  [`.codex/config.toml`](.codex/config.toml) pins `main...HEAD` so the reviewer is shown the
  branch against its base rather than the default working-tree capture, which is empty once
  the work is committed. So commit before asking for the gate review: uncommitted edits are
  not in that range, and the reviewer will report on the branch without them rather than
  notice they are missing. For mid-development review of work that is *not* committed yet,
  call from the Claude side instead — the Codex reviewer has a shell and can see the working
  tree.
- Say what changed and why, and point the reviewer at this file and `README.md`. It runs
  configuration-isolated, so `CLAUDE.md` is not auto-loaded; it will read convention files
  when told to.
- The reviewer reviews; it cannot fix. The Claude direction has no write-capable tool in the
  session at all; the Codex direction runs under a read-only policy whose write refusals are
  verified, though enforcement there is the CLI's rather than demonstrably the OS's — see
  `README.md`. Bring the findings back and act on them yourself.
- After acting on feedback, call `cross_model_review` again with the **same session** so the
  reviewer reports what is resolved, what is still open, and what regressed. That request
  must carry every finding you dismissed and the evidence for dismissing it: a dismissal the
  reviewer never sees is not a dismissal, it is a bypass. Only use `fresh: true` when the
  earlier findings would mislead. If the response reports that the session had expired and
  was replaced with a fresh one, the reviewer remembers nothing — re-supply the earlier
  findings and your dismissals yourself.
- Never approve, merge, or tell the user a PR is ready to merge without that review having
  run and its findings either resolved or disputed with concrete evidence the reviewer has
  seen and answered.
- If the review fails — `CLI_NOT_FOUND`, `NOT_AUTHENTICATED`, `RATE_LIMITED`, any of the
  codes in the README — hand the user the remediation the tool returned, say the review did
  not run, and stop. Do not substitute your own read of the diff, and do not fall back to a
  same-model subagent: a model reviewing its own work shares its own blind spots, which is
  the entire premise of this project. `cross_model_review_status` checks the reviewer CLI
  and auth for free, before anything is billed.
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
  this is possible — so it is required, and it is what actually closes the exception.

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
  session handling, and mention the cost to the user.
- CI is Windows-only by design and additionally re-verifies the `CLI_NOT_FOUND` failure
  contract against the shipped binary. Do not weaken that check.

## Conventions that are easy to get wrong

- **Never commit `cross-review.exe`.** `dist\` is gitignored; releases are built and
  published by the tag-driven CI workflow, never from a workstation.
- **Pin models by full id** (`claude-opus-5`, `gpt-5.6-terra`). Aliases resolve to older
  models.
- **stdout is protocol traffic only.** All diagnostics go to stderr.
- **The reviewer's isolation and read-only posture are security boundaries.** The tool
  policy, `--safe-mode` / `--ignore-user-config`, the path-scoped `Read(./**)` grants, and
  the job-object process reaping all exist for reasons documented in the README with
  verified evidence. Do not relax any of them without saying plainly what boundary moves.
- **Claim only what was verified.** The README distinguishes "verified" from "assumed"
  deliberately. Keep that discipline in code comments and in what you tell the user.
