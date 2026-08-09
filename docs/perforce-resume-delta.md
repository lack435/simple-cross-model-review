# Perforce resume delta — design

Status: **proposal, not yet implemented.** This document is the plan under review; nothing
in the code implements it yet. It has been through one round of cross-model review (see
[Review history](#review-history)).

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

A Perforce changelist gives none of that: a **pending** changelist mutates in place, a
**shelved** one can be replaced (`p4 shelve -f`), and a **submitted** one is immutable (so a
re-review shows a byte-for-byte identical diff every turn — nothing to delta, everything to
elide). The git delta's precondition is absent for the mutable cases and moot for the
immutable one. The *purpose* — don't re-spend the reviewer's context on bytes it already saw
— is still achievable, through a different mechanism.

## Mechanism: per-file elision keyed on the reviewer's own view

On a genuine resume, for each **elidable evidence unit** in the captured change, compare a
**fingerprint of the exact evidence the reviewer was shown last turn** against this turn's.
Units whose fingerprint matches collapse to a single line; everything else is shown in full
and explicitly labelled.

The claim this makes to the reviewer is **byte-identity** — "this is identical to what you
already saw" — which is directly verifiable and needs no ancestry proof. That is the one
guarantee Perforce cannot give for a mutable changelist, and this design sidesteps it rather
than faking it.

Deliberately **coarse**: a unit that changed *at all* is re-sent whole, not diffed against
its own last-shown state. A diff-of-a-diff is noisy and error-prone, and a reviewer
re-reading one changed file is cheap.

### What is elidable, and what is not

Only the **token-heavy** evidence participates in elision:

- **Elidable:** a file's textual `p4 diff` section, and an added file's body (which is *not*
  in `p4 diff` — it is read from the workspace and rendered separately, at
  `src/vcs/perforce.rs:565`, `:628`, `:951`, the Perforce analog of git untracked files).
- **Never elidable — always rendered in full every turn:** binary files, deletes,
  unreadable/omitted content, and every metadata note. These are already one-liners, so
  eliding them saves nothing and only widens the attack surface. Critically, the backend
  runs `p4 diff -du` **without** `-t` (`src/vcs/perforce.rs:595`), so a same-type binary
  *edit* produces identical *empty* textual evidence on both turns; fingerprinting that empty
  text would falsely elide a real change. Making binary non-elidable removes the hazard
  outright rather than relying on the type field to catch it.

This split is the direct answer to review finding 2: the elidable units are exactly the ones
worth eliding, and each carries the *entire* evidence a reviewer was shown for it (diff body
or add body), never a fragment.

## Complete case coverage

| CL type | Between-turn behaviour | Result |
|---|---|---|
| **Pending** | mutates in place (edit reworked, reverted, newly opened) | per-file elision of text diffs and add bodies — the primary win |
| **Shelved** (`include_shelved`) | shelf can be replaced; a file may be opened *and* shelved with different content | fingerprinted in its **own basis namespace**, separate from the workspace section |
| **Submitted** | immutable | every unit matches → the whole CL collapses. Falls out for free |

Per-file **state transitions**, each rendered explicitly so nothing reads as silently
omitted:

- **unchanged** → `path — unchanged since your previous turn (N lines)`
- **modified** → full current diff/body, labelled *changed since last turn*
- **added to CL** (newly opened) → full body
- **removed from CL** (reverted) → `path — no longer in the changelist; disregard your earlier review of it` — **only emitted when the file inventory for that changelist is known complete** (see completeness, below); an inventory truncated by `p4 opened`/`describe` limits could otherwise report a still-present file as removed.
- **moved** (`p4 move`, depot path changes) → treated as remove-old + add-new; both shown in full (never elided across a rename). `move/add` + `move/delete` pairing into one "moved" line is later polish.
- **type flip** (text ↔ binary) → the binary side is non-elidable, so this can never falsely elide.

## Identity of an evidence unit

The lookup key is `(changelist number, basis, depot path)`, where `basis` distinguishes the
workspace/pending, shelved, and submitted segments of the same file.

The **changelist number is part of the key** (review finding 1): the backend renders each
requested changelist as its own `Segment` (`src/vcs/perforce.rs:217`, `:869`), and one depot
path can appear in several requested changelists. `(basis, depot-path)` alone cannot tell
those records apart. The depot path is the stable Perforce identity (workspace/local paths
move with the client view), canonicalised as the backend already does.

## Correctness invariants

These are what make the design robust, not merely working.

### 1. Fingerprint the exact evidence shown, captured from the same output

The fingerprint is computed over the **canonical bytes of the evidence unit as the reviewer
receives it** — the diff body (or add body) plus the identity tuple and the semantic inputs
that determine that body. Hashing the shown bytes means encoding, line-ending
normalisation, RCS keyword expansion, and any other rendering of the content are *already
baked in*, because they are already baked into what was shown (review findings 2 and 5).

Two inputs are **not** visible in the body and must be captured from the *same* per-file
evidence, never reconstructed from a separate call:

- **Base revision.** `p4 diff` compares workspace content against a depot revision, and
  `p4 have` can move after a sync even for an open file — so a byte-identical workspace can
  produce a different diff, and content alone would falsely elide. The current parser keeps
  only `(depot, body)` (`src/vcs/perforce.rs:1334`, `:1524`); a separate `p4 have` call may
  report a revision the diff did not use. Capture the base revision from the diff header the
  diff itself emitted; if it is not recoverable there, mark the unit **unknown** and do not
  elide it.
- **Diff-format inputs.** The `p4 diff` flags and any client/config setting that changes the
  rendered form for identical content (diff algorithm, whitespace flags) are folded into a
  **schema version** (below), so a change to how we invoke `p4` invalidates every prior
  fingerprint rather than silently comparing across two formats.

`p4 describe -a` behaviour for added files, and the client mapping / local path used, are to
be pinned down during implementation against a real workspace; where any of them cannot be
captured from the shown evidence, the unit is marked unknown and not elided.

### 2. A persisted fingerprint needs a *collision-resistant* hash

The fingerprint is written to `sessions.json`, and the diff it covers is **attacker-influenced
content from a repository this project explicitly does not trust**. A non-cryptographic,
fixed hash (FNV and the like) can be *deliberately* collided: an author who controls both the
turn-N and turn-(N+1) revisions could craft a changed file whose evidence hashes identically
to the benign version reviewed on turn N, so the malicious version renders as "unchanged" and
escapes the merge gate (review finding 4). Length plus FNV defeats accidental collisions, not
an adversary.

So the digest must be collision-resistant. This is a Windows-only project, so the vetted,
dependency-free path is **Windows CNG** (`BCryptHashData`, SHA-256) via FFI — no crate, and a
reviewed OS implementation rather than a hand-rolled one. If a trustworthy digest is
unavailable at runtime, **elision is disabled** (full capture) rather than falling back to a
weak hash. `DefaultHasher` is additionally unusable because it is not stable across Rust
releases and this value is persisted; CNG SHA-256 is both stable and collision-resistant.

Store `(length, digest, algorithm+schema version)`. The **schema version** invalidates every
fingerprint whenever the digest algorithm, the evidence canonicalisation, or the `p4`
invocation changes — closing the "comparing across two formats" hole in invariant 1.

### 3. Completeness is per unit and covers *every* truncation source

A unit may be elided next turn only if it was shown **completely** this turn and last turn.
Completeness is not just the `MAX_DIFF_BYTES` rendering budget (review finding 3). Every one
of these marks the affected unit — or the whole capture — non-elidable:

- the `MAX_DIFF_BYTES` (400 KB) rendering budget cutting a diff short;
- the **8 MiB process-output cap** on a `p4` invocation (`src/reviewer/mod.rs:31`, `:289`),
  which can cut off the final file of a large `describe`/`diff`;
- truncated `describe`, `opened`, or `where` output, which the backend already tracks
  (`src/vcs/perforce.rs:309`, `:1120`, `:1147`, `:1166`);
- the per-file and total **add-body caps** (the `MAX_UNTRACKED_*` family);
- a **skipped changelist** or a **cancelled capture**.

Ordering that lets elision relieve the prompt cap: parse and hash the **retained raw `p4`
output** *before* applying the 400 KB rendering budget, so collapsed units free budget for
the units shown in full. If the raw output itself was truncated at the process cap, take a
**full capture with elision disabled** for that changelist — a truncated inventory also means
"removed" cannot be asserted (invariant in the transitions table).

### 4. Gated on a genuine resume, with the failure timeline made explicit

Elision consults the fingerprint map only when the reviewer provably still holds the prior
turn. The gating splits by *when* it is known (review finding 6):

- **Pre-capture:** `fresh: true` clears `prior` (`src/tools.rs:179`), and a stale/non-matching
  session is refused at `resume_block` before capture runs. Either way capture takes the full
  path. These are the only points at which a full-capture *fallback* can be chosen.
- **Post-capture:** `SESSION_NOT_FOUND` is detected only *after* capture, when the reviewer
  CLI rejects the resume (`src/tools.rs:790`, `:831`). It therefore cannot be a pre-capture
  fallback; it simply means this turn failed and the session is forgotten. That is already
  safe, because the fingerprint map is recorded **only after a parsed review** — a failed turn
  writes no baseline — but the doc must not describe it as a capture-time gate, which the
  previous draft did.

### 5. Changelist-set identity is already enforced

The session's `changes` binding (`src/session.rs`) refuses a resume that names a different
changelist set, so the fingerprint map is always for the same changelists. No new invariant.

## State model

The baseline carried on the session is an **explicit three-state value**, not a bare
`Option<map>` (review finding 6):

- `Full(map)` — the previous turn captured every changelist completely; `map` holds one
  fingerprint per elidable unit. Only this state permits elision.
- `Disabled` — the previous turn was incomplete in any of the ways invariant 3 lists (a
  truncation, a skipped or cancelled changelist, an over-cap add body). The next turn full-
  captures. Recorded rather than a partial `Full` so a partial baseline can never be eluded
  against.
- absent — first turn, a git session, or a record predating the field.

Do **not** reuse `SessionStore`'s existing "retain the old `Option` if the new one is `None`"
behaviour (`src/session.rs:169`, `:173`): that would preserve a stale `Full(map)` across a
turn that should have disabled elision. The three-state value is written explicitly every
turn.

Bounds: a concrete **entry cap** and **total byte cap** on the map; past either, the turn
records `Disabled` (full capture next turn) with a stated warning — no silent truncation. The
map is Perforce-only, `#[serde(default)]`, stored beside the git `head_sha`/`base_sha` on
`SessionRecord` / `TurnFacts`, and invalidated whenever the persisted **schema version** does
not match the running server's.

## Reviewer-facing rendering

A framing note up front, mirroring git's follow-up preamble (`render` in `src/vcs/git.rs`):
that the reviewer saw the full changelist(s) earlier in this session, that unchanged files
are collapsed to one line below, that only what moved is shown in full, and that it should
re-check its earlier findings against the changes. Then the per-file lines from the
transition table. This replaces the current `resumed_capture_note` in `tools.rs`, which today
only warns that a pending changelist's contents can move between turns.

## Change surface

- **`src/vcs/perforce.rs`** — the bulk. Restructure `render` and the `Segment` / `DiffSection`
  path so each evidence unit (diff section *and* add body) is addressable and individually
  shown-or-collapsed; capture the base revision from the diff header; compute fingerprints
  from retained raw output before the rendering budget; the resume-delta decision as its own
  backend-specific function, mirroring the discipline of git's `incremental_base`.
- **CNG digest helper** — a small `unsafe` FFI wrapper over `BCryptHashData` (SHA-256), with a
  runtime "digest unavailable → disable elision" path. VCS-neutral, so it belongs beside the
  other shared primitives.
- **`src/session.rs`** — the `Full/Disabled` baseline value and its schema version on
  `SessionRecord` and `TurnFacts`, `#[serde(default)]`.
- **`src/vcs/mod.rs`** and **`src/tools.rs`** — thread the prior baseline into `capture` the
  way `GitResumeBaseline` is threaded now, and return the new baseline out like `head_sha`;
  prefer a unified `resume` enum over two backend-specific `Option`s.
- **config** — reuse the existing `resume_incremental_diff` switch.

## Test plan

- **Unit:** fingerprint stability across a simulated restart (serialize → deserialize → still
  matches); a shared depot path across two changelists keyed distinctly (finding 1); each
  transition (unchanged / modified / added / removed / moved / type-flip); base-rev change
  with identical workspace content → **not** elided (finding 5); binary same-type edit →
  **not** elided (finding 2); every truncation source → unit non-elidable and "removed"
  suppressed on an incomplete inventory (finding 3); an incomplete turn records `Disabled`,
  not `Full` (finding 6); digest-unavailable → elision disabled (finding 4); schema-version
  mismatch invalidates the map.
- **Golden render snapshots** for the resumed Perforce prompt (like
  `render_output_is_byte_for_byte_stable` in `src/vcs/mod.rs`): a mixed turn with some units
  collapsed, one modified, one added, one removed.
- **Live smoke** (`smoke.ps1 -Reviewer codex`) against a real pending changelist edited
  between two turns — verifies the round trip, that turn 2 is billed materially less, and the
  `p4 diff -du` output for binary, move/delete, and shelved-add cases that cannot be validated
  without a real server. Costs tokens; run when touching this path.

## Optional later layer (not v1)

Skip the `p4 diff` on unchanged files via `p4 fstat` (have-rev plus a workspace content hash
we compute by reading the file) or shelf digests, comparing metadata *including base rev*
before diffing. Cuts capture wall-clock too, not just tokens. Kept out of the first cut: the
token win — the actual "inordinate usage" the caller is billed for — is fully delivered
without it, and it adds a second correctness surface.

## Open decisions / risks

- **Unified `resume` parameter vs two backend-specific `Option`s** on `capture` — leaning
  unified enum.
- **CNG FFI vs a vendored SHA-256** — CNG is OS-vetted and dependency-free but adds `unsafe`
  FFI; a vendored SHA-256 is pure-safe-Rust and deterministic but is not an independently
  vetted implementation. Leaning CNG for the "vetted" property on a Windows-only tool.
- **Rename detection fidelity** — `move/add` + `move/delete` pairing is later polish; v1
  treats them as add + delete (safe, more verbose).
- **Dogfooding** — this change touches `session.rs` serialization and the capture seam; the
  cross-review gate reviewer should be pointed at the completeness invariants, the digest
  boundary, and base-rev capture.

## Review history

- **Round 1 (Codex, gpt-5.6-luna, effort=max):** REQUEST CHANGES, six findings (five major,
  one minor). All accepted and folded in: changelist number added to the identity key (F1);
  fingerprint redefined over the whole evidence unit with binary/delete made non-elidable
  (F2); completeness extended to the 8 MiB process cap and `describe`/`opened`/`where`
  truncation, with "removed" gated on a complete inventory (F3); FNV replaced with a
  collision-resistant CNG SHA-256 digest plus schema version, and "disable elision if no
  digest" (F4); base revision captured from the diff header rather than a separate `p4 have`,
  with unknown → non-elidable (F5); explicit `Full/Disabled` baseline state and the corrected
  pre- vs post-capture failure timeline (F6).
