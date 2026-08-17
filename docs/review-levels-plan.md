# Plan: user-defined review "levels" (model + effort presets)

## Context

Today the reviewer's `model` and `effort` are pinned once at server launch
(`--model` / `--effort`) and are immutable for the life of the server entry. A caller
cannot ask for a faster, cheaper pass or a slower, deeper one without the operator editing
config and restarting.

We want the operator to **define named levels** at launch, each level being a
`(model, effort)` pair, and let the caller pick one **per review at start time** via a new
`level` argument on `cross_model_review`. Levels are fixed once a session starts: collect
(`cross_model_review_result`) and re-review/resume never take a `level` — the session keeps
whatever it resolved to at start.

Because each server entry drives exactly one reviewer direction, each config defines that
direction's levels. The intended dogfood mapping:

| Level | Claude (`.codex/config.toml`) | Codex/Luna (`.mcp.json`) |
|-------|-------------------------------|--------------------------|
| fast | `claude-opus-4-8` / `low` | `gpt-5.6-luna` / `high` |
| standard (default) | `claude-opus-4-8` / `medium` | `gpt-5.6-luna` / `xhigh` |
| thorough | `claude-opus-4-8` / `high` | `gpt-5.6-luna` / `max` |

Note "standard" drops Luna from today's `max` to `xhigh` **on purpose** — xhigh and max are
near-equal in quality but max runs ~4x slower.

## Design

Resolve a level to `(model, effort)` **at review start**, writing the resolved strings into
the active `ReviewerSpec.model` / `.effort` before anything reads them. This is the key
decision: every existing consumer already keys on those two string fields — the invocation
builder (`codex.rs:119/129`, `claude.rs:118/119`), `record_turn` → `SessionRecord.model/effort`
(`tools.rs:3436-3437`), and resume matching (`config.rs:1346-1347`). So resolving eagerly into
those fields keeps all of them working with no new persisted field and no ledger schema bump.

Levels are declared **per reviewer entry** and carried on `ReviewerSpec` as a map. They are
**not** extra entries in the fallback chain (`reviewers: Vec<ReviewerSpec>`) — that Vec is the
rate-limit fallback walk and must not gain level rows.

### 1. Data model — `src/config.rs`

- Add to `ReviewerSpec` (around lines 111-129):
  ```rust
  pub levels: BTreeMap<String, LevelOverride>,   // name -> resolved (model, effort)
  pub default_level: Option<String>,             // which level applies when `level` omitted
  ```
  `struct LevelOverride { model: String, effort: String }`.
- **Exclude `levels`/`default_level` from identity.** `same_reviewer_identity` (lines 139-145)
  and `validate_chain` (line 1403) must keep comparing only the resolved
  `reviewer/model/effort/profile/bin` — same treatment `usage_minimum` already gets. Two
  entries that differ only in their level menu are still the same rate-limit identity.

### 2. CLI parsing — `src/config.rs` `Config::from_args` + `PendingEntry`

- Add `--level NAME:MODEL:EFFORT` (repeatable), bound to the most-recent `--reviewer` like
  `--model`/`--effort` already are (`set_model`/`set_effort`, lines 343-365; `finalize`
  380-393). Model ids and effort names contain no colons, so a colon-delimited triple parses
  unambiguously.
- Add `--default-level NAME` (optional), also per-entry.
- Validation at parse/finalize time (fail fast, before any billing):
  - level name non-empty and unique within the entry;
  - `MODEL` non-empty (pin-by-full-id convention — don't hard-reject, but the existing
    unknown-effort warning path at 1130-1144 is the model for a soft warn);
  - `EFFORT` validated against `known_efforts()` for the entry's `ReviewerKind` (77-82),
    reusing the existing warn-not-fail policy;
  - `--default-level`, if set, must name a declared level.

### 3. MCP tool schema — `src/mcp.rs` (`cross_model_review`, ~lines 831-863)

- When the active reviewer declares >=1 level, inject an optional `level` property:
  `{ "type": "string", "enum": [<declared names>], "description": "<name -> model/effort, and which is default>" }`.
  Build the enum + description from config so the calling agent sees the menu.
- When no levels are declared, omit the property entirely — with `additionalProperties:false`
  this means old configs behave exactly as today and a stray `level` is rejected.

### 4. Start-review resolution — `src/tools.rs` `start_review` (~617-659) + Job build (~943-947)

- Parse optional `level` from request args alongside `instructions`/`session`/`fresh`.
- **Only on a fresh start** (new session, or `fresh:true`): resolve the effective level:
  - explicit `level` present -> must exist in the selected entry's `levels` (else fail fast
    with a clear `INVALID_LEVEL`-style error, no model call);
  - absent -> entry's `default_level` if set, else fall back to today's `--model`/`--effort`.
  - Clone the selected `cfg.reviewers[start_index]` and overwrite `.model`/`.effort` from the
    resolved `LevelOverride`. That clone becomes `Job.spec` — unchanged plumbing downstream.
- **On resume** (existing session, `fresh:false`): ignore `level` entirely; the session's
  persisted `(model, effort)` already define the effective spec.

### 5. Resume matching — `src/config.rs` `resume_entry_index` (1343-1375)

This is the one non-additive change. Today it matches `record.model/effort` against each live
chain entry's `model`/`effort`. A session started at a non-default level persisted, e.g.,
`(opus, high)`, while the live entry's base is `(opus, medium)` — so the naive match would
refuse every non-default-level resume.

- Expand the match: an entry matches the record when `record.(model,effort)` equals **either**
  the entry's base `(model, effort)` **or** any of its declared `levels` pairs (with
  `reviewer`/`profile`/`bin` matching as today).
- This preserves the intended fail-closed safety: if the operator later removes a level or
  remaps its `(model, effort)`, a session pinned to the old pair no longer matches -> refuse ->
  rebaseline. That is correct.

### 6. Fallback-chain + levels (scope boundary)

For the common single-entry dogfood configs this never arises. To avoid over-building:
resolve the requested level against the **selected** entry only; if a rate-limit fallback
advances to an entry that does not declare that level, that entry uses its own base
`model/effort` and emits a stderr diagnostic. Document this; do not add cross-entry level
consistency machinery in v1.

### 7. Dogfood configs + docs (separate follow-up commit, NOT bundled with code)

- `.mcp.json`: add `--level fast:gpt-5.6-luna:high`, `--level standard:gpt-5.6-luna:xhigh`,
  `--level thorough:gpt-5.6-luna:max`, `--default-level standard`. Since standard=xhigh this
  changes the Codex default effort from `max` -> `xhigh` (intended).
- `.codex/config.toml`: add the Claude column (`low`/`medium`/`high`) + `--default-level standard`.
- `README.md`: document `--level` / `--default-level`, the `level` call arg, resume behavior,
  and the fallback caveat. `AGENTS.md`: note that the gate can now be run at a chosen level and
  that level is fixed per session.
- These config edits dirty the tree, so they must land **after** the code change is committed
  and merged — never in the same commit as the gated PR (per AGENTS.md dirty-tree rule).

## What this deliberately does NOT do

- No new persisted `SessionRecord` field, no ledger schema bump (resolve into existing
  `model`/`effort`).
- No level in resume/collect calls; no mid-session level switching (use `fresh:true`).
- No `level`-in-identity, no cross-entry level consistency validation.
- No per-level fallback chains.

## Critical files

- `src/config.rs` — `ReviewerSpec`, `LevelOverride`, `from_args`/`PendingEntry`, `resume_entry_index`, validation.
- `src/tools.rs` — `start_review` arg parse + level resolution, Job spec build.
- `src/mcp.rs` — `cross_model_review` schema `level` property.
- `.mcp.json`, `.codex/config.toml`, `README.md`, `AGENTS.md` — follow-up commit.

## Verification

- `cargo test` — unit tests, no network. Add tests for: `--level` parse (valid/dup/bad-effort),
  `--default-level` unknown -> reject, level->spec resolution on fresh start, `level` omitted ->
  default, unknown `level` on call -> fast error, and **resume matching against a level pair**
  (start at `thorough`, persist, resume -> `resume_entry_index` finds the entry).
- `.\build.ps1` — fmt, clippy `-D warnings`, tests, release build, restage.
- `smoke.ps1 -Reviewer codex` and `-Reviewer claude` — real round trip. Run once with an
  explicit `level` and once omitted; confirm the reviewer runs at the resolved effort. Costs
  tokens — mention to user before running.
- The change touches protocol (new tool arg) and session/resume handling, so the smoke round
  trip is required, not optional.

## Test-the-gate

After merge, run the cross-review gate itself at each level against a scratch diff to confirm
the `level` arg flows end-to-end and resume stays intact when level is omitted on re-review.
