import type { Command } from 'commander'
import { requireAuth } from '../../config/store.js'
import { setupClient, client } from '../../lib/api-client.js'
import { resolveProjectSlug } from '../../config/resolve-project.js'
import { hasProjectConfig, writeProjectConfig } from '../../config/project-config.js'
import { deploy } from '../deploy/deploy.js'
import { deployLocalImage } from '../deploy/deploy-local-image.js'
import { runSetupWizard } from './setup-wizard.js'
import { detectGitBranch } from '../../lib/detect-project.js'
import { promptConfirm } from '../../ui/prompts.js'
import { info, warning, newline, colors } from '../../ui/output.js'
import { getProjectBySlug } from '../../api/sdk.gen.js'

interface UpOptions {
  project?: string
  environment?: string
  branch?: string
  name?: string
  preset?: string
  manual?: boolean
  noServices?: boolean
  wait?: boolean
  yes?: boolean
}

async function up(projectArg: string | undefined, options: UpOptions): Promise<void> {
  await requireAuth()
  await setupClient()

  // Resolve project — check if already linked
  const resolved = await resolveProjectSlug(projectArg ?? options.project)

  if (!resolved) {
    // No project linked — run the setup wizard
    const result = await runSetupWizard({
      name: options.name,
      preset: options.preset,
      branch: options.branch,
      manual: options.manual,
      noServices: options.noServices,
      yes: options.yes,
    })

    if (!result) {
      // Wizard was cancelled
      return
    }

    // Wizard already triggered the first deployment and saved config
    return
  }

  // Project is already linked — deploy as usual

  if (resolved.source !== 'flag') {
    info(`Using project ${colors.bold(resolved.slug)} (from ${resolved.source})`)
  }

  // Fetch project to check source type
  const { data: project } = await getProjectBySlug({
    client,
    path: { slug: resolved.slug },
  })

  const sourceType = project?.source_type

  if (sourceType === 'manual' || sourceType === 'docker_image') {
    // Manual/docker_image projects deploy via local image build + upload
    await deployLocalImage({
      project: resolved.slug,
      environment: options.environment,
      wait: options.wait,
      yes: options.yes,
    })
  } else {
    // Git-based projects trigger the pipeline
    let branch = options.branch
    if (!branch) {
      const detectedBranch = detectGitBranch()
      if (detectedBranch && detectedBranch !== 'HEAD') {
        branch = detectedBranch
      }
    }

    await deploy({
      project: resolved.slug,
      environment: options.environment,
      branch,
      wait: options.wait,
      yes: options.yes,
    })
  }

  // Offer to save config if it doesn't exist
  if (!hasProjectConfig() && !options.yes) {
    newline()
    const save = await promptConfirm({
      message: 'Save this project link for future use?',
      default: true,
    })
    if (save) {
      await writeProjectConfig({ projectSlug: resolved.slug })
      info('Saved to .temps/config.json')
    }
  }
}

export function registerUpCommand(program: Command): void {
  program
    .command('up [project]')
    .description('Deploy the current project (runs setup wizard if not linked)')
    .option('-p, --project <project>', 'Project slug or ID')
    .option('-e, --environment <env>', 'Target environment name')
    .option('-b, --branch <branch>', 'Git branch to deploy (auto-detected from cwd)')
    .option('-n, --name <name>', 'Project name (for new projects)')
    .option('--preset <preset>', 'Framework preset slug (skip auto-detection)')
    .option('--manual', 'Use manual deployment mode (no git)')
    .option('--no-services', 'Skip external service setup')
    .option('--no-wait', 'Do not wait for deployment to complete')
    .option('-y, --yes', 'Skip confirmation prompts')
    .action((projectArg, opts) => {
      if (projectArg && !opts.project) {
        opts.project = projectArg
      }
      return up(projectArg, opts)
    })
}
