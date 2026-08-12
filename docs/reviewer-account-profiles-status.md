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
| 3·#10 | `Config.launch_root` captured before `--cwd`, canonicalized (allowlist key) | `9d934f4` | **approved** (`rv-19320-6`) |
| 3·#11 | allowlist store (`allowlist.rs`) + `winsec.rs` ACL FFI + authorization wiring | `9d934f4`, `e5d35de`, `b858568` | **approved** (`rv-19320-6`) |
| 3·#12 | `profile::secure_profile_dir` provisioning (handle-relative descent + name safety) | `1fa38a2`..`fa9d14f` | **approved** (`rv-19320-11`) |

**Invariant that holds today:** ambient (no profile) is byte-for-byte unchanged; every *non-ambient*
profile use is refused (`PROFILE_NOT_AUTHORIZED`). Since #11, that refusal is driven by the **real
allowlist store** rather than a deny-all stub: `Config::profile_authorized` reads the account currently
in the profile home (via the new `Reviewer::fingerprint_at` seam, a direct home-path read that does
*not* recurse through `resolve_authorized_home`) and asks the store whether the full four-field tuple
is on file. With no setup tool yet the store is always empty, so the check still denies every
non-ambient profile — the invariant holds, now on live data. The `[f4]`/`[f5]` pre-spawn lock +
switch guard remain **deferred to #14** (documented in `resolve_authorized_home`); the empty store
means that race window authorizes nothing meanwhile.

**New foundation landed with #11 — `src/winsec.rs`** (new module, gate-approved): Windows security-
descriptor FFI in `winjob`'s style (no new crate; declares `advapi32`/`ntdll` directly). It builds one
restrictive, inheritance-protected DACL (current user + SYSTEM + Administrators, deduped by SID for the
LocalSystem case) and **applies + verifies it through a handle** (`SetSecurityInfo`/`GetSecurityInfo`);
`create_secured_dir`, `write_secured_file`, `read_secured_file`, plus a **handle-relative
`open_child_relative` (`NtCreateFile`, `RootDirectory` + `OBJ_DONT_REPARSE`)** so a store/credential
file is proven a direct child of its verified parent — no by-path reopen TOCTOU (`[f15]`/`[f20]`/`[f22]`).
Since #12, `src/winsec.rs` also provides `create_secured_child_dir` (handle-relative secured subdir),
`reject_reparse_on_ancestors`, a `FILE_ATTRIBUTE_DIRECTORY` check on `open_dir_no_follow` (so a DACL is
never applied to a file), and a component-length guard in `nt_open`; `profile::secure_profile_dir`
returns a **held** `SecuredProfileDir` (no-follow, no-delete-share handle) that #15 keeps alive across
the vendor login. Name safety lives in `validate_profile_name` (rejects trailing-dot aliases, reserved
device names, over-length). **What #15 still needs from the [f20] contract:** the post-login
*handle-relative credential-file re-verify* (open the written `auth.json`/`.claude.json` relative to the
held dir handle and re-check its DACL) — the primitive (`open_child_relative` + `verify_restrictive_dacl`)
exists; #15 wires it into the setup flow.

## Remaining (Phase 3)

Do roughly in this order.

4. **#14 Switch guard + probe expected-account + spawn atomicity** — the store *consultation* half of
   #14 is **already done** (#11: `profile_authorized` now checks the full 4-field tuple, `launch_root`
   key). What remains: (a) change the probe's `expected` account in both `auth_check`s from the home's
   own fingerprint to the **allowlist's authorized** account (see gotcha below); (b) the post-review
   start-vs-final fingerprint guard (`[f4]`); (c) the pre-spawn per-home lock + generation recheck so
   authorize→probe→spawn is one critical section (`[f5]`); (d) validate the codex-invocation
   controlled-env / evidence-server interaction (`smoke.ps1`).
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
  use a `fresh: true` review. The #10/#11 gate (`rv-19320`) hit **all three** flakes across six
  attempts — an evidence-service timeout, a 1800s reviewer timeout, and an unstructured envelope —
  before converging to `approve` on `rv-19320-6`; budget extra collect attempts and expect to re-issue
  `fresh: true` after a lost turn (the write-ahead marker makes the session non-resumable, by design).
  The #12 gate then hit the **frozen findings ledger** on a resume (a resolved finding stuck at
  `open` with `last_status_change_turn` not advancing and its detail still quoting the pre-fix code):
  verify the fix in-file, then take an authoritative verdict from a `fresh: true` review — that is what
  converged #12 to `approve` (`rv-19320-11`). Do not trust a resumed finding whose status did not move.
- **Before the PR:** run the full `.\build.ps1` (release + restage `dist\cross-review.exe`) — needs
  the agent MCP sessions unloaded (the running server locks `dist\`). Not done yet; everything it
  checks *except* the release/dist stage has been run green manually.

## How to resume

1. `git checkout docs/multi-account-plan`; confirm `git log` matches the table above.
2. Re-read this file + the impl plan.
3. Start at **#10 → #11** (launch root + allowlist store). Keep each task green (`cargo fmt --check`,
   `clippy -D warnings`, `cargo test`) and gate substantial chunks through cross-review before moving on.
