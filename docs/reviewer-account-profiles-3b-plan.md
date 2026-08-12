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
- **Process containment (f-r2.5, f10).** Merely omitting `job.terminate()` is insufficient: the
  existing `JobObject` sets `KILL_ON_JOB_CLOSE`, so its `Drop`→`CloseHandle` kills survivors even after
  a clean exit (`winjob.rs:143-151`). And it **assigns the child to the job *after* spawn**
  (`winjob.rs:121-131`, whose own comment notes `std::process::Command` "cannot spawn suspended, so a
  child that spawns a grandchild before this call lands would leave that grandchild outside the job") —
  an uncontained window unacceptable for a login we must be able to abort cleanly. Login therefore uses
  a **dedicated job that keeps `KILL_ON_JOB_CLOSE`** (f-b1) and **creation-time association**: create
  the child **suspended** (`CREATE_SUSPENDED`), `AssignProcessToJobObject` while suspended, **then
  resume**, so every descendant is in the job from the first instruction. This needs process creation
  beyond `std::process::Command` (`creation_flags(CREATE_SUSPENDED)` + resuming the primary thread, or a
  direct `CreateProcessW`), scoped to the login runner (the existing review runner keeps its
  pre-existing post-spawn assignment; this plan does not weaken it). **`KILL_ON_JOB_CLOSE` is retained,
  not dropped (f-b1):** if the *setup process itself crashes* mid-login, the OS closes the job handle
  and **reaps the still-running login child/helpers**, so a later recovery can never race a login that
  is still writing staging. This does not harm the success path because success closes the job **only
  after** the quiescence wait below (`ActiveProcesses == 0`) — there is nothing left in-job to kill —
  and the browser is out-of-job, so it is unaffected either way.
- **Browser ownership + process quiescence (f14).** The **browser is out-of-job and never gated on**:
  the vendor launches it via the shell (`ShellExecute`/`start`), handing the URL to a detached or
  already-running browser that is not our descendant, and the vendor **login process** (the callback
  server) exits when the OAuth redirect completes, *independently* of the browser. **The no-token probe
  must confirm exactly this** — that the login process exits on callback completion, not on browser
  close (else waiting on it would hang on the user's browser). `run_login` **waits for the login
  process's exit**, then, before [f20]-verify + rename, **waits (bounded) for in-job helper quiescence**
  (`QueryInformationJobObject` `ActiveProcesses == 0`; the browser is out-of-job so excluded) so no
  lingering helper is still writing the staging dir during verification or the rename; the rename also
  retries on a transient sharing loss (as `write_secured_file` does). On success the job is closed only
  **after** this quiescence wait, so `KILL_ON_JOB_CLOSE` (retained, f-b1) has nothing left to kill. On
  **timeout/cancel** it calls
  `TerminateJobObject`, **waits (bounded) for `ActiveProcesses == 0`** before cleaning up staging (so
  `remove_dir_all` never races a dying writer), and **invalidates the callback** — tearing the job down
  kills the callback server, so a late OAuth redirect hits a dead port and cannot complete after we have
  moved on. If the job cannot be created, the child cannot be associated, a terminate fails, or the job
  does not quiesce within the bound, login **fails closed** — refuse or report an uncontained-abort
  error; we never run, abandon, or clean up after a login we cannot prove contained. Child stdin is
  **closed**.
- **Containment limits, stated honestly (f18).** `ActiveProcesses == 0` proves only that *job members*
  are gone. Two escape routes exist and are handled as follows: (1) **job breakaway** — the login job
  is created **without** `JOB_OBJECT_LIMIT_BREAKAWAY_OK` / silent-breakaway, so a child cannot detach
  itself from the job via the standard mechanism; (2) a helper spawned **out-of-band** (via the WMI or
  service-control path, whose real parent is a system service, not our child) **cannot be contained by
  any job** — this is an inherent Windows limitation, not something this design can close. Two things
  make this a bounded, fail-closed posture rather than an open risk (f-a4): **(a) probe-time gate,
  fail closed** — the no-token probe does not merely *record* out-of-job spawns; if it observes the
  vendor spawning **any** out-of-job/breakaway process during login, tool-login for that vendor is
  **not enabled** (setup refuses and directs the user to sign in manually), so the feature ships only
  for vendors whose login the probe proves stays in-job, closing the "a helper writes staging after
  [f20]-verify" window (f4) by construction rather than tolerating it; **(b) trust-boundary scope** — a
  *same-user* process tampering with the credential home is outside the ACL-only boundary this project
  documents (README "Credentials at rest": the ACL protects from *other users*, not same-user code; the
  reviewer already runs same-user with unconfined reads), and a bare `codex login` has the identical
  exposure. The robust closure (running login under a **separate restricted identity / AppContainer
  broker** whose writes can be gated) is **future hardening, out of 3b scope**, not a regression this
  plan introduces. **Stale-callback safety**
  across the reused fixed port rests on: the machine-wide login mutex (no two callback servers coexist)
  + job teardown on abort (the server dies, so a late redirect after cancellation hits a dead port) +
  the **vendor's own OAuth `state`/PKCE**, which rejects a redirect whose state does not match the
  in-flight login — a reliance we state explicitly because we neither run nor can inspect the callback
  server ourselves.
- **Machine-wide serialization (f4, f19).** The vendor CLIs bind a **fixed localhost callback port**
  (Codex ~1455). A loopback TCP port is **per-machine, not per-session**, so two RDP/console users on
  one machine collide on it — a **`Local\` (per-session) mutex is therefore wrong** (f19). Serialize
  **machine-wide** with a **`Global\` named mutex secured by a DACL** (or, equivalently, a secured
  machine-wide lockfile under a protected `%PROGRAMDATA%` location), created with an explicit DACL so it
  cannot be squatted by another user to deny service, and held for the entire login+callback lifetime.
  A base-scoped lockfile is insufficient. Small secured-named-mutex RAII primitive (FFI, no new crate).
  Login timeout ~3 min, separate from the 5-min `APPROVAL_TIMEOUT`.

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
dir **while its handle is held**; require `method == Subscription`. Then follow the explicit handle
ordering (f11): **release the staging handle → `rename(staging→home)` → re-open and hold the final
`home` handle**. The release is mandatory because `SecuredProfileDir`'s handle omits
`FILE_SHARE_DELETE` (`profile.rs:210-224`), so a rename while it is held would fail; between release
and rename the staging dir is still protected by its restrictive DACL and by the per-home exclusive
setup lock plus the global login mutex, so nothing else touches it. After the rename, re-run the probe
against the held final `home` and require **exact equality** with the captured identity (not merely "is
a subscription account").

**The account read must be structurally bound to the held home (f-a5).** The identity probes today read
the account file **by path** — Codex `home/auth.json` (`codex.rs:444`), Claude `home/.claude.json`
(`claude.rs:235`) — so a late reparse/replacement between [f20]-verify and the probe could make the
authorized fingerprint describe a *different* object than the one we verified. The setup confirmation
therefore reads the **account fingerprint handle-relative through the held `home` directory handle**
(`open_child_relative` + no-reparse, the same primitive as [f20]) rather than by path, and does so
**immediately before** `AllowlistStore::authorize`. So the account written into the allowlist is bound
to the structurally-contained object we hold, with no by-path window. (Claude's `auth status` CLI call
still runs for the *method*; the *account UUID* that becomes the fingerprint is read handle-relative
from `.claude.json`.) The `AllowlistStore::authorize` commit records **exactly** that account, the
final handle held across the write.

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

**Stable per-home key, independent of any leaf that may not exist yet (f12, f-a1).** The lock, marker,
and journal are keyed by `home_key`, which today canonicalizes the home path but **falls back to the
raw spelling when the path does not exist** (`setup.rs:377-379`). During the `home → .old` window
`home` is absent, and on a **fresh first-provision even `profiles/{reviewer}` does not exist yet** (it
is created later, `profile.rs:272`), so canonicalizing the parent is *also* insufficient (f-a1) — two
equivalent spellings (8.3 vs long, case, explicit-home aliases) would still key to different
locks/journals and race. Key instead on a **path-independent identity**:
- **`Named`:** `canonical(base) + reviewer + validated-name`. The base is created and canonicalized
  **before** any lock is taken; `reviewer` and the validated name are exact strings. The key never
  depends on whether `profiles/{reviewer}/{name}` exists.
- **`ExplicitHome`:** canonicalize the **longest existing ancestor** and append the normalized
  remaining components, so all spellings of the same target collapse to one key whether or not the leaf
  exists.
Tests cover a **missing parent** on fresh provisioning and 8.3/long/case/`.`/`..` aliases → one key.

**One write-ahead journal for both operations, flushed to stable storage (f15).** The marker is
extended into a journal `{ operation, home, staging_path, old_path, rejected_path, nonce, intent,
old_file_id, expected_entry, phase }` — where `old_file_id` is `.old`'s `BY_HANDLE_FILE_INFORMATION`
identity (f-a3), `expected_entry` is the complete `AllowEntry` this run will commit (f-b4), and `phase`
advances to a durable `committed` after the store flush. It is written **before** each filesystem
mutation (write-ahead) so the nonce-named paths are durable before any rename that could crash: before
creating staging, before `rename(home → .old)`, before `rename(staging → home)`, **and before a
quarantine `rename(home → rejected_path)` (f17)**. The
quarantine is thus itself a journaled WAL step, not an untracked side effect: if recovery crashes after
renaming `home → rejected-{nonce}` but before restoring `.old`, the journal still names
`rejected_path`, so the next `begin` sees a present, journal-owned, marker-verified rejected dir and
finishes the transition (restore `.old`, retain the rejected dir under the journal for the human to
resolve) rather than stranding it. A `rejected-{nonce}` that is present, marker-matching, and named in
the journal is an **owned quarantined dir**; one whose marker does not match is left untouched
(fail closed). Because atomic-write-plus-rename alone can be reordered behind the directory
renames under **power loss**, each write-ahead journal update calls **`FlushFileBuffers`** on the
journal file before the mutation it guards proceeds, so the intent reaches stable storage first and the
recovery invariant holds across power loss, not only a process crash.

**Recovery runs two phases: rejected-path first, then presence-driven H/O/S (f20).** A journalled
`rejected_path` is a fourth tracked object, so recovery **first** reconciles it before the H/O/S table:
if the journal names `rejected_path` and it is present with a **matching ownership marker**, the run
reached quarantine — recovery completes any pending `.old→home` restore, then **retains the journal
with the rejected dir recorded** (quarantined credentials are never auto-deleted; they await operator
disposition, the journal being their durable ownership record). If `rejected_path` is named but absent
(already resolved), recovery drops it from the journal and continues. Only once no unresolved
journal-owned rejected path remains does the H/O/S phase run; the all-absent row below therefore clears
the journal **only when `rejected_path` is also absent/resolved**.

**The H/O/S phase is presence-driven over all eight states, with a fail-closed default (f-r2.1, f8).**
`SetupSession::begin` replays under the lock, deciding from the **actual presence** of the three
journalled nonce'd paths (`intent` is only an advisory hint; renames are atomic and the paths are
nonce-unique, so presence determines state). `H`=home present, `O`=`.old-nonce` present,
`S`=`staging-nonce` present. **Every one of the eight combinations has a specified fail-closed
action**; any state the operation could not have produced (marked *unexpected*) **retains the journal
and refuses**, surfacing an error rather than guessing:

| H | O | S | interpretation | action |
|---|---|---|---|---|
| 0 | 0 | 0 | nothing created / fully rolled back (first-provision); **home lost with nothing to restore** (re-login) | first-provision: clear journal **iff `rejected_path` is also resolved** (f20), else retain for the rejected phase. re-login: **unexpected → retain journal, refuse** |
| 0 | 0 | 1 | staging created, not swapped in | remove staging (ours); first-provision: clear journal; re-login: home lost → **retain journal, refuse** |
| 0 | 1 | 0 | home moved to `.old`, staging gone before swap-in (failed cleanup) | **restore `.old→home`**, clear journal |
| 0 | 1 | 1 | crashed between `home→.old` and `staging→home` | **restore `.old→home`**, remove staging, clear journal |
| 1 | 0 | 0 | complete; **or first-provision/re-login crashed after `staging→home` but before authorize** (f16); or first-provision never started | **consult store + marker (f16)**: exact store entry present → committed, clear journal, keep home; no entry + home marker matches journal nonce → uncommitted → **quarantine** the home (re-login also restores `.old`; first-provision leaves home absent), journalling the quarantine (f17); no entry + marker absent/mismatched → **retain journal, refuse** |
| 1 | 0 | 1 | pre-swap original home + our staging (or an external home collided with our first-provision rename) | remove staging (ours); **do not touch home** (not ours to delete); clear journal |
| 1 | 1 | 0 | re-login crashed after `staging→home`, before/at authorize | **consult store (f9)**: new entry durably present → remove `.old`, keep home, clear journal; else **roll back** — see object-ownership proof + quarantine (f13) below — restore `.old→home`, clear journal |
| 1 | 1 | 1 | our `.old` + our staging + a present home (external re-creation / impossible interleave) | **unexpected → retain journal, refuse** |

**Delete/quarantine only what is provably ours (f3, f9, f13).** Recovery never *deletes* a home. It
deletes only a nonce'd `staging`/`.old` dir this run created; a home is only ever **quarantined**
(renamed aside to `home.rejected-{nonce}` for the human), never `remove_dir_all`d. And it proves
object identity before touching a home (f13): `OwnedStaging` writes an **ownership-nonce marker file**
into the staging dir at creation (a small `.cross-review-provision-{nonce}` file, carried into `home`
by the rename); the journal records that nonce. At `H,O,¬S` with the new authorization *not* durably in
the store, recovery **verifies the marker at `home` matches the journal nonce** — only then does it
quarantine the uncommitted new home aside and `restore .old→home`; if the marker is **absent or
mismatched** (a late helper or another process replaced `home`), recovery **retains the journal and
refuses** (fail closed), quarantining nothing it cannot prove it owns. It **never deletes a
store-authorized home** and **never removes `.old` until the new store entry is durably confirmed**
(f9) — at `H,O,¬S`, `.old` still holds the store-authorized credentials. Commit order:
`release staging handle → rename(staging→home) → reopen+hold home → handle-relative account read →
journal authorizing → AllowlistStore::authorize → flush store to stable storage → remove .old →
clear journal`. A crash after the store publishes but before the journal clears leaves only the journal
to clear next `begin`; `home` (now store-authorized) is never removed. A pre-commit failure ends with
the valid existing home restored (re-login) or an orphaned staging removed (first-provision), and
**no** allowlist change.

**A uniform stable-storage contract for every publication (f-a2, f-b2, f-b3).** "Durable" means *on
stable storage*, not just atomically renamed. `atomic_write` and `write_secured_file`
(`allowlist.rs:194-206`, `winsec.rs:917-940`) today do temp-write + rename with at most a `File::flush`
(a userspace flush, not `FlushFileBuffers`). So **every** durable publication in the setup path
follows the same rule: **`FlushFileBuffers` the file, then `FlushFileBuffers` the containing directory**
(the directory flush makes the atomic *rename*/directory-entry itself durable — flushing only the file
leaves the rename able to be lost, f-b3). This applies to:
- **the credential files (f-b2):** after [f20] apply-and-verify, flush the vendor-written credential
  file contents (Codex `auth.json`; Claude `.credentials.json` + `.claude.json`) **and the home
  directory metadata** before commit — otherwise a power loss after the store flush + `.old` delete
  could leave the newly-authorized home with missing or truncated credentials and no `.old` to fall
  back to;
- **the store:** flush the store file + its parent dir before the irreversible `.old` delete, so a
  power loss cannot leave the new home present, `.old` removed, and the store still on the **old**
  entry;
- **the journal:** flush the journal file + its parent dir on every write-ahead update, so a lost
  directory entry cannot strand `.old` or an untracked replacement.

**The parent-directory barrier also covers every swap rename (f-b5).** The profile-parent directory
entries are mutated not only by the file publications above but by the swap renames themselves —
`rename(home → .old)`, `rename(staging → home)`, the quarantine `rename(home → rejected)` and its
restore, and the `.old` deletion. Each such directory-entry mutation is followed by a
**`FlushFileBuffers` on the containing (profile-parent) directory before the journal advances or is
cleared past it**, so a power loss cannot lose the rename/unlink and leave the final home missing or
resurrect an orphaned credential-bearing `.old`. Fail closed if that barrier cannot be proven.

Commit order with durability: `[f20]-verify → flush credential files + home dir → handle-relative
account read → journal authorizing (flushed) → AllowlistStore::authorize → flush store + parent →
journal committed (flushed) → remove .old → flush profile-parent dir → clear journal`, and likewise the
swap renames each flush the profile-parent dir before the journal advances. If any required flush
cannot be proven, **fail closed**. Fault tests simulate a power loss at each boundary (after credential
flush, after each swap rename, after store rename, after store flush, after the `.old` delete).

**Recovery can certify and attribute the commit (f-b4).** Recovery's "consult the store" must check for
**this run's exact authorization**, not merely that *some* entry exists — otherwise a pre-existing
entry for a different launch-root/account, or a store rename that happened before its flush, could be
mistaken for a successful commit. So the journal records the **complete expected `AllowEntry`**
(`launch_root`, `effective_home`, `reviewer_family`, `account_fingerprint`) and a durable **post-flush
`committed` phase**. Recovery certifies the commit only when the **flushed** store contains that exact
entry; otherwise it treats the authorization as not landed and takes the rollback path.

**Object-identity proof for `.old`, not paths alone (f-a3).** `.old` holds the *pre-existing* home,
which we did not create, so the ownership-nonce marker does not cover it; recovery must not restore or
delete it **by path** alone (a post-crash reparse or replacement at that path could be installed as
`home` or destroyed). Before `rename(home → .old)`, capture the directory's
`BY_HANDLE_FILE_INFORMATION` (volume serial + file index) into the journal; at recovery, open `.old`
(and the swap target) **no-reparse** and **verify the file-id matches** before any restore/delete — a
reparse point or a mismatched id → **retain journal, refuse** (fail closed). The same guard covers the
`restore .old → home` step.

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
- **rv-20864-4** marked all seven above resolved and raised four more, all folded in: **f-r3.1** the
  presence table was incomplete → all **eight** states enumerated with a fail-closed default;
  **f-r3.2** re-login recovery could delete `.old` before the new authorization was durable → consult
  the store, roll the swap back rather than destroy the only valid credentials; **f-r3.3** the job is
  assigned *after* spawn (uncontained window) → creation-time association (suspended create → assign →
  resume), checked, fail-closed; **f-r3.4** the held staging handle (no delete-sharing) would block the
  commit rename → explicit release-then-rename-then-reopen ordering.
- **rv-20864-5** marked f8/f10/f11 resolved and raised four more, folded in: **f-r4.1 (f12)**
  `home_key` bifurcates while the leaf is absent → key on the canonical parent + normalized leaf;
  **f-r4.2 (f13)** rollback deleted a final home by path with no ownership proof → ownership-nonce
  marker for object identity + quarantine-never-delete + fail-closed on mismatch; **f-r4.3 (f14)**
  no process quiescence / undefined browser ownership → browser out-of-job, bounded quiescence waits,
  callback invalidation on abort; **f-r4.4 (f15)** WAL not power-loss durable → `FlushFileBuffers`
  before each guarded mutation.
- **rv-20864-6** marked f13/f14 resolved and raised four more, folded in: **f-r5.1 (f16)**
  first-provision `H=1,O=0,S=0` cleared its journal with an uncommitted home → marker+store-driven
  quarantine at that state; **f-r5.2 (f17)** the quarantine was not itself journaled → `rejected_path`
  added to the WAL schema and a partial quarantine is recoverable; **f-r5.3 (f18)** out-of-job
  browser/helper lifetime → deny breakaway, rely on vendor OAuth `state` + machine-wide mutex + server
  teardown for stale callbacks, and **document the WMI/service-escape residual honestly**; **f-r5.4
  (f19)** a `Local\` mutex is per-session but the callback port is per-machine → **`Global\` secured
  mutex** (or `%PROGRAMDATA%` secured lockfile).
- **rv-20864-7** resolved f15/f16/f17/f19 and raised one: **f-r6.1 (f20)** the journalled
  `rejected_path` was outside the H/O/S recovery machine → a **prioritized rejected-path recovery
  phase** now runs first, and the all-absent row clears the journal only when the rejected path is also
  resolved.
- **rv-20864-9 (fresh authoritative)** confirmed the resumed ledger's resolutions and, un-frozen,
  surfaced five deeper last-mile findings, all folded in: **f-a1** parent-canonical key still unstable
  on a fresh first-provision (parent not yet created) → key `Named` on `base+reviewer+name`, `Explicit`
  on the longest existing ancestor; **f-a2** "durably confirmed" store had no flush → `FlushFileBuffers`
  the store + parent before removing `.old`; **f-a3** `.old` restored/deleted by path → capture its
  `BY_HANDLE_FILE_INFORMATION` id, verify no-reparse + id before restore/delete; **f-a4** an observed
  out-of-job helper could write after verify → **probe-time fail-closed gate** (unsupported vendor if it
  spawns out-of-job) + trust-boundary scoping + broker as future work; **f-a5** identity probes read by
  path → read the account fingerprint **handle-relative** to the held home immediately before authorize.
- **rv-20864-10..12 (fresh authoritative).** rv-10 resolved the `.old` object-identity and
  handle-relative account-read findings (f9/f12/f18-successors); rv-11 flaked with an unstructured
  envelope (issue #63); rv-12 confirmed the earlier resolutions and raised a final power-loss-durability
  cluster, all folded in: **f-b1** dropping `KILL_ON_JOB_CLOSE` let a login child survive a
  *setup-process crash* and race recovery → **retain `KILL_ON_JOB_CLOSE`** (safe because success closes
  the job only after quiescence, and the browser is out-of-job); **f-b2** vendor credential contents
  were never flushed → flush the credential files + home dir before commit; **f-b3** the journal rename
  flushed the file but not its parent dir → flush file **and** parent for every publication; **f-b4**
  recovery could not certify/attribute the commit → persist the full expected `AllowEntry` + a durable
  post-flush `committed` phase and require the exact flushed entry.
- **rv-20864-13** resolved f-b1/f-b2/f-b3 and raised **f-b5** — the parent-directory flush barrier had
  to extend to the swap renames themselves (`home→.old`, `staging→home`, quarantine/restore, `.old`
  delete), not only the file publications, so a lost directory-entry update cannot drop the final home
  or resurrect `.old`. Folded into the durability contract. (f-b4 froze at a stale `open`; its full
  `AllowEntry` + `committed`-phase resolution is in the doc.)
- **Ledger note:** across rv-20864-4..7 the entries f9/f12/f18 repeatedly froze at a stale `open`
  (identical detail text, non-advancing turn — issue #62). Their live content was addressed: f9's
  successor **f13 is resolved**; f12's parent-canonical key is in the doc with no live successor; **f18
  is dispositioned as a documented residual** — a helper a third-party CLI spawns via WMI/service
  control has a system service as its real parent and **cannot be contained by any job object**, so the
  honest posture (breakaway denied, residual documented, probe-observed, stale-callback handled by
  vendor OAuth `state` + machine-wide mutex + server teardown) is the correct engineering answer, not a
  guarantee we could truthfully make (matching the repo's README posture on unconfined reviewer reads).
  The authoritative verdict is taken from the running fresh session as the remaining live findings
  converge.

## Files to modify

- **`src/winsec.rs`** — `secure_and_verify_child_file` (f20); `create_new_secured_child_dir`
  (exclusive create, f1); named-mutex RAII primitive (f4); tests.
- **`src/winjob.rs`** — a **no-breakaway** login job that **keeps `KILL_ON_JOB_CLOSE`** (f-b1) +
  **creation-time association** (suspended create → assign → resume) + checked terminate + a
  **quiescence wait** (`ActiveProcesses==0`) before close/cleanup for login (f-r2.5/f-r3.3/f14/f18/f-b1).
- **`src/session.rs`** — refactor `ExclusiveLock` to `LockFileEx` `Shared`/`Exclusive` on one path
  (f9); recovery-aware `SetupSession::begin` (WAL journal replay, f2/f8/f-r2.1..4).
- **`src/profile.rs`** — `SecuredProfileDir::secure_and_verify_credential`; staging-create path.
- **`src/reviewer/mod.rs`** — `LoginOutcome` + `run_login` (contained, isolated, controlled-env,
  closed stdin); per-reviewer login command builder.
- **`src/reviewer/codex.rs` / `claude.rs`** — login command shapes; credential-file names; a
  setup-scoped, mandatorily-isolated confirmation probe (f-r2.6) that reads the account fingerprint
  **handle-relative** to the held home (f-a5).
- **`src/allowlist.rs` / `src/winsec.rs`** — a **uniform `FlushFileBuffers`(file)+`FlushFileBuffers`
  (parent-dir) durability helper** used for the store, journal, and credential-file/home flushes
  (f-a2/f-b2/f-b3); the store carries the full expected `AllowEntry` recovery certifies (f-b4); a
  handle-relative account-file read helper (f-a5); capture/verify `BY_HANDLE_FILE_INFORMATION` for the
  swap (f-a3).
- **`src/setup.rs`** — `login` arg, recovery-before-classification, `OwnedStaging` (with ownership-nonce
  marker, f13), unified staged-create + rename-into-place for both operations, identity binding,
  `home_key` stable-parent keying (f12), `FlushFileBuffers` WAL (f15), injected-login seam, tests.
- **the review path (`attempt()`)** — [f5] shared-read lock spanning the whole attempt.
- **`src/mcp.rs` / `src/tools.rs`** — `login` boolean in the setup schema + description (setup blocks
  minutes while the human OAuths).
- **`docs/reviewer-account-profiles-status.md`** — mark 3b done; record the deferred real smoke, the
  no-token probe requirement, and the non-TTY-login / [f20] apply-then-verify findings.

## Verification

- `cargo test` — unit tests: exclusive-create collision refused (f1); first-provision rename refuses a
  concurrently-appeared home (f-r2.2); identity-equality binding (f3); non-subscription rejected;
  first-provision + re-login happy paths; **fault-injection recovery** driving a crash between every
  mutation and its journal write for both operations, asserting **each of the eight presence states**
  reaches its specified action incl. crash-after-store-publish keeps home and the `H,O,¬S` store-driven
  roll-back vs. cleanup (f-r2.1/f-r2.3/f-r3.1/f-r3.2); unexpected states retain the journal and refuse;
  recovery-runs-before-classification (f-r2.4); retain-journal-on-failed-cleanup (f8); [f20] primitive
  (apply+verify child; widened-ACL negative); named-mutex mutual exclusion (f4); shared/exclusive lock
  (readers coexist, reader⇄writer exclude) (f9); controlled-env clears rogue vars (f-r2.7);
  hostile-cwd-config for login **and** the confirmation probe (f6/f-r2.6); login fails closed when
  the job is uncontainable, incl. creation-time association (f-r2.5/f-r3.3); staging handle released
  before the commit rename (f-r3.4); **`home_key` alias stability** — 8.3/long/case spellings map to
  one key with the leaf absent (f12); **ownership-marker mismatch → quarantine-and-refuse** rather than
  delete (f13); login **fails closed if the job does not quiesce** before staging cleanup (f14);
  first-provision uncommitted-home quarantine at `H=1,O=0,S=0` (f16); **partial-quarantine recovery**
  (crash mid-quarantine leaves a journaled, recoverable `rejected` dir) (f17); a `Global\` mutex
  excludes a second session (f19); **rejected-path recovery phase** — a lone `R=1` after quarantine is
  reconciled before the H/O/S table and never stranded by the all-absent row (f20); **key stability with
  a missing parent** on fresh provisioning (f-a1); **power-loss between store-flush and `.old`-delete**
  leaves a recoverable state (f-a2); recovery **refuses on a `.old` reparse/file-id mismatch** (f-a3);
  the confirmation **account read is handle-relative** and a by-path reparse cannot shift the authorized
  account (f-a5); a **login child is reaped on a setup-process crash** (`KILL_ON_JOB_CLOSE` retained)
  and cannot race recovery (f-b1); **power-loss fault tests** at each flush boundary — after credential
  flush, after store rename, after store flush, after `.old` delete — leave a recoverable state
  (f-b2/f-b3); recovery **certifies the exact flushed `AllowEntry`** before removing `.old` or rolling
  back (f-b4).
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
