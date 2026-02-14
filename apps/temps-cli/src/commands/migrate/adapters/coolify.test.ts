import { test, expect, describe } from 'bun:test'
import { CoolifyAdapter } from './coolify.js'
import type { ProjectSnapshot } from '../types.js'

describe('CoolifyAdapter.generatePlan', () => {
  const adapter = new CoolifyAdapter()

  function makeSnapshot(overrides: Partial<ProjectSnapshot> = {}): ProjectSnapshot {
    return {
      id: 'uuid-123',
      name: 'my-app',
      framework: 'nixpacks',
      git: {
        provider: 'github',
        owner: 'myorg',
        repo: 'my-app',
        defaultBranch: 'main',
        cloneUrl: 'https://github.com/myorg/my-app.git',
      },
      envVars: [
        { key: 'APP_PORT', value: '3000', isSecret: false, source: 'coolify' },
        { key: 'DB_PASSWORD', value: 'secret123', isSecret: true, source: 'coolify' },
        { key: 'REDIS_URL', value: 'redis://redis:6379', isSecret: false, source: 'coolify' },
      ],
      services: [
        {
          id: 'db-uuid',
          name: 'app-postgres',
          type: 'postgres',
          version: '16',
          hasData: true,
          envVarKeys: ['DB_PASSWORD'],
        },
        {
          id: 'env-detected-redis',
          name: 'redis (from env vars)',
          type: 'redis',
          hasData: true,
          envVarKeys: ['REDIS_URL'],
          connectionUrl: 'redis://redis:6379',
        },
      ],
      domains: [
        { domain: 'myapp.example.com', isApex: false },
      ],
      build: {
        type: 'nixpacks',
        buildCommand: 'npm run build',
        installCommand: 'npm install',
        outputDirectory: 'dist',
      },
      ...overrides,
    }
  }

  test('generates plan with correct platform', () => {
    const plan = adapter.generatePlan(makeSnapshot())
    expect(plan.platform).toBe('coolify')
    expect(plan.sourceProjectId).toBe('uuid-123')
  })

  test('infers nixpacks preset for nixpacks build pack', () => {
    const plan = adapter.generatePlan(makeSnapshot())
    expect(plan.project.preset).toBe('nixpacks')
  })

  test('maps multiple services correctly', () => {
    const plan = adapter.generatePlan(makeSnapshot())
    expect(plan.services.length).toBe(2)
    expect(plan.services.some((s) => s.type === 'postgres')).toBe(true)
    expect(plan.services.some((s) => s.type === 'redis')).toBe(true)
  })

  test('preserves version from services', () => {
    const plan = adapter.generatePlan(makeSnapshot())
    const pg = plan.services.find((s) => s.type === 'postgres')
    expect(pg!.version).toBe('16')
  })

  test('marks service env vars as skip', () => {
    const plan = adapter.generatePlan(makeSnapshot())
    const dbPassword = plan.envVars.find((e) => e.key === 'DB_PASSWORD')
    expect(dbPassword!.skip).toBe(true) // Managed by postgres service
  })

  test('handles domain extraction', () => {
    const plan = adapter.generatePlan(makeSnapshot())
    expect(plan.domains.length).toBe(1)
    expect(plan.domains[0]!.domain).toBe('myapp.example.com')
  })

  test('skips unknown service types', () => {
    const plan = adapter.generatePlan(
      makeSnapshot({
        services: [
          {
            id: 'unknown-svc',
            name: 'some-unknown',
            type: 'unknown',
            hasData: true,
            envVarKeys: [],
          },
        ],
      })
    )

    expect(plan.services[0]!.action).toBe('skip')
    expect(plan.services[0]!.actionDescription).toContain('Skip')
  })

  test('detects docker-compose as unsupported', () => {
    const plan = adapter.generatePlan(
      makeSnapshot({
        sourceMetadata: {
          uuid: 'uuid-123',
          name: 'my-app',
          docker_compose_location: '/docker-compose.yml',
        },
      })
    )

    expect(plan.unsupportedFeatures.some((f) => f.feature.includes('Docker Compose'))).toBe(true)
  })

  test('handles empty project gracefully', () => {
    const plan = adapter.generatePlan(
      makeSnapshot({
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
    expect(plan.project.git).toBeUndefined()
  })
})
