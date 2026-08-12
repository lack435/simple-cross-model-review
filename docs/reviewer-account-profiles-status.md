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
| 3·#14 | probe-against-authorized-account + per-spawn identity/method probe + switch guard | `efb7e8d`, `b4ac18a`, `58bf1f3` | **approved** (`rv-19320-14`) |

**State today:** ambient (no profile) is byte-for-byte unchanged. A non-ambient profile is refused
(`PROFILE_NOT_AUTHORIZED`) unless the allowlist store holds a matching entry — and **since #15 part 3a
the store can be populated**: `cross_model_setup_profile` (authorize-only) lets a human authorize an
already-provisioned, signed-in profile, at which point the whole non-ambient review path goes **live**
(probe, switch guard, controlled-env spawn). So the feature is now usable end-to-end for a profile you
have already logged in; only *provisioning a brand-new home via the vendor login* (#15 part 3b) is
missing. Authorization is driven by the **real allowlist store**: `Config::profile_authorized` reads
the account currently in the profile home (via the `Reviewer::fingerprint_at` seam, a direct home-path
read that does *not* recurse through `resolve_authorized_home`) and asks the store whether the full
four-field tuple is on file. Since #14 the account check is no longer tautological
(`resolve_authorized_home_with_account` pins the authorized account; the probe and switch guard compare
against *it*), the identity+method probe re-runs on **every** non-ambient spawn (never cached — worker
`attempt()`, `Reviewer::resolve_home_identity`), and the post-review switch guard refuses a review whose
account moved mid-flight. **Remaining for #15:** the `[f5]` per-home cross-process lock (shared read on
the review path, exclusive in setup) — deferred deliberately because #15 owns the setup side of that
same lock; and `smoke.ps1` profile end-to-end, which needs a real authorized profile (#15).

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

Only #15 is left. It is the largest piece and it *activates* everything above: once a profile can be
authorized, the store stops being empty and the whole non-ambient path (probe, switch guard,
controlled-env spawn) goes live — so `smoke.ps1 -Reviewer codex|claude` end-to-end against a real
authorized profile is part of #15, as is the deferred `[f5]` per-home lock (setup takes the **exclusive**
side; the review path takes a **shared** read — add the shared acquire around `attempt()`'s
authorize→probe→spawn when the setup lock lands, keyed on the effective home like `[f23]`).

5. **#15 Setup MCP tool + localhost page** — being built in parts.
   - **#15·part 1 — DONE, gate-approved (`rv-19320-19`):** the loopback approval page (`src/approval.rs`,
     `[f9]`) + `digest::random_hex_token` (CSPRNG). `ApprovalServer` binds loopback/ephemeral, mints a
     one-time token, and serves a single approval page (per-connection bounded threads, absolute
     read+write budgets, atomic first-writer-wins outcome, terminal-state invalidation, HTML-escaped,
     `no-store`); `open_in_browser` via `ShellExecuteW`.
   - **#15·part 2 + 3a — DONE, gate-approved (`rv-4656-4`):** the setup lock + provisional marker
     (`src/setup.rs`, `[f23]`) and the **authorize-only** setup MCP tool. `SetupSession` holds an OS-level
     cross-process exclusive lock keyed by the **canonicalized** effective home (`session::ExclusiveLock`)
     and a small in-progress marker (cleared on commit/drop, stale one reclaimed on next begin). The
     `cross_model_setup_profile` tool (`mcp.rs`/`tools.rs`) authorizes an **already-provisioned, signed-in**
     profile for this `launch_root`: parse → read-only identity probe (`resolve_home_identity`, the [f21]
     shared probe) → **human approval** (part 1) → re-confirm the account → commit `allowlist::authorize`,
     all under the lock and honouring tool-call cancellation **at the atomic rename boundary** (the store
     write threads a cancel flag). A home that does not exist yet is refused (vendor login is part 3b).
   - **#15·part 3b — TODO (the last piece):** **first-provision** and **staged re-login**, which need
     **vendor-login orchestration** (drive `codex login` / `claude login`, non-TTY browser callback, closed
     child stdin, bounded timeout, cancellation, **redaction** of login stdout/stderr) — the riskiest,
     least-specified part. These use `secure_profile_dir` to create/stage the home and the deferred
     **ownership-scoped created-dir rollback** (build it *with* the creation mechanism: record ownership
     before creating, refuse a pre-existing dir, retain the marker until cleanup succeeds — `src/setup.rs`
     module doc says so), plus the `[f20]` post-login handle-relative credential re-verify (primitive
     present). Also still deferred: the `[f5]` review-vs-setup **shared** lock (setup takes the exclusive
     side already; add the shared read around `attempt()`), and `smoke.ps1` end-to-end against a real
     provisioned profile.

## Resume-critical gotchas (not obvious from the code)

- **The probe's account check is no longer tautological (fixed in #14).** The identity+method probe
  now runs per non-ambient spawn in the worker (`attempt()` → `Reviewer::resolve_home_identity`, never
  cached — `auth_check` is liveness-only) and asserts against the **allowlist-authorized** account that
  `resolve_authorized_home_with_account` pins, not the home's own re-read. The post-review `switch_guard`
  re-reads the account and refuses a mid-flight swap before recording/delivery. So a profile silently
  re-logged to a different account is caught pre-spawn *and* post-review. (Historical note: task #13 left
  `expected = <home>`'s own fingerprint, which #14 replaced.)
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
