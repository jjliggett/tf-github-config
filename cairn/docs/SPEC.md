# Cairn: specification

| | |
|---|---|
| Status | Draft 0.1, 2026-11-04 |
| Scope | Server (`cairnd`), CLI (`cairn`), MCP server, remote runner |
| Language | Rust (server, runner, CLI). The OpenTofu provider for configuring Cairn is the one Go component (see §6.8). |
| Name | "Cairn" is a working name: a stack of stones that marks a trail. Rename freely. |
| Inputs | [Benchmark report](benchmark-report.md) for OTF 0.6.3, Terrakube 2.33.3 and Atlantis 0.49.0, the pinned sources in `../upstream/`, and the Spacelift/spacectl feature set |

## 1. Summary

Cairn is a self-hosted OpenTofu automation server that one person can run on a
Raspberry Pi next to Forgejo, and that a small team can grow into. It has three parts:

- **`cairnd`**, one static binary that serves the API, the web UI, webhooks, the
  MCP endpoint and the module registry, and runs OpenTofu itself.
- **`cairn`**, a CLI with the reach of `spacectl`. `cairn mcp serve` also makes it
  a local MCP server.
- **`cairnd runner`**, the same server binary in runner mode, for running plans
  on other machines (for example mediabox). It only makes outbound connections.

It combines:

- **OTF's footprint and compatibility**: a single process, Postgres or SQLite, and
  the TFE API, so `tofu`'s `cloud {}` block and the `tfe` provider work unchanged.
- **Terrakube's breadth**: OPA policies, drift schedules, dynamic credentials
  including Vault/OpenBao, a private registry, and group-based RBAC from the IdP.
- **Atlantis's PR workflow**: plans posted to the PR, comment commands, and
  locks. Cairn fixes Atlantis's Gitea/Forgejo lock leak.
- **Spacelift's model and ergonomics**: stacks, spaces, contexts, stack
  dependencies that pass outputs, policies at named decision points, proposed and
  tracked runs, local previews, and a scriptable CLI.

The benchmarks set the bar. A plan+apply of 100 resources takes OpenTofu about
0.15 s. OTF finishes in about 0.5 s and Terrakube in about 27 s, because of fixed
sleeps in its executor. At idle, Terrakube's stack uses roughly 40 times OTF's
memory. Cairn's budgets are in §9.

## 2. Goals and non-goals

### Goals

1. **Fit on a Pi 4 (4 GB) that is already busy.** One process, ≤ 25 MiB idle RSS
   for `cairnd`, no JVM, no Redis, and no separate IdP proxy. SQLite by default,
   and your existing Postgres when you want it.
2. **Be a drop-in TFC/TFE target for the OpenTofu CLI.** `tofu login`, the
   `cloud {}` block (remote and local execution), state versions, outputs and
   the core of the `tfe` provider all work. This repository's own `terraform.tf`
   should only need a new `hostname`.
3. **Treat Forgejo/Gitea as a first-class VCS**, alongside GitHub and GitLab:
   webhooks, commit statuses, a summary comment that updates in place, comment
   commands, and lock handling that is correct for closed and merged PRs.
4. **Match Spacelift's working model** (stacks, spaces, contexts, dependencies,
   policies, drift detection, schedules, runner pools, a module registry) closely
   enough that a Spacelift user feels at home.
5. **Provide a CLI as good as `spacectl`**, scriptable with JSON output, exit
   codes and wait/follow modes, and git-aware: it infers the stack from the
   working directory.
6. **Be agent-native.** A well-designed MCP server (stdio and Streamable HTTP),
   tools with safety annotations, human approval for anything that changes
   infrastructure, structured plan summaries rather than raw logs, and an event
   stream.
7. **Make runs fast**: event-driven scheduling with no fixed sleeps or polling
   loops, so orchestration adds ≤ 300 ms to a run on x86 and ≤ 1 s on a Pi.

### Non-goals (v1)

- Kubernetes-native execution (a k8s executor can come later behind the same
  runner interface).
- Running Pulumi, CloudFormation, Ansible or Terragrunt. The engine interface
  allows Terraform as well as OpenTofu, and nothing else is planned.
- Sentinel (Rego covers the use cases; Sentinel is proprietary).
- Multi-region SaaS scale. The target is up to about 1,000 stacks and 50
  concurrent runs on one Postgres-backed node.
- A heavy single-page-app UI.

## 3. Requirements traced to the measurements

Each row records something observed in the benchmark, or read in the pinned
source, and the requirement Cairn takes from it.

| # | Observation | Requirement for Cairn |
|---|---|---|
| R1 | Terrakube needs 4 app containers plus Dex, Valkey and Postgres (and MinIO and Traefik in the reference Compose file). That is about 1.5 GiB of compressed arm64 images and ~25 s for the API to be ready. | Single binary, at most 1 external dependency (Postgres, optional). Ready in ≤ 200 ms on x86 and ≤ 1 s on a Pi 4. Image ≤ 20 MiB compressed, excluding OpenTofu. |
| R2 | Terrakube's executor sleeps for fixed periods (`Thread.sleep(5000)` ×2, `sleep(2000)`, ~3 s waits for the log stream) and uses Quartz polling. A 0.15 s tofu run takes ~27 s. | **No fixed sleeps anywhere on the run path.** Phases advance on events (child-process exit, notifications). Log completion is signalled by EOF, not timeouts. |
| R3 | The Terrakube executor processes one job at a time (`setCorePoolSize(1)`), so 5 runs at once take 5 × 27 s. | Runner concurrency is configurable; the default is the number of CPUs. Runs of different stacks never block each other. |
| R4 | Listing 50 workspaces in OTF takes ~20 ms unloaded and saturates around 130 req/s on 4 cores (one workspace read: ~4 ms). | List endpoints use one query per page, with no N+1 permission or "latest run" lookups. Targets: p50 ≤ 2 ms and ≥ 5,000 req/s at 16 clients on the bench host. |
| R5 | Terrakube's workspace list cannot be paginated ("Cannot paginate workspace"); its UI fetches everything. | Every list endpoint is paginated (keyset cursor, `limit` ≤ 200). |
| R6 | Atlantis's Gitea parser treats a PR closed without merging as "other", so its locks are never released (`ParseGiteaPullRequestEvent`). | PR locks are released on close *and* merge, have an optional TTL, and can be listed and released from the CLI, UI and MCP. Contract tests cover each VCS's close and merge payloads. |
| R7 | Atlantis stores no state, has no UI login, and cannot run plans remotely. | Built-in versioned state (HTTP and TFE backends), OIDC login, and remote runners. |
| R8 | OTF has no policies, drift schedules or Vault credentials. Terrakube has all three. | Rego policies (§5.5), cron-based drift detection (§5.7), and an OIDC issuer for workload identity plus a native OpenBao/Vault JWT login (§7.4). |
| R9 | Terrakube's executor clones its extensions repository from GitHub on **every** job. | No per-run network fetches that aren't part of the user's config. Engines, git mirrors and the provider cache are cached and checksum-verified. |
| R10 | OTF's otfd idles at ~24 MiB; Atlantis's Go binary at about the same. | Budget: `cairnd` ≤ 25 MiB idle on aarch64 with SQLite. Each active run adds ≤ 5 MiB to `cairnd`; the tofu child process is counted separately. |

## 4. Concepts

The domain follows Spacelift's vocabulary. Every concept also maps to TFE so
OpenTofu's `cloud {}` block and the `tfe` provider keep working.

| Cairn | What it is | TFE mapping | Spacelift equivalent |
|---|---|---|---|
| **Space** | A tree of folders that sets RBAC boundaries and inheritance (contexts and policies attached to a space apply to everything below it). The root space is `root`. | Organization (top-level space) and Project (child space) | Space |
| **Stack** | One root module in one state, from a repo, branch and path, or uploaded from the CLI. | Workspace | Stack |
| **Run** | One execution against a stack (§4.1). | Run | Run |
| **Context** | A reusable set of environment variables, mounted files and hooks. It can be attached explicitly or automatically by label (`autoattach:<label>`). | Variable set | Context |
| **Policy** | A Rego module attached to a decision point (§5.5). | Policy set and policy | Policy |
| **Dependency** | An edge from stack A to stack B, optionally mapping an A output to a B input. A tracked run of A that changes the mapped outputs triggers B. | Run trigger | Stack dependency |
| **Schedule** | A cron job on a stack: drift detection, a task, or a delete (TTL). | n/a | Scheduling |
| **Runner pool** | A named set of runners. The embedded pool `local` always exists. | Agent pool | Worker pool |
| **Module / provider** | Private registry entries (§6.7). | Registry module / provider | Module registry |
| **Integration** | A cloud or secret-backend trust config: AWS, GCP or Azure workload identity, or an OpenBao/Vault JWT role. | Dynamic credentials | Cloud integration |
| **Blueprint** | A template that creates a stack and its contexts and policies from a form. *Phase 3.* | No-code module | Blueprint |

### 4.1 Runs

| Type | Created by | Changes infra | Blocks other runs | Notes |
|---|---|---|---|---|
| `proposed` | PR or MR, `cairn stack preview`, MCP `trigger_preview` | No | No (runs concurrently, read-only state lock) | Speculative plan. Results go to the PR. |
| `local-preview` | `cairn stack preview --local` | No | No | The CLI uploads the working tree (respecting `.gitignore` and `.cairnignore`), like `spacectl stack local-preview`. |
| `tracked` | Push to the tracked branch, `cairn stack deploy`, a dependency trigger, or the API | Yes | Yes (workspace queue) | Plan → (policy) → approval or auto-apply → apply. |
| `drift` | Schedule | No (unless reconcile is on) | No | Refresh-only plan. A non-empty diff marks the stack `drifted` and can start a tracked run. |
| `task` | `cairn stack task -- <cmd>` | Maybe | Yes | Runs any command in the prepared workspace, e.g. `tofu state mv` or `tofu import`. |
| `destroy` | `cairn stack destroy`, a TTL schedule | Yes | Yes | Needs `admin` on the stack, plus the approval policy. |
| `test` | Module push or tag | No | No | `tofu test` for registry modules. |

The tracked-run state machine is compatible with TFE's run states, so the
`cloud {}` block can read them:

```
queued ─▶ preparing ─▶ planning ─▶ planned ──▶ policy_checked ──▶ unconfirmed ──▶ confirmed ─▶ applying ─▶ applied
   │          │            │          │  (no changes)                 │  (discard)                  │
   └──────────┴────────────┴──────────┴──▶ planned_and_finished       ├──▶ discarded               └──▶ errored
                                     any state ──▶ canceled / errored   └──▶ (policy deny) ─▶ errored
```

The scheduler keeps two queues, like OTF (see `upstream/otf/ALGORITHMS.md`):

- a **per-stack queue**: at most one tracked, task or destroy run is active per
  stack, and later runs wait;
- a **global phase queue** that runners take from: applies first, then tracked
  plans, then proposed plans.

Users can raise or lower a run's priority (`cairn run prioritize`).

## 5. Behaviour

### 5.1 Run execution pipeline

These steps are the same for the embedded runner and remote runners:

1. **Claim.** The runner takes a phase from the queue. Embedded runners are
   woken by an in-process `Notify`. Remote runners use a long-poll request
   that the server holds open until work arrives (≤ 25 s, then the runner
   re-polls). Claims are leases with a heartbeat.
2. **Source.** Fetch the commit into a per-repository bare mirror cached on the
   runner (`gix`, shallow fetch of the one commit), then check it out as a
   worktree. A `local-preview` run unpacks the uploaded tarball instead.
3. **Prepare.**
   - Resolve the engine version from the stack setting, then the
     `required_version` constraint, then the default.
   - Use the cached engine binary, or download it once and verify its SHA256
     sums and signature.
   - Write the backend override: an `http` backend pointing at
     `/api/v1/stacks/{id}/state` with a run-scoped token and lock and unlock
     URLs.
   - Mount context files, export variables, and mint workload-identity tokens
     (§7.4).
   - Point `TF_PLUGIN_CACHE_DIR` at a shared cache, enabling `-lockfile=readonly`
     where a lock file exists.
4. **Hooks.** Run `before_init`, `after_init`, `before_plan`, `after_plan`,
   `before_apply`, `after_apply`, `before_destroy` and `after_destroy` from
   contexts and from the stack (the Spacelift model). Hooks are shell commands
   in the same sandbox.
5. **Plan.** Run `tofu init -input=false` and then
   `tofu plan -input=false -out=plan.bin -json`. The JSON event stream gives a
   live structured log. `tofu show -json plan.bin` gives the plan document,
   which is stored compressed, along with a summary: counts by action, the list
   of changed resources, drifted resources and replacements.
6. **Policy.** Evaluate `plan` policies (§5.5) against the plan document plus
   run, stack and user metadata. Then evaluate `approval` policies.
7. **Confirm.** Auto-apply if allowed. Otherwise the run waits in
   `unconfirmed`. Confirmation comes from the UI, CLI, PR comment or MCP
   (with human elicitation, §6.5).
8. **Apply.** Run `tofu apply -json plan.bin` on the **same runner** when it is
   still warm (the workspace is kept for up to 1 h). Otherwise re-prepare and
   verify that the plan's `configuration` and `prior_state` hashes still match.
9. **Finish.**
   - Upload the new state version through the HTTP backend; the server
     encrypts it at rest.
   - Capture `tofu output -json` (non-sensitive outputs in clear; sensitive
     ones encrypted) so dependencies work even when the state is encrypted
     client-side.
   - Evaluate `trigger` policies and dependency triggers, then send
     notifications.

Every transition is one database transaction that emits one event onto the
event bus. The UI, CLI `--follow`, SSE subscribers, VCS status updates and MCP
resource subscriptions all read from that one stream.

### 5.2 Sandboxing and limits

Each phase runs as a child process group with:

- a wall-clock timeout per phase (default 2 h plan, 24 h apply), with SIGINT
  then SIGKILL;
- an optional memory and CPU cap. On Linux with cgroup v2 this is a per-run
  sub-cgroup (`memory.max`, `cpu.max`); where that isn't available the cap is
  skipped and a warning is logged;
- filesystem confinement on Linux via Landlock (read-only system, read-write
  workspace and caches) when the kernel supports it, otherwise none;
- an optional container mode for remote runners: `--isolation=podman|docker`
  runs each phase in a throwaway container with the workspace bind-mounted.

### 5.3 State

- **Storage.** Versioned state per stack, stored as blobs (filesystem or any
  `object_store` backend: S3/MinIO/Garage, GCS, Azure). Metadata (serial,
  lineage, checksum, resource count, size, run id, creator) is in the database.
  State is envelope-encrypted at rest (§7.3).
- **Locking.** One lock per stack, held by a run, or by a user for
  local-execution `cloud {}` workspaces. Locks show who holds them and why, and
  can be force-released by an admin.
- **Interfaces.**
  - The **HTTP backend** (`GET`/`POST`/`LOCK`/`UNLOCK`) is for runs and for
    users who prefer `backend "http"`.
  - The **TFE state-versions API** is for the `cloud {}` block.
  - `cairn stack state pull | push | list | show | rollback <serial>` covers
    the CLI.
- **Rollback.** Creates a new version whose content is an old serial. History
  is never rewritten.
- **OpenTofu client-side state encryption** passes through: Cairn stores the
  ciphertext. The resource browser and drift summaries then fall back to
  run-time data (plan JSON and captured outputs), and the UI says so.

### 5.4 VCS integration

Providers: **Forgejo/Gitea** (tested against Forgejo 11+ and Gitea 1.24+),
GitHub (as a GitHub App), and GitLab.

| Event | Behaviour |
|---|---|
| Push to a stack's tracked branch | Changed files are matched against the stack's `project_root` plus extra `trigger_paths` globs (and `push` policies, if any). Each match queues a tracked run. |
| PR or MR opened, or new commits pushed | The same matching queues a `proposed` run per affected stack (a newer commit cancels the older proposed run). A commit status `cairn/<space>/<stack>` is set per stack. **One summary comment** per PR lists every affected stack with its plan counts, policy verdicts and a link. It is edited in place, never appended. |
| PR comment `/cairn plan [stack…]`, `/cairn apply [stack…]`, `/cairn unlock` | Atlantis-style commands, available when the space enables `pr_apply`. Each command checks that the commenter is mapped to a Cairn user with the right role (§7.2). |
| PR **merged** | Proposed-run locks are released. Tracked runs start from the push event. |
| PR **closed without merge** | Locks are released and pending proposed runs are canceled. This is the Atlantis bug from R6. |
| PR reopened | Treated as opened. |

Lock semantics come in two modes, chosen per space:

- **`apply-after-merge`** (default, Spacelift-like): proposed runs take no
  write locks.
- **`apply-before-merge`** (Atlantis-like): the first PR to plan a stack holds
  its lock until the PR is merged or closed, or until a TTL expires (default
  7 days).

Webhook payloads are verified with the HMAC secret for that provider. Each
delivery is stored for 7 days, can be replayed with
`cairn admin webhook redeliver`, and is deduplicated by delivery ID.

Polling is a fallback for networks that can't receive webhooks. It works like
Terrakube's Azure DevOps poller and is configured per VCS integration with a
minimum interval of 60 s.

### 5.5 Policies (Rego)

Policies are evaluated in-process with `regorus`, Microsoft's Rust Rego
interpreter, so no OPA sidecar is needed. They are evaluated at these decision
points:

| Kind | Input | Rules it can emit | Default |
|---|---|---|---|
| `login` | OIDC claims, IP, time | `allow`, `deny`, `role[space] = "reader"…` | Group mapping only (§7.2) |
| `access` | user, space, stack, action | `allow`, `deny` | RBAC only |
| `push` | VCS event, changed files, stack | `track`, `propose`, `ignore`, `cancel` | Path matching (§5.4) |
| `plan` | plan JSON, summary, run, stack, user | `deny[msg]` (hard), `warn[msg]` (advisory) | none |
| `approval` | run, plan summary, approvals so far | `approve`, `reject`, `required_approvals = n` | One approval for tracked runs without auto-apply |
| `trigger` | finished run, its outputs, the dependency graph | `trigger[stack_id]` | Dependency edges (§4) |
| `notification` | run event | `route[target]`, `message` | All configured targets |
| `agent` | MCP client info, tool, arguments, principal | `allow`, `require_human`, `deny` | Agents may read; anything that changes state needs a human (§6.5) |

Enforcement levels are **advisory** (`warn`), **soft-mandatory** (a `deny` that
a user with `override` permission can override, with an audit entry) and
**hard-mandatory**.

Policies:

- are attached to spaces (inherited) or to stacks, or attached by label;
- can be **tested locally** with `cairn policy test` against saved inputs
  (`cairn run show --policy-input`) and with OPA-style `_test.rego` files;
- can come **from a git repo**, synced as a "policy stack" with its own
  proposed and tracked runs, so policies themselves get reviewed in PRs;
- have every evaluation stored (input hash, result, duration) and visible on
  the run.

### 5.6 Dependencies and outputs

- Stack dependencies form a DAG; cycles are rejected when an edge is created.
- An edge can map outputs: `A.outputs.vpc_id → B.vars.vpc_id`. When a tracked
  run of A finishes and a mapped output changed, B gets a tracked run, after
  `trigger` policies are applied.
- `cairn stack deps graph` prints the DAG (text or DOT).
- `cairn stack deploy --with-dependents` runs a subgraph in topological order.
  Stacks at the same depth run in parallel.

### 5.7 Drift detection and schedules

- Cron schedules (`croner` syntax) per stack: `drift` (with an optional
  `reconcile: true` that queues a tracked run when drift is found), `task`
  (any command), and `delete` (destroy at a given time, for ephemeral
  environments).
- Each stack shows its drift status and the last time it was checked. A
  space-level drift report is available in the UI, through
  `cairn drift report` and through MCP `list_drift`.
- Schedules are jittered within ±10 % of their interval so 200 stacks with
  `@daily` don't all start at midnight.

## 6. Interfaces

All interfaces are served by `cairnd` on one port (TLS optional; TLS is usually
terminated by Caddy or Traefik). Routes:

| Path | What |
|---|---|
| `/` | Web UI (§6.6) |
| `/api/v1/…` | Native REST API (§6.2) |
| `/api/v2/…`, `/.well-known/terraform.json` | TFE-compatible API (§6.1) |
| `/api/v1/events` | Server-Sent Events stream (filters: space, stack, run, kind) |
| `/hooks/{forgejo,gitea,github,gitlab}/{integration_id}` | VCS webhooks |
| `/mcp` | MCP Streamable HTTP endpoint (§6.5) |
| `/v1/modules/…`, `/v1/providers/…` | Registry protocols (§6.7) |
| `/.well-known/openid-configuration`, `/oidc/jwks` | Cairn as a workload-identity issuer (§7.4) |
| `/metrics`, `/healthz`, `/readyz` | Prometheus metrics and probes |
| `/openapi.json`, `/llms.txt` | Machine-readable API description and an LLM-oriented index of the docs |

### 6.1 TFE-compatible API (subset)

The goal is that unmodified `tofu` (1.6 and later) and the `tfe` provider work.
The implementation follows OTF's `upstream/otf/docs/docs/tfe_api.md`, and
compatibility is verified by running the real CLI in CI (§12.3).

| Area | Endpoints | Phase |
|---|---|---|
| Discovery and auth | `/.well-known/terraform.json` (`tfe.v2`, `tfe.v2.1`, `tfe.v2.2`, `modules.v1`, `providers.v1`, `login.v1`), `GET /api/v2/ping`, `GET /api/v2/account/details`, `login.v1` OAuth authorization-code + PKCE flow for `tofu login` | 1 |
| Organizations | `GET /organizations/:org`, `GET /organizations/:org/entitlement-set` | 1 |
| Workspaces | list (with `search[name]` and `search[tags]`), show by name or id, create, update, delete, lock/unlock/force-unlock, tags | 1 |
| Config versions | create, upload (signed URL), show, download | 1 |
| Runs | create, show, list per workspace, apply, discard, cancel, force-cancel; plan and apply resources and their logs (chunked log endpoint); `policy-checks`, `task-stages` and `cost-estimates` returned empty but valid | 1 |
| State versions | current, create (with `json-state-upload-url`), list, download, `current-state-version-outputs` | 1 |
| Variables | workspace vars CRUD, variable sets (mapped to contexts) | 1 |
| Teams and tokens | teams, team membership, team tokens, user tokens (enough for the `tfe` provider) | 2 |
| VCS | `oauth-clients` and `oauth-tokens` (mapped to VCS integrations) | 2 |
| Policies | `policy-sets` (Rego only), `policy-checks` | 3 |
| Agents | `agent-pools`, `agent-tokens` (mapped to runner pools) | 3 |

Everything is JSON:API, as TFE requires, and every list endpoint is paginated
(R5).

### 6.2 Native REST API

The native API is a resource-oriented JSON API, more convenient than JSON:API,
and the one the CLI, UI and MCP all use.

- **OpenAPI 3.1**, generated from the Rust types with `utoipa` and served at
  `/openapi.json`. Clients for other languages are generated from it.
- **Pagination**: keyset cursors (`?limit=50&after=<cursor>`, `next` link in
  the body and the `Link` header). List queries are one SQL query each (R4).
- **Filtering**: `?label=env:prod&state=drifted&q=net`, plus `fields=` to trim
  responses.
- **ETags and `If-Match`** on mutable resources, so concurrent edits fail
  cleanly instead of overwriting each other.
- **Idempotency keys** (`Idempotency-Key` header) on every POST that creates a
  run, so retries from scripts and agents don't queue duplicate runs.
- **Errors**: `application/problem+json` (RFC 9457) with a stable `type` URI
  per error class and a remedy hint (for example "stack is locked by run X;
  `cairn stack unlock` or wait").
- **Long operations**: return `202` with the run resource. Clients follow up
  with `GET /runs/{id}?wait=30s` (server-side long-poll) or the SSE stream.
  Clients never need to busy-poll.

### 6.3 CLI: `cairn`

Design rules:

- One static binary (`x86_64`/`aarch64` Linux and macOS, plus Windows).
- Every command accepts `-o table|json|yaml|name`. JSON output has a published
  JSON Schema and is stable within a major version.
- When stdout isn't a TTY, prompts are off, colours are off and JSON is the
  default.
- Exit codes: `0` success, `1` error, `2` usage, `3` policy denied, `4` plan has
  changes (with `--detailed-exitcode`, as in `tofu plan`), `5` timeout, `6`
  needs approval.
- **Repo-aware.** Inside a git checkout, `--stack` defaults to the stack whose
  repo, branch and `project_root` match the current directory, as with
  spacectl's `--id` inference. If several stacks match, the CLI lists them.
- **Profiles.** `cairn profile login <name> --host …` uses the OAuth 2.0 device
  flow against Cairn, which delegates to Authentik. Tokens go in the OS keyring,
  falling back to a `0600` file. `CAIRN_TOKEN` and `CAIRN_HOST` override for CI.
- `--wait` and `--follow` are available on anything that creates a run.
- Shell completion for bash, zsh, fish and PowerShell, including dynamic stack
  and run IDs.

The command tree, with the spacectl command each one matches:

| `cairn` | spacectl | Notes |
|---|---|---|
| `login`, `logout`, `whoami`, `profile list/use/current/export-token` | `profile …`, `whoami` | |
| `stack list/show/open/create/edit/delete` | `stack list/show/open` | `open` launches the browser |
| `stack preview [--local] [--target addr]… [--var k=v]` | `stack preview`, `stack local-preview` | `--local` uploads the working tree |
| `stack deploy [--sha] [--with-dependents] [--auto-confirm]` | `stack deploy` | `--auto-confirm` still goes through the approval policy |
| `stack confirm/discard/approve/reject/cancel/retry/replan --target` | same | |
| `stack logs [--run] [--follow] [--phase plan\|apply]` | `stack logs` | Streams over SSE |
| `stack changes [--run] [-o json]` | `stack changes` | Structured plan summary |
| `stack outputs [--sensitive]`, `stack resources` | `stack outputs`, `stack resources` | |
| `stack state pull/push/list/show/rollback` | n/a | |
| `stack env list/set/unset [--secret] [--file]` | `stack environment` | |
| `stack lock/unlock [--note]`, `stack enable/disable` | `stack lock/unlock`, `stack enable/disable` | |
| `stack set-current-commit`, `stack sync-commit` | same | |
| `stack deps list/add/remove/graph` | `stack dependencies` | |
| `stack task -- <cmd>` | `stack task` | |
| `stack drift status/detect` | n/a | |
| `run list/show/watch/prioritize/deprioritize` | `stack run-metadata`, `stack prioritize-run` | |
| `context list/show/create/attach/detach/env/file` | `context …` | |
| `policy list/show/create/edit/test/eval/samples` | `policy …` | `test` runs locally with `regorus` |
| `space list/show/create` | `space …` | |
| `module list/show/versions/publish`, `provider list/show/publish` | `module …`, `provider …` | |
| `runner-pool list/create/token`, `runner list/drain` | `workerpool …` | |
| `lock list/release` | n/a | Covers both stack locks and PR locks (R6) |
| `drift report` | n/a | |
| `audit tail [--follow]` | `audit-trail` | |
| `api <METHOD> <path> [-f k=v]` | `graphql` (raw) | Like `gh api` |
| `mcp serve [--read-only] [--space …]`, `mcp install <client>` | `mcp server` | `install` writes MCP client config for Claude Code, Cursor and others |
| `completion`, `version`, `docs [--format md]` | same | |

### 6.4 Agent-native design (beyond MCP)

- `/llms.txt` and `cairn docs --format md` give the full docs as plain markdown
  for coding agents.
- The repository ships an **agent skill** (`skills/cairn/SKILL.md`) that teaches
  the preview → review → deploy workflow and the CLI's JSON contracts.
  spacectl ships similar skills in its repo.
- Structured artifacts are first-class API resources:
  - `GET /runs/{id}/plan/summary`: counts, changed addresses, replacements and
    drift.
  - `GET /runs/{id}/plan.json`: the full plan document, size-capped and paged.
  - `GET /runs/{id}/diagnostics`: tofu's JSON diagnostics with file and line,
    so agents don't have to scrape logs.
- Every response that mentions a run includes `next_actions`: the operations
  that are currently valid for the caller (for example `["confirm", "discard"]`)
  and the CLI or MCP call for each.
- Secrets never appear in logs or API responses. Values marked sensitive by
  tofu, context secrets, and token-shaped strings are redacted before
  persistence.

### 6.5 MCP server

Cairn implements MCP spec revision **2025-11-25** with the official Rust SDK
(`rmcp`). It degrades gracefully for clients that only support 2025-06-18.

**Transports**

| Transport | Started by | Auth |
|---|---|---|
| stdio | `cairn mcp serve` (local, uses the CLI profile) | The profile's token |
| Streamable HTTP | `cairnd` at `/mcp` | OAuth 2.1 per the MCP authorization spec. `cairnd` publishes Protected Resource Metadata (RFC 9728) and acts as the authorization server, delegating login to the configured OIDC IdP (Authentik). It supports Client ID Metadata Documents and dynamic client registration, and binds tokens to the `/mcp` resource (RFC 8707). |

**Principals and scopes.** An MCP session acts as a user, never as an
anonymous service. Each token carries scopes:

- `cairn:read` (default),
- `cairn:preview` (create proposed and local-preview runs),
- `cairn:write` (deploy, confirm, discard, change variables and locks),
- `cairn:admin`.

On top of that the `agent` policy (§5.5) can require a human for any action.

**Tools.** Each tool has a JSON Schema input, an `outputSchema` (it returns
`structuredContent` plus a short text rendering) and annotations. Lists are
paginated with cursors. Large artifacts (logs, plan JSON) are returned as
*resource links*, not inline blobs.

| Tool | Purpose | Annotations | Scope |
|---|---|---|---|
| `search` | Fuzzy search across spaces, stacks, runs, contexts, policies and modules | read-only | read |
| `list_stacks`, `get_stack` | Inventory, with state, drift, lock and last run | read-only | read |
| `list_runs`, `get_run` | Status, timings, triggered-by, `next_actions` | read-only | read |
| `get_plan_summary` | Changes grouped by action, replacements, and an ordered "risky" list (deletes, replacements, IAM, data stores) | read-only | read |
| `get_run_diagnostics` | Errors and warnings with file and line, plus failure categories (auth, provider, quota, syntax) | read-only | read |
| `get_run_logs` | Tail, a line range, or `grep`, capped at 400 lines per call | read-only | read |
| `get_outputs` | Non-sensitive outputs; sensitive ones only with write scope and policy permission | read-only | read |
| `list_resources` | Resource addresses and types from state; attribute values are never returned | read-only | read |
| `get_dependency_graph` | The DAG around a stack, with output mappings | read-only | read |
| `list_drift` | Drifted stacks and the last check | read-only | read |
| `list_locks` | Stack and PR locks, with holder and age | read-only | read |
| `evaluate_policy` | Run a policy against a run's input, or an ad-hoc input, without enforcing it | read-only | read |
| `trigger_preview` | Queue a proposed run (optionally with a ref or targets) and return the run, with a resource link to follow it | not read-only, not destructive, idempotent with a key | preview |
| `local_preview` | stdio only: tar the client's working directory and start a local-preview run | not destructive | preview |
| `trigger_deploy` | Queue a tracked run. It stops at `unconfirmed` unless policy allows auto-apply | **destructive** | write |
| `confirm_run` | Approve and apply an `unconfirmed` run. **Always asks a human through elicitation** (shown in the client UI with the plan summary and the "risky" list). If the client can't elicit, the call returns `needs_human` with a URL for a person to confirm in the web UI (URL-mode elicitation where supported). | **destructive** | write |
| `discard_run`, `cancel_run`, `retry_run` | Run control | destructive (cancel), idempotent | write |
| `set_variable`, `delete_variable` | Stack or context variables. Secret values are write-only and never echoed back. | destructive | write |
| `lock_stack`, `unlock_stack` | Locking, with a note | idempotent | write |

**Resources and subscriptions**

- `cairn://stacks/{id}`, `cairn://runs/{id}`, `cairn://runs/{id}/logs`,
  `cairn://runs/{id}/plan.json`, `cairn://stacks/{id}/outputs` and
  `cairn://policies/{id}` are exposed as resource templates with completion
  support for their IDs.
- Clients can `resources/subscribe` to a run and get
  `notifications/resources/updated` on each state change. For long-running
  calls such as `trigger_deploy … wait=true`, Cairn uses the MCP **tasks**
  primitive where the client supports it, and otherwise progress
  notifications.

**Prompts**

- `review-plan` (risk checklist, blast radius, policy verdicts),
- `triage-failed-run` (diagnostics → likely cause → suggested fix),
- `drift-report`,
- `write-policy` (natural language → Rego plus a test, validated with
  `evaluate_policy`).

**Safety rules (non-negotiable)**

1. No tool applies a change without a human decision, unless an `agent` policy
   explicitly allows auto-apply for that stack and principal. That policy is
   audited and shown in the UI.
2. Every MCP call is audited with the principal, the client's `clientInfo`, the
   tool and arguments (secrets redacted), and the result.
3. Tool outputs are size-capped (default 25k tokens) and use continuation
   cursors.
4. Text that comes from repositories or tofu output is put in fenced,
   clearly-labelled fields so clients can treat it as data, not instructions.
5. `cairn mcp serve --read-only` drops all non-read-only tools from the
   session.

### 6.6 Web UI

- Server-rendered HTML (`askama` templates plus `htmx`) embedded in the binary.
  No SPA build, about 50 KB of JS. This is OTF's approach (templ plus htmx), not
  Terrakube's 31k-line React app.
- Live run logs over SSE. Plan view with resource diffs. Stack dependency graph
  (SVG rendered server-side). Drift dashboard. Policy-evaluation history.
  Locks page.
- Accessible (WCAG 2.2 AA), keyboard-navigable, dark mode.
- The UI uses the same session cookie as the OIDC login, and every action calls
  the native API (§6.2), so nothing can be done only through the UI.

### 6.7 Registry

- **Module registry protocol** (`modules.v1`): publish from a git tag (VCS
  webhook) or by upload (`cairn module publish`). Proposed runs of `tofu test`
  on PRs.
- **Provider registry protocol** (`providers.v1`) for private providers, plus
  the **provider network mirror protocol** so air-gapped runners can pull
  public providers through Cairn's cache.
- **OCI**: OpenTofu 1.10+ can install modules and providers from OCI registries.
  Cairn documents using Forgejo's built-in OCI package registry for that, so
  people who already run Forgejo don't need Cairn's registry.

### 6.8 Configuration as code

- **`.cairn/config.yml` in the repo** (like `atlantis.yaml` or Spacelift's
  `.spacelift/config.yml`) declares the stacks for that repo: `project_root`,
  `trigger_paths`, engine version, hooks, labels and dependencies. Changes are
  previewed in PRs like any other change. Admins can restrict which spaces a
  repo may declare stacks in.
- **An OpenTofu provider, `cairn/cairn`**, manages spaces, stacks, contexts,
  policies, integrations and runner pools, written in Go with
  `terraform-plugin-framework`, because that is where the provider tooling is.
  Until it ships, the `tfe` provider covers organizations, workspaces,
  variables and teams through §6.1.
- **Bootstrap**: `cairnd --bootstrap bootstrap.yml` declares the initial admin
  group, VCS integration and a "management stack" that points at the repo
  holding the Cairn provider config, the GitOps-style "Cairn manages Cairn"
  pattern.

## 7. Security

### 7.1 Authentication

- **Humans**: OIDC authorization code with PKCE against any compliant IdP
  (Authentik, Keycloak, Dex, Google…). Sessions are server-side, with
  `HttpOnly` `SameSite=Lax` cookies.
- **The CLI**: the OAuth 2.0 device authorization grant, issued by Cairn after
  IdP login.
- **API tokens**: personal, team and stack-scoped. They are random 256-bit
  values with a `cairn_` prefix, stored as SHA-256 hashes, with an optional
  expiry and last-used timestamp.
- **Runners**: a pool token is exchanged for a short-lived runner credential.
  Runners hold leases, not long-lived database access.
- **Runs**: each phase gets a run token valid only for that run's state, logs
  and outputs, which expires when the phase ends.

### 7.2 Authorization

- **Roles per space**, inherited downwards: `reader`, `planner` (can create
  proposed runs), `writer` (tracked runs, confirm, variables) and `admin`.
  Stack-level overrides are possible.
- **IdP group mapping**: `groups` claim (or a configured claim) → `(space,
  role)`, re-evaluated at every login, with an optional `login` policy for
  anything more complex. This is the Terrakube feature users wanted, without
  Dex in the middle.
- **VCS identities**: users link their Forgejo, GitHub or GitLab identity.
  PR-comment commands are authorized as that user. Unlinked commenters get a
  polite refusal.

### 7.3 Secrets and encryption

- **Envelope encryption**: one data key per space, sealed by a master key that
  comes from a file (default), the environment, the OpenBao/Vault transit
  engine, or a cloud KMS.
- **What is encrypted**: XChaCha20-Poly1305 for state, plan files, secret
  variables, sensitive outputs and webhook secrets.
- **Key rotation**: `cairn admin rotate-keys` re-seals data keys without
  rewriting blobs.

### 7.4 Dynamic credentials

Cairn is an **OIDC workload-identity issuer**. For each run phase it mints a JWT
signed with a rotating key published at `/oidc/jwks`. The claims are:

- `sub`: `space:<space>:stack:<stack>:run_phase:<plan|apply>`
- `aud`: per integration
- `cairn_*` claims for space, stack, run, branch, commit and trigger

Built-in integrations exchange that token before the run and inject the
results:

- **AWS**: `AssumeRoleWithWebIdentity`, injecting `AWS_*` variables or a
  credentials file.
- **GCP**: Workload Identity Federation.
- **Azure**: federated credentials.
- **OpenBao and Vault**: JWT auth-method login with a role → a token injected as
  `VAULT_TOKEN` (and `BAO_TOKEN`) with a TTL equal to the phase timeout.
  Optional secret pre-fetch: `bao kv get` paths are mapped into variables.

The raw JWT is also available as `CAIRN_WORKLOAD_IDENTITY_TOKEN` for anything
custom. Plan-phase and apply-phase tokens have different `sub` values, so cloud
roles can make plans read-only.

### 7.5 Supply chain and hardening

- **Engine downloads**: verified against the SHA256SUMS and their signature,
  using OpenTofu's published signing keys and cosign bundle. An offline mirror
  directory is supported.
- **Release artifacts**: SBOMs (CycloneDX) and SLSA provenance. `cargo deny`
  in CI checks licenses and advisories.
- **Outbound requests** (notification webhooks, VCS API base URLs): SSRF
  protection that refuses private ranges unless explicitly allowed, as
  Terrakube does.
- **Audit log**: append-only and hash-chained (each entry includes the
  previous entry's hash), exportable as JSON Lines and streamable via SSE.

## 8. Data and storage

| Store | Default | Alternatives | Notes |
|---|---|---|---|
| Metadata | SQLite (WAL, `synchronous=NORMAL`) in `$CAIRN_DATA/cairn.db` | PostgreSQL 14+ | Behind a `Store` trait. `sqlx` with compile-time-checked queries per backend. Migrations embedded and applied on start (with `--no-migrate` for controlled upgrades). |
| Blobs (state, plans, logs, config tarballs, registry artifacts) | Filesystem `$CAIRN_DATA/blobs` | Any `object_store` backend (S3/MinIO/Garage, GCS, Azure) | Content-addressed (SHA-256) and deduplicated. Retention: logs and config tarballs 90 days, plan files 30 days, state versions forever (configurable). |
| Event bus | In-process (`tokio::sync::broadcast`) | Postgres `LISTEN/NOTIFY` when several `cairnd` share a database | No Redis (Terrakube needs Valkey for logs and coordination). |
| Runner caches | `$CAIRN_DATA/cache/{engines,git,plugins}` | n/a | Size-capped LRU. Can be prewarmed. |

Main tables: `spaces`, `stacks`, `stack_labels`, `runs`, `run_phases`,
`run_events`, `state_versions`, `outputs`, `contexts`, `context_attachments`,
`variables`, `policies`, `policy_attachments`, `policy_evaluations`,
`dependencies`, `schedules`, `vcs_integrations`, `webhook_deliveries`,
`pr_locks`, `stack_locks`, `users`, `groups`, `role_bindings`, `tokens`,
`runner_pools`, `runners`, `leases`, `modules`, `module_versions`, `providers`,
`integrations`, `audit_log`, `notifications`.

**Backup.** `cairn admin backup <dir>` uses SQLite's online backup API plus a
blob manifest, or tells you to use `pg_dump` for Postgres. Restore is the same
command in reverse.

## 9. Performance and resource budgets

These budgets are acceptance criteria. The benchmark harness in `../bench/`
gains a `cairn/run.py` driver and runs in CI on every release. A budget
regression of more than 10 % fails the release.

| Metric | OTF (measured) | Terrakube (measured) | Atlantis (measured) | **Cairn budget** |
|---|---|---|---|---|
| Processes or containers to run | 2 | 7 | 1 | **1** (+ optional Postgres) |
| Compressed image, arm64, app only | 33 MiB | 1,364 MiB | 56 MiB | **≤ 20 MiB** (≤ 75 MiB with tofu bundled) |
| Cold start → healthy (4 vCPU) | {{otf_cold}} | {{tk_cold}} | {{at_cold}} | **≤ 0.2 s** |
| Cold start, 1-CPU budget | {{otf_cold1}} | {{tk_cold1}} | {{at_cold1}} | **≤ 0.5 s** |
| Idle RSS, app | {{otf_idle}} | {{tk_idle}} | {{at_idle}} | **≤ 25 MiB** |
| Idle RSS, whole stack | {{otf_idle_stack}} | {{tk_idle_stack}} | {{at_idle_stack}} | **≤ 25 MiB** (SQLite) |
| Idle CPU, whole stack | {{otf_idle_cpu}} | {{tk_idle_cpu}} | {{at_idle_cpu}} | **≤ 0.1 % of a core** (no busy timers) |
| List 50 stacks, 1 client p50 | {{otf_c1}} | {{tk_c1}} | n/a | **≤ 2 ms** |
| List 50 stacks, 16 clients | {{otf_rps}} | {{tk_rps}} | n/a | **≥ 5,000 req/s, p99 ≤ 20 ms** |
| Run → applied (100 resources; tofu itself ≈ {{tofu_base}}) | {{otf_run}} | {{tk_run}} | {{at_run}} | **≤ tofu + 0.3 s** |
| No-op run | {{otf_noop}} | {{tk_noop}} | n/a | **≤ tofu + 0.3 s** |
| 5 runs at once, wall time | {{otf_par}} | {{tk_par}} | {{at_par}} | **≤ 1.5 × one run** on ≥ 4 cores |
| Peak RSS, 5 runs at once (stack, incl. tofu) | {{otf_par_rss}} | {{tk_par_rss}} | {{at_par_rss}} | **≤ 60 MiB + 5 × tofu** |
| PR opened → plan feedback on the PR | n/a | n/a | {{at_pr}} | **≤ tofu + 1 s** |

Engineering rules that keep these budgets:

- Async I/O everywhere (tokio). Blocking work (tar, compression, SQLite) runs
  on `spawn_blocking` with bounded pools.
- Logs are streamed, never fully buffered. Plan JSON is parsed incrementally
  (`serde_json::StreamDeserializer`) to compute summaries.
- `mimalloc` as the global allocator. Release builds use `lto = "fat"`,
  `codegen-units = 1`, `panic = "abort"` for the server and `strip = true`.
- No timer wakes up more often than every 30 s at idle.
- Everything that can grow (connection pools, caches, channels) has a limit,
  and those limits are exported as metrics.

## 10. Observability

- **Metrics**: Prometheus `/metrics` with queue depth per pool, run duration
  per phase, policy evaluation time, webhook latency, DB pool usage and cache
  hit rate.
- **Logs**: `tracing` with JSON or pretty output. Each request and run has a
  span, and `run_id` is attached to every log line inside a run.
- **Tracing (optional)**: OpenTelemetry OTLP export behind the
  `--features otel` build flag, so the default binary stays small.
- **Run timing breakdown** (queue wait, source fetch, engine resolution, init,
  plan, policy, apply, state upload), stored per run and shown in the UI. It
  makes regressions like R2 visible immediately.

## 11. Deployment

- **Binaries**: static musl binaries for `x86_64`, `aarch64` and `armv7`, built
  with `cross` and published as GitHub/Forgejo releases with checksums,
  signatures (cosign keyless) and SBOMs.
- **OCI image**: built `FROM scratch` from the static binary plus CA
  certificates, multi-arch. A `-tofu` variant bundles a default OpenTofu
  version.
- **systemd**: a unit with `DynamicUser=`, `StateDirectory=cairn`,
  `ProtectSystem=strict` and `MemoryMax=` recommendations for small hosts.
- **Docker Compose**: an example with Caddy for TLS and an optional Postgres.
  This is the recommended homelab layout, next to an existing Forgejo and
  Authentik.
- **Configuration**: a TOML file plus `CAIRN_*` environment variables plus
  flags (in increasing precedence). `cairnd config check` validates a config
  without starting the server.
- **Upgrades**: forward-only migrations that run online. `cairnd --version`
  shows the schema version, and downgrades are refused clearly.

### 11.1 Fit for the tesseract homelab

| Need | How Cairn covers it |
|---|---|
| Forgejo | First-class VCS integration (§5.4). Forgejo's OCI registry can host modules and providers (§6.7). |
| Authentik | Direct OIDC, with the groups claim mapped to roles (§7.2). No Dex. |
| OpenBao | JWT-auth dynamic credentials (§7.4) and the transit engine as the master key (§7.3). |
| Postgres already running | Optional backend. SQLite also works if the Pi shouldn't carry more connections. |
| Runs on mediabox | `cairnd runner --pool mediabox` connects outbound to the Pi. |
| Pi already in swap | ≤ 25 MiB idle, one process, no JVM (§9). |

## 12. Implementation plan (Rust)

### 12.1 Workspace layout

```
cairn/
  crates/
    cairn-core/      domain types, state machine, events, errors (no I/O)
    cairn-store/     Store trait + sqlite/postgres impls, migrations, blob store (object_store)
    cairn-sched/     queues, leases, schedules (croner), dependency DAG
    cairn-runner/    workspace prep, engine manager, sandbox, tofu process + JSON stream parsing
    cairn-policy/    regorus wrapper, decision points, policy testing
    cairn-vcs/       Forgejo/Gitea, GitHub (App), GitLab: webhooks, statuses, comments, clone auth
    cairn-auth/      OIDC RP, device flow, tokens, RBAC, workload-identity issuer, integrations
    cairn-api/       axum routers: native API, TFE API, SSE, webhooks, registry, OpenAPI (utoipa)
    cairn-mcp/       rmcp server: tools/resources/prompts, used by cairnd (/mcp) and cairn (stdio)
    cairn-ui/        askama templates + htmx, assets embedded with rust-embed
    cairn-client/    typed client for the native API (used by CLI and MCP stdio)
  bin/
    cairnd/          server + `runner` subcommand
    cairn/           CLI (clap) + `mcp serve`
  provider/          terraform-provider-cairn (Go)
  bench/             benchmark harness (this repo's ../bench, extended with a cairn driver)
```

### 12.2 Key dependencies

| Need | Crate |
|---|---|
| Async runtime and HTTP | `tokio`, `axum`, `tower`, `tower-http`, `hyper` |
| HTTP client (VCS, IdP, cloud APIs) | `reqwest` with `rustls` |
| Database | `sqlx` (sqlite, postgres) |
| Blobs | `object_store` |
| Git | `gix` (fetch/checkout), shelling out to `git` as a fallback for exotic auth |
| Policies | `regorus` |
| MCP | `rmcp` |
| OIDC and JWT | `openidconnect`, `jsonwebtoken` |
| Crypto | `chacha20poly1305`, `sha2`, `rand`, `argon2` (UI passwords only, for the optional local-users mode) |
| CLI | `clap` (derive) + `clap_complete`, `comfy-table`, `indicatif`, `keyring` |
| Templates | `askama`, `rust-embed` |
| Scheduling | `croner` |
| Serialization | `serde`, `serde_json`, `serde_yaml_ng`, `toml` |
| Telemetry | `tracing`, `tracing-subscriber`, `metrics` + `metrics-exporter-prometheus`, optional `opentelemetry-otlp` |
| Sandboxing | `landlock`, `nix` (process groups, signals), cgroup v2 via the filesystem |
| OpenAPI | `utoipa`, `utoipa-axum` |

### 12.3 Testing

- **Unit and property tests**: the run state machine (`proptest`, checking that
  no illegal transitions are possible), queue fairness, DAG cycle detection, and
  redaction.
- **Store contract tests**: the same suite against SQLite and Postgres
  (`testcontainers`).
- **TFE compatibility**: CI runs real `tofu` against `cairnd`: `tofu login`,
  `cloud {}` init/plan/apply in remote and local execution modes, state
  migration from the local backend, and `tfe` provider CRUD for organizations,
  workspaces, variables and teams. The suite is pinned to the last 3 OpenTofu
  minor versions.
- **VCS contract tests**: recorded webhook payloads (open, sync, comment, close,
  merge, reopen) for Forgejo, Gitea, GitHub and GitLab, plus a live Forgejo
  container in CI (the bench harness already provides one).
- **MCP conformance**: the MCP Inspector CLI in CI, plus golden tests for tool
  schemas and annotations.
- **Benchmarks**: `bench/` with a `cairn` driver. The budgets in §9 are
  enforced.

## 13. Milestones

| Milestone | Contents | Exit criterion |
|---|---|---|
| **M0: skeleton** (2 wk) | Workspace and crates, config, SQLite store, migrations, native API for spaces and stacks, CLI login (local token), `/healthz`, CI, release pipeline | `cairnd` starts in < 100 ms; `cairn stack list` works |
| **M1: replace TFC for this repo** (6 wk) | TFE API phase 1, the state backend, the embedded runner, engine manager, tracked and proposed runs, Forgejo integration (push → run, PR → proposed run with status and summary comment), OIDC login (Authentik) with group mapping, SSE logs, minimal UI, CLI `stack preview/deploy/confirm/logs/outputs/state`, read-only MCP tools over stdio | `tf-github-config` runs on Cairn by changing only `cloud { hostname }`; §9 budgets met for cold start, idle and run overhead |
| **M2: Spacelift workflow** (6 wk) | Contexts (with auto-attach), Rego policies (plan, approval, push), drift schedules, locks (R6), comment commands, the write-scoped MCP tools with elicitation, MCP over HTTP with OAuth, Postgres backend, `.cairn/config.yml` | A PR on Forgejo produces plans, policy verdicts and drift reports; Claude Code can preview and request an apply that a human confirms |
| **M3: scale-out and credentials** (6 wk) | Remote runners and pools, workload-identity issuer plus AWS/GCP/Azure/OpenBao integrations, dependencies with output mapping, module and provider registry, notifications, audit log, OpenTofu provider v0 | A runner on mediabox applies with OpenBao-issued credentials; a change to stack A triggers B with mapped outputs |
| **M4: polish** (ongoing) | Blueprints, TFE API phases 2 and 3, UI dependency graph, `cairn import tfc`, GitHub App and GitLab parity, HA notes | 1.0 |

## 14. Migration from Terraform Cloud

`cairn import tfc --org jjliggett --token …` does the following:

1. It reads workspaces, variables (values of sensitive ones can't be read, so
   they are listed for re-entry), tags and the current state version through
   the TFE API.
2. It creates the matching space and stacks, and uploads the state versions.
3. It prints the `cloud {}` block change needed (`hostname = "cairn.<domain>"`).

For this repository, the result is a one-line change in `terraform.tf`.

## 15. Open questions

1. **Name.** "Cairn" may clash with other projects; check before the first
   release.
2. **SQLite plus remote runners**: fine for one node. Should "HA" require
   Postgres, or is Litestream-style replication of SQLite enough for homelabs?
3. **Ship our own policy library** (CIS-style samples), or link to existing
   Rego libraries?
4. **Container isolation on Pi runners**: rootless podman is ideal but adds
   setup. Should Landlock-only be the default there?
5. **GitHub App vs PAT** for the GitHub integration: an App is better for orgs,
   a PAT is simpler for individuals. Supporting both costs maybe 1 week.

## Appendix A: Where each feature comes from

| Capability | OTF | Terrakube | Atlantis | Spacelift | Cairn |
|---|---|---|---|---|---|
| Single small binary | ✔ | ✘ (7 services) | ✔ | n/a (SaaS) | ✔ |
| Forgejo/Gitea | ✔ | ✘ | ✔ (lock leak on close, R6) | ✘ | ✔ |
| PR feedback | commit status | comments (GitHub, GitLab, Bitbucket) | comments | comments + checks | status + one updating comment + commands |
| TFE API / `cloud {}` | ✔ | ✔ | ✘ | ✘ | ✔ |
| State storage | Postgres | blob storage | ✘ | ✔ | blob storage + DB metadata, encrypted |
| SSO + group RBAC | OIDC, teams | Dex, groups | basic auth | ✔ | OIDC direct, group → role per space |
| Remote runners | otf-agent | executors / k8s | ✘ | worker pools | `cairnd runner`, outbound only |
| Policy as code | ✘ | OPA | conftest | Rego (many decision points) | Rego (`regorus`) at 8 decision points |
| Drift detection | ✘ | schedules | API | ✔ | cron + reconcile |
| Dynamic credentials | AWS, GCP, Azure | + Vault | ✘ | AWS, GCP, Azure | + OpenBao/Vault |
| Stack dependencies with outputs | run triggers | workspace chaining | ✘ | ✔ | ✔ |
| Contexts / variable sets | variable sets | global variables | ✘ | ✔ (auto-attach) | ✔ (auto-attach) |
| Private registry | modules | modules + providers | ✘ | modules + providers | modules + providers + network mirror |
| CLI | `otf` (admin) | ✘ | ✘ | `spacectl` | `cairn` (spacectl-class) |
| MCP | ✘ | ✘ | ✘ | stdio via spacectl (36 tools, no annotations) | stdio + HTTP/OAuth, annotations, elicitation, resources, prompts |
| Run overhead (100 resources) | ~0.4 s | ~27 s | ~0.4 s | n/a | ≤ 0.3 s |
