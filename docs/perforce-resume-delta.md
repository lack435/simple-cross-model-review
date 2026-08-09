# Perforce resume delta — design

Status: **proposal, not yet implemented.** This document is the plan under review; nothing
in the code implements it yet.

## Problem

PR #36 gave the **git** backend an incremental resume delta: on a re-review under the same
session, the reviewer is sent only `<prior_head>..HEAD` — the commits added since its last
turn — instead of the whole configured range again. The reviewer still holds the earlier
full diff in its resumed conversation, so it re-checks its findings against the delta rather
than re-reading the entire change every turn.

The **Perforce** backend has no equivalent. Every resumed turn re-captures and re-sends the
full changelist diff (up to the `MAX_DIFF_BYTES` cap). For a large changelist re-reviewed
several times in the act-on-feedback loop, that re-spends the reviewer's context — and the
caller's token budget — on content the reviewer has already seen. This is the exact waste
the git delta refuses to pay, and it is not acceptable for Perforce either.

## Why git's mechanism cannot be ported

Git deltas along an **immutable, ancestry-checkable commit range**. `incremental_base`
(`src/vcs/git.rs`) only emits a delta when the range still resolves to the same effective
base *and* the prior commit is an ancestor of the current HEAD — that ancestry proof is what
makes `<prior>..HEAD` a true, additive continuation of what the reviewer already saw.

A Perforce changelist gives none of that:

- A **pending** changelist mutates *in place*. Turn N and turn N+1 are the same changelist
  number with different workspace contents. There is no prior revision to subtract and no
  ancestry relation between the two states — a file can be reworked, reverted, or removed
  from the change between turns.
- A **shelved** changelist can be replaced (`p4 shelve -f`), so shelved content also moves
  in place.
- A **submitted** changelist is immutable, so a re-review shows a byte-for-byte identical
  diff every turn — there is nothing to delta, only everything to *elide*.

So the git delta's precondition is absent for the mutable cases and moot for the immutable
one. The *purpose* — don't re-spend the reviewer's context on bytes it already saw — is
still achievable, through a different mechanism.

## Mechanism: per-file elision keyed on the reviewer's own view

On a genuine resume, for each file in the captured change, compare a **fingerprint of the
diff the reviewer was shown last turn** against this turn's. Files whose fingerprint matches
collapse to a single line; everything else is shown in full and explicitly labelled.

The claim this makes to the reviewer is **byte-identity** — "this file is identical to what
you already saw" — which is directly verifiable and needs no ancestry proof. That is the
one guarantee Perforce cannot give for a mutable changelist, and this design sidesteps it
rather than faking it.

Deliberately **coarse**: a file that changed *at all* is re-sent whole, not diffed against
its own last-shown state. A diff-of-a-diff is noisy and error-prone, and a reviewer
re-reading one changed file is cheap. Coarse per-file elision is the robust choice, not
merely the simple one.

## Complete case coverage

The same mechanism covers all three changelist types once file identity is namespaced by
segment kind:

| CL type | Between-turn behaviour | Result |
|---|---|---|
| **Pending** | mutates in place (edit reworked, reverted, newly opened) | per-file elision — the primary win |
| **Shelved** (`include_shelved`) | shelf can be replaced; a file may be opened *and* shelved with different content | fingerprinted in its **own namespace**, separate from the workspace section |
| **Submitted** | immutable | every file matches → the whole CL collapses to "unchanged since last turn". Falls out for free |

Per-file **state transitions**, each rendered explicitly so nothing reads as silently
omitted:

- **unchanged** → `path — unchanged since your previous turn (N lines)`
- **modified** → full current diff, laballed *changed since last turn*
- **added to CL** (newly opened) → full diff
- **removed from CL** (reverted) → `path — no longer in the changelist; disregard your earlier review of it`
- **moved** (`p4 move`, depot path changes) → treated as remove-old + add-new; both shown in full (never elided across a rename)
- **type flip** (text ↔ binary) → never elides (type is part of the fingerprint)

## Correctness invariants

These are what make the design robust, not merely working.

### 1. Fingerprint the diff's semantic inputs, not the rendered markdown

The fingerprint covers `(depot path, action, file type, base revision the diff was taken
against, diff text)`. Two subtleties this closes:

- **Base revision must be in the key.** If a `p4 sync` moves the have-rev under a pending
  edit between turns, the workspace content can be byte-identical while the *diff* changes.
  Content alone would falsely elide; including the base revision prevents it.
- Fingerprinting semantic inputs rather than our rendered caveats means a change to the
  server's own prompt wording in a future version does not silently bust every in-flight
  fingerprint (it would just fall back to full capture, but needlessly).

### 2. A persisted fingerprint needs a *stable* hash

The fingerprint is written to `sessions.json` and read back after a server restart or
upgrade. `std::collections::hash_map::DefaultHasher` is explicitly **not** guaranteed stable
across Rust releases, so using it would silently start mismatching every fingerprint after a
toolchain bump — disabling the delta invisibly. With `serde` as the only dependency, the
robust choice is a small **vendored stable hash** (FNV-1a-style, 128-bit, in-repo), storing
`(length, hash128)` so a false "unchanged" would require a hash collision *at a fixed
length* — astronomically unlikely.

Storing the full last-shown diff text per file would give zero-collision exact equality but
bloats session state for large changelists, defeating the goal. Length + 128-bit hash is the
right trade.

### 3. Never elide a partially-shown file, either direction

If a file's diff was cut short by the `MAX_DIFF_BYTES` budget last turn *or* this turn, it is
marked incomplete and re-sent — the direct analog of git suppressing `head_sha` on a
truncated capture. Carry a per-file complete/partial flag.

### 4. Gated on a genuine resume

Elision consults the fingerprint map only when `prior` is present and the reviewer provably
still holds the prior turn. `fresh: true`, `SESSION_NOT_RESUMABLE`, and `SESSION_NOT_FOUND`
all fall back to full capture. No new gate is needed — the existing resume machinery in
`tools.rs` already draws this line (`prior` is `None` for a fresh review, and a
non-resumable session is refused at `resume_block` before capture runs).

### 5. Changelist-set identity is already enforced

The session's `changes` binding (`src/session.rs`) refuses a resume that names a different
changelist set, so the fingerprint map is always for the same changelists. No new invariant.

### 6. Elision relieves the prompt cap

Charge the `MAX_DIFF_BYTES` budget only for files rendered in full — collapsed files cost one
line. A large changelist that currently truncates may fully fit once stable files collapse.

Honest limit: the 60-second *capture* wall-clock budget is still spent running `p4 diff` on
unchanged files (unless the fstat-digest optimisation below is adopted). The token saving —
the actual "inordinate usage" the caller is billed for — is fully delivered regardless,
because tokens are dominated by prompt bytes sent to the reviewer, not local `p4` calls.

## State model

A new optional field on `SessionRecord` / `TurnFacts`, **Perforce-only**, sibling to the git
`head_sha` / `base_sha`: a list of per-file records
`(segment-kind, depot-path, base-rev, action, type, complete, length, hash128, line-count)`.
Retained and advanced each turn like `head_sha`. `#[serde(default)]` for back-compat with
sessions recorded before the field existed.

**Bounded with a stated cap** (in the spirit of `MAX_UNTRACKED_FILES`): past N files, skip
elision and fall back to full capture with an explicit warning — no silent truncation.

## Reviewer-facing rendering

A framing note up front, mirroring git's follow-up preamble (`render` in `src/vcs/git.rs`):
that the reviewer saw the full changelist(s) earlier in this session, that unchanged files
are collapsed to one line below, that only what moved is shown in full, and that it should
re-check its earlier findings against the changes. Then the per-file lines from the
transition table.

This replaces the current `resumed_capture_note` in `tools.rs`, which today only warns that a
pending changelist's contents can move between turns.

## Change surface

- **`src/vcs/perforce.rs`** — the bulk of the work. Restructure `render` and the
  `Segment` / `DiffSection` path so each file's diff is an addressable unit that can be shown
  in full or collapsed. Fingerprint computation. The resume-delta decision as its own
  backend-specific function, mirroring the discipline of git's `incremental_base`.
- **`src/vcs/shared.rs`** (or a new small module) — the stable hash. It is VCS-neutral, so
  `shared` is the right home given its single-sourcing mandate.
- **`src/session.rs`** — the per-file fingerprint list on `SessionRecord` and `TurnFacts`,
  `#[serde(default)]`.
- **`src/vcs/mod.rs`** and **`src/tools.rs`** — thread the prior fingerprint map into
  `capture` the way `GitResumeBaseline` is threaded now, and return the new map out like
  `head_sha`. `capture`'s signature grows a Perforce baseline alongside the git one;
  consider a unified `resume` enum rather than two `Option` parameters.
- **config** — reuse the existing `resume_incremental_diff` switch so one flag governs
  incremental behaviour for whichever backend is active.

## Test plan

- **Unit:** fingerprint stability across a simulated restart (serialize → deserialize → still
  matches); each state transition (unchanged / modified / added / removed / moved /
  type-flip); base-rev change with identical content → **not** elided; truncated-either-turn
  → not elided; cap exceeded → full fallback with warning.
- **Golden render snapshots** for the resumed Perforce prompt (like
  `render_output_is_byte_for_byte_stable` in `src/vcs/mod.rs`), covering a mixed turn: some
  files collapsed, one modified, one removed, one added.
- **Live smoke** (`smoke.ps1 -Reviewer codex`) against a real pending changelist edited
  between two turns — verifies the round trip and that turn 2 is billed materially less.
  Costs tokens; run when touching this path.

## Optional later layer (not v1)

Skip the `p4 diff` on unchanged files via `p4 fstat` (have-rev plus a workspace content hash
we compute by reading the file — we are already a process at the workspace root) or shelf
digests, comparing metadata *including base rev* before diffing. Cuts capture wall-clock too,
not just tokens. Kept out of the first cut: it is a capture-cost optimisation, the token win
is fully delivered without it, and it adds a second correctness surface.

## Open decisions / risks

- **Unified `resume` parameter vs two backend-specific `Option`s** on `capture` — leaning
  unified enum for clarity.
- **Rename detection fidelity** — Perforce `move/add` + `move/delete` pairing; v1 treats them
  as independent add + delete (safe, slightly more verbose). Pairing them into a single
  "moved" line is later polish.
- **Dogfooding** — this change touches `session.rs` serialization and the capture seam, so
  the cross-review gate reviewer should be pointed specifically at fingerprint stability and
  the truncation-suppression invariants.
