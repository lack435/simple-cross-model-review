import { expect, mock, test } from 'claude-code/testing'
import type { On } from 'claude-code'

const T = 'mcp__cross-review__cross_model_review'

const started = (id: string, session: string) =>
  `Review started. It runs in the background.\n\nreview_id: ${id}\nsession:   ${session}\nreviewer:  codex\n`

const envelope = (id: string, value: object) =>
  `status:    completed\nreview_id: ${id}\nsession:   feat-x (turn 1, new review)\n\n` +
  `<<<CROSS_REVIEW_ENVELOPE_OUT:${id}>>>\n${JSON.stringify(value)}\n<<<CROSS_REVIEW_ENVELOPE_OUT_END:${id}>>>\n`

/** Answers each cross-review tool with the canned text queued for it; records every status line. */
const harness = (on: On, answers: Record<string, { text: string; isError?: boolean }[]>) => {
  mock.clock(on, { now: 1_000_000 })
  const statuses: (string | undefined)[] = []
  on('tool.call', ($, e) => {
    const next = answers[e.tool]?.shift()
    if (!next) throw new Error(`no answer queued for ${e.tool}`)
    return { result: { content: [{ type: 'text', text: next.text }] }, text: next.text, isError: next.isError }
  })
  on('ui.status', ($, e, next) => {
    statuses.push(e.text)
    return next(e)
  })
  return statuses
}

test('a started review shows as running, then its outcome', async ($, on) => {
  const statuses = harness(on, {
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
  expect(statuses.at(-1)).toBe('cross-review · ⟳ feat-x t1 0s')

  await $.tool.call({ tool: `${T}_result`, review_id: 'rv-1-1' })
  expect(statuses.at(-1)).toBe('cross-review · ✎ feat-x t1 changes requested (3 open)')
})

test('a resumed review reads its turn; a failure keeps its code', async ($, on) => {
  const statuses = harness(on, {
    [T]: [
      { text: started('rv-1-2', 'feat-x (resumed, turn 2)') },
      { text: 'REQUEST REJECTED\ncode: SESSION_BUSY\n\nbusy\n', isError: true },
    ],
  })

  await $.tool.call({ tool: T, session: 'feat-x', instructions: 'x' })
  expect(statuses.at(-1)).toBe('cross-review · ⟳ feat-x t2 0s')

  await $.tool.call({ tool: T, session: 'other', instructions: 'x' })
  expect(statuses.at(-1)).toBe('cross-review · ⟳ feat-x t2 0s · ⚠ other SESSION_BUSY')
})

test('other tools pass through untouched', async ($, on) => {
  const statuses = harness(on, { 'mcp__other__thing': [{ text: 'review_id: nope' }] })
  await $.tool.call({ tool: 'mcp__other__thing' })
  expect(statuses).toEqual([])
})
