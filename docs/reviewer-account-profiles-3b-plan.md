# Plan — #15 part 3b: vendor-login orchestration (first-provision + staged re-login)

> **Status:** proposed, under cross-model review. Not yet implemented. This is the design that must
> reach APPROVE through this repository's own `cross-review` gate before code is written.

## Context

`cross-review` runs code review under a dedicated, ACL-locked credential home instead of the
developer's ambient login. The "reviewer account profiles" feature (branch `docs/multi-account-plan`)
is complete except for its last, riskiest piece: **provisioning a brand-new profile home via the
vendor's own login**, and **re-logging an existing home**.

Today [`src/setup.rs`](../src/setup.rs) `run_setup` implements only **authorize-only** setup — it
authorizes a repo to use a home that *already exists and is signed in*, and hard-refuses a
non-existent home (`src/setup.rs:160`). So the feature is unusable for anyone who hasn't already
manually pointed a vendor CLI at a profile dir. This task adds the two missing operations from the
[f2] design in [`reviewer-account-profiles-impl.md`](reviewer-account-profiles-impl.md):
**first-provision** (create home → vendor login → confirm account → authorize) and **re-login
existing** (staged replacement into a fresh dir, then swap in). It activates the whole non-ambient
path for new users.

Scope decisions (confirmed with the maintainer): implement **all of 3b in one gated chunk**;
**defer the real-OAuth `smoke.ps1`** and land with unit tests over a mocked login child.

Two correctness findings, verified against the code during planning, drive the design:

- **[f20] must apply-then-verify, not verify-only.** The dir DACL's ACEs are non-inheritable
  (`build_dacl` → `AddAccessAllowedAce` with ace_flags 0, `winsec.rs:687-694`; `apply_restrictive_dacl`
  sets `PROTECTED_DACL_SECURITY_INFORMATION`, `winsec.rs:712`). A credential file the vendor writes
  inside the home does *not* inherit that DACL, so `verify_restrictive_dacl` alone
  (`winsec.rs:729`, requires `SE_DACL_PROTECTED` + an exact ACE set) would **fail closed on every
  legitimate login**. We must **apply** the restrictive DACL to the credential file (handle-relative)
  and *then* verify — locking it down and proving structural containment. This is a deliberate,
  documented refinement of the literal [f20] wording ("re-read its DACL"); the security intent
  (credential file contained inside the verified dir *and* locked to this user) is met more fully.
- **Non-TTY vendor login is the #1 unverified assumption.** Bare `codex login` /
  `claude auth login --claudeai` self-host a localhost OAuth callback and auto-open the default
  browser; nothing reads stdin or needs a TTY *in principle*, and this process already auto-opens a
  browser via `ShellExecuteW` (`approval::open_in_browser`). But that it completes headless with
  stdin closed is **unproven until smoke runs** (deferred). We implement the browser-auto-open path
  as primary and keep the code shaped so a device-auth fallback is a localized change. This risk is
  recorded in the status doc.

## Design

### Operation classification ([f2])

Add an optional boolean tool arg (`login`, default false) so a side-effectful vendor login is
**opt-in**. Classification in `run_setup`:

| home | `login` | operation |
|---|---|---|
| absent | `true` | **first-provision** |
| absent | unset/false | today's "sign it in first, or pass login:true" refusal |
| present | `true` | **re-login existing** (staged replacement) |
| present | unset/false | **authorize-only** (unchanged, already built) |

The chosen operation is shown on the existing approval page so the human sees which of the three they
are authorizing (and, for re-login, that a working home will be replaced). Approval still precedes any
side effect ([f18]).

### Redacted login runner (`src/reviewer/mod.rs`)

```rust
pub struct LoginOutcome { pub success: bool, pub timed_out: bool, pub cancelled: bool, pub exit: Option<i32> }
pub fn run_login(command: Command, timeout: Duration, cancel: &AtomicBool) -> LoginOutcome
```

Built on the existing job-object'd child runner (`run`/`run_observed`). Child gets
`apply_controlled_env(cmd, home_var, staging_home)` + `current_dir(neutral_dir(cfg))` + **stdin
closed**. **Redaction is enforced at the type level**: `LoginOutcome` has no text field, and
`run_login` drops the child's stdout/stderr without returning or `eprintln!`-ing them (tokens/URLs may
appear there). Only exit code / timed-out / cancelled drive control flow. Login failures produce a
**generic** `setup_failure`, never `out.diagnostics()`. Per-reviewer command builder keyed on
`ReviewerKind`: Codex `codex login`; Claude `claude auth login --claudeai`. Never the
api-key/access-token stdin flags.

A **global login lock** (`ExclusiveLock` on `{base}\auth\login.lock`) serializes all logins
machine-wide: the vendor CLIs bind a fixed localhost callback port (Codex ~1455), so two concurrent
logins of *different* homes would otherwise collide. Login gets its own timeout constant (~3 min),
separate from the 5-min `APPROVAL_TIMEOUT`.

### [f20] credential re-verify (`src/winsec.rs`, `src/profile.rs`)

New `pub` primitive wrapping the existing private `open_child_relative` (`winsec.rs:444`):

```rust
/// Open a direct file child of `parent` handle-relative (RootDirectory + OBJ_DONT_REPARSE,
/// no share-delete), lock it to the restrictive DACL, then verify it.
pub fn secure_and_verify_child_file(parent: &OwnedHandle, leaf: &OsStr) -> io::Result<()>
```

Method on `SecuredProfileDir` (reads its currently-`dead_code` private `handle`, `profile.rs:223`):

```rust
pub fn secure_and_verify_credential(&self, leaf: &OsStr) -> io::Result<()>
```

Files re-verified per reviewer, after a bounded poll-for-arrival (a CLI may flush just after exit):
Codex `auth.json`; Claude `.credentials.json` **and** `.claude.json`. Fail closed (→ rollback) on a
missing file, containment failure, or DACL error. Structural containment (handle-relative, no reparse)
proves the file is a direct child of the dir we hold; nested trees (Codex `sessions/`) are covered by
the dir's own DACL for traversal — recursing is an optional hardening follow-up.

### Ownership-scoped rollback ([f2]/[f23]) — `src/setup.rs`

RAII guard entangled with directory creation (the `setup.rs` module doc forbids a standalone
record-a-path API):

```rust
struct OwnedProvision { secured: Option<SecuredProfileDir>, path: PathBuf, committed: bool }
//   create_fresh(...)   first-provision: REFUSE a pre-existing dir, then secure_profile_dir. The
//                       refuse-if-exists check *is* the ownership record — rollback can only ever
//                       delete a dir this run created.
//   create_staging(...) re-login: secure_profile_dir on a fresh nonce'd staging path we always own.
//   disarm(self)        success: keep the dir.
// Drop: if !committed { self.secured = None; remove_dir_all(&path) }
//                       drop the no-delete-share handle BEFORE removal, else the hold blocks it.
```

Any early return between creation and the allowlist commit unwinds cleanly: `OwnedProvision` drops
(removes only what this run created), `SetupSession` drops (clears marker, releases per-home lock),
nothing committed. "Retain the marker until cleanup succeeds" falls out of the guard living to
function return.

### Staged-replacement swap (re-login) — `src/setup.rs`

Windows has no atomic dir swap (`ReplaceFile` is files-only; `MoveFileEx` cannot swap two dirs), so
do a guarded three-move with `home`, `home.staging-{nonce}`, `home.old-{nonce}`:

1. login + [f20] re-verify + identity probe on the staging dir **while holding its handle**.
2. drop the staging handle (`owned.secured = None`) — releases the no-delete-share hold.
3. if `home` exists: `rename(home, home.old-{nonce})`.
4. `rename(home.staging-{nonce}, home)`.
5. re-open `home` no-follow, `verify_restrictive_dacl`, re-run identity probe (closes the drop-handle window).
6. commit allowlist; then best-effort remove `home.old-{nonce}`.

Recovery: fail after (3) before (4) → move `.old` back to `home`; fail after (4) → new home is valid
and locked, leave it and report the un-committed allowlist (the review path stays refused until
re-run); fail before (3) → guard deletes staging, existing home untouched.

**[f5] is coupled to re-login, not separable.** Steps 3–4 rename the live `home`; a concurrent review
under it (the review path takes no lock yet) could fault the rename or observe a half-swapped state.
First-provision is safe (no home ⇒ unauthorized ⇒ no review running). So this task also adds the
**[f5] shared-read lock on the review path**: setup already holds the exclusive side via
`SetupSession`; add a *shared* acquire around the worker's `attempt()` authorize→probe→spawn, keyed on
the effective home like [f23].

### `run_setup` state machine (post-classification)

parse+classify → `SetupSession::begin` (per-home lock+marker; for re-login keyed on the *final* home)
→ (re-login) read-only probe of the existing home for the approval display → **approval** (existing
pattern, rows name the operation) → global login lock → `OwnedProvision::create_fresh`/`create_staging`
→ `run_login` (cancel flag, bounded) → on failure return (RAII rollback) → [f20] re-verify → identity
probe must be `Subscription`, capture account → (re-login) staged swap → `is_cancelled()` check →
`AllowlistStore::authorize(entry, cancel_flag)` → `disarm()` + `session.commit()`.

### Testability seam

Factor the login step behind an injected callback so unit tests never run real OAuth:
`login: &dyn Fn(&Path, &RequestCancel) -> LoginOutcome`, production impl calls `run_login`. Same
`begin_with_wait`-style split already used in `setup.rs:74`.

## Files to modify

- **`src/winsec.rs`** — `secure_and_verify_child_file` + unit tests (apply+verify child; widened-ACL negative).
- **`src/profile.rs`** — `SecuredProfileDir::secure_and_verify_credential`.
- **`src/reviewer/mod.rs`** — `LoginOutcome` + `run_login` + per-reviewer login command builder.
- **`src/reviewer/codex.rs` / `claude.rs`** — expose login command shape + credential-file names.
- **`src/setup.rs`** — classification, `login` arg, `OwnedProvision`, first-provision + staged
  re-login flows, injected-login seam, tests.
- **the review path (`attempt()`)** — [f5] shared-read lock around authorize→probe→spawn.
- **`src/mcp.rs` / `src/tools.rs`** — add the `login` boolean to the setup tool schema + description
  (note that setup blocks minutes while the human OAuths).
- **`docs/reviewer-account-profiles-status.md`** — mark 3b done; record the deferred smoke + the
  non-TTY-login and [f20]-apply-then-verify findings.

## Verification

- `cargo test` — new unit tests (first-provision happy/failure/timeout/cancel/refuse-pre-existing/
  non-subscription; re-login happy/swap-recovery/login-failure-leaves-home-intact; [f20] primitive;
  swap mechanics on plain temp dirs).
- `.\build.ps1` — fmt, clippy `-D warnings`, tests, release (needs agent MCP sessions unloaded to
  restage `dist\`).
- **Optional de-risk (no tokens, no completed sign-in):** spawn `codex login` into a scratch
  `CODEX_HOME` with a short timeout and observe whether it auto-opens the browser + hosts the callback
  without a TTY, then kill before completing — a cheap check of the #1 unknown while full smoke stays
  deferred.
- **Deferred (needs the maintainer + tokens):** `smoke.ps1 -Reviewer codex|claude` real end-to-end —
  first-provision lands the credential in the *dedicated* home (not `~/.codex`/`~`), identity probe
  reports the account, allowlist entry + restrictive DACLs on home and credential file; then a
  re-login account switch; then a cancelled/failed login leaving the prior home intact.

## Risks / unknowns to carry into review

1. Non-TTY vendor login completing headless with stdin closed + browser auto-open — **unverified
   until smoke**; primary path implemented, device-auth fallback kept localized.
2. [f20] apply-then-verify (not verify-only) — deviation from literal doc wording; confirm the
   intended contract.
3. Exact credential-file set + write timing per reviewer (poll-for-arrival guards the race).
4. Total tool-call time budget (approval ≤5 min + login minutes) vs the MCP client timeout.
5. Fixed OAuth callback-port collision across concurrent logins → global login lock.
6. Swap-vs-concurrent-review safety → why [f5] is bundled into this task.
