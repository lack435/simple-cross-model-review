# Claude reviewer evidence service

Status: draft plan, on `plan/claude-evidence-service`. Not yet implemented.

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

1. An isolated Claude reviewer can list, read, search, page the captured change, and (for Git)
   walk history and read revisions through the evidence service, without a shell and without
   losing OAuth/profile auth.
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
3. The default Claude review keeps its isolation posture (§"What boundary moves"),
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
  Evidence injection happens on the isolated (default) path; opting out of isolation remains the
  caller's own responsibility, as today.
- Do not add migration or compatibility machinery for the capability bundle. It is per-turn and
  deleted at turn end; a schema mismatch fails the turn and the review re-runs.

## Concrete changes

### 1. `src/reviewer/claude.rs` — argv

In the `if cfg.isolate_reviewer` block (currently `claude.rs:131-147`), replace
`cmd.arg("--safe-mode")` with:

```rust
cmd.args(["--settings", "{\"disableAllHooks\":true,\"autoMemoryEnabled\":false}"]);
cmd.arg("--disable-slash-commands");
// (keep) --strict-mcp-config
```

Then, when the `evidence` argument is `Some` (stop discarding it — it is `_evidence` today at
`claude.rs:81`), add the evidence server:

```rust
cmd.args(["--mcp-config", evidence_config_path]);
```

The prompt is delivered on **stdin**, not as a positional arg (confirmed: no `cmd.arg(prompt)`
in `invocation`), so the variadic `--mcp-config` cannot swallow it. (A planning run *did* hit
that swallow when a prompt trailed `--mcp-config` on the command line — noted so the
implementation keeps the prompt off the argv, which it already does.)

Update the existing comment block to state the new posture and cite the empirical results,
rather than the `--safe-mode` rationale it documents today.

### 2. Parent-side: build and pass the `EvidenceInvocation` for Claude

Today the bundle is captured and the `EvidenceInvocation` constructed only for the Codex path
(`src/tools.rs:~3305`; `mod.rs:246-247`). Extend that so the same capability bundle is written
and an `EvidenceInvocation` is constructed for an **isolated Claude** review as well, reusing
the identical capture→bundle machinery. `EvidenceInvocation.sterile_dir` is Codex-specific (its
empty working root) and is `None` for Claude — only `executable`, `bundle_file`, and `nonce` are used to render the config
in §3. (The sterile *cwd* Claude now needs, per f2/§6, is a separate concern from this Codex
`sterile_dir` field, which is the Codex evidence server's own working root.)

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

Tell the reviewer the evidence tools exist and are the way to look **past** the captured change
— read the live working tree, search, and walk history — and that the captured change is also
in the prompt and remains the authoritative selected change (f1). One safety line (§7, f4): if
the reviewer has **neither** a non-empty captured change **nor** working evidence tools, the
review is inconclusive and must not be approved. Keep this short; the tool descriptions carry
the detail, as they do for Codex.

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

- Reuse the existing `SterileDir` machinery rather than inventing a second mechanism.
- **Add a separate `verified_empty_sterile_dir` check (review f5); do NOT change
  `verified_non_git_dir`.** That helper (`mod.rs:998-1014`) only asserts non-Git ancestry and is
  shared with callers that operate on deliberately non-empty directories — the system temp root
  and evidence-bundle dirs (`mod.rs:1174-1218`, `evidence.rs:428-454`). Making it require
  emptiness globally would break Codex sterile-dir creation and bundle setup. So the new check is
  the union — non-Git (reuse `verified_non_git_dir`) **plus** empty **plus** no `.claude` layer —
  applied only to the Claude sterile cwd.
- Ensure the isolated Claude path never takes the repository-cwd fallback (`claude.rs:93-97`);
  an isolated run with no available sterile cwd must refuse, not fall back into the repo.
- Test adversarially: a reviewed repo carrying `.claude/settings.json` (hook), a plugin, and a
  `CLAUDE.md` must influence the review in **no** way.

This is scoped to the isolated path only; `--allow-reviewer-config` runs are the caller's own
choice, as today.

### 7. Runtime availability — capture guaranteed, plus a scoped fail-closed gate (review f4, turns 2–3)

Turn 2 correctly refuted my turn-1 "additive, not load-bearing" argument: it was not universal.
`supplies_change_of` (`config.rs:1704-1718`) **omits** the capture for a *shelled* Claude under
`--diff auto` — only `reviewer == Codex && isolate_reviewer` currently forces it — so an
isolated Claude with a shell enabled would have **no** in-prompt capture, and a missing evidence
server there is a genuine false-approval risk, not a lost improvement. The right response is not
runtime `EVIDENCE_UNAVAILABLE` machinery; it is to **make the premise hold by construction**:

- **Extend the capture guarantee to isolated Claude** (`config.rs:1714-1717`): under `--diff
  auto`, isolated Claude captures the change regardless of shell, exactly mirroring the existing
  Codex `isolate_reviewer` clause. After this plan every isolated Claude has evidence, so the
  clause becomes symmetric — isolated ⟹ capture — for both reviewers. Combined with f3 (the
  in-prompt capture is retained for Claude, not nulled), **every isolated Claude review with a
  configured change now always has the authoritative captured change in its prompt.** The
  evidence server is then additive *by construction*, not "usually": missing at startup or dying
  mid-turn, Claude still reviews the full captured change — today's baseline — never thinner.
  `--diff off` is the one explicit no-change config and is reported as such, as the Codex plan
  already does.

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
     dead-before-any-call path even when Claude makes no call. This is an *additional* startup
     check, not a replacement for (3).
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
- **Fail-closed startup (f4).** A deliberately-broken evidence command in `--mcp-config` must
  make `claude -p` fail, not silently succeed without the server.
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

## Verification

- `cargo test` — add argv tests (mirroring `argv_tests.rs:280-298`): an isolated Claude
  invocation contains `--settings {"disableAllHooks":true,"autoMemoryEnabled":false}`,
  `--disable-slash-commands`,
  `--strict-mcp-config`, and `--mcp-config <file>`, and does **not** contain `--safe-mode`;
  `--disallowed-tools` and `--permission-mode dontAsk` remain present; `--allow-reviewer-config`
  drops the three isolation flags (as it drops `--safe-mode` today). One test that the Claude
  path constructs and passes an `EvidenceInvocation` rather than discarding it, and that the
  generated `--mcp-config` JSON names the `--evidence-server` command with the turn's bundle and
  nonce. No new evidence-core tests — that layer is reused, and its tests already exist.
- **f3 test:** an isolated Claude review keeps `prompt_change` populated (the in-prompt capture
  is preserved) even though evidence setup is present — the Codex-only suppression at
  `tools.rs:3273-3279` does not fire for Claude.
- **f2 / f5 test:** the isolated Claude cwd is a verified-empty non-repo dir. The **new**
  `verified_empty_sterile_dir` rejects a non-empty dir and one containing a `.claude` layer,
  while `verified_non_git_dir` is left unchanged and its existing non-empty callers still pass
  (regression). The isolated path refuses rather than falling back to the repo cwd when no
  sterile cwd is available. The adversarial project-config end-to-end case (reviewed repo
  carrying a hook/plugin/`CLAUDE.md`) is proved in Phase 0 / smoke, since it needs a real
  `claude` run.
- **f4 test (capture guarantee):** `supplies_change_of` returns true for an isolated Claude under
  `--diff auto` *with a shell enabled* (the case that previously omitted the capture), so the
  in-prompt capture is present for every isolated-Claude-with-configured-change combination;
  `--diff off` still reports no change.
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
- `src/reviewer/mod.rs` / `src/tools.rs` — construct + pass `EvidenceInvocation` for the
  isolated Claude path (`mod.rs:246-247`; `tools.rs:~3305`); generate + clean the per-turn
  `--mcp-config` JSON; **gate the in-prompt capture suppression on Codex** (`tools.rs:3273-3279`,
  f3); **verified-empty sterile cwd for isolated Claude** — reuse `SterileDir`, add a new
  `verified_empty_sterile_dir` helper (leaving shared `verified_non_git_dir`, `mod.rs:998-1014`,
  unchanged — f5), and remove the repo-cwd fallback on the isolated path (`claude.rs:93-97`,
  `mod.rs:313-318`, f2); **the §7 scoped gate** — when the capture is empty/incomplete and no
  evidence call succeeded, return `EVIDENCE_UNAVAILABLE` instead of a verdict (f4).
- `src/config.rs` — extend `supplies_change_of` (`:1704-1718`) so isolated Claude captures under
  `--diff auto` regardless of shell, mirroring the Codex `isolate_reviewer` clause (f4).
- `src/setup.rs` / `src/tools.rs` — **only if §8 Q3 is reached:** the scrub-fallback path takes the
  **exclusive** side of the per-home review lock (`tools.rs:3095-3124`, `acquire_review_home_lock`),
  instead of today's shared side, so concurrent same-profile reviews serialize (f6, turn 5).
- `src/evidence.rs`, `src/evidence/*` — **unchanged** (reused).
- `README.md`, `AGENTS.md` — reviewer matrix, no-shell section, isolation-boundary statement,
  smoke note.
- `src/reviewer/argv_tests.rs` — new isolation-argv + evidence-injection assertions.
