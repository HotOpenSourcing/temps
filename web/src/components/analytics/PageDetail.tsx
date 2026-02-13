import {
  getPagePathDetailOptions,
  getPagePathVisitorsOptions,
} from '@/api/client/@tanstack/react-query.gen'
import {
  PageVisitorSession,
  ProjectResponse,
} from '@/api/client/types.gen'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from '@/components/ui/card'
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from '@/components/ui/table'
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger,
} from '@/components/ui/tooltip'
import { Skeleton } from '@/components/ui/skeleton'
import { useQuery } from '@tanstack/react-query'
import { format } from 'date-fns'
import {
  ArrowLeft,
  BarChart3,
  Clock,
  DoorOpen,
  DoorClosed,
  Globe,
  Loader2,
  LogIn,
  LogOut,
  Monitor,
  MousePointerClick,
  Users,
} from 'lucide-react'
import { useState } from 'react'
import { useNavigate } from 'react-router-dom'
import { TimeAgo } from '../utils/TimeAgo'

interface PageDetailProps {
  project: ProjectResponse
  pagePath: string
  startDate: Date | undefined
  endDate: Date | undefined
  environment: number | undefined
  onBack: () => void
}

function formatDuration(seconds: number): string {
  if (seconds < 60) return `${seconds}s`
  const mins = Math.floor(seconds / 60)
  const secs = seconds % 60
  if (mins < 60) return secs > 0 ? `${mins}m ${secs}s` : `${mins}m`
  const hrs = Math.floor(mins / 60)
  const remainMins = mins % 60
  return remainMins > 0 ? `${hrs}h ${remainMins}m` : `${hrs}h`
}

function formatSessionInfo(session: PageVisitorSession): string {
  const parts: string[] = []
  if (session.browser) parts.push(session.browser)
  if (session.operating_system) parts.push(session.operating_system)
  return parts.join(' / ') || '-'
}

function formatLocation(session: PageVisitorSession): string | null {
  const parts: string[] = []
  if (session.city) parts.push(session.city)
  if (session.country) parts.push(session.country)
  return parts.length > 0 ? parts.join(', ') : null
}

export function PageDetail({
  project,
  pagePath,
  startDate,
  endDate,
  environment,
  onBack,
}: PageDetailProps) {
  const navigate = useNavigate()
  const [currentPage, setCurrentPage] = useState(1)
  const perPage = 50

  // Fetch page detail analytics
  const { data: detailData, isLoading: detailLoading } = useQuery({
    ...getPagePathDetailOptions({
      query: {
        page_path: pagePath,
        project_id: project.id,
        start_date: startDate ? startDate.toISOString() : '',
        end_date: endDate ? endDate.toISOString() : '',
        environment_id: environment,
      },
    }),
    enabled: !!startDate && !!endDate,
  })

  // Fetch visitor sessions for this page
  const { data: visitorsData, isLoading: visitorsLoading } = useQuery({
    ...getPagePathVisitorsOptions({
      query: {
        page_path: pagePath,
        project_id: project.id,
        start_date: startDate ? startDate.toISOString() : '',
        end_date: endDate ? endDate.toISOString() : '',
        environment_id: environment,
        page: currentPage,
        per_page: perPage,
      },
    }),
    enabled: !!startDate && !!endDate,
  })

  const totalPages = visitorsData
    ? Math.ceil(visitorsData.total_count / perPage)
    : 0

  return (
    <div className="space-y-6">
      {/* Back button and page path header */}
      <div className="flex items-center gap-3">
        <Button variant="ghost" size="sm" onClick={onBack} className="gap-2">
          <ArrowLeft className="h-4 w-4" />
          Back to Pages
        </Button>
      </div>

      {/* Page path title */}
      <div>
        <h2 className="text-2xl font-bold font-mono">{pagePath}</h2>
        <p className="text-sm text-muted-foreground mt-1">
          {startDate && endDate
            ? `${format(startDate, 'LLL dd, y')} - ${format(endDate, 'LLL dd, y')}`
            : 'Page analytics'}
        </p>
      </div>

      {/* Summary stats */}
      {detailLoading ? (
        <div className="grid grid-cols-2 md:grid-cols-3 lg:grid-cols-6 gap-4">
          {[...Array(6)].map((_, i) => (
            <Card key={`stat-skeleton-${i}`}>
              <CardContent className="pt-4 pb-4">
                <Skeleton className="h-4 w-20 mb-2" />
                <Skeleton className="h-6 w-12" />
              </CardContent>
            </Card>
          ))}
        </div>
      ) : detailData ? (
        <div className="grid grid-cols-2 md:grid-cols-3 lg:grid-cols-6 gap-4">
          <StatCard
            label="Unique Visitors"
            value={detailData.unique_visitors.toLocaleString()}
            icon={<Users className="h-4 w-4 text-muted-foreground" />}
          />
          <StatCard
            label="Total Page Views"
            value={detailData.total_page_views.toLocaleString()}
            icon={<BarChart3 className="h-4 w-4 text-muted-foreground" />}
          />
          <StatCard
            label="Avg. Time on Page"
            value={formatDuration(Math.round(detailData.avg_time_on_page))}
            icon={<Clock className="h-4 w-4 text-muted-foreground" />}
          />
          <StatCard
            label="Bounce Rate"
            value={`${detailData.bounce_rate.toFixed(1)}%`}
            icon={
              <MousePointerClick className="h-4 w-4 text-muted-foreground" />
            }
          />
          <StatCard
            label="Entry Rate"
            value={`${detailData.entry_rate.toFixed(1)}%`}
            icon={<LogIn className="h-4 w-4 text-muted-foreground" />}
          />
          <StatCard
            label="Exit Rate"
            value={`${detailData.exit_rate.toFixed(1)}%`}
            icon={<LogOut className="h-4 w-4 text-muted-foreground" />}
          />
        </div>
      ) : null}

      {/* Top referrers and countries side by side */}
      {detailData && (
        <div className="grid grid-cols-1 md:grid-cols-2 gap-6">
          {/* Top Referrers */}
          {detailData.referrers.length > 0 && (
            <Card>
              <CardHeader className="pb-3">
                <CardTitle className="text-sm font-medium">
                  Top Referrers
                </CardTitle>
              </CardHeader>
              <CardContent className="pt-0">
                <div className="space-y-2">
                  {detailData.referrers.slice(0, 5).map((ref) => (
                    <div
                      key={ref.referrer}
                      className="flex items-center justify-between text-sm"
                    >
                      <span className="truncate text-muted-foreground">
                        {ref.referrer || '(direct)'}
                      </span>
                      <div className="flex items-center gap-2">
                        <span className="font-medium">{ref.visits}</span>
                        <Badge variant="outline" className="text-xs">
                          {ref.percentage.toFixed(1)}%
                        </Badge>
                      </div>
                    </div>
                  ))}
                </div>
              </CardContent>
            </Card>
          )}

          {/* Top Countries */}
          {detailData.countries.length > 0 && (
            <Card>
              <CardHeader className="pb-3">
                <CardTitle className="text-sm font-medium">
                  Top Countries
                </CardTitle>
              </CardHeader>
              <CardContent className="pt-0">
                <div className="space-y-2">
                  {detailData.countries.slice(0, 5).map((country) => (
                    <div
                      key={country.country}
                      className="flex items-center justify-between text-sm"
                    >
                      <span className="truncate text-muted-foreground">
                        {country.country}
                      </span>
                      <div className="flex items-center gap-2">
                        <span className="font-medium">
                          {country.visitors} visitors
                        </span>
                        <Badge variant="outline" className="text-xs">
                          {country.percentage.toFixed(1)}%
                        </Badge>
                      </div>
                    </div>
                  ))}
                </div>
              </CardContent>
            </Card>
          )}
        </div>
      )}

      {/* Visitor Sessions Table */}
      <Card>
        <CardHeader>
          <div className="flex items-center justify-between">
            <div>
              <CardTitle className="text-base">Visitor Sessions</CardTitle>
              <CardDescription>
                Individual visits to this page
                {visitorsData && (
                  <span className="ml-1">
                    ({visitorsData.total_count.toLocaleString()} total)
                  </span>
                )}
              </CardDescription>
            </div>
            {visitorsLoading && (
              <div className="flex items-center gap-2 text-sm text-muted-foreground">
                <Loader2 className="h-4 w-4 animate-spin" />
                Loading...
              </div>
            )}
          </div>
        </CardHeader>
        <CardContent className="p-0">
          {visitorsLoading && !visitorsData ? (
            <div className="p-8 space-y-4">
              {[...Array(5)].map((_, i) => (
                <div
                  key={`visitor-skeleton-${i}`}
                  className="flex items-center gap-4"
                >
                  <Skeleton className="h-4 w-16" />
                  <Skeleton className="h-4 w-32" />
                  <Skeleton className="h-4 w-20" />
                  <Skeleton className="h-4 w-24" />
                </div>
              ))}
            </div>
          ) : !visitorsData?.sessions ||
            visitorsData.sessions.length === 0 ? (
            <div className="p-8 text-center">
              <p className="text-sm text-muted-foreground">
                No visitor sessions found for this page in the selected date
                range
              </p>
            </div>
          ) : (
            <>
              <Table>
                <TableHeader>
                  <TableRow>
                    <TableHead>Visitor</TableHead>
                    <TableHead>Viewed</TableHead>
                    <TableHead>Time on Page</TableHead>
                    <TableHead>Browser / OS</TableHead>
                    <TableHead>Location</TableHead>
                    <TableHead>Flow</TableHead>
                  </TableRow>
                </TableHeader>
                <TableBody>
                  {visitorsData.sessions.map((session, idx) => (
                    <TableRow
                      key={`${session.visitor_id}-${session.viewed_at}-${idx}`}
                      className="cursor-pointer hover:bg-muted/50"
                      onClick={() =>
                        navigate(
                          `/projects/${project.slug}/analytics/visitors/${session.visitor_id}`
                        )
                      }
                    >
                      <TableCell>
                        <div className="flex items-center gap-1.5">
                          <Users className="h-3 w-3 text-muted-foreground shrink-0" />
                          <span className="text-sm font-medium">
                            {session.visitor_id}
                          </span>
                        </div>
                      </TableCell>
                      <TableCell>
                        <span className="text-sm text-muted-foreground">
                          <TimeAgo date={session.viewed_at} />
                        </span>
                      </TableCell>
                      <TableCell>
                        <div className="flex items-center gap-1">
                          <Clock className="h-3 w-3 text-muted-foreground" />
                          <span className="text-sm">
                            {session.time_on_page != null
                              ? formatDuration(session.time_on_page)
                              : '-'}
                          </span>
                        </div>
                      </TableCell>
                      <TableCell>
                        <span className="text-sm text-muted-foreground">
                          {formatSessionInfo(session)}
                        </span>
                      </TableCell>
                      <TableCell>
                        {formatLocation(session) ? (
                          <Tooltip>
                            <TooltipTrigger asChild>
                              <div className="flex items-center gap-1.5">
                                <Globe className="h-3 w-3 text-muted-foreground shrink-0" />
                                <span className="text-sm">
                                  {formatLocation(session)}
                                </span>
                              </div>
                            </TooltipTrigger>
                            <TooltipContent>
                              <div className="text-xs">
                                {session.city && <div>City: {session.city}</div>}
                                {session.country && (
                                  <div>Country: {session.country}</div>
                                )}
                              </div>
                            </TooltipContent>
                          </Tooltip>
                        ) : (
                          <span className="text-xs text-muted-foreground">
                            -
                          </span>
                        )}
                      </TableCell>
                      <TableCell>
                        <div className="flex items-center gap-1.5">
                          {session.is_entry && (
                            <Tooltip>
                              <TooltipTrigger>
                                <Badge
                                  variant="outline"
                                  className="text-xs px-1.5 py-0"
                                >
                                  <DoorOpen className="h-3 w-3 mr-0.5" />
                                  Entry
                                </Badge>
                              </TooltipTrigger>
                              <TooltipContent>
                                Session started on this page
                              </TooltipContent>
                            </Tooltip>
                          )}
                          {session.is_exit && (
                            <Tooltip>
                              <TooltipTrigger>
                                <Badge
                                  variant="outline"
                                  className="text-xs px-1.5 py-0"
                                >
                                  <DoorClosed className="h-3 w-3 mr-0.5" />
                                  Exit
                                </Badge>
                              </TooltipTrigger>
                              <TooltipContent>
                                Session ended on this page
                              </TooltipContent>
                            </Tooltip>
                          )}
                          {session.is_bounce && (
                            <Badge
                              variant="destructive"
                              className="text-xs px-1.5 py-0"
                            >
                              Bounce
                            </Badge>
                          )}
                          {session.session_page_number != null && (
                            <Tooltip>
                              <TooltipTrigger>
                                <Badge
                                  variant="secondary"
                                  className="text-xs px-1.5 py-0"
                                >
                                  <Monitor className="h-3 w-3 mr-0.5" />
                                  Page #{session.session_page_number}
                                </Badge>
                              </TooltipTrigger>
                              <TooltipContent>
                                This was page #{session.session_page_number} in
                                the visitor's session
                              </TooltipContent>
                            </Tooltip>
                          )}
                        </div>
                      </TableCell>
                    </TableRow>
                  ))}
                </TableBody>
              </Table>

              {/* Pagination */}
              {totalPages > 1 && (
                <div className="flex items-center justify-between px-4 py-3 border-t">
                  <p className="text-sm text-muted-foreground">
                    Page {currentPage} of {totalPages}
                  </p>
                  <div className="flex items-center gap-2">
                    <Button
                      variant="outline"
                      size="sm"
                      disabled={currentPage <= 1}
                      onClick={() =>
                        setCurrentPage((p) => Math.max(1, p - 1))
                      }
                    >
                      Previous
                    </Button>
                    <Button
                      variant="outline"
                      size="sm"
                      disabled={currentPage >= totalPages}
                      onClick={() =>
                        setCurrentPage((p) => Math.min(totalPages, p + 1))
                      }
                    >
                      Next
                    </Button>
                  </div>
                </div>
              )}
            </>
          )}
        </CardContent>
      </Card>
    </div>
  )
}

interface StatCardProps {
  label: string
  value: string
  icon: React.ReactNode
}

function StatCard({ label, value, icon }: StatCardProps) {
  return (
    <Card>
      <CardContent className="pt-4 pb-4">
        <div className="flex items-center gap-2 mb-1">
          {icon}
          <span className="text-xs text-muted-foreground">{label}</span>
        </div>
        <p className="text-lg font-semibold">{value}</p>
      </CardContent>
    </Card>
  )
}
