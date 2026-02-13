import { useEffect, useRef, useMemo, useCallback, useState } from 'react'
import createGlobe, { type COBEOptions } from 'cobe'
import { useQuery } from '@tanstack/react-query'
import {
  getVisitorsOptions,
  getLiveVisitorsListOptions,
} from '@/api/client/@tanstack/react-query.gen'
import { ProjectResponse, VisitorInfo } from '@/api/client/types.gen'
import { Avatar, AvatarFallback } from '@/components/ui/avatar'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import {
  Tooltip,
  TooltipContent,
  TooltipProvider,
  TooltipTrigger,
} from '@/components/ui/tooltip'
import {
  Users,
  ArrowLeft,
  Globe as GlobeIcon,
  ExternalLink,
} from 'lucide-react'
import { useNavigate } from 'react-router-dom'

interface VisitorGlobePageProps {
  project: ProjectResponse
}

interface GlobeMarker {
  location: [number, number]
  size: number
  isLive: boolean
  visitor?: VisitorInfo
}

// ─── Avatar helpers ────────────────────────────────────────────────

function hashString(str: string): number {
  let hash = 0
  for (let i = 0; i < str.length; i++) {
    const char = str.charCodeAt(i)
    hash = (hash << 5) - hash + char
    hash |= 0
  }
  return Math.abs(hash)
}

const AVATAR_COLORS = [
  { bg: 'bg-red-500/20', text: 'text-red-400', hex: '#f87171' },
  { bg: 'bg-orange-500/20', text: 'text-orange-400', hex: '#fb923c' },
  { bg: 'bg-amber-500/20', text: 'text-amber-400', hex: '#fbbf24' },
  { bg: 'bg-yellow-500/20', text: 'text-yellow-400', hex: '#facc15' },
  { bg: 'bg-lime-500/20', text: 'text-lime-400', hex: '#a3e635' },
  { bg: 'bg-green-500/20', text: 'text-green-400', hex: '#4ade80' },
  { bg: 'bg-emerald-500/20', text: 'text-emerald-400', hex: '#34d399' },
  { bg: 'bg-teal-500/20', text: 'text-teal-400', hex: '#2dd4bf' },
  { bg: 'bg-cyan-500/20', text: 'text-cyan-400', hex: '#22d3ee' },
  { bg: 'bg-sky-500/20', text: 'text-sky-400', hex: '#38bdf8' },
  { bg: 'bg-blue-500/20', text: 'text-blue-400', hex: '#60a5fa' },
  { bg: 'bg-indigo-500/20', text: 'text-indigo-400', hex: '#818cf8' },
  { bg: 'bg-violet-500/20', text: 'text-violet-400', hex: '#a78bfa' },
  { bg: 'bg-purple-500/20', text: 'text-purple-400', hex: '#c084fc' },
  { bg: 'bg-fuchsia-500/20', text: 'text-fuchsia-400', hex: '#e879f9' },
  { bg: 'bg-pink-500/20', text: 'text-pink-400', hex: '#f472b6' },
  { bg: 'bg-rose-500/20', text: 'text-rose-400', hex: '#fb7185' },
]

function getVisitorAvatar(visitor: VisitorInfo) {
  const hash = hashString(visitor.visitor_id)
  const color = AVATAR_COLORS[hash % AVATAR_COLORS.length]

  let initials = ''
  if (visitor.city && visitor.country) {
    initials = (visitor.city[0] + visitor.country[0]).toUpperCase()
  } else if (visitor.city) {
    initials = visitor.city.substring(0, 2).toUpperCase()
  } else if (visitor.country) {
    initials = visitor.country.substring(0, 2).toUpperCase()
  } else {
    initials = visitor.visitor_id.slice(-2).toUpperCase()
  }

  return { initials, colorClass: color.bg, textClass: color.text, hex: color.hex }
}

function countryCodeToFlag(countryCode: string | null | undefined): string {
  if (!countryCode || countryCode.length !== 2) return ''
  const codePoints = countryCode
    .toUpperCase()
    .split('')
    .map((char) => 127397 + char.charCodeAt(0))
  return String.fromCodePoint(...codePoints)
}

function getTimeAgo(date: Date): string {
  const now = new Date()
  const diffMs = now.getTime() - date.getTime()
  const diffMins = Math.floor(diffMs / 60000)
  if (diffMins < 1) return 'just now'
  if (diffMins < 60) return `${diffMins}m ago`
  const diffHours = Math.floor(diffMins / 60)
  if (diffHours < 24) return `${diffHours}h ago`
  const diffDays = Math.floor(diffHours / 24)
  return `${diffDays}d ago`
}

function getBrowserName(userAgent: string | null | undefined): string | null {
  if (!userAgent) return null
  if (userAgent.includes('Edg')) return 'Edge'
  if (userAgent.includes('Chrome') && !userAgent.includes('Chromium'))
    return 'Chrome'
  if (userAgent.includes('Safari') && !userAgent.includes('Chrome'))
    return 'Safari'
  if (userAgent.includes('Firefox')) return 'Firefox'
  if (userAgent.includes('Opera') || userAgent.includes('OPR')) return 'Opera'
  if (userAgent.includes('bot') || userAgent.includes('Bot')) return 'Bot'
  return null
}

function getOSName(userAgent: string | null | undefined): string | null {
  if (!userAgent) return null
  if (userAgent.includes('Mac OS')) return 'macOS'
  if (userAgent.includes('Windows')) return 'Windows'
  if (userAgent.includes('Linux')) return 'Linux'
  if (userAgent.includes('Android')) return 'Android'
  if (userAgent.includes('iPhone') || userAgent.includes('iPad')) return 'iOS'
  return null
}

// ─── 3D projection: lat/lng → screen x,y ────────────────────────

interface ProjectedMarker {
  x: number
  y: number
  z: number // positive = front-facing
  visitor: VisitorInfo
}

/**
 * Project lat/lng to 2D screen coordinates matching cobe's rendering.
 *
 * Cobe convention:
 * - phi: rotation around Y axis (auto-rotate increments this)
 * - theta: tilt around X axis
 * - Marker locations: [latitude, longitude] in degrees
 *
 * We convert to cobe's internal 3D coords, then apply the same rotations,
 * and finally orthographic-project to the canvas square [0..size, 0..size].
 */
function projectToScreen(
  lat: number,
  lng: number,
  phi: number,
  theta: number,
  size: number
): { x: number; y: number; z: number } {
  const latRad = (lat * Math.PI) / 180
  const lngRad = (lng * Math.PI) / 180

  // Spherical to cartesian (cobe convention: Y-up, Z toward camera)
  // A point at lat=0, lng=0 faces the camera when phi=0, theta=0
  const px = -Math.cos(latRad) * Math.sin(lngRad)
  const py = Math.sin(latRad)
  const pz = Math.cos(latRad) * Math.cos(lngRad)

  // Rotate by phi around Y axis (cobe rotates the globe, so negate phi for the marker)
  const cosPhi = Math.cos(phi)
  const sinPhi = Math.sin(phi)
  const rx = px * cosPhi + pz * sinPhi
  const ry = py
  const rz = -px * sinPhi + pz * cosPhi

  // Rotate by theta around X axis
  const cosTheta = Math.cos(-theta)
  const sinTheta = Math.sin(-theta)
  const fx = rx
  const fy = ry * cosTheta - rz * sinTheta
  const fz = ry * sinTheta + rz * cosTheta

  // Orthographic projection: x maps to screen-x, y maps to screen-y, z = depth
  const radius = size / 2
  const screenX = size / 2 + fx * radius
  const screenY = size / 2 - fy * radius // flip Y: screen Y grows downward

  return { x: screenX, y: screenY, z: fz }
}

// ─── Sidebar visitor item ────────────────────────────────────────

interface RecentVisitorItemProps {
  visitor: VisitorInfo
  projectSlug: string
}

function RecentVisitorItem({ visitor, projectSlug }: RecentVisitorItemProps) {
  const navigate = useNavigate()
  const flag = countryCodeToFlag(visitor.country_code)
  const location = [visitor.city, visitor.country].filter(Boolean).join(', ')
  const timeAgo = getTimeAgo(new Date(visitor.last_seen))
  const { initials, colorClass, textClass } = getVisitorAvatar(visitor)
  const browser = getBrowserName(visitor.user_agent)
  const os = getOSName(visitor.user_agent)
  const deviceInfo = [browser, os].filter(Boolean).join(' / ')

  return (
    <TooltipProvider>
      <Tooltip>
        <TooltipTrigger asChild>
          <button
            type="button"
            className="flex items-start gap-3 text-sm p-2 -mx-2 rounded-md cursor-pointer hover:bg-accent/50 transition-colors w-full text-left"
            onClick={() =>
              navigate(
                `/projects/${projectSlug}/analytics/visitors/${visitor.id}`
              )
            }
          >
            <Avatar className="h-8 w-8 flex-shrink-0 mt-0.5">
              <AvatarFallback
                className={`${colorClass} ${textClass} text-xs font-semibold`}
              >
                {initials}
              </AvatarFallback>
            </Avatar>
            <div className="min-w-0 flex-1">
              <p className="truncate font-medium text-xs">
                {flag && <span className="mr-1">{flag}</span>}
                {location || 'Unknown location'}
              </p>
              {visitor.current_page && (
                <p className="truncate text-xs text-muted-foreground font-mono mt-0.5">
                  {visitor.current_page}
                </p>
              )}
              <div className="flex items-center gap-2 mt-0.5">
                {deviceInfo && (
                  <span className="text-[10px] text-muted-foreground">
                    {deviceInfo}
                  </span>
                )}
                <span className="text-[10px] text-muted-foreground">
                  {timeAgo}
                </span>
              </div>
            </div>
            <ExternalLink className="h-3 w-3 text-muted-foreground flex-shrink-0 mt-1" />
          </button>
        </TooltipTrigger>
        <TooltipContent side="left">
          <p>View visitor journey</p>
        </TooltipContent>
      </Tooltip>
    </TooltipProvider>
  )
}

// ─── Main page component ─────────────────────────────────────────

export function VisitorGlobePage({ project }: VisitorGlobePageProps) {
  const navigate = useNavigate()
  const canvasRef = useRef<HTMLCanvasElement>(null)
  const containerRef = useRef<HTMLDivElement>(null)
  const pointerInteracting = useRef<number | null>(null)
  const pointerInteractionMovement = useRef(0)
  const phiRef = useRef(0)
  const thetaRef = useRef(0.25)
  const globeRef = useRef<ReturnType<typeof createGlobe> | null>(null)
  const markersRef = useRef<GlobeMarker[]>([])
  const [globeSize, setGlobeSize] = useState(0)
  const [projectedMarkers, setProjectedMarkers] = useState<ProjectedMarker[]>(
    []
  )
  const animFrameRef = useRef<number>(0)

  // Last 30 days
  const dateRange = useMemo(() => {
    const now = new Date()
    const thirtyDaysAgo = new Date(now)
    thirtyDaysAgo.setDate(thirtyDaysAgo.getDate() - 30)
    return { startDate: thirtyDaysAgo, endDate: now }
  }, [])

  const { data: visitorsData } = useQuery({
    ...getVisitorsOptions({
      query: {
        project_id: project.id,
        start_date: dateRange.startDate.toISOString(),
        end_date: dateRange.endDate.toISOString(),
        limit: 200,
        has_activity_only: true,
      },
    }),
  })

  const { data: liveData } = useQuery({
    ...getLiveVisitorsListOptions({
      query: {
        project_id: project.id,
        window_minutes: 30,
      },
    }),
    refetchInterval: 10000,
  })

  // Build cobe markers + keep visitor refs for overlay
  const markers = useMemo(() => {
    const result: GlobeMarker[] = []
    const liveVisitorIds = new Set(
      liveData?.visitors?.map((v) => v.visitor_id) ?? []
    )

    if (visitorsData?.visitors) {
      for (const visitor of visitorsData.visitors) {
        if (visitor.latitude != null && visitor.longitude != null) {
          result.push({
            location: [visitor.latitude, visitor.longitude],
            size: liveVisitorIds.has(visitor.visitor_id) ? 0.1 : 0.05,
            isLive: liveVisitorIds.has(visitor.visitor_id),
            visitor,
          })
        }
      }
    }

    if (liveData?.visitors) {
      for (const liveVisitor of liveData.visitors) {
        if (
          liveVisitor.latitude != null &&
          liveVisitor.longitude != null &&
          !visitorsData?.visitors?.some(
            (v) => v.visitor_id === liveVisitor.visitor_id
          )
        ) {
          // Cast LiveVisitorInfo to VisitorInfo shape (they're structurally identical now)
          const asVisitorInfo = liveVisitor as unknown as VisitorInfo
          result.push({
            location: [liveVisitor.latitude, liveVisitor.longitude],
            size: 0.1,
            isLive: true,
            visitor: asVisitorInfo,
          })
        }
      }
    }

    return result
  }, [visitorsData, liveData])

  useEffect(() => {
    markersRef.current = markers
  }, [markers])

  // Visitors with location for overlay (non-crawlers only, limit to avoid clutter)
  const overlayVisitors = useMemo(() => {
    if (!visitorsData?.visitors) return []
    return visitorsData.visitors
      .filter(
        (v) => v.latitude != null && v.longitude != null && !v.is_crawler
      )
      .sort(
        (a, b) =>
          new Date(b.last_seen).getTime() - new Date(a.last_seen).getTime()
      )
      .slice(0, 20) // cap for performance
  }, [visitorsData])

  // Recent visitors for sidebar
  const recentVisitors = useMemo(() => {
    return overlayVisitors.slice(0, 10)
  }, [overlayVisitors])

  const liveCount = liveData?.visitors?.length ?? 0

  // Compute globe size from container
  useEffect(() => {
    function updateSize() {
      if (containerRef.current) {
        const rect = containerRef.current.getBoundingClientRect()
        const size = Math.min(rect.width, rect.height)
        setGlobeSize(size)
      }
    }
    updateSize()
    window.addEventListener('resize', updateSize)
    return () => window.removeEventListener('resize', updateSize)
  }, [])

  // Animation loop: project markers to screen coords every frame
  useEffect(() => {
    let running = true

    function tick() {
      if (!running || globeSize === 0) return

      const phi = phiRef.current + pointerInteractionMovement.current
      const theta = thetaRef.current

      const projected: ProjectedMarker[] = []
      for (const v of overlayVisitors) {
        if (v.latitude == null || v.longitude == null) continue
        const { x, y, z } = projectToScreen(
          v.latitude,
          v.longitude,
          phi,
          theta,
          globeSize
        )
        projected.push({ x, y, z, visitor: v })
      }

      setProjectedMarkers(projected)
      animFrameRef.current = requestAnimationFrame(tick)
    }

    animFrameRef.current = requestAnimationFrame(tick)
    return () => {
      running = false
      cancelAnimationFrame(animFrameRef.current)
    }
  }, [globeSize, overlayVisitors])

  // Initialize cobe globe
  const initGlobe = useCallback(() => {
    if (!canvasRef.current || globeSize === 0) return

    if (globeRef.current) {
      globeRef.current.destroy()
      globeRef.current = null
    }

    const pixelSize = globeSize * 2

    const options: COBEOptions = {
      devicePixelRatio: 2,
      width: pixelSize,
      height: pixelSize,
      phi: 0,
      theta: 0.25,
      dark: 1,
      diffuse: 1.2,
      mapSamples: 20000,
      mapBrightness: 6,
      baseColor: [0.15, 0.18, 0.22],
      markerColor: [0.34, 0.92, 0.57],
      glowColor: [0.08, 0.1, 0.14],
      markers: markersRef.current.map((m) => ({
        location: m.location,
        size: m.size,
      })),
      onRender: (state) => {
        if (!pointerInteracting.current) {
          phiRef.current += 0.003
        }
        state.phi = phiRef.current + pointerInteractionMovement.current
        thetaRef.current = state.theta

        state.markers = markersRef.current.map((m) => ({
          location: m.location,
          size: m.size,
        }))
      },
    }

    globeRef.current = createGlobe(canvasRef.current, options)
  }, [globeSize])

  useEffect(() => {
    initGlobe()
    return () => {
      if (globeRef.current) {
        globeRef.current.destroy()
        globeRef.current = null
      }
    }
  }, [initGlobe])

  return (
    <div className="space-y-4">
      {/* Header */}
      <div className="flex items-center justify-between">
        <div className="flex items-center gap-3">
          <Button
            variant="ghost"
            size="icon"
            onClick={() =>
              navigate(`/projects/${project.slug}/analytics`)
            }
          >
            <ArrowLeft className="h-4 w-4" />
          </Button>
          <div>
            <h2 className="text-xl font-semibold flex items-center gap-2">
              <GlobeIcon className="h-5 w-5" />
              Visitor Globe
            </h2>
            <p className="text-sm text-muted-foreground">
              {visitorsData?.filtered_count ?? 0} visitors from around the
              world in the last 30 days
            </p>
          </div>
        </div>
        <div className="flex items-center gap-2">
          {liveCount > 0 && (
            <Badge
              variant="outline"
              className="gap-1.5 border-green-500/50 text-green-500"
            >
              <span className="relative flex h-2 w-2">
                <span className="absolute inline-flex h-full w-full animate-ping rounded-full bg-green-400 opacity-75" />
                <span className="relative inline-flex h-2 w-2 rounded-full bg-green-500" />
              </span>
              {liveCount} live
            </Badge>
          )}
          <Badge variant="secondary" className="gap-1">
            <Users className="h-3 w-3" />
            {markers.length} on globe
          </Badge>
        </div>
      </div>

      {/* Globe + Sidebar layout */}
      <div className="flex flex-col lg:flex-row gap-6">
        {/* Globe container */}
        <div
          ref={containerRef}
          className="flex-1 flex items-center justify-center rounded-lg border bg-card overflow-hidden"
          style={{ minHeight: 500 }}
        >
          {globeSize > 0 && (
            <div
              className="relative"
              style={{ width: globeSize, height: globeSize }}
            >
              <canvas
                ref={canvasRef}
                style={{ width: globeSize, height: globeSize }}
                className="cursor-grab active:cursor-grabbing"
                onPointerDown={(e) => {
                  pointerInteracting.current =
                    e.clientX - pointerInteractionMovement.current
                  if (canvasRef.current)
                    canvasRef.current.style.cursor = 'grabbing'
                }}
                onPointerUp={() => {
                  pointerInteracting.current = null
                  if (canvasRef.current)
                    canvasRef.current.style.cursor = 'grab'
                }}
                onPointerOut={() => {
                  pointerInteracting.current = null
                  if (canvasRef.current)
                    canvasRef.current.style.cursor = 'grab'
                }}
                onMouseMove={(e) => {
                  if (pointerInteracting.current !== null) {
                    const delta = e.clientX - pointerInteracting.current
                    pointerInteractionMovement.current = delta / 100
                  }
                }}
                onTouchMove={(e) => {
                  if (pointerInteracting.current !== null && e.touches[0]) {
                    const delta =
                      e.touches[0].clientX - pointerInteracting.current
                    pointerInteractionMovement.current = delta / 100
                  }
                }}
              />

              {/* Avatar overlays — same size as canvas, positioned on top */}
              <GlobeAvatarOverlays
                projectedMarkers={projectedMarkers}
                globeSize={globeSize}
                projectSlug={project.slug}
              />
            </div>
          )}
        </div>

        {/* Sidebar — recent visitors */}
        <div className="lg:w-[320px] rounded-lg border bg-card p-5 space-y-4 overflow-hidden">
          <p className="text-xs font-medium text-muted-foreground uppercase tracking-wider">
            Recent visitors
          </p>
          {recentVisitors.length === 0 ? (
            <p className="text-sm text-muted-foreground">
              No visitor locations available
            </p>
          ) : (
            <div className="space-y-1">
              {recentVisitors.map((visitor) => (
                <RecentVisitorItem
                  key={visitor.id}
                  visitor={visitor}
                  projectSlug={project.slug}
                />
              ))}
            </div>
          )}
        </div>
      </div>
    </div>
  )
}

// ─── Globe avatar overlays ───────────────────────────────────────

interface GlobeAvatarOverlaysProps {
  projectedMarkers: ProjectedMarker[]
  globeSize: number
  projectSlug: string
}

function GlobeAvatarOverlays({
  projectedMarkers,
  projectSlug,
}: GlobeAvatarOverlaysProps) {
  const navigate = useNavigate()

  return (
    <div className="absolute inset-0 pointer-events-none overflow-hidden">
      {projectedMarkers.map((pm) => {
        // Hide markers on back side of globe
        if (pm.z < 0.05) return null

        const { initials, hex } = getVisitorAvatar(pm.visitor)
        const avatarSize = 28
        const x = pm.x - avatarSize / 2
        const y = pm.y - avatarSize / 2

        // Fade based on how front-facing the point is
        const opacity = Math.min(1, pm.z * 2)

        return (
          <button
            type="button"
            key={pm.visitor.id}
            className="absolute pointer-events-auto cursor-pointer transition-opacity duration-150"
            style={{
              left: x,
              top: y,
              width: avatarSize,
              height: avatarSize,
              opacity,
              zIndex: Math.round(pm.z * 100),
              padding: 0,
              border: 'none',
              background: 'none',
            }}
            title={`${pm.visitor.city ?? ''}, ${pm.visitor.country ?? ''}\n${pm.visitor.current_page ?? ''}`}
            onClick={() =>
              navigate(
                `/projects/${projectSlug}/analytics/visitors/${pm.visitor.id}`
              )
            }
          >
            <div
              className="w-full h-full rounded-full flex items-center justify-center text-[9px] font-bold border border-white/20 shadow-lg"
              style={{
                backgroundColor: `${hex}30`,
                color: hex,
                boxShadow: `0 0 8px ${hex}40`,
              }}
            >
              {initials}
            </div>
          </button>
        )
      })}
    </div>
  )
}
