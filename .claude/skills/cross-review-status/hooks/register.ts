import { atom, read, update } from 'claude-code'
import type { EngineInterface, Register } from 'claude-code'

import type { Review, ReviewKind } from '../types'

const reviews = atom({ plugin: 'cross-review-status', key: 'reviews' } as const, {})

// Any server name: `.mcp.json` calls it `cross-review`, but a user may not.
const TOOL = /^mcp__.+__cross_model_(review|consult)(_result|_cancel)?$/
const ENVELOPE = /<<<CROSS_REVIEW_ENVELOPE_OUT:([^>\n]+)>>>\n([\s\S]*?)\n<<<CROSS_REVIEW_ENVELOPE_OUT_END:\1>>>/
const SHOWN = 4
const TICK_MS = 15_000

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

const ago = (ms: number): string => {
  const s = Math.max(0, Math.round(ms / 1000))
  if (s < 60) return `${s}s`
  const m = Math.floor(s / 60)
  return m < 60 ? `${m}m` : `${Math.floor(m / 60)}h${String(m % 60).padStart(2, '0')}`
}

const clip = (s: string, n = 24) => (s.length > n ? `${s.slice(0, n - 1)}…` : s)

const describe = (r: Review, now: number): string => {
  const name = `${r.kind === 'consult' ? 'consult ' : ''}${clip(r.session)}${r.turn ? ` t${r.turn}` : ''}`
  switch (r.status) {
    case 'running':
      return `⟳ ${name} ${ago(now - r.startedAt)}`
    case 'cancelled':
      return `⊘ ${name} cancelled`
    case 'failed':
      return `⚠ ${name} ${r.code ?? 'failed'}`
    case 'completed':
      if (r.kind === 'consult' || r.outcome === null) return `✓ ${name} done`
      if (r.outcome === 'converged') return `✓ ${name} converged`
      if (r.outcome === 'changes_requested') {
        return `✎ ${name} changes requested${r.openCount === null ? '' : ` (${r.openCount} open)`}`
      }
      return `⚠ ${name} ${r.outcome}`
  }
}

const refresh = async ($: EngineInterface) => {
  const all = Object.values((await read($, reviews)) as Record<string, Review>)
  if (all.length === 0) {
    $.ui.status(undefined)
    return
  }
  const now = await $.clock.now()
  const ordered = all.sort((a, b) =>
    a.status === 'running' && b.status !== 'running'
      ? -1
      : b.status === 'running' && a.status !== 'running'
        ? 1
        : (b.finishedAt ?? b.startedAt) - (a.finishedAt ?? a.startedAt),
  )
  const parts = ordered.slice(0, SHOWN).map(r => describe(r, now))
  const more = ordered.length - SHOWN
  $.ui.status(`cross-review · ${parts.join(' · ')}${more > 0 ? ` · +${more}` : ''}`)
}

const put = async ($: EngineInterface, r: Review) => {
  await update($, reviews, prev => ({ ...(prev as Record<string, Review>), [r.session]: r }))
  await refresh($)
}

/** The job a `_result` / `_cancel` call names: by `review_id`, else by `session` as the server allows. */
const target = async (
  $: EngineInterface,
  reviewId: string | null,
  session: string | null,
): Promise<Review | null> => {
  const all = (await read($, reviews)) as Record<string, Review>
  return (
    (reviewId === null ? undefined : Object.values(all).find(r => r.reviewId === reviewId)) ??
    (session === null ? undefined : all[session]) ??
    null
  )
}

// The suffix the server prints after a session name on a result: `(turn 2)`, `(turn 1, new review)`.
const RESULT_SESSION_SUFFIX = / \(turn \d+(?:, [a-z ]+)?\)$/

export const register: Register = on => {
  on('session.start', async ($, e, next) => {
    $.clock.every(TICK_MS, () => void refresh($))
    await refresh($)
    return next(e)
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

    const reviewId = str(args.review_id) ?? field(text, 'review_id')
    const askedSession = str(args.session)
    const known = await target($, reviewId, askedSession)
    const session = field(text, 'session')?.replace(RESULT_SESSION_SUFFIX, '') ?? askedSession
    if (!known && (!reviewId || !session)) return ran
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
