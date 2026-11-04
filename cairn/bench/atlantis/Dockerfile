# Runtime image for Atlantis v0.49.0 built from the pinned submodule.
# Upstream publishes images only to ghcr.io, which the benchmark host could
# not pull from, so this mirrors the `alpine` stage of
# cairn/upstream/atlantis/Dockerfile on top of alpine/git (Alpine + git +
# openssh; the Alpine package CDN was also unreachable from the host).
# Differences from upstream: no conftest, git-lfs, dumb-init or curl (unused
# by this benchmark), and tofu is baked in as tofu<version> so
# --tf-download=false works, matching the pre-seeded engine for OTF/Terrakube.
FROM alpine/git@sha256:ac6c828f15d82775d204d3d75112e1d34af8945da80ccf922713c9891ba68e7a
ARG TOFU_VERSION=1.11.6
RUN addgroup -g 1000 atlantis && adduser -u 100 -S -G atlantis atlantis && \
    chown atlantis:root /home/atlantis/ && chmod u+rwx /home/atlantis/
COPY atlantis /usr/local/bin/atlantis
COPY tofu /usr/local/bin/tofu${TOFU_VERSION}
USER atlantis
WORKDIR /home/atlantis
ENTRYPOINT ["/usr/local/bin/atlantis"]
CMD ["server"]
