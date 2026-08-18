# cross_model_consult — a lightweight second opinion

Status: **implemented (v1, tree-only).** This document is the design of record; the sections below
describe the full design, and the note immediately below records what v1 actually ships.

> **v1 scope — tree-only (`include_change` deferred).** The shipped consult is **tree-only**: it
> captures no diff and reads the repository through the read-only evidence service. The
> `include_change: true` capture contract described under [Change capture](#change-capture) (finding
> f2) is **not implemented in v1** — the tool advertises no `include_change` argument, and a caller
> cannot request one (the schema is `additionalProperties: false`). It is a deliberate scope cut, not
> an oversight: the common consult ("does this direction look right?", "where is X?") is tree-only,
> and the capture contract + resume binding are enough machinery to be their own PR. The
> [Change capture](#change-capture) section below is retained as the design for that follow-up. Every
> other section describes v1 as shipped.

Revision 2 folded in the cross-review of revision 1 (session `cross-model-consult-plan`, 8 findings,
all accepted). The through-line of that review: *resumability*, not the ledger, is what pulls in the
durability and cost guards — and a consult is resumable, so it inherits more of the existing
machinery than revision 1 credited. The net effect is a **smaller, more conservative** plan that
reuses more and invents less.

Revision 3 folds in the turn-2 re-review, which resolved 6 of the 8 (including both recommendation
flips) and went deeper on two: f2 and f3 rested on *reuse* claims the code does not actually support
(the capture binding does not cover the git diff endpoint or `include_change`; the evidence gate did
not account for fallback-chain fall-through). Three precision findings followed (f9–f11). All
accepted; the corrections are wiring precision and two real correctness fixes, not a redesign.

## What it is

A second, informal way to reach the other model: not a gated review that must converge, but a
question. "Does this direction look right?" "Where is X handled in this code?" "Am I missing a
simpler approach here?" It is to `cross_model_review` what a quick word over the shoulder is to a
sign-off: same person, no ceremony, nothing to certify.

The value is the same as the review's — a *different* model with different blind spots — pointed at
a lighter job. The review answers "is this correct enough to merge"; the consult answers "help me
think about this." They are different questions and should be different tools.

## Why a new tool, not a mode of `cross_model_review`

`cross_model_review`'s entire response contract exists to serve convergence: `outcome`
(`converged`/`changes_requested`/`escalate`/`rebaseline`), the `findings` ledger with stable ids and
dispositions, the `converged` signal, block repair, stagnation. A consult certifies nothing, so
every one of those fields would be conditionally-meaningless under a `casual: true` flag, and the
tool description would have to teach two protocols at once.

A separate tool with a *tiny* contract — question in, prose out — keeps both honest. This is the
scope-cut counterweight `AGENTS.md` names explicitly ("the gate ratchets toward rigor, and you are
the counterweight"): the right move here is to *not* build machinery, and to reuse only the parts
that are genuinely orthogonal to the review contract.

## Where it sits on the rigor line

`AGENTS.md` draws the line: rigor belongs where the blast radius is real (accounts, credentials, the
read/write boundary, isolation). For a *review*, review-integrity is also on that side, because a
review that is quietly thinner than it looks produces a **false approval** — not merely a lost
review.

A consult makes no approval claim. A thinner-than-it-looks consult is just a *worse answer*, which is
the "worst case is re-run" category where rigor should be light. So the design splits cleanly:

- **Kept hard (security boundaries, unchanged):** reviewer isolation (config isolation, read-only,
  no-shell-for-Claude / OS-sandbox-for-Codex), the read-only evidence service and its path scoping,
  account-identity binding, job-object process reaping.
- **Kept because resumability needs them, not the ledger (revision 2 correction):** the durable
  write-ahead turn guard, and the `--session-max-turns` / `--session-max-idle-seconds` cost/drift
  limits. See below — revision 1 wrongly filed these under "ledger machinery."
- **Dropped (genuinely review-integrity, meaningless here):** the findings ledger, convergence /
  `outcome`, block repair, the *stagnation* gate (`--stagnant-session-turns`), and the
  capture-completeness fail-closed *refusal* path.

## Tool surface

Add **two** new tools. Reuse the rest.

- **`cross_model_consult`** *(new)* — start a consult. Returns a `review_id` immediately, exactly as
  `cross_model_review` does. Arguments:
  - `question` (required) — what you want a second opinion on, or want found. Replaces
    `instructions`; same free-text channel, no diff pasted in (see capture below).
  - `session` (optional) — name it to make follow-ups resumable, same as reviews.
  - `fresh` (optional) — restart a named session's conversation.
  - `include_change` (optional, **default false**) — bound at session creation; see "Change capture."
  - `level` (optional) — reuses the existing `--level` presets and **the review's exact resolution
    order**: a passed level, else `--default-level`, else the entry's base `--model`/`--effort`.
    There is no promised `"standard"` default (**f8**) — whatever the review path would resolve to is
    what a consult resolves to.
  - Perforce `change` / `include_shelved` — only meaningful when `include_change` is true; identical
    semantics to reviews, and part of the capture identity bound at session creation.
- **`cross_model_consult_result`** *(new — revision 2 change, was "reuse `_result`")* — wait for and
  return the consult. **f4** killed the polymorphic-reuse idea: `cross_model_review_result`'s output
  is a strict running/completed *review* union (`additionalProperties: false`, `src/findings.rs`),
  documented as "switch on `outcome`." Retrofitting a discriminated union onto that strict schema is
  uglier than a second small waiter that shares the same underlying `Registry::wait` plumbing but
  returns the consult envelope. So: dedicated result tool, shared waiter implementation.
- **`cross_model_review_cancel`** *(reused, unchanged)* — generic infra; stops a running job by id,
  whatever its kind.
- **`cross_model_review_status`** *(reused, unchanged)* — CLI + auth check, costs nothing.

### The dedicated result tool is a full contract, not just a second entry point (f9)

Adding a dedicated `cross_model_consult_result` creates real wiring the plan must specify, because the
existing dispatch and progress paths special-case `cross_model_review_result` by name (`src/mcp.rs`,
the progress-token handling and the machine-envelope carve-out) and `Registry::Review` has no job
kind (`src/registry.rs`). Revision 3 pins:

- **A job kind on the registry.** The registry job carries `kind` (`review` | `consult`) — either a
  field on the existing job or a `Registry::Consult` variant — so a waiter knows what it is collecting
  and the reaper/cancel paths stay kind-agnostic.
- **Cross-kind id rejection, both ways.** A `review_id` that names a *review* job, passed to
  `cross_model_consult_result`, is refused with a clear error, and a consult id passed to
  `cross_model_review_result` likewise. An id is never silently collected through the wrong envelope.
  Resolution by `session` name resolves within the tool's own kind.
- **Input schema** — `review_id` or `session` plus `wait_seconds`, mirroring
  `cross_model_review_result`.
- **Running / completed structured schemas** — the consult envelope below is its *own* strict schema
  (a running variant with `status=running`, a completed variant with `answer` etc.), not a branch of
  the review union.
- **Progress + cancellation** — the same `notifications/progress` emission the review collect uses is
  generalized to fire for a consult collect (the current by-name special-case is widened to both
  result tools); cancellation is already generic (`cross_model_review_cancel` by id).

**Kind-awareness must reach every by-name special-case, not just the dispatcher (f9, turn 3).** The
re-review enumerated the review-specific call-sites the first pass missed; the plan pins them as the
implementation checklist so none is left review-shaped:

- the main tool dispatch and the progress-token carve-out (`src/mcp.rs`);
- **progress-by-session's untyped latest-job lookup** (`src/tools.rs` ~1476) — must resolve within the
  requesting tool's kind, or a `session`-addressed consult progress request could bind a review job;
- the **running-status remediation text** that tells callers to call `cross_model_review_result`
  (`src/tools.rs` ~1410) — a consult must name its own result tool;
- the **completed-status text** that assumes findings/outcome and review continuation
  (`src/tools.rs` ~1633) — a consult renders prose and its own follow-up guidance.

Each is either made kind-aware or given a consult variant; the shared `Registry::wait` plumbing
underneath is unchanged.

## Response shape

A consult envelope is prose-first and small:

- `answer` — the model's response, verbatim. This is the whole point; there is no machine record to
  read instead.
- `denials` / `denial_count` / `denial_count_is_floor` — commands/reads the reviewer was refused,
  and whether that number is exact or a lower bound (reused as-is). The floor flag travels with the
  count in **both** channels (**f11**): the existing contract treats the count as a lower bound when
  collection output is capped (`src/registry.rs`, `src/findings.rs`), so a consult that surfaced the
  count without it would let a client misread an inexact number as exact.
- `warnings` — anything qualifying the turn (a short/stale capture lands here, never as a refusal).
- `usage` — cost of the turn (reused metrics path; consults are recorded like any turn).
- `reviewer`, `resumed`, `resumable` — which entry ran, whether it continued a prior consult, whether
  a follow-up will resume.

No `outcome`, no `findings`, no `converged`, no `disposition`. `structuredContent` carries the same
fields as the text body (the parity discipline from issue #73 still applies — a structured-only
client must not get a poorer answer).

**`evidence_read` is cut (f7).** Revision 1 proposed surfacing which files the reviewer opened, for
trust calibration. The parsers do not retain that today (Claude records evidence *health* not paths;
Codex counts evidence calls without their arguments; built-in Claude reads and Codex shell reads are
outside the trace entirely). Making the field mean "everything actually read" would need new
instrumentation this feature has not budgeted, and a field that silently means less than it says is
the exact failure mode this project avoids. Dropped. If a cheap signal is wanted, the existing
evidence-service *request count* (already surfaced as a warning today) is honest and free; a consult
may echo that, named for what it is, but nothing claims to be a complete read audit.

## Sessions and resume

Reuse `SessionRecord` wholesale for identity and resume: `cli_session_id`, `reviewer`, `model`,
`effort`, `cwd`, `profile_identity`, `raw_bin`, `resolved_bin`. A consult session simply **never
populates `findings_ledger` or `terminal_reason`**, and the ledger/coverage/stagnation path is not
run for it.

Integrity points, all fail-closed (this is session-record identity, which *is* on the rigor line):

1. **A `kind` discriminator on `SessionRecord`** (`"review"` | `"consult"`). A resume must match kind:
   a `cross_model_review` resume of a consult session, or a `cross_model_consult` resume of a review
   session, is **refused** rather than continued against a conversation shaped for the other protocol.
   Legacy records with no `kind` are treated as `"review"` (the only kind that existed).
2. **Identity binding is unchanged.** A consult resume whose freshly-resolved account/profile,
   configured binary, or resolved binary does not match the stored one is refused exactly as a review
   resume is.
3. **The durable write-ahead turn guard is retained (f1), with its existing lifecycle intact (f10).**
   The findings-pending marker written before every reviewer turn is *not* ledger machinery — it is
   the guard that stops the reviewer's CLI conversation advancing while the stored record does not
   reflect it, which a crash or a failed persist would otherwise leave as a resumable-looking stale
   session. A consult resumes, so it needs the same guard, retained under a neutral (non-findings)
   name. The blast radius for a consult is smaller than for a review — a lost or duplicated *turn*,
   re-run, not a false approval — so this is retention of an existing guard, **not** a licence to
   build consult-specific durability machinery around it.

   Its lifecycle is the *existing* one, not a simplified "cleared only after `record_turn`" (that was
   revision 2's imprecision). Two properties must carry over exactly (`src/tools.rs`, `src/session.rs`):
   - **The no-child-started cleanup exception.** An invocation or spawn that fails *before a child
     could advance the conversation* clears the marker safely — nothing was put at risk, so stranding
     the session would be the bug. It is cleared after `record_turn` on the success path *and* on
     these pre-child failures.
   - **Marker separation.** The turn guard stays distinct from the Perforce `.pending` baseline
     marker; they protect different things and must not be conflated on the consult path.
   - **The on-disk filename does not change (f10, turn 3).** "Neutral name" is about how the plan
     *describes* the guard, not a rename of the durable file. The marker stays at its existing path
     (`.findings-pending`, `src/session.rs`) so a marker written before the upgrade is still seen
     after it — renaming the file would make old pending markers invisible and silently defeat the
     fail-closed resume protection this guard exists for.

### Turn and idle limits are kept (f6)

Revision 1 proposed dropping `--session-max-turns` and `--session-max-idle-seconds` for consults.
That was wrong: those limits guard *growing prompt cost and context drift* (resume re-sends the whole
conversation — measured ~190k → ~970k tokens over six turns), which is a property of any resumable
conversation, not of the ledger. They are **kept for consults**, with the same configurable knobs
(either can be set to `0` to disable). A consult past either limit is refused a resume and the caller
starts `fresh`, exactly as a review is.

Only `--stagnant-session-turns` is genuinely ledger-coupled — "turns without raising or resolving a
finding" is undefined without findings — so **stagnation alone is dropped** for consults.

Separately, the reviewer CLI's own session lifetime still applies underneath all of this: a resume can
come back `SESSION_NOT_FOUND` / `SESSION_NOT_RESUMABLE` when the CLI expired the conversation, and the
consult surfaces that plainly so the caller retries `fresh`.

## Change capture

Default **off**, and **bound at session creation via a new explicit capture contract (f2)**. Most
consults ("where is X", "does this approach make sense") are about the tree or a direction, not a
diff, and the evidence service lets the reviewer read whatever it needs.

Revision 2 claimed this could *reuse* the review path's existing resume binding. That was wrong, and
the re-review caught it: `SessionRecord` today persists the *Perforce* bindings (`changes`,
`include_shelved`, `capture_identity`, `perforce_baseline`) and git `head_sha`/`base_sha`, but it
carries **no `include_change` field and no git `--diff` endpoint / `DiffMode`**, and the resume gate
(`resume_block`) checks neither (`src/session.rs`, `src/tools.rs`, `src/vcs/git.rs`). So a resumed
consult could silently continue an old conversation against a *different* git diff mode. There is no
existing binding to reuse for the git case; it has to be built.

So revision 3 persists an **explicit consult capture contract** on the session record and compares it
on resume:

- `include_change` (bool), and
- when `include_change` is true and the backend is git: the resolved `--diff` endpoint / `DiffMode`;
- when Perforce: the existing changelist-set + `include_shelved` + `capture_identity` bindings, which
  already exist and already refuse a changed set on resume.

A resume whose freshly-resolved capture contract differs from the persisted one is **refused** (use
`fresh: true`), rather than mixing a conversation with a capture it was not built against.

**Bound vs. per-turn drift, made explicit (f2, turn 3).** The re-review noted that capture is decided
by more than `DiffMode` — the `auto` decision also reads reviewer shell capability, isolation, and
chain composition, and `resume_incremental_diff` swaps full-vs-delta capture per turn. Drawing the
line the same place reviews already draw it:

- **Bound** (a change refuses the resume): `include_change`; the *configured* git `--diff` mode; the
  Perforce changelist set + `include_shelved`. These are caller/config intent.
- **Allowed to drift per turn, reused verbatim from the review path** (*not* bound): the resolved
  `HEAD`/`base` and the incremental-vs-full delta. Reviews already advance `head_sha` every turn and
  delta against it; a consult inherits that behaviour unchanged rather than freezing it, because it is
  a per-turn *optimisation* of the same configured mode, not a different mode.
- **`include_change: true` never silently yields no diff.** An empty or `auto`-suppressed capture is
  reported as a `warning` (the review path's existing empty-capture reporting), so the caller is never
  told a change was shown when none was.

**Perforce + `include_change: false` needs its own semantics (f2).** The review path requires a
`change` argument unconditionally for Perforce (`src/tools.rs`), which collides with consult's
default-off. Resolution: for a consult, `change` is required **only when `include_change: true`**. An
`include_change: false` consult is a *tree-only* consult — no changelist capture at all; the reviewer
reads the workspace through the evidence tools, exactly as a git tree-only consult does. This is a
consult-path rule, so it does not touch the review path's unconditional requirement.

When `include_change: true`, the capture pipeline itself is reused verbatim (`--diff`/`--vcs`, the
git/Perforce backends, truncation caps, the "labelled as evidence not instructions" fencing). A
consult never pastes a diff into `question`, same rule as reviews.

The capture-completeness **fail-closed refusal** path is *not* inherited: a short or stale capture on a
consult is a `warning`, never a refusal — there is no false-approval to guard against. (The
stale-local-`main` foot-gun still warrants its warning so the caller isn't misled about what was read.)

## Evidence service

The evidence service is the *entire* value of a consult — it is how the model reads code to answer.
So it is **required**, and consult eligibility is gated on it (f3).

The evidence service does not exist for every reviewer configuration: it is present for the Codex
reviewer and for a profile-pinned, shell-less Claude reviewer, but an *ambient* or *shell-enabled*
Claude reviewer deliberately runs without it (README "The Claude reviewer's read-only evidence
service"). A consult targeting a configuration that cannot provide the evidence service is
**refused before the model is spawned, with `EVIDENCE_UNAVAILABLE`** — the same fail-closed code the
review path uses. There is no silent evidence-less consult.

**The gate is on the whole reachable chain, not just the active entry (f3).** Revision 2 said "checked
at the call," which the re-review showed is insufficient: a fresh review advances through the fallback
chain on a rate limit (`src/tools.rs`), and evidence setup is decided *per active entry*. A consult
that started on evidence-capable Codex could fall through to an ambient Claude with no evidence
service and run evidence-less — precisely the guarantee this section makes, violated.

**"Reachable" defined conservatively (f3, turn 3).** The proactive usage gate makes reachability
dynamic — it *skips* entries at selection, and headroom observed during one attempt changes which
entry a later fallback picks — so a snapshot taken at spawn can miss an entry that becomes reachable
after a rate-limit fallback. The decided rule avoids the atomicity problem instead of solving it: for
a fresh consult, **reachable = the entry selected now plus every entry after it in the chain**,
evaluated **ignoring the usage gate**. That is a static superset, and it is sound precisely because
the usage gate only ever *removes* entries from play — it can never make an entry reachable that isn't
already in this set — so no pre-spawn recheck is needed and there is no window to race. The first
ineligible entry in that set refuses the consult up front with `EVIDENCE_UNAVAILABLE`, naming it. A
resume binds to the one entry that created the session and checks only that entry. Tests cover a
usage-gated start, a rate-limit fall-through to a later entry, and headroom changing mid-attempt.

(This is a *consult-time* eligibility check over the chain, not a startup rejection of the chain
itself — the same chain stays valid for reviews, which do not require evidence of every entry.)

What does **not** apply is the review's runtime gate that requires the reviewer to have *read the
captured change* before the turn is trusted. A consult with `include_change: false` has no change to
verify reading of; requiring it to read something specific would defeat the "just ask a question"
purpose. The read boundary (path scoping, no writes, no shell for Claude) is unchanged — only the
"you must have consumed the diff" liveness check is absent.

## Prompt protocol (f5)

A consult needs its **own preamble**; it cannot reuse the review preamble. The existing prompt
(`src/prompt.rs`) identifies the model as a *code reviewer*, instructs it to emit a machine-readable
findings block, and its follow-up guidance assumes prior findings to reconcile. Reused verbatim, that
would produce review-shaped output the consult path neither parses, validates, nor repairs.

So the consult path supplies:

- a **prose-only preamble** that frames the model as a second pair of eyes answering a question, asks
  for a direct prose answer, and **does not request a findings block** (there is no block extraction,
  no block repair on this path);
- **follow-up guidance for resumes** that references the prior conversation generally, not a findings
  ledger ("you were asked X and said Y; here is the next question");
- the same **evidence-tool direction** the review preamble gives (how to discover, read, search, and
  page the change), because that vocabulary is exactly what a consult needs.

The existing `--preamble-file` / `--no-preamble` overrides apply to the consult preamble too.

## Concurrency

Consults share the `--max-concurrent-reviews` cap (same spawn cost, same backstop against a caller
that starts jobs and abandons the polls). No separate limit.

## Testing

- **Unit tests** — the `kind` discriminator and cross-kind resume refusal; the retained write-ahead
  guard on the consult path; `include_change` capture-identity binding and refusal-on-change;
  turn/idle limits enforced while stagnation is not; the `EVIDENCE_UNAVAILABLE` eligibility gate for
  a non-evidence reviewer config; consult envelope shape (no `outcome`/`findings`); level resolution
  matching the review path exactly.
- **`smoke.ps1` — both directions.** This touches spawn, the evidence service, isolation, and now a
  distinct preamble/prompt path, which `AGENTS.md` classifies as protocol needing the real round
  trip. A consult round trip must pass under `-Reviewer codex` **and** `-Reviewer claude`.
  `build.ps1` (which never starts a reviewer) is not a substitute.

## What this plan deliberately refuses to add

Stated so a later round does not quietly re-grow them:

- No findings ledger, and no "wouldn't it be nice to track what it found." The moment a consult
  certifies or tracks anything, it becomes a worse copy of the review that already exists.
- No convergence, no `outcome`, no re-review coverage semantics, no stagnation gate.
- No capture-completeness refusals (warnings only).
- No `evidence_read` audit (f7) — no new instrumentation to make a field mean more than the data
  supports.
- No new level rules (f8), no new concurrency limit, no new isolation posture — every security
  boundary is the review's, unchanged.

## What the cross-review changed

Revision 1 → 2 (turn 1, 8 findings, all accepted):

| # | Finding | Revision 2 | Turn-2 status |
| --- | --- | --- | --- |
| f1 | write-ahead guard is session-durability, not ledger | Guard retained under a neutral name | resolved (lifecycle refined in f10) |
| f2 | `include_change` had no resume binding | Claimed reuse of existing binding | **still open** → rev 3 |
| f3 | evidence service absent for some reviewer configs | Gated at the call | **still open** → rev 3 |
| f4 | polymorphic `_result` reuse underspecified | Flipped to dedicated `cross_model_consult_result` | resolved (wiring gap → f9) |
| f5 | no consult prompt protocol | Added prose-only preamble + resume guidance | resolved |
| f6 | turn/idle limits are not ledger-only | Kept for consults; only stagnation dropped | resolved |
| f7 | `evidence_read` cannot mean "all files read" | Field cut | resolved |
| f8 | no guaranteed `standard` level default | Reuse review level resolution exactly | resolved |

Revision 2 → 3 (turn 2 — two open, three new, all accepted):

| # | Finding | Revision 3 |
| --- | --- | --- |
| f2 | git diff endpoint / `include_change` are **not** persisted or resume-checked today; Perforce `change` is unconditional | Persist an explicit consult capture contract and compare on resume; Perforce `change` required only when `include_change: true`; `include_change: false` is tree-only |
| f3 | fresh consult can fall through the chain to an evidence-less entry | Gate over the *whole reachable chain*; first ineligible entry refuses `EVIDENCE_UNAVAILABLE` |
| f9 | dedicated result tool underspecified | Registry job `kind`, cross-kind id rejection both ways, input/running/completed schemas, generalized progress |
| f10 | marker "cleared only after `record_turn`" was imprecise | Preserve the no-child-started cleanup exception and marker separation |
| f11 | envelope omitted `denial_count_is_floor` | Floor flag travels with the count in both channels |

Turn 3 (f11 resolved; f2/f3/f9/f10 held open on a *deeper* layer of existing call-sites — decided, not
expanded):

| # | Deeper gap the re-review found | Revision 3's decided position |
| --- | --- | --- |
| f2 | capture also depends on the `auto` decision + per-turn incremental delta, not just `DiffMode` | Bind configured mode + `include_change` + Perforce set; let resolved HEAD/base and delta drift per turn exactly as reviews do; `include_change: true` warns on empty capture |
| f3 | "every reachable entry" is ambiguous under the dynamic usage gate | Reachable = selected entry + all later chain entries, evaluated *ignoring* the usage gate (a static superset, sound because the gate only removes entries) |
| f9 | more review-specific by-name sites (progress-by-session, running/completed remediation text) | Enumerated as the kind-awareness checklist; each made kind-aware or given a consult variant |
| f10 | a renamed marker file would hide pre-upgrade markers | On-disk filename unchanged; "neutral name" is descriptive only |

## Design status

After three rounds the **design** is settled: the rigor split, the two-tool shape, the
reuse-vs-build boundary, and the session / capture / evidence / prompt models all held, and the
turn-3 items are decided above. What the re-review is now surfacing is *implementation completeness* —
enumerating existing call-sites and dynamic-interaction edges, each pinned to a `file:line`. That is a
better input to coding-with-tests under the normal PR gate than to further plan prose, and continuing
to grind the doc is the ratchet `AGENTS.md` names. The four items above therefore stand as the
**implementation punch-list**, to be resolved in code under the ordinary cross-review merge gate, not
by another revision of this document.
