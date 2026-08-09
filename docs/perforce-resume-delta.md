# Perforce resume delta — design

Status: **proposal, not yet implemented.** This document is the plan under review; nothing
in the code implements it yet. It has been through three rounds of cross-model review (see
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

Git deltas along an **immutable, ancestry-checkable commit range**. A Perforce changelist
gives none of that: a **pending** changelist mutates in place, a **shelved** one can be
replaced (`p4 shelve -f`), and a **submitted** one is immutable. The git delta's precondition
is absent for the mutable cases and moot for the immutable one. The *purpose* — don't
re-spend the reviewer's context on bytes it already saw — is still achievable, through a
different mechanism.

## Mechanism: per-file elision keyed on the reviewer's own view

On a genuine resume, for each **elidable evidence unit**, compare a **fingerprint of the exact
evidence the reviewer was shown last turn** against this turn's. Units whose fingerprint
matches, and whose surrounding inventory proves they are still present and complete, collapse
to a single line; everything else is shown in full and explicitly labelled.

The claim to the reviewer is **byte-identity with what it was shown** — directly verifiable,
no ancestry proof needed. Deliberately **coarse**: a unit that changed at all is re-sent
whole.

### What is elidable, and what is not

Only **token-heavy textual content** participates in elision:

- **Elidable:** a file's textual `p4 diff` section, and a **pending** added file's body (read
  from the workspace and rendered separately at `src/vcs/perforce.rs:565`, `:628`, `:951`).
- **Never elidable — rendered in full every turn:** binary files, deletes, files opened but
  restored to depot content (no diff section), unreadable/omitted content, content that
  required **lossy UTF-8 decoding** (see invariant 1), and every metadata note. These are
  already one-liners, so eliding them saves nothing and only widens the attack surface.
  `p4 diff -du` runs **without** `-t` (`src/vcs/perforce.rs:595`), so a same-type binary edit
  yields identical *empty* textual evidence on both turns; making binary non-elidable removes
  that hazard outright.
- **Server-side added content (submitted/shelved):** `describe` and the shelved command omit
  `-a` (`src/vcs/perforce.rs:308`, `:715`), which Perforce requires to emit added bodies, so
  that content is not captured today and is classified **omitted / non-elidable** in v1.

## Complete case coverage

| CL type | Between-turn behaviour | Result |
|---|---|---|
| **Pending** | mutates in place (edit reworked, reverted, restored-in-place, newly opened) | per-file elision of text diffs and pending add bodies — the primary win |
| **Shelved** (`include_shelved`) | shelf can be replaced | fingerprinted in its own basis namespace, against an authoritative shelf ledger |
| **Submitted** | immutable | **all elidable units** collapse. Non-elidable notes still render |

Note the framing: a re-review collapses *all elidable units*, not "the whole changelist" —
binary/deleted/omitted evidence is still shown every turn. The reviewer-facing note says
exactly that, so the reviewer never reads a collapsed prompt as "nothing here changed."

## Identity and inventory

### Evidence-unit key and entry

Key: `(changelist number, basis, evidence kind, depot path)` —

- **changelist number** (R1-F1): a depot path can appear in several requested changelists,
  each its own `Segment` (`src/vcs/perforce.rs:217`, `:869`).
- **basis**: pending/workspace, shelved, or submitted.
- **evidence kind**: `text-diff` / `pending-add-body` / `non-elidable-note`; a path that
  changes kind registers as a mismatch, not a coincidental key hit.
- **depot path**: the stable Perforce identity, canonicalised as the backend already does.

Each entry carries: state (`present/elidable` with fingerprint, `present/no-diff`, or
`present/non-elidable`), action, type, and — for elidable entries — the **comparator ID** (the
revision the diff was taken against; see invariant 2). The comparator ID is also folded into
the hashed canonical input, so a base-revision change alone busts the fingerprint even when
the rendered body is identical (R3-F1). A missing or ambiguous comparator ID makes the unit
non-elidable.

### The persisted state is an inventory, not just fingerprints

The map holds **every** evidence-unit key present last turn (R2-F1), so this turn can classify
each transition:

- key gone this turn → **removed** — asserted **only when both inventories are known
  complete** (see completeness); a truncated listing could otherwise fake a removal.
- key present but now `no-diff` where it was a diff (an edit restored to depot content, still
  open) → rendered as *"restored to its depot revision; the diff you saw no longer applies"* —
  never silently dropped, never elided.
- kind/action/type change → shown in full.
- **matched (elided) unit** → the collapsed line is *not* hashed; instead the prior full
  fingerprint is **carried forward** into the new inventory after the current *unelided*
  candidate is hashed and confirmed equal, and the entry stays in the inventory (R3-F5). An
  elided unit is therefore still fully represented for the next turn's classification.

## Correctness invariants

### 1. Fingerprint the canonical rendered unit; commit only when fully shown or carried forward

The fingerprint is over the **canonical bytes of the evidence unit as the reviewer receives
it** — not raw `p4` output, since submitted/shelved reconstruct their sections
(`src/vcs/perforce.rs:1334`, `:1524`, `:1551`) (R2-F2). It is computed **provisionally** during
capture and enters the baseline **only** when the unit was either rendered completely this
turn, or matched-and-carried-forward per the rule above (R3-F5). A unit cut by the
`MAX_DIFF_BYTES` budget, or never reached, is discarded from the baseline — hash early, commit
late.

Because the reviewer only ever sees text decoded with `String::from_utf8_lossy`
(`src/vcs/perforce.rs:668`, `src/reviewer/mod.rs:548`), distinct raw bytes can map to the same
displayed text. Hashing displayed bytes correctly protects *what was shown*, but not the
underlying file identity. So any unit whose content **required lossy decoding is classified
non-elidable** (R3-F6); storing an additional raw-byte digest is the alternative if that
proves too conservative in practice.

### 2. Comparator identity is defined per basis and persisted

Each basis has its own comparator identity; where it cannot be established from the captured
evidence the unit is non-elidable (R2-F2, R3-F1):

- **Submitted** — immutable. `describe` diffs against the previous revision; the `#N` in the
  affected-file header is the *changed* revision, and `(CL, path, #N)` identifies the unit
  permanently, so a fingerprint over its diff bytes is stable by construction.
- **Shelved** — `describe -S`'s previous-revision basis. A shelf can be replaced, so content
  is fingerprinted and the basis revision is the comparator ID.
- **Pending** — the live case. `p4 diff` compares the workspace file against the revision open
  for edit, and `p4 have` can move after a sync even for an open file, so a byte-identical
  workspace can yield a different diff. The comparator revision is taken from the **same** diff
  evidence, never a separate `p4 have`; if not recoverable there, the unit is non-elidable.

The comparator ID lives in each elidable entry **and** in the hashed input (invariant 1), so
neither a dropped `#N` on reconstruction nor a silent base move can elide a changed diff.

### 3. A persisted fingerprint needs a collision-resistant, versioned digest

The diff is **attacker-influenced content from an untrusted repository**. FNV can be
*deliberately* collided by an author who controls both revisions, slipping a changed file past
the merge gate as "unchanged" (R1-F4). The digest is **Windows CNG `BCryptHashData` /
SHA-256** via FFI — dependency-free on this Windows-only tool, OS-vetted. **Every CNG failure
discards the candidate baseline** (elision disabled). `DefaultHasher` is separately unusable
(not stable across Rust releases; persisted). Store `(length, digest, algorithm + schema
version)`; the **schema version** invalidates the map when the algorithm, the canonicalisation,
or the `p4` invocation (including diff flags) changes.

### 4. Completeness is per unit *and* per inventory, from trustworthy ledgers

Elision requires the unit was shown completely **and** its inventory is complete. Sources that
mark a unit or a whole changelist non-elidable / inventory-incomplete (R1-F3, R2-F4, R3-F3,
R3-F7):

- `MAX_DIFF_BYTES` (400 KB) rendering budget;
- the **8 MiB process-output cap** (`src/reviewer/mod.rs:31`, `:289`), which retains only a
  *prefix* — render the prefix without elision and preserve the incompleteness, do **not**
  claim a full capture that cannot recover the suffix;
- truncated `describe` / `opened` / `where` output (`src/vcs/perforce.rs:309`, `:1120`,
  `:1147`, `:1166`);
- a **`where` failure**, which `where_of` turns into an *empty map* indistinguishable from "no
  mappings" — treated as unknown path resolution → non-elidable, inventory incomplete;
- failed/timed-out `p4` commands, and malformed/ambiguous diff sections;
- the per-file and total add-body caps (`MAX_UNTRACKED_*`);
- the **description cap** (`cap_desc` / `MAX_DESC_BYTES`, `src/vcs/perforce.rs:998`) and the
  **omission-note cap** (`MAX_OMISSION_NOTES`, `src/vcs/shared.rs:194`): when metadata or notes
  are suppressed, the turn cannot record `Full`;
- a skipped changelist or a cancelled capture.

For the **shelved inventory specifically**, deriving keys from parsed `-du` diff sections alone
is not a trustworthy ledger — omitted-add, binary, no-diff, and delete files may emit no
section (R3-F3). Take an authoritative shelf listing (`p4 describe -s -S`, tagged), cross-check
it against the diff sections, and mark the inventory incomplete on any disagreement.

"Complete inventory" (the file *list* is whole → "removed" can be asserted) and "complete unit"
(this file's *content* was fully shown → it can be elided) are tracked separately.

### 5. The resume binding covers backend, capture mode, and workspace — checked where each is knowable

The session binds only `changes` today (`src/session.rs:46`), checked pre-capture by
`resume_block`. That is insufficient (R2-F3, R3-F2):

- **Backend identity.** `SessionRecord` has no VCS field, so a Git record (`changes == None`)
  can satisfy the Perforce-only binding logic (`src/tools.rs:1221`). Add a backend/VCS field
  and **reject cross-Git/Perforce resumes** at `resume_block` (pre-capture — it is knowable
  from config there).
- **`include_shelved`.** Persist it; a session that showed a shelf on turn 1 and omits it on
  turn 2 must not leave that shelf silently in scope.
- **Resolved Perforce client / capture identity.** The client is resolved *inside* capture
  (`src/vcs/perforce.rs:78`), so `resume_block` cannot see it (R3-F2). Two acceptable shapes:
  a lightweight Perforce **identity preflight** (resolve client) before `resume_block`, so a
  mismatch refuses consistently with the `changes` check; or — v1's minimum — the backend
  **validates client + `include_shelved` before it consults the prior inventory** and, on
  mismatch, takes a full capture with elision disabled. Either way the inventory is never
  consulted until identity is confirmed.

### 6. The baseline is the last *persisted* successfully-parsed turn

Elision consults the map only on a genuine resume: `fresh: true` clears `prior`
(`src/tools.rs:179`); a stale/mismatched session is refused at `resume_block`;
`SESSION_NOT_FOUND` is post-capture (`src/tools.rs:790`, `:831`) and forgets the session.

A failed turn — including a failed `fresh` — does not `record_turn` (`src/tools.rs:824`,
`:1110`), so the prior record persists untouched, which is safe because it came from a real
parsed turn against the same binding.

But a parsed turn whose **state-write itself fails** is *not* safe (R3-F4): the review is
delivered even when `record_turn` fails (`src/tools.rs:1110`, `:1136`), so the reviewer has
advanced while the persisted inventory has not — a later turn could elide against a superseded
baseline and hide a transition. So a `record_turn` failure for a Perforce session carrying a
baseline **poisons the session**: forget/remove the mapping (as `SESSION_NOT_FOUND` already
does) so the next call cannot resume the stale inventory and must go `fresh`. The baseline is
thus the last *persisted* parsed turn, not merely the last parsed one.

## State model

An explicit three-state value on the session, written explicitly every turn (never via
`SessionStore`'s "retain old `Option` if new is `None`" behaviour, `src/session.rs:169`,
`:173`):

- `Full(inventory)` — the previous persisted turn captured every changelist completely and
  suppressed no metadata/notes; only this state permits elision.
- `Disabled` — the previous persisted turn was incomplete in any way invariant 4 lists, or
  exceeded the entry/byte caps; the next turn full-captures.
- absent — first turn, a git session, a cross-backend/poisoned record, or a record predating
  the field.

Bounds: concrete **entry** and **total-byte** caps; past either the turn records `Disabled`
with a stated warning. Perforce-only, `#[serde(default)]`, stored beside the git
`head_sha`/`base_sha`, carrying its **schema version** and **backend identity**; a version or
backend mismatch on read invalidates it to absent.

## Reviewer-facing rendering

A framing note up front, mirroring git's follow-up preamble (`render` in `src/vcs/git.rs`):
the reviewer saw the full changelist(s) earlier in this session, **all elidable files** are
collapsed to one line, only what moved is shown in full, non-elidable evidence is still shown,
and it should re-check its earlier findings. Then the per-file transition lines, including the
explicit *removed*, *restored-to-depot*, and *shelf-no-longer-in-scope* notes. This replaces
the current `resumed_capture_note` in `tools.rs`.

## Change surface

- **`src/vcs/perforce.rs`** — the bulk: addressable evidence units (diff section, pending add
  body, non-elidable notes); per-basis comparator capture; authoritative shelf ledger
  (`describe -s -S`) cross-checked against diff sections; provisional fingerprints over
  canonical unit bytes committed only when fully shown or carried forward; in-capture
  client/`include_shelved` validation before consulting the inventory; the resume-delta
  decision as its own function.
- **CNG digest helper** — a small `unsafe` FFI wrapper over `BCryptHashData` (SHA-256) with a
  runtime "digest unavailable → disable elision" path; VCS-neutral, beside the shared
  primitives.
- **`src/session.rs`** — the `Full(inventory)`/`Disabled` value, schema version, **backend
  identity**, and the extended binding (`include_shelved`, client identity) on
  `SessionRecord`/`TurnFacts`, `#[serde(default)]`; `resume_block` extended to reject
  cross-backend resumes and check the mode where knowable.
- **`src/tools.rs`** — poison the session on `record_turn` failure for a baseline-carrying
  Perforce session; thread the prior baseline into `capture` like `GitResumeBaseline`.
- **`src/vcs/mod.rs`** — prefer a unified `resume` enum over two backend-specific `Option`s.
- **config** — **decision:** widen `resume_incremental_diff` (`src/config.rs:324`) to a
  backend-agnostic contract (one switch governs incremental resume for whichever backend is
  active) and update its documentation, rather than adding a second Perforce-only switch.

## Test plan

- **Unit:** fingerprint stability across a simulated restart; shared depot path across two
  changelists keyed distinctly (F1); every transition including *restored-to-depot/no-diff*,
  kind-change, and an **elided unit carried forward then returning changed** (R3-F5); per-basis
  comparator identity and a base-rev change with identical body → not elided (R3-F1/F2); binary
  same-type edit → not elided (F2); lossy-decoded content → non-elidable (R3-F6); every
  completeness source including `where`-failure, 8 MiB prefix truncation, and the
  description/omission caps → non-elidable, "removed" suppressed on incomplete inventory
  (F3/F4/R3-F3/R3-F7); shelved authoritative-ledger disagreement → incomplete (R3-F3);
  provisional hash discarded for a budget-truncated unit (R2-F4); `include_shelved`/client
  change → not elided (R2-F3/R3-F2); cross-backend record rejected (R3-F2); **`record_turn`
  failure poisons the session** so the next turn goes fresh (R3-F4); digest-unavailable →
  disabled (F4); schema/backend-version mismatch invalidates the map.
- **Golden render snapshots** for the resumed Perforce prompt: a mixed turn with collapsed,
  modified, added, removed, and restored-to-depot units.
- **Live smoke** (`smoke.ps1 -Reviewer codex`) against a real pending changelist edited between
  turns — verifies the round trip, that turn 2 is billed materially less, and the `p4 diff -du`
  / `describe -s -S` output for binary, move/delete, and shelved cases that cannot be validated
  without a real server.

## Optional later layer (not v1)

Skip the `p4 diff` on unchanged files via `p4 fstat` (have-rev plus a workspace content hash)
or shelf digests, comparing metadata *including base rev* before diffing. Cuts capture
wall-clock too. Kept out of the first cut: the token win is fully delivered without it.

## Open decisions / risks

- **Server-side added bodies** — pass `-a` to `describe`/shelved and fingerprint those bodies,
  or keep them omitted/non-elidable as v1 does.
- **8 MiB prefix recovery** — reissue per-file commands to recover a truncated suffix vs v1's
  "render the prefix, disable elision."
- **Client-identity check placement** — preflight before `resume_block` vs in-capture
  full-capture fallback (both specified in invariant 5).
- **CNG FFI vs a vendored SHA-256** — leaning CNG for the OS-vetted property.
- **Rename detection** — `move/add` + `move/delete` pairing is later polish.
- **Dogfooding** — touches `session.rs` serialization, the resume binding, and the capture
  seam; point the gate reviewer at the completeness/inventory invariants, the digest boundary,
  per-basis comparator identity, the extended/backend-aware binding, and the write-failure
  poisoning.

## Review history

- **Round 1 (Codex, gpt-5.6-luna, max):** REQUEST CHANGES, six findings — all accepted (key
  changelist number; whole-evidence-unit fingerprint with binary non-elidable; completeness
  beyond `MAX_DIFF_BYTES`; collision-resistant CNG SHA-256 + schema version; per-basis base-rev
  with unknown→non-elidable; explicit `Full`/`Disabled` state).
- **Round 2:** F1 and F4 resolved, F2 substantially. REQUEST CHANGES, all accepted (full prior
  inventory with evidence kind; per-basis comparator identity and hash-rendered-not-raw with
  provisional-commit; bind `include_shelved` + client; 8 MiB prefix handling and more
  completeness sources; server-side added bodies need `-a` → omitted in v1; baseline = last
  successfully parsed turn; config contract git-only).
- **Round 3:** R2-F5 resolved, R2-F4 mostly. REQUEST CHANGES, all accepted (persist the
  comparator ID in the entry and hashed input; add a backend-identity field and validate
  client where knowable, since it resolves in-capture; authoritative shelf ledger via
  `describe -s -S`; poison the session on `record_turn` write failure; carry forward an elided
  unit's fingerprint; lossy-decoded content non-elidable; qualify framing to "all elidable
  units", add description/omission caps to completeness, and decide to widen
  `resume_incremental_diff` to backend-agnostic).
