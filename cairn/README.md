# cairn

**Cairn** is a lightweight OpenTofu automation server and CLI written in
Rust. The aim is OTF's footprint, Terrakube's features, Atlantis's PR
workflow, and Spacelift's model and CLI ergonomics, with an MCP server for
agents. This directory holds the MVP implementation, the specification, and
the benchmarks that set its targets.

| Path | What |
|---|---|
| [`crates/cairnd`](crates/cairnd) | The server: API, embedded runner, state, events |
| [`crates/cairn-cli`](crates/cairn-cli) | `cairn`: the CLI and `cairn mcp serve` |
| [`tests/`](tests/) | End-to-end tests against a real `cairnd`, `cairn` and OpenTofu (`test_*.py`) |
| [`docs/GUIDE.md`](docs/GUIDE.md) | User guide: identity and access, storage, policies, VCS, registry, schedules, dependencies |
| [`docs/SPEC.md`](docs/SPEC.md) | The specification: concepts, behaviour, APIs, CLI, MCP server, security, budgets, Rust implementation plan and milestones |
| [`docs/benchmark-report.md`](docs/benchmark-report.md) | What we measured, including Cairn against OTF, Terrakube and Atlantis |
| [`bench/`](bench/) | Reproducible benchmark harness (Docker + cgroup accounting) and raw results |

## Quick start

```sh
cargo build --release
export CAIRN_SITE_TOKEN=$(openssl rand -hex 24)
./target/release/cairnd --data ./cairn-data &        # listens on 127.0.0.1:8833

./target/release/cairn login --host http://127.0.0.1:8833 --token "$CAIRN_SITE_TOKEN"
cairn stack create demo
cairn stack preview --stack demo --dir ./my-module    # proposed run: plan only
cairn stack deploy  --stack demo --dir ./my-module    # tracked run: plan, ask, apply
cairn stack outputs --stack demo
```

`cairnd` downloads the OpenTofu version a stack asks for (default 1.11.6) from
GitHub releases and verifies its SHA-256 checksum. To stay offline, use
`--no-download` and put the binary at `<data>/engines/<version>/tofu`, or pin
one binary for every stack with `--tofu-bin`. Every flag has a `CAIRN_*`
environment variable; see `cairnd --help`. Container images:
[`Dockerfile`](Dockerfile) (glibc on scratch, ~5 MiB compressed) and
[`Dockerfile.static`](Dockerfile.static) (musl).

### Agents (MCP)

```sh
claude mcp add cairn -- cairn mcp serve          # Claude Code; any stdio MCP client works
cairn mcp serve --read-only                      # inspection tools only
```

The tools are `list_stacks`, `get_stack`, `list_runs`, `get_run`,
`get_plan_summary`, `get_run_diagnostics`, `get_run_logs`, `get_outputs`,
`list_state_versions`, `trigger_preview`, `trigger_deploy`, `confirm_run`,
`discard_run` and `cancel_run`, plus the prompts `review-plan` and
`triage-failed-run`. Every tool is annotated as read-only, destructive or
idempotent. Runs started from MCP never auto-apply. `confirm_run` always asks
the person through MCP elicitation; clients that can't elicit get
`needs_human` with the `cairn run confirm …` command to run instead.

### Using it from OpenTofu directly (HTTP backend)

```hcl
terraform {
  backend "http" {
    address        = "http://cairn:8833/api/v1/stacks/<stack-id>/state"
    lock_address   = "http://cairn:8833/api/v1/stacks/<stack-id>/state/lock"
    unlock_address = "http://cairn:8833/api/v1/stacks/<stack-id>/state/lock"
    lock_method    = "POST"
    unlock_method  = "DELETE"
    username       = "cairn"   # ignored
    # password: set TF_HTTP_PASSWORD to a Cairn token
  }
}
```

State written this way is versioned like state from managed runs, and the
managed runner picks it up.

## MVP status

| Area | Implemented now | Next (see SPEC §13) |
|---|---|---|
| Server | One binary; SQLite (WAL); native `/api/v1` (spaces, stacks, variables, configs, runs, logs, outputs, state, locks, tokens, SSE events, long-poll); Prometheus `/metrics` | Postgres backend, OpenAPI document |
| Runs | Proposed, tracked, destroy, task (tofu commands under the stack lock, gated by `task` policies) and drift runs (with optional reconcile); auto-apply or confirm/discard, cancel (SIGINT, then kill), per-stack queue, configurable concurrency, no fixed sleeps, structured plan summaries | Dependencies |
| Schedules | Cron (UTC) and one-shot schedules for drift detection, tracked runs, tasks and destroys; event-driven timer, fires missed schedules once after downtime | Time zones |
| State & storage | Versioned, deduplicated by SHA-256; OpenTofu `http` backend with locking; serial/lineage checks; blobs on local disk, S3-compatible stores (AWS, MinIO, R2, GCS interop…) or Azure Blob; encrypted at rest by default; native TLS | Postgres for metadata, rollback command |
| TFE API | Organizations, workspaces (with pagination), configuration versions with pre-signed uploads, runs and actions, plans and applies with logs, current state version and outputs, workspace locks | `tofu login` and the `cloud {}` block over TLS, state-version upload, the `tfe` provider, teams |
| Identity & access | Space tree with inherited roles (reader/planner/writer/admin); users from OIDC (bearer ID tokens and browser/CLI login with PKCE); teams = IdP groups; user and service tokens with expiry; audit log; sensitive variables encrypted (ChaCha20-Poly1305) | Workload identity / OpenBao |
| Policy | Rego (in-process `regorus`): `plan` (deny/warn on `tofu show -json`), `approval` (reviews; reject discards), `login` (deny/admin/space roles), `push` (track/propose/ignore), `task` (deny), `notification` (filter and reshape signed webhooks); explicit or label auto-attachment; dry-run endpoint | `trigger` (with dependencies) |
| VCS | GitHub, GitLab, Bitbucket Cloud, Azure DevOps, Gitea, Forgejo: verified webhooks, source as commit archives, push → tracked / PR → proposed runs, `/cairn plan` and `/cairn apply` comments, commit statuses and one updating PR comment on all six, PR close/merge stops its runs and frees locks, `push` policies, manual sync | GitHub App auth, Bitbucket Data Center |
| Registry | Private `modules.v1` and `providers.v1` (namespaces = spaces, RBAC applies), semver ordering, goreleaser-layout provider publishing with GPG-signed SHA256SUMS verified by OpenTofu, pre-signed downloads, run-scoped registry tokens | Publishing modules from VCS tags |
| CLI / MCP | Login, spaces, stacks, preview/deploy with live logs, runs (review, policies), policies, webhooks, VCS integrations and stack sync, module and provider publishing, destroy/task/drift, schedules, variables, state, outputs, raw API; MCP over stdio with a human gate on applies | Repo-aware stack inference, MCP over HTTP with OAuth |

Tests: `cargo test` (unit) and
`TOFU=$(which tofu) python3 -m unittest discover -s tests -p 'test_*.py'`
(end-to-end against a real `cairnd`, `cairn` and OpenTofu; the OIDC tests
start Dex in Docker and are skipped without it).

## Research material
| `upstream/otf` | Submodule: [leg100/otf](https://github.com/leg100/otf) at `v0.6.3` |
| `upstream/terrakube` | Submodule: [terrakube-io/terrakube](https://github.com/terrakube-io/terrakube) at `2.33.3` |
| `upstream/atlantis` | Submodule: [runatlantis/atlantis](https://github.com/runatlantis/atlantis) at `v0.49.0` |

The submodules are shallow and pinned to the release tags that were
benchmarked. To fetch them:

```sh
git submodule update --init --depth 1 cairn/upstream/otf cairn/upstream/terrakube cairn/upstream/atlantis
```

## Moving this into its own repository

This lives in `tf-github-config` for now, and `main.tf` declares the new
`cairn` GitHub repository. After `tofu apply` has created it, run these
commands from a fresh clone of `tf-github-config` to give it its own history,
with the submodules at its root:

```sh
git subtree split --prefix=cairn -b cairn-only
git clone --branch cairn-only . ../cairn && cd ../cairn
for m in otf:leg100/otf terrakube:terrakube-io/terrakube atlantis:runatlantis/atlantis; do
  git config -f .gitmodules submodule.upstream/${m%%:*}.path upstream/${m%%:*}
  git config -f .gitmodules submodule.upstream/${m%%:*}.url https://github.com/${m#*:}
  git config -f .gitmodules submodule.upstream/${m%%:*}.shallow true
done
git add .gitmodules && git commit -m "chore: restore .gitmodules at repo root"
git submodule update --init --depth 1
git remote set-url origin git@github.com:jjliggett/cairn.git && git push -u origin HEAD:root
```

`git subtree split` keeps the pinned submodule commits under `cairn/upstream/`,
but not the `.gitmodules` file at the root of `tf-github-config`. The loop above
recreates that file.
