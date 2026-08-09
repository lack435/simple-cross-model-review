# Perforce resume delta — design

Status: **proposal, not yet implemented.** This document is the plan under review; nothing
in the code implements it yet. It has been through two rounds of cross-model review (see
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
base *and* the prior commit is an ancestor of the current HEAD. A Perforce changelist gives
none of that: a **pending** changelist mutates in place, a **shelved** one can be replaced
(`p4 shelve -f`), and a **submitted** one is immutable (so a re-review shows a byte-for-byte
identical diff every turn — nothing to delta, everything to elide). The git delta's
precondition is absent for the mutable cases and moot for the immutable one. The *purpose* —
don't re-spend the reviewer's context on bytes it already saw — is still achievable, through
a different mechanism.

## Mechanism: per-file elision keyed on the reviewer's own view

On a genuine resume, for each **elidable evidence unit** in the captured change, compare a
**fingerprint of the exact evidence the reviewer was shown last turn** against this turn's.
Units whose fingerprint matches, and whose surrounding inventory proves they are still
present and complete, collapse to a single line; everything else is shown in full and
explicitly labelled.

The claim this makes to the reviewer is **byte-identity** — "this is identical to what you
already saw" — which is directly verifiable and needs no ancestry proof.

Deliberately **coarse**: a unit that changed at all is re-sent whole, not diffed against its
own last-shown state.

### What is elidable, and what is not

Only **token-heavy textual content** participates in elision:

- **Elidable:** a file's textual `p4 diff` section, and a **pending** added file's body
  (which is not in `p4 diff` — it is read from the workspace and rendered separately at
  `src/vcs/perforce.rs:565`, `:628`, `:951`).
- **Never elidable — rendered in full every turn:** binary files, deletes, files opened but
  restored to depot content (no diff section), unreadable/omitted content, and every
  metadata note. These are already one-liners, so eliding them saves nothing and only widens
  the attack surface. The backend runs `p4 diff -du` **without** `-t`
  (`src/vcs/perforce.rs:595`), so a same-type binary *edit* produces identical *empty*
  textual evidence on both turns; making binary non-elidable removes that false-elision
  hazard outright.
- **Server-side added content (submitted/shelved):** the `describe` and shelved commands do
  **not** pass `-a` (`src/vcs/perforce.rs:308`, `:715`), which Perforce requires to emit
  added-file bodies, so that content is not captured today. In v1 it is therefore classified
  **omitted / non-elidable**, not treated as elidable add-body evidence. Passing `-a` and
  fingerprinting those bodies as their own units is a later decision, noted under open
  questions.

## Complete case coverage

| CL type | Between-turn behaviour | Result |
|---|---|---|
| **Pending** | mutates in place (edit reworked, reverted, restored-in-place, newly opened) | per-file elision of text diffs and pending add bodies — the primary win |
| **Shelved** (`include_shelved`) | shelf can be replaced; a file may be opened *and* shelved with different content | fingerprinted in its **own basis namespace** |
| **Submitted** | immutable | every unit matches → the whole CL collapses. Falls out for free |

## Identity and inventory

### Evidence-unit key

`(changelist number, basis, evidence kind, depot path)`:

- **changelist number** (round-1 F1): a depot path can appear in several requested
  changelists, each rendered as its own `Segment` (`src/vcs/perforce.rs:217`, `:869`).
- **basis**: pending/workspace, shelved, or submitted — the same file has distinct evidence
  in each.
- **evidence kind** (round-2): `text-diff` vs `pending-add-body` vs `non-elidable-note`. A
  path that changes kind between turns (e.g. `edit` reverted then re-added) must register as a
  mismatch, not a coincidental key hit.
- **depot path**: the stable Perforce identity (workspace/local paths move with the client
  view), canonicalised as the backend already does.

### The persisted state is an inventory, not just fingerprints

The map holds **every** evidence-unit key present last turn, each tagged with its state —
`present/elidable` (carries a fingerprint), `present/no-diff` (opened but no textual change),
or `present/non-elidable` — plus its action and type. Fingerprints exist only for the
elidable entries, but the full key set is needed so this turn can classify each transition
(round-2 F1):

- a key gone from this turn's capture → **removed** — but **only asserted when both the prior
  and current inventories are known complete** (see completeness); a truncated `opened` /
  `describe` listing could otherwise report a still-present file as removed.
- a key present but now `no-diff` where it was a diff before (an edit restored to depot
  content, still open) → explicitly rendered as *"restored to its depot revision; the diff
  you saw earlier no longer applies"*, never silently dropped and never elided.
- kind/action/type change → shown in full.

## Correctness invariants

### 1. Fingerprint the canonical rendered unit, committed only when fully shown

The fingerprint is over the **canonical bytes of the evidence unit as the reviewer receives
it** — not the raw `p4` output, because the submitted and shelved paths parse and reconstruct
their sections (`src/vcs/perforce.rs:1334`, `:1524`, `:1551`), so "raw output" and "shown
evidence" are not the same bytes (round-2 F2). Encoding, line-ending normalisation, and
keyword expansion are already baked into those shown bytes.

The fingerprint is computed **provisionally** during capture and **committed to the baseline
only for a unit that was actually rendered completely this turn** (round-2 F4). A unit cut by
the `MAX_DIFF_BYTES` budget, or never rendered because an earlier unit exhausted it, is
discarded from the baseline rather than recorded as if the reviewer had seen it. This is what
lets elision relieve the prompt cap without ever eliding against bytes the reviewer never
got: hash early, commit late.

### 2. Base / comparator identity is defined per basis

"Read the base revision from the diff header" is not one rule (round-2 F2). Each basis has its
own comparator identity, and where it cannot be established from the captured evidence the
unit is marked **unknown → non-elidable**:

- **Submitted** — immutable. `p4 describe` diffs a file against its previous revision; the
  `#N` in the affected-file header is the *changed* revision. `(CL, path, #N)` fully and
  permanently identifies the unit, so a fingerprint over its diff bytes is stable by
  construction — no have-revision subtlety applies.
- **Shelved** — the shelved diff's previous-revision basis, captured from `describe -S`. A
  shelf can be replaced, so content is fingerprinted; the basis revision is part of the key.
- **Pending** — the live case. `p4 diff` compares the workspace file against the revision
  currently open for edit, and `p4 have` can move after a sync even for an open file, so a
  byte-identical workspace can yield a different diff. The comparator revision must be taken
  from the **same** diff evidence, never a separate `p4 have` call that might report a
  revision the diff did not use; if it is not recoverable there, the unit is non-elidable.

Diff-format inputs (the `p4 diff` flags, diff algorithm, whitespace handling) are folded into
the **schema version** (invariant 3), so changing how `p4` is invoked invalidates prior
fingerprints rather than silently comparing across two formats.

### 3. A persisted fingerprint needs a collision-resistant, versioned digest

The diff is **attacker-influenced content from a repository this project does not trust**. A
non-cryptographic fixed hash (FNV) can be *deliberately* collided by an author who controls
both revisions, rendering a changed file as "unchanged" and slipping it past the merge gate
(round-1 F4). So the digest is **Windows CNG `BCryptHashData` / SHA-256** via FFI —
dependency-free on this Windows-only tool, an OS-vetted implementation. **Every CNG failure
discards the candidate baseline** (elision disabled) rather than falling back to a weak hash;
a keyed hash is unnecessary once the digest is collision-resistant. `DefaultHasher` is
separately unusable (not stable across Rust releases, and this value is persisted).

Store `(length, digest, algorithm + schema version)`. The **schema version** invalidates the
whole map when the digest algorithm, the canonicalisation, or the `p4` invocation changes.

### 4. Completeness is per unit *and* per inventory, and covers every shortfall

Elision requires that the unit was shown completely **and** that the inventory it sits in is
complete. Sources that mark a unit or a whole changelist non-elidable (round-2 F4, extending
round-1 F3):

- the `MAX_DIFF_BYTES` (400 KB) rendering budget;
- the **8 MiB process-output cap** (`src/reviewer/mod.rs:31`, `:289`) — this retains only a
  *prefix*, so the correct response is to **render the available prefix without elision and
  preserve the incompleteness**, not to claim a "full capture" that cannot recover the missing
  suffix (unless per-file commands are reissued, out of scope for v1);
- truncated `describe` / `opened` / `where` output (`src/vcs/perforce.rs:309`, `:1120`,
  `:1147`, `:1166`);
- a **`where` failure**, which `where_of` currently turns into an *empty map* — indistinguishable
  from "no mappings" — so it must be treated as unknown path resolution, hence non-elidable
  and inventory-incomplete;
- failed or timed-out `p4` commands, and malformed/ambiguous diff sections;
- the per-file and total add-body caps (`MAX_UNTRACKED_*`);
- a skipped changelist or a cancelled capture.

"Complete inventory" (the file *list* is whole, so "removed" can be asserted) and "complete
unit" (this file's *content* was fully shown, so it can be elided) are tracked separately.

### 5. The resume binding must cover capture mode and workspace, not just the CL set

The session binds only the `changes` set today (`src/session.rs:46`), which `resume_block`
checks. That is not enough (round-2 F3): `include_shelved` is neither persisted nor checked,
so a session can show a shelf on turn 1 and omit it on turn 2 while the shelf stays in the
reviewer's context with nothing to explain it is out of scope. Persist and check on resume:

- `include_shelved`;
- the **resolved Perforce client / capture identity** (a changed client view remaps paths and
  content).

A mismatch **refuses the resume** (or forces a fresh full capture); basis namespacing prevents
key collisions but does nothing for this mode mismatch.

### 6. Gated on a genuine resume; baseline is the last *successfully parsed* turn

Elision consults the map only when the reviewer provably still holds the prior turn:

- **Pre-capture** (the only place a full-capture fallback is *chosen*): `fresh: true` clears
  `prior` (`src/tools.rs:179`); a stale/mismatched session is refused at `resume_block` before
  capture.
- **Post-capture**: `SESSION_NOT_FOUND` is detected only after capture, when the reviewer CLI
  rejects the resume (`src/tools.rs:790`, `:831`); it forgets the session but cannot be a
  capture-time gate.

The baseline is defined as **the last successfully parsed turn**, not "the previous
invocation" (round-2 F6): a failed turn — including a failed `fresh` — does not call
`record_turn` (`src/tools.rs:824`, `:1110`), so the prior record persists untouched. That is
safe precisely because the retained state came from a real parsed turn against the same
binding; the definition just has to say so.

## State model

An explicit three-state value on the session, written explicitly every turn (round-1/2 F6),
never via `SessionStore`'s "retain the old `Option` if the new is `None`" behaviour
(`src/session.rs:169`, `:173`), which would preserve a stale baseline across a turn that
should have disabled elision:

- `Full(inventory)` — the previous parsed turn captured every changelist completely;
  `inventory` holds one entry per evidence unit (with fingerprints on the elidable ones).
  Only this state permits elision.
- `Disabled` — the previous parsed turn was incomplete in any way invariant 4 lists, or
  exceeded the entry/byte caps. The next turn full-captures.
- absent — first turn, a git session, or a record predating the field.

Bounds: concrete **entry** and **total-byte** caps; past either, the turn records `Disabled`
with a stated warning. The value is Perforce-only, `#[serde(default)]`, stored beside the git
`head_sha`/`base_sha`, carrying its **schema version**; a version mismatch on read invalidates
it to absent.

## Reviewer-facing rendering

A framing note up front, mirroring git's follow-up preamble (`render` in `src/vcs/git.rs`):
the reviewer saw the full changelist(s) earlier in this session, unchanged files are collapsed
to one line, only what moved is shown in full, and it should re-check its earlier findings.
Then the per-file transition lines, including the explicit *removed*, *restored-to-depot*, and
*shelf-no-longer-in-scope* notes. This replaces the current `resumed_capture_note` in
`tools.rs`.

## Change surface

- **`src/vcs/perforce.rs`** — the bulk. Make each evidence unit (diff section, pending add
  body, and the non-elidable notes) addressable and individually shown-or-collapsed; capture
  the per-basis comparator identity; compute provisional fingerprints over canonical unit
  bytes and commit them only for fully-rendered units; the resume-delta decision as its own
  backend-specific function.
- **CNG digest helper** — a small `unsafe` FFI wrapper over `BCryptHashData` (SHA-256) with a
  runtime "digest unavailable → disable elision" path; VCS-neutral, beside the shared
  primitives.
- **`src/session.rs`** — the `Full(inventory)`/`Disabled` value, its schema version, and the
  extended resume binding (`include_shelved`, client identity) on `SessionRecord`/`TurnFacts`,
  `#[serde(default)]`; `resume_block` extended to check them.
- **`src/vcs/mod.rs`** and **`src/tools.rs`** — thread the prior baseline into `capture` like
  `GitResumeBaseline`, return the new one out like `head_sha`; prefer a unified `resume` enum.
- **config** — `resume_incremental_diff` is documented **git-only** (`src/config.rs:324`);
  either widen that contract to be backend-agnostic or add a separate Perforce switch (round-2
  F6).

## Test plan

- **Unit:** fingerprint stability across a simulated restart; shared depot path across two
  changelists keyed distinctly (F1); each transition including *restored-to-depot/no-diff* and
  kind-change (F1/round-2); base-rev change with identical workspace content → not elided (F5);
  binary same-type edit → not elided (F2); per-basis comparator identity; every completeness
  source including `where`-failure and 8 MiB prefix truncation → non-elidable, and "removed"
  suppressed on an incomplete inventory (F3/F4); provisional hash discarded for a
  budget-truncated unit (F4); `include_shelved`/client change → resume refused (F3); incomplete
  or failed turn leaves the last parsed `Full`/`Disabled` intact (F6); digest-unavailable →
  elision disabled (F4); schema-version mismatch invalidates the map.
- **Golden render snapshots** for the resumed Perforce prompt (like
  `render_output_is_byte_for_byte_stable` in `src/vcs/mod.rs`): a mixed turn with collapsed,
  modified, added, removed, and restored-to-depot units.
- **Live smoke** (`smoke.ps1 -Reviewer codex`) against a real pending changelist edited between
  turns — verifies the round trip, that turn 2 is billed materially less, and the `p4 diff -du`
  output for binary, move/delete, and shelved cases that cannot be validated without a real
  server.

## Optional later layer (not v1)

Skip the `p4 diff` on unchanged files via `p4 fstat` (have-rev plus a workspace content hash)
or shelf digests, comparing metadata *including base rev* before diffing. Cuts capture
wall-clock too. Kept out of the first cut: the token win is fully delivered without it.

## Open decisions / risks

- **Server-side added bodies** — pass `-a` to `describe`/shelved commands and fingerprint those
  bodies as units, or keep them omitted/non-elidable as v1 does. Adds capture cost and caps.
- **8 MiB prefix recovery** — reissuing per-file commands to recover a truncated suffix (so a
  huge changelist can still elide) vs v1's "render the prefix, disable elision."
- **Unified `resume` parameter vs two backend-specific `Option`s** on `capture` — leaning
  unified enum.
- **CNG FFI vs a vendored SHA-256** — CNG is OS-vetted and dependency-free but adds `unsafe`
  FFI; a vendored SHA-256 is pure-safe-Rust but not independently vetted. Leaning CNG.
- **Rename detection** — `move/add` + `move/delete` pairing is later polish; v1 treats them as
  add + delete.
- **Dogfooding** — touches `session.rs` serialization, the resume binding, and the capture
  seam; the gate reviewer should be pointed at the completeness/inventory invariants, the
  digest boundary, per-basis comparator identity, and the extended resume binding.

## Review history

- **Round 1 (Codex, gpt-5.6-luna, max):** REQUEST CHANGES, six findings. All accepted:
  changelist number in the key (F1); fingerprint over the whole evidence unit, binary/delete
  non-elidable (F2); completeness beyond `MAX_DIFF_BYTES` (F3); collision-resistant CNG SHA-256
  + schema version (F4); base-rev from the diff, unknown→non-elidable (F5); explicit
  `Full`/`Disabled` state and corrected failure timeline (F6).
- **Round 2 (same session):** F1 and F4 resolved, F2 substantially resolved. REQUEST CHANGES on
  the deeper issues, all accepted: persist a full prior **inventory** (not just fingerprints)
  to classify removed / restored-to-depot / kind-change, with evidence kind in the key;
  **per-basis** comparator identity (submitted `#N` immutable, shelved `describe -S` basis,
  pending open-rev) and hash the canonical *rendered* unit, not raw output; provisional hashes
  committed only for fully-rendered units; extend the resume binding to `include_shelved` and
  client identity; 8 MiB truncation renders the prefix with elision disabled; `where`-failure /
  timeouts / malformed sections added to completeness; server-side added bodies need `-a` so
  they are omitted/non-elidable in v1; baseline defined as the last *successfully parsed* turn;
  `resume_incremental_diff` contract is git-only and must be widened or a Perforce switch added.
