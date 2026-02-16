<p align="center">
  <img src="https://raw.githubusercontent.com/AnomalyCo/temps/main/temps-demo.gif" alt="Temps Platform" width="700" />
</p>

<h1 align="center">@temps-sdk/node-sdk</h1>

<p align="center">
  <a href="https://www.npmjs.com/package/@temps-sdk/node-sdk"><img src="https://img.shields.io/npm/v/@temps-sdk/node-sdk.svg" alt="npm version" /></a>
  <a href="https://www.npmjs.com/package/@temps-sdk/node-sdk"><img src="https://img.shields.io/npm/dm/@temps-sdk/node-sdk.svg" alt="npm downloads" /></a>
  <a href="https://github.com/AnomalyCo/temps/blob/main/LICENSE"><img src="https://img.shields.io/npm/l/@temps-sdk/node-sdk.svg" alt="license" /></a>
</p>

<p align="center">
  The official Node.js SDK for Temps. A typed API client for the entire platform and a Sentry-compatible error tracking client for server-side applications.
</p>

---

```bash
# npm
npm install @temps-sdk/node-sdk

# bun
bun add @temps-sdk/node-sdk

# pnpm
pnpm add @temps-sdk/node-sdk

# yarn
yarn add @temps-sdk/node-sdk
```

**Peer dependencies:** `typescript >= 5`

## API Client

### Setup

```typescript
import { TempsClient } from '@temps-sdk/node-sdk';

const temps = new TempsClient({
  baseUrl: 'https://your-instance.temps.dev',
  apiKey: 'your-api-key',
});
```

### Namespaces

The client organizes all platform operations into 24 typed namespaces:

```typescript
// Projects
const { data: projects } = await temps.projects.list();
const { data: project } = await temps.projects.get({ path: { project_id: 1 } });

// Deployments
const { data: deployments } = await temps.deployments.list({ path: { project_id: 1 } });
await temps.deployments.deploy({ path: { project_id: 1 }, body: { branch: 'main' } });
await temps.deployments.rollback({ path: { project_id: 1, deployment_id: 5 } });

// Git Providers
const { data: providers } = await temps.git.listProviders();
await temps.git.syncRepositories({ path: { provider_id: 1 } });

// External Services (PostgreSQL, Redis, S3)
const { data: services } = await temps.externalServices.list({ path: { project_id: 1 } });

// Domains & SSL
const { data: domains } = await temps.domains.list();
await temps.domains.provision({ path: { domain_id: 1 } });

// Monitoring
const { data: monitors } = await temps.monitoring.listMonitors();
const { data: incidents } = await temps.monitoring.listIncidents({ path: { monitor_id: 1 } });

// Analytics
const { data: stats } = await temps.analytics.getGeneralStats({ path: { project_id: 1 } });
const { data: visitors } = await temps.analytics.getVisitors({ path: { project_id: 1 } });
```

**Full namespace list:**

| Namespace | Description |
|-----------|-------------|
| `temps.apiKeys` | API key CRUD, activate/deactivate |
| `temps.analytics` | Visitors, sessions, events, enrichment, stats |
| `temps.auditLogs` | Audit log retrieval |
| `temps.authentication` | Login, logout, magic links, MFA, password reset |
| `temps.backups` | Backup schedules, S3 sources, run/restore |
| `temps.crons` | Cron jobs and execution history |
| `temps.deployments` | Deploy, cancel, pause, resume, rollback, teardown, logs |
| `temps.dns` | DNS providers, managed domains, zone listing, A record lookup |
| `temps.domains` | SSL/TLS domains, ACME provisioning, challenge tokens |
| `temps.email` | Email providers, domains, send, stats |
| `temps.externalServices` | PostgreSQL, Redis, S3 service lifecycle, env vars, containers |
| `temps.files` | File retrieval by ID |
| `temps.funnels` | Funnel CRUD, metrics, previews |
| `temps.git` | GitHub/GitLab providers, repo connections, sync |
| `temps.loadBalancer` | Route management |
| `temps.monitoring` | Monitors, incidents, uptime history, status buckets |
| `temps.notifications` | Preferences, providers (email, Slack), test notifications |
| `temps.performance` | Web Vitals metrics, grouped page metrics |
| `temps.platform` | Instance info, IPs, geolocation, activity graph, presets |
| `temps.projects` | Full project lifecycle: environments, domains, env vars, containers, analytics, errors, session replay, webhooks, DSN, IP access |
| `temps.proxyLogs` | Proxy log queries, time bucket stats |
| `temps.repositories` | Synced repos, branches, tags, presets |
| `temps.sessionReplay` | Session init, events, deletion, visitor sessions |
| `temps.settings` | Platform settings |
| `temps.users` | User CRUD, MFA, roles |

### Raw Client Access

For advanced use cases or direct access to the underlying HTTP client:

```typescript
const client = temps.rawClient;
```

### TypeScript

All request/response types are auto-generated from the OpenAPI spec and exported:

```typescript
import type {
  ProjectResponse,
  DeploymentResponse,
  CreateProjectRequest,
} from '@temps-sdk/node-sdk';
```

---

## Error Tracking

A Sentry-compatible error tracking client that captures exceptions, messages, and performance data from your Node.js application and sends them to your Temps instance.

### Setup

```typescript
import { ErrorTracking } from '@temps-sdk/node-sdk';

ErrorTracking.init({
  dsn: 'https://public-key@your-instance.temps.dev/1',
  environment: 'production',
  release: '1.0.0',
});
```

The DSN follows the Sentry format: `https://<public-key>@<host>/<project-id>`. Find it in your project's settings in the Temps dashboard.

### Configuration

```typescript
ErrorTracking.init({
  // Required
  dsn: 'https://key@host/project_id',

  // Environment
  environment: 'production',     // Default: 'production'
  release: '1.2.3',              // Your app version
  serverName: 'api-server-1',    // Server identifier

  // Sampling
  sampleRate: 1.0,               // Error sample rate (0.0 - 1.0, default: 1.0)
  tracesSampleRate: 0.1,         // Performance trace sample rate (0.0 - 1.0)

  // Filtering
  ignoreErrors: [                // Errors to suppress (strings or RegExp)
    'ResizeObserver loop',
    /^NetworkError/,
  ],
  beforeSend: (event) => {       // Modify or drop events before sending
    if (event.tags?.internal) return null; // Return null to drop
    return event;
  },

  // Behavior
  maxBreadcrumbs: 100,           // Max breadcrumbs to retain (default: 100)
  attachStacktrace: true,        // Attach stack traces to messages (default: true)
  debug: false,                  // Log to console instead of sending (default: false)

  // Integrations
  integrations: [],              // Custom integrations (call setupOnce() on init)
});
```

### Capturing Errors

```typescript
// Capture an exception
try {
  riskyOperation();
} catch (error) {
  const eventId = ErrorTracking.captureException(error);
  console.log(`Reported as ${eventId}`);
}

// Capture with extra context
ErrorTracking.captureException(error, {
  tags: { subsystem: 'payments' },
  extra: { orderId: '12345', amount: 99.99 },
  level: 'fatal',
});

// Capture a message
ErrorTracking.captureMessage('Disk usage above 90%', 'warning');

// Capture a raw event
ErrorTracking.captureEvent({
  message: 'Custom event',
  level: 'info',
  tags: { source: 'cron' },
});
```

### Context and Scope

Enrich error reports with user, tags, and structured data:

```typescript
// Set the current user (attached to all subsequent events)
ErrorTracking.setUser({
  id: '123',
  email: 'alice@example.com',
  username: 'alice',
  ip_address: '{{auto}}',
});

// Set global tags
ErrorTracking.setTag('region', 'eu-west-1');
ErrorTracking.setTags({ service: 'api', version: '2.0' });

// Set extra data
ErrorTracking.setExtra('requestId', 'abc-123');
ErrorTracking.setExtras({ query: 'SELECT ...', duration: 150 });

// Set structured context
ErrorTracking.setContext('order', {
  id: 'order-456',
  total: 99.99,
  items: 3,
});

// Clear user on logout
ErrorTracking.setUser(null);
```

### Breadcrumbs

Breadcrumbs leave a trail of events leading up to an error -- invaluable for debugging:

```typescript
ErrorTracking.addBreadcrumb({
  category: 'auth',
  message: 'User logged in',
  level: 'info',
  data: { method: 'oauth' },
});

ErrorTracking.addBreadcrumb({
  category: 'http',
  message: 'GET /api/users',
  level: 'info',
  type: 'http',
  data: { status: 200, duration: 45 },
});

// Clear all breadcrumbs
ErrorTracking.clearBreadcrumbs();
```

| Property | Values |
|----------|--------|
| `type` | `'default'`, `'http'`, `'navigation'`, `'console'` |
| `level` | `'debug'`, `'info'`, `'warning'`, `'error'`, `'critical'` |

### Scoped Context

Isolate context for specific operations so it doesn't leak to other events:

```typescript
// configureScope modifies the current scope permanently
ErrorTracking.configureScope((scope) => {
  scope.setTag('transaction', 'checkout');
  scope.setUser({ id: '123' });
});

// withScope creates a temporary scope -- discarded after the callback
ErrorTracking.withScope((scope) => {
  scope.setTag('component', 'payment-form');
  scope.setExtra('cardLast4', '4242');
  ErrorTracking.captureMessage('Payment processed');
  // These tags/extras don't affect other events
});
```

### Performance Monitoring

Track transactions and spans to measure latency across your stack:

```typescript
// Start a transaction
const transaction = ErrorTracking.startTransaction({
  name: 'POST /api/checkout',
  op: 'http.server',
});

// Create child spans for sub-operations
const dbSpan = transaction.startChild({
  op: 'db.query',
  description: 'SELECT * FROM orders WHERE id = ?',
});
// ... database query ...
dbSpan.finish();

const apiSpan = transaction.startChild({
  op: 'http.client',
  description: 'POST https://payments.example.com/charge',
});
// ... external API call ...
apiSpan.setStatus('ok');
apiSpan.finish();

// Add custom measurements
transaction.setMeasurement('order_total', 99.99, 'usd');
transaction.setMeasurement('items_count', 3, 'count');

// Finish the transaction (sends it to Temps if sampled)
transaction.setStatus('ok');
transaction.finish();
```

Spans can be nested. Each span captures `startTimestamp`, `endTimestamp`, `op`, `description`, `status`, `tags`, and `data`.

### User Feedback

Collect feedback from users after an error occurs:

```typescript
const eventId = ErrorTracking.captureException(error);

// Later, when the user submits a feedback form:
ErrorTracking.captureUserFeedback({
  event_id: eventId,
  name: 'Alice',
  email: 'alice@example.com',
  comments: 'The checkout button did nothing when I clicked it.',
});
```

### Global Error Handlers

The SDK automatically captures unhandled errors at the process level:

| Handler | Level | Behavior |
|---------|-------|----------|
| `process.on('uncaughtException')` | `fatal` | Captured, then `process.exit(1)` |
| `process.on('unhandledRejection')` | `error` | Captured, process continues |

Both are tagged with `handled: false` so you can filter them in the dashboard.

### Shutdown

Flush pending events before process exit:

```typescript
// Flush pending events (waits up to 2s by default)
await ErrorTracking.flush(2000);

// Or close the client entirely (disables further capturing + flushes)
await ErrorTracking.close(2000);
```

### Framework Integrations

#### Express.js

```typescript
import express from 'express';
import { ErrorTracking } from '@temps-sdk/node-sdk';

ErrorTracking.init({ dsn: '...' });

const app = express();

// Add breadcrumbs for each request
app.use((req, res, next) => {
  ErrorTracking.addBreadcrumb({
    category: 'http',
    message: `${req.method} ${req.path}`,
    type: 'http',
    data: { query: req.query },
  });
  next();
});

// Set user context from auth middleware
app.use((req, res, next) => {
  if (req.user) {
    ErrorTracking.setUser({
      id: req.user.id,
      email: req.user.email,
    });
  }
  next();
});

// Error handler (must be last)
app.use((err, req, res, next) => {
  ErrorTracking.captureException(err, {
    extra: {
      method: req.method,
      url: req.originalUrl,
      body: req.body,
    },
  });
  res.status(500).json({ error: 'Internal server error' });
});
```

#### Hono

```typescript
import { Hono } from 'hono';
import { ErrorTracking } from '@temps-sdk/node-sdk';

ErrorTracking.init({ dsn: '...' });

const app = new Hono();

app.onError((err, c) => {
  ErrorTracking.captureException(err, {
    extra: { path: c.req.path, method: c.req.method },
  });
  return c.json({ error: 'Internal server error' }, 500);
});
```

### Debug Mode

Log events to the console instead of sending them. Useful during development:

```typescript
ErrorTracking.init({
  dsn: '...',
  debug: process.env.NODE_ENV !== 'production',
});
```

---

## Complete Example

```typescript
import { TempsClient, ErrorTracking } from '@temps-sdk/node-sdk';

// Initialize error tracking
ErrorTracking.init({
  dsn: 'https://key@your-instance.temps.dev/1',
  environment: process.env.NODE_ENV,
  release: process.env.npm_package_version,
});

// Initialize API client
const temps = new TempsClient({
  baseUrl: 'https://your-instance.temps.dev',
  apiKey: process.env.TEMPS_API_KEY,
});

// Use both together
async function deployProject(projectId: number, branch: string) {
  ErrorTracking.addBreadcrumb({
    category: 'deploy',
    message: `Starting deployment for project ${projectId}`,
    level: 'info',
  });

  try {
    const { data } = await temps.deployments.deploy({
      path: { project_id: projectId },
      body: { branch },
    });
    return data;
  } catch (error) {
    ErrorTracking.captureException(error, {
      tags: { projectId: String(projectId), branch },
    });
    throw error;
  }
}
```

## Requirements

- Node.js 18+ or Bun
- TypeScript 5+ (peer dependency)

## Related

- [`@temps-sdk/kv`](https://www.npmjs.com/package/@temps-sdk/kv) -- Key-value store
- [`@temps-sdk/blob`](https://www.npmjs.com/package/@temps-sdk/blob) -- File storage
- [`@temps-sdk/react-analytics`](https://www.npmjs.com/package/@temps-sdk/react-analytics) -- React analytics, session replay, engagement tracking

## License

MIT
