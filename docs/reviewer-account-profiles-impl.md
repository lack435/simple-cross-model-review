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
  `Config` threading through every read site, per-`Command` env + provider-var scrub, the exact
  resolved-account assertion. A profile *request* is parsed and validated but *refused at use* (no
  authorization yet).
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
  `{base}\profiles\{reviewer}\{n}`, then the same hardening `codex_sterile_dir` already applies
  (`src/reviewer/mod.rs` ~428): canonicalize, **reject reparse points** (`file_attributes() &
  0x400`), **canonical containment** under the profile root via `is_within`. `ExplicitHome(p)` →
  canonicalize + reparse-reject, no containment (it is deliberately outside the root) but
  local/trusted-only (Phase 3 gates it). `base` = `%CROSS_REVIEW_HOME%`, else
  `%LOCALAPPDATA%\cross-review` — **[decided]** deliberately *not* `--state-dir`, which is
  user/repo-settable and must never determine a credential home.
- **`Config::reviewer_home(spec) -> Option<PathBuf>`**: the resolved effective home for an entry, or
  `None` for `Ambient`. Single source of truth for the read sites below.

### `src/config.rs` / `src/reviewer/*` — thread the home through *every* read site

[design requirement 2] The home must reach all of these from `Config`, not ambient env. `codex_home`
(`src/reviewer/codex.rs` ~363) and `claude_config_path` (`src/reviewer/claude.rs` ~444) become
`(cfg, spec)`-parameterised, returning `reviewer_home(spec)` when set and falling back to ambient
env only for `Ambient`. Call sites to convert:

- `auth_check` — **signature gains `spec`** (`fn auth_check(&self, bin, cfg, spec, cancel)`), since
  today it takes only `(bin, cfg, cancel)` and cannot know the profile. Ripples to the trait
  (`src/reviewer/mod.rs` ~521), both adapters, `ensure_entry_ready` (`src/tools.rs` ~78), and tests.
- `account_fingerprint` — already `(cfg, spec)`; switch its `codex_home()` / `claude_config_path()`
  to the parameterised form.
- `invocation` — set the child env (below).
- **`observe_headroom` / `find_rollout(&codex_home())`** (`src/reviewer/codex.rs` ~326-336) — the
  site the design's first draft missed. Same conversion, or the usage gate reads headroom from the
  wrong home.
- `usage_headroom_key` (`src/tools.rs` ~94) already funnels through `account_fingerprint`, so it
  inherits the fix — call out in a test that its key is the profile account.

### `src/reviewer/claude.rs` + `codex.rs` — child env + scrub + assertion

- **Set the home per-`Command`** ([design requirement 4], never `std::env::set_var`): in `invocation`
  *and* `auth_check`, when `reviewer_home(spec)` is `Some(home)`, `cmd.env("CODEX_HOME"/
  "CLAUDE_CONFIG_DIR", home)`. `Ambient` sets nothing (byte-for-byte today's behaviour).
- **Scrub conflicting provider vars** ([design requirement 1]) via `cmd.env_remove(...)` on the child:
  Claude `ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN`, `CLAUDE_CODE_OAUTH_TOKEN`; Codex **[assumed]**
  `OPENAI_API_KEY`, `CODEX_API_KEY`, `CODEX_ACCESS_TOKEN`. The list is version-pinned and treated as
  possibly-incomplete — the assertion below is the guarantee.
- **Exact resolved-account assertion** ([design requirements 1, f2]): after `auth_check`, compare the
  account the CLI reports it is authenticated as against the account in the profile home's credential
  file (`account_fingerprint`). Two sub-parts, both **deferred-to-impl to verify exact fields**:
  (a) the CLI-reported identity must be extracted at a granularity that is *comparable* to the file
  fingerprint — `claude auth status` reports email/org *name* while the fingerprint is the UUID
  (`src/reviewer/claude.rs` ~454), so the probe compares on a common field (email+org) or obtains the
  UUID, pinned to a known output shape and failing closed on anything unrecognised; (b) it asserts
  the auth *method* is the subscription OAuth, not an API key that slipped the scrub. Only `Ambient`
  skips the assertion.
- **Run the assertion per spawn, not from a stale cache** ([f3]): the preflight cache
  (`ensure_entry_ready`, `src/tools.rs` ~52, keyed by entry index, cached for process lifetime) must
  not let a review reuse a success after a profile was re-authenticated under the running server.
  **[decided]** the cache key (or the cached `Preflight`) includes the current account fingerprint,
  revalidated cheaply before reuse; a fingerprint change misses the cache and re-runs auth+assertion.

### Phase 1 authorization stub

`fn profile_authorized(cfg, spec) -> bool` returns `false` whenever the selector is non-`Ambient`
(no store yet). The routing path refuses a profile use with an actionable failure (`PROFILE_NOT_
AUTHORIZED`, pointing at the not-yet-shipped setup). `Ambient` is always allowed. Phase 3 replaces
the body.

### Phase 1 tests (`cargo test`, no network)

- `validate_profile_name`: accept/reject table (`.`, `..`, `a/b`, `..\\x`, `C:x`, empty, valid).
- `effective_home`: containment holds; a traversal name, a rooted/drive name, and a reparse point
  are refused; `%CROSS_REVIEW_HOME%` override honoured; `Ambient` → `None`.
- argv/env inspection (extend `src/reviewer/argv_tests.rs`): `CODEX_HOME`/`CLAUDE_CONFIG_DIR` set on
  the child for a named profile; the provider vars are `env_remove`d; **`Ambient` sets and removes
  nothing** (regression guard).
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
    effective_home: Option<String>,   // canonical resolved home; None only for Ambient
    account_fingerprint: String,      // the account that actually ran
}
...
#[serde(default, skip_serializing_if = "Option::is_none")]
pub profile_identity: Option<ProfileIdentity>,
```

[f1] The tag is what separates a **new `Ambient`** session (persists `Some(ProfileIdentity{ selector:
Ambient, .. })`, resumes normally) from a **legacy** record (`profile_identity: None`, fail-closed
non-resumable). Reserving `None` for legacy is the whole point — do not represent ambient as absence.

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
- ambient-new (`Some(Ambient)`) resumes; legacy (`None`) is refused.
- resume refused when the effective home changes (same name, different `%CROSS_REVIEW_HOME%`) and
  when the account fingerprint changes.
- selector is part of `resume_entry_index` identity (a profile-differing entry does not match).

---

## Phase 3 — Authorization, provisioning, setup

### Allowlist store

A per-machine store under `%CROSS_REVIEW_HOME%` / `%LOCALAPPDATA%\cross-review` — **[decided]** a
fixed, ACL-protected location the repo cannot point at (never `--state-dir`). Entries bind
**[f8/f9]** `(immutable root) → (canonical effective home + reviewer family)`. **[assumed / design
point for the reviewer]** the *immutable root* is the server's own launch cwd captured at process
start, before any repo-supplied `--cwd` is applied (`src/config.rs` ~765/829 read `--cwd`); if the
launch cwd cannot be established independently of repo config, fall back to requiring an explicit
confirmation whenever `--cwd` is supplied. A change to the mapping forces reauthorization.

### Profile-dir provisioning + ACL

Create the profile home with an ACL restricted to the current user. **[decided]** reuse the FFI
approach `src/winjob.rs` already uses for job objects rather than adding a crate; if a direct
`SetNamedSecurityInfo`/`icacls` path is simpler and dependency-free, note it. Apply the same
canonicalize + reparse-reject + containment checks as `effective_home`.

### Wire authorization + the guardrail

- `profile_authorized` (the Phase 1 stub) now consults the store: a non-`Ambient` selector is allowed
  only with a matching `(immutable root → effective home)` entry; otherwise `PROFILE_NOT_AUTHORIZED`
  with a remediation pointing at the setup tool.
- Guardrail ([design "refuse on mismatch"]): the post-review `account_fingerprint` re-check refuses
  (does not deliver) on a mid-review account switch.

### Setup MCP tool + one-off localhost confirmation page

- New MCP tool (`src/tools.rs` / `src/mcp.rs`): validates the name, **requires human authorization**
  (below), creates the ACL'd dir, spawns the vendor login with the home env set (`codex login` /
  `claude` sign-in) which opens the browser OAuth, polls the credential file, confirms the resolved
  account, and writes the allowlist entry.
- **Human-authorization gate** ([design point 3, f6]): a tiny `std::net` localhost listener serves a
  single-purpose approve page ("authorize root X → profile `work`? [approve]"), opened with
  `ShellExecute`/`start`; approval writes the allowlist entry / proceeds with login. No GUI crate; no
  credential field of our own — sign-in is always the vendor page.
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
- guardrail refuses on a simulated fingerprint switch.
- **`smoke.ps1`** end-to-end once, since Phase 3 touches spawning and the MCP surface (real login,
  costs tokens — flagged to the user).

---

## Cross-cutting

- **Build/test discipline:** `.\build.ps1` (fmt, clippy `-D warnings`, tests, release) before handing
  each phase back; `smoke.ps1` only for Phase 3. Never commit `dist\cross-review.exe`.
- **Ethos:** no new crate unless unavoidable; ACL/FFI reuses `winjob`'s approach; keep the
  serde-only footprint.
- **Claim discipline:** the `[assumed]` items — the Codex provider-var list, the exact-identity probe
  fields, and the immutable-launch-root mechanism — are the three things to verify while building,
  and each fails closed if the assumption is wrong.

## Open questions for the reviewer

1. **Immutable launch root:** is the server's process-start cwd reliably knowable independently of a
   repo-supplied `--cwd`, so the allowlist can key on it? If not, is confirm-on-`--cwd` an acceptable
   substitute, or is there a better host-provided root?
2. **Assertion granularity:** is comparing Claude's `auth status` email/org against the profile
   file's account acceptable, or must the probe obtain the UUID-level identity to match
   `account_fingerprint` exactly?
3. **Phase boundaries:** is folding Phase 2 into Phase 1 preferable (identity is cheap once the home
   is threaded), or is the smaller first diff worth the extra round?
