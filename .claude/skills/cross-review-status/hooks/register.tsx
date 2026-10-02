import { atom, read, update } from 'claude-code'
import type { EngineInterface, Register } from 'claude-code'

import type { Review, ReviewKind } from '../types'

const reviews = atom({ plugin: 'cross-review-status', key: 'reviews' } as const, {})
// The time the band draws elapsed times against, advanced by a timer while a job runs.
const clock = atom({ plugin: 'cross-review-status', key: 'now' } as const, 0)

// Any server name: `.mcp.json` calls it `cross-review`, but a user may not.
const TOOL = /^mcp__.+__cross_model_(review|consult)(_result|_cancel)?$/
const ENVELOPE = /<<<CROSS_REVIEW_ENVELOPE_OUT:([^>\n]+)>>>\n([\s\S]*?)\n<<<CROSS_REVIEW_ENVELOPE_OUT_END:\1>>>/
// Finished jobs kept in the band beside every running one.
const FINISHED_SHOWN = 3
const TICK_MS = 5_000

type Record_ = Record<string, unknown>

const field = (text: string, name: string): string | null =>
  new RegExp(`^${name}:\\s+(.+)$`, 'm').exec(text)?.[1]?.trim() ?? null

const asRecord = (v: unknown): Record_ | null =>
  typeof v === 'object' && v !== null && !Array.isArray(v) ? (v as Record_) : null

/** The machine envelope: `structuredContent` when core hands it over, else the `_OUT` text block. */
const envelopeOf = (result: unknown, text: string): Record_ | null => {
  const structured = asRecord(asRecord(result)?.structuredContent)
  if (structured) return structured
  const json = ENVELOPE.exec(text)?.[2]
  if (json === undefined) return null
  try {
    return asRecord(JSON.parse(json))
  } catch {
    return null
  }
}

const num = (v: unknown): number | null => (typeof v === 'number' ? v : null)
const str = (v: unknown): string | null => (typeof v === 'string' ? v : null)

const elapsed = (ms: number): string => {
  const s = Math.max(0, Math.round(ms / 1000))
  if (s < 60) return `${s}s`
  const m = Math.floor(s / 60)
  if (m < 60) return `${m}m${String(s % 60).padStart(2, '0')}s`
  return `${Math.floor(m / 60)}h${String(m % 60).padStart(2, '0')}m`
}

type Look = { glyph: string; color: string; label: string }

const look = (r: Review, now: number): Look => {
  switch (r.status) {
    case 'running':
      return { glyph: '⟳', color: 'cyan', label: `running ${elapsed(now - r.startedAt)}` }
    case 'cancelled':
      return { glyph: '⊘', color: 'gray', label: 'cancelled' }
    case 'failed':
      return { glyph: '✗', color: 'red', label: r.code ?? 'failed' }
    case 'completed':
      if (r.kind === 'consult' || r.outcome === null) return { glyph: '✓', color: 'green', label: 'answered' }
      if (r.outcome === 'converged') return { glyph: '✓', color: 'green', label: 'converged' }
      if (r.outcome === 'changes_requested') {
        const open = r.openCount === null ? '' : ` · ${r.openCount} open`
        return { glyph: '✎', color: 'yellow', label: `changes requested${open}` }
      }
      return { glyph: '⚠', color: 'red', label: r.outcome.replace(/_/g, ' ') }
  }
}

/** Running jobs, then the most recently finished ones. */
const shown = (all: Review[]): Review[] => {
  const running = all.filter(r => r.status === 'running').sort((a, b) => a.startedAt - b.startedAt)
  const finished = all
    .filter(r => r.status !== 'running')
    .sort((a, b) => (b.finishedAt ?? b.startedAt) - (a.finishedAt ?? a.startedAt))
  return [...running, ...finished.slice(0, FINISHED_SHOWN)]
}

const tick = async ($: EngineInterface) => {
  const now = await $.clock.now()
  await update($, clock, () => now)
}

const put = async ($: EngineInterface, r: Review) => {
  await update($, reviews, prev => ({ ...(prev as Record<string, Review>), [r.session]: r }))
  await tick($)
}

const clearFinished = ($: EngineInterface) =>
  update($, reviews, prev =>
    Object.fromEntries(Object.entries(prev as Record<string, Review>).filter(([, r]) => r.status === 'running')),
  )

/**
 * The job a call names, looked up as the server does (src/tools.rs): by `review_id` when one is
 * given, and only by `session` when it is not.
 */
const target = async (
  $: EngineInterface,
  reviewId: string | null,
  session: string | null,
): Promise<Review | null> => {
  const all = (await read($, reviews)) as Record<string, Review>
  if (reviewId !== null) return Object.values(all).find(r => r.reviewId === reviewId) ?? null
  return session === null ? null : (all[session] ?? null)
}

// The suffix the server prints after a session name on a result: `(turn 2)`, `(turn 1, new review)`.
const RESULT_SESSION_SUFFIX = / \(turn \d+(?:, [a-z ]+)?\)$/

export const register: Register = on => {
  on('session.start', async ($, e, next) => {
    // 0.1.0 pinned a status line, which outlives a reload of the module; take it down.
    $.ui.status(undefined)
    // Redraws elapsed times only while something is running.
    $.clock.every(TICK_MS, () => {
      void read($, reviews).then(all => {
        if (Object.values(all as Record<string, Review>).some(r => r.status === 'running')) void tick($)
      })
    })
    await tick($)
    return next(e)
  })

  on('ui.render', { component: 'AbovePrompt' }, async ($, e, next) => {
    // Whatever the plugins beneath and the engine draw stays, below this plugin's rows.
    const below = await next(e)
    const rows = shown(Object.values((await read($, reviews)) as Record<string, Review>))
    if (e.props.hasSurvey || rows.length === 0) return below

    const now = await read($, clock)
    const { Box, Button, Text } = $.ui.resolve(e)
    const hasFinished = rows.some(r => r.status !== 'running')

    return (
      <Box flexDirection="column">
        <Box flexDirection="row" gap={2}>
          <Box flexShrink={0}>
            <Text bold>cross-review</Text>
          </Box>
          <Box flexDirection="column" flexGrow={1} flexShrink={1} minWidth={0}>
            {rows.map(r => {
              const { glyph, color, label } = look(r, now)
              return (
                <Box key={`row:${r.session}`} flexDirection="row" gap={1}>
                  <Box flexShrink={0}>
                    <Text color={color}>{glyph}</Text>
                  </Box>
                  <Box flexShrink={1} minWidth={0}>
                    <Text wrap="truncate-end">{r.kind === 'consult' ? `consult ${r.session}` : r.session}</Text>
                  </Box>
                  {r.turn === null ? null : (
                    <Box flexShrink={0}>
                      <Text dimColor>{`t${r.turn}`}</Text>
                    </Box>
                  )}
                  <Box flexShrink={0}>
                    <Text color={color}>{label}</Text>
                  </Box>
                </Box>
              )
            })}
          </Box>
          {hasFinished ? (
            <Box flexShrink={0}>
              {/* Drawn as the X other bands close with: the desktop does not yet draw role="dismiss" itself. */}
              <Button key="clear" label="✕" plain dimColor role="dismiss" onPress={() => clearFinished($)} />
            </Box>
          ) : null}
        </Box>
        {below}
      </Box>
    )
  })

  on('tool.call', async ($, e, next) => {
    const match = TOOL.exec(e.tool)
    if (!match) return next(e)

    const kind = match[1] as ReviewKind
    const verb = match[2] ?? 'start'
    const args = e as unknown as Record_
    const ran = await next(e)
    const text = ran.text ?? ''
    const now = await $.clock.now()

    if (verb === 'start') {
      const session = field(text, 'session')
      const reviewId = field(text, 'review_id')
      const asked = str(args.session) ?? 'review'
      if (ran.deny !== undefined || ran.isError || !reviewId || !session) {
        // A refused start (SESSION_BUSY, most often) leaves the job already running untouched.
        if ((await target($, null, asked))?.status === 'running') return ran
        await put($, {
          session: asked,
          reviewId: reviewId ?? '',
          kind,
          turn: null,
          status: 'failed',
          outcome: null,
          openCount: null,
          code: field(text, 'code') ?? (ran.deny !== undefined ? 'denied' : null),
          startedAt: now,
          finishedAt: now,
        })
        return ran
      }
      // `session:   name (new)` or `session:   name (resumed, turn 3)`
      const parsed = /^(.*) \((?:new|resumed, turn (\d+))\)$/.exec(session)
      await put($, {
        session: parsed?.[1] ?? asked,
        reviewId,
        kind,
        turn: parsed?.[2] ? Number(parsed[2]) : 1,
        status: 'running',
        outcome: null,
        openCount: null,
        code: null,
        startedAt: now,
        finishedAt: null,
      })
      return ran
    }

    const known = await target($, str(args.review_id), str(args.session))
    const reviewId = field(text, 'review_id') ?? known?.reviewId ?? null
    const session = field(text, 'session')?.replace(RESULT_SESSION_SUFFIX, '') ?? null
    // A job this mod never saw is recorded only from a response that names it; an error for one
    // (an unknown or evicted id) says nothing about any job the band shows.
    if (!known && (ran.deny !== undefined || ran.isError || !reviewId || !session)) return ran
    // A job started before this mod loaded: rebuild what we can from the response.
    const base: Review = known ? { ...known, reviewId: reviewId ?? known.reviewId } : {
      session: session ?? '',
      reviewId: reviewId ?? '',
      kind,
      turn: null,
      status: 'running',
      outcome: null,
      openCount: null,
      code: null,
      startedAt: now,
      finishedAt: null,
    }

    if (verb === '_cancel') {
      if (ran.deny === undefined && !ran.isError) {
        await put($, { ...base, status: 'cancelled', finishedAt: now })
      }
      return ran
    }

    if (ran.deny !== undefined) return ran
    if (ran.isError) {
      await put($, { ...base, status: 'failed', code: field(text, 'code'), finishedAt: now })
      return ran
    }

    const env = envelopeOf(ran.result, text)
    const status = str(env?.result_status) ?? str(env?.status) ?? field(text, 'status')
    const turn = num(env?.turn) ?? base.turn

    if (status === 'running') {
      const elapsed = num(env?.elapsed_seconds)
      await put($, {
        ...base,
        turn,
        status: 'running',
        startedAt: elapsed === null ? base.startedAt : now - elapsed * 1000,
      })
    } else if (status === 'completed') {
      await put($, {
        ...base,
        turn,
        status: 'completed',
        outcome: str(env?.outcome),
        openCount: num(env?.open_count),
        finishedAt: now,
      })
    }
    return ran
  })
}
