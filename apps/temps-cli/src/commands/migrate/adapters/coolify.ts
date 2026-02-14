/**
 * Coolify platform adapter.
 *
 * Coolify REST API:
 *   Base URL: http://<host>:8000/api/v1
 *   Auth: Bearer token
 *
 * Key endpoints used:
 *   GET /applications              — list all applications
 *   GET /applications/:uuid        — application detail
 *   GET /applications/:uuid/envs   — environment variables
 *   GET /databases                 — list all databases
 *   GET /services                  — list all managed services
 */

import type {
  PlatformCredentials,
  DiscoveredProject,
  ProjectSnapshot,
  MigrationPlan,
  EnvVar,
  ServiceSnapshot,
  DomainSnapshot,
  GitInfo,
  BuildInfo,
  EnvVarPlan,
  ServicePlan,
  DomainPlan,
  UnsupportedFeature,
} from '../types.js'
import type { PlatformAdapter, CredentialValidation } from './base.js'
import {
  isLikelySecret,
  detectServiceTypeFromUrl,
  inferPreset,
  buildSteps,
  buildSummary,
} from './base.js'

// ---------------------------------------------------------------------------
// Coolify API response types
// ---------------------------------------------------------------------------

interface CoolifyApplication {
  uuid: string
  name: string
  description?: string
  fqdn?: string | null
  git_repository?: string | null
  git_branch?: string | null
  git_commit_sha?: string | null
  build_pack?: string | null
  base_directory?: string | null
  publish_directory?: string | null
  docker_compose_location?: string | null
  dockerfile?: string | null
  dockerfile_location?: string | null
  install_command?: string | null
  build_command?: string | null
  start_command?: string | null
  ports_exposes?: string | null
  status?: string
  created_at?: string
  updated_at?: string
  environment?: {
    id: number
    name: string
    project?: {
      id: number
      name: string
      uuid: string
    }
  }
}

interface CoolifyEnvVar {
  id: number
  key: string
  value: string
  is_build_time?: boolean
  is_preview?: boolean
  is_shared?: boolean
  application_id?: number
  uuid?: string
}

interface CoolifyDatabase {
  uuid: string
  name: string
  description?: string
  type?: string
  image?: string
  status?: string
  public_port?: number | null
  created_at?: string
  updated_at?: string
  environment?: {
    id: number
    name: string
    project?: {
      id: number
      name: string
      uuid: string
    }
  }
}

interface CoolifyVersion {
  version?: string
}

// ---------------------------------------------------------------------------
// HTTP helpers
// ---------------------------------------------------------------------------

function normalizeBaseUrl(baseUrl: string): string {
  let url = baseUrl.replace(/\/+$/, '')
  if (!url.includes('/api')) {
    url += '/api/v1'
  }
  return url
}

async function coolifyFetch<T>(
  path: string,
  creds: PlatformCredentials
): Promise<T> {
  const base = normalizeBaseUrl(creds.baseUrl ?? 'http://localhost:8000')
  const url = `${base}${path}`

  const res = await fetch(url, {
    headers: {
      Authorization: `Bearer ${creds.token}`,
      'Content-Type': 'application/json',
      Accept: 'application/json',
    },
  })

  if (!res.ok) {
    const body = await res.text()
    throw new Error(`Coolify API ${res.status}: ${body}`)
  }

  return res.json() as Promise<T>
}

// ---------------------------------------------------------------------------
// Adapter implementation
// ---------------------------------------------------------------------------

export class CoolifyAdapter implements PlatformAdapter {
  readonly platformId = 'coolify' as const

  async validateCredentials(creds: PlatformCredentials): Promise<CredentialValidation> {
    try {
      // Coolify's version endpoint is the lightest way to validate connectivity
      const version = await coolifyFetch<CoolifyVersion>('/version', creds)
      // Also try to list applications to validate the token has permissions
      await coolifyFetch<CoolifyApplication[]>('/applications', creds)

      return {
        valid: true,
        message: `Connected to Coolify ${version.version ?? '(unknown version)'} at ${creds.baseUrl}`,
        accountName: creds.baseUrl,
      }
    } catch (err) {
      return {
        valid: false,
        message: `Authentication failed: ${err instanceof Error ? err.message : String(err)}`,
      }
    }
  }

  async discoverProjects(creds: PlatformCredentials): Promise<DiscoveredProject[]> {
    const apps = await coolifyFetch<CoolifyApplication[]>('/applications', creds)

    return apps.map((app) => ({
      id: app.uuid,
      name: app.name,
      framework: app.build_pack ?? undefined,
      updatedAt: app.updated_at,
      gitUrl: app.git_repository ?? undefined,
      metadata: {
        ...(app.build_pack ? { buildPack: app.build_pack } : {}),
        ...(app.status ? { status: app.status } : {}),
        ...(app.environment?.project?.name ? { project: app.environment.project.name } : {}),
        ...(app.git_branch ? { branch: app.git_branch } : {}),
      },
    }))
  }

  async snapshotProject(creds: PlatformCredentials, projectId: string): Promise<ProjectSnapshot> {
    // Fetch application details, env vars, and databases in parallel
    const [app, envVarsRaw, databases] = await Promise.all([
      coolifyFetch<CoolifyApplication>(`/applications/${projectId}`, creds),
      coolifyFetch<CoolifyEnvVar[]>(`/applications/${projectId}/envs`, creds).catch(() => []),
      coolifyFetch<CoolifyDatabase[]>('/databases', creds).catch(() => []),
    ])

    const git = this.extractGitInfo(app)
    const envVars = this.extractEnvVars(envVarsRaw)
    const domains = this.extractDomains(app)
    const build = this.extractBuildInfo(app)

    // Match databases that belong to the same Coolify project
    const projectUuid = app.environment?.project?.uuid
    const relatedDatabases = projectUuid
      ? databases.filter((db) => db.environment?.project?.uuid === projectUuid)
      : []

    const services = this.extractServices(relatedDatabases, envVars)

    return {
      id: app.uuid,
      name: app.name,
      framework: app.build_pack ?? undefined,
      git,
      envVars,
      services,
      domains,
      build,
      sourceMetadata: app,
    }
  }

  generatePlan(snapshot: ProjectSnapshot): MigrationPlan {
    const preset = inferPreset(snapshot.framework, snapshot.build?.type)

    const project = {
      name: snapshot.name,
      preset,
      directory: snapshot.build?.dockerfilePath ?? '.',
      mainBranch: snapshot.git?.defaultBranch ?? 'main',
      git: snapshot.git,
      buildCommand: snapshot.build?.buildCommand,
      installCommand: snapshot.build?.installCommand,
      outputDir: snapshot.build?.outputDirectory,
    }

    const serviceEnvKeys = new Set(snapshot.services.flatMap((s) => s.envVarKeys))
    const envVars: EnvVarPlan[] = snapshot.envVars.map((ev) => ({
      key: ev.key,
      value: ev.value,
      isSecret: ev.isSecret,
      source: ev.source,
      skip: serviceEnvKeys.has(ev.key),
    }))

    const services: ServicePlan[] = snapshot.services.map((svc) => this.buildServicePlan(svc))

    const domains: DomainPlan[] = snapshot.domains.map((d) => ({
      domain: d.domain,
      action: 'import' as const,
      actionDescription: d.redirectTo
        ? `Import redirect: ${d.domain} → ${d.redirectTo}`
        : `Import domain ${d.domain}`,
      redirectTo: d.redirectTo,
      statusCode: d.redirectStatusCode,
    }))

    const unsupportedFeatures = this.detectUnsupportedFeatures(snapshot)
    const steps = buildSteps({ project, envVars, services, domains })
    const summary = buildSummary('coolify', { envVars, services, domains }, unsupportedFeatures)

    return {
      platform: 'coolify',
      sourceProjectId: snapshot.id,
      sourceProjectName: snapshot.name,
      project,
      envVars,
      services,
      domains,
      steps,
      summary,
      unsupportedFeatures,
    }
  }

  // -------------------------------------------------------------------------
  // Private helpers
  // -------------------------------------------------------------------------

  private extractGitInfo(app: CoolifyApplication): GitInfo | undefined {
    const repoUrl = app.git_repository
    if (!repoUrl) return undefined

    // Parse git URL: https://github.com/owner/repo or git@github.com:owner/repo.git
    const parsed = this.parseGitUrl(repoUrl)
    if (!parsed) return undefined

    return {
      provider: parsed.provider,
      owner: parsed.owner,
      repo: parsed.repo,
      defaultBranch: app.git_branch ?? 'main',
      cloneUrl: repoUrl,
    }
  }

  private parseGitUrl(url: string): { provider: GitInfo['provider']; owner: string; repo: string } | null {
    // HTTPS pattern: https://github.com/owner/repo(.git)?
    const httpsMatch = url.match(/https?:\/\/(github\.com|gitlab\.com|bitbucket\.org|gitea\.[^/]+)\/([^/]+)\/([^/.]+)/)
    if (httpsMatch) {
      const host = httpsMatch[1]!
      const owner = httpsMatch[2]!
      const repo = httpsMatch[3]!.replace(/\.git$/, '')
      return {
        provider: host.includes('github') ? 'github' : host.includes('gitlab') ? 'gitlab' : host.includes('bitbucket') ? 'bitbucket' : host.includes('gitea') ? 'gitea' : 'unknown',
        owner,
        repo,
      }
    }

    // SSH pattern: git@github.com:owner/repo.git
    const sshMatch = url.match(/git@(github\.com|gitlab\.com|bitbucket\.org|gitea\.[^:]+):([^/]+)\/([^/.]+)/)
    if (sshMatch) {
      const host = sshMatch[1]!
      const owner = sshMatch[2]!
      const repo = sshMatch[3]!.replace(/\.git$/, '')
      return {
        provider: host.includes('github') ? 'github' : host.includes('gitlab') ? 'gitlab' : host.includes('bitbucket') ? 'bitbucket' : host.includes('gitea') ? 'gitea' : 'unknown',
        owner,
        repo,
      }
    }

    return null
  }

  private extractEnvVars(envs: CoolifyEnvVar[]): EnvVar[] {
    return envs.map((e) => ({
      key: e.key,
      value: e.value ?? '',
      source: `coolify${e.is_build_time ? ':build-time' : ''}${e.is_preview ? ':preview' : ''}`,
      isSecret: isLikelySecret(e.key),
      isBuildTime: e.is_build_time ?? false,
    }))
  }

  private extractDomains(app: CoolifyApplication): DomainSnapshot[] {
    // Coolify stores domains as comma-separated URLs in the `fqdn` field
    if (!app.fqdn) return []

    return app.fqdn
      .split(',')
      .map((f) => f.trim())
      .filter(Boolean)
      .map((fqdn) => {
        try {
          const url = new URL(fqdn)
          const hostname = url.hostname
          return {
            domain: hostname,
            isApex: hostname.split('.').length === 2,
          }
        } catch {
          // If it's not a URL, treat it as a plain domain
          return {
            domain: fqdn.replace(/^https?:\/\//, '').replace(/\/.*/, ''),
            isApex: false,
          }
        }
      })
      // Filter out .sslip.io and .traefik.me (Coolify's auto-generated domains)
      .filter((d) => !d.domain.includes('.sslip.io') && !d.domain.includes('.traefik.me'))
  }

  private extractBuildInfo(app: CoolifyApplication): BuildInfo | undefined {
    const buildPack = app.build_pack ?? 'nixpacks'

    return {
      type: buildPack,
      buildCommand: app.build_command ?? undefined,
      installCommand: app.install_command ?? undefined,
      outputDirectory: app.publish_directory ?? undefined,
      dockerfilePath: app.dockerfile_location ?? app.base_directory ?? undefined,
    }
  }

  private extractServices(databases: CoolifyDatabase[], envVars: EnvVar[]): ServiceSnapshot[] {
    const services: ServiceSnapshot[] = []

    for (const db of databases) {
      const svcType = this.inferDatabaseType(db)

      // Find env vars that reference this database
      const relatedEnvKeys = envVars
        .filter((ev) => {
          if (!ev.value) return false
          const lower = ev.key.toLowerCase()
          const typeLower = svcType.toLowerCase()
          return lower.includes(typeLower) || lower.includes('database') || lower.includes('db_')
        })
        .map((ev) => ev.key)

      services.push({
        id: db.uuid,
        name: db.name,
        type: svcType,
        version: this.extractVersionFromImage(db.image),
        hasData: true,
        envVarKeys: relatedEnvKeys,
      })
    }

    // Also detect services from env var URLs (for external services not in Coolify's database list)
    for (const ev of envVars) {
      if (!ev.value || !ev.value.includes('://')) continue
      const svcType = detectServiceTypeFromUrl(ev.value)
      if (!svcType) continue

      // Don't duplicate if already found from databases
      const alreadyFound = services.some((s) => s.type === svcType)
      if (alreadyFound) continue

      services.push({
        id: `env-detected-${svcType}`,
        name: `${svcType} (from env vars)`,
        type: svcType,
        hasData: true,
        envVarKeys: [ev.key],
        connectionUrl: ev.value,
      })
    }

    return services
  }

  private inferDatabaseType(db: CoolifyDatabase): ServiceSnapshot['type'] {
    const image = (db.image ?? '').toLowerCase()
    const type = (db.type ?? '').toLowerCase()

    if (image.includes('postgres') || type.includes('postgres')) return 'postgres'
    if (image.includes('mysql') || image.includes('mariadb') || type.includes('mysql') || type.includes('mariadb')) return 'mysql'
    if (image.includes('redis') || type.includes('redis') || image.includes('keydb') || image.includes('dragonfly')) return 'redis'
    if (image.includes('mongo') || type.includes('mongo')) return 'mongodb'
    if (image.includes('minio') || type.includes('s3')) return 's3'

    return 'unknown'
  }

  private extractVersionFromImage(image?: string | null): string | undefined {
    if (!image) return undefined
    const parts = image.split(':')
    return parts.length > 1 ? parts[parts.length - 1] : undefined
  }

  private buildServicePlan(svc: ServiceSnapshot): ServicePlan {
    const dataImplications = []

    if (svc.hasData) {
      dataImplications.push({
        severity: 'data-not-migrated' as const,
        message: `${svc.type} data is NOT migrated. Only the service container is created.`,
        recommendedAction: `Export data from Coolify's ${svc.type} and import into the new Temps service.`,
      })
    }

    return {
      name: svc.name,
      type: svc.type,
      version: svc.version,
      action: svc.type === 'unknown' ? 'skip' : 'create',
      actionDescription:
        svc.type === 'unknown'
          ? `Skip unknown service type "${svc.name}" (manual setup required)`
          : `Create a new ${svc.type}${svc.version ? ` v${svc.version}` : ''} service in Temps`,
      envVarKeys: svc.envVarKeys,
      dataImplications,
    }
  }

  private detectUnsupportedFeatures(snapshot: ProjectSnapshot): UnsupportedFeature[] {
    const unsupported: UnsupportedFeature[] = []
    const meta = snapshot.sourceMetadata as CoolifyApplication | undefined

    if (meta?.docker_compose_location) {
      unsupported.push({
        feature: 'Docker Compose deployment',
        reason: 'Coolify supports docker-compose based deployments which Temps handles differently.',
        alternative: 'Split services into individual Temps projects or use Temps external services for databases.',
      })
    }

    if (meta?.build_pack === 'static') {
      unsupported.push({
        feature: 'Coolify static site build pack',
        reason: 'Coolify has a special static site deployment mode.',
        alternative: 'Use the "static" preset in Temps which serves files via nginx.',
      })
    }

    return unsupported
  }
}
