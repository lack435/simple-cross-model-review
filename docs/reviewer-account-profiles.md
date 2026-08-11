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
into it.

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

The tool, per profile:

1. Creates the profile directory with an NTFS ACL locked to the current user (and validates the name
   per the safe-name rule above).
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

An optional visual profile manager (a status grid) should be a **tiny localhost HTML page opened in
the default browser**, not a GUI toolkit — it keeps the serde-only, self-contained-binary posture.
It must never render a credential field of its own; sign-in is always the vendor OAuth page. Polish,
sequenced after the tool-only flow.

### Credentials at rest

**OS ACL is the only protection, and the plan does not add another layer.** `cross-review` stores no
secret of its own — it only *causes* the vendor CLI to write its normal credential file into a
different directory. So there is nothing for us to encrypt; the honest posture is NTFS-ACL-only,
same protection class the vendors already rely on by default, applied to a per-user directory we
create with an explicit restrictive ACL. (No DPAPI layer is proposed; we hold no plaintext to wrap.)
Requirements on our handling of those files:

- **Never log or return their contents.** Today the code reads them wholesale
  (`codex_account_id`, `src/reviewer/codex.rs` ~374-383; `claude_account_id`,
  `src/reviewer/claude.rs` ~454-463) and extracts a single id field. Keep that: read only the
  account-identifier field, never surface token fields, never echo the file.
- **Minimize parsing** and treat a malformed file as "unknown account" (fail-open on identity read,
  as today), never as a hard error that leaks the offending contents.
- **Define locking and cleanup:** the vendor refresh writes the file concurrently with our reads; a
  read must tolerate a partial write (retry / treat as unknown), and profile deletion must remove the
  ACL'd tree.

### Subscription implications (all accepted, none blocking)

- **No central provisioning.** Every teammate OAuths their own work account once, on their own
  machine. The per-user ACL'd dir stays necessary.
- **Token refresh writes back to the dedicated home**, touched only by `cross-review`-spawned
  reviewers, so it never fights the desktop app's `~/.codex`.
- **The same subscription used by a desktop app and the reviewer home** shares that account's own
  concurrency/rate limits — already handled by the usage-remaining gate (`src/reviewer/mod.rs`).

## Correctness requirements

These are the traps that will make an implementation subtly wrong if skipped:

1. **Scrub or reject conflicting auth-provider variables on the child, then verify the resolved
   method.** **[verified]** Claude's auth precedence puts `ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN`,
   and `CLAUDE_CODE_OAUTH_TOKEN` *ahead* of the subscription OAuth credentials in `CLAUDE_CONFIG_DIR`
   (per its authentication docs). So pointing `CLAUDE_CONFIG_DIR` at the work profile is defeated if
   any of those is present in the inherited environment — the review silently uses the wrong
   credential. Codex has the analogous `OPENAI_API_KEY`. The child `Command` must therefore start
   from a controlled environment for these variables (remove/override the provider keys so the
   profile OAuth wins), and the preflight must **assert the resolved auth method and account match
   the profile** (`auth_check` already reports `authMethod`/account for Claude), failing closed on a
   mismatch rather than proceeding under whatever won.

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
   reject a missing or mismatched identity with `SESSION_NOT_RESUMABLE`; and **define legacy-record
   behaviour** — a record written before this field exists has no identity to match, so it must be
   treated as non-resumable (fail closed), never resumed under an assumed account.

4. **Set env per-`Command`, never `std::env::set_var`.** Reviews run concurrently; a process-global
   mutation to select an account is a race that cross-wires two reviews.

5. **Read account identity from the credential file, not from an env var.** **[verified]** Claude
   Code (v2.1.195+) ignores account-identity variables set via environment; trusting env for identity
   would be wrong and spoofable. Read the id from the home's own files, as `claude_account_id` /
   `codex_account_id` already do.

### Optional guardrail

Because the dedicated home already makes the reviewer's account deterministic, a guardrail is polish
rather than a rescue. If added: after a review, confirm the reviewer resolved to the profile's
intended account via the existing `account_fingerprint`, and surface a mismatch. Severity (warn vs
refuse) is an open decision below.

## Explicitly out of scope

- **The caller's account.** `cross-review` never changes which account the user's Claude Code /
  Codex session is signed into. That is chosen at launch, before `cross-review` runs, and belongs to
  the host tool. This holds regardless of the host's internal auth-resolution details.
- **Routing the caller's account per repo (e.g. `apiKeyHelper`, launch-env wiring).** Dropped with
  the caller half. **[assumed, version-dependent]** We understand a committed `.claude/settings.json`
  cannot reliably redirect Claude Code's *own* auth for a session (its `env` block is documented as
  applying to the session's subprocesses, and `CLAUDE_CONFIG_DIR` is resolved early), but this is
  host behaviour we have **not** version-tested and do not control, so the plan makes no load-bearing
  claim about the mechanism — only that the caller account is not ours to route. (`CLAUDE_CONFIG_DIR`
  *does* relocate credentials when set in the process environment before launch — that much is
  agreed; it is the settings-file redirection specifically that is unverified.)
- **API-key auth.** All accounts in view are subscription OAuth. No key minting or distribution.

## Open decisions

1. **Setup surface:** MCP tool only to start, or ship the localhost manager page in the first cut.
   (Leaning tool-only first — gated on the login protocol above being specified and tested.)
2. **Guardrail severity** on a reviewer/profile account mismatch: warn in every response, or refuse
   the review.

(The earlier "profile scope" decision is resolved above: per reviewer-chain entry, not global.)
