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

Resolve a level to an **effective `(model, effort)` for the starting entry**, carried on the
`Job` and applied wherever the start entry's spec is materialized. Every downstream consumer
already keys on `ReviewerSpec.model`/`.effort` — the invocation builder (`codex.rs:119/129`,
`claude.rs:118/119`), `record_turn` → `SessionRecord.model/effort` (`tools.rs:3436-3437`), and
resume matching (`config.rs:1346-1347`) — so once `self.spec` carries the resolved pair, all of
them work with no new persisted field and no ledger schema bump.

**Correction from design review (f1/f2):** it is NOT enough to set `Job.spec` once at
construction. `Job::run` re-derives the active spec inside its fallback loop —
`let entry = chain[i].clone()` (`tools.rs:2219`) then `self.spec = entry.clone()`
(`tools.rs:2281`) — which clobbers any pre-set level; and the pre-capture display reads the base
config directly (`self.cfg.reviewers[start_index].describe_with_bin`, `tools.rs:2045`), and the
rate-limited-attempt metric records `entry.model`/`entry.effort` (`tools.rs:2378`). So the
override must be applied at the point the **start** entry is materialized, not once up front.

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

### 4. Effective-spec lifecycle — `src/tools.rs` `start_review` (~617-659), Job build (~943-947), `Job::run` (~2040-2380)

Resolution happens in `start_review`; **application** happens wherever the start entry's spec
is materialized in `Job::run`.

**Resolve (in `start_review`):**
- Parse optional `level` from request args alongside `instructions`/`session`/`fresh`.
- **Fresh start** (new session, or `fresh:true`): compute the effective pair:
  - explicit `level` present -> must exist in the selected entry's `levels` (else fail fast
    with a clear `INVALID_LEVEL`-style error, no model call);
  - absent -> entry's `default_level` if set, else today's `--model`/`--effort`.
- **Resume** (existing session, `fresh:false`): the effective pair is the session's persisted
  `(record.model, record.effort)` — not a `level` arg. See section 4a for how an explicit
  `level` on a resume is handled.
- Carry the resolved pair on the `Job` as `start_spec_override: Option<LevelOverride>` (the
  effective `(model, effort)` for `start_index` only). `None` means "use the entry's base pair"
  (backward-compatible: no levels, no override).

**Apply (in `Job::run`):** when materializing the entry for `i == self.start_index`, overwrite
`entry.model`/`entry.effort` from `start_spec_override` *before* `self.spec = entry.clone()`
(`tools.rs:2281`). Because `entry` then carries the override for the start entry:
- the invocation (reads `self.spec`) runs at the resolved pair — fixes **f1**;
- the rate-limited-attempt metric at `tools.rs:2378` (reads `entry.model/effort`) records the
  resolved pair — fixes **f2**;
- `record_turn` (reads `self.spec`) persists the resolved pair, so resume matches it.
- The pre-capture display at `tools.rs:2045` must use the effective describe for the start
  entry too (build it from the overridden spec), so a snapshot names the pair that will run.
- **Fallback entries** (`i != start_index`) keep their own base pair — no override applied —
  which is the accepted v1 scope boundary (section 6); the metric/display then report the
  *actual* fallback pair, which is what the reviewer required.

### 4a. Explicit `level` on resume (f3)

Ignoring `level` on resume silently can mislead a caller into believing they got a
fast/thorough pass when the session kept its original pair. Guard cheaply rather than error on
every re-review (agents naturally re-pass the same args):
- On resume, if `level` is **present and resolves to a pair that differs** from the session's
  persisted `(model, effort)`: fail fast with a clear error — session is pinned to
  `model=…/effort=…`; pass `fresh:true` to start a new session at the requested level.
- If `level` is absent, or resolves to the **same** pair the session already uses: proceed.
- Surface the effective session level in the response/diagnostic either way.

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
- No mid-session level switching: an explicit `level` on resume that *differs* from the
  session's pair is rejected (use `fresh:true`), not silently applied (§4a).
- No `level`-in-identity, no cross-entry level consistency validation.
- No per-level fallback chains; a fallback entry runs at its own base pair (§6), which the
  design-review reviewer accepted as proportionate to the lost-review failure mode.

## Critical files

- `src/config.rs` — `ReviewerSpec`, `LevelOverride`, `from_args`/`PendingEntry`, `resume_entry_index`, validation.
- `src/tools.rs` — `start_review` level resolution (~617-659); `Job.start_spec_override` field +
  build (~943-947); **application in `Job::run`** at the start-entry materialization (~2219-2281),
  the pre-capture display (~2045), and the rate-limited-attempt metric (~2378); §4a resume guard.
- `src/mcp.rs` — `cross_model_review` schema `level` property.
- `.mcp.json`, `.codex/config.toml`, `README.md`, `AGENTS.md` — follow-up commit.

## Verification

- `cargo test` — unit tests, no network. Add tests for: `--level` parse (valid/dup/bad-effort),
  `--default-level` unknown -> reject, level->spec resolution on fresh start, `level` omitted ->
  default, unknown `level` on call -> fast error, **resume matching against a level pair**
  (start at `thorough`, persist, resume -> `resume_entry_index` finds the entry), and the §4a
  resume guard (differing `level` on resume -> rejected; matching/absent -> accepted).
- **f1/f2 regression tests:** assert the resolved pair actually reaches the built CLI argv on a
  **fresh** level start AND after a **resume** (the effective spec survives `Job::run`'s
  `self.spec = entry.clone()`), and that a rate-limited start-entry attempt metric records the
  resolved pair, not the base pair.
- `.\build.ps1` — fmt, clippy `-D warnings`, tests, release build, restage.
- `smoke.ps1 -Reviewer codex` and `-Reviewer claude` — real round trip. Run once with an
  explicit `level` and once omitted; confirm the reviewer runs at the resolved effort. Costs
  tokens — mention to user before running.
- The change touches protocol (new tool arg) and session/resume handling, so the smoke round
  trip is required, not optional.

## Test-the-gate

After merge, run the cross-review gate itself at each level against a scratch diff to confirm
the `level` arg flows end-to-end and resume stays intact when level is omitted on re-review.
