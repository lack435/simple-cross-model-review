# Rewinding the reviewer after an unrecorded turn (issue #209)

Status: **plan**, not implemented.

## Problem

Every reviewer turn sets the findings write-ahead marker before the reviewer starts, and clears it
only once the turn is durably recorded. A turn that is cancelled, times out, crashes, or fails after
the child started leaves the marker set. The next non-`fresh` call is then refused
`SESSION_NOT_RESUMABLE`. That refusal exists because the reviewer CLI's own conversation may now be
*ahead* of the ledger. A reviewer that raised a blocker in a turn the server never recorded may
believe that blocker is on the record, not raise it again, and let the session converge without it.

The cost is the one #209 reports. The caller has to start `fresh: true`, the reviewer loses its
memory of the session, and the open findings have to be carried across by hand.

## The observation that makes this fixable

The ledger never needs rolling back. An unrecorded turn writes nothing to it, so it already stands
at the last durable turn. Only the reviewer CLI's conversation is ahead. If the conversation can be
rewound to the point where the ledger stands, the two agree again and the session can resume
normally.

Both reviewer CLIs can do that. **Verified** on 2026-11-10 with real calls. In each case turn 1 asked
the model to remember ALPHA and turn 2 to remember BRAVO. After a rewind past turn 2, the model was
asked what it remembered:

- **Codex 0.165.0.** `codex exec` has no rewind. The experimental `codex app-server` JSON-RPC
  protocol does: `initialize`, then `thread/resume {threadId, excludeTurns: true}`, then
  `thread/revert {threadId, beforeTurnId}`. A revert without the resume fails with "thread not
  found". The revert needs no sign-in and makes no model call. Codex keeps the original rollout file
  and writes a new one for the same thread id, whose `history_base` points at the old file up to the
  reverted turn. **`codex exec resume <thread>` honours it:** it answered `ALPHA` only. Turn ids come
  from `thread/turns/list`. Each turn carries `startedAt` (seconds), and the schema documents turn
  ids as UUIDv7, so each id embeds its start time in milliseconds. Also verified on a copy of a real
  5-turn review thread: `turns/list` went from 5 turns to 4.
- **Claude Code.** The hidden flag `--resume-session-at <uuid>`, with `--fork-session`. The value
  must be the top-level `uuid` of a stream-json `assistant` event; the API message id (`msg_…`)
  produced no answer. The fork answered `ALPHA` under a new session id. A plain resume of the
  original still answered `ALPHA, BRAVO`, so the original conversation is untouched.

Neither interface is a supported one: `app-server` is marked experimental, and
`--resume-session-at` is absent from `--help`. That is why every rewind below falls back to today's
refusal when it cannot be carried out, rather than to something weaker.

## Design

### What the server remembers

Two new optional fields on `SessionRecord`, written by `record_turn`:

- `recorded_at_ms`: wall-clock milliseconds when the turn was durably recorded.
- `claude_rewind_uuid`: for a Claude reviewer, the `uuid` of the last `assistant` event of the last
  run whose answer the turn used: the main run, or the block repair that produced the accepted block.
  Absent when that run's output was not stream-json; see scope.

The findings write-ahead marker gets content instead of being empty: the wall-clock milliseconds
at which it was set, `mark_ms`, plus a `rewindable` flag. An empty marker, as written by today's
binary, is read as not rewindable, so an upgrade changes nothing for a session already stuck.

### When a rewind is allowed

At the existing marker gate in `start_review`, for a non-`fresh` call, before the refusal:

1. The marker is **readable, rewindable, and set after the last record**, meaning
   `mark_ms > record.recorded_at_ms`. If instead `mark_ms <= recorded_at_ms`, the turn *was*
   recorded and only the marker clear failed. Then the conversation already matches the ledger: clear
   the marker and resume with no rewind.
2. A record exists. A failed turn 1 has nothing to rewind to; it keeps today's refusal, and
   `fresh: true` costs nothing there.
3. Every check `resume_block` already makes still passes: reviewer, model, working root, kind,
   bindings, turn and idle limits.
4. The reviewer has a rewind point. For Codex that is `recorded_at_ms`. For Claude it is
   `claude_rewind_uuid`.

Anything else gives today's refusal, unchanged.

**An account refusal is never rewindable.** When the post-run switch guard refuses a turn because
the profile's account moved, the marker is deliberately left set so that the session rebaselines.
That path rewrites the marker with `rewindable: false`. A crash leaves the marker as written, which
is rewindable: the ledger is then simply at the last durable turn, as for a cancel.

### Doing the rewind

The rewind runs inside `attempt`, **after** the pre-spawn authorization and identity checks and with
the per-home setup lock held, exactly where the reviewer itself would launch. It runs **before**
this turn's write-ahead marker is written. So a rewind that fails returns today's refusal, and the
next call sees the same marker and can try again.

- **Codex.** Start `codex app-server` with the same resolved binary, controlled environment and
  pinned `CODEX_HOME`. Call `initialize`, then `thread/resume`, then `thread/turns/list` in ascending
  order. Revert before the first turn whose start time, taken from its UUIDv7 id, is after
  `recorded_at_ms`. If there is no such turn, the failed run never started one and there is nothing
  to revert. Two sanity checks, either of which refuses:
  - the number of turns kept is at least `record.turns`, since every recorded server turn left at
    least one Codex turn;
  - the revert reports success, and a second `turns/list` shows exactly the kept prefix.
  
  These guard against clock trouble or a protocol change reverting recorded history. The process is
  bounded by a short timeout, run under the job object, and reaped like every other child.
- **Claude.** There is no separate step. The launch adds
  `--resume-session-at <claude_rewind_uuid> --fork-session` to the usual `--resume <id>`. The fork
  comes back under a new session id. That turn's record stores it as `cli_session_id`, and
  `resumed_session_id_mismatch` accepts it for exactly this launch. Its `claude_rewind_uuid` is taken
  from the forked run, as on any other turn.

### What the caller sees

A rewound turn carries a warning: the previous turn did not finish, and the reviewer was rewound to
the last recorded turn, N. Findings from the unrecorded turn are not on the record, and the
reviewer no longer remembers them. The `cross_model_review_cancel` reply gains one sentence: the
next call in the session resumes from the last recorded turn where the reviewer supports it, and
otherwise needs `fresh: true`.

Consults use the same marker, and a consult conversation is resumed the same way. The same rewind
applies, with no ledger involved.

## The boundary that moves

This adds a **second way of starting the Codex CLI** next to the reviewer launch: a short-lived
`codex app-server` that runs no model and only edits the thread's history in the pinned profile
home. Before implementation, check and record in this doc:

- whether `thread/resume` starts MCP servers, plugins or network connections (the test output
  included a `remoteControl/status/changed` notification);
- which configuration it loads. `app-server` has no `--ignore-user-config` in its `--help`, so find
  out whether the pinned home's `config.toml`, if present, can change what it does;
- that it writes nothing outside the pinned home's thread store.

If any of these cannot be confined to what `codex exec` already does under the reviewer's posture,
the Codex half is cut and Codex keeps today's refusal. The Claude half adds no new process.

## Scope cuts

- **Claude's buffered output path** (no stream-json, so no assistant `uuid`) gets no rewind point and
  keeps today's refusal. The git and evidence paths are stream-json; this covers the ambient Perforce
  case.
- **A failed turn 1** keeps today's refusal. There is no memory to lose.
- **No retry loop** around a failed rewind: the refusal says so, and the caller can try again or go
  `fresh`.

## Tests

- Unit: marker content round trip, with an empty or legacy marker read as not rewindable; the
  eligibility table (rewindable or not, `mark_ms` against `recorded_at_ms`, no record, account
  refusal); choosing the Codex revert point from a turn list, including no turn to revert, a clock
  anomaly, and too few kept turns; the Claude launch flags; the forked session id accepted only on a
  rewound launch; capture of `claude_rewind_uuid` from the main run and from a repair.
- Fake app-server: a scripted JSON-RPC peer for the Codex step, covering success, an error from
  `thread/revert`, an unchanged `turns/list` after a revert, and a timeout.
- End to end, which costs tokens: `smoke.ps1 -Reviewer codex` and `-Reviewer claude`, plus a manual
  check in each direction: review, re-review, cancel the re-review mid-turn, then call again on the
  same session. Expect a resumed turn with the rewind warning, a reviewer that remembers turn 1, and
  the ledger unchanged.
