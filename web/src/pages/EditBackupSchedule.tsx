'use client'

/**
 * Standalone page for editing an existing backup schedule.
 *
 * Route: /backups/s3-sources/:id/schedules/:scheduleId/edit
 *
 * Replaces the modal-based "Edit Schedule" dialog that previously lived inside
 * S3SourceDetail. Using a routed page means the form is never constrained to
 * modal height on small screens.
 *
 * Only fields that differ from the original schedule are included in the PATCH
 * body, keeping the audit log clean.
 */

import {
  getBackupScheduleOptions,
  getS3SourceOptions,
} from '@/api/client/@tanstack/react-query.gen'
import { Button } from '@/components/ui/button'
import {
  Card,
  CardContent,
  CardDescription,
  CardFooter,
  CardHeader,
  CardTitle,
} from '@/components/ui/card'
import { Input } from '@/components/ui/input'
import { Label } from '@/components/ui/label'
import { RadioGroup, RadioGroupItem } from '@/components/ui/radio-group'
import { Skeleton } from '@/components/ui/skeleton'
import { useBreadcrumbs } from '@/contexts/BreadcrumbContext'
import { usePageTitle } from '@/hooks/usePageTitle'
import {
  UpdateBackupScheduleRequest,
  updateBackupSchedule,
} from '@/lib/backup-schedules'
import { scheduleOptions } from '@/lib/schedule-options'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { ArrowLeft } from 'lucide-react'
import { useEffect, useState } from 'react'
import { Link, Navigate, useNavigate, useParams } from 'react-router-dom'
import { toast } from 'sonner'

export function EditBackupSchedule() {
  const { id, scheduleId } = useParams<{ id: string; scheduleId: string }>()
  const sourceId = id ? parseInt(id, 10) : undefined
  const scheduleIdNum = scheduleId ? parseInt(scheduleId, 10) : undefined
  const navigate = useNavigate()
  const { setBreadcrumbs } = useBreadcrumbs()
  const queryClient = useQueryClient()

  // All hooks before any early return.
  const { data: source } = useQuery({
    ...getS3SourceOptions({ path: { id: sourceId! } }),
    enabled: !!sourceId,
  })

  const {
    data: schedule,
    isLoading: isLoadingSchedule,
    isError: isScheduleError,
  } = useQuery({
    ...getBackupScheduleOptions({ path: { id: scheduleIdNum! } }),
    enabled: !!scheduleIdNum,
  })

  // Form state is seeded from the loaded schedule via the effect below
  // (`setSeeded`), so the initial values here are placeholders until then.
  const [name, setName] = useState('')
  const [description, setDescription] = useState('')
  const [retentionPeriod, setRetentionPeriod] = useState(7)
  const [maxRuntimeHours, setMaxRuntimeHours] = useState<number | ''>('')
  const [enabled, setEnabled] = useState(true)
  const [selectedPreset, setSelectedPreset] = useState<string>(
    scheduleOptions[1].value,
  )
  const [customCron, setCustomCron] = useState('')
  const [seeded, setSeeded] = useState(false)

  // Seed form state from the loaded schedule (once).
  useEffect(() => {
    if (!schedule || seeded) return
    setName(schedule.name)
    setDescription(schedule.description ?? '')
    setRetentionPeriod(schedule.retention_period)
    setMaxRuntimeHours(
      schedule.max_runtime_secs
        ? Math.round(schedule.max_runtime_secs / 3600)
        : '',
    )
    setEnabled(schedule.enabled)
    const preset = scheduleOptions.find(
      (o) => !o.customizable && o.value === schedule.schedule_expression,
    )
    setSelectedPreset(preset ? preset.value : 'custom')
    setCustomCron(preset ? '' : schedule.schedule_expression)
    setSeeded(true)
  }, [schedule, seeded])

  useEffect(() => {
    setBreadcrumbs([
      { label: 'Backups', href: '/backups' },
      {
        label: source?.name ?? 'S3 Source',
        href: `/backups/s3-sources/${id}`,
      },
      { label: schedule?.name ?? 'Edit Schedule' },
    ])
  }, [setBreadcrumbs, id, source?.name, schedule?.name])

  usePageTitle(schedule ? `Edit — ${schedule.name}` : 'Edit Schedule')

  const mutation = useMutation({
    mutationFn: (body: UpdateBackupScheduleRequest) =>
      updateBackupSchedule(scheduleIdNum!, body),
    onSuccess: () => {
      toast.success('Backup schedule updated')
      void queryClient.invalidateQueries({
        queryKey: ['list-backup-schedules'],
      })
      void queryClient.invalidateQueries({ queryKey: ['BackupSchedules'] })
      navigate(`/backups/s3-sources/${id}`)
    },
    onError: (err: unknown) => {
      const message = err instanceof Error ? err.message : 'Update failed'
      toast.error('Failed to update schedule', { description: message })
    },
  })

  if (!sourceId || !scheduleIdNum) {
    return <Navigate to="/backups" replace />
  }

  // Guard: the loaded schedule must belong to the source in the URL to prevent
  // silently re-binding a schedule to a different source's detail page.
  if (schedule && schedule.s3_source_id !== sourceId) {
    return (
      <div className="space-y-6 max-w-3xl mx-auto p-4 md:p-6">
        <Card>
          <CardHeader>
            <CardTitle>Schedule not found</CardTitle>
            <CardDescription>
              Schedule {scheduleIdNum} does not belong to S3 source {sourceId}.
            </CardDescription>
          </CardHeader>
          <CardFooter>
            <Button variant="outline" asChild>
              <Link to={`/backups/s3-sources/${id}`}>
                <ArrowLeft className="mr-2 h-4 w-4" />
                Back to S3 source
              </Link>
            </Button>
          </CardFooter>
        </Card>
      </div>
    )
  }

  function handleSubmit() {
    if (!schedule) return

    const cron = selectedPreset === 'custom' ? customCron : selectedPreset

    if (!name.trim()) {
      toast.error('Schedule name is required')
      return
    }
    if (!cron) {
      toast.error('Please select or enter a schedule expression')
      return
    }

    // Build the PATCH body with only the fields that actually changed so the
    // audit log stays clean and no-op fields are not written to the DB.
    const body: UpdateBackupScheduleRequest = {}

    if (name !== schedule.name) body.name = name
    if (description !== (schedule.description ?? ''))
      body.description = description
    if (cron !== schedule.schedule_expression) body.schedule_expression = cron
    if (retentionPeriod !== schedule.retention_period)
      body.retention_period = retentionPeriod
    if (enabled !== schedule.enabled) body.enabled = enabled

    const newMaxSecs =
      maxRuntimeHours === '' ? undefined : Number(maxRuntimeHours) * 3600
    const existingMaxSecs = schedule.max_runtime_secs ?? undefined
    if (newMaxSecs !== existingMaxSecs && newMaxSecs !== undefined) {
      body.max_runtime_secs = newMaxSecs
    }

    mutation.mutate(body)
  }

  return (
    <div className="space-y-6 max-w-3xl mx-auto p-4 md:p-6">
      <div className="flex items-center gap-2">
        <Button variant="ghost" size="sm" asChild>
          <Link to={`/backups/s3-sources/${id}`}>
            <ArrowLeft className="mr-2 h-4 w-4" />
            Back to S3 source
          </Link>
        </Button>
      </div>

      <Card>
        <CardHeader>
          <CardTitle>Edit backup schedule</CardTitle>
          <CardDescription>
            Update this schedule's name, cadence, or retention settings.
          </CardDescription>
        </CardHeader>

        <CardContent className="space-y-6">
          {isLoadingSchedule || !seeded ? (
            // Skeleton placeholders that match the real form layout so the
            // page does not collapse then expand when data arrives.
            <div className="space-y-6">
              <div className="grid gap-2">
                <Skeleton className="h-4 w-32" />
                <Skeleton className="h-10 w-full" />
              </div>
              <div className="grid gap-2">
                <Skeleton className="h-4 w-40" />
                <Skeleton className="h-10 w-full" />
              </div>
              <div className="grid gap-2">
                <Skeleton className="h-4 w-24" />
                <div className="space-y-3">
                  <Skeleton className="h-6 w-full" />
                  <Skeleton className="h-6 w-full" />
                  <Skeleton className="h-6 w-full" />
                  <Skeleton className="h-6 w-full" />
                  <Skeleton className="h-6 w-full" />
                </div>
              </div>
              <div className="grid gap-2">
                <Skeleton className="h-4 w-36" />
                <Skeleton className="h-10 w-full" />
              </div>
              <div className="grid gap-2">
                <Skeleton className="h-4 w-36" />
                <Skeleton className="h-10 w-full" />
              </div>
            </div>
          ) : isScheduleError ? (
            <p className="text-sm text-destructive">
              Failed to load schedule. Please go back and try again.
            </p>
          ) : (
            <>
              <div className="grid gap-2">
                <Label htmlFor="edit-name">Schedule Name</Label>
                <Input
                  id="edit-name"
                  value={name}
                  onChange={(e) => setName(e.target.value)}
                />
              </div>

              <div className="grid gap-2">
                <Label htmlFor="edit-description">Description (Optional)</Label>
                <Input
                  id="edit-description"
                  placeholder="Daily backup at midnight"
                  value={description}
                  onChange={(e) => setDescription(e.target.value)}
                />
              </div>

              <div className="grid gap-2">
                <Label>Schedule</Label>
                <RadioGroup
                  value={selectedPreset}
                  onValueChange={setSelectedPreset}
                  className="gap-4"
                >
                  {scheduleOptions.map((option) => (
                    <div
                      key={option.value}
                      className="flex items-start space-x-3 space-y-0"
                    >
                      <RadioGroupItem
                        value={option.value}
                        id={`edit-${option.value}`}
                      />
                      <div className="grid gap-1.5 leading-none">
                        <Label
                          htmlFor={`edit-${option.value}`}
                          className="text-sm font-medium leading-none peer-disabled:cursor-not-allowed peer-disabled:opacity-70"
                        >
                          {option.label}
                        </Label>
                        <p className="text-sm text-muted-foreground">
                          {option.description}
                        </p>
                      </div>
                    </div>
                  ))}
                </RadioGroup>
                {selectedPreset === 'custom' && (
                  <div className="mt-4">
                    <Label htmlFor="edit-custom-cron">
                      Custom Cron Expression
                    </Label>
                    <Input
                      id="edit-custom-cron"
                      placeholder="0 0 * * *"
                      value={customCron}
                      onChange={(e) => setCustomCron(e.target.value)}
                      className="mt-1"
                    />
                    <p className="text-xs text-muted-foreground mt-1">
                      Format: second minute hour day month weekday
                    </p>
                  </div>
                )}
              </div>

              <div className="grid gap-2">
                <Label htmlFor="edit-retention">
                  Retention Period (days)
                </Label>
                <Input
                  id="edit-retention"
                  type="number"
                  min={1}
                  value={retentionPeriod}
                  onChange={(e) =>
                    setRetentionPeriod(parseInt(e.target.value, 10))
                  }
                />
              </div>

              <div className="grid gap-2">
                <Label htmlFor="edit-max-runtime">Max runtime (hours)</Label>
                <Input
                  id="edit-max-runtime"
                  type="number"
                  min={1}
                  step={1}
                  placeholder="auto"
                  value={maxRuntimeHours}
                  onChange={(e) => {
                    const raw = e.target.value
                    setMaxRuntimeHours(raw === '' ? '' : Number(raw))
                  }}
                />
                <p className="text-xs text-muted-foreground">
                  Wall-clock ceiling for one backup attempt. Leave empty to
                  use the engine default.
                </p>
              </div>

              <div className="flex items-center gap-2">
                <input
                  id="edit-enabled"
                  type="checkbox"
                  checked={enabled}
                  onChange={(e) => setEnabled(e.target.checked)}
                  className="h-4 w-4"
                />
                <Label htmlFor="edit-enabled">Enabled</Label>
              </div>
            </>
          )}
        </CardContent>

        <CardFooter className="justify-end gap-2">
          <Button variant="outline" asChild>
            <Link to={`/backups/s3-sources/${id}`}>Cancel</Link>
          </Button>
          <Button
            onClick={handleSubmit}
            disabled={mutation.isPending || isLoadingSchedule || !seeded}
          >
            {mutation.isPending ? 'Saving…' : 'Save changes'}
          </Button>
        </CardFooter>
      </Card>
    </div>
  )
}
