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
**defer the real-OAuth, token-spending `smoke.ps1`** and land with unit tests over a mocked login
child — but a **no-token login-behaviour probe per vendor is required before landing** (see f5
resolution below), which spends no tokens and completes no sign-in.

Two correctness findings, verified against the code during planning, drive the design:

- **[f20] must apply-then-verify, not verify-only.** The dir DACL's ACEs are non-inheritable
  (`build_dacl` → `AddAccessAllowedAce` with ace_flags 0, `winsec.rs:687-694`; `apply_restrictive_dacl`
  sets `PROTECTED_DACL_SECURITY_INFORMATION`, `winsec.rs:712`). A credential file the vendor writes
  inside the home does *not* inherit that DACL, so `verify_restrictive_dacl` alone
  (`winsec.rs:729`, requires `SE_DACL_PROTECTED` + an exact ACE set) would **fail closed on every
  legitimate login**. We must **apply** the restrictive DACL to the credential file (handle-relative)
  and *then* verify — locking it down and proving structural containment. This is a deliberate,
  documented refinement of the literal [f20] wording ("re-read its DACL").
- **Non-TTY vendor login is the highest-risk assumption.** Bare `codex login` /
  `claude auth login --claudeai` self-host a localhost OAuth callback and auto-open the default
  browser; nothing reads stdin or needs a TTY *in principle*, and this process already auto-opens a
  browser via `ShellExecuteW` (`approval::open_in_browser`). Its headless viability is de-risked by a
  no-token probe before landing (f5), with the full real-OAuth end-to-end deferred.

## Review-response log (rv-20864-1, Codex reviewer)

The first plan review returned request-changes with 7 findings. All were accepted; the design below
incorporates each. Summary of how:

| # | Finding | Resolution |
|---|---|---|
| f1 | refuse-if-exists is a TOCTOU, not ownership proof | First-provision uses an **atomic exclusive create** (`FILE_CREATE`) as the ownership record — a collision is refused, never "adopted then rolled back". |
| f2 | staged swap not crash-safe/reversible | Add a **durable swap journal** in the marker + **recovery on `SetupSession::begin`** (replayed under the lock) + phase-aware restore; the [f5] shared review lock is held across the **whole** attempt. |
| f3 | final identity not bound to the confirmed one | Retain the expected `ResolvedIdentity`; require **exact equality** (account+method) on the post-swap re-probe; hold the final dir handle through the allowlist commit; authorize exactly the confirmed account. |
| f4 | login lock only base-scoped | Replace with a **machine/session-wide named mutex** held for the whole login+callback lifetime. |
| f5 | primary login path unverified; job reaping | **No-token probe per vendor required before landing**; login runner **waits for natural exit and does not reap on success**; device-auth fallback given an **explicit flow interface**, not "localized". |
| f6 | login config isolation not guaranteed | Login runs from a **freshly created, verified, owned, empty scratch dir** (never `neutral_dir`/`--state-dir`) + vendor config-isolation flags; **hostile-cwd-config test** added. |
| f7 | approval-before-side-effects wording too broad | State explicitly that the lock+marker are **pre-approval bookkeeping that create/authorize nothing**; the [f18] invariant is no credential write and no allowlist write before approval. |

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
credential or allowlist side effect ([f18]; see the bookkeeping exception under Crash-safety).

### Redacted login runner (`src/reviewer/mod.rs`)

```rust
pub struct LoginOutcome { pub success: bool, pub timed_out: bool, pub cancelled: bool, pub exit: Option<i32> }
pub fn run_login(command: Command, timeout: Duration, cancel: &AtomicBool) -> LoginOutcome
```

**Redaction is enforced at the type level**: `LoginOutcome` has no text field, and `run_login` drops
the child's stdout/stderr without returning or `eprintln!`-ing them (tokens/URLs may appear there).
Only exit code / timed-out / cancelled drive control flow. Login failures produce a **generic**
`setup_failure`, never `out.diagnostics()`. Per-reviewer command builder keyed on `ReviewerKind`:
Codex `codex login`; Claude `claude auth login --claudeai`. Never the api-key/access-token stdin flags.

**Process lifetime (f5).** The vendor process *is* the localhost callback server; it stays alive until
the OAuth redirect completes, then exits after writing the credential. So `run_login` **waits for the
vendor process's natural exit** (bounded by the login timeout) and, on success, **does not** call
`job.terminate()` — the existing review runner reaps the whole job tree after the direct child exits
(`reviewer/mod.rs:1176`), which could kill a login helper or the just-opened browser. Job termination
is used **only** on timeout or cancellation, to guarantee no orphaned child survives an abort. Child
gets stdin **closed** (empty payload → immediate EOF via the existing stdin thread).

**Config isolation (f6).** The login child runs from a **freshly created, restrictive-DACL, verified,
empty scratch directory that this run owns** — **not** `neutral_dir(cfg)`, which resolves to the
repo-settable `--state-dir` (`reviewer/mod.rs:257-262`) and could carry a hostile `config`/hooks that
an auth command loads from cwd. In addition, pass the vendor config-isolation flags the *login*
subcommand accepts (to be verified per-CLI during impl: Codex `-c`/`--ignore-user-config`-style;
Claude `--safe-mode`/`--strict-mcp-config`), falling back to the clean-cwd guarantee when a flag is
unsupported. A **hostile-cwd-config test** plants a malicious config in a scratch cwd and asserts the
login command builder never runs from it.

**Machine-wide login serialization (f4).** The vendor CLIs bind a **fixed localhost callback port**
(Codex ~1455), so two concurrent logins collide regardless of profile home *or* `CROSS_REVIEW_HOME`.
Serialize with a **Windows named mutex** (`CreateMutexW`, session-scoped `Local\` name — the callback
port is per-user-session), acquired before spawning login and **held for the entire login+callback
lifetime**. A base-scoped lockfile is insufficient because different bases would not share it. Add a
small named-mutex RAII primitive (FFI in the `winjob`/`winsec` style — no new crate). Login gets its
own timeout constant (~3 min), separate from the 5-min `APPROVAL_TIMEOUT`.

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
record-a-path API). **Ownership is proved by an atomic exclusive create, not a prior existence check
(f1):**

- New winsec `create_new_secured_child_dir(parent, leaf)` = `create_secured_child_dir` but with
  `FILE_CREATE` (exclusive) instead of `FILE_OPEN_IF`; `ERROR_ALREADY_EXISTS`/`OBJECT_NAME_COLLISION`
  maps to a distinct `AlreadyExists` error. The **successful exclusive create is the ownership record**
  — there is no window in which a pre-existing dir could be adopted and later deleted by rollback.
  (`FILE_CREATE = 2` already exists in `winsec.rs:233`; `write_secured_file` already relies on it.)

```rust
struct OwnedProvision { secured: Option<SecuredProfileDir>, path: PathBuf, committed: bool }
//   create_fresh(...)   first-provision: EXCLUSIVE create of the profile leaf; AlreadyExists → refuse
//                       ("home already exists; use re-login"). No probe-then-create race.
//   create_staging(...) re-login: exclusive create of a fresh nonce'd staging path we always own.
//   disarm(self)        success: keep the dir.
// Drop: if !committed { self.secured = None; remove_dir_all(&path) }
//                       drop the no-delete-share handle BEFORE removal, else the hold blocks it.
```

Any early return between creation and the allowlist commit unwinds cleanly: `OwnedProvision` drops
(removes only what this run exclusively created), `SetupSession` drops (clears marker after running any
needed recovery, releases the per-home lock), nothing committed.

### Identity binding ([f19]/[f4]/f3) — first-provision and re-login

Capture the **expected `ResolvedIdentity`** (account + method) from the probe run **against the home
while its handle is held**. The allowlist entry is committed with **exactly that account**, and:

- **first-provision:** the probe under the held handle is the confirmed identity; require
  `method == Subscription`; authorize that exact account.
- **re-login:** after the swap, **re-open the final `home` no-follow and hold it**, then re-run the
  identity probe and require **exact equality** with the pre-swap confirmed identity (not merely "is a
  subscription account"). The held final handle is kept across the `AllowlistStore::authorize` commit,
  so the account the store records is bound to the verified object, closing the "probe reads a file by
  path after the handle was dropped" gap the reviewer flagged.

### Staged-replacement swap (re-login) — `src/setup.rs`

Windows has no atomic dir swap (`ReplaceFile` is files-only; `MoveFileEx` cannot swap two dirs), so
do a **journalled** three-move with `home`, `home.staging-{nonce}`, `home.old-{nonce}`. See
Crash-safety for the durable journal + recovery that makes each step reversible.

1. login + [f20] re-verify + identity probe on the staging dir **while holding its handle**.
2. write journal `phase=swap-out {home, old, staging}`; drop the staging handle.
3. if `home` exists: `rename(home, home.old-{nonce})`; update journal `phase=swap-in`.
4. `rename(home.staging-{nonce}, home)`; update journal `phase=verify`.
5. re-open `home` no-follow + hold, `verify_restrictive_dacl`, re-run identity probe, require exact
   equality with the pre-swap identity (f3).
6. commit allowlist (with the held final handle alive); update journal `phase=commit`; then
   best-effort remove `home.old-{nonce}`; clear the journal.

### Testability seam

Factor the login step behind an injected callback so unit tests never run real OAuth:
`login: &dyn Fn(&Path, &RequestCancel) -> LoginOutcome`, production impl calls `run_login`. Same
`begin_with_wait`-style split already used in `setup.rs:74`.

## Crash-safety & recovery ([f18] bookkeeping exception + f2 + f7)

**[f18] bookkeeping exception (f7).** `SetupSession::begin` creates the secured `auth` dir, takes the
per-home lock, and writes the provisional marker **before** approval. This is **pre-approval
bookkeeping that creates and authorizes nothing** — no credential home, no credential file, no
allowlist entry. The [f18] invariant is precisely that **no credential write and no allowlist write
happens before human approval**; the lock+marker exist to serialize the approval itself and to enable
crash recovery. The plan states this exception explicitly rather than leaving "no side effects before
approval" to imply the marker too.

**Durable swap journal + recovery (f2).** The provisional marker is extended into a swap journal: when
a re-login begins its swap it records `{ phase, home, old_path, staging_path, nonce }` (atomic write,
the existing `atomic_write`). `SetupSession::begin` **no longer blindly clears** a found marker — under
the lock it first **replays** any swap journal to a consistent state:

- `phase=swap-out` (or no rename done): remove an orphaned staging dir; home is intact.
- `phase=swap-in` (home moved to `.old`, staging not yet in place): **restore** `rename(.old → home)`;
  remove staging.
- `phase=verify`/`commit` with `home` present and `.old` present: the new home is in place; remove the
  leftover `.old`. (A `phase=commit` means the allowlist may or may not have been written; the store's
  own atomic write is the source of truth — recovery never invents an authorization.)
- once consistent, clear the journal/marker and proceed.

Because recovery runs **under the per-home exclusive lock**, no other setup or the (shared-locked)
review path can observe a half-swapped home. A pre-commit failure therefore always ends with the
**valid existing home restored** and **no allowlist change**; a post-`swap-in` failure leaves the new,
valid, locked home in place with the allowlist simply not updated (review path stays refused until
re-run) — never a stranded-absent home and never a destroyed valid home.

**[f5] shared review lock spans the whole attempt (f2).** Setup holds the per-home **exclusive** lock;
the review path takes a **shared** read lock that must be **held across the entire `attempt()`** —
authorize → identity probe → spawn → switch guard, i.e. the child's whole lifetime — not just a
sub-step. Only then can the swap's `rename(home, …)` never race a live review that has the home open.
Keyed on the effective home like [f23].

## Files to modify

- **`src/winsec.rs`** — `secure_and_verify_child_file` (f20); `create_new_secured_child_dir`
  (exclusive create, f1); a named-mutex RAII primitive (f4) + unit tests.
- **`src/profile.rs`** — `SecuredProfileDir::secure_and_verify_credential`; an exclusive-create
  provisioning path for first-provision.
- **`src/reviewer/mod.rs`** — `LoginOutcome` + `run_login` (no-reap-on-success, closed stdin, own
  scratch cwd) + per-reviewer login command builder.
- **`src/reviewer/codex.rs` / `claude.rs`** — login command shape + credential-file names + any
  login-subcommand isolation flags.
- **`src/setup.rs`** — classification, `login` arg, `OwnedProvision`, first-provision + staged
  re-login flows, swap journal + recovery in `SetupSession::begin`, identity binding, injected-login
  seam, tests.
- **the review path (`attempt()`)** — [f5] shared-read lock spanning the whole attempt.
- **`src/mcp.rs` / `src/tools.rs`** — add the `login` boolean to the setup tool schema + description
  (note that setup blocks minutes while the human OAuths).
- **`docs/reviewer-account-profiles-status.md`** — mark 3b done; record the deferred real smoke, the
  no-token probe requirement, and the non-TTY-login / [f20] apply-then-verify findings.

## Verification

- `cargo test` — new unit tests: first-provision happy/failure/timeout/cancel; **exclusive-create
  collision is refused** (f1); non-subscription rejected; **identity-equality binding** (f3);
  re-login happy; **swap journal recovery** for each phase (swap-out/swap-in/verify) on plain temp
  dirs (f2); login-failure-leaves-home-intact; [f20] primitive (apply+verify child; widened-ACL
  negative); named-mutex mutual exclusion (f4); **hostile-cwd-config** test (f6).
- `.\build.ps1` — fmt, clippy `-D warnings`, tests, release (needs agent MCP sessions unloaded to
  restage `dist\`).
- **No-token login-behaviour probe, required before landing (f5):** for each vendor, spawn the login
  command into a scratch home with stdin closed and a short timeout, and confirm it (a) auto-opens the
  browser, (b) binds its localhost callback, (c) does not block on stdin — then kill before completing
  OAuth. Spends no tokens, completes no sign-in. Validates the headless assumption. **If it fails, the
  device-auth fallback is not localized:** it requires an explicit flow that surfaces the user
  code/URL on our own `ApprovalServer` loopback page (a `LoginOutcome`/flow variant carrying a
  "needs user code" state), designed and reviewed separately before use.
- **Deferred (needs the maintainer + tokens):** `smoke.ps1 -Reviewer codex|claude` real end-to-end —
  first-provision lands the credential in the *dedicated* home (not `~/.codex`/`~`), identity probe
  reports the account, allowlist entry + restrictive DACLs on home and credential file; then a
  re-login account switch; then a cancelled/failed login leaving the prior home intact.

## Risks / unknowns to carry into implementation

1. Non-TTY vendor login completing headless with stdin closed + browser auto-open — de-risked by the
   no-token probe (f5) before landing; device-auth fallback has an explicit, separately-reviewed flow.
2. [f20] apply-then-verify (not verify-only) — deliberate refinement of the doc wording; rationale
   above.
3. Exact credential-file set + write timing per reviewer (poll-for-arrival guards the race).
4. Total tool-call time budget (approval ≤5 min + login minutes) vs the MCP client timeout.
5. Whether each login subcommand accepts the vendor config-isolation flags (f6) — verified per-CLI
   during impl; clean-scratch-cwd is the guaranteed fallback.
6. Named-mutex scope (`Local\` per-session vs `Global\`) — per-session matches the per-user callback
   port; confirmed during impl.
