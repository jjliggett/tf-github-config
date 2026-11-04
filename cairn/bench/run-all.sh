#!/usr/bin/env bash
# Run every benchmark sequentially (tools never overlap), then build SUMMARY.md.
#
#   BENCH_WORK=/scratch/dir ./run-all.sh            # full suite, then 1-CPU suite
#   ONLY="otf atlantis" CPU_MODES="" ./run-all.sh   # subset, unconstrained only
#
# Needs: docker (cgroup v1 for the sampler), go (to build Atlantis), python3,
# hey (go install github.com/rakyll/hey@latest) and an OpenTofu binary at
# $BENCH_WORK/tofu/$TOFU_VERSION/tofu.
set -euo pipefail
cd "$(dirname "$0")"
export BENCH_WORK="${BENCH_WORK:-/tmp/cairn-bench}"
export PATH="$PATH:$HOME/go/bin"
ONLY="${ONLY:-otf terrakube atlantis}"
CPU_MODES="${CPU_MODES-1}"
mkdir -p "$BENCH_WORK" results

python3 lib/footprint.py > /dev/null
# Registry manifests can be rate limited (Docker Hub 429); keep the previous file then.
if [ -z "${SKIP_IMAGE_SIZES:-}" ]; then
  if python3 lib/image_sizes.py leg100/otfd:0.6.3 leg100/otf-agent:0.6.3 \
      ghcr.io/runatlantis/atlantis:v0.49.0 azbuilder/api-server:2.33.3 \
      azbuilder/executor:2.33.3 azbuilder/open-registry:2.33.3 azbuilder/terrakube-ui:2.33.3 \
      dexidp/dex:v2.43.1 valkey/valkey:8-alpine postgres:16-alpine > "$BENCH_WORK/image-sizes.jsonl"; then
    mv "$BENCH_WORK/image-sizes.jsonl" results/image-sizes.jsonl
  else
    echo "warning: image size lookup failed; keeping results/image-sizes.jsonl" >&2
  fi
fi
python3 baseline.py

for tool in $ONLY; do
  echo "=== $tool (unconstrained)"; python3 "$tool/run.py"
done
for cpus in $CPU_MODES; do
  for tool in $ONLY; do
    echo "=== $tool (${cpus} CPU budget)"; BENCH_CPUS="$cpus" python3 "$tool/run.py"
  done
done
python3 lib/report.py
