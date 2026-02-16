/**
 * Shared service setup utilities for project creation and setup wizard.
 * Extracted from commands/projects/create.ts to avoid duplication.
 */
import { client, getErrorMessage } from './api-client.js'
import { listServices, createService } from '../api/sdk.gen.js'
import type { ExternalServiceInfo, ServiceTypeRoute } from '../api/types.gen.js'
import { promptSelect, promptText, promptConfirm, promptCheckbox, type SelectOption } from '../ui/prompts.js'
import { withSpinner, startSpinner, succeedSpinner } from '../ui/spinner.js'
import { success, error, info, newline, colors } from '../ui/output.js'

/** Service type configuration */
export const SERVICE_TYPES: { id: ServiceTypeRoute; name: string; description: string }[] = [
  { id: 'postgres', name: 'PostgreSQL', description: 'Reliable Relational Database' },
  { id: 'redis', name: 'Redis', description: 'In-Memory Data Store' },
  { id: 's3', name: 'S3', description: 'Object Storage (MinIO)' },
  { id: 'mongodb', name: 'MongoDB', description: 'Document Database' },
]

/**
 * Interactive service selection flow.
 * Asks user if they want services, shows existing ones, allows creating new ones.
 * Returns array of service IDs to link to the project.
 */
export async function selectStorageServices(): Promise<number[]> {
  newline()

  const addServices = await promptConfirm({
    message: 'Add storage services (PostgreSQL, Redis, etc.)?',
    default: false,
  })

  if (!addServices) {
    return []
  }

  // Load existing services
  const spinner = startSpinner('Loading services...')
  const { data: servicesData } = await listServices({ client })
  succeedSpinner('Services loaded')

  const existingServices = servicesData || []
  const selectedServiceIds: number[] = []

  newline()

  // Show existing services if any
  if (existingServices.length > 0) {
    const serviceChoices: SelectOption<number | string>[] = existingServices.map((s) => ({
      name: `${s.name} (${s.service_type})`,
      value: s.id,
      description: `Created ${new Date(s.created_at).toLocaleDateString()}`,
    }))

    serviceChoices.push({
      name: colors.success('+ Create new service'),
      value: 'create_new',
      description: 'Create a new storage service',
    })

    const selected = await promptSelect({
      message: 'Select existing service or create new',
      choices: serviceChoices,
    })

    if (selected !== 'create_new') {
      selectedServiceIds.push(selected as number)

      // Ask if they want to add more
      let addMore = true
      while (addMore) {
        addMore = await promptConfirm({
          message: 'Add another service?',
          default: false,
        })

        if (addMore) {
          const remainingServices = existingServices.filter(
            (s) => !selectedServiceIds.includes(s.id)
          )

          if (remainingServices.length === 0) {
            info('No more services available')
            break
          }

          const moreChoices: SelectOption<number | string>[] = remainingServices.map((s) => ({
            name: `${s.name} (${s.service_type})`,
            value: s.id,
          }))

          moreChoices.push({
            name: colors.success('+ Create new service'),
            value: 'create_new',
            description: 'Create a new storage service',
          })

          const moreSelected = await promptSelect({
            message: 'Select service',
            choices: moreChoices,
          })

          if (moreSelected === 'create_new') {
            const newServiceId = await createNewService()
            if (newServiceId) {
              selectedServiceIds.push(newServiceId)
            }
          } else {
            selectedServiceIds.push(moreSelected as number)
          }
        }
      }

      return selectedServiceIds
    }
  }

  // Create new service
  const newServiceId = await createNewService()
  if (newServiceId) {
    selectedServiceIds.push(newServiceId)
  }

  return selectedServiceIds
}

/**
 * Interactive service selection with pre-suggested types.
 * Similar to selectStorageServices but shows a checkbox of suggested types first.
 */
export async function selectServicesWithSuggestions(
  suggestedTypes: ServiceTypeRoute[]
): Promise<number[]> {
  newline()

  // Load existing services
  const spinner = startSpinner('Loading services...')
  const { data: servicesData } = await listServices({ client })
  succeedSpinner('Services loaded')

  const existingServices = servicesData || []
  const selectedServiceIds: number[] = []

  // Build choices - show suggested types with existing service matches
  const serviceChoices: SelectOption<string>[] = []

  for (const serviceType of SERVICE_TYPES) {
    const existing = existingServices.filter((s) => s.service_type === serviceType.id)
    const isSuggested = suggestedTypes.includes(serviceType.id)

    if (existing.length > 0) {
      for (const svc of existing) {
        serviceChoices.push({
          name: `${serviceType.name}: ${svc.name}`,
          value: `existing:${svc.id}`,
          description: isSuggested ? 'Recommended - already exists' : 'Existing service',
        })
      }
    }

    serviceChoices.push({
      name: `${serviceType.name}: Create new`,
      value: `new:${serviceType.id}`,
      description: isSuggested
        ? `Recommended for your project`
        : serviceType.description,
    })
  }

  newline()

  const selected = await promptCheckbox<string>({
    message: 'Select services to add (space to toggle, enter to confirm)',
    choices: serviceChoices,
  })

  for (const selection of selected) {
    if (selection.startsWith('existing:')) {
      const id = parseInt(selection.split(':')[1]!, 10)
      selectedServiceIds.push(id)
    } else if (selection.startsWith('new:')) {
      const type = selection.split(':')[1]! as ServiceTypeRoute
      const newId = await createNewServiceOfType(type)
      if (newId) {
        selectedServiceIds.push(newId)
      }
    }
  }

  return selectedServiceIds
}

/**
 * Create a new service with interactive type selection and name prompt.
 */
export async function createNewService(): Promise<number | null> {
  newline()

  const typeChoices: SelectOption<ServiceTypeRoute>[] = SERVICE_TYPES.map((t) => ({
    name: t.name,
    value: t.id,
    description: t.description,
  }))

  const serviceType = await promptSelect({
    message: 'Select service type',
    choices: typeChoices,
  })

  return createNewServiceOfType(serviceType)
}

/**
 * Create a new service of a specific type with a name prompt.
 */
export async function createNewServiceOfType(serviceType: ServiceTypeRoute): Promise<number | null> {
  const typeLabel = SERVICE_TYPES.find((t) => t.id === serviceType)?.name || serviceType

  const serviceName = await promptText({
    message: `${typeLabel} service name`,
    default: `${serviceType}-${Date.now().toString(36)}`,
    required: true,
  })

  const { data, error: apiError } = await withSpinner(
    `Creating ${typeLabel} service...`,
    async () => {
      return await createService({
        client,
        body: {
          name: serviceName,
          service_type: serviceType,
          parameters: {},
        },
      })
    }
  )

  if (apiError || !data) {
    error(`Failed to create service: ${getErrorMessage(apiError)}`)
    return null
  }

  success(`Service "${serviceName}" created`)
  return data.id
}
