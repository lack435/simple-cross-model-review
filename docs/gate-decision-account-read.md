# The proactive gate decision and the account it was decided on (#81)

Status: **implemented.** This document is the plan — approved through this repository's own gate
(Codex, gpt-5.6-luna, effort=max) over six rounds, converged with no open findings — and it now
describes the code as it stands. Tracks issue #81, filed from #80's implementation review as finding
f2 and scoped out of that PR deliberately. #69 / #80 fixed the *write* half of the same family
(`docs/post-run-account-check.md`); this is the *decision* half it named as a follow-up under "The
gate decision is a separate defect".

> **Review history.** Round 1 against this repository's own gate (Codex, gpt-5.6-luna, effort=max),
> five findings, all accepted. The reviewer confirmed the plan's central thesis — declining the
> unification is correct, because a launched entry's gate-vs-pin divergence causes no harm the pin +
> probe + guard + #69 write-binding do not already cover. But the *mechanism* was wrong in two
> load-bearing ways. **f1 (major):** the all-gated case returns `REVIEWERS_EXHAUSTED` *directly* from
> `gate_fresh_selection` (`src/tools.rs:525`) and never reaches `exhaustion_failure`, so the original
> single-point re-read missed it — and, chasing that down, the "single-entry harm" the plan led with
> is a *microsecond* window (immediate return), not the wide one. **f2 (major):** dropping a stale
> skip while still reporting exhaustion for a remaining rate-limited entry is itself a false
> exhaustion — any stale skip means the chain is not exhausted. **f3:** an unreadable account at fold
> time is not proof the skip still holds; align it with the gate's own fail-open posture. **f4:** the
> skip's decision fingerprint must come from the *same* read that built the gate key, not a second
> one. **f5:** unauthorized entries are not currently gate-skipped at all (`home_for_reads` maps them
> to no key), so the unification's cost is a *timing* change in when `PROFILE_NOT_AUTHORIZED` surfaces,
> not a hidden-skipped-error change.
>
> **Round 2:** all five resolved; four new findings, all accepted. **f6 (major):** `GateIdentity.home`
> must be the effective home *directory*, not Claude's `.claude.json` config-file path — otherwise the
> reread double-appends (`.claude.json\.claude.json`), classifies every Claude skip as stale, and turns
> normal exhaustion into endless retryable failures. **f7 (major):** the claim that
> `REVIEWER_ACCOUNT_CHANGED` means "nothing ran, nothing billed, no marker" is false in the *mixed*
> case, where a later entry ran (billed) and set the findings marker. **f8 (minor):** direct
> `finalize_exhaustion` tests do not prove both production call sites use it, nor a single filesystem
> read; add call-site + both-adapter coverage. **f9 (minor):** document `REVIEWER_ACCOUNT_CHANGED` in
> the README failure-code list. This revision folds in all five round-1 and all four round-2 findings.

This document argues for a **smaller** change than the issue's stated direction — so it leads with the
evidence for that, not the mechanism. If the argument is wrong the mechanism does not matter, and the
argument is the part to attack first.

## The three reads, and which one is the defect

Per review there are three reads of the profile home's account, and issue #81 is that nothing ties
them together:

1. **The gate *decision*** — `usage_headroom_key` (`src/tools.rs:337`), read in `gate_fresh_selection`
   (`src/tools.rs:499`, the pre-start selection) and in the walk's per-fallback gate
   (`src/tools.rs:2146`). It reads the account currently under the home, forms the store key, and
   skips the entry when `self.usage.get(key).clears(minimum)` is false.
2. **The attempt's *pin*** — `resolve_authorized_home_with_account` (`src/config.rs:1193`), resolved
   independently at the top of `attempt` (`src/tools.rs:2759`). Asserted by the pre-spawn probe and
   the post-run `switch_guard`.
3. **The headroom *write*** — since #69/#80, already bound to the pin via `write_usage_key`
   (`src/tools.rs:313`), not to read 1.

So #69 already unified reads 2 and 3. #81 is about read 1 diverging from reads 2/3.

**The divergence is purely temporal.** Both `account_fingerprint` (read 1) and `fingerprint_at`
(read 2, via `profile_authorized`) read the *same underlying file* — Codex `auth.json`
`tokens.account_id` (`src/reviewer/codex.rs:473-480`), Claude `.claude.json` account uuid
(`src/reviewer/claude.rs:213-220`). At a single instant they return the same value. They disagree
only because read 1 happens at *selection* and read 2 at *spawn*, and the home re-logged A→B in
between (with B also authorized — a re-auth of the same account changes nothing, since the fingerprint
is the account id).

## The load-bearing fact: the harm is on *skipped* entries only

An entry the gate touches ends up in one of two states, and they are not symmetric:

- **Cleared** → it launches. It runs under its *pin* (read 2), verified by the pre-spawn probe and
  `switch_guard`; its headroom is written under the *pin* (#69). The gate having cleared it on read 1
  is now irrelevant — **running is never the harmful outcome.** If read 1 (A) and the pin (B) disagree,
  the entry simply runs on B and records B, correctly. The only cost of a gate-vs-pin disagreement on a
  *launched* entry is the mild inverse: the gate might clear an entry whose *pin* account is actually
  below-minimum, so a call is spent that the gate existed to save. That is a single rate-limited call,
  self-correcting (the refusal updates the store), and it is the cheap direction. (Round 1 confirmed
  independently: no launched-entry harm from gate-vs-pin divergence beyond this.)
- **Skipped** → it never launches, so it never reaches a pin. Its skip was decided on read 1 alone. If
  read 1 (A) was below-minimum but the home has since re-logged to a healthy B, the skip is wrong — and
  there is no read 2 later to catch it, because a skipped entry has no attempt.

This is the crux, and it has a consequence the issue's suggested fix does not survive.

### Why "resolve once, thread through to the pin" does not close the harm

The issue's suggested direction is: *resolve the authorized home and account once per entry, early,
and use that one value for the gate decision, the pin, and the write, so the states cannot diverge
within a call.* Threading one value through reads 1/2/3 removes the gate↔pin disagreement. But:

- For a **launched** entry, that disagreement was already harmless (it ran on the pin, wrote the pin).
- For a **skipped** entry, there *is no pin and no write* to thread a value into. The skip is decided
  by read 1 at selection; the re-login that makes it wrong happens *after* read 1. No value resolved
  at-or-before the gate can reflect an account that changes after it. Reading "once, early" reads even
  *earlier*, not later — it cannot see the future re-login any more than today's read can.

Worked single-entry case: chain `[E0]`, `E0` gating, store has `A → below-min`.

| t | event | read 1 (gate) | outcome |
| --- | --- | --- | --- |
| t0 | `gate_fresh_selection` | reads **A**, below min | `E0` skipped → returns exhausted **immediately** |
| t1 | home re-logs A→B (B healthy) | — | the skip already stands; no attempt, no pin, no write |

For the skip to be *wrong*, the re-login must land at t1 (after t0). A re-login *before* t0 makes read 1
return B, which has no below-min snapshot, so `E0` runs — no harm. "Resolve once, early" still reads at
t0 and still sees A. It changes nothing about this case.

**Conclusion (round 1 confirmed):** unifying reads 1/2/3 addresses a divergence that is harmless where
it exists (launched entries) and absent where the harm is (skipped entries). It is a real
*auditability* improvement — one resolution point per entry is easier to reason about — but it is not a
*correctness* fix for this bug, and it is not free (see "Considered and not chosen"). This plan
therefore does not adopt it.

## What the harm actually costs, measured against `AGENTS.md`'s rigor test

`AGENTS.md` — "How much rigor, and where": rigor belongs where the blast radius is real (account and
credential handling, the read-only/write boundaries, isolation); for a *review* the worst case is
usually "lost and re-run", and a change that adds a new way for a turn to degrade usually costs more
than the failure it prevents. This harm sits on the cheap side of that line, and more cheaply than most:

- **It cannot cause a false approval, and cannot store an unverified reading.** #69 closed that half.
  This is entirely upstream of any review running.
- **It spends nothing.** The gate is a pre-spawn decision. A wrong skip burns no model call — unlike a
  lost review, which at least ran. The cost is a spurious *refusal*, not wasted work.
- **It self-heals on retry.** The next call re-reads the fingerprint, finds account B, finds no
  below-min snapshot for B, and runs ungated. The current behaviour already recovers; what is wrong is
  only that the refusal it hands back *first* misreports the cause.
- **It needs a rare conjunction:** two authorized accounts on one home, a re-login timed into the
  window, *and* the skipped entry being load-bearing (the last or only unexhausted entry — otherwise a
  healthy entry still runs and the only cost is a less-preferred reviewer).

So this is not a defect that warrants fail-closed machinery. It warrants making the one *irreversible*
symptom honest, and nothing heavier.

### The windows, sized honestly (revised after round 1)

Not all wrong skips are equal, and round 1's f1 forced the sizing to be precise.

- **The wide window — close it.** A **pre-start** gated skip is recorded in `FreshSelection`
  (`src/tools.rs:380`), carried into the `Job`, and — *only when at least one later entry actually
  runs* — folded into a terminal exhaustion by `exhaustion_failure` (`src/tools.rs:355`, called from
  the walk at `:2159` and `:2263`) *many minutes later*, after that entry burned its whole turn and
  rate-limited. Between the skip's fingerprint read and this fold, a full review elapsed. That is
  minutes, and it is the window where a re-login is actually plausible, ending in the worst symptom: a
  terminal `REVIEWERS_EXHAUSTED` naming "usage below minimum" for an entry whose account has moved.
- **The microsecond windows — accept them.** (i) The in-walk per-fallback gate (`:2146`) reads the
  fingerprint and the attempt pins it microseconds later, same loop iteration. (ii) The **all-gated
  selection** path (`gate_fresh_selection:525`) decides every skip and returns exhausted in one
  uninterrupted loop — decision and report are adjacent, so a re-login has to land in a gap of
  microseconds. This is the case round 1 (f1) caught the original plan missing; the honest response is
  that no fold-time re-read can close it, because there is no elapsed time between the decision and the
  report. It is a TOCTOU of the same order as (i), and like the residuals in
  `docs/post-run-account-check.md` it is accepted, not closed.

The fix below still routes the all-gated path through the same validator — not to close its
(microsecond) *changed-account* window, but because doing so is uniform, costs nothing, and lets that
path handle the *unreadable-account* case (f3) consistently with the wide one.

## The fix

**A usage-gate skip is provisional: it is only honest while the account it was decided on is still the
account under the home. Enforce that at every point a skip becomes irreversible — both terminal
exhaustion returns — by re-reading each gated entry's fingerprint and, if the account is no longer the
one the skip was decided on, refusing to report it as usage-exhausted.**

Four parts, one per accepted finding plus the core:

1. **One read yields the key *and* the skip's identity (f4), over the effective home *directory* (f6).**
   `usage_headroom_key` today returns only `Option<String>`. Replace it with `Option<GateIdentity>`
   where `GateIdentity { key: String, home: PathBuf, fingerprint: String }`, and:
   - **`home` is the effective home *directory*, obtained through an adapter-neutral seam** — not a
     reviewer-specific config-*file* path. This matters because the two adapters disagree on shape:
     Codex `fingerprint_at(home)` reads the account out of the `home` dir directly
     (`src/reviewer/codex.rs:478-480`), while Claude `fingerprint_at(home)` appends `.claude.json`
     itself (`src/reviewer/claude.rs:218-220`) — and Claude's `account_fingerprint` reaches the same
     file through `claude_config_path`, which has *already* appended `.claude.json`. If `home` were
     populated with that config-file path, the fold-time `fingerprint_at(home)` would read
     `…\.claude.json\.claude.json`, always `None`, and mark every Claude skip stale — turning normal
     exhaustion into an endless retryable loop (f6). So the seam is
     `effective_read_home(cfg, spec) -> Option<PathBuf>` returning the home **directory** (Codex
     `CODEX_HOME`; Claude `CLAUDE_CONFIG_DIR`), and `account_fingerprint` becomes its composition:
     `effective_read_home(cfg, spec).and_then(|d| fingerprint_at(&d))`. There is then exactly one
     definition of "the account under the effective home", used by the gate key, the skip's stored
     fingerprint, and the fold-time reread — all `fingerprint_at` over the same directory, so no
     path-shape can drift between build and reread.
   - **`effective_read_home` *is* `home_for_reads`, not a re-derivation (f10).** `account_fingerprint`'s
     three-way contract is load-bearing well beyond this change, so the seam must not resolve the home
     afresh. `home_for_reads(cfg, spec, ambient)` (`src/reviewer/mod.rs:739-757`) returns the authorized
     home for an authorized profile, the adapter's **ambient fallback** for `Ok(None)` (Codex `~/.codex`,
     Claude the user home — not merely an env var), and `None` for an unauthorized or unresolvable
     profile. `account_fingerprint` also feeds session identity and the resume check
     (`src/tools.rs:195-214`, `:801-816`), so dropping the ambient fallback would silently disable
     ambient gating and make ambient sessions non-resumable, and bypassing the `Err ⇒ None` arm would
     regress unauthorized-profile behaviour. `effective_read_home` is therefore *defined as*
     `home_for_reads(cfg, spec, <that adapter's ambient>)` (returning its directory), and the composed
     `account_fingerprint` is provably today's value for **all three** cases and both adapters — Codex:
     `fingerprint_at(home_for_reads) == codex_account_id(codex_home)`; Claude:
     `fingerprint_at(home_for_reads) == claude_account_id(home_for_reads.join(".claude.json"))`.
     Equivalence is structural, not asserted by inspection, and is pinned by authorized-profile,
     ambient, and unauthorized-profile tests (see Tests).
   - The **fingerprint is read once**: the key is built from *that* value
     (`entry_key(reviewer, bin, fingerprint)`), and the same value is what a skip records. The home
     directory is a *stable* path (a re-login rewrites the account file's contents, not the directory),
     so the later `fingerprint_at(home)` compares like-for-like.

   Gate-decision callers use `.key`; the skip path captures the whole struct. No second fingerprint read
   exists to disagree with the first.

2. **A skip carries what a re-read needs.** `pre_start_gated_descs: Vec<String>` and the in-walk
   `gated_descs: Vec<String>` become `Vec<GatedSkip>`, where
   `GatedSkip { describe: String, reviewer: ReviewerKind, home: PathBuf, fingerprint: String }` is built
   from the `GateIdentity` above. This lives on the in-flight `Job` and the walk's locals exactly as the
   describe-strings do today — no store, no ledger, no persistence.

3. **One validator, both exhaustion returns (f1), and the *sole* constructor of the outcome (f13).** A
   new `finalize_exhaustion(rate: &[String], gated: &[GatedSkip]) -> Failure` is the single place either
   terminal exhaustion is built — `gate_fresh_selection`'s `:525` direct return and the walk's two sites
   all call it, and the old `errors::reviewers_exhausted_gated` / `exhaustion_failure` construction is
   *removed*, folded inside. So the wiring is structural, not something a test must police: there is no
   old path left for a call site to be accidentally on (the same sole-constructor guarantee #69 used for
   `collect_run` being the only `usage.record` caller). It re-reads each `GatedSkip`'s
   `fingerprint_at(home)` and classifies:
   - **still-gated** — the live fingerprint equals the decided one.
   - **stale** — the live fingerprint *differs* (moved) **or** is unreadable. Unreadable is stale, not
     still-gated (f3): the gate itself treats an unreadable identity as fail-*open* (no key ⇒ not gated,
     `src/tools.rs:503-505`), so at fold time an unreadable account is likewise not evidence the entry
     is still unavailable. Failing it toward "still exhausted" would contradict the gate's own posture
     and misreport a mid-relogin as a usage limit.

   Then (f2): **if any skip is stale, the chain is not exhausted.** Return the new retryable
   `REVIEWER_ACCOUNT_CHANGED`, whose detail names the stale reviewer(s) and *separately* retains the
   rate-limited entries for diagnosis (they are still in `metrics_attempts` regardless — the code change
   is to the terminal *outcome*, not the metrics history). Only when **every** gated skip is still-gated
   does it fall through to today's `exhaustion_failure` wording (pure-rate / pure-gated / mixed) over the
   still-gated describe strings — byte-for-byte unchanged on that path.

   `gate_fresh_selection`'s direct all-gated return (`:525`) calls `finalize_exhaustion(&[], &gated)`; the
   walk's two `exhaustion_failure(...)` calls become `finalize_exhaustion(&rate, &gated)`. The re-read is
   `fingerprint_at` — a local file read, no spawn, no auth, no lease — so the pre-lease
   no-spawn/no-preflight contract of `gate_fresh_selection` is preserved.

4. **`REVIEWER_ACCOUNT_CHANGED` changes the terminal failure code (and, intentionally, the top-level
   metrics code) but not the per-attempt marker/billing/session lifecycle (f7, f12).**
   `finalize_exhaustion` chooses only the terminal `Failure` (code + detail). `finish_run` then copies
   that code into the top-level metrics record (`src/tools.rs:2351-2356`, `:2441-2478`), so a mixed run's
   top-level failure code *does* change from `REVIEWERS_EXHAUSTED` to `REVIEWER_ACCOUNT_CHANGED` — that
   is the point: the honest code is what should be recorded. What does **not** change is the *per-attempt*
   state, which was already fixed by what ran before exhaustion was built: the ran entry's own
   `metrics::Attempt` keeps its `failure_code`/`billed: true`, the findings marker keeps whatever value
   the run left, and the session's resumability is unchanged. ("Billed" is not a property of a `Failure`
   at all — `Failure` has no billing field (`src/errors.rs:14-19`); billing is per-attempt. So the code
   is described as *retryable*, never as "not billed".) The lifecycle differs by sub-case, and the
   round-1 plan wrongly described only the first:
   - **Pre-start-only** (`rate` is empty — no entry ran): no `attempt` was entered, so
     `mark_findings_pending` was never called and nothing was billed. Here the code genuinely means
     "nothing ran"; the remediation says to retry, and the next call re-gates against the current
     account. This is the clean case.
   - **Mixed** (`rate` is non-empty — a later entry ran and rate-limited): that entry set the findings
     marker before running (`src/tools.rs:2907-2922`), was recorded `billed: true`
     (`src/tools.rs:2271-2281`), and — like any post-start failure — left the marker set
     (`src/tools.rs:3033-3055`). Relabelling to `REVIEWER_ACCOUNT_CHANGED` changes none of that: it is
     the *same turn* with the *same* marker/billing state that today's mixed `REVIEWERS_EXHAUSTED`
     produces, only with a truthful code and a detail that names both the moved reviewer and the
     rate-limited one. Its remediation therefore does **not** claim "nothing ran"; it follows the same
     post-run retry path as today's mixed exhaustion (a subsequent same-session resume meets the
     findings gate exactly as it does now).

   `finalize_exhaustion` distinguishes the two purely from `rate.is_empty()` to word the remediation
   correctly; it does not — and must not — reach into marker, billing, metrics, or session state. It is
   also distinct from `PROFILE_IDENTITY_MISMATCH`, which the `switch_guard` raises on a *launched*
   review as a security refusal; `REVIEWER_ACCOUNT_CHANGED` is not a boundary event (no unauthorized
   run happened — the account that "moved" belongs to an entry that was never launched), and is
   retryable.

**Why re-read rather than re-run.** Actually *running* the newly-healthy entry means re-entering chain
selection after the walk decided it was exhausted — control-flow surgery on the walk, on the account
path, which the issue and #69's precedent both flag as the expensive option. Re-reading a fingerprint
and choosing a truthful code is local to a path that is *already terminal and already failing*. It
cannot make a passing review fail; it can only relabel a refusal that is going out regardless. The user
retries either way — the only difference is whether the message they retry against is true. Per
`AGENTS.md`, a failure code that misreports the reviewer's state is itself a bug in this repository;
this makes it not misreport.

## Considered and not chosen

- **Unify reads 1/2/3 (the issue's suggested direction).** Declined as the fix, for the reason argued
  above and confirmed in round 1: it removes a divergence that is harmless on launched entries and
  non-existent on skipped ones. Its cost is a real change on the account path, but **not** the one the
  original plan claimed. Correction from round 1 (f5): unauthorized entries are *not* gate-skipped
  today — `usage_headroom_key`'s fingerprint read goes through `home_for_reads` (`src/reviewer/mod.rs:748-757`),
  which maps an unauthorized profile to `None`, so the entry gets no key and is never gated; it proceeds
  and surfaces `PROFILE_NOT_AUTHORIZED` at `ensure_entry_ready` / the pin. So there is no
  "hidden skipped authorization error" that unification would expose. What unification *would* change is
  *when* `PROFILE_NOT_AUTHORIZED` surfaces — moving `resolve_authorized_home_with_account` into pre-lease
  selection would raise it before the session lease and, in a fresh multi-entry walk, a non-`RATE_LIMITED`
  failure stops the walk rather than falling through. And for the start entry, unification either carries
  the pin forward from selection (widening the pin→spawn window the pre-spawn probe guards — a
  security-timing change) or re-pins at spawn and leaves the divergence intact. Real auditability value,
  real account-path cost, **zero** movement on the filed harm. If a maintainer wants the invariant for
  defense-in-depth, it is a separate PR with its own account-path tests, not this one.
- **Re-run the stale entry instead of relabelling.** Closes the harm outright (the review runs) rather
  than handing back a retryable refusal. Rejected as disproportionate: it is the walk surgery above, for
  a zero-spend, self-healing symptom whose retry already does exactly this re-run one call later.
- **Do nothing but document.** A legitimate floor under `AGENTS.md` (the harm is self-healing). Rejected
  because the terminal `REVIEWERS_EXHAUSTED — usage below minimum` is a concrete misreport of the
  reviewer's state, which this repository treats as a bug, and the fix for it is small and boundary-free.
- **A provenance/tentative flag on the store, or any rollback.** Nothing is written on a skip, so there
  is nothing to make tentative or roll back. Out of scope by construction.

## Behaviour table

| Situation | Today | After |
| --- | --- | --- |
| Gated skip(s), all accounts unchanged at fold time | `REVIEWERS_EXHAUSTED` (gated/mixed), "usage below minimum" | **unchanged, byte-for-byte** |
| Pure rate-limited chain, no gated skips | `REVIEWERS_EXHAUSTED` (rate) | **unchanged** (single-reviewer path preserved) |
| Wide window: pre-start skip on A + a later entry ran and rate-limited, A re-logged to B by fold time | `REVIEWERS_EXHAUSTED`, "usage below minimum" (misreport) | `REVIEWER_ACCOUNT_CHANGED` (retryable), names the moved reviewer; rate-limited entry retained in detail. **Marker/billing unchanged from today's mixed exhaustion** — the ran entry set the marker and was billed, and it stays that way |
| Pure pre-start exhaustion (no entry ran) with a moved account | `REVIEWERS_EXHAUSTED` (gated) | `REVIEWER_ACCOUNT_CHANGED` (retryable); no marker was set and nothing was billed — plain retry re-gates |
| Any exhaustion where a gated skip's account is unreadable at fold time | `REVIEWERS_EXHAUSTED` | `REVIEWER_ACCOUNT_CHANGED` (retryable) — consistent with the gate's fail-open posture; self-heals |
| All-gated selection, an account changed in the microsecond return window | `REVIEWERS_EXHAUSTED` (gated) | `REVIEWER_ACCOUNT_CHANGED` if the re-read catches it (window is microseconds; not relied upon) |
| Any launched entry (gate cleared it) | runs on pin, writes pin | **unchanged** |

## Tests

Two layers, because f8 is right that a direct-only test proves the function but not the wiring.

**Unit, over `finalize_exhaustion` directly** (a free function over `GatedSkip`s, so testable without a
`Job`). Each uses a real temp home *directory* so `fingerprint_at` is exercised end to end, in the style
of `switch_guard_refuses_a_changed_or_unreadable_account`:

- **`a_gated_skip_whose_account_moved_yields_account_changed`** — one `GatedSkip` decided on A, home now
  presents B, no rate limits: `REVIEWER_ACCOUNT_CHANGED`, naming the reviewer.
- **`a_stale_skip_beside_a_rate_limit_still_yields_account_changed`** — `[gated-on-A-now-B, rate-limited]`:
  `REVIEWER_ACCOUNT_CHANGED`, not a mixed exhaustion; the rate-limited entry appears in the detail (f2).
- **`an_unreadable_fold_time_account_yields_account_changed`** — `GatedSkip` on A, home's account file
  now unreadable: `REVIEWER_ACCOUNT_CHANGED`, not a spurious "usage below minimum" (f3).
- **`all_still_gated_is_todays_exhaustion_verbatim`** — every gated skip's account unchanged: the
  gated/mixed detail is byte-for-byte today's `exhaustion_failure` output (regression guard).
- **`pure_rate_exhaustion_is_unchanged`** — `finalize_exhaustion(&rate, &[])`: today's exact rate wording,
  guarding the single-reviewer path `docs/reviewer-fallback-chain.md` requires stay identical.
- **`a_moved_claude_account_is_detected_not_double_appended`** (f6) — a `GatedSkip` for a **Claude** entry
  whose `home` is the `CLAUDE_CONFIG_DIR`: a *stable* account stays still-gated (i.e. the reread finds the
  account, proving `home` is the directory and not `…\.claude.json`), and a genuinely moved one is stale.
  This is the regression test for the double-append trap, and it must run against the Claude adapter, not
  Codex.
- **`gate_identity_key_and_fingerprint_stay_consistent`** (f4, f14) — build a `GateIdentity`, then
  **remove the underlying account file**, and assert its `fingerprint` is unchanged and its `key` still
  equals `entry_key(reviewer, bin, fingerprint)`. The invariant this pins is *captured-identity
  consistency* — the gate key and the skip's recorded fingerprint are the one captured value, and use
  does not re-fetch — **not** "one physical read": `home_for_reads` reads the account during
  authorization (`src/config.rs:1220-1248`) before the composed read, so construction is not a single
  syscall and the plan does not claim it is (f14). Asserted for **both** adapters, so the
  `effective_read_home` seam is covered for each shape.
- **`account_fingerprint_is_unchanged_for_authorized_ambient_and_unauthorized`** (f10) — the redefined
  `account_fingerprint` returns today's value for an authorized profile, for ambient (via the adapter's
  ambient fallback), and `None` for an unauthorized profile, for **both** adapters. Guards the
  session-identity/resume consumers that share this read.

**Call-site, at the real callers (f8/f11).** Wiring is guaranteed structurally by the sole-constructor
point above, so these are behaviour/regression tests, sized to what each window can actually observe:

- **`gate_fresh_selection_all_gated_is_todays_exhaustion_for_a_stable_account`** — drive
  `gate_fresh_selection` (its pre-lease/no-spawn contract intact) with a stable-account all-gated chain
  and assert it still returns today's `REVIEWERS_EXHAUSTED` verbatim. This is a **regression guard on the
  common path**, not a wiring proof (f13): with a stable account the output is identical whether or not
  the relabel logic runs, so it cannot — and does not claim to — prove the route; the sole-constructor
  structure does that, and the relabel is proven where the transition is observable (below). Its window
  is microseconds (read and return adjacent), so it deliberately stages no transition.
- **`the_walk_terminal_exhaustion_relabels_a_stale_pre_start_skip`** — the mixed path, and the one place
  the A→B transition is genuinely observable: a pre-start skip is recorded on account A during selection,
  an **earlier** chain entry then rate-limits (so it is appended to the attempt history as
  `billed: true`, `src/tools.rs:2269-2281`), the test moves that pre-start skip's home directory to
  account B, and the walk reaches terminal exhaustion and folds. Assert `REVIEWER_ACCOUNT_CHANGED`;
  assert the **top-level** metrics failure code changed to it (f12); and assert the **per-attempt** state
  is exactly today's mixed exhaustion — the earlier rate-limited entry is still `billed: true` in the
  recorded attempts, and the findings marker is still set (f7). Using an *earlier* rate-limited entry is
  deliberate: the *terminal* attempt is the top-level record, not an appended `metrics::Attempt`
  (`src/tools.rs:2122-2125`, `:2260-2267`), so a billed-attempt assertion must target an earlier entry
  (f12). Billing lives only on appended `Attempt`s — `metrics::Record` has no `billed` field
  (`src/metrics.rs:381-469`) — so the only *session*-state assertion is that the findings marker is still
  set; there is no top-level billing flag to assert. This test pins the "code changes, per-attempt
  lifecycle does not" claim.

`REVIEWER_ACCOUNT_CHANGED` gets the same failure-contract coverage the other codes have in
`src/errors.rs` tests (*retryable*, remediation present — billing is per-attempt, not a `Failure`
property), including that its pre-start-only and mixed remediation strings differ as specified.

## Files touched

| File | Change |
| --- | --- |
| `src/reviewer/mod.rs` + `codex.rs` + `claude.rs` | expose the adapter-neutral `effective_read_home(cfg, spec) -> Option<PathBuf>` **defined as `home_for_reads` returning its directory** (so authorized/ambient/unauthorized semantics are preserved by construction — f10); redefine `account_fingerprint` as `fingerprint_at` over it, so gate key, stored fingerprint and reread are one call over one directory shape (f6), and every existing `account_fingerprint` consumer (session identity, resume) sees the identical value |
| `src/tools.rs` | `usage_headroom_key` → `Option<GateIdentity>` (`{key, home-dir, fingerprint}`, one read); `FreshSelection`/`gated_skip_attempt` and the in-walk `gated_descs` carry `GatedSkip { describe, reviewer, home, fingerprint }`; new `finalize_exhaustion` re-reads each skip and returns `REVIEWER_ACCOUNT_CHANGED` (with pre-start-only vs mixed remediation, `rate.is_empty()`-driven) if any is stale (moved or unreadable), else today's gated/mixed/rate wording folded in from `exhaustion_failure`; it becomes the **sole constructor** of that outcome (`errors::reviewers_exhausted_gated` direct return at `:525` and the standalone `exhaustion_failure` both removed and routed through it — f13); marker/billing/session untouched; the two-layer test set above |
| `src/errors.rs` | `REVIEWER_ACCOUNT_CHANGED` constructor (retryable; billing is per-attempt, not a `Failure` property), pre-start-only and mixed remediation, + its contract test |
| `docs/usage-remaining-gate.md` | the proactive-gate section: a usage-gated skip is honored only while the account it was decided on is still under the home; a moved-or-unreadable account at exhaustion time is retryable, not usage-exhausted |
| `docs/post-run-account-check.md` | update "The gate decision is a separate defect" to point here; record that the wide (pre-start-fold) window is closed and the microsecond windows (in-walk gate, all-gated immediate return) are accepted |
| `README.md` | the usage-headroom section: a skip is provisional on the account it was measured for; **and** add `REVIEWER_ACCOUNT_CHANGED` to the reviewer failure-code list (`README.md:884-892`) with its retryability and its pre-start-only vs mixed marker/billing semantics (f9) |

## Verification before hand-back

- `.\build.ps1` — fmt, clippy `-D warnings`, unit tests, release build.
- `.\smoke.ps1 -Reviewer codex` — the change is on the shared selection/exhaustion path; the Codex
  direction exercises the evidence service and the rollout headroom read. **Calls a real model and
  costs tokens** — mentioned to the user before it is run. A clean run does not depend on the account
  actually moving (the terminal relabel is unit-tested against `finalize_exhaustion`); the smoke run
  confirms the common path is untouched.
- This repository's own review gate on the implementation diff, per `AGENTS.md`.

## What is deliberately not in this change

- **No pin move, no probe change, no `switch_guard` change.** The security backstops stay exactly where
  and what they are. Launched entries are untouched.
- **No unification of reads 1/2/3.** Argued above; declined with reasons rather than omitted.
- **No walk re-entry / re-selection after exhaustion.** The terminal path relabels; it does not re-run.
- **No store, ledger, or provenance-flag change.** A skip writes nothing.
- **No new fail-closed path.** The only new outcome is *more* permissive than today (a retryable code
  where today there is a terminal misreport), so no benign case newly fails.
