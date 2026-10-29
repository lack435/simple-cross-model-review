# Companion repositories — v1 plan

Status: **plan** for issue #167, on `plan/companion-repositories`. Cross-review session
`plan-companion-repositories` (Codex, gpt-5.6-luna, effort=xhigh): **converged at round 5**. Four
findings raised: f1–f3 accepted and resolved; f4 accepted as real but pre-existing and out of
scope, which the reviewer confirmed before approving.

**Implementation notes** (`feat/companion-repositories`). Built as planned, with these differences:

- **Health parsing (item 5)** recognises evidence servers by name shape
  (`evidence::is_evidence_server`: `cross_review_evidence` or `cross_review_evidence_<valid name>`)
  rather than by threading this turn's set of server names into the parsers. Only servers this
  process configured can appear (Claude runs under `--strict-mcp-config`, Codex with
  `--ignore-user-config`), so the shape identifies them, and the parsers keep their signatures.
  The Codex error-attribution check (`contains(SERVER_NAME)`) already covered companions and is
  unchanged.
- **Unserved companions (items 6 and 7)** are not attestable: when any declared companion has no
  complete served change, the turn's `served` is cleared too, so the hook reports
  `no_canonical_diff` exactly as for an unserved working root. No new skip reason was needed.
- **Companion bundles** carry no in-prompt change, and their `change_label`/`status_summary` (what
  `repository_scope` reports) names the companion and says its change is served by
  `repository_diff`.

**Revision note (r1 → r2).** Round 1 raised one finding, accepted:

- **f1** (major): every server shared the review nonce, so cursor tokens (`{nonce}-{n}`) collided
  across servers and a misrouted cursor could continue the wrong repository's diff while both serve
  records read complete. Each companion server now gets its own nonce `{review_id}-c-<name>`
  (item 4). This also *removes* the r1 change to `serve_stdio`: every file name already derives from
  the nonce.

Round 1 also answered the three open questions; they are recorded under *Resolved questions*.

**Revision note (r2 → r3).** Round 2 held f1 open and raised f2; both accepted:

- **f1** (held): item 4's setup still passed `&self.id` to `Bundle::create`, contradicting the new
  nonce rule. It now uses the companion nonce for the bundle, `write_bundle`, `handshake`, the
  serve-record path and the spawn argv, and says the review id is never a companion's nonce.
- **f2** (major): r2 claimed `src/evidence/` was unchanged without saying how Claude gets every
  server, and `write_claude_mcp_config` (`src/evidence.rs`) writes exactly one. Item 5 now
  specifies one combined MCP config listing each server with its own bundle and nonce, and the
  overview distinguishes the unchanged serving side from the parent-side helper that does change.

**Revision note (r3 → r4).** Round 3 resolved f1 and f2 and raised f3, accepted:

- **f3** (major): the rule "state dir and temp dir not inside a companion" missed paths *derived
  under* the state dir. `Config::tmp_dir()` is `<state_dir>/tmp` and the reviewer writes there, so a
  companion configured at exactly that path passed the check yet received writes. Rather than
  enumerate derived paths, item 1 now requires a companion to be **disjoint** from both the state dir
  and the temp dir in both directions, which covers every derived path at once; item 4's per-turn
  recheck applies the same rule.

**Revision note (r4 → r5).** Round 4 resolved f3 and raised f4, which is **disputed as out of
scope, not as wrong**. f4 is real: a junction at `<state_dir>/tmp` can redirect the reviewer's
final-message write into a companion. But the same junction redirects it into the primary working
root today, `tmp_file` has no reparse check for any root, and planting it needs write access the
reviewer does not have. The plan makes a companion exactly as protected as the primary, so the fix
(one reparse check in `tmp_file`) is a separate pre-existing-hole PR. Recorded under *Scope cuts*.

## The problem, in one paragraph

A change can span two side-by-side git repositories: code in one, generated (often binary) output
committed to the other. A review reads exactly one repository, so the reviewer sees the output and
only a *description* of the code that produced it. The approval still converges, certifying the
complete primary diff while the code that actually changed was never served. v1 lets a review
declare an operator-configured second repository whose change is **served through the evidence
service and certified exactly like the primary's**: no approve without its complete canonical diff,
and no converged hook unless its committed change still matches what was served.

## The one design change from the issue: one evidence server per repository

The issue proposes a `repository` selector on every `repository_*` tool, with one evidence server
holding a `Core` per repository. Mapping the evidence service shows that everything in it assumes
one root per process, and every assumption would have to be reopened:

- `Core` holds one `root`, one drift cache (`observed_stamp`), one `calls`/`returned_bytes` budget
  (`src/evidence/core.rs:486-517`).
- Cursor tokens are `{nonce}-{n}` and diff op ids `{nonce}-diff-{calls}`, with no repository in
  them (`core.rs:1600`, `core.rs:1231`).
- The serve record is one file per nonce, with no repository key on any line (`core.rs:1256-1268`,
  `src/evidence.rs:599-604`).
- `require_only` key lists (`core.rs:627-659`), the eight input schemas (`evidence.rs:1158-1250`),
  the `repository_scope` output schema (`additionalProperties: false`, `evidence.rs:1307-1322`) and
  the handshake's exact-surface check (`evidence.rs:1072-1083`) all fix the tool surface.

**v1 instead starts one unchanged evidence server per declared companion**, under its own MCP
server name (`cross_review_evidence_<name>`), each with its own bundle and serve-record file. Each
companion server is today's single-root `Core`, byte for byte: same tools, same schemas, same path
confinement, LFS/autocrlf staging, drift stamp and `repository_diff` canonical definition. The
serving side does not change at all (`serve_stdio`, `Core` and everything under `src/evidence/`):
each server gets its own nonce, and every name it mints derives from that nonce (item 4). The
parent-side helpers in `src/evidence.rs` change only where they assume one server: the Claude MCP
config writer (item 5). The cost moves to the
parent, which spawns and reads N servers instead of one; that is plumbing over code that already
works, not new confinement logic. It removes issue items 3 (selector) and most of 4 (repository key
in the serve record) outright.

What this gives up: the reviewer sees companion tools under a second server name rather than a
`repository` argument. The prompt names each server and the repository it serves, so this is a
presentation difference, not a capability one.

## Scope cuts from the issue (all deliberate)

- **No Claude `Read`/`Grep`/`Glob` rules for companions** (issue item 6). The Claude reviewer reads
  a companion only through that companion's evidence server, exactly as the Codex reviewer does.
  This is a *narrower* boundary move than the issue proposed: the reviewer's native read tools stay
  scoped to the primary root.
- **No new `HookReport` field.** Companion heads go to the hook *payload* (item 5) where the
  operator's script needs them. Adding them to `HookReport` would change the result envelope's
  `hook` schema (`src/findings.rs:1482-1491`) and bump the envelope version for information the
  calling agent has no use for. The skip *reason* names the companion that failed binding.
- **No change to `captured:`.** It stays the primary's capture. A companion whose canonical diff
  was not served complete already blocks convergence and the floor warning names it.
- **No per-companion resume delta.** The session records no `head_sha`/`base_sha` per companion;
  the reviewer pulls the companion's whole canonical diff each turn, which the floor requires for an
  approve anyway.
- **Payload v1 stays byte-identical when no companion is declared.** `schema_version: 2` and the
  `companions` array are emitted only on a review that declared at least one companion, so existing
  consumers see no change at all unless the operator configures and uses the feature.
- Out of scope, as in the issue: per-call paths, Perforce (primary or companion), forge status for
  the companion.
- **Out of scope, pre-existing (r4 f4): a reparse point *below* the state dir.** `tmp_file`
  (`reviewer/mod.rs:2394-2397`) runs `create_dir_all(<state_dir>/tmp)` and writes the reviewer CLI's
  final-message file there with no reparse check. A junction at `<state_dir>/tmp` can therefore
  redirect that write into a companion, but it redirects it into the **primary working root**
  equally well today, and no existing check (`neutral_dir`, the capability dir, the sterile dir)
  looks at `tmp_dir()`. Planting it requires write access to the operator's own state directory,
  which the reviewer does not have (Codex's writes are refused by the OS-enforced read-only policy;
  the Claude reviewer has no write tool). This plan neither creates nor widens that hole: the
  disjointness rule makes a companion exactly as protected as the primary root. The fix is one
  reparse check in `tmp_file` covering every root at once, and belongs in its own small PR, not
  inside companion support. Tracked as issue #168.

## Work items

### 1. Configuration — `--companion <name>=<path>` (`src/config.rs`)

- Repeatable, like `--on-converged-arg` (`config.rs:1295`). `take` already splits `--flag=value` at
  the first `=` (`config.rs:1063-1066`), so `--companion=assets=..\assets` yields `assets=..\assets`;
  split that at its first `=`.
- **Name:** `^[a-z][a-z0-9-]{0,15}$`, unique. Short and without `_` so the generated server name
  `cross_review_evidence_<name>` never contains `__` (Claude splits `mcp__<server>__<tool>` on it)
  and Claude/Codex tool names stay well inside client length limits (verify the actual limit during
  implementation; the longest becomes `mcp__cross_review_evidence_<16>__repository_revision`).
- **Path:** a relative path joins the normalized `cwd` (as `resolve_hook_program` does,
  `config.rs:2246-2275`), then is canonicalized with `normalize_dir`. Refused at startup, with a
  message naming the flag, when:
  - the backend is Perforce;
  - the path does not exist, or any component is a reparse point (`winsec::reject_reparse_on_ancestors`,
    `src/winsec.rs:587`);
  - it is not a git toplevel (`.git` present in that directory, the same test as
    `reviewer::is_git_toplevel`, `src/reviewer/mod.rs:1056`; a linked worktree's `.git` file counts);
  - it equals, contains, or is inside the working root or another companion;
  - it is not **disjoint** from the state directory or from the temp directory: refused if either
    of the two is inside the other, in either direction, judged on physical paths (junctions
    followed, as `worktree::state_dir_is_outside` does; unresolvable reads as overlapping).
- The last rule is what lets item 6 skip threading companions into every "outside the working root"
  check. Every directory the server or reviewer writes to is the state dir, the temp dir, or a
  path *derived under* one of them: the sterile dir (fresh child of temp), the neutral dir (state
  dir or temp), the capability dir (`<state_dir or temp>/cross-review-evidence-bundles`), the
  reviewer temp files at `Config::tmp_dir()` = `<state_dir>/tmp` (`config.rs:2226-2227`, written by
  `tmp_file`, `reviewer/mod.rs:2393-2397`), and the session, usage and status files
  (`session.rs:407`, `metrics.rs:516`, `usage.rs:83`, `events.rs:292`). A companion disjoint from
  both bases can contain none of them. Checking only "base not inside companion" was not enough
  (r3 f3): a companion configured *at* `<state_dir>/tmp` has the state dir outside it, yet receives
  the reviewer's temp-file writes. The cost of disjointness is that a repository inside `%TEMP%` or
  inside the state directory cannot be a companion, which is not a realistic layout.
- Stored as `Config.companions: Vec<Companion { name, root: PathBuf }>`. Help text and README
  updated.

### 2. Request surface (`src/mcp.rs`, `src/tools.rs` `start`)

- `companions: ["<name>", ...]` on `cross_model_review` and `cross_model_consult`. The property is
  **injected only when companions are configured**, with an `enum` of configured names (mirroring
  the conditional `level` injection, `mcp.rs:1278-1314`), so the calling agent discovers the names from the
  schema and a server without companions advertises nothing new.
- Validated in `start` before the lease, beside `root` (`tools.rs:931-958`): an array of distinct,
  configured names; an unknown name gets a plain correction listing the configured ones. Allowed
  with `root` (a nested worktree of the primary stays disjoint from every companion by item 1).
- Consult is included because the start path is shared and a consult reads the tree through the
  evidence service like a review does; refusing it would be extra code. A consult has no floor and
  no hook, so for a consult companions only add read access.
- The declared set rides on the per-call `cfg` (as `root` does) as `declared_companions`, so
  everything downstream reads one place. Companion roots always come from the **server's** startup
  configuration (`self.cfg.companions`), never from anything relative to a per-call `root`
  worktree (r1 open question 2).

### 3. Session binding (`src/session.rs`, `tools.rs` `resume_refusal`)

- `SessionRecord.companions: Option<Vec<CompanionBinding { name, root }>>`, `serde(default,
  skip_serializing_if = "Option::is_none")`, written sorted by name.
- `resume_refusal` (`tools.rs:5324-5333`, next to the `cwd` identity check): the requested set
  must equal the recorded one, name *and* root (`pathcmp::identity_eq_str`). `None` reads as the
  empty set, so every existing record resumes unchanged under a call that declares none. A mismatch
  is `SESSION_NOT_RESUMABLE` naming both sets and saying `fresh: true` starts over.
- No ledger is invalidated: the field is additive and absent on old records.

### 4. Evidence setup per repository (`tools.rs:4070-4153`, `src/evidence.rs`)

- For each declared companion, after the primary: a runtime recheck that the companion still
  canonicalizes to its configured root, is not a reparse point, and is still disjoint (item 1's
  both-directions rule) from the state directory and from **this turn's** `std::env::temp_dir()`
  (r1 open question 3: the sterile
  dir, `reviewer/mod.rs:1226`, and the capability dir, `evidence.rs:607`, read `temp_dir()` at turn
  time, so the authority is this per-turn check; item 1's startup check is only an early error).
  Then, with that companion's own nonce `cn = {review_id}-c-<name>` (next bullet) used everywhere
  the primary uses `self.id`: `Bundle::create(&companion.root, Git, &cn, label, status, None,
  page_ceiling)`, `write_bundle(&cfg, &bundle)` (file named from `cn`), `handshake(&exe,
  &bundle_file, &cn)`, and a fresh serve-record file at `serve_record_path(&cfg, &cn)`. The
  reviewer is spawned with `--evidence-server <bundle> <cn>` for that server (item 5). The review id
  is never a companion's nonce.
- **Each server gets its own evidence nonce** (r1 f1). The primary keeps the review id; a
  companion's is `{review_id}-c-<name>` (the name grammar fits `validate_nonce`'s
  `[A-Za-z0-9_-]{1,128}`, `evidence.rs:1398-1411`). The nonce is the namespace for everything a
  server mints, so this one choice separates all of it:
  - cursor tokens `{nonce}-{n:016x}` (`core.rs:1600`) and diff op ids `{nonce}-diff-{calls}`
    (`core.rs:1231`). With a shared nonce, two servers mint *identical* tokens, so a cursor the
    reviewer sends to the wrong server would silently continue that server's own operation and both
    serve records could read complete over mixed pages. With distinct nonces a misrouted cursor is
    simply unknown to the other server's map and refused;
  - the bundle, Claude MCP config and serve-record file names (`{nonce}-evidence.json`,
    `{nonce}-claude-mcp.json`, `{nonce}-serverecord.jsonl`), which both parent (`evidence.rs:599-604`)
    and child (`evidence.rs:756-758`) already derive from the nonce. So `serve_stdio` and stale-file
    reaping (`evidence.rs:666`) need **no change**.
- The parent reads each companion's serve record with `serve_record_path(cfg, companion_nonce)`.
- Per-repository turn state that is singular today becomes a small list: the bundle files, serve
  record files, and `turn_autocrlf` (`tools.rs:4069`, `4111`).
- Budgets: each server keeps its own `Limits::default()`, so total read budget scales with the
  number of companions. Acceptable; noted in the README.

### 5. Reviewer wiring (`src/reviewer/{mod,codex,claude}.rs`)

- `EvidenceInvocation` (`mod.rs:254-265`) gains `companions: Vec<{server_name, bundle_file,
  nonce}>`. It keeps a single `mcp_config_file`.
- **Codex** (`codex.rs:150-202`): one more `mcp_servers.<server_name>` `-c` block per companion,
  identical to the primary's (`required=true`, same `enabled_tools`, same sterile `cwd`, same
  timeouts). `required=true` means a companion server that fails to start fails the run, rather
  than silently reviewing without it.
- **Claude** (`claude.rs:101-180`, `evidence.rs:551-588`): `write_claude_mcp_config` today writes
  `{"mcpServers":{"cross_review_evidence":{command, args:["--evidence-server", bundle, nonce]}}}`
  for one server. It takes a list of `(server_name, bundle_path, nonce)` instead, primary first, and
  writes **one** combined file (still named from the primary nonce, `{review_id}-claude-mcp.json`)
  with one `mcpServers` entry per server, each carrying its own bundle and nonce. The single
  `--strict-mcp-config --mcp-config <file>` argv is unchanged; `--allowed-tools` gets
  `mcp__<server_name>` per companion. A companion server that fails
  to start under Claude leaves its tools absent, so its canonical diff is never served and the floor
  (item 6) fails closed without extra code.
- **Health parsing** keys on exactly one server name today: Claude's prefixes
  (`claude.rs:683-700`), Codex's `server == SERVER_NAME` (`codex.rs:842-845`) and its error
  attribution (`codex.rs:1024-1032`, which already matches companions by accident through
  `contains(SERVER_NAME)`). All three switch to an explicit set of this turn's server names, and
  companion calls count as evidence calls.

### 6. The floor and the in-turn repair (`tools.rs:4551-4692`, `5067-5081`)

- `read_serve_record_aggregate` is called once per repository; `aggregate_serve_records` is
  unchanged (each file holds one repository).
- `EvidenceCoverage.complete_canonical` (`findings.rs:2070-2079`) becomes "the primary **and every
  declared companion** has a complete, terminal, agreeing canonical diff". `looked_at_something`
  stays primary-or-content as today. No new `NonConvergenceReason`: a missing companion diff is
  `evidence_incomplete`, with a warning that names which repository was not served.
- `prompt::evidence_repair` (`src/prompt.rs:353-372`) takes the list of repositories still unmet
  and names each server to pull from. `RESUMED_CANONICAL_DIFF_REMINDER` (`prompt.rs:52`) and the
  main review prompt gain one paragraph, present only when companions are declared: each companion's
  name, root and server, and that an approve requires every listed repository's complete
  `branch-base..worktree` diff.
- The companion's base is its own fork point: `resolve_branch_base` already runs against the
  server's root (`core.rs:2544-2582`), so each companion server resolves
  `merge-base(HEAD, refs/remotes/origin/{HEAD,main,master})` in its own repository with no change.

### 7. Binding at attest time (`src/hook.rs`, `tools.rs` `attestable_for` / `attest`)

- `Attestable` gains `companions: Vec<{name, root, served: ServedChange}>`, built from each
  companion's aggregate via the existing `served_change` (with that bundle's `autocrlf`). Any
  companion without a `served_change` makes the attestable fail with `NO_CANONICAL_DIFF`, as the
  primary does.
- `fire` (`hook.rs:282-295`) runs `check_binding` for the primary and then each companion against
  its own root. A companion mismatch skips with a new reason `companion_head_does_not_match_review`
  carrying the companion name; `HookReport::summary` gives it the same "commit everything,
  including untracked files, then re-review" wording as `HEAD_MISMATCH`, naming the repository.
- `App::attest` (`tools.rs:2426-2512`) adds the companion recheck from item 4 next to
  `worktree::recheck`.
- `payload` (`hook.rs:253-279`): unchanged at `schema_version: 1` when no companion was declared;
  otherwise `schema_version: 2` plus `companions: [{name, repo_root, captured_head, base,
  base_source, diff_sha256}]`. `tree_clean` documents that it covers every listed repository.
  README's payload table updated.

## Verify

Unit tests (`cargo test`, no models):

- Config: name grammar, duplicate names, relative path resolution against `--cwd`, Perforce
  refusal, non-toplevel refusal, junction-in-path refusal, overlap with the working root and with
  another companion; overlap with the state dir in both directions (state dir inside a companion,
  and a companion at `<state_dir>/tmp`), and the same for the temp dir; a junction that makes a
  companion physically overlap the state dir. The per-turn recheck refuses the same cases.
- `start`: undeclared/unknown name refused with the configured list; schema advertises `companions`
  only when configured.
- Resume: same set resumes; added, removed or re-rooted companion refused; legacy record (no field)
  resumes under a call with no companions.
- Floor: with a companion declared and only the primary's canonical diff served, an APPROVE turn is
  `changes_requested` / `evidence_incomplete`; both served converges. The repair prompt names the
  unmet companion.
- Hook: a further commit in the companion after convergence skips with
  `companion_head_does_not_match_review`; payload is byte-identical v1 without companions and v2
  with them.
- Argv: Codex gets one `mcp_servers` block per declared companion (each with its own nonce) and none
  for undeclared; Claude's single combined MCP config lists exactly the declared servers, each with
  its own bundle path and nonce, `--allowed-tools` lists the same servers, and no `Read(<companion>/**)`
  rule is ever emitted.
- Health parsing attributes companion-server calls and errors to the evidence service.
- Cursor isolation: a `repository_diff` cursor minted by the primary server is refused by a
  companion server (and vice versa), and leaves both serve records unchanged.
- Existing single-root and `root` worktree tests pass unchanged.

`smoke.ps1`, **both** `-Reviewer codex` and `-Reviewer claude` (this changes evidence spawning, so
AGENTS.md requires the real round trip; it costs tokens): a scratch companion repository with a
small change, declared on the review, and an assertion that the reviewer reached the companion
server's `repository_diff` and the review converged only with both diffs served.

## Resolved questions (round 1)

1. **One server per repository** — accepted by the reviewer, conditional on per-server cursor
   namespacing (now item 4) and per-file aggregation (item 6).
2. **`root` together with `companions`** — allowed; companion roots are anchored to the server
   configuration, not the per-call worktree (item 2).
3. **Temp directory** — decided: the per-turn recheck in item 4 is authoritative, and the startup
   check is an early error only.
