# Rewinding a Claude reviewer after a cancelled turn (issue #209)

Status: **implemented** (issue #209). Cut down after its first cross-model review; see "What was
cut, and why".

## Problem

A reviewer turn sets the findings write-ahead marker before it starts the reviewer. Three kinds of
failure clear it again, because they provably ran nothing:

- the invocation could not be built;
- the child process never started;
- `REVIEWER_BUSY` (#166/#207): the Claude CLI hit its OAuth refresh lock before any model call;
- the pre-spawn checks before a busy relaunch refused (#207), so the relaunch never started.

Otherwise the marker is cleared only once the turn is durably recorded. Any other failure after the
child started leaves it set, and that includes `cross_model_review_cancel`. The next non-`fresh`
call is then refused `SESSION_NOT_RESUMABLE`.

The refusal protects against the reviewer CLI's conversation being *ahead* of the ledger. A reviewer
that raised a blocker in a turn the server never recorded may believe the blocker is on the record,
not raise it again, and let the session converge without it.

#209 is the cost of that. A caller cancelled a re-review because the change had moved on. The
session was then refused, and going `fresh: true` lost the reviewer's memory and meant carrying the
open findings across by hand.

## The fix in one line

After an explicit cancel, rewind the reviewer's conversation to the last recorded turn. The ledger is
already there: a cancelled turn records nothing. The two then agree again, and the session resumes
normally.

## Verified mechanism (Claude)

Verified on 2026-11-10 with real calls on Haiku. Turn 1 asked the model to remember ALPHA and turn
2 to remember BRAVO. Then:

- `claude -p --resume <id> --resume-session-at <uuid> --fork-session`, with `<uuid>` set to the
  top-level `uuid` of turn 1's last stream-json `assistant` event, answered `ALPHA`, under a new
  session id.
- The API message id (`msg_…`) in place of the `uuid` produced no answer.
- A plain `--resume <id>` of the original still answered `ALPHA, BRAVO`, so the fork leaves the
  original conversation untouched.

`--resume-session-at` does not appear in `--help`. Because it could change without notice, every
step below falls back to today's refusal, never to something weaker.

## Design

### 1. Remember where each recorded turn ends

`SessionRecord` gains one optional field, `claude_rewind_uuid`, written by `record_turn` for a Claude
reviewer. It is the `uuid` of the last `assistant` event of the **last reviewer run the turn
spawned**: the main run, or the final block or evidence repair.

It is written **only if every run the turn spawned completed and was parsed cleanly.** A repair that
failed after it started may have advanced the conversation in ways the stream does not show. A
run with no stream-json output has no `uuid`. In either case the field is cleared, so that turn can
never be the target of a rewind.

**Not changed by this plan:** today, a repair that fails after starting does not stop its turn from
recording as a degraded turn. The conversation can then be slightly ahead of what was recorded, and
the next call plain-resumes it. That is existing behaviour on a path with no cancel and no marker,
and this plan leaves it as it is. All the plan ensures is that such a turn is never used as a
rewind target. Whether a started-but-failed repair should make its turn non-durable is a separate
question for a separate issue: #211.

### 2. Mark a cancelled turn as rewindable, and only that

The marker stays an empty file on every path that writes it today, which means "not rewindable".
There is one new write. When the worker's run of a turn ends in `CANCELLED` after its child started,
and the turn did not record, the worker rewrites the marker with the conversation it belongs to:

```json
{"rewind_after_cancel": {"cli_session_id": "<record.cli_session_id>", "turns": <record.turns>}}
```

Nothing else writes this content. Every other way a marker can be left set therefore keeps today's
refusal unchanged:

- a crash, at any point;
- the post-run account refusal (switch guard);
- the resumed-session-id mismatch;
- a timeout or policy stall;
- a failed record or a failed marker clear.

It also covers these:

- **A crash between the cancel and the rewrite** leaves the empty marker, which is not rewindable.
  That is the safe direction.
- **A cancelled `fresh: true` attempt** has no record identity to write: its conversation is new,
  and the stored record still points at the old one. It writes nothing, so it stays refused.
- **A cancel that lands after the turn recorded** never reaches this path: the marker is already
  cleared, and the next call resumes normally.

### 3. At the gate, allow exactly one case

In `start_review`, at the existing marker gate, for a non-`fresh` call: when the marker parses as
`rewind_after_cancel`, its `cli_session_id` and `turns` equal the stored record's, the record has a
`claude_rewind_uuid`, the reviewer is Claude, and every existing `resume_block` check passes, the
call proceeds as a **rewound resume** instead of being refused. Anything else returns today's
refusal, word for word.

**The reviewer's working-directory mode is checked here too** (review f8). Later, `start_review`
drops the prior record when `reviewer_cwd_mode` has changed since it was written, and starts a new
conversation. On a rewound resume that would quietly turn a call the caller sent as a resume into a
turn 1 with no ledger. So the rewind also requires the stored mode to equal the mode of the entry
that will run. A mismatch gives today's refusal: the caller then chooses `fresh: true` knowingly.

### 4. The rewound launch

The Claude invocation adds `--resume-session-at <claude_rewind_uuid> --fork-session` to its usual
`--resume <cli_session_id>`. The launch is otherwise identical: same pre-spawn checks, same lock,
same write-ahead marker, now empty again.

The run comes back under the fork's new session id:

- **The main run must report a non-empty session id that differs from the original** (review f7).
  Today a run with no reported id falls back to the resumed id. On a rewound launch that would point
  repairs, and the record, at the original, un-rewound conversation. So a missing or unchanged id
  fails the turn: it is not recorded and the marker stays set, empty, which means today's refusal.
  The fork id is then the one repair target, passed explicitly.
- `resumed_session_id_mismatch` accepts a different id for exactly this launch.
- The turn's record stores the new id as `cli_session_id` and the run's new `claude_rewind_uuid`.
- A block repair resumes the fork, through `parsed.session_id` as today.

If the rewound turn itself fails, the stored record is unchanged. The next call sees a new marker:
an empty one, or another cancel marker bound to the same record, which rewinds from the same point
again.

### 5. What the caller sees

- **A rewound turn** carries a warning: the previous turn was cancelled, and the reviewer was
  rewound to turn N, the last recorded one. It does not remember the cancelled turn, and nothing from
  it is on the record.
- **The `cross_model_review_cancel` reply** waits for the worker to finish, up to 10s. It then reads
  what the cancel left behind and states only the cancel's own effect (code review f2):
  - **No marker** (the cancel landed before the reviewer started, or after the turn recorded): the
    session stands at its last recorded turn, as before.
  - **A cancel marker that passes the gate in step 3:** a Claude reviewer whose last recorded turn
    has a rewind point. The next call rewinds the reviewer to that turn instead of refusing the
    session.
  - **Any other marker:** the next call needs `fresh: true`, carrying the open findings, which the
    reply lists by id and title from the stored ledger.

  The reply does not promise that the next call resumes. Checks unrelated to the cancel, such as the
  turn and idle limits, a terminal state, or the next call's own arguments, are made by
  `resume_block` at that point. The third case is also the whole improvement for Codex.
- **A cancel that lands after the main run,** during a block or evidence repair or while the turn is
  being finalized, cancels the whole turn: nothing is recorded, and the marker is made rewindable as
  for a main-run cancel (code review f1).
- **Consults** use the same marker and the same resume, so the same rewind applies, with no ledger
  involved.

## What was cut, and why

The first draft also rewound Codex, through `thread/revert` in the experimental `codex app-server`
protocol. That works: verified with `codex exec resume` after a revert. It also rewound after any
unrecorded turn, not only a cancel. The plan's first review raised six findings; four were major.
Most came from those two choices, so both were cut rather than fortified (AGENTS.md, "How much
rigor, and where"):

- **Codex.** Its exec stream carries no turn id, so the cutoff had to come from wall-clock times.
  Those are open to clock skew, and a server turn can contain several CLI turns (f1). It also needs
  a second, long-lived way of starting the Codex CLI, without `--ignore-user-config`, that would have
  to be shown to match the reviewer's whole isolation posture (f4). The issue was reported against a
  Claude reviewer. Codex keeps today's refusal, with the better cancel reply.
- **Any unrecorded turn.** Every path that deliberately leaves the marker set for a forced
  rebaseline would have needed an exclusion, and the exclusions were not crash-durable (f2). An
  opt-in written only by the cancel path inverts that: the default stays a refusal.

The verified Codex mechanism is recorded here in case a later issue wants it:
`initialize` → `thread/resume {threadId, excludeTurns: true}` → `thread/revert {threadId,
beforeTurnId}`. It needs no sign-in, makes no model call, and `codex exec resume` honours it.

## Tests

- **Unit:**
  - marker round trip, with an empty or unparseable marker read as not rewindable;
  - the gate table: marker kind, identity match or mismatch, a missing `claude_rewind_uuid`, a Codex
    reviewer, a `fresh` call;
  - `claude_rewind_uuid` captured from the main run, from the final repair, and cleared when any run
    failed after starting or produced no stream-json;
  - the rewound launch's flags;
  - the fork's session id accepted only on a rewound launch;
  - the cancel reply's two forms.
- **End to end,** which costs tokens:
  - `smoke.ps1 -Reviewer claude`;
  - a manual Claude check: review, re-review, cancel the re-review mid-turn, call again on the same
    session. Expect a resumed turn with the rewind warning, a reviewer that remembers turn 1, and the
    ledger unchanged.
  - `smoke.ps1 -Reviewer codex`, to confirm Codex is unchanged.
