# Issue #78: per-finding liveness is not closable; a stalled session is

Status: **proposal, revision 3.** Revision 1 proposed a per-finding re-examination gate and gate
round 1 killed it. Revision 2 replaced it with a ledger-movement watchdog but kept claiming to close
#78's liveness half; gate round 2 killed that claim. This revision keeps the watchdog and abandons
the claim.

**The recommendation this document makes is therefore split, and both halves are deliberate:**

- **#78 as specified — "make it impossible for a session to sit indefinitely on a finding no one has
  re-examined" — should be closed won't-fix.** It is not achievable at any price this project should
  pay, for a reason that is a property of the situation and not of any particular design. #78 states
  that closing on evidence is an acceptable outcome; this is that outcome, with the argument.
- **An open-finding no-output watchdog should be built**, because the *session-level* worst case is
  genuinely unbounded and bounding it costs almost nothing. It is a smaller guarantee than #78 asks
  for and it is described as such throughout. It is not fully generic: it watches sessions that hold
  open findings, and a session whose ledger is empty is outside it (see
  [The change](#the-change)).

Read [`stale-open-findings-fix.md`](stale-open-findings-fix.md) first — this builds on the protocol
it established.

## Why per-finding liveness cannot be closed

Revision 1 proposed gating on `last_verified_turn`. Round 1 sank it:

> Any repeated `prior_findings` entry advances `last_verified_turn`, even when the reviewer merely
> echoes an open status; `src/findings.rs:704` stamps every restated id. […] so a reviewer can avoid
> the gate indefinitely without re-examining the finding.

That is correct, and it is worse than an incomplete guarantee. #77's contribution was to *remove* the
compulsion to restate, so that a restatement would begin to mean something: omission became free and
honest, and an echo became a deliberate falsehood rather than compliance with a demand. Revision 1
put the compulsion back — restate or the session dies — and so re-created, as an incentive, the exact
reflex #62 exists to describe.

Round 2 then closed the escape route revision 2 had hoped for. Ledger movement is real and
unfakeable-by-echo, but:

> taking the `max` across all findings means movement on finding B continually resets the gate while
> finding A remains open and untouched. `reconcile` also advances the value for any explicit status
> change or newly minted finding, neither of which proves examination.

Both halves are right. Generalising across the three rounds:

> **The server cannot observe whether the reviewer looked.** Every signal available to it is either
> the reviewer's own claim (`prior_findings`, and so `last_verified_turn`) or an artifact the
> reviewer can produce without examining the finding in question (a mint, a resolution elsewhere).
> Any mechanism keyed on "did you re-examine *this*" is keyed on a claim — and pressuring a claim
> makes it less trustworthy, not more.

One signal would escape this: correlating the reviewer's evidence-service tool calls against a
finding's file and line, which the server owns and the model cannot produce without doing the work.
It is rejected. It exists only in the Codex direction, it cannot attach to a finding with no
location, reading a file is not examining it, and it is a large piece of machinery for a fuzzy
signal. Naming it is not proposing it.

**So the per-finding half of #78 gets no mechanism, and this document recommends saying so in the
issue rather than shipping something that half-does it.** #77's `last_verified_turn` remains the
right answer to the part that *is* tractable: the caller can see which findings were carried, and
name them in the next turn's `instructions`.

A note on wording used throughout, because round 3 caught this document being loose about it. An
*echoed restatement does change the ledger* — it advances `last_verified_turn`, which is persisted.
What it does not change is any finding's `status`, and therefore not `last_status_change_turn`. So
where this document says "the ledger moved" it means precisely **a finding was minted or a finding
was resolved**, never "some byte of the ledger differs".

### The per-finding watchdog was also measured, and it is worse

For completeness, the obvious per-finding variant — fire when *some open finding* has gone `N` turns
without its own status changing — was reconstructed on the same data as everything below. At `N = 3`
it would have fired on 4 findings, in sessions that went on to resolve them and converge. As with the
session-level figures this is a count of hypothetical triggers, not a false-positive rate, and it does
not prove those sessions ought to have continued — but 4 triggers against the session-level
watchdog's 0, over the same 37 sessions, is the comparison that matters, and the per-finding variant
still proves nothing about examination. Worse on both axes.

## What is bounded instead

The server cannot see examination. It can see **output**: whether any finding was raised or resolved
on a turn. That is not a claim about the reviewer's attention — it is a change in the record — and it
cannot be produced by echoing, because an echoed `open` leaves `last_status_change_turn` where it was
(`src/findings.rs:699-702`).

Derived, with nothing new stored:

```
last_movement_turn = max(f.last_status_change_turn) over all findings
stagnation         = turn - last_movement_turn
```

`last_status_change_turn` is set to the current turn both when a finding is minted and when it
resolves (`src/findings.rs:699-739`), so this is exactly **the last turn a finding was minted or
resolved** — not "the last turn the ledger changed", which an echo also does. It predates #77 and its
meaning was untouched by it, which makes it measurable on data this repository already has.

**What this does and does not claim.** It does not claim a mint or a resolution proves examination —
round 2 is right that neither does. It claims only that a session which has produced **no mint and no
resolution** for `N` consecutive turns is not going anywhere, whatever the reviewer is or is not
looking at.

## The measurement

Round 2 was right that revision 2's table was an end-of-session snapshot, so "would have fired zero
times" was not something it measured. The gate quantity is reconstructible per turn, and this is that
reconstruction.

**Method,** so it can be checked. For each session in this workstation's `sessions.json` with a
non-empty ledger, the set of movement turns is the union over findings of
`{first_seen_turn, last_status_change_turn}` — a mint and a resolution respectively. A finding is
open at turn `t` iff `first_seen_turn <= t` and (`status == open` or `last_status_change_turn > t`),
which is correct because resolution is terminal under #77, so a finding resolved at turn `r` was open
for exactly the turns before `r`. Walking `t` from 1 to the session's recorded turn count gives
`open_count(t)` and `stagnation(t)`, and hence the gate's decision at every turn the session ran.

**Stagnation at turn `t` is measured against a prefix maximum** —
`t - max{m in movement turns : m <= t}` — never against the whole final ledger. Round 3 was right to
demand this be stated: an unconditional maximum would let a resolution at turn 5 retroactively
suppress the stagnation that existed at turn 4, hiding exactly the intervals the reconstruction is
looking for. The script that produced the table below takes the prefix; the earlier revision of this
section simply did not say so.

Over 37 ledgered sessions, counting only turns where `open_count > 0`:

| Peak stagnation reached at **any** turn of the session | sessions |
| --- | --- |
| 0 turns | 34 |
| 1 turn | 3 |
| **2 or more turns** | **0** |

All three stagnant turns were a session's final turn. **A gate at `N = 2` would have fired zero
times in 37 sessions**, and this time that is a measurement of every turn rather than of the last
one.

Read it both ways, because the plan depends on both.

**Against building anything:** the condition this watchdog fires on — two consecutive turns with no
mint and no resolution while findings are open — **did not occur on any turn of any recorded
session.** (That is a statement about mints and resolutions, not about what anyone examined; the
reconstruction cannot see examination, which is the whole point of the section above.) The eight
sessions carrying the #62 signature — a finding held open while siblings resolve — were all sessions
whose ledgers were producing resolutions; that is ordinary review with a laggard finding, not a
freeze. This is real evidence for closing #78 entirely, and it is why the per-finding half is being
closed.

**For building this specific thing:** the same number is **zero hypothetical triggers in this
sample**, at a threshold one turn tighter than the proposed default. It is not a false-positive rate
in any statistical sense — 37 sessions from one workstation cannot establish one — but it is direct
evidence that the gate does not touch the kind of session this project actually runs. The case is not
"we see this happening"; it is "the session-level worst case is unbounded, bounding it costs zero
tokens and zero round trips, and the bound did not fire once on the sessions we have."

Two limits, stated rather than buried. This is one workstation's store — this project's own
dogfooding, not a population. And it reconstructs from persisted final ledgers plus the recorded turn
count, so a session whose record was truncated or rebaselined contributes only what it stored.

## The change

**A session whose ledger has not moved for `N` turns, while findings are still open, is terminal.**

On a structured turn, after reconciliation, when `open_count > 0` and `stagnation >= N`:

- a new `NonConvergenceReason::SessionStagnant`;
- `Outcome::Rebaseline` — "this session cannot continue; a human decides, then starts a fresh review
  carrying the still-open findings";
- the sticky `terminal_reason` is persisted, as `ledger_too_large` already does
  (`src/session.rs:644-660`, `src/tools.rs:1817`, `src/tools.rs:3036`), so a later resume is
  **refused** rather than advised against. **It is derived from the envelope's selected ranked
  reason**, not from a second independent condition: the persisted sticky state is whatever the
  reported reason is, when that reason is a sticky one. Round 4 asked for this to be explicit, and it
  is what keeps the envelope, the sticky record and the resume refusal from being able to disagree.
  Under co-occurrence it falls out of the ranking: stagnation together with an over-budget ledger
  reports and persists `ledger_too_large` (rank 0), and stagnation with only advisory reasons reports
  and persists `session_stagnant`. This replaces the current derivation from `over_budget` at
  `src/tools.rs:3036-3038` without changing that path's behaviour, because an over-budget turn always
  reports rank 0.

  **Only the two sticky reasons map.** `ledger_too_large` and `session_stagnant` produce a
  `terminal_reason`; `ledger_unavailable` and `turn_not_durable` do **not**, because today they are
  recorded as ledger coverage rather than as a sticky terminal state, and this change must not quietly
  promote them. The precedence table below marks them "persisted break" for exactly this reason.

  Revision 3 of this document also required a test that an over-budget *degraded* or *not-durable*
  turn still persists `ledger_too_large`. Round 5 showed that obligation was invented and the paths
  cannot reach the state it asserts — the degraded branch sets `over_budget: false`
  (`src/findings.rs:1723`) and never calls `resolve_structured`, and a turn whose persistence failed
  cannot durably persist anything by definition. It is dropped rather than carried, and the tests it
  was standing in for are named in [Testing](#testing);
- a warning naming the still-open findings, each with the turn it was last re-examined — which is
  where #77's `last_verified_turn` stays useful without being load-bearing. That field is `None` on a
  ledger written before #77, and the warning renders `unknown` for it rather than substituting
  `first_seen_turn`: this text is read by a human deciding what to carry forward, and inventing a
  plausible turn number for one there is no record of would be the worse failure.

`--stagnant-session-turns <n>`, default **3**, `0` disables. The same shape as `--session-max-turns`
and `--session-max-idle-seconds`, beside which it sits. The knob earns its place as a rollout and
disable switch for a gate whose firing rate in other people's repositories is unknown.

### The prompt does not change. That is the load-bearing decision.

The reviewer is never told this rule exists.

Telling it would create pressure to *manufacture movement*, and the only two ways to move the ledger
are to raise a finding or to resolve one. A reviewer that resolves a finding it has not verified in
order to keep a session alive is the single outcome #78 forbids most emphatically — and under #77 a
resolution is terminal, so that finding never comes back.

This inverts revision 2 of `stale-open-findings.md`, which held that a stronger outcome requires the
server to have actually asked. That principle was right for a gate that *judges the reviewer*: it
would have been unfair to penalise a reviewer for failing a contract it was never shown. This gate
judges the **session**, not the reviewer, and there is no contract to have been shown.

Stated exactly, because round 3 was right that the looser version overclaimed: what an unchanged
prompt buys is **no pre-trip pressure** — up to and including the turn that trips, the reviewer sees
the same bytes it would see without this feature, so nothing has nudged it toward manufacturing
movement. It does not mean reviewer behaviour is identical in every respect: once the gate fires the
session is terminal, so there is no next turn, which is a difference of exactly the kind intended.

## What this costs

- **Round trips: zero.** No challenge turn, no second model call.
- **Tokens: zero.** The prompt is byte-for-byte unchanged.
- **Turns lost: none.** A tripped gate loses nothing — the turn's findings, new findings, prose,
  verdict and ledger are all returned and persisted exactly as they would have been.
- **Sessions ended: zero hypothetical triggers across 37 sessions**, at a threshold one turn tighter
  than the default, evaluated at every turn. This is the one cost that is *not* zero when it does
  land: a firing forces the manual rebaseline `AGENTS.md` describes — a human carrying the still-open
  findings into a fresh session by hand. That is what the caller does anyway when it gives up, only
  now it is told at turn `N` instead of discovering it at turn 8; but it is a real cost, not a free
  one, and it is the reason the knob can disable the gate outright.

## What it must not do, and does not

#78's constraint is emphatic because in this repository's dogfooding the held-open finding was
correct every time it was disputed — three times across #71 and #62.

- Status is untouched; every carried finding stays `open`.
- `open_count` is unchanged, and the gate is *conditioned on* `open_count > 0`, so it can only ever
  make an outcome graver — it can never produce an approval.
- `findings_trusted` is unchanged and the ledger persists intact, so the human carries a complete
  record into the fresh session.
- The outcome is `rebaseline`, whose defined meaning is that a person decides and the findings are
  preserved.

## Precedence

Round 1 was right that revision 1 misstated co-occurrence: `resolve_structured` adds
`LedgerTooLarge`, `LedgerUnavailable`, `ReviewerBlocked` and `VerdictContradiction` independently of
the open count, so this reason can co-occur with any of them (and with `TurnNotDurable`, applied
afterwards by the caller).

The rule, stated so it can be checked rather than case-enumerated:

> **A sticky terminal reason outranks an advisory one.** Reporting an advisory reason on a turn that
> also killed the session would understate what happened.

| rank | reason | sticky? |
| --- | --- | --- |
| 0 | `ledger_too_large` | yes |
| 1 | `ledger_unavailable` | persisted break |
| 2 | `turn_not_durable` | persisted break |
| **3** | **`session_stagnant`** | **yes** |
| 4 | `reviewer_blocked` | no |
| 5 | `verdict_contradiction` | no |
| 6 | `open_findings` | no |

`session_stagnant` must outrank `open_findings`, which always co-occurs with it, or it would be
unreachable. It yields to the three ledger/durability reasons because those say the record itself is
unusable, which is graver than a usable record that stopped growing.

(A retired `reviewer_withheld_approve` reason once sat at rank 6 above `open_findings`; it was removed
with the `approve_with_comments` verdict — see the retirement note in
[`structured-findings-envelope.md`](structured-findings-envelope.md) — and `open_findings` moved up to
6.)

## Two integration defects round 2 found in the existing code

Both are real, both were verified, and both are part of this change rather than follow-ups —
persisting a sticky terminal reason without them would ship a response that contradicts itself.

**`resumable` does not account for terminal reasons.** `src/tools.rs:3348` reads
`durable && !turn_eval.over_budget && findings_marker_cleared`. That enumerates the *causes* of
non-resumability known when it was written rather than the condition itself, so a turn that persists
`session_stagnant` would return `resumable: true` and then be refused on the next resume. The fix is
general, not stagnation-specific: the calculation takes the terminal reason this turn persists —
`terminal_reason_to_persist.is_none()` at `src/tools.rs:3036` — so any future sticky state is covered
by construction.

**The resume-refusal diagnostic hard-codes the wrong cause, and is not always the one selected.**
`src/tools.rs:3504-3513` interpolates the stored reason and then asserts "the findings ledger outgrew
a single review conversation", which is only true for `ledger_too_large`. Two changes, the second
found by round 4:

- The message becomes **reason-specific**: the existing sentence for `ledger_too_large`, and for
  `session_stagnant` that the review produced no new or resolved findings for several turns and the
  still-open findings must be carried into a fresh session. It is deliberately **threshold-neutral** —
  no "`N` turns" — because the threshold in force when the session died is not persisted, and the
  refusal may be read under a different configuration. Persisting the historical threshold to print
  one number is not worth a new field.
- The terminal-reason check **moves ahead of the turn-count and idle checks** at
  `src/tools.rs:3482-3501`. Today it sits after them, so a stagnant session that is also old, or that
  also hit `--session-max-turns`, is refused with a generic staleness message that says nothing about
  why it is actually dead. A terminal state is not staleness and outranks it: it is the specific thing
  to say, on the same "more specific thing wins" reasoning that already puts turns before idle.

## Blast radius

- **`src/findings.rs`** — `NonConvergenceReason::SessionStagnant` with its `rank` and
  `Outcome::from_reason` arms; `Ledger::last_movement_turn() -> Option<u32>`; the check and its
  warning in `resolve_structured`, which takes the stagnation and the threshold; `finalize_turn`
  takes the threshold. `outputSchema` types `non_convergence_reason` as a plain string
  (`src/findings.rs:1282`), so **no schema change and no version bump**.
- **`src/tools.rs`** — the threshold into `finalize_turn`; the new reason folded into the existing
  `terminal_reason` persistence at `:3036`; the two defects above at `:3348` and `:3504`.
- **`src/config.rs`** — `--stagnant-session-turns`, its default, its `--help` text.
- **`src/session.rs`** — documentation only, and round 3 was right that revision 2 wrongly listed
  this file as untouched. `SessionRecord::terminal_reason` at `:211-215` says the state is "currently
  only `\"ledger_too_large\"`" and explains a refused resume as the session having outgrown a
  conversation. Both become false. The persisted type does not change and no code does.
- **`README.md`**, **`docs/structured-findings-envelope.md`** — the new reason in the reason table,
  the escalation list, and the sticky-terminal list.
- **`AGENTS.md`** — one line: on `rebaseline` with this reason, carry the still-open findings into a
  fresh session.

Untouched: `src/prompt.rs`, `src/metrics.rs`, `Budget`, `render_digest`, `Ledger`'s persisted shape,
`Finding`, and the reconciliation path. **No ledger break** — nothing new is stored, so every ledger
that loads today still loads.

## Testing

- A session with an open finding and no movement for `N` turns reports `session_stagnant`, outcome
  `rebaseline`, and persists the sticky `terminal_reason`; the next resume is refused, with the
  reason-specific, threshold-neutral message.
- **That response reports `resumable: false`** — the `src/tools.rs:3348` defect, pinned.
- **The persisted sticky reason tracks the ranked reason.** Stagnation with an over-budget ledger
  persists `ledger_too_large`; stagnation with only advisory reasons persists `session_stagnant`.
- **Only sticky reasons are persisted.** A degraded turn reporting `ledger_unavailable`, and a turn
  reporting `turn_not_durable`, leave `terminal_reason` unset — the behaviour today, pinned so the new
  derivation cannot promote a persisted break into a sticky terminal state.
- The three cases replacing the dropped parity obligation, tested separately as round 5 asked: sticky
  reason selection on a **structured** over-budget turn; the **pre-model** over-budget path
  (`src/tools.rs:1817`), which sets the sticky reason with no turn at all; and a turn whose
  persistence **failed**, which is refused and persists nothing.
- **A stagnant session that is *also* past `--session-max-turns` or the idle limit is refused with the
  terminal-reason message, not the staleness one** — the ordering change at `src/tools.rs:3482`.
- Status stays `open`, `open_count` is unchanged, `findings_trusted` stays true, and the findings are
  returned in full on the tripping turn.
- `N-1` turns of stagnation does not trip; the turn is an ordinary `changes_requested`.
- Movement resets it: a finding resolved, or a new finding raised, means stagnation zero — including
  the case where a *new* finding is raised while every old one is carried.
- **An echoed restatement does not reset it.** `prior_findings` restating an open finding as `open`
  leaves `last_status_change_turn` alone, so the gate still trips. This is round 1's `f1` turned into
  a test, and it is the property that distinguishes this design from revision 1.
- `open_count == 0` never trips, however old the last movement.
- An empty ledger (no finding ever raised) never trips.
- `--stagnant-session-turns 0` never trips.
- A finding carrying no `last_verified_turn` — a pre-#77 ledger — appears in the warning as `unknown`
  rather than as a substituted turn number.
- Precedence, one test per *reachable* co-occurrence: `session_stagnant` beats `open_findings`,
  `reviewer_blocked` and `verdict_contradiction`, and loses to `ledger_too_large`,
  `ledger_unavailable` and `turn_not_durable`.
- The prompt is unchanged — pinned by the existing prompt tests continuing to pass unmodified.

## Verification

`build.ps1` (fmt, clippy `-D warnings`, unit tests, release build). The reviewer protocol is
unchanged — no prompt bytes move — so this does not meet the `AGENTS.md` bar that mandates
`smoke.ps1`. One `smoke.ps1 -Reviewer codex` run is still worth its tokens, because session
resumability and terminal-state persistence do change and those are end-to-end paths; round 2 agreed.
Its cost is stated to the user before it runs.
