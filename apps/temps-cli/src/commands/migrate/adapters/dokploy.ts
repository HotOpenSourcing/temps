/**
 * Dokploy platform adapter.
 *
 * Dokploy uses tRPC-style flat routes:
 *   Base URL: https://<host>/api
 *   Auth: x-api-key header
 *
 * Key endpoints used:
 *   GET /project.all                          — all projects with nested applications
 *   GET /application.one?applicationId=<id>   — application detail
 *   GET /domain.byApplicationId?applicationId=<id> — domains for an application
 *   GET /auth.get                             — validate API key
 *
 * Env vars: stored as newline-separated KEY=VALUE strings in the `env` field.
 * Git source: varies by `sourceType` (github/gitlab/bitbucket/gitea/git/docker).
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
  detectServiceTypeFromKey,
  detectServiceTypeFromUrl,
  inferPreset,
  buildSteps,
  buildSummary,
} from './base.js'

// ---------------------------------------------------------------------------
// Dokploy API response types
// ---------------------------------------------------------------------------

interface DokployProject {
  projectId: string
  name: string
  description?: string
  createdAt?: string
  applications?: DokployApplication[]
  mariadb?: DokployDatabase[]
  mongo?: DokployDatabase[]
  mysql?: DokployDatabase[]
  postgres?: DokployDatabase[]
  redis?: DokployDatabase[]
}

interface DokployApplication {
  applicationId: string
  name: string
  appName?: string
  description?: string
  env?: string | null
  buildType?: string | null
  sourceType?: string | null
  // GitHub source
  repository?: string | null
  owner?: string | null
  branch?: string | null
  // GitLab source
  gitlabProjectId?: number | null
  gitlabRepository?: string | null
  gitlabOwner?: string | null
  gitlabBranch?: string | null
  gitlabPathNamespace?: string | null
  // Bitbucket source
  bitbucketRepository?: string | null
  bitbucketOwner?: string | null
  bitbucketBranch?: string | null
  // Generic git source
  customGitUrl?: string | null
  customGitBranch?: string | null
  // Docker source
  dockerImage?: string | null
  // Build config
  buildPath?: string | null
  publishDirectory?: string | null
  dockerfile?: string | null
  dockerContextPath?: string | null
  buildArgs?: string | null
  // Domains (may be embedded or separate call)
  domains?: DokployDomain[]
  // Status
  applicationStatus?: string
  createdAt?: string
  updatedAt?: string
}

interface DokployDomain {
  domainId: string
  host: string
  path?: string
  port?: number
  https?: boolean
  certificateType?: string
  applicationId?: string
  uniqueConfigKey?: number
}

interface DokployDatabase {
  databaseId?: string
  name: string
  appName?: string
  description?: string
  databaseType?: string
  dockerImage?: string
  env?: string | null
  externalPort?: number | null
  databasePassword?: string
  databaseUser?: string
  databaseRootPassword?: string
  createdAt?: string
}

interface DokployAuth {
  id?: string
  email?: string
  rol?: string
}

// ---------------------------------------------------------------------------
// HTTP helpers
// ---------------------------------------------------------------------------

function normalizeBaseUrl(baseUrl: string): string {
  let url = baseUrl.replace(/\/+$/, '')
  if (!url.endsWith('/api')) {
    url += '/api'
  }
  return url
}

async function dokployFetch<T>(
  path: string,
  creds: PlatformCredentials
): Promise<T> {
  const base = normalizeBaseUrl(creds.baseUrl ?? 'http://localhost:3000')
  const url = `${base}${path}`

  const res = await fetch(url, {
    headers: {
      'x-api-key': creds.token,
      'Content-Type': 'application/json',
      Accept: 'application/json',
    },
  })

  if (!res.ok) {
    const body = await res.text()
    throw new Error(`Dokploy API ${res.status}: ${body}`)
  }

  return res.json() as Promise<T>
}

// ---------------------------------------------------------------------------
// Adapter implementation
// ---------------------------------------------------------------------------

export class DokployAdapter implements PlatformAdapter {
  readonly platformId = 'dokploy' as const

  async validateCredentials(creds: PlatformCredentials): Promise<CredentialValidation> {
    try {
      const auth = await dokployFetch<DokployAuth>('/auth.get', creds)
      const accountName = auth.email ?? auth.id ?? creds.baseUrl ?? 'unknown'

      return {
        valid: true,
        message: `Authenticated as ${accountName} at ${creds.baseUrl}`,
        accountName,
      }
    } catch (err) {
      return {
        valid: false,
        message: `Authentication failed: ${err instanceof Error ? err.message : String(err)}`,
      }
    }
  }

  async discoverProjects(creds: PlatformCredentials): Promise<DiscoveredProject[]> {
    const projects = await dokployFetch<DokployProject[]>('/project.all', creds)
    const discovered: DiscoveredProject[] = []

    for (const project of projects) {
      if (!project.applications || project.applications.length === 0) continue

      for (const app of project.applications) {
        discovered.push({
          id: app.applicationId,
          name: `${project.name} / ${app.name}`,
          framework: app.buildType ?? app.sourceType ?? undefined,
          updatedAt: app.updatedAt ?? app.createdAt,
          gitUrl: this.getGitUrl(app),
          metadata: {
            project: project.name,
            ...(app.sourceType ? { sourceType: app.sourceType } : {}),
            ...(app.buildType ? { buildType: app.buildType } : {}),
            ...(app.applicationStatus ? { status: app.applicationStatus } : {}),
          },
        })
      }
    }

    return discovered
  }

  async snapshotProject(creds: PlatformCredentials, projectId: string): Promise<ProjectSnapshot> {
    // Fetch application detail and all projects (for database context)
    const [app, allProjects, domains] = await Promise.all([
      dokployFetch<DokployApplication>(`/application.one?applicationId=${projectId}`, creds),
      dokployFetch<DokployProject[]>('/project.all', creds),
      dokployFetch<DokployDomain[]>(`/domain.byApplicationId?applicationId=${projectId}`, creds).catch(() => []),
    ])

    // Find which Dokploy project contains this application (for related databases)
    const parentProject = allProjects.find((p) =>
      p.applications?.some((a) => a.applicationId === projectId)
    )

    const git = this.extractGitInfo(app)
    const envVars = this.parseEnvString(app.env)
    const domainSnapshots = this.extractDomains(domains, app)
    const build = this.extractBuildInfo(app)
    const services = this.extractServices(parentProject, envVars)

    return {
      id: app.applicationId,
      name: app.name,
      framework: app.buildType ?? undefined,
      git,
      envVars,
      services,
      domains: domainSnapshots,
      build,
      sourceMetadata: app,
    }
  }

  generatePlan(snapshot: ProjectSnapshot): MigrationPlan {
    const preset = inferPreset(snapshot.framework, snapshot.build?.type)

    const project = {
      name: snapshot.name.replace(/\s*\/\s*/g, '-'), // "Project / App" → "Project-App"
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
      actionDescription: `Import domain ${d.domain}`,
    }))

    const unsupportedFeatures = this.detectUnsupportedFeatures(snapshot)
    const steps = buildSteps({ project, envVars, services, domains })
    const summary = buildSummary('dokploy', { envVars, services, domains }, unsupportedFeatures)

    return {
      platform: 'dokploy',
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

  private getGitUrl(app: DokployApplication): string | undefined {
    switch (app.sourceType) {
      case 'github':
        if (app.owner && app.repository) return `https://github.com/${app.owner}/${app.repository}`
        break
      case 'gitlab':
        if (app.gitlabOwner && app.gitlabRepository) return `https://gitlab.com/${app.gitlabOwner}/${app.gitlabRepository}`
        break
      case 'bitbucket':
        if (app.bitbucketOwner && app.bitbucketRepository) return `https://bitbucket.org/${app.bitbucketOwner}/${app.bitbucketRepository}`
        break
      case 'git':
        return app.customGitUrl ?? undefined
    }
    return undefined
  }

  private extractGitInfo(app: DokployApplication): GitInfo | undefined {
    switch (app.sourceType) {
      case 'github':
        if (!app.owner || !app.repository) return undefined
        return {
          provider: 'github',
          owner: app.owner,
          repo: app.repository,
          defaultBranch: app.branch ?? 'main',
          cloneUrl: `https://github.com/${app.owner}/${app.repository}.git`,
        }
      case 'gitlab':
        if (!app.gitlabOwner || !app.gitlabRepository) return undefined
        return {
          provider: 'gitlab',
          owner: app.gitlabOwner,
          repo: app.gitlabRepository,
          defaultBranch: app.gitlabBranch ?? 'main',
          cloneUrl: `https://gitlab.com/${app.gitlabOwner}/${app.gitlabRepository}.git`,
        }
      case 'bitbucket':
        if (!app.bitbucketOwner || !app.bitbucketRepository) return undefined
        return {
          provider: 'bitbucket',
          owner: app.bitbucketOwner,
          repo: app.bitbucketRepository,
          defaultBranch: app.bitbucketBranch ?? 'main',
          cloneUrl: `https://bitbucket.org/${app.bitbucketOwner}/${app.bitbucketRepository}.git`,
        }
      case 'git':
        if (!app.customGitUrl) return undefined
        return {
          provider: 'unknown',
          owner: '',
          repo: app.customGitUrl.split('/').pop()?.replace('.git', '') ?? '',
          defaultBranch: app.customGitBranch ?? 'main',
          cloneUrl: app.customGitUrl,
        }
      default:
        return undefined
    }
  }

  /**
   * Parse Dokploy's env var string format: newline-separated KEY=VALUE pairs.
   */
  private parseEnvString(env?: string | null): EnvVar[] {
    if (!env) return []

    const result: EnvVar[] = []

    for (const line of env.split('\n')) {
      const trimmed = line.trim()
      if (!trimmed || trimmed.startsWith('#')) continue

      const eqIndex = trimmed.indexOf('=')
      if (eqIndex === -1) continue

      const key = trimmed.slice(0, eqIndex).trim()
      let value = trimmed.slice(eqIndex + 1).trim()

      // Remove surrounding quotes
      if ((value.startsWith('"') && value.endsWith('"')) || (value.startsWith("'") && value.endsWith("'"))) {
        value = value.slice(1, -1)
      }

      if (!key) continue

      result.push({
        key,
        value,
        source: 'dokploy:env',
        isSecret: isLikelySecret(key),
        isBuildTime: false,
      })
    }

    return result
  }

  private extractDomains(domains: DokployDomain[], app: DokployApplication): DomainSnapshot[] {
    // Combine explicit domains from API with any embedded in the app
    const allDomains = [...domains, ...(app.domains ?? [])]

    return allDomains
      .filter((d) => d.host)
      .map((d) => ({
        domain: d.host,
        isApex: d.host.split('.').length === 2,
      }))
      // Deduplicate by domain name
      .filter((d, i, arr) => arr.findIndex((x) => x.domain === d.domain) === i)
  }

  private extractBuildInfo(app: DokployApplication): BuildInfo | undefined {
    const buildType = app.buildType ?? 'nixpacks'

    return {
      type: buildType,
      buildCommand: undefined, // Dokploy uses build type, not build commands
      installCommand: undefined,
      outputDirectory: app.publishDirectory ?? undefined,
      dockerfilePath: app.dockerfile ?? app.buildPath ?? undefined,
    }
  }

  private extractServices(parentProject: DokployProject | undefined, envVars: EnvVar[]): ServiceSnapshot[] {
    const services: ServiceSnapshot[] = []

    if (parentProject) {
      // Map Dokploy's typed database arrays
      const dbArrays: Array<{ dbs: DokployDatabase[]; type: ServiceSnapshot['type'] }> = [
        { dbs: parentProject.postgres ?? [], type: 'postgres' },
        { dbs: parentProject.mysql ?? [], type: 'mysql' },
        { dbs: parentProject.mongo ?? [], type: 'mongodb' },
        { dbs: parentProject.redis ?? [], type: 'redis' },
        { dbs: parentProject.mariadb ?? [], type: 'mysql' },
      ]

      for (const { dbs, type } of dbArrays) {
        for (const db of dbs) {
          const relatedEnvKeys = envVars
            .filter((ev) => {
              const lower = ev.key.toLowerCase()
              return lower.includes(type) || lower.includes('database') || lower.includes('db_url')
            })
            .map((ev) => ev.key)

          services.push({
            id: db.databaseId ?? db.name,
            name: db.name,
            type,
            version: this.extractVersionFromImage(db.dockerImage),
            hasData: true,
            envVarKeys: relatedEnvKeys,
          })
        }
      }
    }

    // Also detect services from env var URLs
    for (const ev of envVars) {
      if (!ev.value || !ev.value.includes('://')) continue
      const svcType = detectServiceTypeFromUrl(ev.value)
      if (!svcType) continue

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
        recommendedAction: `Export data from Dokploy's ${svc.type} and import into the new Temps service.`,
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
    const meta = snapshot.sourceMetadata as DokployApplication | undefined

    if (meta?.sourceType === 'docker') {
      unsupported.push({
        feature: 'Docker image deployment',
        reason: 'Dokploy supports deploying pre-built Docker images directly.',
        alternative: 'In Temps, set up a git-based deployment with a Dockerfile, or use the Docker preset.',
      })
    }

    if (meta?.buildType === 'docker-compose' || meta?.buildType === 'compose') {
      unsupported.push({
        feature: 'Docker Compose deployment',
        reason: 'Multi-container compose deployments need to be split.',
        alternative: 'Split services into individual Temps projects and use external services for databases.',
      })
    }

    return unsupported
  }
}
