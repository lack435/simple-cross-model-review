# cross_model_consult — `include_change: true` implementation plan (issue #105)

Status: **plan, pre-implementation.** This is the implementation punch-list for the
`include_change` capture contract that `docs/cross-model-consult-plan.md` deferred out of the v1
(tree-only) consult. The design is settled there (finding **f2**, plus the bound-vs-drift line in
its turn-3 table); this document does not re-open it. It enumerates the existing call-sites to
change, pins each to a `file:line`, and records the two decisions the code forces that the design
prose did not pin.

Read alongside `docs/cross-model-consult-plan.md` (§ *Change capture*) and `AGENTS.md`
(§ *How much rigor, and where* — a consult certifies nothing, so a thin capture on it is a
*warning*, never a refusal).

## What ships in v1 today, and why this is a separate PR

The shipped consult is tree-only: `start()` **rejects** `include_change`, `change`, and
`include_shelved` server-side (`src/tools.rs:814`), the worker hard-wires `vcs::Capture::empty()`
for a consult (`src/tools.rs:2614`), and `SessionRecord` carries no capture contract for the git
case. Everything below is either plumbing an existing capability past that rejection, or building
the **one genuinely new binding** the design flagged: the git `--diff` mode has no resume binding
to reuse (`SessionRecord` persists Perforce `changes`/`include_shelved`/`capture_identity` and git
`head_sha`/`base_sha`, but no configured `DiffMode` and no `include_change`), so a resumed consult
could otherwise continue an old conversation against a different configured capture.

## Scope of this PR

Both backends. The issue text covers git **and** Perforce; the Perforce inputs are already parsed
for reviews, so gating them on `include_change` for a consult is a small delta, not a second
project. If the reviewer judges the Perforce `include_shelved` decision (below) large enough to
split, that is the natural seam — git-only here, Perforce `include_change` as a follow-up — but the
default is both.

## Work items

### 1. Schema — advertise the inputs (`src/mcp.rs`)

- Add `include_change` (boolean, default false) to the consult `inputSchema.properties`
  (`src/mcp.rs:898`). Describe it as: off by default (tree-only, read through the evidence
  service); when true, the reviewer is *also* shown the configured change, exactly as a review is.
- The Perforce-only block at `src/mcp.rs:1128` currently injects `change`/`include_shelved` into
  `tools[0]` (the review tool) only. Extend it to inject the two properties into the consult tool as
  well — matched **by name**, not index, mirroring the level-injection loop at `src/mcp.rs:1194`
  (robust to tool ordering).
- **The `change` description must differ for a consult (f6).** The review description at
  `src/mcp.rs:1136-1142` begins "Required." and describes a review; injected unchanged into the
  consult schema it would misstate the consult contract, where `change` is *rejected* when
  `include_change` is false and *required only when* `include_change` is true. Give the consult its
  own conditional description saying exactly that. Do not reuse the review string.
- **Do not** push `change` into the consult tool's `required` array. For a review `change` is
  unconditional (`src/mcp.rs:1160`); for a consult it is required only when `include_change: true`,
  which the schema cannot express as a flat `required` entry — the runtime is the real validator
  (item 2), and the property description states the conditional requirement in prose.
- Update the consult tool description (`src/mcp.rs:884`) so it no longer implies tree-only-always;
  note `include_change` is available and defaults off.

### 2. Parsing — conditional acceptance, not just conditional requirement (`src/tools.rs`, in `start`)

The v1 rejection loop (`src/tools.rs:814`) refuses `include_change`, `change`, and `include_shelved`
for any consult. The naive change — drop only `include_change` from the loop — is **wrong**, and the
plan review caught it (f1): it would leave `change`/`include_shelved` *acceptable on an
`include_change: false` consult*, where the worker skips capture but `start` still reports the
changelists (`src/tools.rs:1320`), the session still persists the changelist set + shelved flag
(`src/tools.rs:4103-4117`), and a later tree-only follow-up that omits the now-irrelevant changelist
is then refused by the changelist-set binding in `resume_block` (`src/tools.rs:4524-4541`). A
tree-only consult must carry **no** capture state at all.

So the rule is **conditional acceptance**, keyed on `include_change`:

- Parse `include_change` first, strictly (a JSON `null`/absent is `false`; a present non-boolean is a
  `bad_request`), mirroring the `include_shelved` parse at `src/tools.rs:876`.
- **When `include_change` is false** (or absent — the default), keep rejecting `change` and
  `include_shelved` for a consult exactly as v1 does. A tree-only consult takes none of the capture
  args, so nothing is reported, persisted, or later refused against. This is the current behaviour,
  narrowed from "reject all three" to "reject the two capture inputs."
- **When `include_change` is true**, accept `change`/`include_shelved` on the same terms a review
  does. On Perforce, `change` becomes **required** (the existing `!is_consult && Perforce &&
  changes.is_empty()` check at `src/tools.rs:887` stays as-is for reviews; add a consult arm
  requiring `change` when `include_change` and `changes.is_empty()`, with a message naming
  `include_change: true` as the reason). On git, `change`/`include_shelved` are still rejected by the
  existing git-backend guard (`src/tools.rs:862`) — git has no changelists regardless.
- Keep the git-backend rejection of `change`/`include_shelved` (`src/tools.rs:862`) unchanged: the
  pointed "wrong backend" message stays correct.
- Thread `include_change` into the job struct (item 3).

### 3. Capture — one predicate gates *every* capture branch and persisted field (`src/tools.rs`, worker)

- Add an `include_change: bool` field to the job struct (beside `include_shelved`, ~`src/tools.rs:2272`)
  and set it from the parsed value in `start` where the job is constructed (~`src/tools.rs:1229`).
- **Define one predicate and use it everywhere:** `should_capture_change = !self.is_consult() ||
  self.include_change`. The v1 guards are all spelled `!self.is_consult()`, which now means "not a
  review" where it should mean "this turn is capturing a change." Replacing each with
  `should_capture_change` is what keeps a *tree-only* consult (`is_consult && !include_change`)
  carrying **no** capture state while a change-capturing consult behaves exactly like a review.

  The plan review (f5, a follow-on from f1) showed that gating only the capture *call* is not enough:
  the worker's *automatic* Perforce bookkeeping also runs off `pending_marked`, which a tree-only
  consult leaves false, so it would persist `PerforceBaseline::Disabled` and emit a spurious "next
  incremental re-review could not be protected" warning, and the session record would persist the
  changelist set / shelved flag. **Every** capture branch and persisted capture field must move onto
  the one predicate:
  - the capture call itself (`src/tools.rs:2614`): `Capture::empty()` when `!should_capture_change`,
    else `vcs::capture(&self.cfg, &self.changes, self.include_shelved, resume, &self.cancel)`
    verbatim (same truncation caps and evidence-not-instructions fencing reviews use);
  - the Capturing phase (`src/tools.rs:2558`);
  - the Perforce in-progress marker read (`src/tools.rs:2570`) and `mark_pending` write
    (`src/tools.rs:2581`);
  - the `perforce_baseline` decision (`src/tools.rs:2639-2643`) — a tree-only consult must persist
    `None`, not `Disabled`;
  - the false-protection warning (`src/tools.rs:2670-2680`) — must not fire for a tree-only consult;
  - the persisted Perforce capture fields *the worker constructs* (`src/tools.rs:4103-4119`):
    `changes`, `include_shelved`, `capture_identity`, `perforce_baseline` must all be `None` for a
    tree-only consult, not the automatic config-derived values they take today (in particular
    `changes` must be `None`, not the `Some(canonical([]))` = `Some([])` that
    `(Perforce).then(|| …)` produces for an empty change set);
  - the unconditional `clear_pending` call (`src/tools.rs:4155-4164`), which today fires on any
    Perforce turn (plan review f7). A turn that does not participate in capture must not touch the
    Perforce in-progress marker it neither read nor wrote — the marker's fail-closed contract
    (`src/session.rs:584-608`) assumes only capture turns manage it. Gate it on
    `should_capture_change`; reviews keep their current behaviour. `backend` stays set unconditionally
    (it is session identity, not capture state).

**Passing `None` from the worker is necessary but not sufficient — the resume merge needs a
fail-closed refusal (f5, deeper layer).** `SessionStore::record_turn` merges a resumed turn's fields
with `.or(existing)` (`src/session.rs:497`, `505-507`): a `None` from the worker *inherits* the
stored value rather than clearing it. So the worker constructing `None` (above) guarantees a clear
only on a **fresh** turn, which takes the direct `turns: 1` branch (`src/session.rs:523`) with no
`.or`. A resume merges against whatever the record already holds.

A turn-4 review turned up the reachable case an earlier binding-only argument missed, and it is a
real one: **a v1 (pre-PR) Perforce consult record already carries capture state.** v1 rejects the
capture *arguments*, but the worker still runs the automatic Perforce bookkeeping, so such a record
persists `changes: Some([])`, `include_shelved: Some(false)`, and `perforce_baseline: Some(Disabled)`
(`src/tools.rs:2639-2643`, `4103-4119`). That record has `kind: Some("consult")` — so it resumes as a
consult — and `include_change: None`, which the legacy-compat rule would otherwise treat as *unbound*
and let resume tree-only, straight into the `.or(existing)` merge that retains the garbage. The
`include_change` binding does **not** close this path, because the record predates the field.

Resolution: **refuse it, do not silently merge or clear.** `resume_block` (item 5) refuses a
*tree-only* consult resume (effective `include_change` false, i.e. `None` or `false`) whose stored
record carries **any** capture field (`changes`, `include_shelved`, `capture_identity`,
`perforce_baseline`, `head_sha`/`base_sha` all `Some`) → `fresh: true` rebaseline. This is
fail-closed, localized to the already-consult-aware `resume_block`, and needs no clear-vs-inherit
mode on the shared `record_turn`. It fires only for a legacy Perforce consult session on its first
post-PR resume — a one-time rebaseline, exactly the disposable-session story `AGENTS.md` prescribes.
Post-PR the worker's `None` construction (above) means a genuine tree-only consult record never
carries capture state, so this refusal never fires for one. (The earlier "binding makes it
unreachable, add no clearing" argument was wrong precisely here; it is retracted.)

- A git consult with `include_change: true` needs no Perforce marker (those branches are already
  backend-gated); a Perforce consult with `include_change: true` **does** need its baseline marker,
  so it produces a resume-delta baseline like any Perforce turn. Both fall out of the single
  predicate — no per-branch special-casing.
- **Test both Perforce modes explicitly:** a tree-only Perforce consult persists no `changes`/
  `include_shelved`/`perforce_baseline` and emits no protection warning; a Perforce
  `include_change: true` consult persists them like a review.

### 4. Capture-completeness stays warning-only — but the empty-capture warning must be *added* (`src/tools.rs`)

The section-7 runtime liveness gate ("the reviewer must have read the captured change") stays
disarmed for a consult (`src/tools.rs:3535`, `capture_thin = !is_consult && …`) — a consult
certifies nothing, so nothing here becomes a refusal. That part is unchanged.

But the plan review (f2) showed my assumption that "the review path's existing empty-capture
reporting already lands in `warnings`" is **false for the cases that matter here**. An
`include_change: true` consult can produce no diff and answer *without any warning that no change was
shown*, because:

- `src/vcs/git.rs:451-452` returns an empty capture when `chain_needs_capture()` is false;
- a clean capture can have zero counts with no warning (`src/vcs/git.rs:598-701`);
- the consult disarms the liveness gate that would otherwise flag a thin capture
  (`src/tools.rs:3540-3544`);
- the consult renderer emits only `snapshot.warnings` (`src/tools.rs:1865-1897`), so if nothing put a
  warning there, none is shown.

The design (f2, turn 3) is explicit that "`include_change: true` never silently yields no diff." So
this PR must **add** an explicit warning: when `include_change: true` and the resolved capture is
absent or empty (git `auto`-suppressed, `chain_needs_capture()` false, or zero-count), push a warning
onto the turn — "you asked to include the change, but no diff was captured; the reviewer answered
from the tree alone." Keep it **warning-only** (never a refusal), on both channels. Test that this
warning fires and the consult still answers.

- The stale-local-`main` foot-gun warning still applies through the reused reporting; no
  consult-specific handling for that one.

### 5. The capture contract on the session record — the substantive part (f2)

This is the only genuinely new machinery. Kept minimal: persist the *configured* capture intent,
compare it on resume, refuse on mismatch. No versioning, no migration — a legacy record has `None`
`include_change`/`diff_mode` and is treated as unbound, consistent with every other optional field on
`SessionRecord` — **except** the one legacy shape that is not inert: a v1 Perforce consult record
that already carries capture bookkeeping (see the f5 refusal below and in item 3).

- **`DiffMode` must serialize.** It derives only `Clone, Debug, PartialEq, Eq` today
  (`src/vcs/git.rs:124`). Add `Serialize, Deserialize` (or persist it via a small
  `to_string`/`parse` round-trip — `DiffMode::parse` already exists at `src/vcs/git.rs:139` and is
  the natural inverse). Persisting the string form avoids leaking serde onto the enum and reuses the
  existing parser as the read path; that is the preferred option unless the reviewer sees a reason
  to derive serde directly.
- **Two new `SessionRecord` fields** (`src/session.rs:130`), both
  `#[serde(default, skip_serializing_if = "Option::is_none")]` for legacy-record compatibility:
  - `include_change: Option<bool>`,
  - `diff_mode: Option<String>` — the **configured** `cfg.diff` at session creation, *not* the
    per-turn resolved `head..base`. `None` for a review record (this contract is consult-only) or a
    record predating the field.
- **Persist them every turn** through `TurnRecord`/`record_turn` (`src/session.rs:322`), following
  the pattern the Perforce bindings already use (written each turn, `.or(existing)` where a binding
  should persist across turns). `diff_mode`/`include_change` are invariants for the session's life,
  so persist-then-inherit like `changes`, not advance-every-turn like `head_sha`.
- **Refuse on mismatch in `resume_block`** (`src/tools.rs:4447`). Add a capture-contract check
  (after the backend check at `src/tools.rs:4513`, before the terminal/staleness checks). For a
  consult resume. **Compare *effective* `include_change`, not raw `Option`s** (reviewer note, turn 5):
  a stored `None` and a requested-absent both mean `false`, so normalize `None → false` on both sides
  before comparing — else a legacy *git* tree-only consult (`include_change: None`, no capture
  fields) would be falsely refused against a requested `false`.
  - if the record's effective `include_change` differs from this call's effective `include_change`
    → refuse (`fresh: true`);
  - if `include_change` is true and backend is git and the record's `diff_mode` differs from the
    current `cfg.diff` (compared in canonical string form) → refuse;
  - **(f5) if the effective `include_change` is tree-only (record's `include_change` is `None` or
    `false`) and the record carries *any* capture field (`changes`, `include_shelved`,
    `capture_identity`, `perforce_baseline`, `head_sha`, `base_sha` is `Some`) → refuse.** This is
    the legacy-v1-Perforce-consult case from item 3: a genuine post-PR tree-only consult record never
    carries capture state (the worker constructs `None`), so this only fires for a pre-PR record on
    its first resume, which rebaselines.
  Message names the capture contract as the reason and points at `fresh: true`, matching the tone of
  the existing changelist-set refusal (`src/tools.rs:4531`).
- **Bound vs. drift (f2, turn 3) — do not over-bind.** Bound (refuses on change): `include_change`,
  the *configured* git `DiffMode`, the Perforce changelist set (already bound) + `include_shelved`
  (see decision D2). Allowed to drift per turn, reused verbatim from the review path: the resolved
  `head_sha`/`base_sha` and the incremental-vs-full delta. These advance every turn for a review and
  must keep advancing for a consult — they are a per-turn optimisation of the same configured mode,
  not a different mode. Do **not** freeze them into the contract.

### 6. Design-of-record correction (`docs/cross-model-consult-plan.md`)

Per decision D2 (below) and plan-review f3, reclassify the `include_shelved` entry in the change-
capture bound/drift treatment from "bound (refuses resume)" to "allowed scope drift, made safe by
full re-capture," so the design doc and this implementation do not document two contradictory
contracts. One-line correction, no design re-opening.

### 7. Tests

- **Unit (`src/tools.rs`, `src/session.rs`):**
  - `resume_block` refuses when stored `include_change` ≠ requested; resumes when equal.
  - `resume_block` refuses when `include_change: true` and stored `diff_mode` ≠ current `cfg.diff`;
    resumes when equal; ignores `diff_mode` when `include_change: false`.
  - the capture-contract fields are *unbound-when-absent*: a **consult-kind** record whose
    `include_change`/`diff_mode` are `None` **and which carries no capture fields** (a git-shaped
    tree-only consult) still resumes a tree-only consult. NB (plan review f4): do **not** write this
    as a `kind: None` "legacy consult" record — a `kind: None` record reads as `KIND_REVIEW`
    (`src/session.rs:267-273`) and `resume_block` refuses it cross-kind *before* the capture-contract
    check ever runs, so such a test would pass for the wrong reason. A legacy record can only be a
    review; construct an explicit consult-kind record missing only the new fields to exercise the
    unbound-when-absent path.
  - Perforce `change` required only when `include_change: true`; `include_change: false` Perforce
    consult stays tree-only and names no changelist.
  - the job actually captures (non-empty `capture`) when `include_change: true`, and stays
    `Capture::empty()` when false.
  - **the tree-only-consult "no capture state" invariant, incl. the merge (f5, deeper layer):**
    - a fresh tree-only Perforce consult persists `changes`/`include_shelved`/`capture_identity`/
      `perforce_baseline` all `None` (not `Some([])`), emits no incremental-protection warning, and
      leaves any pre-existing pending marker untouched (f7);
    - **the legacy-v1-Perforce-consult scenario (f5):** a stored **consult-kind** record with
      `include_change: None` and the v1 garbage (`changes: Some([])`, `include_shelved: Some(false)`,
      `perforce_baseline: Some(Disabled)`), resumed as a tree-only consult, is **refused** →
      rebaseline, so the `.or(existing)` merge is never reached with capture state to inherit. This is
      the exact shape a v1 Perforce consult leaves on disk;
    - the mismatch case: a stored `include_change: true` consult resumed with `include_change: false`
      is refused (the binding);
    - a genuine (post-PR) tree-only consult resumed tree-only across turns keeps those fields `None`;
    - a Perforce `include_change: true` consult persists them like a review.
  - `DiffMode` string round-trip (`parse(to_string(m)) == m`) for every variant, so the persisted
    contract survives a reload.
- **`smoke.ps1` — both directions.** The capture pipeline is protocol (`AGENTS.md`), so this needs
  the real round trip under `-Reviewer codex` **and** `-Reviewer claude`: a consult with
  `include_change: true` that comes back with the change actually shown. `build.ps1` (never starts a
  reviewer) is not a substitute. This costs tokens — flag to the user before running.

## Decisions the code forces (not pinned by the design prose)

### D1 — persist `DiffMode` as a string, not by deriving serde on the enum (**confirmed**)

`DiffMode::parse` (`src/vcs/git.rs:139`) is already the canonical text→enum path and carries all the
ambiguity-rejection logic; a `Display`/`to_string` inverse plus `parse` on read reuses it and keeps
serde off the enum. The alternative (derive `Serialize`/`Deserialize`) is fewer lines but couples
the on-disk format to the enum's shape. The plan review confirmed this: persist a **canonical**
string via an explicit `Display`/serializer, and on read parse only canonical, config-produced
values (the record was written by this server from `cfg.diff`, so an un-round-trippable value is a
corrupt record → the loader's existing fail-closed path, not a new error surface).

### D2 — Perforce `include_shelved` on resume: **resolved → D2b, with a design-doc correction**

The design lists `include_shelved` under **bound (a change refuses the resume)**. The current code
does **not** refuse on it: a changed `include_shelved` disables *elision* and forces a full
re-capture (`src/vcs/perforce.rs:157`), it does not block the resume. For a review that is safe —
the changelist set is the identity, and a full re-capture shows the reviewer the newly-in/out-of-
scope shelved content freshly.

**Decision: D2b** — accept the existing forced full re-capture rather than add a consult-only
`include_shelved` refusal to `resume_block`. It prevents the actual hazard (stale *elided* evidence)
without adding machinery a review's own behaviour already covers, per the `AGENTS.md` rigor
guidance. The rejected alternative, **D2a**, would add a hard refusal so the caller is *told* rather
than silently re-captured; the plan review agreed D2b is proportionate.

One caveat the plan review raised (f3) and this plan accepts: a full re-capture prevents stale
*elision*, but the prior turn's shelved snapshot still sits in the resumed reviewer conversation —
so "no stale evidence survives" was too strong. The honest framing is **allowed scope drift with
full re-capture**: the current turn's evidence is correct and complete, and the earlier snapshot
remains as conversation history exactly as any superseded diff does on a review resume.

Because D2b diverges from the design-of-record's literal "bound = refuse" wording, this PR **also
corrects that wording** so both contracts are not left documented (f3): the `include_shelved` entry
in `docs/cross-model-consult-plan.md`'s bound/drift treatment is reclassified from "bound (refuses
resume)" to "allowed scope drift, made safe by full re-capture." That is a one-line correction to an
already-shipped design doc, made under this PR's own gate — not a re-opening of the design. Either
way, `include_change` and the git `diff_mode` remain hard-refused per item 5.

## What this plan deliberately does not add

- No `DiffMode` version field or migration in the loader — legacy `None` is unbound, the existing
  fail-closed compat story (`AGENTS.md`: "a findings ledger is disposable").
- No consult-specific capture pipeline — item 3 reuses `vcs::capture` verbatim.
- No relaxation of the read boundary, isolation, or the `EVIDENCE_UNAVAILABLE` eligibility gate —
  `include_change` changes *what is shown*, not *how the reviewer is confined*.
- No capture-completeness refusal — warnings only, per `AGENTS.md` and the design.
