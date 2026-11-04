# Benchmark report: OTF vs Terrakube vs Atlantis (and the Cairn MVP)

All tools were measured on the same host with the same OpenTofu config, using
the harness in [`../bench`](../bench/README.md). The raw numbers are in
[`../bench/results`](../bench/results/), and
[`SUMMARY.md`](../bench/results/SUMMARY.md) has every row for every tool. The
Cairn MVP's results are [at the end](#cairn-mvp-vs-the-three).

**Summary.** OTF and Atlantis are small and fast: one Go process each,
10–14 MiB idle, and a plan+apply of 100 resources in about 0.5 s, of which
OpenTofu itself needs 0.17 s. Terrakube is in a different class:

- 7 containers, ~1.5 GiB of arm64 images (with its dependencies), 26 s to start;
- 2.4 GiB RSS at idle on this 16 GiB host, and at least 3.3 GiB of memory
  limits before its three JVMs will start at all;
- **27 s per run**, because its executor contains hard-coded sleeps;
- one job at a time.

OTF's one weak spot is its workspace list API, at 18 req/s for 50 workspaces,
caused by per-row permission queries. Atlantis's is Forgejo/Gitea lock
handling. Cairn's spec sets budgets that beat the best measured number in
each row. The Cairn MVP meets them in every row it implements; the PR
workflow isn't built yet.

## Setup

| | |
|---|---|
| Host | 4 vCPU Intel Xeon @ 2.1 GHz, 16 GiB RAM, Linux 6.18, x86_64, Docker 29.9 (cgroup v1) |
| Versions | OTF 0.6.3 (`leg100/otfd`), Terrakube 2.33.3 (`azbuilder/*`), Atlantis 0.49.0 (built from the pinned submodule) |
| Engine | OpenTofu 1.11.6, pre-seeded for every tool, so no downloads during measured phases |
| VCS | Local Gitea 1.24 (Forgejo-compatible), not counted in any tool's resources |
| Fixture | 100 `terraform_data` resources, no providers. Plain `tofu`: init 0.03 s, plan 0.07 s, apply 0.07 s |
| Stacks | OTF: otfd + Postgres. Terrakube: api, executor, registry, ui, Dex, Valkey, Postgres (no MinIO, Traefik or LDAP). Atlantis: atlantis only |
| Sampling | cgroup memory (`total_rss`) and CPU (`cpuacct.usage`) per container, every 0.2 s |

## Results

### Footprint

| | OTF | Terrakube | Atlantis |
|---|---|---|---|
| Containers to run | 2 | 7 (10 in the reference Compose) | 1 |
| App images, compressed arm64 | 33 MiB | 1,362 MiB | 56 MiB |
| Including required deps, arm64 | 142 MiB | 1,527 MiB | 56 MiB |
| Cold start → healthy | **0.7 s** | **25.6 s** (api; executor 12 s, registry 10 s) | **0.2 s** |
| Idle RSS, app | 13 MiB | 2,382 MiB (api 1,725, executor 360, registry 293, ui 4) | 10 MiB |
| Idle RSS, whole stack | 36 MiB (Postgres 23) | 2,405 MiB | 10 MiB |
| Idle CPU, whole stack | 0.3 % of a core | 1.7 % of a core | < 0.1 % |
| Smallest memory limit it starts in | 16 MiB (otfd) | 3,328 MiB (api 1,536 + executor 1,024 + registry 768) | 16 MiB |
| Source (non-test lines) | Go 46k + templ 5k | Java 36k + TypeScript 31k | Go 42k |

### API: list 50 workspaces

Atlantis has no inventory API, so it is shown with `GET /api/locks`, its only
authenticated read endpoint. That is not a like-for-like comparison.

| | OTF (`/api/v2/organizations/:org/workspaces`) | Terrakube (`/api/v1/organization/:org/workspace`) | Atlantis (`/api/locks`) |
|---|---|---|---|
| 1 client, p50 / p95 | 152 / 192 ms | 99 / 137 ms | 0.1 / 0.2 ms |
| 16 clients, req/s | **18** | **31** | 34,521 |
| 16 clients, p99 | 1,012 ms | 825 ms | 3 ms |
| CPU during the 16-client phase | 2.9 cores busy (Postgres 1.8, otfd 1.1) | 3.3 cores (api 2.6, Postgres 0.7) | n/a |

### Runs (plan + apply, 100 creates)

| | OTF | Terrakube | Atlantis (`/api/apply`) |
|---|---|---|---|
| First run (warms engine and git cache) | 0.48 s | 34.5 s | 0.66 s |
| Create → planned, median of 5 | 0.22 s | 10.75 s | 0.33 s (`/api/plan`) |
| Create → applied, median of 5 | **0.44 s** | **26.95 s** | **0.57 s** |
| No-op run, median of 5 | 0.24 s | 10.63 s | n/a (no state backend) |
| 5 runs at once, wall time | **1.01 s** | **146.5 s** (strictly serial) | **1.09 s** |
| CPU per run, whole stack | 0.66 s (otfd 0.52, Postgres 0.13) | 8.0 s (api 4.9, executor 2.2, Postgres 0.7) | 0.60 s |
| Peak RSS, 5 at once, whole stack | 154 MiB | 2,559 MiB | 103 MiB |
| PR opened → plan comment on Gitea | n/a | n/a | 1.6 s |
| `atlantis apply` comment → result | n/a | n/a | 0.8 s |

### With the whole stack capped at 1 CPU

Every container of a tool's stack shared one CFS quota of 1 CPU, a rough
stand-in for a small ARM board.

| | OTF | Terrakube | Atlantis |
|---|---|---|---|
| Cold start → healthy | **0.64 s** | **94.4 s** (registry 41 s, executor 47 s, api 94 s) | **0.26 s** |
| Create → applied, median | 0.78 s | 28.9 s | 0.72 s |
| No-op run, median | 0.50 s | 11.1 s | n/a |
| 5 runs at once, wall time | 4.1 s | 147.5 s | 3.3 s |
| CPU per run, whole stack | 0.72 s | 8.7 s | 0.62 s |
| List 50 workspaces, 16 clients | 5 req/s (p99 3.8 s) | 6 req/s (p99 8.0 s) | (locks) 32,795 req/s |
| PR opened → plan comment | n/a | n/a | 1.8 s |

With one CPU the JVM switches to the serial collector, and the Terrakube API's
heap, still sized from the 16 GiB host, grew to **7.3 GiB RSS** during the API
phase. That tells you about JVM ergonomics, not about a Pi; the memory floors
below are the useful number for a small board.

## Findings

### 1. Terrakube's run time is mostly fixed sleeps

The executor log for a single job shows the same idle gaps every time:

| Gap | Where |
|---|---|
| 5.0 s after `tofu init` | `executor/.../TerraformExecutorServiceImpl.java:693` `Thread.sleep(5000)` |
| 2.0 s before the next step | `TerraformExecutorServiceImpl.java:988` `Thread.sleep(2000)` |
| 3.0 s "Waited 3000 ms for stream completion" | `TerraformExecutorServiceImpl.java:128` polling loop |
| 5.0 s before uploading state | `TerraformExecutorServiceImpl.java:885` `Thread.sleep(5000)` |

Plan and apply are separate steps and each one hits these sleeps. That is how
0.17 s of OpenTofu becomes 27 s, and why a no-op plan still takes 10.6 s. On
top of that, the executor's async pool has a core size of 1
(`SpringAsyncAutoConfiguration.java:27`), so 5 runs submitted together finish
after 146 s. Terrakube's answer to concurrency is more executor replicas or
ephemeral Kubernetes jobs, each another JVM.

**For Cairn:** no fixed sleeps on the run path, completion signalled by events,
and configurable runner concurrency (SPEC R2, R3).

### 2. Terrakube's memory is JVM heap, and it grows into whatever the host has

All three Java images use the Paketo memory calculator, which sizes `-Xmx` from
the container's memory limit, or from the whole host when there is no limit. On
this 16 GiB host the API settled at 1.7 GiB RSS after startup and stayed there.
The memory-floor test (`bench/memory_floor.py`) starts each service under
decreasing hard limits with no swap:

| Service | Fails at | Starts at | RSS once healthy | Time to healthy |
|---|---|---|---|---|
| api-server | 1,024 MiB (OOM, exit 3); 768 MiB and below refused | **1,536 MiB** | 1,029 MiB | 18.5 s |
| executor | 768 MiB (OOM, exit 3); 640 MiB and below refused | **1,024 MiB** | 266 MiB | 5.1 s |
| open-registry | 640 MiB and below refused | **768 MiB** | 221 MiB | 4.3 s |
| *OTF otfd* | 12 MiB (OOM-killed) | **16 MiB** | 13 MiB | 0.2 s |
| *Atlantis* | 12 MiB (OOM-killed) | **16 MiB** | 9 MiB | 0.3 s |

"Refused" means exit code 82 from the memory calculator, for example: "fixed
memory regions require 726734K which is greater than 512M available"
(`-Xss1M × 250 threads`, 240M code cache, ~210M metaspace). Those are the
images' defaults, and the reference Compose file doesn't change them.
Terrakube's own development script runs the services with `-Xmx512m`
(api, executor) and `-Xmx256m` (registry)
(`scripts/setupDevelopmentEnvironment.sh`). Combined with a smaller
`BPL_JVM_THREAD_COUNT` and code cache, that should bring each container down
to roughly 0.5–0.8 GiB. That is untested here and still about 1.5–2.5 GiB for
the three services.

**On a 4 GB Pi that is already in swap, those floors are the real cost**:
3.3 GiB of limits for the three JVMs, plus Dex and Valkey. OTF's otfd and
Atlantis each start in 16 MiB.

### 3. OTF's workspace list does about 20 database queries per workspace

`ToTFE` fills in the permissions block of every workspace with 10
`CanAccess` calls (`internal/workspace/tfe_types.go:452-461`). Each call runs
`generateRequest`, which resolves the parent organization and fetches the
workspace policy (`internal/authz/authorizer.go`).

`bench/otf/query_count.py` counts Postgres transactions per request
(`pg_stat_database`):

| Request | Transactions |
|---|---|
| List 50 workspaces | **1,005** |
| List 10 workspaces | 206 |
| Get 1 workspace | 25 |

That is about 20 per workspace, and it explains:

- 152 ms per request with one client;
- 18 req/s at 16 clients;
- Postgres using more CPU than otfd under load.

A single workspace read takes about 4 ms. The UI and the `tfe` provider
(`tfe_workspace` data sources) both list workspaces, so the cost shows up with
a few hundred workspaces.

**For Cairn:** list endpoints are one query per page, with permissions
computed once per request and not per row (SPEC R4).

### 4. OTF's run path is already efficient

OTF adds about 0.27 s per run on top of OpenTofu (create → applied 0.44 s,
against 0.17 s for tofu's init, plan and apply). It runs 5 runs in parallel in 1.01 s with its default
`--concurrency 5`. A no-op plan takes 0.24 s. This is the bar Cairn has to
beat, not Terrakube's.

### 5. Atlantis is the lightest, but leaks locks on Forgejo/Gitea and does less

- Startup takes 0.24 s, idle is 10 MiB, and the PR loop is fast: plan comment
  1.6 s after the PR opens, apply result 0.8 s after the comment, and locks
  released 0.2 s after a merge.
- **Lock leak:** `ParseGiteaPullRequestEvent`
  (`server/events/event_parser.go`) maps a PR that is `closed` but not merged
  to `OtherPullEvent`, so its project locks are never released. Anyone else's
  PR touching that directory is then blocked until someone deletes the lock in
  Atlantis's UI. The benchmark merges PRs instead.
- Locks are per (repo, directory, workspace) across all PRs, so two PRs that
  touch the same project can't both plan.
- No state storage, no SSO (basic auth only), and no remote runners.

### 6. Operational papercuts we hit getting Terrakube to run

- The workspace list rejects `page[size]` ("Cannot paginate workspace"); the UI
  fetches every workspace and paginates in the browser.
- An organization admin can create the organization but not workspaces until
  they add a team grant.
- The UI's nginx config hard-codes `listen [::]:8080` and won't start on hosts
  without IPv6.
- Every job clones `terrakube-extensions` from GitHub. Engine versions are
  resolved through the API from `api.github.com`, which fails without internet
  access unless you run a mirror.
- Mounting only `~/.terraform-spring-boot/local` (the obvious choice for LOCAL
  storage) leaves a root-owned parent directory the executor can't write to.

## What it means for tesseract

| Constraint | OTF | Terrakube | Atlantis | Cairn MVP (measured) |
|---|---|---|---|---|
| Memory on a Pi already in swap | ~36 MiB with its own Postgres; ~13 MiB using the existing one | ≥ 3.3 GiB of limits to start; 2.4 GiB+ RSS if left uncapped | ~10 MiB | **1.6 MiB** idle; starts in a 6 MiB limit |
| Time to a PR plan | ~0.2 s + webhook | ~11 s, and runs queue | ~1.6 s | not built yet (budget ≤ tofu + 0.5 s); a run takes 0.19 s |
| Forgejo | ✔ | ✘ | ✔ (lock leak) | planned (M2) |
| Policies, drift, Vault/OpenBao credentials | ✘ | ✔ | partial | planned (M2–M3) |

## Caveats

- **x86, not arm64.** The 1-CPU cap is a rough stand-in for a Pi: it limits
  CPU time, not memory bandwidth or I/O. Absolute numbers on a Pi will be
  worse, while the ratios between tools should hold.
- **The fixture has no providers.** Real runs add provider downloads and API
  calls that dominate run time equally for every tool. These measurements
  isolate the orchestration overhead.
- **Five runs per median.** The spread was small (see min and max in the
  JSON), except for Terrakube's parallel runs, where the spread *is* the
  serial queue.
- **Atlantis was built from source on an Alpine and git base** because ghcr.io
  wasn't reachable from the host. The footprint table uses upstream's
  published image sizes (60 MiB amd64 and 56 MiB arm64 compressed), read from
  the registry. The locally built image was only used to run it.
- **Terrakube ran its smallest viable stack** (no MinIO, Traefik or LDAP), which
  only flatters it.

## Cairn MVP vs the three

Cairn 0.1.0 (`crates/cairnd`) was measured with the same harness
(`bench/cairn/run.py`). For runs and lists it makes **the same TFE API calls
with the same 0.1 s status polling** as the OTF driver. Its container is a
glibc build on `scratch`, about 5 MiB compressed, with the data directory on a
bind mount and OpenTofu 1.11.6 pre-seeded like the others. Cold starts and the
trivial read are medians of repeated back-to-back trials (`cold_start.py`,
`trivial_api.py`).

| All host CPUs | OTF | Terrakube | Atlantis | **Cairn** |
|---|---|---|---|---|
| Containers / compressed amd64 image | 2 / 35 MiB | 7 / 999 MiB | 1 / 60 MiB | **1 / 5 MiB** |
| Cold start → healthy (median of 10) | 0.42 s | 25.6 s (1 sample) | 0.35 s | **0.17 s** |
| … of which the app's own startup | 0.18 s | — | 0.05 s | **0.005 s** |
| Idle RSS, whole stack | 36 MiB | 2,405 MiB | 10 MiB | **1.6 MiB** |
| Idle CPU over 56 s | 10 ms (otfd) + Postgres | 1 s | 20 ms | **0 ms** |
| Smallest memory limit it starts in | 16 MiB | 3,328 MiB | 16 MiB | **6 MiB** (Docker's minimum) |
| List 50 workspaces: 1 client p50 | 152 ms | 99 ms | n/a | **0.5 ms** |
| List 50 workspaces: 16 clients | 18 req/s | 31 req/s | n/a | **8,640 req/s** (p99 4.6 ms) |
| Trivial authenticated read, 16 clients | — | — | 33,012 req/s (p99 3.1 ms) | **50,596 req/s** (p99 2.1 ms) |
| Create → applied, median of 5 | 0.44 s | 26.95 s | 0.57 s | **0.20 s** (0.19 s with long-poll) |
| No-op run | 0.24 s | 10.63 s | n/a | **0.21 s** |
| 5 runs at once | 1.01 s | 146.5 s | 1.09 s | **0.74 s** |
| Peak RSS, 5 runs at once (incl. tofu) | 154 MiB | 2,559 MiB | 103 MiB | **81 MiB** |
| CPU per run, whole stack | 0.66 s | 7.96 s | 0.60 s | **0.34 s** |

| 1-CPU budget | OTF | Terrakube | Atlantis | **Cairn** |
|---|---|---|---|---|
| Cold start → healthy | 0.64 s | 94.4 s | 0.26 s | **0.19 s** |
| List 50 workspaces: 16 clients | 5 req/s | 6 req/s | n/a | **4,468 req/s** |
| Trivial authenticated read, 16 clients | — | — | 28,255 req/s (p99 2.6 ms) | **44,175 req/s** (p99 2.1 ms) |
| Create → applied | 0.78 s | 28.9 s | 0.72 s | **0.51 s** |
| No-op run | 0.50 s | 11.1 s | n/a | **0.36 s** |
| 5 runs at once | 4.1 s | 147.5 s | 3.29 s | **2.41 s** |
| Peak RSS, 5 runs at once | 186 MiB | 1,714 MiB | 106 MiB | **81 MiB** |

**Not compared yet:** the PR workflow (Atlantis: plan comment 1.6 s after the
PR opens), because Cairn has no VCS integration yet, and arm64 images (Cairn
isn't cross-built yet).

### What made the difference

These were measured while building the MVP; each one is visible in the
history of `bench/results`.

| Change | Effect |
|---|---|
| No fixed sleeps or polling loops: phases advance on child exit, and waiters use long-poll or SSE | 0.19 s from create to applied, against 0.17 s of OpenTofu work |
| Turning off SQLite's memory statistics (`SQLITE_CONFIG_MEMSTATUS = 0`), whose global mutex serialized every reader connection | List throughput ×3.2 (3.1k → 9.9k req/s on the glibc build) |
| Typed serialization for TFE responses instead of `serde_json::Value` trees, with pre-sized buffers | Fewer allocations and copies on the hot path |
| The system allocator instead of mimalloc | Idle RSS 11.2 → 1.6 MiB, and 24 → 3.7 MiB after load, at a cost of ~25 % peak throughput |
| glibc instead of musl for the image | musl's `memcpy` halved list throughput (5.4k vs 9.9k req/s) |
| One `tofu apply -auto-approve` instead of `plan -out` + `apply` for auto-apply runs | Saves a process start and a plan-file round trip: 0.20 → 0.16 s per run with long-poll |
| `GOGC=50` for OpenTofu processes (configurable, `0` turns it off) | tofu's anonymous memory drops 26.6 → 18.7 MiB per process, for ~30 ms more per apply; peak with 5 parallel runs 104 → 81 MiB |
| Single-layer image, data dir created with the right owner | Faster `docker run`; the benchmark harness also stopped copying 113 MB right before each start, which had inflated the first cold-start numbers |

### Caveats specific to Cairn

- It implements less than the others today (no VCS, policies, UI or SSO); see
  the [README status table](../README.md#mvp-status). Those features have to
  arrive without breaking the budgets, which the harness will check.
- Its state lives in SQLite on the same host. OTF's numbers include a Postgres
  round trip per query, which is part of OTF's design, not an artifact.
- `GOGC=50` trades about 20 % more OpenTofu CPU for 30 % less memory. Even so,
  Cairn uses less CPU per run than any of the others, because the orchestrator
  itself does almost nothing.

