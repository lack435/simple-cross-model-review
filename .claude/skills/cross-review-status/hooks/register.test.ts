import { expect, mock, test } from 'claude-code/testing'
import type { Engine } from 'claude-code/testing'
import type { On, RenderSurface } from 'claude-code'

const T = 'mcp__cross-review__cross_model_review'

const started = (id: string, session: string) =>
  `Review started. It runs in the background.\n\nreview_id: ${id}\nsession:   ${session}\nreviewer:  codex\n`

const envelope = (id: string, value: object, session = 'feat-x (turn 1, new review)') =>
  `status:    completed\nreview_id: ${id}\nsession:   ${session}\n\n` +
  `<<<CROSS_REVIEW_ENVELOPE_OUT:${id}>>>\n${JSON.stringify(value)}\n<<<CROSS_REVIEW_ENVELOPE_OUT_END:${id}>>>\n`

/** Answers each cross-review tool with the canned text queued for it. */
const harness = (on: On, answers: Record<string, { text: string; isError?: boolean }[]>) => {
  const clock = mock.clock(on, { now: 1_000_000 })
  on('tool.call', ($, e) => {
    const next = answers[e.tool]?.shift()
    if (!next) throw new Error(`no answer queued for ${e.tool}`)
    return { result: { content: [{ type: 'text', text: next.text }] }, text: next.text, isError: next.isError }
  })
  // The engine's own band beneath the plugins: a marker the mod must keep.
  on('ui.render', { component: 'AbovePrompt' }, () => ({ type: 'Text', key: 'engine', children: ['engine band'] }))
  return clock
}

const BAND_PROPS = {
  hasSurvey: false,
  isWorking: false,
  maxRows: 12,
  bodyColumns: 120,
  scroll: { offset: 0, bodyRows: 12 },
  view: {},
}

const mountBand = ($: Engine, surface: RenderSurface = 'terminal') =>
  $.ui.mount({ plugin: 'cross-review-status', surface, component: 'AbovePrompt', props: BAND_PROPS })

const leaves = (node: unknown): string[] =>
  typeof node === 'string' ? [node] : ((node as { children?: unknown[] }).children ?? []).flatMap(leaves)

/** Each row the band draws, its texts joined by spaces; and whether the engine's own band survived. */
const band = async ($: Engine, surface: RenderSurface = 'terminal') => {
  const ui = await mountBand($, surface)
  const rows = (await ui.findAll({ type: 'Box' })).filter(b => b.key?.startsWith('row:'))
  const keepsEngine = JSON.stringify(await ui.drawn()).includes('engine band')
  await ui.unmount()
  return { rows: rows.map(r => leaves(r).join(' ')), keepsEngine }
}

test('a started review shows as running, then its outcome', async ($, on) => {
  harness(on, {
    [T]: [{ text: started('rv-1-1', 'feat-x (new)') }],
    [`${T}_result`]: [
      {
        text: envelope('rv-1-1', {
          result_status: 'completed',
          session: 'feat-x',
          turn: 1,
          outcome: 'changes_requested',
          open_count: 3,
        }),
      },
    ],
  })

  await $.tool.call({ tool: T, session: 'feat-x', instructions: 'x' })
  expect(await band($)).toEqual({ rows: ['⟳ feat-x t1 running 0s'], keepsEngine: true })

  await $.tool.call({ tool: `${T}_result`, review_id: 'rv-1-1' })
  for (const surface of ['terminal', 'desktop'] as const) {
    expect((await band($, surface)).rows).toEqual(['✎ feat-x t1 changes requested · 3 open'])
  }
})

test('a resumed review reads its turn; a failure keeps its code', async ($, on) => {
  harness(on, {
    [T]: [
      { text: started('rv-1-2', 'feat-x (resumed, turn 2)') },
      { text: 'REQUEST REJECTED\ncode: SESSION_BUSY\n\nbusy\n', isError: true },
    ],
  })

  await $.tool.call({ tool: T, session: 'feat-x', instructions: 'x' })
  expect((await band($)).rows).toEqual(['⟳ feat-x t2 running 0s'])

  await $.tool.call({ tool: T, session: 'other', instructions: 'x' })
  expect((await band($)).rows).toEqual(['⟳ feat-x t2 running 0s', '✗ other SESSION_BUSY'])
})

test('a refused start leaves the running review in place', async ($, on) => {
  harness(on, {
    [T]: [
      { text: started('rv-1-3', 'feat-x (new)') },
      { text: 'REQUEST REJECTED\ncode: SESSION_BUSY\n\nbusy\n', isError: true },
    ],
  })

  await $.tool.call({ tool: T, session: 'feat-x', instructions: 'x' })
  await $.tool.call({ tool: T, session: 'feat-x', instructions: 'x' })
  expect((await band($)).rows).toEqual(['⟳ feat-x t1 running 0s'])
})

test('a collect by session updates that session, and zero open is shown', async ($, on) => {
  harness(on, {
    [T]: [{ text: started('rv-1-4', 'feat-x (new)') }],
    [`${T}_result`]: [
      {
        text: envelope('rv-1-4', {
          result_status: 'completed',
          turn: 1,
          outcome: 'changes_requested',
          open_count: 0,
        }),
      },
    ],
  })

  await $.tool.call({ tool: T, session: 'feat-x', instructions: 'x' })
  await $.tool.call({ tool: `${T}_result`, session: 'feat-x' })
  expect((await band($)).rows).toEqual(['✎ feat-x t1 changes requested · 0 open'])
})

test('a review started before the mod loaded keeps parentheses in its name', async ($, on) => {
  harness(on, {
    [`${T}_result`]: [
      {
        text: envelope(
          'rv-1-5',
          { result_status: 'completed', turn: 3, outcome: 'converged', open_count: 0 },
          'feat (ui) (turn 3, continuing an earlier review)',
        ),
      },
    ],
  })

  await $.tool.call({ tool: `${T}_result`, review_id: 'rv-1-5' })
  expect((await band($)).rows).toEqual(['✓ feat (ui) t3 converged'])
})

test('an error for an id the mod never saw records nothing, and touches no session', async ($, on) => {
  const unknown = { text: "REQUEST REJECTED\ncode: BAD_REQUEST\n\nNo review with review_id 'rv-9-9' exists\n", isError: true }
  harness(on, {
    [T]: [{ text: started('rv-1-6', 'feat-x (new)') }],
    [`${T}_result`]: [unknown, unknown],
  })

  await $.tool.call({ tool: `${T}_result`, review_id: 'rv-9-9' })
  expect(await band($)).toEqual({ rows: [], keepsEngine: true })

  await $.tool.call({ tool: T, session: 'feat-x', instructions: 'x' })
  await $.tool.call({ tool: `${T}_result`, review_id: 'rv-9-9', session: 'feat-x' })
  expect((await band($)).rows).toEqual(['⟳ feat-x t1 running 0s'])
})

test('elapsed time advances while a job runs, and nothing ticks once none does', async ($, on) => {
  const clock = harness(on, {
    [T]: [{ text: started('rv-1-9', 'feat-x (new)') }],
    [`${T}_result`]: [
      { text: envelope('rv-1-9', { result_status: 'completed', turn: 1, outcome: 'converged', open_count: 0 }) },
    ],
  })
  let ticks = 0
  on('state.set', { plugin: 'cross-review-status', key: 'now' }, ($, e, next) => {
    ticks += 1
    return next(e)
  })

  await $.tool.call({ tool: T, session: 'feat-x', instructions: 'x' })
  await clock.advance(65_000)
  expect((await band($)).rows).toEqual(['⟳ feat-x t1 running 1m05s'])
  // 65s at one tick per 5s, plus the one `put` makes: the counter does see ticks.
  expect(ticks).toBeGreaterThan(10)

  await $.tool.call({ tool: `${T}_result`, review_id: 'rv-1-9' })
  const settled = ticks
  await clock.advance(60_000)
  expect(ticks).toBe(settled)
})

test('other tools pass through untouched', async ($, on) => {
  harness(on, { 'mcp__other__thing': [{ text: 'review_id: nope' }] })
  await $.tool.call({ tool: 'mcp__other__thing' })
  expect(await band($)).toEqual({ rows: [], keepsEngine: true })
})

test('clear drops finished jobs and keeps running ones', async ($, on) => {
  harness(on, {
    [T]: [{ text: started('rv-1-7', 'done (new)') }, { text: started('rv-1-8', 'live (new)') }],
    [`${T}_result`]: [
      { text: envelope('rv-1-7', { result_status: 'completed', turn: 1, outcome: 'converged', open_count: 0 }, 'done (turn 1, new review)') },
    ],
  })

  await $.tool.call({ tool: T, session: 'done', instructions: 'x' })
  await $.tool.call({ tool: `${T}_result`, review_id: 'rv-1-7' })
  await $.tool.call({ tool: T, session: 'live', instructions: 'x' })

  const ui = await mountBand($)
  await ui.press({ key: 'clear' })
  await ui.unmount()
  expect((await band($)).rows).toEqual(['⟳ live t1 running 0s'])
})
