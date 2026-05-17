/**
 * Hand-written helper for the PATCH /api/backups/schedules/{id} endpoint.
 *
 * TODO(sdk-regen): replace with generated updateBackupSchedule mutation after
 * next `bun run openapi-ts` run against a server that exposes this endpoint.
 */

/**
 * Patch body for `PATCH /api/backups/schedules/{id}`.
 *
 * All fields are optional; only fields that are present in the object sent to
 * the API will be updated. Omit a field entirely to leave its column unchanged.
 *
 * Note: null-clearing `max_runtime_secs` is not supported via PATCH — send a
 * positive integer to set, or omit to leave unchanged. To clear it, disable
 * the schedule and recreate it.
 */
export interface UpdateBackupScheduleRequest {
  /** New display name. Must not be empty if present. */
  name?: string
  /** New description. Pass `""` to clear the existing description. */
  description?: string
  /**
   * New cron expression. When changed the server recomputes `next_run`.
   * Must have runs at least 1 hour apart.
   */
  schedule_expression?: string
  /** Days to retain backups. Must be >= 1. */
  retention_period?: number
  /**
   * Wall-clock timeout in seconds. Must be >= 60.
   * Send a positive integer to set; omit to leave unchanged.
   */
  max_runtime_secs?: number
  /** Enable or disable the schedule. */
  enabled?: boolean
  /** Replaces the full tag list when present. */
  tags?: string[]
}

async function readJsonOrThrow<T>(response: Response): Promise<T> {
  if (!response.ok) {
    let detail = response.statusText
    try {
      const body = (await response.json()) as { detail?: string; title?: string }
      detail = body.detail ?? body.title ?? detail
    } catch {
      // fall through with statusText
    }
    throw new Error(detail)
  }
  return (await response.json()) as T
}

/**
 * Apply a partial update to an existing backup schedule.
 *
 * Only fields present in `body` are applied; absent fields leave the
 * corresponding column unchanged. On success, returns the updated schedule
 * object (same shape as `BackupScheduleResponse` from the generated SDK).
 */
export async function updateBackupSchedule(
  id: number,
  body: UpdateBackupScheduleRequest,
): Promise<unknown> {
  const response = await fetch(`/api/backups/schedules/${id}`, {
    method: 'PATCH',
    credentials: 'include',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(body),
  })
  return readJsonOrThrow<unknown>(response)
}
