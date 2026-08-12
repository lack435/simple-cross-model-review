# Reviewer account profiles — implementation plan

Status: **plan**, no code yet. The "why" and the decisions live in
[reviewer-account-profiles.md](reviewer-account-profiles.md); this is the "how" — file-by-file work,
phasing, and tests — to converge before writing code. Markers as in the design note:
**[verified]** / **[assumed]** / **[decided]**. Line references are illustrative.

## Phasing

Three PRs, each independently gate-reviewed. The ordering carries one safety invariant: **profile
*use* fails closed until the Phase 3 authorization store exists.** Phase 1 wires a
`profile_authorized()` decision point that, with no store, denies — so the routing machinery lands
inert and un-exploitable, and Phases 1–2 can merge before the security surface is built.

- **Phase 1 — Routing core (inert):** flags, safe-name validation, effective-home resolution,
  `Config` threading through every read site, a per-`Command` controlled environment (`env_clear` +
  vetted allowlist), the exact pre-spawn resolved-account assertion. A profile *request* is parsed and
  validated but *refused at use* (no authorization yet).
- **Phase 2 — Session identity:** tagged profile identity persisted on `SessionRecord`, compared on
  resume, legacy records fail closed.
- **Phase 3 — Authorization + provisioning + setup:** profile-dir creation with per-user ACL, the
  allowlist store, first-use authorization wired into the routing decision (replacing Phase 1's
  deny-all), the setup MCP tool, and the one-off localhost confirmation page.

---

## Phase 1 — Routing core

### New type: `ProfileSelector`

A per-entry selector, part of reviewer *identity* (like `bin`, which already distinguishes an
account — `ReviewerSpec::same_reviewer_identity`, `src/config.rs` ~133):

```
enum ProfileSelector {
    Ambient,               // no profile flag — inherit ambient env, today's behaviour
    Named(String),         // --*-profile <name>, validated safe name
    ExplicitHome(PathBuf), // --*-home <abs>, local/trusted-only escape hatch
}
```

It lives on `ReviewerSpec` (`src/config.rs` ~111-123) so each fallback entry carries its own, and
joins `same_reviewer_identity` and `resume_entry_index` matching. `usage_minimum`'s exclusion from
identity is the counter-example to follow in reverse: the profile *is* identity, so it goes in.

### `src/config.rs` — parsing and resolution

- **Flags:** `--codex-profile` / `--claude-profile` (name), `--codex-home` / `--claude-config-dir`
  (absolute path). Family-scoped like `--sandbox`/`--tools`: a `--codex-profile` on a Claude entry
  is a parse error, not silently inert. Profile and home for the same family are mutually exclusive
  (explicit home wins is a *runtime* rule; at parse time, reject setting both on one entry).
- **Safe-name validation** (`validate_profile_name`): grammar `[A-Za-z0-9._-]+`, non-empty, not `.`
  or `..`. Reject at parse time with a caller-facing message. **[decided]** containment only — not
  authorization (that is Phase 3).
- **Effective-home resolution** (new `fn effective_home(base, reviewer, &ProfileSelector) ->
  Result<Option<PathBuf>>`): `Ambient` → `None` (inherit). `Named(n)` →
  `{base}\profiles\{reviewer}\{n}`. `ExplicitHome(p)` → its own canonical form; no containment (it is
  deliberately outside the root) but local/trusted-only (Phase 3 gates it). `base` =
  `%CROSS_REVIEW_HOME%`, else `%LOCALAPPDATA%\cross-review` — **[decided]** deliberately *not*
  `--state-dir`, which is user/repo-settable and must never determine a credential home.
- **[f5] Do NOT reuse `codex_sterile_dir` unchanged.** That function
  (`src/reviewer/mod.rs` ~487) canonicalizes *before* testing the reparse attribute and checks
  containment against `cfg.cwd` — safe for its own use (a temp-parented dir it owns), wrong for a
  credential home. The profile check must instead: **reject a reparse point on each *original* path
  component** before canonicalization (a junction at the pre-canonical path resolves to an ordinary
  directory and would pass a post-canonical test), enforce **containment under the profile root**
  (not `cfg.cwd`), and close the create/check TOCTOU with **handle-based / no-follow Windows APIs**
  (open with `FILE_FLAG_OPEN_REPARSE_POINT` and inspect, rather than path-check-then-create). Write
  this as a dedicated `secure_profile_dir` helper with its own tests; note in passing that
  `codex_sterile_dir`'s ordering is worth a separate look but is out of scope for this PR.
- **[f1] There is no unauthorized `reviewer_home` accessor on the review path.** Home resolution and
  authorization are one operation: `resolve_authorized_home(cfg, spec) -> Result<Option<PathBuf>,
  Failure>` (`Ok(None)` = ambient/inherit; `Ok(Some(home))` = authorized; `Err(PROFILE_NOT_AUTHORIZED)`
  otherwise). The plain-`Option` resolver stays **private** to that function (and to the separate
  provisioning path), so a review-path caller cannot obtain a home without passing authorization — the
  `Option` return of a bare accessor could not express the refusal anyway.

### `src/config.rs` / `src/reviewer/*` — thread the home through *every* read site

[design requirement 2] The home must reach all of these from `Config` (not ambient env) **and through
`resolve_authorized_home`** (not a bypassing accessor). `codex_home` (`src/reviewer/codex.rs` ~363)
and `claude_config_path` (`src/reviewer/claude.rs` ~444) become `(cfg, spec)`-parameterised and obtain
the home via the authorized resolver, falling back to ambient env only for `Ambient`; a
non-`Ambient` selector that is unauthorized surfaces the `Err`, it does not silently read a home. Call
sites to convert:

- `auth_check` — **signature gains `spec`** (`fn auth_check(&self, bin, cfg, spec, cancel)`), since
  today it takes only `(bin, cfg, cancel)` and cannot know the profile. Ripples to the trait
  (`src/reviewer/mod.rs` ~521), both adapters, `ensure_entry_ready` (`src/tools.rs` ~78), and tests.
- `account_fingerprint` — already `(cfg, spec)`; switch its `codex_home()` / `claude_config_path()`
  to the authorized form.
- `invocation` — set the child env (below).
- **`observe_headroom` / `find_rollout(&codex_home())`** (`src/reviewer/codex.rs` ~326-336) — the
  site the design's first draft missed. Same conversion, or the usage gate reads headroom from the
  wrong home.
- `usage_headroom_key` (`src/tools.rs` ~94) already funnels through `account_fingerprint`, so it
  inherits the fix — call out in a test that its key is the profile account.

### `src/reviewer/claude.rs` + `codex.rs` — controlled child env + assertion

- **Set the home per-`Command`** ([design requirement 4], never `std::env::set_var`): in `invocation`
  *and* `auth_check`, when `reviewer_home(spec)` is `Some(home)`, `cmd.env("CODEX_HOME"/
  "CLAUDE_CONFIG_DIR", home)`. `Ambient` sets nothing (byte-for-byte today's behaviour).
- **[f2/f3/f16] Controlled child environment — a concrete allowlist, started from empty.** For a
  non-`Ambient` child, call `cmd.env_clear()` and then set **exactly** this set, each **passed through
  from the parent's value** (except the home var, which we set):
  `SystemRoot`, `windir`, `SystemDrive`, `ComSpec`, `PATH`, `PATHEXT`, `TEMP`, `TMP`, `USERPROFILE`,
  `HOMEDRIVE`, `HOMEPATH`, `APPDATA`, `LOCALAPPDATA`, `PROGRAMDATA`, `PROGRAMFILES`,
  `PROGRAMFILES(X86)`, `NUMBER_OF_PROCESSORS`, `PROCESSOR_ARCHITECTURE` — **plus** the profile home
  var (`CODEX_HOME` / `CLAUDE_CONFIG_DIR`). This list is the contract: no `etc.`, and any key not on
  it (every provider-auth variable included) is simply **absent**. The known provider names (Claude
  `ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN`, `CLAUDE_CODE_OAUTH_TOKEN`; Codex `OPENAI_API_KEY`,
  `CODEX_API_KEY`, `CODEX_ACCESS_TOKEN`) need no explicit removal since we start from empty — they are
  listed only as the things the assertion below double-checks are gone. **[deferred-to-impl]** the
  list is a starting contract; if a CLI turns out to need another OS var to launch, it is added to the
  allowlist explicitly (never by falling back to inheriting the parent environment).
- **[f3, surfaces verified] Pre-spawn identity + method assertion — built from real local surfaces,
  with the controlled env as the structural guarantee.** The security guarantee that the reviewer
  uses the profile account is the **controlled environment** (`env_clear` + positive allowlist +
  home var): with the child's environment cleared and no provider-auth variable on the allowlist,
  the profile's own credential file is the child's *only* credential source — nothing can override
  it, so the account is fixed by construction. The assertion below is the fail-closed confirmation of
  that, not the sole mechanism. It is **pre-spawn** [f11] (part of the preflight, before the review
  `invocation`), so the first request cannot go out wrong; a value from the review run itself (its
  `result`/rollout) is too late and is explicitly **not** the probe.

  **Verified surfaces (Codex CLI 0.144.5, Claude Code, Windows, 2026-08):**
  - **Liveness** (already built): `auth_check` under the controlled env + profile home confirms the
    home is signed in — Codex `login status` exit 0 / Claude `auth status` `loggedIn:true`.
  - **Method** (subscription vs API key): **Codex** — `auth.json.auth_mode == "chatgpt"` (the auth
    file, read under the airtight env; also confirmable via `codex doctor`, which is `CODEX_HOME`-aware
    — verified it reports `auth mode: none` for an empty home). **Claude** — `auth status`
    `authMethod`/`subscriptionType` (keys present: `authMethod`, `subscriptionType`, `orgId`, `email`,
    `orgName`, verified). Fail closed unless the method is the subscription/ChatGPT OAuth.
  - **Identity** (which account): `account_fingerprint` (already built) — Codex `tokens.account_id`,
    Claude `accountUuid`/`organizationUuid`. **Verified** the Codex `id_token` JWT's nested
    `https://api.openai.com/auth.chatgpt_account_id` equals `tokens.account_id`, and the id_token also
    carries `sub`/`email`/`chatgpt_plan_type` — available as an optional richer cross-check. For
    Claude, `auth status` `orgId`/`email` cross-check the file's uuids.

  So define a typed `ResolvedIdentity { account, method }` read from these surfaces, **version-pinned**
  (Codex 0.144.x auth-file shape / Claude `auth status` keys), that **fails closed** on an
  unrecognised shape or a non-subscription method. Assert the profile's method is subscription and its
  `account_fingerprint` is present. **Named profiles enable for both reviewers** — the earlier
  "no Codex surface → disable" concern is resolved: `codex doctor` + the auth-file `auth_mode`/id_token
  give a real method+identity surface, and the airtight env is the guarantee it is used.
- **[f2/f3] Never cache the assertion.** The preflight cache (`ensure_entry_ready`, `src/tools.rs`
  ~52, keyed by entry index, cached for process lifetime) may cache **only resolution data** (the
  resolved bin). The identity + method assertion **re-runs on every non-`Ambient` spawn** — a
  fingerprint-keyed cache would still miss a same-account auth-*method* change or a newly-introduced
  provider var, so the assertion is not cacheable at all.
- **[f4/f5] Authorization, probe, and spawn are one atomic critical section under the per-home lock.**
  The probe runs in a child before the review child; a concurrent re-login (a setup, or an external
  `codex login`) could swap the home's account from A to B, so the first request goes out under B and
  is only caught post-hoc by the switch guard (after billing). [f5] It is not enough to lock only
  probe→spawn: if authorization (the allowlist tuple/fingerprint check) runs *before* the lock, a
  re-login can change A→B in the gap, and the later probe then compares B against the current file
  (also B) and passes — launching without ever rechecking that B matches the stored-A tuple. So the
  order is: **acquire the per-home lock (shared/read) → check the full allowlist tuple including the
  account fingerprint → probe identity+method → launch the review child, all while holding the lock**;
  the exclusive setup lock cannot interleave. Equivalently, carry a **generation token** read at the
  authorization check (credential-file identity + mtime) and re-verify it is unchanged at both probe
  and spawn, failing closed on any change.

  **[f1-fresh] Be honest about the boundary of this lock: it serializes *cross-review's* writers, not
  an external `codex login`.** A user (or another tool) writing the credential file directly does not
  take our lock, so it can change the account after our recheck and before the reviewer child's first
  read — and a generation token cannot close that final gap either. So this pre-spawn atomicity is a
  *best-effort narrowing*, not an absolute guarantee, and the plan does **not** claim otherwise. The
  actual fail-closed backstop for the external-login race is the **post-review switch guard**: the
  identity asserted here is carried through the run and compared against the final fingerprint, so a
  review whose account was swapped mid-flight is **refused, never delivered** — a request may briefly
  issue under a swapped account, but its result does not reach the caller. Attempting to hard-lock the
  file against external writes for the whole review is rejected: it would also block the reviewer's own
  legitimate token refresh.

### Phase 1 authorization at the resolution choke point

**[f1] Authorization is enforced where the home is resolved, so it covers every profile-dependent
path — not bolted onto one "routing" call.** Introduce `resolve_authorized_home(cfg, spec) ->
Result<Option<PathBuf>, Failure>` as the *only* way any code obtains a profile home:
`Ambient` → `Ok(None)`; a non-`Ambient` selector → resolve the effective home, then check
authorization and return `Err(PROFILE_NOT_AUTHORIZED)` (actionable, pointing at the not-yet-shipped
setup) unless approved. In Phase 1 the check is a deny-all stub for non-`Ambient`; Phase 3 fills it.
Every profile-dependent site on the **review path** must obtain the home through this function
*before* reading it or letting it influence a decision, and the authorization check must precede the
preflight-cache lookup. Enumerated sites to route through it: **fresh selection / the usage gate
(`gate`/`usage_headroom_key`), `status`, the resume `auth_check` path, fallback-entry execution, and
every read site from the section above.** No review path may read a profile home it has not
authorized.

**[f10] Provisioning is a distinct resolver, not this one.** The Phase 3 setup tool must create and
initialize a home *before* any repo is authorized to use it, so it cannot go through
`resolve_authorized_home`. Split the two: `resolve_provisioning_target(selector, reviewer)` — used
only by setup, gated by the *human authorization gate* (not the allowlist), returns the
`secure_profile_dir` target for creation/login — and `resolve_authorized_home` — the review path,
gated by the allowlist. The provisioning target must never be reachable from a review path (a home
that exists but has no allowlist entry is still refused there), so creating a profile never implicitly
authorizes any repo to use it.

**[f2-fresh] The provisioning resolver takes a `ProfileSelector`, not just a name, so the explicit-home
escape hatch is reachable.** `ExplicitHome` requires an allowlist entry like any non-ambient selector,
so setup must be able to authorize one — otherwise `--codex-home` / `--claude-config-dir` are dead
flags. For `Named(n)` it resolves under the profile root (safe-name + containment); for
`ExplicitHome(p)` it validates the path directly (canonicalize, reparse-reject, must resolve to a real
directory, **local/trusted-only** — never accepted from committed repo config that is not on the local
allowlist) and returns it as the provisioning target. Both then go through the same human-gated
create/login/confirm/commit flow and land the same `(launch_root → effective_home + reviewer_family +
account_fingerprint)` allowlist entry keyed on the resolved home. (If explicit homes are judged not
worth the surface for the first cut, the alternative is to drop the two flags entirely — but the plan
must not leave them defined-yet-unauthorizable.)

### Phase 1 tests (`cargo test`, no network)

- `validate_profile_name`: accept/reject table (`.`, `..`, `a/b`, `..\\x`, `C:x`, empty, valid).
- `effective_home` / `secure_profile_dir`: containment holds; a traversal name, a rooted/drive name,
  and a reparse point **on an original (pre-canonical) component** are refused; `%CROSS_REVIEW_HOME%`
  override honoured; `Ambient` → `None`.
- argv/env inspection (extend `src/reviewer/argv_tests.rs`): for a named profile the child env is the
  **vetted allowlist plus the home var and nothing else** — assert the exact key set, that no
  provider-auth var is present even when one is set in the parent, and that the child does not start
  from the inherited environment; **`Ambient` sets/clears nothing** (byte-for-byte regression guard).
- `account_fingerprint` / headroom read from the profile home, not ambient env.
- `profile_authorized` denies a named profile in Phase 1; the routing path returns
  `PROFILE_NOT_AUTHORIZED`.

---

## Phase 2 — Session identity

### `src/session.rs` — persist a tagged identity

Add, following the established `Option<T>` + `#[serde(default, skip_serializing_if)]` convention
where **`None` means a legacy record** (`raw_bin`/`resolved_bin` are the exact precedent, ~143-153):

```
struct ProfileIdentity {          // serialized tagged
    selector: ProfileSelectorId,  // Ambient | Named(name) | ExplicitHome
    effective_home: Option<String>,       // canonical resolved home; None only for Ambient
    account_fingerprint: Option<String>,  // the account that ran; see per-variant contract below
}
...
#[serde(default, skip_serializing_if = "Option::is_none")]
pub profile_identity: Option<ProfileIdentity>,
```

[f1] The tag is what separates a **new `Ambient`** session (persists `Some(ProfileIdentity{ selector:
Ambient, .. })`, resumes normally) from a **legacy** record (`profile_identity: None`, fail-closed
non-resumable). Reserving `None` for legacy is the whole point — do not represent ambient as absence.

[f8] **`account_fingerprint` is `Option` because the existing `account_fingerprint()` API returns
`Option<String>` and usage accounting treats absence as normal — but the *session contract is
per-variant*, and a missing value is never a wildcard:**
- `Ambient`: **carries and compares its account fingerprint**, matching the approved design note
  ("an `Ambient` record carries its own fingerprint and resumes normally"). `effective_home` is
  `None` (there is no profile home), but a **present** stored fingerprint that differs from the
  current one refuses the resume — otherwise re-logging the ambient account to a different account
  and resuming would be exactly the silent cross-account resume this feature exists to stop [f12].
  [f14] There is **no permissive path**: if a fingerprint could not be captured for a *new* ambient
  session (`account_fingerprint` returned `None` at record time), the session is recorded but marked
  **non-resumable**, and its resume is refused — not allowed to fall back to an unbound resume.
  Normally the fingerprint *is* capturable (the reviewer just authenticated), so this only bites a
  genuine read failure, which fails closed like every other unestablished identity. (This is distinct
  from a *legacy* record, `profile_identity: None`, which is non-resumable for predating the field.)
- `Named` / `ExplicitHome`: the fingerprint is **required**. If it could not be read at record time,
  or is absent/unreadable at resume, the resume **fails closed** (`SESSION_NOT_RESUMABLE`) — a named
  profile whose identity cannot be established is refused, never resumed under an assumed account.

So the rule is uniform across all variants: **a resumable session requires a captured account
fingerprint; absent it, the session is non-resumable.** This matches the approved design note's
fail-closed resume rule with no exception.

[f7] `effective_home` is the *canonical resolved home*, not the name: the same `Named(work)` resolves
to different homes under different `%CROSS_REVIEW_HOME%`, and the account fingerprint alone catches a
different account, not the same account in a different home.

### Resume comparison

The turn-record write path populates `profile_identity` from the resolved home + fingerprint. On
resume, alongside the existing `resume_entry_index` identity match (`src/config.rs` ~946, extended so
the selector is part of entry identity), compare the record's `effective_home` **and**
`account_fingerprint` against a fresh resolution/read — mismatch or a legacy `None` is refused with
`SESSION_NOT_RESUMABLE` (the same fail-closed family the CWD-mode and Perforce-identity checks already
use). A fresh resolution here mirrors the `resolved_bin` re-resolution already done on resume.

### Phase 2 tests

- serde round-trip of each `ProfileIdentity` variant.
- ambient-new with a captured fingerprint resumes; legacy (`profile_identity: None`) is refused.
- **[f25]** a new ambient record whose `account_fingerprint` is `None` is **non-resumable** (proves the
  uniform fail-closed rule — no unbound ambient resume).
- ambient resume is refused when the stored and current fingerprints are both present and differ.
- resume refused when the effective home changes (same name, different `%CROSS_REVIEW_HOME%`) and
  when the account fingerprint changes.
- selector is part of `resume_entry_index` identity (a profile-differing entry does not match).

---

## Phase 3 — Authorization, provisioning, setup

### Allowlist store

A per-machine store under `%CROSS_REVIEW_HOME%` / `%LOCALAPPDATA%\cross-review` — **[decided]** a
fixed, ACL-protected location the repo cannot point at (never `--state-dir`).

**[f19] One entry schema, used everywhere:** an entry binds `(launch_root) → (canonical effective_home
+ reviewer_family + account_fingerprint)`. Authorization matching compares **all four** — so a review
is authorized only when the launch root, the resolved home, the family, *and* the account currently in
that home all match a stored entry. If the account in the home changes (a re-login to a different
account under the same profile home), the fingerprint no longer matches and the review is refused as
`PROFILE_NOT_AUTHORIZED` until reauthorized. This is the single contract; setup, resume, and
authorization all reference this exact tuple.

**[f22] The store is the authorization boundary and gets its own security contract.** The store file
**and its parent directory** are created and verified with the same restrictive DACL as a profile home
(current-user/SYSTEM/Administrators only, inheritance removed, handle-based set + verify, no-follow
open — see the ACL section), because an attacker who can write the store can authorize themselves.
Reads verify the DACL and reject a reparse; writes are **atomic** (write-temp + rename) under an
**exclusive lock**, so a concurrent writer cannot interleave or a reader observe a half-written store.
On any ACL/verification failure the store is treated as untrusted and every profile use fails closed.

**[f7, resolved] Capture the immutable launch root now, at process start.** The reviewer confirmed
the process-start CWD *is* available independently of `--cwd` — `--cwd` only sets `Config::cwd`. So
`main.rs` reads and canonicalizes `std::env::current_dir()` **before** `Config::from_args`, and stores
it explicitly (e.g. `Config::launch_root`). The allowlist keys on `launch_root`, **never**
`Config::cwd`. **[decided]** define `--cwd`'s effect: when `--cwd` is supplied (the review root
differs from the launch root), that is an out-of-band redirection and requires its own explicit
confirmation rather than silently inheriting the launch root's authorization. A change to the
`launch_root → effective_home` mapping forces reauthorization.

### Profile-dir provisioning + ACL

**[f6] Specify the ACL fully; this dir will hold credentials.** Set a DACL that grants only the
current-user SID (plus SYSTEM and Administrators per Windows norms) and **removes inheritance** — an
ACL built with `InitializeAcl` + explicit `AddAccessAllowedAce` for each SID (current user from the
process token via `OpenProcessToken`/`GetTokenInformation(TokenUser)`), applied with
`DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION`. **[decided]** implement via direct
`windows`/`windows-sys` FFI in the style of `src/winjob.rs` (which is job-object FFI, *not* ACL code —
so this is new, not a reuse); **no `icacls`/shell fallback**. **Verify** the resulting DACL after
setting it and **fail closed** (refuse to write any credential) on any error.

**[f15] Create, ACL, and verify through ONE handle — never reopen by path.** The `secure_profile_dir`
helper creates/opens the directory with a handle (no-follow); the ACL is applied and verified on **that
same handle** via `SetSecurityInfo` / `GetSecurityInfo` (the `HANDLE` forms), **not**
`SetNamedSecurityInfo` (the path form). Reopening by path between create, ACL-set, and verify
reintroduces exactly the reparse/replacement TOCTOU `secure_profile_dir` closed — a swapped path could
receive the DACL meant for the verified object.

**[f20] Our handle governs *our* dir; the vendor child writes by path — bridge the gap explicitly.**
The separately-spawned vendor login is a different process; it only receives `CODEX_HOME` /
`CLAUDE_CONFIG_DIR` as **paths** and writes relative to them, so it cannot inherit our `HANDLE`. To
keep the directory it writes into the one we verified: (a) hold the directory handle open across the
child's lifetime, opened with `CreateFile`/`FILE_FLAG_BACKUP_SEMANTICS` and a **share mode that omits
`FILE_SHARE_DELETE`** — a directory open without delete-sharing cannot be renamed or deleted while the
handle is held, which concretely blocks the rename-over (the DACL already denies other users write
access, so the same-user hold is the remaining lever); (b) after the child exits, **prove containment
by handle-relative open, not by file identity** — [f3] `BY_HANDLE_FILE_INFORMATION`'s volume+file id
identify the credential file but do *not* encode its parent, so they cannot show it sits inside the
held directory (ancestor/intermediate reparse or replacement stays unproven). Instead open the
credential file **relative to the held directory handle** (`NtCreateFile` with `RootDirectory` set to
that handle, a leaf name with no separators, `OBJ_DONT_REPARSE` / no-follow) so containment is
*structural* — the open can only resolve a direct child of the object we hold — then re-read its DACL;
(c) **fail closed** (discard the login, do not commit the allowlist) on any failure or DACL mismatch.
The held handle prevents replacement of the verified directory; the handle-relative open proves the
credential file is genuinely inside it.

### Wire authorization + the guardrail

- `resolve_authorized_home` (the Phase 1 choke point) now consults the store: a non-`Ambient` selector
  is allowed only with an entry matching the **full [f19] tuple** — `(launch_root → effective_home +
  reviewer_family + account_fingerprint)`, the current account in the home included; otherwise
  `PROFILE_NOT_AUTHORIZED` with a remediation pointing at the setup tool. (A fingerprint that no longer
  matches — the account in the home changed — is refused as unauthorized until reauthorized.)
- **[f4] The guardrail needs an explicit *expected* identity captured at start.** Detecting an
  A→B mid-review switch requires comparing the *final* fingerprint against the account asserted **at
  spawn** (A), not against a fresh reread of the profile file — a reread would compare B with itself
  and pass. So the start fingerprint asserted in Phase 1 is carried through the run (on the
  `Preflight`/job context) and the post-review check compares final-vs-start, **rejecting before the
  review is recorded or delivered**. Test the explicit switch case.

### Setup MCP tool + one-off localhost confirmation page

- **[f2] Three distinct operations, because a credential home may already exist and be valid.** The
  tool must not treat every setup as a fresh provision, or a failed re-login could delete a working
  home:
  - **Authorize-only** (home exists, already logged in, just needs this repo authorized): the common
    "add this repo to my existing `work` profile" case. It **touches neither the credential home nor
    the login** — its commit prerequisite is a **successful read-only identity probe** (no vendor
    login), then it commits the allowlist entry. No dir creation, so nothing to roll back. [f6]
  - **First provision** (no home yet): create the dir, login, confirm, commit. Rollback deletes the
    dir **because this run created it**.
  - **Re-login existing** (home exists, re-authenticating / switching account): use **staged
    replacement** — log in to a fresh staging dir, confirm identity, then swap into place; on failure
    the existing home is untouched. Never log in directly over a valid home.
  - Rollback is scoped by an **ownership/generation marker**: a run removes **only** what it created
    (its staging dir / freshly-created dir), never a pre-existing home. [f2]
- New MCP tool (`src/tools.rs` / `src/mcp.rs`), as an ordered state machine: classify the operation
  (above) → validate the name → **human approval** (below) → [provision/stage as needed] → confirm the
  resolved account → **only then commit the allowlist entry**.
- **[f18/f6] Approval authorizes the *intent*; the commit prerequisite depends on the operation.**
  Human approval does **not** itself write the allowlist entry — it moves the profile into a
  **provisional/in-setup** state. For **first-provision** and **re-login**, the `(launch_root →
  effective_home + reviewer_family + fingerprint)` entry commits only after the vendor login succeeds
  *and* the identity probe confirms the account; for **authorize-only**, there is no login step, so the
  prerequisite is the read-only identity probe alone [f6]. In every case a profile still in setup is
  **refused by `resolve_authorized_home`** (in-setup ≠ authorized), so a half-finished setup can never
  route a review.
- **[f21] The setup identity confirmation runs through the provisioning path, not the review path.**
  Confirming the just-logged-in account cannot call `resolve_authorized_home` (it would reject the
  as-yet-unauthorized profile) and must not bypass the choke point ad hoc. Factor the probe into a
  shared helper (`assert_resolved_identity(home)` — the same `env_clear` + allowlist + UUID/method
  probe from Phase 1) that the **provisioning** flow invokes against `resolve_provisioning_target`'s
  home under the human gate, and the **review** flow invokes against `resolve_authorized_home`'s home.
  One probe implementation, two gated entry points; neither weakens the authorization invariant.
- **[f23] Serialize and make setup recoverable, across processes.** Setup can be invoked from
  concurrent MCP tool calls in one server *and* from separate server instances, so the lock must be an
  **OS-level, cross-process** primitive keyed by the effective home — a named mutex, or an exclusively
  opened lockfile beside the provisional marker (same locking discipline as the allowlist store, f22) —
  not an in-process `Mutex`. A second setup for the same profile that cannot take the lock waits or is
  refused. The provisional marker records a **holder + expiry**; if the holding process dies, the
  marker's expiry lets the next setup **reclaim** it rather than deadlock. On cancellation, timeout,
  login failure, or identity-mismatch the flow **rolls back** — remove the provisional marker and only
  the dirs **this run created** (its staging / freshly-created dir, per the [f2] ownership marker;
  never a pre-existing home), release the lock, commit nothing — so a failed or abandoned setup never
  strands the profile in a half-state, orphans a credential dir, or destroys a valid existing one.
- **[f9] The localhost human-authorization page is an attack surface; specify its invariants.** Bind
  the listener to **loopback only** (`127.0.0.1`, ephemeral port). The approval URL carries an
  **unguessable one-time capability token** with a **short expiry**; the server **validates every
  request** (method, path, token, single-use) and matches the `(root, profile)` **server-side** from
  state it holds, never trusting page/query parameters. All interpolated values are **HTML/URL
  escaped**. The page is opened with `ShellExecute`/`start`; approval consumes the token and advances
  the state machine to the provisional/in-setup state (it does **not** commit the allowlist — see
  f18). No GUI crate; no credential field of our own — sign-in is always the vendor page.
- **Redaction:** the login subprocess's stdout/stderr may echo tokens/URLs; captured for
  liveness/timeout only, never logged or returned.
- **[deferred-to-impl, from the design]** exact login command per reviewer, non-TTY browser callback
  handling, child stdin (closed), bounded timeout, cancellation.

### Phase 3 tests

- allowlist round-trip; entry keyed on the immutable root; a `--cwd`-supplied root does not satisfy
  an entry made for a different launch root (or forces confirmation).
- `profile_authorized` denies without an entry, allows with one; `ExplicitHome` gated the same way.
- ACL applied to a created dir (best-effort / permissions-visible assertion).
- setup tool state machine with a mocked login (no real OAuth in unit tests).
- guardrail refuses on a simulated start→final fingerprint switch (compares against the captured
  start identity, not a reread).
- localhost approval: a request with a missing/expired/reused token is rejected; the `(root, profile)`
  is matched server-side, not from query params.
- **`smoke.ps1`** end-to-end once, since Phase 3 touches spawning and the MCP surface (real login,
  costs tokens — flagged to the user).

---

## Cross-cutting

- **Build/test discipline:** `.\build.ps1` (fmt, clippy `-D warnings`, tests, release) before handing
  each phase back; `smoke.ps1` only for Phase 3. Never commit `dist\cross-review.exe`.
- **Ethos:** no new crate unless unavoidable; the ACL work is new FFI in `winjob`'s *style* (not a
  reuse — `winjob` is job objects, not ACLs); keep the serde-only footprint.
- **Claim discipline:** the identity/method probe surfaces are now **verified** (Codex `auth.json`
  `auth_mode`/`tokens.account_id`/id_token claims and `codex doctor`; Claude `auth status`
  `authMethod`/`subscriptionType`/`orgId`/`email`) — see the assertion bullet. The one remaining
  `[assumed]` item is the Codex provider-var list, and the cleared-plus-allowlist child environment
  makes it non-load-bearing: an unknown provider var is simply absent rather than relied upon to have
  been removed.

## Resolved questions (from review)

1. **Immutable launch root — resolved:** the process-start cwd *is* available before `--cwd` parsing;
   capture and canonicalize it in `main.rs` before `Config::from_args`, key the allowlist on it, and
   require explicit confirmation when `--cwd` is supplied. (Phase 3.)
2. **Assertion granularity — resolved (surfaces since verified):** the probe reads real local
   surfaces — Codex `auth.json` `auth_mode`/`tokens.account_id`/id_token (and `CODEX_HOME`-aware
   `codex doctor`), Claude `auth status` `authMethod`/`subscriptionType`/`orgId`/`email` — for a
   typed, version-pinned method+identity assertion that fails closed on an unrecognised shape or a
   non-subscription method. The controlled environment is the structural guarantee the profile
   account is used; the assertion confirms it. **Named profiles enable for both reviewers** (the
   earlier "no Codex surface → disable" fear is resolved). See the assertion bullet above. (Phase 3.)
3. **Phase boundaries — decided:** keep Phase 2 (session identity) separate from Phase 1. Given the
   depth the identity/fail-closed contract turned out to need (`f8`), the smaller first diff is worth
   the extra gate round rather than bundling it into an already-large routing-core PR.
