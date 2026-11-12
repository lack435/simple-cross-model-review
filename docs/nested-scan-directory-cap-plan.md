# Nested-repository scan: count only directories against the per-directory cap (issue #224)

Status: **implemented**. The plan review (cross-review session `issue-224-nested-dir-cap`,
Codex gpt-6-luna, effort=xhigh) converged at round 3 with two minor documentation-accuracy
findings, both resolved: f1, the design doc's "every cap stops detection"; f2, the listing cap
truncates rather than stops. The implementation review (same session) converged at round 5 after
two more minor findings, both resolved: f3, the exact-256 boundary was untested; f4, the design
doc's 4,096 bound described entries read rather than directories kept. The new test failed on the old code as the issue describes:
`unserved: []`, `incomplete: "more than 256 entries in `vendor/`"`.

## Problem

`nested::enumerate` (`src/nested.rs`) lists the collapsed untracked/ignored directories from
`git ls-files --others --directory`, then descends one level into each with `read_dir`. That descent
counts **every entry** against `MAX_CHILDREN_PER_DIR` (256) *before* it checks whether the entry is
a directory. A build-output directory that holds 46,761 entries, only 5 of them directories, trips
the cap on every review and adds "Detection of nested git repositories stopped early (more than 256
entries in `datpipe/out/`)" to every start and result.

The issue overstates one cost, which is worth correcting before planning against it. The cap's
`break` leaves only *that directory's* loop. Other listed directories are still scanned. The
real costs are:

- a warning on every turn, which teaches the caller to stop reading the one warning #217 added; and
- any subdirectory of the capped directory that comes after the 256th entry in enumeration order
  is not checked. That is a real miss: a nested repository under `out/` named after the files is
  never seen.

The `ls-files` side the issue suggests filtering is already filtered: `untracked_directories`
(`src/evidence/git.rs`) keeps only entries ending in `/`. Only the `read_dir` descent needs changing.

## Why the cap is there

Review f4 of the #217 plan asked for bounded **work**, not just bounded results. "Entries per
directory" was one of the bounds that answered it. So whatever replaces it must still bound the work
a directory full of files can cost.

## Measured

A scratch binary ran `read_dir` and `DirEntry::file_type()` on this machine (Windows 11, NTFS, warm
cache) over a directory of 50,000 files, 2 directories and 1 junction:

- `entries=50003 dirs=2 symlinks=1 elapsed=11.9ms` (and 12.1 ms on a second run);
- the junction reports `is_dir() == false` and `is_symlink() == true`, which is the same answer
  today's `is_plain_dir` (`symlink_metadata`) gives.

`DirEntry::file_type()` on Windows comes from the directory-listing data, so classifying an entry
costs no extra system call. Today's code calls `symlink_metadata` for each entry, which does cost
one. Not measured: a cold cache or a network share. The deadline below covers both.

## Change

1. **Classify from the entry.** In the descent, use `entry.file_type()` in place of
   `is_plain_dir(&child)`, and skip anything that is not a plain directory (files, junctions,
   symlinks) before it is counted. If `file_type()` returns an error, that error is reported as now
   and counts against the cap.
2. **Cap directories, not entries.** Rename `MAX_CHILDREN_PER_DIR` to `MAX_SUBDIRS_PER_DIR` and keep
   it at 256. Count only plain subdirectories (excluding `.git`) and unreadable entries. The message
   becomes "more than 256 subdirectories in `<dir>`". `MAX_INSPECTED_DIRS` (512) still bounds the
   total, unchanged.
3. **Bound the files with the deadline.** Check the scan's existing deadline inside the entry loop.
   A directory too large to list in the remaining budget ends the scan with the existing
   "ran out of its 5s time budget" incomplete warning. That warning is accurate: the directory
   really was not fully checked.

   I am deliberately not adding a second, larger raw-entry cap. f4 needs the work bounded, and the
   deadline already does that. A raw-entry cap of N would bring this issue back for any directory
   with more than N files. At the measured rate, the 5s budget covers roughly 20 million entries
   before it binds.
4. **Docs.**
   - In `docs/nested-repository-visibility-plan.md`, change the bounds bullet from "entries read per
     directory during descent" to subdirectories, and point to this plan. Also correct the sentence
     that introduces the bounds (review f1). It says every cap, when hit, "stops detection", which was
     never true of the per-directory cap: that cap stops only that directory's descent and marks
     detection incomplete, while the scan carries on with the remaining listed directories. The
     listing cap drops the listed directories past the first 4,096 and scans the rest (review f2).
     Only the inspected-directory cap, the candidate cap and the deadline stop the scan outright.
   - In the README, the nested-repository paragraph needs no change: it describes the caps
     generically.

Nothing changes in what is reported, in the warning wording other than the cap message, or in any
protocol field. The scan is advisory (see the module docs), so the rigor bar is "don't make a
false-clean more likely". Change 1 keeps today's treatment of junctions, so no reparse point is
followed that was not followed before.

## Tests (`src/nested.rs`)

- **The issue's check.** An ignored `vendor/` holds 1,000 files named so that they list before a
  nested repository `vendor/zz-sdk/` that has a change. Assert that `incomplete` is `None` and that
  `vendor/zz-sdk/` is reported. On today's code this fails twice over: the scan stops early, and the
  repository is never reached.
- **A junction is still not followed.** Under an ignored directory, a junction (`mklink /J`) points
  at a repository with a change. Assert that it is not reported and that detection is complete.
  This pins change 1 to today's reparse behaviour.
- **The existing cap test** (`the_children_cap_marks_detection_incomplete`) already creates 257
  subdirectories. It keeps that layout, with its assertion updated to the "subdirectories in
  `vendor/`" message.
- **Untested:** the in-loop deadline. Exercising it needs a directory large enough to outlast a real
  deadline, or a seam that isn't otherwise needed. The outer-loop deadline test already covers the
  same `out_of_time` path and message.

Verification: `.\build.ps1`. No `smoke.ps1`: the evidence service and the protocol are untouched.
