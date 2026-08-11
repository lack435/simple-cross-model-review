# Reviewer account profiles

Status: **plan**, not yet implemented. This note fixes the scope and the mechanism before any
code lands, so the reviewer has something to argue with rather than a diff to reverse-engineer.

Following the README's discipline, claims are marked **[verified]** (checked against this tree or a
cited source), **[assumed]** (believed but not verified here — an implementation task must confirm),
or **[decided]** (a design choice, not a fact). Line references are to the tree at the time of
writing and are illustrative, not load-bearing.

## The problem, stated narrowly

`cross-review` spawns a reviewer CLI and lets it resolve its own account from that CLI's default
config home — Codex from `~/.codex` (`$CODEX_HOME`), Claude from `~/.claude` / `~/.claude.json`
(`$CLAUDE_CONFIG_DIR`). **[verified]** The reviewer child inherits the server's environment; no
per-child env is set today (`src/reviewer/claude.rs`, `src/reviewer/codex.rs` set only
`current_dir` and args). Nothing selects an account.

Codex's desktop app and its CLI **share that default home by default**. **[assumed]** So the account
a review bills is an accident of whichever account the user last signed the *desktop app* into.
Concretely: a developer doing work in Claude Code, with their **personal** Codex desktop app signed
in, spends **personal** Codex usage on **work** reviews — silently, with nothing in the response to
show it happened.

The fix controls exactly one thing: **the account of the reviewer `cross-review` spawns.** It does
not touch, and must not touch, the account of the tool the user is *driving* (their Claude Code or
Codex caller session). See [Explicitly out of scope](#explicitly-out-of-scope).

## Mechanism

Both reviewer CLIs relocate their entire auth + config state through one environment variable each
— `CODEX_HOME` and `CLAUDE_CONFIG_DIR` — and both **relocate the OAuth credential store**, not just
settings. **[verified]** for Claude (`CLAUDE_CONFIG_DIR` relocates `.credentials.json`, per its
authentication docs); **[verified]** the tool already relies on `codex_home()` reading `$CODEX_HOME`
(`src/reviewer/codex.rs`). That variable is the whole seam.

A **profile** is a dedicated config home holding one signed-in account:

```
%LOCALAPPDATA%\cross-review\profiles\codex\<name>\     -> CODEX_HOME
%LOCALAPPDATA%\cross-review\profiles\claude\<name>\    -> CLAUDE_CONFIG_DIR
```

`cross-review` sets the variable **on the child `Command` only** (`.env(...)`), never on its own
process environment. Because the reviewer then reads `...\profiles\codex\work\auth.json` instead of
`~\.codex\auth.json`, it is immune to what the desktop app is doing. The dedicated home *is* the
isolation.

Setting the home is necessary but **not sufficient** — the child's *other* inherited environment can
still override it. See correctness requirement 1 (auth-var precedence) below.

### Config surface

- `--codex-profile <name>` / `--claude-profile <name>`: resolve to
  `{base}\profiles\{reviewer}\{name}`, where `{base}` is `%CROSS_REVIEW_HOME%` or, unset,
  `%LOCALAPPDATA%\cross-review`.
- `--codex-home <abs>` / `--claude-config-dir <abs>`: explicit escape hatch, wins over the profile
  name.
- **Neither set**: inherit the ambient environment exactly as today. This feature is purely
  additive; existing setups are unchanged.

**[decided] Profiles bind per reviewer-chain entry, not globally.** Reviewer identity today is
per `ReviewerSpec` (`src/config.rs`, ~111-145: reviewer/model/effort/bin per entry). A profile is
part of that identity, so each fallback entry carries its own — a Codex→Codex fallback to a second
account is expressible, and a Claude fallback entry carries its own `CLAUDE_CONFIG_DIR`. This has
consequences that must be honoured, not discovered: the profile participates in **duplicate
detection**, **same-family fallback** matching, **resume matching** (requirement 3), and the
**usage-log key** (requirement 2). An implementation that bolts a global profile onto per-entry
specs would silently mis-key all four.

**[decided] Profile names are safe names, validated before use.** A name flows from committed repo
config (`.mcp.json` args), which is inside this project's untrusted-repo threat model. Constructing
`{base}\profiles\{reviewer}\{name}` from an unvalidated name lets `..`, a rooted or drive-prefixed
path, or a reparse point escape the profile root — the same class of hazard `codex_sterile_dir`
already defends against. Required: a strict grammar (`[A-Za-z0-9._-]+`, non-empty, not `.`/`..`, no
separators, no drive prefix), a **canonical containment check** that the resolved directory is
under the profile root, **reparse-point rejection** (as `codex_sterile_dir` does via
`file_attributes() & 0x400`), and verification that the parent chain's ACLs are ours before writing
into it. This is containment only — it stops a name from escaping the profile root, but **does not
authorize which profile a repo may select**; that is a separate local decision, [Trust
boundaries](#trust-boundaries) point 1.

### Per-repo configuration is a name, never a path

The profile *name* is a non-secret shared label. It resolves to a per-user directory by
construction, so one committed config produces a different, private credential home on every
machine. This is what makes a team repo safe to configure.

`C:\dev\mattekar` (personal), `.mcp.json`:

```json
{ "mcpServers": { "cross-review": { "command": "...cross-review.exe",
  "args": ["--reviewer","codex","--model","gpt-5.6-luna","--effort","max",
           "--codex-profile","personal"], "timeout": 2400000 } } }
```

`C:\dev\main\UE` (work, team-shared), `.mcp.json` — **identical for everyone**, only the label
`work` in it, no path, no email, no token:

```json
{ "mcpServers": { "cross-review": { "command": "cross-review.exe",
  "args": ["--reviewer","codex","--model","gpt-5.6-luna","--effort","max",
           "--codex-profile","work"], "timeout": 2400000 } } }
```

The reverse direction (`.codex/config.toml`, Claude reviewing) adds `--claude-profile <name>` the
same way. Nothing per-person is ever committed; the only per-person state — the signed-in account —
lives in `%LOCALAPPDATA%`, outside the repo, with nothing to `.gitignore`.

## Setup UX: terminal-less, subscription OAuth

The audience is not terminal users; PowerShell is a hard sell, and most work from a desktop/IDE
surface. So setup is an **MCP tool** the user invokes from chat ("set up my work account for
reviews"), not a documented shell command.

Because this tool writes credential state and can overwrite a profile, it is gated by an explicit
human authorization step and is never completed on a model decision alone — [Trust
boundaries](#trust-boundaries) point 3.

The tool, per profile:

1. Confirms human authorization (see above), then creates the profile directory with an NTFS ACL
   locked to the current user (and validates the name per the safe-name rule above).
2. Sets `CODEX_HOME` / `CLAUDE_CONFIG_DIR` to it and spawns the **vendor's own** login, which opens
   the subscription OAuth page in the default browser. `cross-review` never sees the credential.
3. Polls the profile's credential file for arrival, then confirms the resolved account.

The single detail this exists to guarantee: **the login must land in the dedicated home, not the
default.** A teammate who runs a bare `codex login` signs into `~/.codex` — which does nothing for
reviews and can clobber their desktop personal login. The tool spawning the vendor login with the
home already pointed at the profile dir is the reason the tool owns login rather than a doc.

**This protocol must be specified and tested before tool-only is chosen as the first surface**
(currently underspecified — an explicit open item):
- the exact login command per reviewer, and how a non-TTY child reaches the browser callback;
- child stdin handling (closed vs. piped), a bounded timeout, and cancellation;
- output capture that **redacts** — the login subprocess's stdout/stderr may echo tokens or URLs
  with secrets; none of it may be logged or returned;
- what "logged in" is confirmed against (the credential file existing is necessary but the
  *resolved account* is what the tool reports).

**[decided] First cut is the MCP tool plus a minimal one-off localhost confirmation page — no full
manager yet.** The human authorization gate (point 3) and first-use confirmation (point 1) both need
a surface a non-terminal user can act on; a small, single-purpose localhost page opened in the
default browser ("authorize root X to use profile `work`? [approve]") serves that without a GUI
toolkit and reuses the browser the OAuth already uses. The full visual profile manager (a status
grid of every profile's set-up / logged-in / authorized state) is the same tiny-localhost-page
technique scaled up and is **deferred** — it is polish, sequenced after the tool-only flow. Neither
page ever renders a credential field of its own; sign-in is always the vendor OAuth page.

### Credentials at rest

**OS ACL is the only protection, and the plan does not add another layer.** `cross-review` stores no
secret of its own — it only *causes* the vendor CLI to write its normal credential file into a
different directory. So there is nothing for us to encrypt; the honest posture is NTFS-ACL-only,
same protection class the vendors already rely on by default, applied to a per-user directory we
create with an explicit restrictive ACL. (No DPAPI layer is proposed; we hold no plaintext to wrap.)
**The ACL protects the credentials from *other users*, not from the reviewer process itself** — it
runs as the same user with unconfined reads, so it can read the profile credential store; that
exposure is a trust-boundary concern, [Trust boundaries](#trust-boundaries) point 2, not something an
ACL closes. Requirements on our handling of those files:

- **Never log or return their contents.** Today the code reads them wholesale
  (`codex_account_id`, `src/reviewer/codex.rs` ~374-383; `claude_account_id`,
  `src/reviewer/claude.rs` ~454-463) and extracts a single id field. Keep that: read only the
  account-identifier field, never surface token fields, never echo the file.
- **Minimize parsing**, and never turn a malformed file into a hard error that leaks the offending
  contents. But the *failure direction of an unreadable identity depends on what it is for*, and the
  two must not be conflated:
  - **Usage accounting fails open.** An unknown account for the usage-log key degrades to `Unknown`,
    exactly as today (`Headroom::Unknown` never gates) — a missing identity must not block a review.
  - **Preflight and resume fail closed.** The auth-method assertion (requirement 1) and the resume
    identity comparison (requirement 3) must reject an unreadable or mismatched identity
    (`SESSION_NOT_RESUMABLE` for resume), never proceed under an assumed account. These are security
    checks, not accounting, so "unknown" here means *stop*, not *continue*.
- **Define locking and cleanup:** the vendor refresh writes the file concurrently with our reads; a
  read must tolerate a partial write (retry, then apply the direction above for its purpose), and
  profile deletion must remove the ACL'd tree.

### Subscription implications (all accepted, none blocking)

- **No central provisioning.** Every teammate OAuths their own work account once, on their own
  machine. The per-user ACL'd dir stays necessary.
- **Token refresh writes back to the dedicated home**, touched only by `cross-review`-spawned
  reviewers, so it never fights the desktop app's `~/.codex`.
- **The same subscription used by a desktop app and the reviewer home** shares that account's own
  concurrency/rate limits — already handled by the usage-remaining gate (`src/reviewer/mod.rs`).

## Trust boundaries

The reviewed repository *and its committed cross-review config* are untrusted — that is this
project's whole premise. Account routing adds three exposures OS ACLs alone do not close, because
the reviewer and the setup tool run as the same OS user as everything else.

1. **Which account a repo may select is an authorization decision, not just path-safety.** [f5]
   The safe-name rule stops traversal, but a valid name is still attacker-chosen: an untrusted repo's
   `.mcp.json` could name `--codex-profile personal` and route a review — its usage, and (see 2) its
   credential exposure — through an account the user never authorized for that repo. **[decided]**
   Profile *use* is gated locally, not by committed config alone, by **first-use confirmation that
   persists to a local allowlist**: the first time a working root requests a profile, the human
   authorization gate (point 3) approves it once, and the approval is written to a per-machine,
   non-committed allowlist under `%CROSS_REVIEW_HOME%` so later reviews from that root run without
   re-prompting. The confirmation is the gate; the allowlist is its memory. The repo *requests* a
   name; the machine *authorizes* it. The
   `--codex-home` / `--claude-config-dir` absolute escape hatch is local/trusted-only, gets the same
   containment + ACL checks, and is never honoured from committed repo config that is not on the
   local allowlist.

   [f8] The allowlist key must be a root the **repo cannot forge**. Committed project config is the
   CLI source of truth and can set `--cwd` (`src/config.rs`; README), so an allowlist keyed by the
   effective working root lets a hostile repo present an already-authorized path as its key. Bind
   authorization instead to the immutable launch/repository root the host determined *before* repo
   config was applied — or require explicit confirmation whenever `--cwd` is supplied — and keep the
   authorization store itself under a fixed, ACL-protected location, not one the repo can point at.

   [f9] Symmetrically, an allowlist entry authorizes a **resolved target, not a name**. `Profile(name)`
   resolves through `%CROSS_REVIEW_HOME%`, so approving the *name* for a root would let a later base
   change point the same approved name at a different canonical home/account on a *fresh* review — the
   selection path that f7's resume-identity binding does not cover. So an entry binds
   `(immutable root) → (canonical effective home + reviewer family)` — or a stable profile id anchored
   to a trusted root — and a change to that mapping forces reauthorization rather than silently
   inheriting the old approval.

2. **The reviewer can read the profile credential store; ACLs do not stop it.** [f4] The Codex
   reviewer runs as the same user, and — per the README — its read-only posture confines *writes*,
   not *reads*; no CLI surface was found that confines its reads. So a prompt-injected review of a
   hostile repo could read `auth.json` / `.credentials.json` from the profile home and return the
   tokens in its review text. This is already true of the shared `~/.codex` today; profiles neither
   create nor cure it, but the plan must **not** claim OS ACLs protect the credentials from the
   reviewer — they protect against *other users*, not the same-user reviewer process. **[decided]**
   **Accept and document, with the point-1 allowlist as the trust boundary.** Authorizing a working
   root to route through a profile *is* the assertion that the code there is trusted with that
   account's token — the same exposure the reviewer has had over `~/.codex` since day one, neither
   widened nor narrowed per review. This matches the tool's existing posture and adds no new
   machinery. Two supporting notes: a **separate OS identity / credential broker** (running the
   reviewer under a restricted token that cannot read the user's credential homes) is the documented
   hardening path for anyone who must route a profile at genuinely untrusted code, deferred as a
   large lift that fights the small-self-contained-binary posture; and scoping the reviewer's *reads*
   away from the homes was rejected as infeasible — the README records that no CLI surface confines
   Codex's direct reads. This decision bounds where profiles may safely be used, so it is inseparable
   from point 1: the allowlist is not just usage control, it is the security boundary.

3. **The setup/login tool is side-effectful and needs a human gate.** [f6] It writes credential
   state, launches OAuth, and can overwrite a profile. The MCP dispatcher runs model-issued tool
   calls without confirmation, so prompt injection from an untrusted repo could trigger a login or
   silently overwrite a profile. **[decided]** Setup, login, and profile overwrite require explicit
   human authorization — a client-side confirmation or a local one-time authorization token — and are
   never completed on a model decision alone.

## Correctness requirements

These are the traps that will make an implementation subtly wrong if skipped:

1. **Give the child a controlled environment (not the inherited one), then verify the resolved
   method.** **[verified]** Claude's auth precedence puts `ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN`,
   and `CLAUDE_CODE_OAUTH_TOKEN` *ahead* of the subscription OAuth credentials in `CLAUDE_CONFIG_DIR`
   (per its authentication docs). So pointing `CLAUDE_CONFIG_DIR` at the work profile is defeated if
   any of those is present in the inherited environment — the review silently uses the wrong
   credential. Codex has analogous variables — **[assumed]** at least `OPENAI_API_KEY`,
   `CODEX_API_KEY`, and `CODEX_ACCESS_TOKEN`. The child `Command` must therefore run in a **controlled
   environment** — cleared and rebuilt from a vetted allowlist (OS essentials + the profile home var)
   so no inherited provider key is present to win over the profile OAuth. (Exact allowlist in the
   implementation plan.)

   The controlled environment is the first line, not the guarantee — the enumerated provider-variable
   names are **version-pinned and cannot be trusted as complete** (a future reviewer release could add
   one). Because the child starts from a cleared environment plus a vetted allowlist, an unknown
   provider variable is simply absent; and **the guarantee is a pre-spawn identity assertion.** Before
   the review invocation, the preflight must **assert the resolved auth method and account match the
   profile**, failing closed on a mismatch — which catches any provider variable that ever did slip
   through, because a bypass shows up as a resolved account that is not the profile's. It runs *before*
   the child spawns,
   so the first request cannot go out under the wrong account; a separate post-review check is only
   the residual mid-run *switch* guard, not this assertion. The provider-variable list must be pinned
   to the supported reviewer version and each provider-auth path tested; the assertion is what makes an
   incomplete list safe rather than silently wrong. (Probe mechanics — UUID-level identity, fail-closed
   on unknown output — are specified in the implementation plan.)

   **The assertion must be an *exact identity probe*, run per spawn.** [f2] Today's preflight is too
   weak to serve as that assertion: `claude auth status` reports the account *email / org name* while
   the routing fingerprint is the account/org *UUID* (`account_fingerprint`), and the Codex preflight
   only checks exit status. The assertion must compare the **same fingerprint identity the routing
   uses**, be pinned to a known reviewer-output version, and fail closed on unrecognised output —
   never pass on a display-string match. [f3] It must run on **every actual spawn**: the
   process-lifetime preflight cache keyed by entry index (`src/tools.rs`) would otherwise let a review
   reuse a stale success after a profile was re-authenticated under the still-running server, so
   either the cache key includes the current account fingerprint (revalidated before reuse) or the
   assertion re-runs at spawn regardless of cache.

2. **Thread the resolved home through *every* read site, not just the obvious three.** The profile
   home must be one value in `Config` that all of these read — auditing every `codex_home()` /
   `claude_config_path()` call site, because missing one splits identity from behaviour:
   - the preflight `auth_check`;
   - `account_fingerprint` (the usage-log key);
   - `invocation` (the child that actually runs);
   - **`observe_headroom` / `find_rollout(&codex_home())`** (`src/reviewer/codex.rs` ~326-336, 363-371)
     — missed in the first draft. If headroom is read from the ambient home while usage is keyed to
     the profile account, the usage gate compares an account's key against another account's
     headroom, or records `Unknown`.

3. **Persist the profile identity and fail closed on resume.** **[verified]** `SessionRecord`
   (`src/session.rs` ~73-173) has no profile/account field, and `resume_entry_index`
   (`src/config.rs` ~939-970) does not compare account identity — so binding "into the session
   identity" is not automatic, it is new persisted state. Required: persist the **normalized profile
   identity and the account fingerprint** on the record; compare both before spawning a resume;
   reject a missing or mismatched identity with `SESSION_NOT_RESUMABLE`.

   [f1] Persist the identity as a **tagged value** — `Ambient` / `Profile(name)` / `Explicit(path)` —
   serialized explicitly, and reserve *absence of the field* for legacy records only. Otherwise a new
   no-profile (ambient) session, which by design must keep resuming, is indistinguishable from a
   legacy record, which must not: both would look like "no identity." With the tag, an `Ambient`
   record carries its own fingerprint and resumes normally, while a field-absent legacy record is
   non-resumable (fail closed), never resumed under an assumed account.

   [f7] The tag must identify the **canonical effective home**, not just the name. `Profile(name)`
   resolves to a different directory under a different `%CROSS_REVIEW_HOME%` (or default root), and
   the account fingerprint only catches a *different account* — not the *same account in a different
   home* (a different credential/config/session store). So the persisted identity is the resolved,
   canonicalized home path plus reviewer family (or a stable local profile id anchored to a trusted
   root), and resume compares that, not the bare name.

4. **Set env per-`Command`, never `std::env::set_var`.** Reviews run concurrently; a process-global
   mutation to select an account is a race that cross-wires two reviews.

5. **Read account identity from the credential file, not from an env var.** **[verified]** Claude
   Code (v2.1.195+) ignores account-identity variables set via environment; trusting env for identity
   would be wrong and spoofable. Read the id from the home's own files, as `claude_account_id` /
   `codex_account_id` already do.

### Guardrail on a detected mismatch

The primary defence is requirement 1's assertion, which fails closed *at spawn*, before tokens are
spent. This guardrail covers only the residual case — a mismatch that emerges *during* a review (e.g.
a mid-run token refresh switches accounts): after a review, confirm the reviewer resolved to the
profile's intended account via the existing `account_fingerprint`. **[decided] On a mismatch,
refuse** — do not deliver a review whose provenance is wrong, consistent with requirement 1's
fail-closed posture and the project's explicit-refusal-over-silent-fallback stance. A warning that
still delivers the review invites being ignored, which is the failure mode this whole feature exists
to prevent.

## Explicitly out of scope

- **The caller's account.** `cross-review` never changes which account the user's Claude Code /
  Codex session is signed into. That is chosen at launch, before `cross-review` runs, and belongs to
  the host tool. This holds regardless of the host's internal auth-resolution details.
- **Routing the caller's account per repo (e.g. `apiKeyHelper`, launch-env wiring).** Dropped with
  the caller half. How the host resolves its *own* auth — including whether a committed
  `.claude/settings.json` can redirect it — is host behaviour that varies across host versions;
  `cross-review` neither relies on nor asserts any particular mechanism here, so this plan makes **no
  claim** about it (the earlier draft's statement about `.claude/settings.json` `env` scope is
  removed as unverified rather than replaced with the opposite). The only claim is scope: the caller
  account is not ours to route. Distinct and not a host claim: `CODEX_HOME` / `CLAUDE_CONFIG_DIR` set
  in a *process's own environment before launch* relocate that process's credential store — that is
  exactly the mechanism this plan uses for the reviewer child, verified for the reviewer, and says
  nothing about how any host process behaves.
- **API-key auth.** All accounts in view are subscription OAuth. No key minting or distribution.

## Decisions

The four design forks are resolved:

1. **Reviewer read-exposure** ([Trust boundaries](#trust-boundaries) point 2): **accept and document,
   with the point-1 allowlist as the trust boundary.** Separate-OS-identity is the documented
   hardening path; read-scoping was rejected as infeasible.
2. **Profile-use authorization** ([Trust boundaries](#trust-boundaries) point 1): **first-use
   confirmation that persists to a local allowlist.** The invariant is fixed: key on the immutable
   launch/repository root (never a repo-settable `--cwd`), target the canonical effective home +
   reviewer family (never a bare name), reauthorize when that mapping changes.
3. **Setup surface:** **MCP tool plus a minimal one-off localhost confirmation page**; the full
   visual manager is deferred.
4. **Mismatch severity:** **refuse (fail closed)**, consistent with requirement 1.

(The earlier "profile scope" decision is resolved above: per reviewer-chain entry, not global.)

**Deferred to implementation, not design forks:** the exact terminal-less login protocol (login
commands, non-TTY browser callback, stdin/timeout/cancellation, output redaction — see
[Setup UX](#setup-ux-terminal-less-subscription-oauth)) and the version-pinned exact-identity probe
(requirement 1) must be specified and tested when built.
