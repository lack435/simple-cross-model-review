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
non-existent home (`src/setup.rs:160`). This task adds **first-provision** (create home → vendor login
→ confirm account → authorize) and **re-login existing** (staged replacement), activating the whole
non-ambient path for new users.

Scope decisions (confirmed with the maintainer): implement **all of 3b in one gated chunk**;
**defer the real-OAuth, token-spending `smoke.ps1`** and land with unit tests over a mocked login
child — but a **no-token login-behaviour probe per vendor is required before landing** (spends no
tokens, completes no sign-in).

### The unifying design decision

Both operations use **one mechanism**: always create a **nonce-named staging directory**, drive the
vendor login into *that*, verify + confirm identity while holding its handle, and only then **rename it
into place**. The final `home` is *only ever produced by an atomic rename from a staging dir this run
exclusively created* — it is never created, written, or (in recovery) deleted directly. This collapses
the ownership-ambiguity and rename-window hazards the reviewer found (see review rounds below) into a
single invariant:

> **Recovery only ever deletes a nonce-named `staging`/`.old` directory named in the durable journal.
> A final `home` is never deleted by this feature.**

## Design

### Operation classification ([f2]) — after recovery, not before (f-r2.4)

Add an optional boolean tool arg (`login`, default false) so a side-effectful vendor login is
**opt-in**. **`run_setup` runs recovery first, then classifies:** resolve the effective home →
`SetupSession::begin` (acquire the per-home exclusive lock, **replay the journal to a consistent
state**, write a fresh marker) → *then* branch on `home.is_dir()`:

| home (post-recovery) | `login` | operation |
|---|---|---|
| absent | `true` | **first-provision** |
| absent | unset/false | today's "sign it in first, or pass login:true" refusal |
| present | `true` | **re-login existing** (staged replacement) |
| present | unset/false | **authorize-only** (unchanged, already built) |

Classifying *before* recovery would let an interrupted swap (home absent, `.old` present) be misread
as first-provision and skip repair — so recovery is inside `begin`, ahead of the `is_dir` check.
The chosen operation is shown on the approval page. Approval precedes any credential/allowlist side
effect ([f18]; the lock+marker are the bookkeeping exception, below).

### Redacted, contained, isolated login runner (`src/reviewer/mod.rs`)

```rust
pub struct LoginOutcome { pub success: bool, pub timed_out: bool, pub cancelled: bool, pub exit: Option<i32> }
pub fn run_login(command: Command, timeout: Duration, cancel: &AtomicBool) -> LoginOutcome
```

- **Redaction (type-level).** `LoginOutcome` has no text field; `run_login` drops the child's
  stdout/stderr without returning or `eprintln!`-ing them (tokens/URLs may appear there). Only exit /
  timed-out / cancelled drive control flow. Login failures produce a **generic** `setup_failure`,
  never `out.diagnostics()`. Per-reviewer command: Codex `codex login`; Claude
  `claude auth login --claudeai`. Never the api-key/access-token stdin flags.
- **Controlled environment (f-r2.7).** The login child gets `apply_controlled_env`
  (`env_clear` + `CONTROLLED_ENV_ALLOWLIST` + the home var → staging path, `reviewer/mod.rs:308`) as a
  **required, tested invariant** — dropping inherited `BROWSER`, proxy, provider-auth
  (`ANTHROPIC_*`/`OPENAI_*`/`CODEX_*`) and any vendor-specific variable that could redirect the OAuth
  flow or expose its URL. A hostile-environment test sets rogue `BROWSER`/provider-auth vars and
  asserts they are cleared. (Residual: corporate `HTTP(S)_PROXY` is dropped too, exactly as the
  already-working review path drops it; flagged for smoke validation.)
- **Config isolation from cwd (f-r2.6).** The child runs from a **freshly created, restrictive-DACL,
  verified, empty scratch directory this run owns** — **never** `neutral_dir(cfg)`, which resolves to
  the repo-settable `--state-dir` (`reviewer/mod.rs:257-262`). Vendor login-subcommand isolation flags
  are passed where supported. **This isolation is mandatory for every setup-path vendor CLI call**,
  including the post-login confirmation probe (below), not just the login child.
- **Process containment (f-r2.5).** Merely omitting `job.terminate()` is insufficient: the existing
  `JobObject` sets `KILL_ON_JOB_CLOSE`, so its `Drop`→`CloseHandle` kills survivors even after a clean
  exit (`winjob.rs:143-151`). Login therefore uses a **dedicated job created WITHOUT
  `KILL_ON_JOB_CLOSE`**: `run_login` **waits for the vendor process's natural exit** (the vendor *is*
  the callback server), and on **success closes the job without terminating** (browser/helpers
  survive); on **timeout/cancel** it calls `TerminateJobObject` explicitly. If the job cannot be
  created or the child assigned, login **fails closed** (refuse — we will not run a login we cannot
  contain on abort). Needs a small `winjob` no-kill-on-close variant. Child stdin is **closed**.
- **Machine-wide serialization (f4).** The vendor CLIs bind a **fixed localhost callback port**
  (Codex ~1455), so concurrent logins collide regardless of home or `CROSS_REVIEW_HOME`. Serialize
  with a **Windows named mutex** (`CreateMutexW`, session-scoped `Local\` name), held for the entire
  login+callback lifetime. A base-scoped lockfile is insufficient. Small named-mutex RAII primitive
  (FFI, no new crate). Login timeout ~3 min, separate from the 5-min `APPROVAL_TIMEOUT`.

### Confirmation probe, mandatorily isolated (f-r2.6)

After login, identity is confirmed by `resolve_home_identity`. Codex's probe is a pure `auth.json`
read (no CLI, no cwd) and is safe. **Claude's probe spawns `claude auth status` from
`neutral_dir(cfg)` with `--safe-mode` only conditional** (`claude.rs:294-301`) — a hostile
`--state-dir` config could influence the confirmation *before* authorization. The setup flow therefore
runs the confirmation probe through a **setup-scoped path that forces the owned scratch cwd and
mandatory isolation flags**, tested against a planted hostile config.

### [f20] credential re-verify — apply-then-verify (`src/winsec.rs`, `src/profile.rs`)

The dir DACL's ACEs are non-inheritable (`build_dacl`→`AddAccessAllowedAce` ace_flags 0,
`winsec.rs:687-694`; `apply_restrictive_dacl` sets `PROTECTED_DACL_SECURITY_INFORMATION`), so a
vendor-written credential file does *not* inherit the restrictive DACL and a **verify-only** check
would fail closed on every legitimate login. So we **apply then verify** — a deliberate refinement of
the impl-doc's "re-read its DACL". New `pub` primitive over the private `open_child_relative`
(`winsec.rs:444`):

```rust
/// Open a direct file child of `parent` handle-relative (RootDirectory + OBJ_DONT_REPARSE,
/// no share-delete), lock it to the restrictive DACL, then verify it.
pub fn secure_and_verify_child_file(parent: &OwnedHandle, leaf: &OsStr) -> io::Result<()>
// SecuredProfileDir::secure_and_verify_credential(&self, leaf) reads its held `handle` (profile.rs:223)
```

Re-verified per reviewer after a bounded poll-for-arrival: Codex `auth.json`; Claude
`.credentials.json` **and** `.claude.json`. Fail closed (→ rollback) on missing file, containment
failure, or DACL error. Structural containment (handle-relative, no reparse) proves the file is a
direct child of the held staging dir. Nested trees (Codex `sessions/`) are covered by the dir DACL for
traversal — recursion is an optional hardening follow-up.

### Identity binding (f3) — first-provision and re-login

Capture the **expected `ResolvedIdentity`** (account + method) from the probe run against the staging
dir **while its handle is held**; require `method == Subscription`. After the rename into place,
**re-open and hold the final `home` handle**, re-run the probe, and require **exact equality** with the
captured identity (not merely "is a subscription account"). The `AllowlistStore::authorize` commit
records **exactly** that account, with the final handle held across the write, binding the recorded
account to the verified object.

### Exclusive-create staging + ownership (f1, f-r2.2)

Staging dirs are created with a **new winsec `create_new_secured_child_dir(parent, leaf)`** =
`create_secured_child_dir` but with `FILE_CREATE` (exclusive, `winsec.rs:233` `FILE_CREATE=2`) instead
of `FILE_OPEN_IF`; a collision → distinct `AlreadyExists`. Because staging names are nonce-unique, an
`AlreadyExists` there means a crashed prior run's leftover, handled by recovery — never an adopt-then-
delete of someone else's dir. The final `home` is produced only by `rename(staging → home)`; on
Windows a directory rename **fails if the target exists**, so a `home` that appeared concurrently
(first-provision) is refused at the atomic rename and staging is rolled back — the rename *is* the
race-safe collision check, and no pre-existing home is ever deleted.

```rust
struct OwnedStaging { secured: Option<SecuredProfileDir>, staging: PathBuf, committed: bool }
// Drop: if !committed { self.secured = None; if remove_dir_all(&staging).is_err() { retain journal } }
//       (drop the no-delete-share handle BEFORE removal, else the hold blocks it)
```

## Crash-safety & recovery (f2, f3, f-r2.1, f-r2.3, f7)

**[f18] bookkeeping exception (f7).** `SetupSession::begin` creates the secured `auth` dir, takes the
per-home lock, and writes the marker **before** approval. This is **pre-approval bookkeeping that
creates and authorizes nothing** — no credential home, no credential file, no allowlist entry. The
[f18] invariant is precisely that **no credential write and no allowlist write happens before human
approval**; the lock+marker exist only to serialize approval and enable recovery.

**One write-ahead journal for both operations.** The marker is extended into a journal
`{ operation, home, staging_path, old_path, nonce, intent }`. It is written **before** each filesystem
mutation (write-ahead), so the nonce-named paths are durable before any rename that could crash:
before creating staging, before `rename(home → .old)`, before `rename(staging → home)`.

**Recovery is presence-driven, not phase-driven (f-r2.1).** `SetupSession::begin` replays under the
lock, deciding from the **actual presence** of the three journalled paths (the `intent` field is only
an advisory hint; renames are atomic and the nonce'd paths are unique, so presence determines state):

| home | `.old-nonce` | `staging-nonce` | meaning | action |
|---|---|---|---|---|
| present/absent | absent | present | login/verify done, not yet swapped in | **remove staging**; home untouched |
| absent | present | present | crashed between `home→.old` and `staging→home` | **restore `.old→home`**, remove staging |
| present | present | absent | crashed after `staging→home`, before cleanup | **remove `.old`**; keep home |
| present | absent | absent | complete, or crashed during authorize | consult the store; **never delete home**; clear journal |

**Never delete a final `home` (f3).** Once `home` exists via rename it is a valid, ACL-locked,
logged-in dir; recovery only reconciles the nonce'd `staging`/`.old` siblings and clears the journal.
So the commit is crash-safe: order is `rename(staging→home)` → journal `authorizing` →
`AllowlistStore::authorize` (its own atomic write is the **source of truth**; recovery never invents an
authorization) → clear journal. A crash after the store publishes but before the journal clears simply
leaves the journal to be cleared next `begin`; `home` is never removed, so the store never points at a
missing profile. A pre-commit failure always ends with the valid existing home restored (re-login) or
just an orphaned staging removed (first-provision), and **no** allowlist change.

**Retain-until-clean (f8).** If a required cleanup `remove_dir_all` fails (a lingering handle, a
transient lock), the journal is **retained** so the next `begin` retries, rather than clearing the
marker and stranding a credential-bearing staging dir with no owner.

**[f5] shared review lock spans the whole attempt, explicit protocol (f9).** Setup holds the per-home
**exclusive** lock; the review path takes a **shared** read lock **held across the entire `attempt()`**
(authorize → probe → spawn → switch guard, the child's whole lifetime), so a swap's `rename(home,…)`
never races a live review holding the home open. The existing `session::ExclusiveLock` opens
share-mode-zero and cannot express a shared reader, so refactor it to **`LockFileEx` byte-range locks**
(`LOCKFILE_EXCLUSIVE_LOCK` for setup, shared for review) on **one** per-home lock path opened
`FILE_SHARE_READ|WRITE`. Contract: readers coexist; reader blocks writer and vice-versa. Keyed on the
effective home like [f23].

**Fault-injection tests** exercise a crash between **every** filesystem mutation and its journal write,
for both operations, asserting recovery reaches a consistent state that never loses a valid home and
never deletes one it did not create.

### Testability seam

Factor the login step behind an injected callback so unit tests never run real OAuth:
`login: &dyn Fn(&Path, &RequestCancel) -> LoginOutcome`, production impl calls `run_login`. Same
`begin_with_wait`-style split already in `setup.rs:74`.

## Review rounds (Codex reviewer)

- **rv-20864-1 → -2** (7 findings): f1 exclusive-create ownership; f2 durable swap journal; f3
  identity binding; f4 named-mutex login lock; f5 no-token probe + no-reap; f6 login cwd isolation; f7
  [f18] bookkeeping exception. All accepted and folded in; f1/f7 marked resolved (f2–f6 froze at a
  stale `open` — the known frozen-ledger flake, issue #62 — their live remainder captured by f8/f9).
- **rv-20864-2** raised f8 (durable journal must cover first-provision) and f9 (shared-lock protocol).
- **rv-20864-3 (fresh authoritative)** raised 7 deeper findings, all folded into the **unified
  staged-create model** above: **f-r2.1** stale swap-phase across rename windows → presence-driven WAL
  recovery + fault injection; **f-r2.2** pre-create journal could delete a colliding home → only ever
  create/delete nonce'd staging, final home only via rename; **f-r2.3** first-provision commit had no
  durable state → never-delete-home + store-as-source-of-truth ordering; **f-r2.4** recovery bypassed
  when home absent → recovery runs in `begin` before classification; **f-r2.5** no-reap conflicts with
  `KILL_ON_JOB_CLOSE` → dedicated no-kill-on-close job, fail-closed if uncontainable; **f-r2.6** Claude
  confirmation probe still ran from `neutral_dir` → mandatory isolated setup-scoped probe; **f-r2.7**
  login child had no controlled-env contract → required, tested `apply_controlled_env`.

## Files to modify

- **`src/winsec.rs`** — `secure_and_verify_child_file` (f20); `create_new_secured_child_dir`
  (exclusive create, f1); named-mutex RAII primitive (f4); tests.
- **`src/winjob.rs`** — a no-kill-on-close job variant + explicit terminate/detach for login (f-r2.5).
- **`src/session.rs`** — refactor `ExclusiveLock` to `LockFileEx` `Shared`/`Exclusive` on one path
  (f9); recovery-aware `SetupSession::begin` (WAL journal replay, f2/f8/f-r2.1..4).
- **`src/profile.rs`** — `SecuredProfileDir::secure_and_verify_credential`; staging-create path.
- **`src/reviewer/mod.rs`** — `LoginOutcome` + `run_login` (contained, isolated, controlled-env,
  closed stdin); per-reviewer login command builder.
- **`src/reviewer/codex.rs` / `claude.rs`** — login command shapes; credential-file names; a
  setup-scoped, mandatorily-isolated confirmation probe (f-r2.6).
- **`src/setup.rs`** — `login` arg, recovery-before-classification, `OwnedStaging`, unified
  staged-create + rename-into-place for both operations, identity binding, injected-login seam, tests.
- **the review path (`attempt()`)** — [f5] shared-read lock spanning the whole attempt.
- **`src/mcp.rs` / `src/tools.rs`** — `login` boolean in the setup schema + description (setup blocks
  minutes while the human OAuths).
- **`docs/reviewer-account-profiles-status.md`** — mark 3b done; record the deferred real smoke, the
  no-token probe requirement, and the non-TTY-login / [f20] apply-then-verify findings.

## Verification

- `cargo test` — unit tests: exclusive-create collision refused (f1); first-provision rename refuses a
  concurrently-appeared home (f-r2.2); identity-equality binding (f3); non-subscription rejected;
  first-provision + re-login happy paths; **fault-injection recovery** between every mutation/journal
  write for both operations, incl. crash-after-store-publish keeps home (f3/f-r2.1/f-r2.3);
  recovery-runs-before-classification (f-r2.4); retain-journal-on-failed-cleanup (f8); [f20] primitive
  (apply+verify child; widened-ACL negative); named-mutex mutual exclusion (f4); shared/exclusive lock
  (readers coexist, reader⇄writer exclude) (f9); controlled-env clears rogue vars (f-r2.7);
  hostile-cwd-config for login **and** the confirmation probe (f6/f-r2.6); login fails closed when
  the job is uncontainable (f-r2.5).
- `.\build.ps1` — fmt, clippy `-D warnings`, tests, release (needs agent MCP sessions unloaded to
  restage `dist\`).
- **No-token login-behaviour probe, required before landing (f5):** per vendor, spawn login into a
  scratch home with stdin closed and a short timeout; confirm it auto-opens the browser, binds its
  callback, and does not block on stdin; kill before OAuth completes. No tokens, no sign-in. **If it
  fails, the device-auth fallback is not localized** — it needs an explicit flow surfacing the user
  code/URL on our own `ApprovalServer` page (a `LoginOutcome`/flow "needs user code" variant), designed
  and reviewed separately.
- **Deferred (needs the maintainer + tokens):** `smoke.ps1 -Reviewer codex|claude` real end-to-end —
  first-provision lands the credential in the *dedicated* home (not `~/.codex`/`~`), the probe reports
  the account, allowlist entry + restrictive DACLs on home and credential file; then a re-login account
  switch; then a cancelled/failed login leaving the prior home intact.

## Risks / unknowns to carry into implementation

1. Non-TTY vendor login completing headless (stdin closed + browser auto-open) — de-risked by the
   no-token probe before landing; device-auth fallback has an explicit, separately-reviewed flow.
2. [f20] apply-then-verify (not verify-only) — deliberate refinement of the doc wording.
3. Exact credential-file set + write timing per reviewer (poll-for-arrival guards the race).
4. Total tool-call time budget (approval ≤5 min + login minutes) vs the MCP client timeout.
5. Whether each login subcommand accepts config-isolation flags (f6) — verified per-CLI in impl; the
   owned-scratch-cwd + controlled-env are the guaranteed floor.
6. Corporate proxy vars dropped by the controlled env (f-r2.7) — same as the working review path;
   confirm in smoke.
7. Named-mutex scope (`Local\` per-session) matches the per-user callback port; confirm in impl.
