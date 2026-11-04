# Benchmark harness

These scripts measure OTF, Terrakube and Atlantis the same way, so their
numbers can be compared directly. They also define the budgets that Cairn must
meet (see `../docs/SPEC.md` §9). Results are in `results/`, and the write-up is
`../docs/benchmark-report.md`.

## What is measured

Each driver (`<tool>/run.py`) starts the tool's **whole required stack** in
Docker. Every container uses the host network and runs from an image that is
already pulled. The driver then runs the same phases for each tool:

| Phase | What happens | Recorded |
|---|---|---|
| Cold start | `docker run` until the first healthy HTTP response (for Terrakube, until all four services are healthy; they start at the same time, as with `docker compose up`) | seconds |
| Idle | 30 s to settle, then a 60 s window with no traffic | RSS and CPU time per container |
| API read | 50 workspaces exist. The workspace-list endpoint is called by 1 client (320 requests) and then by 16 clients (3,200 requests) using `hey` | req/s, p50/p95/p99, peak RSS |
| First run | One plan and apply on a fresh workspace (warms engine and git caches) | seconds |
| Serial runs | 5 × plan and apply, each on a fresh workspace (100 creates each) | create → planned, create → applied |
| No-op runs | 5 × plan on a workspace that is already applied | seconds |
| Parallel runs | 5 plans and applies started at once on 5 workspaces | wall time, peak RSS |
| PR workflow (Atlantis) | Open a PR on Gitea, wait for the plan comment, comment `atlantis apply`, wait for the result, merge | seconds |

The fixture (`fixtures/tofu-bench`) is 100 `terraform_data` resources. It needs
no providers, so the runs measure the orchestrator and not the network. Plain
`tofu` needs about 0.08 s to plan and 0.08 s to apply it (`results/baseline.json`).

Resources are read from **cgroup v1 accounting** (`lib/cgstat.py`) every
0.2 s, for every container in the stack:

- `rss` is anonymous memory, which the process really owns;
- `working_set` is usage minus inactive file cache, which is what `docker stats`
  shows;
- `cpu_ns` is cumulative CPU time.

`lib/report.py` summarises them per phase.

**Constrained mode.** With `BENCH_CPUS=1`, every container of a stack goes
under one parent cgroup whose shared CFS quota is 1 CPU. This roughly
approximates a small ARM board: a Pi 4 has four cores, each roughly a third to a
quarter as fast as this host's. The approximation is crude, and the conclusions
only rely on large differences.

## Keeping it fair

- **Same engine.** All three tools run OpenTofu 1.11.6 (OTF's default), with
  the binary pre-seeded:
  - OTF: copied into `--engine-bins-dir`;
  - Atlantis: baked in as `tofu1.11.6` with `--tf-download=false`;
  - Terrakube: served from a local release mirror (`CustomTofuReleasesUrl`).

  The executor downloads it once in the warm-up run, which is excluded from the
  medians. Nothing touches the internet during measured phases.
- **Same VCS.** A local Gitea 1.24 (the same API as Forgejo) is the git source
  for Terrakube and the webhook source for Atlantis. It is shared
  infrastructure and isn't counted in any tool's resources. OTF runs are
  uploaded through its TFE API, which is what `tofu` does with a `cloud {}`
  block.
- **Smallest working stack for each tool.** For Terrakube that means dropping
  MinIO, Traefik and OpenLDAP from the reference Compose file and using its
  LOCAL storage on a shared volume. This only makes Terrakube look lighter.
- **Atlantis stores no state** (local backend). OTF and Terrakube upload state
  after every apply. With 100 `terraform_data` resources the state is about
  40 KB, so this favours Atlantis only slightly. No-op runs aren't measured for
  Atlantis, because without a backend every plan starts from empty state.
- **Atlantis image.** Upstream only publishes to ghcr.io, which this host could
  not pull from. `atlantis/Dockerfile` builds v0.49.0 from the pinned submodule
  on Alpine with git, mirroring the `alpine` stage of upstream's Dockerfile.
  `results/image-sizes.jsonl` has upstream's real registry sizes.

## Things the drivers had to work around

These are real findings, not just setup notes:

| Tool | Issue | Where |
|---|---|---|
| Terrakube | The workspace list can't be paginated (`"Cannot paginate workspace"`); the UI fetches everything. | `terrakube/run.py`, `create_org` / API load |
| Terrakube | An org admin still needs a team grant before they can create workspaces. | `terrakube/run.py`, `create_org` |
| Terrakube | The UI's nginx config hard-codes `[::]:8080` and won't start on IPv4-only hosts. | `terrakube/run.py`, `start_terrakube` |
| Terrakube | The executor clones the extensions repo from GitHub on every job, and resolves engine versions from the GitHub releases API through the API server. | the local mirror in `terrakube/run.py` |
| Atlantis | Gitea/Forgejo PRs that are **closed without merging** never release their locks: `ParseGiteaPullRequestEvent` maps them to `OtherPullEvent`. | `atlantis/run.py`, `pr_flow` (it merges instead) |
| Atlantis | Locks are per (repo, dir, workspace) across PRs, so parallel runs need separate directories. | `atlantis/run.py`, parallel phase |

## Reproducing

Requirements:

- Docker with cgroup v1 (or adapt `lib/cgstat.py` for v2);
- Go, to build Atlantis;
- Python 3.10+;
- `hey` (`go install github.com/rakyll/hey@latest`);
- an OpenTofu 1.11.6 binary at `$BENCH_WORK/tofu/1.11.6/tofu`.

```sh
git submodule update --init --depth 1 cairn/upstream/*
export BENCH_WORK=/var/tmp/cairn-bench
mkdir -p $BENCH_WORK/tofu/1.11.6 && (cd $BENCH_WORK/tofu/1.11.6 && \
  curl -sSLO https://github.com/opentofu/opentofu/releases/download/v1.11.6/tofu_1.11.6_linux_amd64.zip && \
  unzip tofu_1.11.6_linux_amd64.zip tofu)
cd cairn/bench && ./run-all.sh        # ~45 min: all three tools, then all three under a 1-CPU budget
ONLY=otf CPU_MODES= ./run-all.sh      # just one tool, unconstrained
```

Each driver accepts `IDLE_SECONDS`, `N_WORKSPACES`, `SERIAL_RUNS`,
`PARALLEL_RUNS` (and `PR_RUNS` for Atlantis) for quicker smoke runs, and
`--keep` to leave the containers running afterwards.
