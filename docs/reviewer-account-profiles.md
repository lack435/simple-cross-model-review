# Reviewer account profiles

Status: **plan**, not yet implemented. This note fixes the scope and the mechanism before any
code lands, so the reviewer has something to argue with rather than a diff to reverse-engineer.

## The problem, stated narrowly

`cross-review` spawns a reviewer CLI and lets it resolve its own account from that CLI's default
config home — Codex from `~/.codex` (`$CODEX_HOME`), Claude from `~/.claude` /
`~/.claude.json` (`$CLAUDE_CONFIG_DIR`). The child simply inherits the server's environment;
nothing selects an account.

Codex's desktop app and its CLI **share that default home by default**. So the account a review
bills is an accident of whichever account the user last signed the *desktop app* into. Concretely:
a developer doing work in Claude Code, with their **personal** Codex desktop app signed in, spends
**personal** Codex usage on **work** reviews — silently, with nothing in the response to show it
happened.

The fix controls exactly one thing: **the account of the reviewer `cross-review` spawns.** It does
not touch, and must not touch, the account of the tool the user is *driving* (their Claude Code or
Codex caller session). That session is already authenticated before `cross-review` is ever invoked;
it is out of scope, deliberately. See [Explicitly out of scope](#explicitly-out-of-scope).

## Mechanism

Both reviewer CLIs relocate their entire auth + config state through one environment variable each
— `CODEX_HOME` and `CLAUDE_CONFIG_DIR` — and both **relocate the OAuth credential store**, not just
settings (verified for Claude in its docs; relied on already for Codex in `codex_home()`,
`src/reviewer/codex.rs`). That variable is the whole seam.

A **profile** is a dedicated config home holding one signed-in account:

```
%LOCALAPPDATA%\cross-review\profiles\codex\<name>\     -> CODEX_HOME
%LOCALAPPDATA%\cross-review\profiles\claude\<name>\    -> CLAUDE_CONFIG_DIR
```

`cross-review` sets the variable **on the child `Command` only** (`.env(...)`), never on its own
process environment. Because the reviewer then reads `...\profiles\codex\work\auth.json` instead of
`~\.codex\auth.json`, it is completely immune to what the desktop app is doing — different file,
different login, no collision in either direction. The dedicated home *is* the isolation.

### Config surface

- `--codex-profile <name>` / `--claude-profile <name>`: resolve to
  `{base}\profiles\{reviewer}\{name}`, where `{base}` is `%CROSS_REVIEW_HOME%` or, unset,
  `%LOCALAPPDATA%\cross-review`.
- `--codex-home <abs>` / `--claude-config-dir <abs>`: explicit escape hatch, wins over the profile
  name.
- **Neither set**: inherit the ambient environment exactly as today. This feature is purely
  additive; existing setups are unchanged.

### Per-repo configuration is a name, never a path

The profile *name* is a non-secret shared label. It resolves to a per-user directory by
construction, so one committed config produces a different, private credential home on every
machine. This is what makes a team repo safe to configure:

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

1. Creates the profile directory with an NTFS ACL locked to the current user.
2. Sets `CODEX_HOME` / `CLAUDE_CONFIG_DIR` to it and spawns the **vendor's own** login
   (`codex login`, `claude` sign-in), which opens the subscription OAuth page in the default
   browser. `cross-review` never sees the credential — token storage stays the vendor's problem,
   and we stay clear of the "never enter credentials" boundary.
3. Polls the profile's `auth.json` / `.credentials.json`, then confirms the resolved account.

The single detail this exists to guarantee: **the login must land in the dedicated home, not the
default.** A teammate who runs a bare `codex login` signs into `~/.codex` — which does nothing for
reviews and can clobber their desktop personal login. The tool spawning the vendor login with
`CODEX_HOME` already pointed at the profile dir is the reason the tool owns login rather than a doc
telling people to run a command.

An optional visual profile manager (a status grid of which accounts are set up) should be a **tiny
localhost HTML page opened in the default browser**, not a GUI toolkit — it reuses the browser the
OAuth already uses and keeps the serde-only, self-contained-binary posture. It must never render a
credential field of its own; sign-in is always the vendor OAuth page. This is polish, sequenced
after the tool-only flow.

### Subscription implications (all accepted, none blocking)

- **No central provisioning.** Every teammate OAuths their own work account once, on their own
  machine. Reinforces "credentials never leave the box"; the per-user ACL'd dir stays necessary.
- **Token refresh writes back to the dedicated home**, touched only by `cross-review`-spawned
  reviewers, so it never fights the desktop app's `~/.codex`.
- **The same subscription used by a desktop app and the reviewer home** shares that account's own
  concurrency/rate limits — already handled by the usage-remaining gate
  (`src/reviewer/mod.rs`), not a new problem.

## Correctness requirements

These are the traps that will make an implementation subtly wrong if skipped:

1. **Thread the profile home through `Config`.** Today the preflight (`auth_check`), the account
   fingerprint (`account_fingerprint`), and the invocation each read the home from *ambient*
   environment or set it independently. If the profile is applied to the child but the preflight and
   fingerprint keep reading ambient env, they check a **different account** than the one that runs.
   The profile home must be one value in `Config` that all three read.
2. **Set env per-`Command`, never `std::env::set_var`.** Reviews run concurrently; a process-global
   mutation to select an account is a race that cross-wires two reviews.
3. **Bind the profile / account into the session identity.** A resumed reviewer session must not
   cross accounts. This joins the existing reviewer/model/working-root identity tuple, failing a
   mismatched resume the same way `SESSION_NOT_RESUMABLE` already does.
4. **Read account identity from the credential file, not from an env var.** Claude Code (v2.1.195+)
   deliberately ignores account-identity variables set via environment; trusting env for identity
   would be both wrong and spoofable. Read the account id from the home's own files, as
   `claude_account_id` / `codex_account_id` already do.

### Optional guardrail

Because the dedicated home already makes the reviewer's account deterministic, a guardrail is polish
rather than a rescue. If added: after a review, confirm the reviewer resolved to the profile's
intended account via the existing `account_fingerprint`, and surface a mismatch loudly. Severity
(warn vs refuse) is an open decision below.

## Explicitly out of scope

- **The caller's account.** `cross-review` never changes which account the user's Claude Code /
  Codex session is signed into. That is chosen at launch, before `cross-review` runs, and belongs to
  the host tool.
- **`apiKeyHelper` and per-project launch wiring.** These were considered for routing the *caller's*
  account per repo and are dropped with the caller half. (For the record: `CLAUDE_CONFIG_DIR` set via
  `.claude/settings.json` `env` does **not** redirect Claude Code's own auth — it is read at startup,
  before settings, and the `env` block reaches only subprocesses. So the declarative caller route was
  a dead end regardless.)
- **API-key auth.** All accounts in view are subscription OAuth. No key minting or distribution.

## Open decisions

1. **Setup surface:** MCP tool only to start, or ship the localhost manager page in the first cut.
   (Leaning tool-only first.)
2. **Profile scope:** independent per-reviewer profiles, or a named *bundle* pairing a Claude and a
   Codex account under one context label (`work`, `personal`).
3. **Guardrail severity** on a reviewer/profile account mismatch: warn in every response, or refuse
   the review.
