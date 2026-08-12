# Reviewer account profiles — build status & handoff

Resume point for the account-profiles feature. Read this, then
[`reviewer-account-profiles-impl.md`](reviewer-account-profiles-impl.md) (the detailed plan) and
[`reviewer-account-profiles.md`](reviewer-account-profiles.md) (design). Everything below is on branch
`docs/multi-account-plan`; all code is committed and green (`cargo fmt --check`, `clippy -D warnings`,
581 tests).

## Done (committed, gate-approved)

| Phase | What | Commit | Gate |
|---|---|---|---|
| Plan | design + implementation plan | `f8aa32f`..`8813fc3` (+ revisions) | approved (design), approved (impl) |
| 1a | ProfileSelector, flags, `resolve_authorized_home` + **deny-all** authorization | `08b7bd3` | — |
| 1b | thread home through reads, controlled child env (`env_clear`+allowlist) | `1d21b2a` | — |
| 2 | session `ProfileIdentity`, fail-closed resume | `2c26960` | — |
| — | Phase 1/2 code-review fixes (f3–f6) | `d99e39d` | **approved** (`rv-24708-26`) |
| — | identity section rewritten from verified CLI surfaces | `a7a06f1`, `a608447` | **approved** |
| 3·#13 | pre-spawn identity + method probe | `dd69142` | (built; not separately gated yet) |

**Invariant that holds today:** ambient (no profile) is byte-for-byte unchanged; every *non-ambient*
profile use is refused (`PROFILE_NOT_AUTHORIZED`) because `Config::profile_authorized` is deny-all.
So all non-ambient code paths are wired + unit-tested but **dormant at runtime** until task #14.

## Remaining (Phase 3)

Do roughly in this order — the store underpins authorization, which the probe's account check needs.

1. **#10 Launch root** — capture+canonicalize `std::env::current_dir()` in `main.rs` *before*
   `Config::from_args`; store as `Config.launch_root`. Never key the allowlist on `Config::cwd`
   (repo-settable via `--cwd`). Plan: impl `## Phase 3 → Allowlist store → [f7]`.
2. **#11 Allowlist store** — ACL'd, cross-process-locked, atomic (temp+rename); entries
   `(launch_root) → (canonical effective_home + reviewer_family + account_fingerprint)`. Plan: impl
   `## Phase 3 → Allowlist store` (`[f19]` schema, `[f22]` store security).
3. **#12 `secure_profile_dir`** — handle-based, no-follow, reparse-reject on original components,
   containment under the profile root, DACL via `SetSecurityInfo` on the creation handle (new FFI in
   `winjob`'s style — `winjob` is job objects, not ACLs). Plan: impl `## Phase 3 → Profile-dir
   provisioning + ACL` (`[f6]`, `[f15]`, `[f20]`).
4. **#14 Wire authorization + switch guard** — replace `profile_authorized` deny-all with the
   allowlist check (full 4-field tuple, `launch_root` key). Change the probe's `expected` account in
   both `auth_check`s from the home's own fingerprint to the **allowlist's authorized** account (see
   gotcha below). Add the post-review start-vs-final fingerprint guard (`[f4]`). Validate the
   codex-invocation controlled-env / evidence-server interaction (`smoke.ps1`).
5. **#15 Setup MCP tool + localhost page** — ordered state machine (classify op → human approval →
   provision/stage → confirm → commit); three ops (authorize-only / first-provision / staged
   re-login); loopback one-time-token approval page; redaction; per-profile cross-process lock +
   expiry reclaim + rollback. Plan: impl `## Phase 3 → Setup MCP tool …` (`[f9]`,`[f18]`,`[f21]`,`[f23]`).

## Resume-critical gotchas (not obvious from the code)

- **The probe's account check is currently a self-consistency check.** In both `auth_check`s,
  `assert_profile_identity(resolved, expected)` passes `expected = <home>`'s own fingerprint, so the
  account half is tautological *today* — the **method** check (subscription) is the live part.
  **Task #14 must switch `expected` to the allowlist's authorized account** — that is what catches a
  profile silently re-logged to a different account. Commented at both call sites (`dd69142`).
- **Verified identity surfaces (task #13):** Codex — `auth.json.auth_mode=="chatgpt"` (method) +
  `tokens.account_id` (== `id_token` `chatgpt_account_id`, verified); `codex doctor` is
  `CODEX_HOME`-aware. Claude — `auth status` `authMethod=="claude.ai"` + `apiProvider=="firstParty"`
  (method), `.claude.json` `accountUuid`/`organizationUuid` (account), `orgId` cross-check. Verified
  on **Codex CLI 0.144.5** locally; repo support matrix is Codex 0.146.0 / Claude 2.1.210
  (`usage-remaining-gate.md`) — **re-verify shapes against supported versions when building**; the
  probe is version-pinned and fails closed on an unrecognised shape.
- **Codex invocation carries a NOTE** (in `codex.rs invocation`): the controlled env's interaction
  with the evidence MCP server (`env={}`) is only exercisable once a profile is authorized — confirm
  end-to-end with `smoke.ps1` in task #14/#15.
- **Phase 2 behaviour change (approved f14):** legacy sessions (predating `profile_identity`) and any
  session without a capturable fingerprint are **non-resumable**. Intended, fail-closed; short blast
  radius (sessions are short-lived).
- **Gate mechanics:** this is a Claude Code session → the reviewer is **Codex** (`.mcp.json`,
  `--diff auto` = working tree). Committed code shows an empty capture, so review by pointing the
  reviewer at `main..HEAD` files with a symbol map (as done for `rv-24708-22`). Known gate flakiness
  this session: evidence-service timeouts, frozen findings ledger on resume, unstructured envelope —
  filed as issues #61/#62/#63; when a resumed finding freezes at a stale status, verify in-file then
  use a `fresh: true` review.
- **Before the PR:** run the full `.\build.ps1` (release + restage `dist\cross-review.exe`) — needs
  the agent MCP sessions unloaded (the running server locks `dist\`). Not done yet; everything it
  checks *except* the release/dist stage has been run green manually.

## How to resume

1. `git checkout docs/multi-account-plan`; confirm `git log` matches the table above.
2. Re-read this file + the impl plan.
3. Start at **#10 → #11** (launch root + allowlist store). Keep each task green (`cargo fmt --check`,
   `clippy -D warnings`, `cargo test`) and gate substantial chunks through cross-review before moving on.
