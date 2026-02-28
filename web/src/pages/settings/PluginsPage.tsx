import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from '@/components/ui/card'
import { useBreadcrumbs } from '@/contexts/BreadcrumbContext'
import { usePageTitle } from '@/hooks/usePageTitle'
import { usePlugins, useReloadPlugins } from '@/hooks/usePlugins'
import { AlertCircle, Loader2, Puzzle, RefreshCw } from 'lucide-react'
import { useEffect } from 'react'
import { toast } from 'sonner'
import { Alert, AlertDescription, AlertTitle } from '@/components/ui/alert'

export function PluginsPage() {
  const { setBreadcrumbs } = useBreadcrumbs()
  const { data: plugins = [], isLoading, error } = usePlugins()
  const reloadPlugins = useReloadPlugins()

  useEffect(() => {
    setBreadcrumbs([
      { label: 'Settings', href: '/settings' },
      { label: 'Plugins' },
    ])
  }, [setBreadcrumbs])

  usePageTitle('Plugins')

  const handleReload = async () => {
    try {
      const result = await reloadPlugins.mutateAsync()
      toast.success(result.message)
    } catch {
      toast.error('Failed to reload plugins')
    }
  }

  if (isLoading) {
    return (
      <div className="flex items-center justify-center min-h-[400px]">
        <Loader2 className="h-8 w-8 animate-spin" />
      </div>
    )
  }

  if (error) {
    return (
      <Alert variant="destructive">
        <AlertCircle className="h-4 w-4" />
        <AlertTitle>Error</AlertTitle>
        <AlertDescription>Failed to load plugins.</AlertDescription>
      </Alert>
    )
  }

  return (
    <div className="space-y-6">
      <Card>
        <CardHeader>
          <div className="flex items-center justify-between">
            <div>
              <CardTitle>External Plugins</CardTitle>
              <CardDescription>
                Manage external plugin binaries. Plugins are discovered from the
                plugins directory on startup or reload.
              </CardDescription>
            </div>
            <Button
              variant="outline"
              onClick={handleReload}
              disabled={reloadPlugins.isPending}
            >
              {reloadPlugins.isPending ? (
                <Loader2 className="mr-2 h-4 w-4 animate-spin" />
              ) : (
                <RefreshCw className="mr-2 h-4 w-4" />
              )}
              <span className="hidden sm:inline">Reload Plugins</span>
              <span className="sm:hidden">Reload</span>
            </Button>
          </div>
        </CardHeader>
        <CardContent>
          {plugins.length === 0 ? (
            <div className="flex flex-col items-center justify-center py-12 text-center">
              <Puzzle className="h-12 w-12 text-muted-foreground mb-4" />
              <p className="text-sm font-medium">No plugins installed</p>
              <p className="text-sm text-muted-foreground mt-1">
                Place plugin binaries in the plugins directory and click Reload.
              </p>
            </div>
          ) : (
            <div className="space-y-3">
              {plugins.map((plugin) => (
                <div
                  key={plugin.name}
                  className="flex items-center justify-between rounded-lg border p-4"
                >
                  <div className="flex items-center gap-3 min-w-0">
                    <div className="flex h-9 w-9 shrink-0 items-center justify-center rounded-md bg-muted">
                      <Puzzle className="h-4 w-4" />
                    </div>
                    <div className="min-w-0">
                      <div className="flex items-center gap-2">
                        <p className="text-sm font-medium truncate">
                          {plugin.display_name || plugin.name}
                        </p>
                        <Badge variant="secondary" className="text-xs shrink-0">
                          v{plugin.version}
                        </Badge>
                      </div>
                      {plugin.description && (
                        <p className="text-xs text-muted-foreground truncate mt-0.5">
                          {plugin.description}
                        </p>
                      )}
                    </div>
                  </div>
                  <div className="flex items-center gap-2 shrink-0 ml-4">
                    {plugin.ui && (
                      <Badge variant="outline" className="text-xs">
                        UI
                      </Badge>
                    )}
                    {plugin.requires_db && (
                      <Badge variant="outline" className="text-xs">
                        DB
                      </Badge>
                    )}
                    <Badge
                      variant="default"
                      className="bg-green-500/15 text-green-700 dark:text-green-400 border-green-500/20 text-xs"
                    >
                      Running
                    </Badge>
                  </div>
                </div>
              ))}
            </div>
          )}
        </CardContent>
      </Card>
    </div>
  )
}
