import { ProxyLogsDataTable } from '@/components/proxy-logs/ProxyLogsDataTable'

export default function ProxyLogs() {
  return (
    <div className="space-y-6">
      <div>
        <h3 className="text-lg font-medium">Proxy Logs</h3>
        <p className="text-sm text-muted-foreground">
          Advanced proxy request logs with comprehensive filtering across all
          projects
        </p>
      </div>
      <ProxyLogsDataTable />
    </div>
  )
}
