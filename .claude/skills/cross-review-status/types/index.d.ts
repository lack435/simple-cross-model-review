export type ReviewKind = 'review' | 'consult'

export type ReviewStatus = 'running' | 'completed' | 'failed' | 'cancelled'

/** The latest job seen for one cross-review session name. */
export type Review = {
  session: string
  reviewId: string
  kind: ReviewKind
  turn: number | null
  status: ReviewStatus
  /** The envelope's `outcome` (converged, changes_requested, escalate, rebaseline). */
  outcome: string | null
  openCount: number | null
  /** The failure code (RATE_LIMITED, SESSION_NOT_RESUMABLE, ...) when the job failed. */
  code: string | null
  startedAt: number
  finishedAt: number | null
}

declare module 'claude-code' {
  interface PluginState {
    'cross-review-status': { reviews: Record<string, Review>; now: number }
  }
}
