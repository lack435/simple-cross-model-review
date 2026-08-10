# Plan: unify path-identity comparison (fix #55, foundationally)

Status: proposed. Tracks GitHub issue #55.

## 1. The reported bug (issue #55)

The reviewer fallback-chain resume path compares Windows executable paths with **exact
string equality**, so a case-only difference in a `--bin` path or in the resolved executable
path is treated as binary drift and refuses an otherwise-valid resume. Windows paths are
case-insensitive, and the working-root check in the same file already folds case
deliberately, so the two bin comparisons are an inconsistency, not a stricter rule.

Two sites:

1. `src/config.rs:821` -- `resume_entry_index` matches a stored record's configured bin via
   `&s.raw_bin() == raw`. `RawBin::Explicit(String)` (`src/session.rs:52`) derives `PartialEq`,
   so this is an exact `String` compare of the configured `--bin`.
2. `src/tools.rs:382` -- the resolved-binary identity gate on resume:
   `if bin.to_string_lossy() != *stored { return Err(resume_refusal(...)) }`.

Both over-*refuse*: they fail safe toward "start fresh" and never let a wrong binary through.
This is a usability/correctness inconsistency, not a security boundary.

## 2. Why the fix is bigger than two `eq_ignore_ascii_case` calls

The issue's suggested fix (make the two sites case-insensitive, ideally via a shared helper)
is correct but under-scoped for a foundational fix. Grepping every path operation in the tree
shows the codebase does not have *one* rule for handling paths. There are **three
comparison/persistence families** -- A (identity/equality), B (security containment), and C
(durable hash key) -- each with its own undocumented convention, and the #55 bug is one symptom
of that. A robust fix names and separates the three, unifies within each, and freezes the one
that must not move. A fourth grouping, **Family D**, is not a comparison family at all: it is a
catalogue of path *construction / normalization / projection* sites that the new helper must be
kept away from, listed so a later "unify all path code" pass cannot silently pull them in.

### Family A -- path *identity / equality* (fail-closed to "start fresh")

Answers "are these the same path?" for a gate that, when unsure, refuses and starts over.
Over-refusing is a UX annoyance; it is never a security hole.

| Site | Current comparison | Fold | Sep-normalize | Trailing sep |
|------|--------------------|------|---------------|--------------|
| `src/tools.rs:2367` cwd resume gate | `record.cwd.eq_ignore_ascii_case(&cwd)` | ASCII | no | no |
| `src/tools.rs:382` resolved-bin gate | `bin.to_string_lossy() != *stored` (exact) | **none (bug)** | no | no |
| `src/config.rs:821` raw-bin match | derived `PartialEq` (exact) | **none (bug)** | no | no |

The cwd site is the intended precedent and carries the rationale comment. The two bug sites
are the same family but skipped the fold. They should all share one primitive.

### Family B -- path *containment* (security boundary)

Answers "is this path inside that root?" and is used as a **security check** (deciding the
reviewer's read scope / neutral working dir), so its fail-direction matters and must not be
loosened.

| Site | Current comparison | Notes |
|------|--------------------|-------|
| `src/reviewer/mod.rs:108` `is_within` | `to_lowercase()` + `\`->`/` + trailing-`/` trim | doc-comment calls it a security check with deliberately one impl |
| `src/vcs/perforce.rs:2427` `lexically_within` | `to_lowercase()` + `/`->`\` + trailing-`\` + `..` rejection | fails closed on any `..` component |

These use **full-Unicode** `to_lowercase`, not ASCII. That is a deliberate, stronger fold and
these sites are out of scope to change behaviorally. They *are* in scope to share Family A's
separator/normalization core where it does not alter their fold or fail-direction (see 4.3).

### Family C -- path as a *persistence key* (must be frozen)

| Site | Current | Why it must not move |
|------|---------|----------------------|
| `src/config.rs:1225` state-dir hash | `fnv1a64(&cwd.to_string_lossy().to_lowercase())` | The fold is baked into a durable directory name under `%LOCALAPPDATA%\cross-review\`. Changing it would relocate the default state dir and orphan in-flight sessions for affected users. |

This looks like "another path lowercase" but it is not a comparison at all -- it is a hash
input. Unifying it with Family A would be a silent data-migration bug. **The plan explicitly
leaves it as-is** and adds a comment marking it a frozen key, so the next reader does not
"consistency-fix" it into an incident.

**Scope of the risk (so the freeze is justified, not overstated):** the hash only names the
*default* state dir. It is bypassed entirely when `--state-dir` is passed (both MCP configs can
set it), and there is a project-local fallback when `%LOCALAPPDATA%` is unset (`src/config.rs:1228`).
And an ASCII-only `cwd` is unaffected by an ASCII-vs-Unicode fold swap in the first place --
`to_lowercase` and `eq_ignore_ascii_case` agree on ASCII. So the relocation would hit only
default-state users whose `cwd` contains a non-ASCII character with case. That is a real but
bounded population; the freeze plus the Unicode-sensitive golden test (section 5) is the
proportionate response, not a claim that every user would be affected.

### Family D -- path *construction / normalization / projection* (must NOT use the identity helper)

These path operations are neither comparisons nor the hash key. The new `pathcmp` helper must
be kept away from all of them -- naming them so a later "unify all path code" pass does not wire
them through it:

| Site | What it does | Why the identity helper must not touch it |
|------|--------------|-------------------------------------------|
| `src/config.rs:363` `absolute_scoped_rules` | Builds the reviewer's **absolute read-scope glob** from the root (`\`->`/`, trailing-`/` trim, `.`/`..` segment rejection). | Security-critical string *emitted to the reviewer's permission rules*, not a comparison. Its normalization is tuned to the matcher's glob semantics; folding it through `pathcmp` would couple a security boundary to unrelated identity rules. |
| `src/vcs/perforce.rs:2451` `root_relative` | **Presentation**: `strip_prefix(cwd)` to show a path relative to the root, falling back to the absolute path. | An exact-prefix projection for display. Case-folding it would strip the wrong prefix length and mangle the shown path; it is not deciding identity. |
| `src/config.rs:1193` `normalize_dir` | **Producer**: `canonicalize` then strip the `\\?\` / `\\?\UNC\` verbatim prefix, because it leaks into the reviewer's prompt/command line. | It *produces* an absolute path that later feeds the identity comparisons; it is upstream of `pathcmp`, not a comparison, and keeps its **own local** prefix-strip -- deliberately not routed through the identity module. Applied to **`cwd` only** (`src/config.rs:707`). An explicit `--bin` is instead lexically absolutized in `resolve_bin` via `std::path::absolute` (4.2 item 1), which neither adds nor removes a `\\?\` prefix and does not resolve symlinks. `pathcmp` never strips `\\?\` itself: a `\\?\`-vs-plain `--bin` spelling therefore still compares unequal and fails closed to a fresh resume -- the safe direction, not a silent mismatch. |
| `src/reviewer/mod.rs:348` (`path_exts`) | Lowercases `PATHEXT` entries while resolving an executable from PATH. | A list of file *extensions* for exe resolution, not a path-identity compare. |

### Surveyed and deliberately excluded

- `src/vcs/perforce.rs:2053` host compare -- a hostname, not a path; already `eq_ignore_ascii_case`. Leave.
- `src/metrics.rs:424` log path `usage-{host_tag()}.jsonl` -- the real log filename embeds the
  host tag, and `host_tag()` **preserves uppercase** (`src/metrics.rs:659`), so this is not a
  lowercase-only name. The `!= "usage.jsonl"` at `src/metrics.rs:1700` is a **test** assertion
  (the file must be host-distinguished), not a production path compare. Neither is an identity
  comparison; both out of scope. (An earlier draft mis-stated this -- corrected.)
- `src/vcs/git.rs:187` and `src/errors.rs:660` `to_ascii_lowercase` -- git-status / evidence
  string parsing, not paths. (Note: the `to_ascii_lowercase` in `src/reviewer/mod.rs:348` is
  **not** in this bucket -- it is the PATHEXT normalization classified in Family D above. An
  earlier draft wrongly lumped all reviewer lowercasing as status/evidence parsing.)

## 3. Design decision: which case fold for Family A

**Recommendation: ASCII case fold (`eq_ignore_ascii_case`), matching the existing cwd precedent.**

Rationale, and why not full-Unicode `to_lowercase`:

- The one intentional, commented Family A site already chose ASCII. Matching it removes an
  inconsistency rather than adding a fourth convention.
- Real-world path drift here is drive-letter case, typed directory case, and `PATH`/env case
  -- all ASCII. ASCII fold covers every case #55 describes.
- `to_lowercase` is allocating, locale-independent-but-Unicode-full, and has genuine
  mis-fold hazards (Turkish dotless-i, Greek final sigma) that can make two byte-distinct
  paths compare equal in ways NTFS's frozen uppercase table would not. For an *identity* gate
  that is the wrong direction: it would make the gate accept more, and this family should only
  ever err toward refusing.
- Full Unicode/OS-accurate path canonicalization (`GetFinalPathNameByHandle`, 8.3 short
  names, symlink resolution) is explicitly out of scope per the issue and would require
  touching the filesystem on a hot gate path. Not doing it.

The two containment sites (Family B) keep their existing `to_lowercase` fold -- narrowing
*their* fold to ASCII is a security-relevant behavior change and is a non-goal here. The
result is: identity comparisons are ASCII-folded, containment comparisons are Unicode-folded,
and **each is documented as such** so the difference is a decision on the record, not drift.

## 4. Implementation

### 4.1 New module `src/pathcmp.rs`

One small, self-contained module (flat layout, like the other `src/*.rs`), registered in
`src/main.rs` (`mod pathcmp;`). It owns Family A's rule and nothing else. This repo uses no
copyright header -- files open with a `//!` module doc comment (see `src/config.rs`), so the
new file does the same.

API (final names to be confirmed against the style of siblings):

- `pub fn normalize_for_identity(path: &Path) -> String`
  Lossy-to-string, replace `/` with `\`, then trim trailing separators **root-awarely**, and
  **do not** lowercase (fold happens in the compare so we never store a folded string).
  Documented as identity-only, not OS-accurate canonicalization.

  **Normalization steps, in order (the whole rule, not just trailing trim).** A naive "trim one
  trailing separator" collapses distinct roots to equal -- `C:\` onto `C:`, `\` onto `""` -- which
  is exactly the direction a fail-closed identity gate must never move. The rule is therefore
  fully specified, and every step below is unit-tested (section 5):
  1. Lossy to string.
  2. Replace `/` with `\`.
  3. Collapse runs of `\` to a single `\`, **except** preserve a leading `\\` (the UNC prefix).
     So `C:\\Tools` -> `C:\Tools`, but `\\srv\share` keeps its leading `\\`.
  4. Trim trailing separators **down to, but never past, a canonical root**. The canonical root
     forms, each of which is a fixed point of the trim (it stops there):
     - Drive absolute root: `X:\` -- kept as `X:\`, never reduced to `X:`.
     - Drive-relative root: `X:` -- kept as `X:` (the current dir on drive X:), distinct from `X:\`.
     - Current-drive root: `\` -- kept as `\`, never reduced to the empty string.
     - UNC share root: `\\server\share` -- trailing separators trimmed to this form, so
       `\\server\share\` and `\\server\share` normalize identically (same share root). It is
       **not** trimmed below the share: `\\server` (server only) stays distinct from
       `\\server\share`.
     - Empty string: stays empty, and equals only another empty input.
     - Otherwise (a normal path): trailing separators trimmed, so `C:\Tools\` -> `C:\Tools`.
  5. Compare the two normalized strings with `eq_ignore_ascii_case`.

  This is lexical only -- it does not resolve `..`, symlinks, 8.3 names, or the `\\?\` verbatim
  prefix. For `cwd` that prefix is already stripped upstream by `normalize_dir` (Family D,
  `src/config.rs:707`); an explicit `--bin` is lexically absolutized in `resolve_bin`
  (`std::path::absolute`, which neither adds nor removes `\\?\`), so a `\\?\`-vs-plain `--bin`
  spelling still compares unequal here and fails closed.
  Fail-closed throughout: when two spellings are not provably the same location under these
  rules, they compare unequal and the resume starts fresh.
- `pub fn identity_eq(a: &Path, b: &Path) -> bool`
  `normalize_for_identity(a).eq_ignore_ascii_case(&normalize_for_identity(b))`.
- `pub fn identity_eq_str(a: &str, b: &str) -> bool`
  Same rule over already-stringified paths, for sites that hold `String`/`Cow<str>` (the
  resolved-bin gate and the `RawBin::Explicit` payload) without forcing a `PathBuf` round-trip.

Both `eq` forms must be built on the exact same `normalize_for_identity` core so `&Path` and
`&str` sites cannot diverge.

### 4.2 Migrate the three Family A sites

1. `src/tools.rs:382` resolved-bin gate. **Exception to the lexical-only rule (added after
   implementation review):** unlike cwd and the config match, this gate selects the *executable*
   a resume runs through, so a lexical ASCII fold is not fail-closed on a Windows per-directory
   case-sensitive volume, where `codex.exe` and `CODEX.EXE` are distinct files. The gate therefore
   uses `pathcmp::resolved_bin_matches(&bin, stored)` rather than bare `identity_eq_str`:
   byte-equal absolute paths match with no I/O; a fold-but-not-byte-equal pair is confirmed with
   an OS check (`std::fs::canonicalize` of both, which on Windows resolves to the real on-disk
   path and casing) and **fails closed** if either cannot be resolved. To keep that identity
   well-defined regardless of the process cwd, `reviewer::resolve_bin` absolutizes the resolved
   path before it is stored/compared/run, using **`std::path::absolute`** -- a *lexical* full-path
   (GetFullPathName semantics) that handles every Windows form including a drive-relative
   `C:foo.exe`, adds no `\\?\` prefix, and crucially **does not resolve symlinks**. Not resolving
   symlinks is deliberate: the reviewer CLI is commonly a stable shim pointing at a versioned
   release directory, so `canonicalize` here would make the resolved bin change on every CLI
   update and refuse resumes for the same install (observed in dogfooding). `std::path::absolute`
   is stable since 1.79, so the crate's MSRV is 1.79. `absolutize` returns no result rather than a
   relative fallback if it fails; callers treat that as unresolved. Net effect for #55: a case- or
   separator-only difference in the *same* install still resumes; a genuinely different executable
   never does. (`resolved_bin_matches`'s fold-mismatch confirmation still uses `canonicalize`, but
   only as a tiebreaker -- the common same-install case is byte-equal and never reaches it. The
   benign gates below stay on lexical `identity_eq_str`.)

2. `src/config.rs:821` raw-bin match. `RawBin` keeps its derived `PartialEq`/`Eq`. (Correction
   from an earlier draft: those derives are **not** what serializes the type -- serde's
   `Serialize`/`Deserialize` derives do -- and `validate_chain` at `src/config.rs:857` compares
   whole `ReviewerSpec` values with `==`, not `RawBin` directly. `PartialEq` is retained simply
   because removing a derive with no offending use is needless churn.) Add an explicit
   identity-aware comparison for the *resume-match* use:
   - Add `RawBin::identity_matches(&self, other: &RawBin) -> bool`: tags must match
     (`PathSearch`==`PathSearch`), and `Explicit(a)` vs `Explicit(b)` compares payloads via
     `pathcmp::identity_eq_str`. `PathSearch` vs `Explicit` never match.
   - `resume_entry_index` uses `s.raw_bin().identity_matches(raw)` instead of `==`.
   - **This forces a matching change in `validate_chain` (see 4.5): once resume-matching is
     identity-aware, duplicate detection must be too, or the chain can hold two entries that
     both `identity_matches` the same record and `.position()` silently binds the resume to the
     wrong (first) one.**

3. `src/tools.rs:2367` cwd gate: switch `record.cwd.eq_ignore_ascii_case(&cwd)` to
   `pathcmp::identity_eq_str(&record.cwd, &cwd)`. **Decided, not conditional:** the cwd gate uses
   the shared helper, so it gains separator- and trailing-separator tolerance on top of the
   case-fold it already had. This is a strict improvement in the same fail-closed direction (it
   only makes *more* spellings of the same directory resume, never fewer of different ones), it
   removes the last hand-rolled path compare, and it makes all three Family A sites use one
   primitive -- the foundational choice the scope commits to. The cwd resume test (section 5) is
   therefore unconditional and asserts both a case-only and a separator-only cwd difference still
   resume, while a genuinely different cwd still invalidates.

### 4.3 Family B: share the core without changing behavior (optional, gated on review)

`is_within` and `lexically_within` both do fold + separator-normalize + trailing-sep +
prefix test, differing in fold (Unicode) and separator direction and the `..` rejection. If
the reviewer agrees it is worth it, extract their shared *separator/trailing* normalization
into a `pathcmp` helper parameterized by fold, keeping each site's Unicode fold and
`lexically_within`'s `..` rejection **exactly**. If there is any doubt this is behavior- and
fail-direction-preserving, **do not do it in this PR** -- leave B untouched and just add a
one-line doc cross-reference from each B site to this plan explaining why B folds Unicode and
A folds ASCII. Family B changing behavior is explicitly not required to close #55.

### 4.4 Family C: freeze with a comment only

At `src/config.rs:1225`, add a comment: the `to_lowercase` here is a **durable hash key**, not
a comparison; changing the fold relocates the default state dir for affected users (those on the
default path with a case-bearing non-ASCII `cwd` -- see the qualified scope in section 2), so it
must not be "unified" with `pathcmp` without a migration. No code change.

### 4.5 `validate_chain` duplicate detection (now in scope -- required for consistency)

`validate_chain` (`src/config.rs:851`, comparing whole `ReviewerSpec` values with `a == b` at
`src/config.rs:857`) rejects two fully-identical chain entries. Its bin comparison is currently
exact. **This must move to the same identity rule as the resume match, in the same PR** -- it is
not an optional consistency nicety. The reason is a correctness coupling, not taste:

- Once `resume_entry_index` (4.2 item 2) matches bins via `identity_matches`, two chain entries
  whose `--bin` differs only by case or separator both `identity_matches` the same stored
  record. `resume_entry_index` uses `.position()`, which returns the **first** match, so a resume
  created by the *second* entry would silently rebind to the first -- exactly the "resume the
  entry that created the session" contract this code exists to hold.
- Rejecting those two entries as duplicates at validation time removes the ambiguity at the
  source: if the chain can never contain two identity-equal bins, the `.position()` match is
  always unique.

Implementation: `validate_chain` compares entries by a dedicated identity method rather than `==`,
comparing reviewer/model/effort exactly and the bin via the `RawBin::identity_matches` rule. (On
integration with the `usage-gate` work, which landed first, this became the existing
`ReviewerSpec::same_reviewer_identity` -- its byte-exact bin check was switched to
`raw_bin().identity_matches`, so one method serves both duplicate detection and that PR's
usage-minimum-excluding identity. The originally-planned separate `is_duplicate_of` was dropped as
redundant.) Keep the derived `PartialEq` for any remaining exact-equality use. Yes, this rejects a chain that parses today (two case-only-different
bins) -- that chain is a genuine duplicate (same install, same account, same rate limit), which
is precisely what `validate_chain`'s existing error message already says it is rejecting. Add a
regression test asserting a case-only-different-bin chain is rejected with the duplicate error.

Also update the wording that still asserts *exact* identity, so the code does not contradict the
new rule: the doc-comment at `src/config.rs:844` ("A fully-identical entry is invalid") and the
duplicate error text at `src/config.rs:857-863` ("identical ... in reviewer, model, effort and
bin") should read as **identity-equivalent** (case- and separator-insensitive on the bin), not
byte-identical.

## 5. Tests

Following the repo's inline `#[cfg(test)]` convention (see the `SessionRecord` builders around
`src/config.rs:1589` and `src/reviewer/argv_tests.rs`):

- **`pathcmp` unit tests** -- one per normalization step in 4.1, equal-cases and fail-closed
  not-equal-cases both:
  - Case fold: `C:\` == `c:\`; `C:\Tools\x.exe` == `C:/Tools/X.exe` (case + separators).
  - Separator collapse: `C:\\Tools` == `C:\Tools`; leading UNC `\\srv\share` keeps its `\\`.
  - Non-root trailing trim: `C:\Tools\` == `C:\Tools`.
  - Root fixed points (the fail-closed cases): `C:\` != `C:`; `\` != `""`; `\\srv\share` !=
    `\\srv`; and the equal pair `\\srv\share\` == `\\srv\share` (both spellings of the one share
    root).
  - ASCII-fold boundary: a non-ASCII case-only pair (e.g. an accented lowercase vs its capital)
    is **not** equal -- documents the ASCII (not Unicode) fold decision as a test.
  - `RawBin`: `PathSearch` matches `PathSearch`; `PathSearch` vs `Explicit` never matches;
    `Explicit` vs `Explicit` uses the identity rule above.
- **Resume, configured bin (`config.rs`)**: `resume_entry_index` returns `Some` when a record's
  `RawBin::Explicit` differs from the chain entry only by case (`C:\Tools\codex.exe` vs
  `C:\tools\codex.exe`). Reuse the existing record builders.
- **Resume, resolved bin (`tools.rs`)**: a resume whose `resolved_bin` differs from the freshly
  resolved bin only by case does **not** produce `resume_refusal`. A genuinely different path
  still refuses (guards against over-folding).
- **cwd gate**: a case-only cwd difference **and** a separator-only cwd difference each still
  resume (4.2 item 3 is decided, so both are unconditional); a genuinely different cwd still
  invalidates.
- **Regression guard for Family C**: a **golden** test pinning `default_state_dir`'s key suffix
  to an exact expected string for a **Unicode-sensitive** input (a cwd containing an uppercase
  non-ASCII char, e.g. an accented capital). An ASCII-only input would not catch a fold swap --
  `to_lowercase` and `eq_ignore_ascii_case` agree on ASCII -- so the input must be one where
  Unicode folding and ASCII folding diverge, and the assertion must be the literal expected
  suffix, not "two calls agree" (which `src/config.rs:2221` already covers and which cannot
  detect a changed hash implementation). If the fold ever changes, the golden value changes and
  the test fails, surfacing the state-dir relocation before it ships.

## 6. Verification

- `.\build.ps1` (fmt check, clippy `-D warnings`, unit tests, release build, restage `dist\`).
- **`smoke.ps1 -Reviewer codex` is required, not waived.** `AGENTS.md:162` mandates the smoke
  round trip when a change touches "the protocol, spawning, or session handling", and this change
  is squarely in session/resume-identity handling. `smoke.ps1` already drives a live review **and
  a resumed follow-up** (it asserts `resumed, turn 2` and `COUNTER=2`), so it exercises exactly
  the resume path being modified. It calls a model for real: document the cost in the PR (one
  reviewer session, two turns -- a few cents to low dollars depending on effort). (No
  case-only-`-ReviewerBin` smoke variant is proposed: `smoke.ps1` uses a single `--bin` for both
  turns (`smoke.ps1:29`, `smoke.ps1:42`) and a fresh state dir per invocation (`smoke.ps1:45`),
  so it structurally cannot feed one bin spelling on the first turn and a case-only variant on
  the resume turn against shared state. The case-only bin/resolved-bin drift is covered directly
  by the `config.rs`/`tools.rs` resume unit tests in section 5 instead, which is where that
  behavior belongs.)
- Per this repo's own gate (AGENTS.md), the PR is reviewed by the cross-model reviewer before
  merge; this plan is itself going through that gate first.

## 7. Scope summary

**In:** new `pathcmp` module for Family A (root-aware identity normalization + ASCII fold);
migrate the two #55 bug sites + the cwd precedent onto it; fold `validate_chain` duplicate
detection onto the same identity rule (required, 4.5); unit tests (incl. root-awareness and a
Unicode-sensitive Family C golden test) + resume tests; freeze-comment on the Family C key;
classify the Family D construction/normalization/projection sites as off-limits to the helper;
run `build.ps1` and the required `smoke.ps1 -Reviewer codex` resume round trip.
**Out (flagged, not silently skipped):** changing Family B fold/behavior; OS-accurate
canonicalization as the *general* identity rule (8.3 names, symlinks, verbatim `\\?\` prefixes)
-- with one deliberate exception, the resolved-bin gate (4.2 item 1), which confirms a fold-only
match with a `canonicalize`-based OS file-identity check so it stays fail-closed on a
case-sensitive volume; non-Windows paths (project is Windows-only).

## 8. Cross-model review resolutions (session `plan-path-comparison`, turn 1)

Codex (`gpt-5.6-luna`, effort max) reviewed turn 1 and returned CHANGES REQUESTED with six
findings; all six were accepted and are addressed above:

- **f1 (major) -- duplicate validation inconsistency.** `validate_chain` moved from "flagged,
  out of scope" to **in scope and required** (4.5), because identity-aware resume matching plus
  exact duplicate validation lets two identity-equal bins both match a record and bind the
  resume to the wrong entry. Corrected the false claims that `RawBin`'s `PartialEq` serializes
  the type and that `validate_chain` compares `RawBin` (it compares `ReviewerSpec`).
- **f2 (major) -- `C:\` vs `C:` collapse.** Normalization is now **root-aware** (4.1): drive and
  UNC roots are preserved, never trimmed to their drive-relative/truncated forms; tested.
- **f3 (major) -- smoke waiver.** Reversed: `smoke.ps1 -Reviewer codex` is now **required** (6),
  since the change touches session/resume handling per `AGENTS.md:162` and smoke already covers
  the resume path; cost documented.
- **f4 (minor) -- survey gaps.** Added **Family D** (2): `config.rs:363 absolute_scoped_rules`
  (security read-scope) and `perforce.rs:2451 root_relative` (presentation) are classified as
  off-limits to the identity helper.
- **f5 (minor) -- Family C test.** Upgraded to a **golden** suffix test over a Unicode-sensitive
  input (5), which a fold swap actually trips (an ASCII input would not).
- **f6 (minor) -- stale citations.** Fixed `src/vcs/perforce.rs:2053` and `src/vcs/git.rs:187`
  paths, and corrected the metrics rationale (real log is `usage-{host_tag}.jsonl` with case
  preserved; the `usage.jsonl` compare is a test assertion).

### Turn 2 (marked f1/f3/f5/f6 resolved; f2->f7 and f4->f8 refined; f9-f12 new)

Turn 2 confirmed f1, f3, f5, f6 resolved and the `.position()`/`is_duplicate_of` reasoning and
the ASCII-vs-Unicode split sound. The remaining and new findings, all accepted and addressed:

- **f7 (major, was f2) -- root handling still incomplete.** 4.1 is now a full, ordered
  normalization spec with explicit canonical root forms, covering the cases the earlier draft
  missed: the current-drive root `\` (never collapsed to `""`), both UNC share-root spellings
  (`\\srv\share\` == `\\srv\share`), and repeated-separator collapse. Section 5 tests each,
  including `\` != `""` and the share-root equal/!= pairs.
- **f8 (minor, was f4) -- Family D still omits sites.** Added `src/config.rs:1193 normalize_dir`
  (verbatim-`\\?\`-prefix producer, upstream of the compare) and `src/reviewer/mod.rs:348`
  (PATHEXT normalization). Fixed the excluded-list wildcard that had wrongly called the
  `reviewer/mod.rs:348` lowercasing status/evidence parsing.
- **f9 (minor) -- cwd behavior undecided.** Decided: the cwd gate uses the shared helper (4.2
  item 3), so its resume test is unconditional (case-only and separator-only both resume).
- **f10 (minor) -- optional smoke `-ReviewerBin` claim.** Removed; `smoke.ps1` cannot feed a
  distinct resume-turn bin against shared state (`smoke.ps1:29/42/45`). Case-only bin drift is
  covered by the section-5 resume unit tests instead.
- **f11 (minor) -- Family C claim too broad.** Qualified to default-state users with a
  case-bearing non-ASCII `cwd` (bypassed by `--state-dir`; ASCII inputs unaffected), keeping the
  freeze and golden test.
- **f12 (minor) -- stale "three families" count.** Reworded (2): three comparison/persistence
  families (A/B/C) plus a separate excluded-operations catalogue (D).

### Turn 3 (APPROVED with comments; f1-f12 resolved, four documentation nits addressed)

Turn 3 confirmed all behavioral findings resolved and gave `VERDICT: APPROVED` ("approve with
comments"), leaving only four minor documentation/scope-precision nits, all accepted and fixed:

- **f13 (minor) -- freeze comment overbroad.** 4.4 no longer says "relocates every existing
  state dir"; it points to the qualified scope in section 2.
- **f14 (minor) -- scope summary stale.** 7 now calls Family D "construction/normalization/
  projection", matching the catalogue.
- **f15 (minor) -- verbatim-prefix provenance too broad.** Family D's `normalize_dir` row now
  says it covers `cwd` only (`src/config.rs:707`); an explicit `--bin` is lexically absolutized in
  `resolve_bin` (`std::path::absolute`), and a `\\?\`-vs-plain `--bin` difference fails closed.
- **f16 (minor) -- duplicate terminology.** 4.5 now also directs updating the `validate_chain`
  doc-comment (`src/config.rs:844`) and error text (`src/config.rs:857-863`) from "fully-identical"
  to "identity-equivalent".
