# Capture summary, surfaced to the caller — design

Status: **proposed.** This document is the plan. It is intended to go through this repository's
own `cross-review` gate (Codex, gpt-5.6-luna, effort=max) before any code is written, the same
way [`incremental-resume-disposition.md`](incremental-resume-disposition.md) did. Filed against
issue [#46](https://github.com/lack435/simple-cross-model-review/issues/46) ("Surface a capture
summary to the caller in every review response").

**Revision note.**

- *r1 → r2:* separated "the diff was not truncated" from "the capture is complete" with an
  explicit `complete` field; redesigned the metrics tag to carry capture identity rather than
  size; built a sanitised `range` descriptor instead of reusing the raw command. *(r1)*
- *r2 → r3:* extended `complete` to every git gap the round-2 review found still silent —
  truncated untracked-file bodies and `stdout_incomplete`/`stdout_lossy` on the diff and status
  streams, which the git path discards today — and adopted a load-bearing invariant: **whenever
  `complete` is false, a caller-facing warning describes the gap**, so the response's "see warnings
  below" pointer never dangles. This adds a caller warning for incomplete-but-not-skipped Perforce
  segments (prompt-only today). Also: explicit `diff:` token, corrected warning ordering, required
  smoke run, fixed set-relationship wording. Changes tagged *(r2)* below.
- *r3 → r4:* pinned down the *scope* of `complete`. It covers **capture-level wholeness** — the
  streams, caps, enumeration, and unrun commands — and explicitly **not** the deliberate per-file
  exclusion of un-includable files (a binary, out-of-root, deleted, or unreadable untracked file),
  which the repository already treats as reviewer-facing detail, not a caller warning, and guards
  with a standing test (`an_ordinary_skipped_file_does_not_warn_the_caller`, `src/vcs/git.rs`).
  Added the untracked *enumeration* (`git ls-files`) stream flags to that wholeness set (they are
  accepted on `success` alone today), and reframed the size figures as **"shown" counts** whose
  floor qualifier fires on any shortfall in the *relevant* evidence, not diff byte-truncation
  alone. Changes tagged *(r3)* below.

## Problem: the caller cannot see what change the reviewer was given

When the server captures a change and hands it to the reviewer, the tool response that comes back
from `cross_model_review_result` describes the review but not the *evidence*. It carries the
review text, a `usage:` line, and — on a resumed turn — a `disposition:` line. What it does
**not** carry is any statement of what was actually captured and sent.

The reviewer's *prompt* echoes the diff command (`### git diff …`), but the prompt is not in the
response. So from the response alone a caller cannot distinguish:

- a correct capture of the intended range;
- a capture of the **wrong range** (a `--diff` pointed at the wrong base, or a stacked branch
  whose pinned `main...HEAD` swept in the PR underneath);
- a **stale-`main`** capture — the "1707 insertions instead of 208" case
  [`AGENTS.md`](../AGENTS.md) documents, where a stale local `main` silently widens the range;
- a **truncated or otherwise partial** diff, where the reviewer was shown only part of it.

All four produce a response that reads identically. This is exactly why `AGENTS.md` pushes so much
defensive bookkeeping onto the caller before every call — fetch `origin`, confirm local `main` is
current, check the tree is clean — *because the capture is otherwise invisible after the fact.*
[PR #44](https://github.com/lack435/simple-cross-model-review/pull/44) added a `disposition:` line
for the incremental-resume slice of this, but that fires only on a resumed turn and describes only
the resume delta. The general capture — what was sent on **every** turn, fresh or resumed — is
still opaque.

## What "surfaced" has to mean here

This repository already treats "the reviewer was given partial or unexpected evidence" as a
first-class safety property. Truncation is stated in the prompt and warned to the caller; skipped
changelists are warned; a dirty tree is flagged; the resume disposition is surfaced. The capture
summary is the one rung below all of them that is still missing: the *baseline* statement of what
the reviewer saw, present whenever a change was sent, that every one of those other signals
qualifies.

Three rules govern the design. Two are inherited; the third the round-2 review forced.

1. **Report what the server *sent*, never what the reviewer received or still holds.** The summary
   describes the capture the server produced and handed over.
2. **A size figure is not a completeness claim.** "Not truncated" means only that the diff budget
   was not hit; it does not mean every file was shown whole. The summary carries a *separate*
   completeness verdict, and its truncation token is scoped explicitly to the diff.
3. *(r2)* **Every completeness gap that sets `complete = false` also emits a caller-facing
   warning.** The `captured:` line states the verdict and points at the warnings for detail;
   this invariant is what makes that pointer honest. It is maintained by construction — the same
   gap facts drive both the verdict and the warnings — and it forces gaps that are silent to the
   caller today (a truncated untracked body; a short-streamed untracked enumeration; an
   incomplete-but-not-skipped Perforce segment) to become warnings.

   *(r3)* A "completeness gap" here means a **capture-level** shortfall — something wrong with the
   capture *as a package*: a stream that ran short or decoded badly, a size cap that dropped
   intended content, an enumeration cut off, a required command that did not run, or an included
   file's body shown only as a prefix. It deliberately does **not** include the per-file exclusion
   of an *un-includable* file: a binary, out-of-root, deleted, or unreadable untracked file is not
   evidence that could have been sent, so its omission is surfaced to the reviewer per-file (which
   can open the file) and is not a caller-level completeness gap. That line is the repository's
   existing, tested design — `an_ordinary_skipped_file_does_not_warn_the_caller` (`src/vcs/git.rs`)
   asserts exactly that a binary untracked file leaves `Capture::warnings` empty, and its comment
   names it as "the assertion that stops 'warn about everything' arriving later as an obvious
   improvement." `complete` respects that boundary rather than moving it.

## Proposal: a typed `CaptureSummary`, computed by each backend, rendered once

Add a `captured:` line to every completed review response that supplied a change, beside `usage:`
and `disposition:`:

```
captured: git diff <base>..<head> — 12 files, +487/-89, 0 untracked — diff intact — complete
captured: changelists 43650, 43651 — 8 evidence units — diff intact — complete
```

The value is carried as a typed `CaptureSummary`, not a preformatted string, so it is testable,
each backend owns its own variant, and the metrics log can record a compact tag from the same
value — exactly the shape `Disposition` already has.

### When a summary exists at all

**A summary is emitted exactly when the server sent a change** — that is, when `capture.change` is
`Some`. This is one gate, and it is simpler than the disposition's two:

- There is **no resume gate.** Unlike a disposition (which is meaningful only relative to a prior
  turn), the capture summary describes the change on *its own terms*, so it is present on a fresh
  turn 1 exactly as on a resume.
- A turn that **sent no change** carries no summary. That covers `--diff none`, `--diff auto` with
  a shell-equipped reviewer (`supplies_change()` is false, so `capture.change` is `None` and the
  reviewer fetches its own diff), and a capture that **failed or was cancelled** (`change` is
  `None`; the caller is told through the existing fail-closed *warning*, not through a summary line
  that would have nothing honest to say).

Because the gate is precisely `change.is_some()`, the natural home for the value is on
`CapturedChange` itself — the struct that exists if and only if a change was captured. It is
**not** `Option` at that layer: a `CapturedChange` always has a summary. The `Option` appears one
layer up, where a whole `Capture` may or may not carry a `change`.

### The `CaptureSummary` value

A VCS-neutral enum in a new `src/vcs/capture_summary.rs`, parallel to `disposition.rs`. Each
backend constructs its own variant from data it already has in hand; the enum owns the rendering.

```rust
pub enum CaptureSummary {
    Git {
        /// A safe, bounded range descriptor — resolved hex endpoints for a pinned range, a fixed
        /// string for working-tree/staged, or the sanitised configured spelling otherwise. Never
        /// the raw command string. See "The git range descriptor".
        range: String,
        files: usize,
        insertions: usize,
        deletions: usize,
        /// New files carried alongside the diff (git-untracked), which a diff cannot cover.
        untracked_files: usize,
        /// The diff stream was shortened (byte cap, short pipe, or lossy decode), so the `+/-`
        /// and file counts are a floor. Broader than the byte cap alone. See "shown counts".
        diff_incomplete: bool,
        /// Capture-level wholeness (streams, caps, enumeration, unrun commands, truncated included
        /// bodies) — NOT the per-file exclusion of un-includable files. See rule 3.
        complete: bool,
    },
    Perforce {
        /// The changelists actually captured (not merely requested).
        changelists: Vec<u64>,
        /// Requested changelists that were skipped, so the count is honestly a subset.
        skipped: usize,
        /// Captured changelists whose evidence was incomplete (binary/out-of-root/lossy/etc.),
        /// distinct from skipped. Drives both the verdict and the new caller warning.
        incomplete_changelists: usize,
        evidence_units: usize,
        diff_truncated: bool,
        complete: bool,
    },
}
```

`CaptureSummary::summary()` renders the body of the `captured:` line; `CaptureSummary::tag()`
renders a compact, bounded kebab-case tag for the metrics log (see [Metrics](#the-metrics-tag)),
matching how `Disposition::tag()` is used.

### Completeness is a first-class field, distinct from truncation

The round-1 review's major finding was that `diff_truncated` alone can report a partial capture as
complete; the round-2 review found the first fix still missed git gaps. So `complete: bool` is
computed **at the backend, from the full set of gap facts** — not re-derived at the render seam
(which would drift from the warning logic) and not inferred from "warnings is non-empty" (which
would wrongly count the Perforce *over-caps* warning, a statement about the *next* turn's resume,
not this capture's evidence).

**Git `complete` is false if any of these capture-level shortfalls holds.** The first four are
surfaced as caller warnings today; *(r2)*/*(r3)* the rest are the gaps rounds 2–3 found silent,
which this design turns into warnings:

- `diff.truncated` — the diff hit `MAX_DIFF_BYTES`;
- `status.truncated` — the `git status` listing hit the cap;
- a non-empty `notes` — a required command did not run: `git status` failed, or *(r3)* the
  `git ls-files` untracked enumeration failed (both already push a note that becomes a warning);
- a non-empty `OmissionReport::capture_level` — untracked content cap reached, or the untracked
  listing was cut short;
- *(r2)* any `untracked` file with `body.truncated` — an included new file was shown only as a
  prefix (per-file or total-budget cap; `src/vcs/git.rs` untracked read). Not a capture-level
  omission today, so it needs to be counted and warned;
- *(r2)* `stdout_incomplete` or `stdout_lossy` on the diff **or** the status `RunOutcome`
  (`src/reviewer/mod.rs` `RunOutcome`; `src/vcs/git.rs` where the diff and status are run). The git
  path reads only `success`/`stdout` today and discards these, so a diff that ended as a short
  prefix under the byte cap, or decoded lossily, is currently invisible. The Perforce path already
  treats these as truncation; git must too;
- *(r3)* `stdout_truncated`, `stdout_incomplete`, or `stdout_lossy` on the **`git ls-files`**
  `RunOutcome` (`src/vcs/git.rs` `untracked`). Today that listing is accepted on `out.success`
  alone, so a *successful but short* enumeration silently drops untracked files — the count comes
  out low and nothing warns. This is the round-3 gap; it makes `untracked_files` a floor, sets
  `complete = false`, and emits a warning, exactly like the diff/status streams.

**What `complete` deliberately excludes** *(r3):* the per-file exclusion of an un-includable
untracked file — a binary, out-of-root, deleted, or unreadable file the enumeration *reached* and
then could not include. Those are noted to the reviewer per-file (`change.untracked_omitted`,
rendered into the prompt) and are **not** caller warnings, per rule 3 and the standing test
`an_ordinary_skipped_file_does_not_warn_the_caller`. `complete` is about whether the capture
*package* is whole — the streams, caps, enumeration, and commands — not about whether every file's
bytes were includable. The `content_cap_skipped` case *is* capture-level (intended content dropped
because the budget ran out) and stays a gap; a *binary* file is not (its bytes were never sendable)
and does not.

**Perforce `complete`** is `skipped.is_empty() && incomplete_changelists == 0 && !diff_truncated`,
where `incomplete_changelists` counts captured segments with `complete == false`
(`src/vcs/perforce.rs` per-segment completeness — a binary or unreadable added file, an
out-of-root or deleted file, a lossy or truncated `p4 describe`; the Perforce path already folds
its stream-incompleteness into that per-segment flag). This is deliberately *narrower* than the
existing `capture_complete` local, which also requires `identity.client_spec_digest.is_some()`:
that extra condition governs whether the capture may seed a resume *baseline*, a different question
from whether the evidence shown this turn was whole.

**Set relationship** *(r2, corrected):* `complete` is the **stricter** condition — `complete`
implies the diff and every other stream was whole, but a whole diff does **not** imply `complete`.

### The git range descriptor

The round-1 review found that `change.command` is built from the raw revision spelling (`cfg.diff`)
and would carry unsanitised, unbounded text into both the response and the log. The summary
therefore does **not** reuse `change.command`. It carries a purpose-built `range` string,
constructed in `git::capture` where the resolved endpoints and the effective mode are both in
scope:

- **A pinned HEAD-anchored range** (the common case: `--diff main...HEAD`, `--diff <base>..HEAD`,
  and every incremental delta) renders from the resolved commit ids as `git diff <base12>..<head12>`,
  abbreviated to 12 hex chars each. Drift-proof, and the resolved base commit is exactly the tell
  that catches a stale-`main` capture — the review endorsed the resolved-SHA form for pinned ranges.
- **Working-tree / staged modes** render as the fixed strings `git diff HEAD` / `git diff --cached`
  — no user-influenced text at all.
- **Any other mode** (a fixed window like `HEAD~3..HEAD~1`, or a range whose endpoints did not
  resolve) retains the *configured* spelling, passed through `shared::safe_label` (control-char
  filtered, backtick-stripped, length-bounded) at construction. This is the one path where
  operator-supplied text reaches the descriptor, and it is sanitised before it is stored.

Hex ids are inherently safe; `safe_label` bounds the fallback. The `range` field is therefore
always safe to interpolate into Markdown and into the JSON log.

### Git: what the numbers are, and where they come from

Constructed in `git::capture` (`src/vcs/git.rs`), which has every input in scope where it builds
the `Change`. *(r3)* Every figure is a **shown count** — what actually reached the reviewer — so it
is never an overstatement; when the relevant evidence was shortened, the render marks it a floor:

- **`range`** as above.
- **`files` / `insertions` / `deletions`** are a cheap parse of the captured diff text
  (`change.diff.text`, already the post-truncation string), via a small
  `diff_line_counts(&str) -> (usize, usize, usize)` helper with its own unit tests:
  - `files` = count of `diff --git ` header lines. A rename-only, mode-only, or binary file still
    emits that header, so it is counted with zero line changes — which is correct.
  - `insertions` = lines beginning with `+` **excluding** the `+++ ` file header;
    `deletions` = lines beginning with `-` **excluding** the `--- ` file header.
  - These are a floor whenever the diff stream was shortened, which is more than the byte cap:
    `diff_incomplete = change.diff.truncated || diff.stdout_incomplete || diff.stdout_lossy`.
- **`untracked_files`** is `change.untracked.len()` — new files git has never seen, carried
  alongside the diff and therefore *not* in the `+/-` counts. Reported separately (the review
  endorsed keeping it separate). *(r3)* It is itself a floor when the enumeration was shortened
  (`git ls-files` short-streamed) or the untracked listing was cut short — the render says so.
- **`diff_incomplete`** as above; **`complete`** as defined above.

### Perforce: what the numbers are, and where they come from

Constructed in `perforce::capture` (`src/vcs/perforce.rs`) where the `CapturedChange` is built:

- **`changelists`** is the `captured` vector — the changelists that produced a segment.
- **`skipped`** is `skipped.len()`; its per-changelist reasons are already warned.
- *(r2)* **`incomplete_changelists`** is the count of captured segments with `complete == false`.
  It drives the verdict **and** a new caller warning (below), because those segment details are
  rendered only into the prompt today, so a caller has no other way to see them.
- **`evidence_units`** is the count of units across all captured segments
  (`segments.iter().flat_map(|s| &s.units).count()`) — *(r3)* a shown count, a floor when
  `diff_truncated || incomplete_changelists > 0`, since an incomplete segment or a budget-cut diff
  means units were dropped or shortened. On an elided resume the *unit count* is unchanged — a
  collapsed unit is still a unit — and the resent/collapsed split stays the disposition's job (the
  review endorsed this, given `complete` now surfaces incompleteness).
- **`diff_truncated`** is `budget.diff_truncated`; **`complete`** as defined above.

### The new warnings that keep the invariant *(r2)*

Rule 3 requires a caller warning for every gap. Two are added because they are silent to the
caller today:

- **Git — truncated untracked bodies.** When one or more included untracked files were shown only
  in part, emit one `incomplete(...)` warning naming the count (the per-file bodies are already in
  the prompt; the caller needs the fact).
- **Git — short-streamed diff or status.** When a diff/status stream came back
  `stdout_incomplete`/`stdout_lossy`, emit an `incomplete(...)` warning — the same wording the
  Perforce path uses for its stream gaps.
- *(r3)* **Git — short-streamed untracked enumeration.** When `git ls-files` returned success but
  `stdout_truncated`/`stdout_incomplete`/`stdout_lossy`, emit an `incomplete(...)` warning that the
  untracked file set may be short (the existing note only fires when `ls-files` *fails*).
- **Perforce — incomplete segments.** When `incomplete_changelists > 0`, emit a warning naming how
  many captured changelists were incomplete and that their per-file reasons are in the prompt. This
  mirrors the existing skipped-changelist and diff-truncation warnings, which already reach the
  caller.

Every other capture-level gap already warns, so no change is needed there. The result:
`complete == false` iff at least one capture-completeness warning was emitted — while the per-file
exclusion of an un-includable file warns *neither* and remains prompt-only, preserving
`an_ordinary_skipped_file_does_not_warn_the_caller`.

### How the size and completeness read together

`render_completed` renders the shown counts, then an explicit diff token *(r2)*, then the
completeness verdict. *(r3)* Each count is prefixed "at least" and flagged a floor when *its own*
evidence was shortened — the diff figures on `diff_incomplete` (not the byte cap alone), the
untracked figure on a short enumeration — so a shortfall in one stream does not falsely qualify a
count drawn from another:

```
… — 12 files, +487/-89, 0 untracked — diff intact — complete
… — 12 files, +487/-89, 3 untracked — diff intact — partial (see warnings below)
… — at least 40 files, +90000/-1200, 0 untracked — diff incomplete (counts are a floor) — partial (see warnings below)
… — 12 files, +487/-89, at least 2 untracked — diff intact — partial (untracked listing short; see warnings below)
… (perforce) — at least 8 evidence units — diff intact — partial (1 of 3 changelists incomplete; see warnings below)
```

*(r2)* The `diff:` token is **always** present. *(r3)* Its states are `diff intact` /
`diff incomplete`, where `diff incomplete` covers the byte cap **and** a short or lossy stream —
so a diff that ended as a prefix without hitting the cap does not read as `intact`; the warning it
emits says which it was. The verdict is always `complete` / `partial`. When `partial`, the line
points **below** to the WARNING lines — corrected from "above": `render_completed` prints the
warnings *after* the `captured:` line — and, by rule 3, at least one such warning always exists.

## How the caller sees it

`render_completed` in `src/tools.rs` gains one block, placed **after `usage:` and before
`disposition:`** — cost first, then what was sent (the general statement), then the resume delta
(the refinement). Warnings follow all three:

```
usage:     …
captured:  git diff <base>..<head> — 12 files, +487/-89, 0 untracked — diff intact — complete
disposition: incremental — only the delta since your last turn (<a>..<b>) was sent, 2 new commits

WARNING: …
```

On a fresh turn there is no `disposition:` line but there **is** a `captured:` line — the whole
point of the general summary. The rendering reads the value off the snapshot; nothing in
`render_completed` knows how the counts were computed.

The line answers the acceptance criteria directly: it states the capture command/range, a size
summary, whether the diff was truncated and whether the capture was otherwise complete — and a
caller can confirm the reviewer saw the intended change from the response alone, because the
resolved range and `+/-` counts together catch the stale-`main` case without re-running git or p4.

## Plumbing: the path from capture to response

The value follows the exact path `disposition` already travels:

1. **The backends** construct the `CaptureSummary`: `git::capture` builds the `Git` variant and
   carries it on its internal `Change` (so resolved endpoints and mode are in scope for the safe
   `range`); `perforce::capture` builds the `Perforce` variant directly.
2. **`CapturedChange`** (`src/vcs/shared.rs`) gains a non-optional `summary: CaptureSummary` field.
   `git_capture` (`src/vcs/mod.rs`) moves the git `Change`'s summary across; Perforce sets it
   inline.
3. **`tools.rs`** takes `capture.change.as_ref().map(|c| &c.summary)`, producing an
   `Option<CaptureSummary>` (None iff no change was sent).
4. **`Outcome`**, **`Review`**, and **`Snapshot`** (`src/registry.rs`) each gain
   `capture_summary: Option<CaptureSummary>`, threaded through `Registry::finish` and
   `Snapshot::of` exactly as `disposition` is. `Outcome::failed` sets it to `None`.
5. **`render_completed`** (`src/tools.rs`) renders the line from `snapshot.capture_summary`.

### The metrics tag

Decision on open question 4: **the metrics field stays in scope**, because a capture summary that
is legible live but invisible in the audit log would reproduce, for the general capture, exactly
the gap this project closed for the resume disposition. The round-1 review was right that a
counts-only tag (`git:12f+487-89`) defeats the field's purpose — it cannot tell two ranges apart
or reveal a stale base — so the tag carries the **identity of the capture**, not its size:

- Git: the resolved endpoints plus markers, e.g. `git:<base12>..<head12>` with `+d` (diff
  incomplete — byte cap or short/lossy stream) / `+p` (capture partial) suffixes.
- Perforce: the captured changelist numbers plus the same markers, e.g. `p4:43650,43651+p`.

(`+d` implies `+p`, since an incomplete diff makes the capture partial; both are shown when the
partial verdict has causes beyond the diff, so an audit can tell "only the diff was cut" from
"other evidence was missing too".)

Both are built from the already-safe `range` / `changelists` fields and the whole tag is
length-bounded before it is written. It is recorded on `Record` (`src/metrics.rs`) as
`captured: Option<String>`, `#[serde(default, skip_serializing_if = "Option::is_none")]`, so older
records and no-change turns stay clean — consistent with the existing `disposition` field.

Following the `disposition_tag` precedent (`src/tools.rs`, where the tag is taken from the local
value *before* the reviewer attempt), the `captured` tag is likewise extracted from the local
`CaptureSummary` before the attempt, so a **failed** reviewer attempt — which still captured and
sent a change — is logged with what it sent. This is the one place the log and the response diverge
on purpose: the *response* line rides the successful outcome (a failed review renders as an error
via `Status::Failed`, never through `render_completed`), while the *log tag* is taken early so
failure is still audited. Both come from the same `CaptureSummary`.

## What this must not do

- **It must not run any new VCS subprocess.** Every figure is a parse of what was already captured
  or a count of a vector already in memory. No extra `git diff --numstat`, no extra `p4` call.
  *(r2, r3)* The added stream-flag checks (`stdout_incomplete`/`stdout_lossy` on diff and status;
  the same plus `stdout_truncated` on `git ls-files`) read flags already on the `RunOutcome` those
  commands already returned — no new command is run.
- **It must not change what is captured or rendered into the prompt.** The summary is a read of the
  capture, computed after it. The golden prompt snapshot in `src/vcs/mod.rs` must be
  byte-for-byte unchanged — the reviewer's prompt does not gain the `captured:` line, only the tool
  response does. *(r2)* The new warnings are caller-facing (`Capture::warnings`); they do not alter
  the prompt body, so the golden test still holds.
- **It must not overclaim on a truncated or partial capture.** `complete` is `false` on any
  *capture-level* gap, and by rule 3 each such gap also emits a warning; a shortened stream
  additionally makes the counts drawn from it a floor. *(r3)* Equally, it must not *under*claim:
  `complete` stays `true` for the deliberate per-file exclusion of an un-includable file, which the
  repository intends to keep prompt-only. The existing warnings still fire on their own terms.
- **It must not carry unsanitised operator text.** The git `range` is hex or `safe_label`-bounded
  at construction; the metrics tag is built from those safe fields and length-bounded.
- **It must not restate the reviewer's confidence.** It reports the *evidence sent*, full stop.

## Blast radius

New: `src/vcs/capture_summary.rs` (the enum, `summary()`, `tag()`, unit tests). Touched:
`src/vcs/git.rs` (build the `Git` variant + `diff_line_counts` + the `range` descriptor; new field
on the internal `Change`; *(r2)* surface `stdout_incomplete`/`stdout_lossy` on diff/status and the
truncated-untracked-body warning; *(r3)* surface the `git ls-files` stream flags), `src/vcs/mod.rs`
(move the summary across in the adapter),
`src/vcs/shared.rs` (field on `CapturedChange`; the `range` fallback reuses `safe_label`),
`src/vcs/perforce.rs` (build the `Perforce` variant; *(r2)* the incomplete-segment warning),
`src/registry.rs` (field on `Outcome`/`Review`/`Snapshot`, threaded through `finish`/`of`),
`src/tools.rs` (take the value, extract the tag before the attempt, render the line),
`src/metrics.rs` (the optional `captured` tag field). Every touched site already has a
`disposition` line one line away, so the diff stays local despite the file count.

No security boundary moves: the summary reads capture data that already crosses the
backend→server seam; it adds no new file read and no new subprocess. The one string derived from
operator configuration — the git `range` — is sanitised and bounded at construction, so it is not
a new injection surface.

## Testing

- **`capture_summary.rs` unit tests**: `summary()` for git (shown counts; `complete` vs `partial`;
  the floor wording with the "at least" prefix on the diff figures for a `diff_incomplete` capture
  *and* on `untracked_files` for a short enumeration — asserting each floor fires only for its own
  count; the always-present `diff:` token in *both* an intact-diff partial and a diff-incomplete
  partial; zero vs present untracked) and Perforce (evidence-unit count as a floor when incomplete;
  `partial` from a skipped *or* an incomplete-but-not-skipped changelist, and the `N of M`
  phrasing); `tag()` stability and bounding, including `+d` implies `+p`; singular vs plural
  ("1 file" vs "12 files"), per the disposition doc's precedent.
- **`diff_line_counts` unit tests** (git): plain multi-file diff; rename-only (file counted, zero
  lines); binary (file counted, zero lines); `+++`/`--- ` headers excluded; empty diff (0/0/0).
- **`range` descriptor tests** (git): a pinned range renders resolved hex endpoints; working-tree
  and staged render the fixed strings; a non-resolving / fixed-window mode renders the *sanitised*
  configured spelling (assert a control char or backtick does not survive).
- **Completeness construction tests** — the gaps both review rounds named:
  - Perforce: `skipped == 0`, `diff_truncated == false`, but a segment `complete == false` yields
    `complete == false`, `incomplete_changelists == 1`, renders `partial`, **and** emits the new
    caller warning.
  - Git: a truncated `status`; a `git status` that did not run; an untracked listing cut short;
    *(r2)* an included untracked file with `body.truncated == true`; *(r2)* a diff whose
    `RunOutcome` reports `stdout_incomplete`/`stdout_lossy`; *(r3)* a `git ls-files` that returned
    success but `stdout_incomplete`/`stdout_lossy`/`stdout_truncated` — each yields
    `complete == false` with a matching caller warning while the diff bytes are otherwise intact.
  - *(r3)* The excluded case, guarding the contract boundary: a binary/out-of-root untracked file
    the enumeration *reached* keeps `complete == true` and `Capture::warnings` empty — i.e. the
    existing `an_ordinary_skipped_file_does_not_warn_the_caller` still passes unchanged, and the new
    `complete` field agrees with it.
  - The invariant itself: for each partial case above, assert `Capture::warnings` is non-empty
    (the "see warnings below" pointer is never dangling).
- **`render_completed` tests** (`src/tools.rs`): the `captured:` line appears on a fresh completed
  turn (where no `disposition:` line does); it sits after `usage:` and before `disposition:`, with
  warnings after it; it is absent when the snapshot carried no change.
- **Golden prompt test** (`src/vcs/mod.rs`): unchanged, asserted to still pass — proof the prompt
  did not move despite the new caller warnings.
- **Metrics tests** (`src/metrics.rs`): a record round-trips the `captured` tag; a no-change record
  omits it; a **failed** turn that captured a change still records the tag.
- *(r2)* **Smoke run required.** Adding a caller-visible response line changes the tool's observable
  output, and [`AGENTS.md`](../AGENTS.md) requires an end-to-end `smoke.ps1` round trip when the
  interface changes. Run `smoke.ps1 -Reviewer claude` (the direction where the server captures and
  the line appears) and note to the user that it calls a model for real and costs tokens. No new
  smoke script is needed — the existing one exercises the round trip.

## Decisions (formerly open questions)

Resolved with the round-1 reviewer; recorded so the implementation does not reopen them.

1. **Command form for the git line — resolved SHAs.** The `range` shows resolved hex endpoints for
   a pinned range, fixed strings for working-tree/staged, and the *sanitised* configured spelling
   only for non-resolving modes. The configured `main...HEAD` spelling is not shown alongside — a
   second source of truth would be a drift risk for no gain over the resolved base commit.
2. **Untracked files — separate figure.** Kept out of the `files` count, because the `+/-` figures
   cannot include untracked content.
3. **Evidence-unit count on an elided Perforce turn — total units.** The line reports total units;
   the resent/collapsed split stays the disposition's job, now that `complete` and
   `incomplete_changelists` surface incompleteness independently.
4. **Metrics tag — in scope, carrying identity not size.** Kept for audit symmetry with the
   disposition tag, redesigned to carry the resolved range / changelist identity plus
   truncation/partial markers, and extracted before the reviewer attempt so failed turns are
   logged. See [The metrics tag](#the-metrics-tag).
