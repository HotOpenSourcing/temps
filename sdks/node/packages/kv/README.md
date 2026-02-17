<p align="center">
  <img src="https://raw.githubusercontent.com/gotempsh/temps/refs/heads/main/web/public/logo/temps-logo-light.png" alt="Temps Platform" width="700" />
</p>

<h1 align="center">@temps-sdk/kv</h1>

<p align="center">
  <a href="https://www.npmjs.com/package/@temps-sdk/kv"><img src="https://img.shields.io/npm/v/@temps-sdk/kv.svg" alt="npm version" /></a>
  <a href="https://www.npmjs.com/package/@temps-sdk/kv"><img src="https://img.shields.io/npm/dm/@temps-sdk/kv.svg" alt="npm downloads" /></a>
  <a href="https://github.com/AnomalyCo/temps/blob/main/LICENSE"><img src="https://img.shields.io/npm/l/@temps-sdk/kv.svg" alt="license" /></a>
</p>

<p align="center">
  Serverless key-value store for Temps projects. Store and retrieve JSON-serializable data with expiration, atomic increments, and pattern matching -- no infrastructure to manage.
</p>

---

```bash
# npm
npm install @temps-sdk/kv

# bun
bun add @temps-sdk/kv

# pnpm
pnpm add @temps-sdk/kv

# yarn
yarn add @temps-sdk/kv
```

## Quick Start

```typescript
import { kv } from '@temps-sdk/kv';

await kv.set('user:123', { name: 'Alice', plan: 'pro' });

const user = await kv.get<{ name: string; plan: string }>('user:123');
// { name: 'Alice', plan: 'pro' }

await kv.del('user:123');
```

That's it. The `kv` singleton reads `TEMPS_API_URL` and `TEMPS_TOKEN` from your environment automatically.

## Configuration

### Environment Variables

```bash
TEMPS_API_URL=https://your-instance.temps.dev   # Your Temps API URL
TEMPS_TOKEN=your-token                           # API key or deployment token
TEMPS_PROJECT_ID=42                              # Required for API keys, optional for deployment tokens
```

### Explicit Configuration

```typescript
import { createClient } from '@temps-sdk/kv';

const kv = createClient({
  apiUrl: 'https://your-instance.temps.dev',
  token: 'your-token',
  projectId: 42,
});
```

> **Deployment tokens** embed the project ID, so `projectId` is optional. **API keys** require `projectId` to be set explicitly.

## API Reference

### `get<T>(key): Promise<T | null>`

Retrieve a value by key. Returns `null` if the key does not exist.

```typescript
const session = await kv.get<{ userId: string; expiresAt: number }>('session:abc');

if (session) {
  console.log(session.userId);
}
```

### `set(key, value, options?): Promise<'OK' | null>`

Store a JSON-serializable value. Returns `'OK'` on success, `null` if a conditional write failed.

```typescript
// Simple set
await kv.set('config:theme', 'dark');

// Set with expiration (seconds)
await kv.set('cache:homepage', htmlContent, { ex: 300 });

// Set with expiration (milliseconds)
await kv.set('rate:user:123', 1, { px: 60000 });

// Only set if key does NOT exist (create-if-missing)
const result = await kv.set('lock:deploy', '1', { nx: true, ex: 30 });
if (result === null) {
  console.log('Lock already held');
}

// Only set if key already exists (update-if-exists)
await kv.set('user:123:status', 'active', { xx: true });
```

**Options:**

| Option | Type | Description |
|--------|------|-------------|
| `ex` | `number` | Expire after N seconds |
| `px` | `number` | Expire after N milliseconds |
| `nx` | `boolean` | Only set if key does **not** exist |
| `xx` | `boolean` | Only set if key **already** exists |

### `del(...keys): Promise<number>`

Delete one or more keys. Returns the number of keys that were removed.

```typescript
const removed = await kv.del('temp:a', 'temp:b', 'temp:c');
console.log(`Removed ${removed} keys`);
```

### `incr(key): Promise<number>`

Atomically increment a numeric value by 1. If the key does not exist, it is initialized to `0` before incrementing.

```typescript
const views = await kv.incr('page:views:/pricing');
console.log(`Page views: ${views}`);
```

### `expire(key, seconds): Promise<number>`

Set a TTL on an existing key. Returns `1` if the timeout was set, `0` if the key does not exist.

```typescript
await kv.set('session:xyz', data);
await kv.expire('session:xyz', 3600); // Expire in 1 hour
```

### `ttl(key): Promise<number>`

Get the remaining time-to-live of a key in seconds.

| Return value | Meaning |
|---|---|
| `>= 0` | Seconds remaining |
| `-1` | Key exists but has no expiration |
| `-2` | Key does not exist |

```typescript
const remaining = await kv.ttl('session:xyz');

if (remaining === -2) {
  console.log('Session expired or never existed');
} else if (remaining === -1) {
  console.log('Session has no expiration');
} else {
  console.log(`Session expires in ${remaining}s`);
}
```

### `keys(pattern): Promise<string[]>`

Find all keys matching a glob-style pattern.

```typescript
const userKeys = await kv.keys('user:*');
// ['user:123', 'user:456', 'user:789']

const sessionKeys = await kv.keys('session:*');
```

> Use `keys` for debugging and administration. In production, prefer direct key access for performance.

## Usage Patterns

### Caching

```typescript
async function getProduct(id: string) {
  const cached = await kv.get<Product>(`product:${id}`);
  if (cached) return cached;

  const product = await db.products.findById(id);
  await kv.set(`product:${id}`, product, { ex: 600 }); // Cache for 10 minutes
  return product;
}
```

### Rate Limiting

```typescript
async function checkRateLimit(userId: string): Promise<boolean> {
  const key = `ratelimit:${userId}`;
  const count = await kv.incr(key);

  if (count === 1) {
    await kv.expire(key, 60); // 60-second window
  }

  return count <= 100; // 100 requests per minute
}
```

### Distributed Locks

```typescript
async function withLock<T>(name: string, fn: () => Promise<T>): Promise<T | null> {
  const acquired = await kv.set(`lock:${name}`, Date.now(), { nx: true, ex: 30 });

  if (acquired === null) return null; // Lock held by another process

  try {
    return await fn();
  } finally {
    await kv.del(`lock:${name}`);
  }
}
```

### Feature Flags

```typescript
await kv.set('feature:dark-mode', { enabled: true, rollout: 0.5 });

const flag = await kv.get<{ enabled: boolean; rollout: number }>('feature:dark-mode');
if (flag?.enabled && Math.random() < flag.rollout) {
  // Show dark mode
}
```

## Multiple Clients

Create separate instances for different use cases:

```typescript
import { KV } from '@temps-sdk/kv';

const cache = new KV({ apiUrl: '...', token: '...' });
const sessions = new KV({ apiUrl: '...', token: '...' });

await cache.set('page:home', html, { ex: 300 });
await sessions.set('sess:abc', userData, { ex: 86400 });
```

## Error Handling

All errors are instances of `KVError` with structured details:

```typescript
import { kv, KVError } from '@temps-sdk/kv';

try {
  await kv.get('my-key');
} catch (error) {
  if (error instanceof KVError) {
    console.error(error.message);  // Human-readable message
    console.error(error.code);     // Error code (see table below)
    console.error(error.status);   // HTTP status code (if applicable)
    console.error(error.title);    // RFC 7807 problem title
    console.error(error.detail);   // RFC 7807 problem detail
  }
}
```

**Error codes:**

| Code | Description |
|------|-------------|
| `MISSING_CONFIG` | Required environment variable or config option not set |
| `NETWORK_ERROR` | Failed to reach the Temps API |
| *HTTP status codes* | API-level errors with RFC 7807 Problem Details |

## TypeScript

Full type safety out of the box. Generic `get<T>()` returns correctly typed values:

```typescript
interface UserSession {
  userId: string;
  role: 'admin' | 'member';
  expiresAt: number;
}

const session = await kv.get<UserSession>('session:abc');
//    ^? UserSession | null

if (session) {
  session.role; // 'admin' | 'member' -- fully typed
}
```

## Requirements

- Node.js 18+ or Bun
- A running Temps instance

## Related

- [`@temps-sdk/blob`](https://www.npmjs.com/package/@temps-sdk/blob) -- File storage
- [`@temps-sdk/react-analytics`](https://www.npmjs.com/package/@temps-sdk/react-analytics) -- React analytics, session replay, error tracking
- [`@temps-sdk/node-sdk`](https://www.npmjs.com/package/@temps-sdk/node-sdk) -- Full platform API client and server-side error tracking

## License

MIT
