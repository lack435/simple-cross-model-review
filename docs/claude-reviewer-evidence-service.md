# Claude reviewer evidence service

Status: approved plan (cross-review session `plan-claude-evidence-service`), on
`plan/claude-evidence-service`. **Phase 0 complete** — all gates cleared against claude 2.1.233
(see "Phase 0 results"); two amendments folded in (MCP allow-list entry in §1; Claude does not fail
closed on a broken evidence server, so §7's parent-side handshake is mandatory). **Phase 1 re-gate
(turn 8): chose option (a)** — evidence is scoped to **profile-pinned, shell-less** isolated Claude
(the single `claude_evidence_enabled(cfg, spec)` predicate, §0), because the existing code keeps
shell-enabled Claude on the repo cwd + shell + `--safe-mode`. f7 requires that one predicate gate
every hook together; the earlier `supplies_change_of` change is dropped (a shell-less isolated
Claude already captures under `--diff auto`). Now implementing Phase 1 under that scope.

Design-review turn 1 (session `plan-claude-evidence-service`) raised four findings, folded in
below with `fN` markers: **f1** narrowed the recovery claim (no range-diff op exists — scope
cut, not machinery added); **f2** requires a verified-empty scratch cwd now that `--safe-mode`
is gone (§6); **f3** keeps Claude's in-prompt capture by not tripping the Codex-only capture
suppression (§2). Turn 2 resolved f1–f3 and raised f4/f5; turn 3 resolved f5 and raised f6;
turn 4 refined f4 and f6 into concrete requirements (both accepted).
**f4** (held across rounds, so treated as correct): the capture guarantee is extended to isolated
Claude by construction (§7), *and* a scoped fail-closed gate returns `EVIDENCE_UNAVAILABLE` on an
empty/incomplete capture unless the evidence service was proven healthy (forced `stream-json` +
startup handshake + no mid-turn transport error + ≥1 successful *content* call) — §7. The earlier
pushback bought the *scoping* (gate fires only on the empty/incomplete sliver), not the absence of
the gate. **f5**: §6 adds a separate `verified_empty_sterile_dir` check, leaving the shared helper
untouched. Turn 5 resolved f4 and left only **f6**, which turn 5 refined again (concurrency): the
profile `CLAUDE_CONFIG_DIR` must not persist auto-memory/`CLAUDE.md` across reviews. §8 gates the
machinery behind Phase 0 — Q1 (does headless `claude -p` even write auto-memory? likely not → f6
closed by assertion), Q2 (disable it via the confirmed key `autoMemoryEnabled: false` in the
settings blob — the expected path), Q3 (a crash-safe **and** concurrency-safe scrub via the
**exclusive** per-home lock — a documented contingency that should not be reached now the key is
known).

## Context

The Claude reviewer direction is **capture-only**. It receives the selected change embedded
in its prompt and has no way to look past it: `--safe-mode` gives it no shell, and its only
tools are `Read`/`Grep`/`Glob` scoped to the repository. Everything a Claude review concludes
therefore rests on one static capture being right.

That capture is pinned per server entry by `--diff`, not chosen per call. When it comes back
thin, empty, or stale the reviewer cannot compensate — it reviews what it was handed, or
nothing:

- `--diff main...HEAD` (the dogfood Claude entry) is **empty** until the work is committed,
  and silently **widens** when local `main` is stale. AGENTS.md spends a long preflight ritual
  (commit-first, `git status` clean, fetch + fast-forward `main`) trying to keep a human from
  handing the reviewer the wrong range. That ritual exists *because* the reviewer has no way to
  check or correct the capture itself.
- A working-tree mode is empty exactly when a PR is ready (already committed).

The Codex direction does **not** have this fragility. It has the evidence service
(`docs/codex-reviewer-evidence-service.md`, issue #47): a repository-scoped, read-only MCP
server — `cross-review --evidence-server` — that lets the reviewer list, read, search, page the
captured change, and (for Git) walk history and read revisions **live**, on demand. A Codex
reviewer handed a thin capture can reconstruct what it needs; a Claude reviewer cannot.

Root cause of the asymmetry, verified in the tree: the evidence service is delivered as an MCP
server, and `--safe-mode` — the Claude reviewer's isolation flag — disables MCP. So the parent
builds an `EvidenceInvocation` and hands it only to Codex; the Claude adapter takes it as
`_evidence` and **discards it** (`src/reviewer/claude.rs:81`). `src/reviewer/mod.rs:246-247`
says so outright: "injected only into a Codex invocation. Claude receives its captured change
in the prompt and never starts this server."

## Decision

Give the Claude reviewer the **same evidence service the Codex reviewer already has**, by
attaching `cross-review --evidence-server` to the Claude spawn as its single trusted MCP
server. This is not a new service — it is a second consumer of the one built for Codex, reused
unchanged.

Doing so requires replacing `--safe-mode` on the isolated Claude path, because both single-flag
options fail — and one of the two failures is already documented in the code
(`claude.rs:141-142`):

- **`--safe-mode`** isolates correctly but disables MCP, so the evidence server cannot attach.
- **`--bare`** allows MCP but, per the CLI's own help, "Anthropic auth is strictly
  `ANTHROPIC_API_KEY` or apiKeyHelper via `--settings` (OAuth and keychain are never read)."
  The profile mechanism authenticates by an OAuth `.credentials.json`, so `--bare` breaks the
  account-pinned billing the whole profile system exists to guarantee.

Both were re-verified empirically during planning (claude 2.1.233), not taken from docs:

| Run | Result |
| --- | --- |
| `claude -p` on an OAuth home (control) | exit 0, `OK` |
| `claude -p --bare` on the same OAuth home | exit 1, **`Not logged in`** |
| project `UserPromptSubmit` hook, plain `-p` (control) | hook **fires** |
| `claude -p --settings '{"disableAllHooks":true}'` | exit 0, `OK`, hook **suppressed** |
| `claude -p --settings '{"disableAllHooks":true}' --strict-mcp-config --mcp-config FILE` | exit 0, `OK`, hook **suppressed** |

So the **granular** path is the only combination that yields {OAuth auth} + {one trusted MCP
server} + {isolation}, and it is verified end-to-end above. Replace `--safe-mode` with:

- `--settings '{"disableAllHooks":true,"autoMemoryEnabled":false}'` — disables all hooks
  (including plugin-provided ones) and auto-memory (the §8/f6 preferred path — key confirmed);
- `--disable-slash-commands` — disables all skills and custom commands;
- keep `--strict-mcp-config` (already passed today) — only `--mcp-config` servers load, no
  ambient/plugin/project MCP;

and add `--mcp-config <generated evidence config>` when an `EvidenceInvocation` is present.

## What boundary moves — the part that gets the rigor

Per AGENTS.md "How much rigor, and where": account identity, the read/write boundary, and the
isolation posture are exactly where being wrong is expensive, so this section is deliberate.
The evidence-delivery mechanics below are not, and are kept minimal.

Dropping `--safe-mode` sounds like relaxing isolation. Stated precisely, here is everything
that moves, and why the move is safe:

1. **The three execution vectors stay closed, each explicitly.** `--safe-mode` was one switch;
   we replace it with three targeted ones that each disable a way a reviewed repository could
   get code to run:
   - hooks (the shell-exec vector documented at `claude.rs:132-137`) → `disableAllHooks`;
   - skills / custom commands → `--disable-slash-commands`;
   - MCP servers → `--strict-mcp-config` (only our evidence server).
2. **Plugins and auto-memory have no granular disable flag** — only `--safe-mode`/`--bare` turn
   them off, and `--bare` is out. This is the residual delta, and it is small: a plugin's only
   *dangerous* surfaces (hooks, commands, MCP servers) are each already killed by the three
   flags above, so a loaded plugin is inert; and in the reviewer's actual runtime — a dedicated
   `CLAUDE_CONFIG_DIR` profile home (`claude.rs:105`) plus a neutral non-repo cwd — there are no
   user or project plugins or memory to discover in the first place. Auto-memory is a
   non-executable context surface, empty in the controlled home. **The move, stated for the
   README:** from "one flag disables all customization discovery" to "three flags disable the
   three execution vectors; plugins/auto-memory rely on the controlled home's emptiness rather
   than an explicit disable." For the execution-isolation properties that matter it is
   equivalent; the delta is non-executable surfaces backed by environment, not by flag.
   **Corrected per review f2:** that environmental backing is *not currently guaranteed*.
   `claude_neutral_target` can fall back to the repository cwd (`claude.rs:93-97`), the neutral
   dir may be non-empty (`mod.rs:313-318`), and `verified_non_git_dir` (`mod.rs:998-1014`)
   checks only Git ancestry, not emptiness. Under `--safe-mode` a repo cwd was harmless because
   config discovery was off; once `--safe-mode` is gone, a repo-or-non-empty cwd lets the
   reviewed repository's `.claude/` and `CLAUDE.md` load. The *execution* surfaces are still
   flag-disabled (hooks/commands/MCP), but `CLAUDE.md`/memory is a non-executable
   **prompt-injection** surface that no granular flag closes. So this plan now **requires** a
   verified-empty scratch cwd for every isolated Claude run (§6), reusing Codex's sterile-root
   machinery. This is the one place the granular swap adds a real obligation — and it is exactly
   where the rigor belongs.
3. **Auth fails closed, and stays OAuth.** We keep OAuth precisely by *not* using `--bare`;
   `--safe-mode`'s own help already promised auth is untouched, and the granular path preserves
   it (control run above). The one way this could go wrong — silently billing a fallback
   account — cannot happen: `--bare` was rejected *because* it breaks OAuth, and the granular
   path never introduces an `ANTHROPIC_API_KEY` fallback. An auth failure surfaces as
   `Not logged in`, not as a review under the wrong account.
4. **Reads do not widen.** Claude keeps `Read`/`Grep`/`Glob` scoped to the repo root. The
   evidence tools it gains are themselves path-confined to the bundle root (canonicalized,
   `..`/absolute/device/ADS rejected, reparse points refused — verified for the Codex consumer,
   reused unchanged). Claude gains history/revision reach it did not have, but every new surface
   is read-only and root-scoped. No write tool is added; `--disallowed-tools Edit,Write,
   NotebookEdit` remains unconditional (`claude.rs:130`), as does `--permission-mode dontAsk`.

## Goals

1. An **in-scope** Claude reviewer — the shell-less, `evidence_enabled` path of §0, which is the
   dogfood direction — can list, read, search, page the captured change, and (for Git) walk history
   and read revisions through the evidence service, without a shell and without losing OAuth/profile
   auth. (A shell-enabled / non-qualifying isolated Claude is out of scope and unchanged.)
2. A thin, empty, or stale in-prompt capture is **mitigated, not eliminated** (corrected per
   review f1). The evidence tools let the reviewer read the **live working tree**
   (`repository_read` / `repository_list` / `repository_search`) and walk **history**
   (`repository_history`, then `repository_revision` per commit) to gain context, verify claims,
   and catch what a thin capture omitted — so it is no longer blind to everything outside its
   prompt. It does **not** let the reviewer reconstruct an exact selected diff the parent failed
   to capture: the service has no live ref/range-diff operation (`repository_revision` takes a
   full object id and runs `git show` — `src/evidence/core.rs:1021-1026`,
   `src/evidence/git.rs:178-199`), and adding one is out of scope — it would break the
   reuse-unchanged property, and the model is that the reviewer reviews *the parent's captured
   change*, not a range it picks itself. The authoritative selected change stays
   `repository_change` / the in-prompt capture; a wrong capture is fixed by the `--diff` config,
   not by reviewer reconstruction.
3. The `evidence_enabled` Claude review keeps its isolation posture (§"What boundary moves"),
   fail-closed auth, no-write boundary, and stdout-as-protocol-only.
4. The evidence server is reused **unchanged**. No new evidence tools, no schema bump, no
   per-reviewer tool-set customization.

## Non-goals

- Do not build a Perforce history/revision backend. The evidence service is Git-only for
  history/revision today (`repository_history`/`repository_revision` return `unsupported` for
  Perforce; search/scope degrade to a filesystem walk). The Claude direction inherits exactly
  that subset — **the same subset the Codex direction already has** — so no Perforce work is
  required to reach parity. A p4 backend is a separate, independently-motivated plan that would
  benefit Codex equally; it must not be bundled here.
- Do not remove the in-prompt capture from the Claude direction. Evidence tools are added as the
  self-serve augmentation; the captured change continues to reach the reviewer in its prompt
  exactly as today. De-duplicating (evidence-first, dropping the in-prompt paste, as Codex did)
  is an optional later simplification, out of scope to keep this diff minimal.
- Do not overhaul the `--allow-reviewer-config` / `--allow-reviewer-mcp` flag semantics.
  Evidence injection happens only on the `evidence_enabled` path (shell-less, §0); opting out of
  isolation remains the caller's own responsibility, as today.
- Do not add migration or compatibility machinery for the capability bundle. It is per-turn and
  deleted at turn end; a schema mismatch fails the turn and the review re-runs.

## Concrete changes

### 0. Scope: one predicate gates the entire new treatment (Phase 1 re-gate, option a, review f7)

Implementation surfaced an interaction the earlier plan left open: the existing
`claude_neutral_target` (`mod.rs:927-977`) deliberately moves **only shell-less** isolated Claude
out of the repo, and keeps a **shell-enabled** Claude on the repo cwd (with `--safe-mode` and its
working shell) so it can self-serve via git — see the rationale at `mod.rs:947-954`. The
cross-review gate chose **option (a)**: the evidence service is given to Claude *exactly when the
centralized `claude_evidence_enabled(cfg, spec)` predicate holds* — a **profile-pinned** (impl f2,
below), shell-less, git, default-rules Claude whose cwd is the git toplevel (the
`claude_neutral_target` conditions **plus** a pinned profile). Shell-enabled, ambient, or otherwise
non-qualifying Claude is **unchanged**: it keeps `--safe-mode`, the repo cwd (or neutral cwd), and
its shell. It gets no evidence, so no missing-evidence false-approval risk arises for it.

**The evidence path also requires a pinned profile home (impl review f2).** `claude_neutral_target`
qualifies an *ambient* Claude (no `--claude-profile`) too, but for ambient runs `CLAUDE_CONFIG_DIR`
is never set, so a granular-flag Claude would load the user's `~/.claude` settings/plugins/`CLAUDE.md`
— which `--safe-mode` blocked and the sterile cwd (project-only) does not. Ambient Claude can't use a
controlled config home (its credentials live in `~/.claude`), so evidence is gated on
`authorized_start.is_some()`: ambient Claude keeps `--safe-mode` + the neutral cwd and gets no
evidence; only a profile-pinned Claude (the dogfood, `--claude-profile work`) takes the evidence
path. The `smoke.ps1 -Reviewer claude` run pins the `work` profile for this reason.

**f7 is the load-bearing discipline: that single predicate must gate *every* new hook together** —
evidence setup, the sterile cwd, the granular-flag swap, the evidence preamble, the runtime
evidence gate (§7), and the capture handling. If any one of them applied on a *broader* condition
(e.g. dropping `--safe-mode` for all isolated Claude while giving the sterile cwd only to the
shell-less subset), a shell-enabled Claude would get partial treatment and re-open the f2
config-contamination hole. So the code defines one eligibility value and threads it through all of
them; there is no second, looser condition.

Consequence: the earlier §7 `supplies_change_of` change is **dropped** — a shell-less isolated
Claude already captures under `--diff auto` (`!reviewer_has_shell_of` is already true), so the
capture is guaranteed for the in-scope path with no config change. The revert is in this branch.

### 1. `src/reviewer/claude.rs` — argv

Compute the §0 predicate once — call it `evidence_enabled` (= the `evidence` argument being
`Some`, which the parent sets exactly when `claude_evidence_enabled(cfg, spec)` holds — a pinned
profile plus the `claude_neutral_target` conditions, which already imply `isolate_reviewer`). **The flag swap keys on `evidence_enabled`, NOT on a
bare `if cfg.isolate_reviewer` — that broader condition is the f7 trap** (it would strip
`--safe-mode` from a shell-enabled Claude that stays on the repo cwd). Three branches:

- **`evidence_enabled` (in scope, shell-less):** replace `--safe-mode` with the granular flags and
  attach the allow-listed evidence server (and run from the sterile cwd, §2/§6):
  ```rust
  cmd.args(["--settings", "{\"disableAllHooks\":true,\"autoMemoryEnabled\":false}"]);
  cmd.arg("--disable-slash-commands");
  cmd.arg("--strict-mcp-config");
  cmd.args(["--mcp-config", evidence_config_path]);
  // Phase 0 finding: under --permission-mode dontAsk the evidence MCP tools are DENIED
  // unless allow-listed. The server-level entry allows all seven repository_* tools;
  // it goes in --allowed-tools alongside the scoped Read/Grep/Glob rules.
  cmd.arg("--allowed-tools");
  cmd.arg("mcp__cross_review_evidence");
  ```
- **`cfg.isolate_reviewer && !evidence_enabled` (shell-enabled / non-qualifying):** keep today's
  `--safe-mode` + `--strict-mcp-config` exactly as now — no granular flags, no evidence, repo cwd.
- **`!cfg.isolate_reviewer`:** unchanged.

Without that entry the reviewer connects to the server, discovers the tools, and then has
every call refused with "denied because Claude Code is running in don't ask mode" — a review
that looks wired but cannot read anything. Verified in Phase 0: with the entry, `repository_scope`
and `repository_change` both returned real data; without it, both were denied.

The prompt is delivered on **stdin**, not as a positional arg (confirmed: no `cmd.arg(prompt)`
in `invocation`), so the variadic `--mcp-config` cannot swallow it. (A planning run *did* hit
that swallow when a prompt trailed `--mcp-config` on the command line — noted so the
implementation keeps the prompt off the argv, which it already does.)

Update the existing comment block to state the new posture and cite the empirical results,
rather than the `--safe-mode` rationale it documents today.

### 2. Parent-side: build and pass the `EvidenceInvocation` for Claude

Today the bundle is captured and the `EvidenceInvocation` constructed only for the Codex path
(`src/tools.rs:~3305`; `mod.rs:246-247`, gated at `tools.rs:3190`). Extend that gate to also fire
for the **in-scope Claude path** — i.e. when `claude_evidence_enabled(cfg, spec)` holds (the §0
predicate: a pinned profile plus the `claude_neutral_target` conditions), so a shell-enabled or
ambient Claude is excluded — reusing the identical capture→bundle machinery.

**`EvidenceInvocation.sterile_dir` IS set for in-scope Claude (corrected per f8), and is what wires
the verified-empty cwd into the child.** It carries the `codex_sterile_dir` path (§6), and Claude's
`invocation` uses it as `current_dir` in place of today's `neutral_dir(cfg)` — that swap is exactly
how the verified-empty directory reaches the child process and replaces the possibly-non-empty
neutral dir that reopened f2. The owning `SterileDir` lives in the `evidence_setup` tuple for the
whole turn (as Codex's already does), so the directory stays alive through the child and is dropped
when the turn ends. So `executable`, `bundle_file`, `nonce`, and `sterile_dir` are all used for
Claude; the read-scope rules still come from `claude_neutral_target` (absolute, pinned to the repo
root) so Read/Grep/Glob keep reaching the repo from the sterile cwd.

**Do not trip the in-prompt capture suppression (review f3).** The parent currently sets
`prompt_change = None` whenever `evidence_setup.is_some()` (`src/tools.rs:3273-3279`), because
for isolated Codex the selected change is delivered *only* through `repository_change`. Claude
must keep its in-prompt capture (Non-goals), so this suppression must be gated on the Codex
reviewer (or the Claude setup path must leave `prompt_change` populated). Keeping the capture is
also what makes the evidence server *additive rather than load-bearing* for Claude — the
property §7 relies on to right-size the runtime-availability handling.

### 3. Generate the Claude `--mcp-config` file

Codex receives the evidence entry via `-c mcp_servers.<name>.*` TOML overrides. Claude's
equivalent is a JSON file passed to `--mcp-config`. Generate it per turn into the existing
state/temp area, alongside the bundle, and delete it with the other turn temporaries:

```json
{ "mcpServers": { "cross_review_evidence": {
    "command": "<absolute cross-review.exe>",
    "args": ["--evidence-server", "<bundle path>", "<nonce>"] } } }
```

`--strict-mcp-config` guarantees this is the only server that loads. The evidence child is
spawned by Claude Code (not the parent); the service already watches stdio EOF / parent death
and self-terminates, so it is reaped when `claude` exits — Phase 0 confirms this for the Claude
client specifically.

### 4. Prompt / preamble

**On the `evidence_enabled` path only** (§0 — the shell-enabled / non-qualifying path keeps its
current preamble and gets no evidence guidance): tell the reviewer the evidence tools exist and are
the way to look **past** the captured change — read the live working tree, search, and walk history
— and that the captured change is also in the prompt and remains the authoritative selected change
(f1). One safety line (§7, f4): if the reviewer has **neither** a non-empty captured change **nor**
working evidence tools, the review is inconclusive and must not be approved. Keep this short; the
tool descriptions carry the detail, as they do for Codex.

### 5. Documentation

- `README.md` reviewer matrix (`README.md:762-767`) and the "Claude reviewer has no shell"
  section (`README.md:844`): Claude now has a **read-only, path-scoped evidence service** (still
  no shell, still no write tool). State the isolation change (`--safe-mode` → the three granular
  flags) and exactly what boundary moves (§"What boundary moves"), keeping the "verified vs
  assumed" discipline.
- `AGENTS.md`: the Claude direction is no longer strictly capture-bound — a reviewer can read
  the live tree and history for context beyond its in-prompt capture. This does **not** retire
  the commit-first / fetch-`main` preflight, and (per f1) it does **not** let the reviewer
  reconstruct an exact selected diff the capture got wrong — the capture is still authoritative
  and a wrong one is still fixed by `--diff`. What it changes is that the reviewer is no longer
  blind to everything the capture omitted. Note that `smoke.ps1 -Reviewer
  claude` now exercises the evidence service (see §Verification), which AGENTS.md currently says
  it does not.

### 6. Require a verified-empty scratch cwd for the isolated Claude run (review f2)

Dropping `--safe-mode` removes `CLAUDE.md` / project-config auto-discovery suppression, and no
granular flag replaces it. So the isolated Claude path must run from a **verified-empty,
non-repository** directory — the same guarantee Codex's sterile root already gives — instead of
the current possibly-repo, possibly-non-empty cwd:

- **Reuse `codex_sterile_dir` (`mod.rs:1138`) — it *is* the verified-empty helper.** Phase 1
  confirmed it already creates a directory verified empty (it refuses any existing entry), non-Git,
  and outside the repo, keyed on `(state_dir, session)` and stable across a session's turns. Its
  emptiness check is **separate** from the shared `verified_non_git_dir`, so f5's concern (do not
  make the shared helper require emptiness) is already satisfied — no new helper is needed. Empty
  subsumes "no `.claude` layer / no `CLAUDE.md`": an empty dir has neither. (Rename it from
  `codex_`-prefixed to a reviewer-neutral name when it gains a second caller.)
- In-scope (per §0) means the reviewer already runs from a neutral cwd today via
  `claude_neutral_target`; this swaps that neutral dir for the verified-empty sterile dir. The
  out-of-scope shell-enabled path keeps the repo cwd and `--safe-mode` unchanged, so the
  "never fall back to repo cwd" concern does not arise — there is no in-scope run without a sterile
  cwd (the same `state_dir`/`session` that names the sterile dir is always present).
- Test adversarially: for the in-scope shell-less path, a reviewed repo carrying
  `.claude/settings.json` (hook), a plugin, and a `CLAUDE.md` must influence the review in **no**
  way.

### 7. Runtime availability — capture already guaranteed in scope, plus a scoped fail-closed gate (review f4)

Turns 2–3 correctly refuted a "purely additive, never load-bearing" argument in the general case,
because `supplies_change_of` (`config.rs:1704-1718`) **omits** the capture for a *shelled* Claude
under `--diff auto`. Option (a) (§0) resolves that not by changing `supplies_change_of` but by
**scope**: the evidence service is given only to **shell-less** isolated Claude, for which
`!reviewer_has_shell_of(reviewer)` is already true, so `supplies_change_of` **already returns
true** and the change is captured with no code change. Combined with f3 (the in-prompt capture is
retained for Claude, not nulled), **every in-scope Claude review with a configured change already
has the authoritative captured change in its prompt.** The evidence server is therefore additive
*by construction*: missing at startup or dying mid-turn, Claude still reviews the full captured
change — today's baseline — never thinner. (The shelled Claude that had no capture is out of scope
entirely and keeps its shell + `--safe-mode`.) `--diff off` is the one explicit no-change config,
reported as such.

That leaves one residual, which turn 3 pinned precisely and which **is** a real silent
false-approval path: if the captured change is **empty or incomplete** (a truncated capture, or
a legitimately empty range like uncommitted `main...HEAD`) **and** the evidence server dies after
startup but before Claude calls any tool, Claude can approve a thin view without ever triggering
a visible tool error. The preamble is model guidance, not an enforced gate; startup connect only
proves availability at startup, not through the turn. Three turns of the reviewer holding this
open is the signal to stop arguing and **enforce** it — this is exactly the silent-thinning →
false-approval case AGENTS.md reserves fail-closed rigor for.

The fix stays proportionate by being **scoped to exactly the load-bearing case**, which is the
reviewer's own scoping:

- **Non-empty, complete capture → no gate.** Evidence is additive by construction (above); the
  common review pays nothing.
- **Empty or incomplete capture → enforced evidence gate.** Turn 4 correctly showed a bare "≥1
  successful call" is too weak — a call can succeed and the server then die, and a
  `repository_scope`-only call proves no change evidence was obtained. So, scoped to this sliver,
  the parent requires **all** of:
  1. **Force `stream-json`** for the turn (today `claude.rs:114-116` uses buffered `json` unless
     the unrelated usage gate is armed), so per-call MCP events — success *and* error — are
     observable at all.
  2. **A pre-review no-model handshake** proved the evidence server initialised — covers the
     dead-before-any-call path even when Claude makes no call. **Phase 0 makes this mandatory, not
     a nice-to-have:** Claude Code does **not** fail closed on a broken evidence server — pointed
     at a nonexistent command, `claude -p` proceeded to a normal verdict without it (exit 0). So
     there is no CLI-level startup gate to lean on; the parent-side handshake is the *only* thing
     that catches an evidence server that cannot start. It is not a replacement for (3).
  3. **No evidence tool call ended in a transport error / disconnect** during the turn — covers
     success-then-death and mid-call death.
  4. **At least one *content* evidence call succeeded** (`repository_read`/`list`/`search`/
     `change`/`history`/`revision`, not `repository_scope` alone) — proving the reviewer actually
     obtained change/tree evidence, which for an empty range means it went and read the
     uncommitted work itself.
  If the capture is empty/incomplete and any of (2)–(4) fails, return **`EVIDENCE_UNAVAILABLE`**
  instead of a verdict — a lost review (re-run), not a false approval. Non-empty complete captures
  skip the gate entirely. The parent already knows at capture time whether the change is
  empty/incomplete (`CaptureSummary::complete` / the `captured` signal), so the gate fires only on
  that sliver.
- **Preamble (§4)** stays as defense-in-depth, not the enforcement.

This is the one place the loop moved me from "no runtime machinery" to "scoped runtime
machinery," and correctly: it is a genuine silent false-approval path, which is the case AGENTS.md
reserves fail-closed rigor for. The scoping — the gate fires only when the capture is
empty/incomplete — is what keeps it proportionate, and is what the earlier pushback bought.

### 8. Config-home isolation — no auto-memory persisting across reviews (review f6)

The sterile cwd (§6) closes the reviewed-repository config surface, but not the profile's own
`CLAUDE_CONFIG_DIR` (`claude.rs:105`). `--safe-mode` disabled auto-memory; dropping it re-enables
Claude Code's auto-memory, which is written into the config home and loaded on later sessions.
The profile home **persists across reviews** because it holds the OAuth credential, so content
generated during one review — possibly influenced by the untrusted reviewed repository — could be
loaded into a later review. That is a cross-review prompt-injection persistence surface, and a
recurrence of f2's isolation concern in a different directory. It must be closed — but the amount
of machinery is contingent on Phase 0 facts, and most of it is likely never built. Phase 0
answers three questions in order, and each "no" collapses the rest:

- **Q1 — does `claude -p` under the granular flags write auto-memory to the config home at all?**
  Headless `-p` may not (auto-memory is largely an interactive feature). If it does **not** write,
  f6 is closed by a Phase 0 assertion plus a regression check that the config home stays clean
  after a review — no disable, no scrub, no lock. This is the likely, cheapest outcome, and the
  counterweight: do not build the scrub/lock machinery below speculatively.
- **Q2 (only if Q1 writes) — disable auto-memory via the settings key, now known to exist:**
  `--settings '{"disableAllHooks":true,"autoMemoryEnabled":false}'` — `autoMemoryEnabled: false`
  folds into the same blob that already carries `disableAllHooks`. Nothing is written, so no scrub,
  no lock, no crash window, no concurrency window. This is the preferred path and, because the key
  is confirmed, the **expected** one — so Q3 below is a documented contingency that should not be
  reached. Phase 0 still (a) confirms the installed CLI accepts the key and that it actually
  suppresses auto-memory writes in headless mode, and (b) checks the one residual the reviewer
  named — a `CLAUDE.md` in the config home. That residual is already closed in practice: the
  reviewed-repo `CLAUDE.md` is out of reach via the sterile cwd (§6), and the controlled profile
  home is provisioned by cross-review with only the credential — it has no `CLAUDE.md` and a review
  writes none once auto-memory is off. Phase 0 verifies the home stays `CLAUDE.md`-free.
- **Q3 (only if it writes AND no key) — the crash-safe *and* concurrency-safe scrub fallback.**
  Two properties, because turns 4–5 found the scrub needs both:
  - *Crash-safe:* the scrub runs **pre-launch** (verify clean, scrub if not, **refuse** if it
    cannot be made clean), so a review always starts clean regardless of how the previous one
    ended — post-review cleanup alone leaks on crash/kill/cancel. It also **explicitly verifies no
    `CLAUDE.md` layer is loaded** (dropping `--safe-mode` makes more `CLAUDE.md` sources eligible
    than auto-memory alone). `.credentials.json` is never touched.
  - *Concurrency-safe (f6, turn 5):* the per-home review lock is currently taken on the **shared**
    side so concurrent same-profile reviews coexist (`tools.rs:3095-3124`). A shared-home scrub
    therefore races — one review's scrub can clear another's active memory, or two can interleave
    writes. So the scrub path must take the **exclusive** side of that same lock for the review's
    duration, serializing same-profile reviews. This reuses the existing lock; the throughput cost
    lands only in this fallback and only on same-profile concurrency, which is uncommon and already
    slow. (If that serialization is ever unacceptable, the alternative is a per-review disposable
    config home with copied credentials — heavier and inside the credential-handling blast radius,
    so only if genuinely needed.) A concurrent-start test proves two same-profile fallback reviews
    serialize rather than race.

Do not ship the granular swap until Q1/Q2 are answered, and — only if Q3 is reached — the
crash-safe, concurrency-safe scrub is in place.

## Phase 0: compatibility proof (before wiring anything permanent)

The one load-bearing unknown is that **Claude Code's `-p` MCP client** — a different client
from Codex's — actually spawns `cross-review --evidence-server`, completes `initialize` +
`tools/list`, and calls a `repository_*` tool successfully under `--strict-mcp-config
--mcp-config`, with `--permission-mode dontAsk` not blocking the call. Prove it with a real
low-cost `claude -p` run against a generated bundle before making the wiring permanent. Phase 0
must also settle what the findings turned up:

- **Evidence-event observability (f4).** Under forced `stream-json`, confirm the parent can read,
  per evidence call, both **success** (with the tool name, to distinguish a content call from
  `repository_scope`) and **transport error / disconnect** — the four inputs §7's gate needs.
  Confirm a *dead* server surfaces as a visible tool-call error, not a silent empty result
  (silent empties would mean even a non-empty capture's augmentation could mislead, and would
  widen the gate).
- **Fail-closed startup (f4).** Tested: a deliberately-broken evidence command does **not** make
  `claude -p` fail — it proceeds without the server. Hence §7's mandatory parent-side handshake.
- **Sterile-cwd isolation (f2).** With the granular flags and a reviewed repo carrying
  `.claude/settings.json` (hook), a plugin, and a `CLAUDE.md`, confirm none influence a run made
  from the verified-empty cwd, and that the run refuses rather than falling back to the repo cwd
  when no sterile cwd is available.
- **Config-home auto-memory (f6).** §8's Q1→Q2→Q3: does `claude -p` under the granular flags write
  auto-memory to a scratch `CLAUDE_CONFIG_DIR` at all (if not, f6 is closed by assertion); if so,
  confirm `--settings '{"autoMemoryEnabled":false}'` is accepted by the installed CLI and
  suppresses the write, and that the home stays `CLAUDE.md`-free — the expected Q2 path. Q3's
  scrub/lock fallback is reached only if that key does not suppress the write.
- **Child reaping.** Confirm the evidence child is terminated when that `claude` exits.

If any fails, stop and amend this plan; do not ship a Claude evidence path that a billed review
could find missing, contaminated, or silently thinned.

This is the whole of the up-front rigor beyond §"What boundary moves": the auth/isolation
boundary (now including the sterile cwd, f2), and this one protocol proof. Everything else is
reuse.

### Phase 0 results (run against claude 2.1.233 + `target/debug/cross-review.exe`, evidence schema 2)

Executed with a real emitted evidence bundle and live `claude -p` runs. All gates cleared; two
amendments folded back above.

- **MCP client connects — PASS.** Claude's `-p` MCP client spawned `cross-review --evidence-server`,
  completed `initialize`/`tools/list`, and exposed all seven `repository_*` tools.
- **Tools callable + allow-list — PASS, with an amendment.** Under `--permission-mode dontAsk` the
  tools were **denied** until allow-listed. Adding server-level `--allowed-tools
  mcp__cross_review_evidence` let `repository_scope` and `repository_change` return real data
  (the change page carried the bundle's marker line). Folded into §1.
- **Evidence-event observability — PASS.** `stream-json` emits per-call `tool_use` (with the tool
  name) and `tool_result` (with `is_error`), so §7's gate can tell a content call from
  `repository_scope` and a success from a denial/error. (Exact *dead-server* transport-error event
  shape to be pinned during implementation via the smoke.)
- **Fail-closed startup — FINDING (amended §7).** Claude does **not** fail closed: pointed at a
  nonexistent evidence command, `claude -p` proceeded to a normal verdict (exit 0) without the
  server. So the parent-side handshake in §7's gate is mandatory, not a fallback — corrected there.
- **Child reaping — PASS.** No orphaned evidence-server process after `claude` exited.
- **Config-home auto-memory (f6) — PASS via Q2.** Headless `-p` **does** write auto-memory
  (`projects/<slug>/memory/…` appeared); `--settings '{"autoMemoryEnabled":false}'` **suppressed**
  it (nothing written). Q1=writes, Q2=disabled → the scrub/lock fallback (Q3) is not needed.
- **`CLAUDE.md` from cwd (f2) — CONFIRMED.** A project `CLAUDE.md` loads under the granular flags
  (it altered the reply), confirming the sterile empty cwd — not the flags — is the isolation
  mechanism. Full adversarial isolation is proved once §6's sterile cwd is wired.

## Verification

- `cargo test` — add argv tests (mirroring `argv_tests.rs:280-298`): an isolated Claude
  invocation contains `--settings {"disableAllHooks":true,"autoMemoryEnabled":false}`,
  `--disable-slash-commands`,
  `--strict-mcp-config`, `--mcp-config <file>`, and `--allowed-tools mcp__cross_review_evidence`
  (the Phase 0 allow-list entry), and does **not** contain `--safe-mode`;
  `--disallowed-tools` and `--permission-mode dontAsk` remain present; `--allow-reviewer-config`
  drops the three isolation flags (as it drops `--safe-mode` today). One test that the Claude
  path constructs and passes an `EvidenceInvocation` rather than discarding it, and that the
  generated `--mcp-config` JSON names the `--evidence-server` command with the turn's bundle and
  nonce. No new evidence-core tests — that layer is reused, and its tests already exist.
- **f3 test:** an isolated Claude review keeps `prompt_change` populated (the in-prompt capture
  is preserved) even though evidence setup is present — the Codex-only suppression at
  `tools.rs:3273-3279` does not fire for Claude.
- **f7 scoping test (the load-bearing one):** the single `claude_evidence_enabled(cfg, spec)`
  predicate gates the whole treatment together. A **shell-less** qualifying isolated Claude gets
  the granular flags, `--mcp-config`, the allow-list, the sterile cwd, and the `EvidenceInvocation`.
  A **shell-enabled** isolated Claude gets **none** of them — it still contains `--safe-mode`, runs
  from the repo cwd, and has no evidence server (no partial treatment). `--allow-reviewer-config`
  drops isolation entirely as today.
- **f2 / f5 test:** the in-scope Claude cwd is `codex_sterile_dir`'s verified-empty non-repo dir
  (its existing emptiness/non-git tests already cover it); `verified_non_git_dir` is left unchanged
  and its non-empty callers still pass. The adversarial project-config end-to-end case (reviewed
  repo carrying a hook/plugin/`CLAUDE.md`) is proved in Phase 0 / smoke, since it needs a real
  `claude` run.
- **f4 test (scoped gate):** feeding synthesized turn observations to the gate, assert — for an
  empty/incomplete capture — `EVIDENCE_UNAVAILABLE` when the handshake failed, when any evidence
  call hit a transport error/disconnect, or when only `repository_scope` succeeded (no content
  call); and a normal verdict when the handshake passed, no transport error occurred, and ≥1
  content call succeeded. For a non-empty complete capture, the gate never fires regardless of
  evidence activity. (Parsing real stream events is proved against a live `claude` stream in
  Phase 0 / smoke.)
- **f6 test (only if Q3 is reached):** the **pre-launch** scrub verifies-or-refuses — a config
  home seeded with a stray auto-memory / `CLAUDE.md` file is either scrubbed clean or the review
  refuses; a home that cannot be made clean refuses; `.credentials.json` is never removed. A
  **concurrent-start test** asserts two same-profile fallback reviews serialize on the exclusive
  lock rather than racing (one waits for the other). The crash-safety case (memory left by a
  killed prior run is cleared before the next launch) and cross-review non-persistence are proved
  in smoke. If Q1 shows headless writes no auto-memory, this collapses to a regression assertion
  that the config home is clean after a review.
- `build.ps1` — fmt, clippy `-D warnings`, tests, release build, restage.
- `smoke.ps1 -Reviewer claude` — extend it so the Claude direction now **asserts** an evidence
  round trip (an evidence tool call completed), which it does not today. This is the real
  protocol proof and costs model tokens; mention the cost to the user before running.
- `smoke.ps1 -Reviewer codex` — regression: the shared bundle/`EvidenceInvocation` construction
  must not change Codex behavior. Costs tokens.
- Confirm the Codex direction's evidence path is byte-for-byte unchanged (only the Claude branch
  and the shared construction gain a consumer).

## What this deliberately does NOT do

- No Perforce history/revision backend (Non-goals). Claude inherits the Codex subset as-is.
- No removal of the in-prompt capture; no evidence-first re-plumbing of the Claude prompt.
- No new evidence tools, no schema bump, no per-reviewer tool-set customization, no bundle
  migration machinery.
- No `--allow-reviewer-*` flag-semantics overhaul.

Each of these is a place the review loop could push toward more machinery. Per AGENTS.md, the
Claude review's worst case is a lost review that is re-run — cheap — so the counterweight
argument is explicit: added machinery here must be justified against that failure mode, not
against a hypothetical. A finding that asks for p4 history, bundle migration, per-reviewer tool
sets, or removing the working in-prompt capture will be disputed on proportionality unless it
identifies a false-approval (not lost-review) risk.

## Critical files

- `src/reviewer/claude.rs` — argv: `--safe-mode` → three granular flags + `--mcp-config`; stop
  discarding `_evidence` (`:81`); comment block (`:131-147`); **force `stream-json` when the §7
  gate is armed** (`:114-116` today only does so for the usage gate) and parse evidence tool-call
  results — both **success and transport error** — from the stream (`:157-183` today parses only
  the final result), feeding the gate's health check (f4); auto-memory disable key in the
  `--settings` blob, or **pre-launch** scrub-and-verify of `CLAUDE_CONFIG_DIR` (`:105`) per §8 (f6).
- **The single eligibility predicate** `claude_evidence_enabled(cfg, spec)` (§0, f7; profile + the
  `claude_neutral_target` conditions)
  gates every hook below; there is no second, looser condition. Shell-enabled / non-qualifying
  isolated Claude keeps `--safe-mode`, the repo cwd, its shell, and gets no evidence.
- `src/reviewer/mod.rs` / `src/tools.rs` — construct + pass `EvidenceInvocation` for the **in-scope
  shell-less** Claude path (`mod.rs:246-247`; `tools.rs:~3305`, gated on the predicate at
  `tools.rs:3190`); generate + clean the per-turn `--mcp-config` JSON; **gate the in-prompt capture
  suppression on Codex** (`tools.rs:3273-3279`, f3); **verified-empty sterile cwd** — reuse
  `codex_sterile_dir` (`mod.rs:1138`; rename reviewer-neutral), which is already the verified-empty
  helper, leaving shared `verified_non_git_dir` untouched (f5); **the §7 scoped gate** — when the
  capture is empty/incomplete and the evidence service was not healthy, return `EVIDENCE_UNAVAILABLE`
  instead of a verdict (f4).
- `src/config.rs` — **no change to `supplies_change_of`** (reverted): a shell-less isolated Claude
  already captures under `--diff auto`, so the in-scope capture is guaranteed with no code change
  (§0/§7, f7).
- `src/setup.rs` / `src/tools.rs` — **only if §8 Q3 is reached:** the scrub-fallback path takes the
  **exclusive** side of the per-home review lock (`tools.rs:3095-3124`, `acquire_review_home_lock`),
  instead of today's shared side, so concurrent same-profile reviews serialize (f6, turn 5).
- `src/evidence.rs`, `src/evidence/*` — **unchanged** (reused).
- `README.md`, `AGENTS.md` — reviewer matrix, no-shell section, isolation-boundary statement,
  smoke note.
- `src/reviewer/argv_tests.rs` — new isolation-argv + evidence-injection assertions.
