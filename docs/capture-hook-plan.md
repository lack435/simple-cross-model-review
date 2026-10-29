# Capture hook — plan

Status: **plan** for issue #174, **converged at round 6** (26 findings, all resolved). Cross-review session `plan-capture-hook` (Codex,
gpt-5.6-luna, effort=xhigh).

**Scope decision (r2 → r3).** After round 2 the maintainer asked for the issue's request in full
rather than a cut-down v1. So two round-1 cuts are **reversed**, each with its own bounds:

- **Line diff (item 5):** a real unified diff, not whole-file replacement hunks.
- **Move support:** moves get a single `move` input with `moved_from` (items 3, 6, 7).

The other round-1 resolutions stand (f1–f4, f6–f8).

**Revision note (r1 → r2).** Round 1 raised nine findings, all accepted:
- **f1:** the hook identity hashes the converter's bytes.
- **f2:** a content digest covers the whole side.
- **f3:** generated text has a per-capture budget.
- **f4:** output records bind to inputs.
- **f5:** move handling, cut in r2 and restored in r3 (see the scope decision).
- **f6:** output files go through the handle-checked reader.
- **f7:** the cache and temp directory sit in the capability-dir location.
- **f8:** the collect budget includes the hook timeout.
- **f9:** the line diff, cut in r2 and restored in r3 (see the scope decision).

**Revision note (r2 → r3).** Round 2 confirmed f4–f9 resolved and held or raised seven items. All
are accepted and resolved below:

- **f1 (held):** a script converter's identity now covers the script too. Every arg that names an
  existing regular file contributes that file's bytes (item 1).
- **f2 (held):** the Perforce local-side digest text was stale. It now says the digest streams the
  whole file and never uses `read_capped` (item 6).
- **f3 (held):** a file skipped for budget makes the capture **incomplete**, so on git the canonical
  serve record is not complete and an approval cannot converge (items 4a, 7).
- **f10:** hunk ranges follow unified-diff rules for added, deleted and empty sides (item 5).
- **f11:** the cache key uses the **source** path, which the parent knows before the run. The
  hook-chosen display path is stored in the entry (item 4).
- **f12:** the binary-deletion fallback at `perforce.rs:1357` is a recording site. Every site now goes
  through one `record_binary` helper (item 6).
- **f13:** `moved_from` is no longer stale: move support is back by the maintainer's decision, with
  pairing rules defined (items 3, 6, 7).

**Revision note (r3 → r4).** Round 3 confirmed f1–f13 resolved and raised nine narrower items in
the restored designs. All are accepted:

- **f14:** a cache miss on a key the parent filled this turn makes the git composition
  **incomplete**. The bundle carries those expected keys (item 7; superseded by negative cache entries in r5).
- **f15:** a git move's base side is read from `base:moved_from`, never `base:path`, for the digest
  and for binding (item 7).
- **f16:** rename detection is explicit (`--find-renames=50%`). Prefill and composition share one
  NUL-aware `-z` parser and the same options (item 7).
- **f17:** Perforce's `BinaryFile` carries `moved_from`, and pairing produces an explicit move record
  (item 6).
- **f18:** a generated unit is placed **whole or not at all**. It never goes through the truncating
  `take_diff` (item 6).
- **f19:** the cache key includes `vcs` and the canonical working root. `change` is declared
  informational by contract: a converter's output must depend only on the file sides (items 3 and 4).
- **f20:** the line diff has hard work, memory and cancellation bounds (item 5).
- **f21:** an untracked file is binary if its bounded read contains a NUL anywhere, or if it is
  larger than the read cap (item 7).
- **f22:** README documentation is an explicit implementation task (item 8).

**Revision note (r4 → r5).** Round 4 confirmed f14–f20 and f22 resolved and raised two more, both
accepted. f21 had one stale paragraph left, now removed.

- **f23:** a source that changed after prefill got a key outside `expected`, and was treated as a
  complete "never converted" miss. Resolved by **replacing** `expected` with something simpler. The
  cache now also stores a **negative entry** when the hook ran on an input and returned no record for
  it. The child has three outcomes:
  - a positive entry is served;
  - a negative entry gives the complete "no generated text" line;
  - an **absent** key, for whatever reason (a source changed after prefill, eviction, a race), makes
    the composition **incomplete**.

  Binding reads the same entries. Nothing is ever treated as complete unless the cache says so
  (item 7).
- **f24:** the line diff gets an aggregate work budget per capture as well as per file (item 5).
- **f21:** the stale "first 8 KiB" paragraph in item 7 is gone. The rule stated in item 7 step 1
  (a NUL anywhere in the bounded read, or larger than the cap) is the only one.

**Revision note (r5 → r6).** Round 5 confirmed f21, f23 and f24 resolved and raised two more, both
accepted.

- **f25:** the hook read the live file whose digest the server had just taken. A file that changed
  A→B→A during the run would store B's conversion under A's key. Now **every side the hook is given
  is a server-written, immutable snapshot in `temp_dir`**, and each side's content digest is taken
  from exactly the bytes written there. The cache key therefore always describes the bytes the hook
  converted (items 3, 6 and 7).
- **f26:** the whole-file fallback was linear but unbounded in aggregate. One **input-byte budget per
  capture** now covers all rendering, line diff and fallback alike. Past it, every remaining file is
  skipped without rendering, stays binary, and makes the capture incomplete (item 5).

**Implementation notes (PR 1: shared pieces and Perforce).** Built as planned, with these
findings from a real p4d (2025.2, `test-perforce-capture-hook.ps1`):

- **Verified:** `p4 opened -ztag` reports `movedFile` on both halves of a pending move. Pending moves
  pair into one `move` input.
- **Verified false:** the assumption that `fstat -Ol` reports `movedFile` for historic revisions. It
  does not, for a submitted or a shelved revision, and `describe -s` does not either. So, as the plan
  allowed, those halves are converted as the add and the delete Perforce reports.
- **Verified:** `fstat -Ol <spec>` returns `fileSize`, `digest` (the server's MD5) and `clientFile`
  for a depot revision (`#rev`) and for a shelved file (`@=CL`). For an opened file, `fileSize` and
  `digest` describe the have-revision. These are the depot-side size check and content digest.
- **Found and fixed in passing (pre-existing):** a submitted changelist mapped local paths only for
  files that had a diff section. A submitted binary add or delete has none (`describe` runs without
  `-a`), so it was reported as "maps outside the working root" when it was simply binary. Every file
  in the changelist's file list is now mapped.
- **Deferral rather than removal.** A binary file the globs match is set aside at its binary site
  instead of becoming an omission. If it is not converted, it gets back exactly the omission it would
  have had. With no `--capture-hook`, capture output is unchanged, except for the deliberate fix
  below for submitted adds and deletes, which were misreported before.
- **`--capture-hook` with no `--capture-hook-glob` is a startup error.** Without a glob the hook could
  never be offered a file, so this is refused rather than left silently inert.

**Implementation notes (PR 2: git).** Built as planned in item 7, with these decisions:

- **One derivation, three callers.** `src/evidence/generated.rs` derives the binary inputs, digests
  each side and composes the section. The parent's prefill, the child's `repository_diff` and the
  binding's `committed_change_digest` all call it, so they cannot drift apart. Digests are
  `sha256:<hex>` of raw bytes, the same form as Perforce's local sides. Blob digests are memoized by
  object id, since a blob never changes.
- **Containment.** A worktree side is digested and snapshotted only **through** a handle opened
  inside the working root and verified after opening, the check the evidence reads use; the path is
  never reopened, so a concurrent reparse-point swap cannot redirect the read. A side that does not
  resolve inside the root (a link leading out) is excluded exactly like an oversized one.
- **Failing closed when the hook cannot be set up.** If the converter's identity or the cache cannot
  be set up, the bundle still carries the hook, with no cache. Every lookup is then absent, so a
  change with matched binaries is served incomplete and cannot be approved, rather than reading as
  complete with their text silently missing.
- **Blob reads are bounded.** `git cat-file blob` runs in a kill-on-close job object and is read on
  its own thread, so the request deadline, a cancel or the size cap ends it. Blob digests are
  memoized only for one composition or prefill.
- **Where the section appears.** Only in a whole-change composition (`path` empty) with a worktree,
  `HEAD` or commit head. An index head is exploratory and never binds, so it gets none.
- **The untracked rule is hook-scoped.** An untracked file is listed as binary only when a glob
  matches it, so without `--capture-hook` the composition is byte-identical to before.
- **Prefill runs once per turn**, before the first reviewer attempt, for every git review and consult
  when a hook is configured. Its warnings reach the caller and `status_summary`. Declared companion
  repositories are not converted in this PR.
- **Declined files no longer make a Perforce capture incomplete** (maintainer comment on #174,
  2026-10-29). A converter often declines most files its glob matches, for example art beside
  logic assets, and the old behaviour then cost every mixed change its resume baseline. A declined
  file is now listed in one line and the segment stays complete, as on git. Files nothing examined
  stay incomplete: over the size limit, a dropped record, a failed run, or over budget. A first-time
  decline was also left as an omission (only a cache hit placed it); it is now placed on both paths.
  Verified on a real p4d, including the resume baseline and the collapse.

**Amendment (r7, maintainer feedback after convergence).** The maintainer, who runs a converter
(an Unreal commandlet) against real changelists of 39–66 open files and hundreds of MB of art, raised
seven points. All are adopted here, and the implementation PR's review covers this amendment.

1. **Cost per turn.**
   - `--capture-hook-max-file-bytes` (default 16 MiB). A side larger than that is never copied,
     digested, downloaded or sent **by the capture hook**. Its size is checked first, from file
     metadata, `git cat-file -s` or `p4 fstat` `fileSize`. Such a file is treated exactly as an
     unmatched file. On git, that means it is listed as binary and does **not** make the
     composition incomplete. On Perforce, a binary file left as an omission has always made its
     changelist incomplete, and an excluded file does the same, no worse than today. The capture's
     own pre-existing bounded reads are unchanged: a text-typed file is read before it is found to
     hold a NUL. The limit governs only what the hook path adds.
   - Separately, a cache lookup digests the live file **in place**, and the snapshot into `temp_dir`
     is taken **only on a miss**. A re-review of unchanged files therefore copies nothing.
   - r5 f25's guarantee still holds. A miss stores its result under the digest of the snapshot the
     converter actually read. If the file changed between lookup and snapshot, the live re-digest
     finds no entry, which is absent and therefore incomplete (items 3, 6 and 7).
2. **Budget.** Generated text has its **own** 400 000-byte budget per capture, separate from the code
   diff's `diff_remaining`, so a large code diff never crowds out converted text (items 4a and 6).
3. **Large edits.** An output record may carry an optional `diff_text_file`. The server uses it in
   place of computing its own diff, with the same containment, UTF-8, size and budget checks, and
   still adds the provenance header. A converter that already writes a compact diff is then never
   pushed into an oversized whole-file fallback (items 3 and 5).
4. **`local_path`** is set for every file in the client view.
   - On Perforce it comes from `fstat`'s `clientFile`, which covers submitted, shelved and deleted
     files.
   - On git it is always the worktree path (items 3, 6 and 7).
5. **Snapshot names** keep the source file name: `temp_dir/<id>/base/<name>` and
   `temp_dir/<id>/modified/<name>` (item 3).
6. **Quiescence** is clarified. The runner never stops a converter for being silent. The only kill is
   the overall `--capture-hook-timeout-seconds`. "Quiescence" is the bounded wait, *after* the main
   process exits, for its other processes in the job to exit. A converter that runs for minutes
   needs that timeout raised, and the turn timeout must allow it (item 2).
7. **`--capture-hook-identity-file <path>`**, repeatable. Each file's bytes are part of the converter
   identity, so an export-format version file the operator names invalidates the cache when it
   changes (item 1).

## The problem, in one paragraph

Many repositories keep real logic in binary files: engine assets, data tables, notebooks, binary
config. Today a binary file in a change is only *named*. Perforce lists it as an omission ("is a
binary edit … p4 does not produce a text diff"), and git shows `Binary files a/X and b/X differ`. An
approval of such a change never looked at what changed. Teams often already have a converter that
turns these files into stable, diffable text. This plan lets the operator plug one in, so its output
becomes part of the **captured, certified change**, rather than unverified text the caller passes in
`context_paths`.

## Where the two backends differ (why this is two designs)

- **Perforce** captures the change **statically, in the parent**, before the reviewer runs
  (`vcs/perforce.rs:78` `capture`).
  - The rendered text becomes `Bundle.change`, served by `repository_change`.
  - Each file is a `Unit` (`perforce.rs:496`). Incremental resume collapses units whose fingerprint
    matches the prior turn (`apply_elision`, `perforce.rs:636`).
  - There is **no** serve-record floor for Perforce, and the converged hook is git-only
    (`tools.rs:4814`, `hook.rs:380`). A Perforce review converges on the reviewer's verdict alone.
- **Git** has no static capture. The change is composed **live, in the evidence child**, on every
  `repository_diff` (`compose_resolved`, `evidence/core.rs:2734`).
  - Its SHA-256 is the serve-record digest the approval floor checks.
  - The parent recomposes `base..HEAD` through the same function (`committed_change_digest`,
    `core.rs:2689`) to bind the converged hook.
  - Generated text therefore cannot be produced *during* a diff request: that would be slow, and the
    attest-time recomposition could not reproduce it byte for byte. Git needs a **content-keyed
    cache** that the parent fills before the turn, and that both the served and the committed
    compositions read identically.

The configuration, runner, protocol, cache and rendering are shared. Each backend is integrated
separately, in **two PRs**: Perforce first (self-contained), then git (composition and binding).

## Shared pieces

### 1. Configuration (`src/config.rs`)

These follow the shape of `--on-converged` / `--on-event`. The hook is operator code and is never
caller-chosen.

- `--capture-hook <program>`, resolved at startup with `resolve_hook_program`.
- `--capture-hook-arg <arg>`, repeatable.
- `--capture-hook-glob <pattern>`, repeatable.
  - A file goes to the hook only if its path matches a glob. With no glob, **no** file goes to the
    hook.
  - Globs are matched against the repository-relative path, using `/`, case-insensitively.
  - Supported syntax is `*`, `**` and `?` only; anything else is a startup error. The matcher is
    ~40 lines, with no crate.
- `--capture-hook-timeout-seconds N`: default 120, clamped to `--timeout-seconds`. It is added to
  `Config::max_wait_secs` (`config.rs:1779`), so a blocking collect still covers a whole review.
- `--capture-hook-materialize`: the server writes both sides of every file to the hook's temp
  directory before the run.
- `--capture-hook-max-file-bytes N` (default 16 MiB; r7). A side larger than this is excluded before
  any copy, digest or download: the file stays binary and the capture is not made incomplete.
- `--capture-hook-identity-file <path>`, repeatable (r7). Each file's bytes are part of the identity.
- A half-configured hook is a startup error, as for the other hooks.
- **Hook identity (r1 f1, r2 f1).** SHA-256 over all of:
  - a cache schema version;
  - the resolved program path and **the program file's bytes**;
  - every arg, and **the bytes of every arg that names an existing regular file** (absolute, or
    relative to the working root). This is how a script run through an interpreter (`pwsh -File
    convert.ps1`) is identified by its contents;
  - the bytes of every `--capture-hook-identity-file` (r7);
  - the sorted globs, the materialize flag and the size limit.

  The identity is recomputed at the start of every capture, so editing the program or a script in
  place invalidates its cache entries. A converter's *other* dependencies (modules it imports, its
  own config files) are not seen. The README tells operators to bump a `--capture-hook-arg` version
  token, or clear the cache, when those change.

### 2. Runner (`src/hook.rs`)

`run_process_with_flags` already provides what a converter needs:
- a required job object;
- no console window;
- JSON on stdin;
- a timeout over the whole process tree;
- a quiescence check.

Quiescence is **not** an idle timeout. The runner never stops a converter for being silent. The only
kill is the overall timeout. Quiescence is the bounded wait, after the main process exits, for the rest
of the job's processes to exit (r7). It keeps only a 2 KiB output tail, though. It gains a collector option that returns **stdout as
data**, bounded at 1 MiB (a JSON index, not the texts themselves), with a truncation flag. stderr
keeps the tail. No second runner is written.

### 3. Protocol

**Input** is one JSON document on stdin:

```json
{"schema_version": 1, "vcs": "git|perforce", "working_root": "...", "temp_dir": "...",
 "change": "<backend change identifier>",
 "files": [{"id": "f1", "path": "...", "change_type": "add|edit|delete|move",
            "moved_from": "...", "local_path": "...",
            "base": {"ref": "...", "file": "..."}, "modified": {"ref": "...", "file": "..."}}]}
```

- `id` is server-assigned. `moved_from` appears only on a `move` and names the source path.
- **Contract (r3 f19, r5 f25).** A converter's output must depend only on the content of a file's
  sides, read from `file` or fetched by `ref` (and the converter itself). `change` and `working_root` are informational. Results are cached and
  reused across changes in the same repository on that basis, and the README says so.
- `ref` is the backend's revision name: a git blob or commit id, or a Perforce `depot#rev`.
- **Snapshots (r5 f25, r7).** `file` is always a **server-written snapshot** in `temp_dir`, never a
  path into the working tree. It is named `temp_dir/<id>/base/<source file name>` or
  `temp_dir/<id>/modified/<source file name>`, keeping the original name and extension. Snapshots are
  taken only for files that **missed** the cache (item 4).
  - A working-tree side (a git worktree file, a Perforce pending local file) is always snapshotted.
    The server streams the file into `temp_dir` and computes its content digest in the same pass,
    so the digest is of exactly the snapshot's bytes.
  - A repository-history side (a git blob, a Perforce depot revision) is written the same way under
    `--capture-hook-materialize`, and otherwise left for the hook to fetch by `ref`. Its digest
    comes from the repository's own object identity (the blob or the depot revision's digest), which
    cannot change under the hook.
  - `local_path` is informational and is set for **every** file in the client view (r7). On
    Perforce it is `fstat`'s `clientFile`, including for submitted, shelved and deleted files; on git
    it is the worktree path. The contract requires a converter to read content from `file` or `ref`,
    and the README says why. A converter may use `local_path` to *name* things, such as an asset's
    package path.

**Output** is one JSON document on stdout:
`{"files": [{"id", "path", "base_text_file"?, "modified_text_file"?, "diff_text_file"?}], "warnings": [..], "note"?}`.

- **Records bind to inputs (r1 f4).**
  - Each record names an input `id`, and each `id` may appear at most once.
  - Its sides must match the input's change type: an edit or a move has both, an add has only
    `modified_text_file`, a delete only `base_text_file`.
  - **Or** it carries `diff_text_file` (r7): the converter's own unified diff of the file. The server
    uses it in place of computing a diff, after the same containment, UTF-8 and size checks, and
    adds its provenance header. With a `diff_text_file` the side files are optional.
  - `path` is the display path. It must be the input's `path` plus a suffix containing no
    separator, for example `<path>.txt`.
  - A record that breaks any of these rules is dropped with a warning, and its file stays binary. An
    input with no record is simply not converted.
- **Containment (r1 f6).** Every `*_text_file` is opened through the evidence service's bounded,
  handle-checked reader. It resolves the path inside `temp_dir`, then verifies that the **opened
  handle** is still inside it (`verify_open_file`, `evidence/core.rs:2606`). Each text file is read
  up to 1 MiB and must be valid UTF-8; otherwise it is dropped with a warning.
- **Temp directory (r1 f7).** `temp_dir` is a fresh directory in the capability-dir location
  (`evidence.rs:624`). That means the state directory, but only when it is outside the reviewed root
  and not inside a git repository; otherwise the system temp directory. It is removed after the run,
  whatever the outcome.
- **One run per capture**, carrying every matched file that missed the cache (item 4).
- **Fails soft.** A timeout, a nonzero exit, invalid JSON or a dropped record becomes a warning. The
  binary file stays listed exactly as today, and the review continues.

### 4. Cache (`src/capture_cache.rs`)

- **Location:** `capture-cache/`, in the same capability-dir location as the temp directory (r1 f7).
- **Key (r2 f11, r3 f19):** SHA-256 of `(hook identity, vcs, canonical working root, change type,
  source path, moved_from, base-side content digest, modified-side content digest)`. `change` is not
  part of the key, by the contract in item 3.
  - Every component is known to the parent **before** the hook runs, so a miss can be computed up
    front.
  - The hook-chosen display path is not part of the key. It is stored in the entry.
  - A missing side hashes as a fixed marker.
  - A content digest always covers the **whole** side, streamed through a fixed buffer, never a
    capped reader's prefix (r1 f2).
  - **Lookup digests in place (r7).** A working-tree side is digested by streaming the live file,
    without copying it. Only on a **miss** is it snapshotted into `temp_dir`. The miss's result is
    stored under the key computed from the **snapshot's** digest, which is the bytes the converter
    read. If the file changed between lookup and snapshot, that key differs from the live one, so the
    next lookup is absent and the capture is incomplete. r5 f25 still holds, and unchanged files cost
    a read, not a copy.
- **Entry:** either **positive**, holding the display path, the rendered generated text (item 5),
  its byte length and the hook's note, or **negative**, meaning the hook ran on this input and
  returned no record for it (r4 f23). Every reader gets identical bytes for a key, which is the
  property git binding needs. A failed hook run (timeout, nonzero exit, invalid output) writes **no**
  entries, so a later turn retries.
- Writes are atomic (temp file plus rename). Reads are bounded.
- **Eviction:** oldest first by modification time once the directory passes 256 MiB, at most once per
  capture. A read refreshes an entry's time.
- **Effect on resumes:** a resumed turn with unchanged sources hits the cache for every file, so the
  hook does not run. A resume after one source changed sends only that file.

### 4a. Budget (r1 f3, r2 f3)

Generated text gets its **own** per-capture budget of 400 000 bytes (the same size as `MAX_DIFF_BYTES`,
`vcs/shared.rs:29`, but a separate allowance). It is not shared with the code diff, so a large code
diff never leaves converted files without room (r7). Files are taken in source-path order while their
rendered text fits.

A file left out for budget **keeps its binary listing and makes the capture incomplete**:
- **Perforce:** the segment is marked incomplete, as any omission makes it, and the incomplete
  warning counts it.
- **Git:** `ComposedDiff.complete` becomes false. The canonical serve record is then not complete,
  so `whole_change_served` fails and **an approval cannot converge** until the change fits.

On git the budget is applied inside the composition from the entries' recorded lengths, so a
composition never builds an unbounded string.

### 5. Rendering (`src/textdiff.rs`): a real line diff, bounded

The issue has the server diff the two text sides itself. A Perforce host may not have git on PATH,
and AGENTS.md rules out adding a crate. So this is a small module:

- **Algorithm:** Myers O(ND) line diff over `\n`-split lines, with lines compared byte-exactly.
  Output is unified hunks with 3 lines of context.
- **Bounds (r3 f20).** These are hard limits, not best-effort:
  - at most 20 000 lines per side;
  - an edit distance of at most 1 000, so trace memory, O(D²) vector entries, stays near one million;
  - a work budget of 20 million comparison steps per file.

  The cancel flag is checked every edit step. Past any bound the file renders as a whole-file
  replacement hunk with a header note, so the output is correct and work and memory stay bounded per
  file.
- **Aggregate bounds (r4 f24, r5 f26).** A capture also has two totals across all its files:
  - 200 million line-diff comparison steps. Once spent, further files use the whole-file fallback.
  - **64 MiB of input bytes** read for rendering, both sides counted, for the line diff and the
    fallback alike. Once spent, every remaining file is **skipped without being read or rendered**:
    it keeps its binary listing and makes the capture incomplete, exactly like a budget skip
    (item 4a).

  Total rendering work per capture is therefore bounded by both totals, however many files there
  are. The hook's own work is bounded by its timeout.
- **Ranges (r2 f10):** unified-diff rules apply.
  - An added file (no base side) is `--- /dev/null`, `+++ b/<display>`, `@@ -0,0 +1,<m> @@`.
  - A deleted file is `--- a/<display>`, `+++ /dev/null`, `@@ -1,<n> +0,0 @@`.
  - An empty side counts 0 lines with start 0, for example `@@ -0,0 +1,3 @@`.
  - A side without a trailing newline gets the standard `\ No newline at end of file` marker.
  - Two identical sides produce no hunks, and the file is listed as `(no textual change after
    conversion)`.
- **Header:** each file starts `# generated from <source> by the capture hook`, plus
  `# moved from <moved_from>` for a move.
- **Converter-supplied diff (r7):** when a record carries `diff_text_file`, that text is used as the
  body after the header, and no line diff runs. It is still counted against the per-capture input-byte
  and output budgets.
- **Determinism:** the same inputs always give the same bytes. Ties are broken by preferring
  deletions first, the conventional choice.

## Perforce integration (PR 1)

### 6. Capture (`src/vcs/perforce.rs`)

- **Recording (r2 f12).** Every binary or no-diff site calls one helper, `record_binary(segment, ...)`.
  It pushes the omission text exactly as today and also records
  `BinaryFile {depot, local, action, base_rev, modified_rev, moved_file}`, where `moved_file` is the
  `movedFile` reported for that half (r3 f17). The sites are `perforce.rs:915`
  (submitted), `:1152` (edit), `:1165` (delete), `:1357` (deleted content with NUL), `:1416` and
  `:1438` (add), and `:1579` (shelved). A test asserts every binary omission path reaches the helper.
- **Move pairing (r2 f13).**
  - **Pending:** `p4 opened -ztag` reports `movedFile` on both halves of a move. `parse_opened_ztag`
    (`:2346`) keeps it on `OpenedFile`. A `move/add` and `move/delete` in the same changelist that
    name each other become **one** `move` input: base is the source's have-revision, modified is the
    new local file, and `moved_from` is the source path.
  - **Submitted and shelved:** pair from `p4 fstat -Ol` `movedFile` for the revision when present.
    That `fstat` reports it for historic revisions is *assumed* and is verified in implementation.
  - Pairing runs over the recorded `BinaryFile`s of a changelist and produces an explicit
    `Move {from: BinaryFile, to: BinaryFile}` record. That record becomes the input, with
    `moved_from = from.depot` (r3 f17).
  - An unpaired half stays a separate add or delete. A paired move replaces both halves' omissions
    with one generated unit.
- **Content digests (r2 f2).**
  - A depot side uses `p4 fstat -Ol` `digest` for that revision: the server's MD5, not computed by us.
  - A pending local side is digested in place for the lookup. On a miss it is snapshotted, and the
    stored key uses the snapshot's digest (item 4, r7). The digest always covers the whole file,
    independent of `read_capped`, which truncates.
  - **Size first (r7).** `p4 fstat -Ol` returns `fileSize` and `clientFile` for each file. A side
    over `--capture-hook-max-file-bytes` is excluded before any digest, snapshot or `p4 print`.
    `clientFile` becomes `local_path`.
- **The run.** After `p4.changelist` has run for every changelist and before `apply_elision`:
  1. Collect the matched inputs and look each one up in the cache.
  2. Send the misses to one hook run. Under materialize, `p4 print -o` writes each depot side to
     `temp_dir`.
  3. Store each result in the cache.
- **Units.** Each converted file becomes a new `UnitKind::Generated` unit carrying its source path,
  and its omission is resolved, along with both halves' omissions for a move. A segment is complete
  when no omission remains, so a converted binary no longer disables the baseline.
- **Fingerprints and resume.** The fingerprint covers the generated text like any unit's body, so
  incremental resume collapses an unchanged generated file for free. `INVENTORY_SCHEMA` goes from 1
  to 2, because `UnitKind` feeds the fingerprint. An old baseline then reads as unusable, costing one
  full capture on the next resume (the AGENTS.md disposable-state rule).
- **Render.** A `#### Generated text (from binary files)` section after the diff block, one fenced
  diff per generated unit, then the hook's `note`.
- **Budget (r3 f18, r7).** A generated unit is placed **whole or not at all**. A new
  `Budget::take_generated` either takes the unit's full length from its **own** `generated_remaining`
  (item 4a) or takes nothing. The truncating
  `take_diff` is never used for generated text, so no partial generated hunk can appear. A unit that
  does not fit stays an omission and makes the segment incomplete (item 4a).
- **Consult.** A Perforce consult with `include_change: true` goes through the same `capture`, so it
  sees the same generated files.
- **Certification.** Perforce has no serve-record floor today. Generated text gets exactly the
  treatment the rest of a Perforce capture gets. A `repository_change` floor for Perforce is a
  separate change.

## Git integration (PR 2)

### 7. Composition and binding

- **Before the turn** (`attempt`, ahead of `Bundle::create`, git only, when a hook is configured):
  1. **Find binary paths** in `branch-base..worktree`, through the isolated runner.
     - Tracked ones are the `-\t-` rows of `git diff --numstat -z --find-renames=50%`. Rename
       detection is explicit, not left to config (r3 f16). The `-z` form gives each rename as
       `-\t-\t\0old\0new\0`, which becomes a `move` input. One NUL-aware parser in `evidence/git.rs`
       serves both this prefill and the composition, with the same options.
     - Untracked ones are binary when their bounded read (the same 1 MiB cap the composition uses)
       contains a NUL **anywhere**, or when the file is larger than that cap. An uncertain file is
       treated as binary (r3 f21).
     - Keep those that match a glob. LFS-tracked paths are excluded, because both sides are
       pointers.
  2. **Content digests** are SHA-256 of each side's raw bytes.
     - The base side comes from `git cat-file blob base:<base path>`, read in full and streamed,
       where the base path is `moved_from` for a move and `path` otherwise (r3 f15).
     - The worktree side is digested in place for the lookup. On a miss it is snapshotted, and the
       result is stored under the snapshot's digest (item 4, r5 f25, r7). The hook converts that
       snapshot.
     - **Size first (r7):** a side over `--capture-hook-max-file-bytes` (worktree metadata, or
       `git cat-file -s` for a blob) is excluded before any read. The composition applies the same
       rule from the same sizes, so the served and committed compositions agree. An excluded file
       is listed as binary, adds no generated-text line, and does not make the capture incomplete.
     - The child's composition later re-digests the live worktree file. A file that changed since
       the snapshot gets a different key, which is **absent**, so the composition is incomplete
       (r4 f23).
  3. **Fill the cache:** look up each input, run the hook once for the misses, and store the results.
  4. **Warnings** go into `capture_warnings`, so they reach the caller and `repository_scope`'s
     `status_summary`.
- **The bundle** gains `capture_hook: Option<{identity, cache_dir, globs}>`. The child never runs the
  hook; it only reads the cache.
- **Composition** (`compose_resolved`). After the tracked diff and the untracked section, the child
  re-derives the same binary inputs for *this* composition and computes the same keys.
  - **A positive entry within budget** appends the stored text under
    `# generated text (from binary files)`.
  - **A negative entry** (the hook ran and declined this exact input) appends one line,
    `=== no generated text for <source> ===`. The composition stays complete: nothing was dropped.
  - **An absent key**, whatever the cause, appends `=== generated text for <source> unavailable ===`
    and sets `complete = false`, so the floor refuses an approval (r3 f14, r4 f23). The causes
    include a source changed after prefill, eviction, a race, or a failed hook run. A re-review
    re-runs the prefill for the new content.
  - **A budget skip** appends `=== generated text for <source> omitted: over budget ===` and sets
    `complete = false` (item 4a).
  
  Every one of these is deterministic given the cache contents. So the canonical digest, the serve
  record and the approval floor cover generated text automatically: **an APPROVE requires the
  generated text to have been served**.
- **Untracked binaries.** `read_untracked` inlines an untracked binary file lossily today, with no
  NUL check. In this PR, an untracked file that is binary by the rule in step 1 above is listed as
  `=== new binary file: <path> ===` instead of inlined, and the converter can then handle it. Untracked
  files never bind anyway, because a commit holds none.
- **Binding.** `ServedChange` gains the same `capture_hook` reference.
  `committed_change_digest` composes `base..HEAD` with the same derivation, rename options, base-path
  rule and lookups, reading committed-side bytes with `git cat-file blob HEAD:path` and base-side bytes
  from `base:moved_from` for a move.
  - When the committed content equals what was served, the keys are equal, the cache hits are the
    same entries, and the bytes match.
  - If the cache was evicted between serve and attest, a miss line differs and the hook skips with
    `head_does_not_match_review`. That is the safe direction, and a re-review refills the cache.
- **Consult.** A git consult composes through the same bundle, so it sees the same generated text.

### 8. Documentation (r3 f22)

These are implementation tasks, in the PR that ships each piece:
- the README gains a "Capture hook" section covering the flags, the protocol, the output contract
  (item 3), and how converter identity and cache invalidation work (item 1);
- `--help` gains the flags.

## Out of scope (as in the issue)

- Per-call or caller-supplied converters.
- Converting files that are not part of the change.
- Visual review.

Also out of scope: a Perforce certification floor, and LFS-stored binaries on git.

## Verify

**Unit tests (`cargo test`):**
- Without `--capture-hook`, Perforce render output and git composition are byte-identical to today
  (golden comparison on existing fixtures).
- **Config:** half-configured refusal, the glob grammar, clamping, the identity changing with the
  program bytes and with a script arg's bytes, and `max_wait_secs` growing by the hook timeout.
- **Runner:** stdout collected as data and bounded; timeout and exit codes; containment required.
- **Protocol:** unknown, duplicate and wrong-sided records are dropped with a warning; an output path
  outside `temp_dir` or through a reparse point is refused; non-UTF-8 and oversized text is dropped.
- **`textdiff`:** add, delete, edit, move, empty sides, no trailing newline, identical sides, each
  bound (lines, edit distance, per-file work, aggregate work) falling back, the aggregate input-byte
  budget skipping later files and marking the capture incomplete, cancellation, determinism, and
  range headers matching unified-diff rules.
- **Cache:** each key component invalidates; a hit refreshes; eviction honours the cap; writes are
  atomic.
- **Budget:** past the budget, files stay binary, the capture is incomplete, and on git the floor
  refuses an APPROVE.
- **Perforce** (existing fake-p4 seams):
  - add, edit, delete and paired move produce generated units;
  - an unpaired move half stays separate;
  - an unmatched file and a hook failure leave the omission and add a warning;
  - a resume with unchanged sources runs no hook and collapses the unit;
  - one changed source re-runs only that file;
  - every binary site reaches `record_binary`;
  - a generated unit that does not fit is not placed partially.
- **Git:**
  - composition with a hit, with a miss, and over budget;
  - an APPROVE served without the generated section does not converge;
  - binding matches after a commit of the reviewed content, and skips after the binary changes;
  - a rename becomes a move whose base side is read from `moved_from`;
  - an absent key makes the composition incomplete, including a source changed after prefill, while
    a negative entry does not;
  - a failed hook run writes no entries, so the next turn retries;
  - **ABA:** a worktree file changed A→B→A while the hook runs never stores B's conversion under A's
    key, because the hook converts the A snapshot;
  - an untracked binary is listed, not inlined.

**`smoke.ps1 -CaptureHook`** (new, opt-in, both reviewers): a scratch git repository with a small
binary file and a PowerShell converter. The reviewer must quote a token that exists only in the
converted text. Perforce has no smoke harness today; its path is covered by the unit tests above.

## Open questions for review

1. Is the split right: Perforce first, then git? The two share items 1–5, and git alone carries the
   binding work.
2. Is the parent-filled, child-read cache acceptable for git? It is what makes the served and
   committed compositions reproducible.
3. Is the `fstat` `movedFile` assumption for submitted and shelved moves acceptable, given that an
   unpaired half falls back safely to an add and a delete?
