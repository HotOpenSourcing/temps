import { test, expect, describe } from 'bun:test'
import { DokployAdapter } from './dokploy.js'
import type { ProjectSnapshot } from '../types.js'

describe('DokployAdapter.generatePlan', () => {
  const adapter = new DokployAdapter()

  function makeSnapshot(overrides: Partial<ProjectSnapshot> = {}): ProjectSnapshot {
    return {
      id: 'app-id-123',
      name: 'MyProject / my-app',
      framework: 'nixpacks',
      git: {
        provider: 'github',
        owner: 'myuser',
        repo: 'my-app',
        defaultBranch: 'main',
        cloneUrl: 'https://github.com/myuser/my-app.git',
      },
      envVars: [
        { key: 'PORT', value: '3000', isSecret: false, source: 'dokploy:env' },
        { key: 'SECRET_KEY', value: 'supersecret', isSecret: true, source: 'dokploy:env' },
        { key: 'DATABASE_URL', value: 'postgres://user:pass@db:5432/app', isSecret: true, source: 'dokploy:env' },
      ],
      services: [
        {
          id: 'pg-id',
          name: 'app-db',
          type: 'postgres',
          version: '15',
          hasData: true,
          envVarKeys: ['DATABASE_URL'],
        },
      ],
      domains: [
        { domain: 'app.example.com', isApex: false },
      ],
      build: {
        type: 'nixpacks',
        outputDirectory: 'dist',
      },
      ...overrides,
    }
  }

  test('generates plan with correct platform', () => {
    const plan = adapter.generatePlan(makeSnapshot())
    expect(plan.platform).toBe('dokploy')
    expect(plan.sourceProjectId).toBe('app-id-123')
  })

  test('normalizes project name (removes /)', () => {
    const plan = adapter.generatePlan(makeSnapshot())
    expect(plan.project.name).toBe('MyProject-my-app')
    expect(plan.project.name).not.toContain('/')
  })

  test('maps services with version', () => {
    const plan = adapter.generatePlan(makeSnapshot())
    expect(plan.services.length).toBe(1)
    expect(plan.services[0]!.type).toBe('postgres')
    expect(plan.services[0]!.version).toBe('15')
  })

  test('marks service env vars as skip', () => {
    const plan = adapter.generatePlan(makeSnapshot())
    const dbUrl = plan.envVars.find((e) => e.key === 'DATABASE_URL')
    expect(dbUrl!.skip).toBe(true)
  })

  test('handles Docker source type as unsupported', () => {
    const plan = adapter.generatePlan(
      makeSnapshot({
        sourceMetadata: {
          applicationId: 'app-id-123',
          name: 'my-app',
          sourceType: 'docker',
        },
      })
    )

    expect(plan.unsupportedFeatures.some((f) => f.feature.includes('Docker image'))).toBe(true)
  })

  test('handles docker-compose build type as unsupported', () => {
    const plan = adapter.generatePlan(
      makeSnapshot({
        sourceMetadata: {
          applicationId: 'app-id-123',
          name: 'my-app',
          buildType: 'docker-compose',
        },
      })
    )

    expect(plan.unsupportedFeatures.some((f) => f.feature.includes('Docker Compose'))).toBe(true)
  })

  test('handles GitLab git source', () => {
    const plan = adapter.generatePlan(
      makeSnapshot({
        git: {
          provider: 'gitlab',
          owner: 'mygroup',
          repo: 'my-app',
          defaultBranch: 'develop',
          cloneUrl: 'https://gitlab.com/mygroup/my-app.git',
        },
      })
    )

    expect(plan.project.git!.provider).toBe('gitlab')
    expect(plan.project.git!.owner).toBe('mygroup')
    expect(plan.project.mainBranch).toBe('develop')
  })

  test('handles empty project', () => {
    const plan = adapter.generatePlan(
      makeSnapshot({
        name: 'empty-app',
        git: undefined,
        envVars: [],
        services: [],
        domains: [],
        build: undefined,
      })
    )

    expect(plan.envVars.length).toBe(0)
    expect(plan.services.length).toBe(0)
    expect(plan.domains.length).toBe(0)
  })

  test('steps are in correct order', () => {
    const plan = adapter.generatePlan(makeSnapshot())
    const ids = plan.steps.map((s) => s.id)

    // Project should be first
    expect(ids[0]).toBe('create-project')

    // Service before env vars
    const svcIdx = ids.findIndex((id) => id.startsWith('service-'))
    const envIdx = ids.findIndex((id) => id === 'set-env-vars')
    if (svcIdx !== -1 && envIdx !== -1) {
      expect(svcIdx).toBeLessThan(envIdx)
    }
  })

  test('summary reflects risk from data services', () => {
    const plan = adapter.generatePlan(makeSnapshot())
    expect(plan.summary.overallRisk).toBe('high') // postgres has data-not-migrated
  })
})
