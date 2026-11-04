# cairn

Research and specification for **Cairn**, a lightweight OpenTofu automation
server and CLI written in Rust. The aim is OTF's footprint, Terrakube's
features, Atlantis's PR workflow, and Spacelift's model and CLI ergonomics,
with an MCP server for agents.

| Path | What |
|---|---|
| [`docs/SPEC.md`](docs/SPEC.md) | The specification: concepts, behaviour, APIs, CLI, MCP server, security, budgets, Rust implementation plan and milestones |
| [`docs/benchmark-report.md`](docs/benchmark-report.md) | What we measured and what it means |
| [`bench/`](bench/) | Reproducible benchmark harness (Docker + cgroup accounting) and raw results |
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
