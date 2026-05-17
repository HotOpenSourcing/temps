# Acceptance Tests

Executable release E2E suite. Each scenario YAML mirrors a section of
[`docs/release-testing.md`](../docs/release-testing.md) but runs end-to-end:
deterministic steps drive the browser / database / docker, and Claude
judges the parts that exact-match assertions can't capture (screenshots,
copy, error messages).

## Why both deterministic + Claude?

- **Pure Playwright** can't judge "does this *look* right" or follow a chain
  of reasoning across screenshots. Brittle selectors → false reds.
- **Pure Claude-in-a-loop** is slow and expensive for the 80% of steps that
  are just "click X, expect 200".
- **Hybrid** runs deterministic steps fast (cheap, repeatable) and dispatches
  to Claude only for the judgment calls (visual, copy, exploratory).

## Layout

```
acceptance-tests/
├── runner/
│   ├── run.ts          # orchestrator
│   ├── verifier.ts     # Claude-as-judge
│   ├── tools.ts        # browser/docker/sql/cli wrappers
│   └── report.ts       # writes results/<ts>/report.md
├── scenarios/
│   ├── 01-onboarding/
│   ├── 03-storage/
│   ├── 05-domains-tls/
│   └── ...
├── fixtures/           # sample apps, seed data, etc.
└── results/            # gitignored; one folder per run
```

## Running

```bash
cd acceptance-tests
bun install                                # one-time

# Run everything against a local control plane
bun run run

# Run a single section
bun run run --scenarios 03-storage

# Run a single scenario
bun run run --scenarios 03-storage/3.2-resource-limits-apply.yaml

# Point at a different env
bun run run --against https://staging.temps.sh
```

### Required env

```
ANTHROPIC_API_KEY=sk-ant-...           # for the Claude-judge steps
TEMPS_BASE_URL=http://localhost:3000   # default; CLI flag overrides
TEMPS_ADMIN_EMAIL=admin@temps.sh
TEMPS_ADMIN_PASSWORD=...               # for the runner's auth bootstrap
DATABASE_URL=postgres://...            # for `sql:` verify steps
```

The runner uses prompt caching on the system prompt + tool defs so a typical
release run (~60 scenarios, ~240 Claude verifications) costs **~$3** rather
than ~$30.

## Writing a scenario

```yaml
id: 3.2
title: Apply CPU + memory cap on a running container
section: 03-storage
docs: docs/release-testing.md#32-resource-limits--apply-cpu--memory-cap-on-a-running-container

prerequisites:
  - kind: postgres_service
    name: pg-acceptance
    status: running

steps:
  - action: ui_navigate
    url: /storage/{{service.id}}
  - action: ui_click
    selector: "button[data-testid='edit-limits']"

verify:
  - kind: shell
    cmd: docker inspect temps-postgres-{{service.name}} --format '{{.HostConfig.Memory}}'
    expect_exact: "536870912"
  - kind: claude_assert
    prompt: |
      Look at the toast at the top right. Does it say roughly
      "Resource limits applied" with a count of "1 live"?
      Pass if yes; fail with the actual text otherwise.
```

Step types (`action`):

- `ui_navigate`, `ui_click`, `ui_fill`, `ui_toggle`, `ui_select`,
  `ui_screenshot`, `ui_wait_for`
- `shell` — run a command, capture stdout
- `sql` — run a SQL query, capture rows
- `sleep` — wait N ms (use sparingly)
- `cli` — run `bunx @temps-sdk/cli ...`

Verify types (`kind`):

- `exact` / `regex` / `json_path` — deterministic assertions on the last
  step's output
- `shell` — run a command and pass/fail on its exit + stdout
- `claude_assert` — hand a screenshot + question to Claude, get pass/fail
- `claude_explore` — give Claude a goal and let it drive (used sparingly,
  for security and edge-case scenarios)

## Writing prompts for the Claude judge

Two rules:

1. **Be specific about pass criteria.** "Does it look right?" is not a
   prompt. "Does the toast contain the words 'Resource limits applied' and
   a count of '1 live'?" is.
2. **Always allow Claude to explain why it failed.** The runner records the
   reason next to the FAIL marker so debugging doesn't require a re-run.

## Calibrating the judge

Before adding a new `claude_assert` to the suite, run it against a
known-good build *and* a known-broken build. If the judge gives the same
verdict on both, the prompt is too loose — tighten it.

## Adding scenarios

When a regression bites you in production, **add a scenario in the same PR
that fixes it.** The suite grows with every "we should have caught this"
lesson.
