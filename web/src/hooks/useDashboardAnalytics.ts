import { client } from '@/api/client/client.gen'
import { useQuery } from '@tanstack/react-query'

export interface ProjectDashboardAnalytics {
  project_id: number
  unique_visitors: number
  previous_unique_visitors: number
  trend_percentage: number | null
  hourly_visits: Array<{ date: string; count: number }>
}

export interface DashboardProjectsAnalyticsResponse {
  projects: Record<string, ProjectDashboardAnalytics>
}

async function fetchDashboardAnalytics(
  projectIds: number[],
  startDate: string,
  endDate: string
): Promise<DashboardProjectsAnalyticsResponse> {
  const response = await client.get({
    url: '/dashboard/projects-analytics',
    query: {
      project_ids: projectIds.join(','),
      start_date: startDate,
      end_date: endDate,
    },
    security: [{ scheme: 'bearer', type: 'http' }],
  })
  return response.data as DashboardProjectsAnalyticsResponse
}

export function useDashboardAnalytics(
  projectIds: number[],
  startDate: string,
  endDate: string
) {
  return useQuery({
    queryKey: ['dashboard-projects-analytics', projectIds, startDate, endDate],
    queryFn: () => fetchDashboardAnalytics(projectIds, startDate, endDate),
    enabled: projectIds.length > 0,
    staleTime: 1000 * 60 * 5, // 5 minutes
    refetchInterval: 1000 * 60, // Refetch every minute
  })
}
