# Runtime image for cairnd: the release binary plus the four glibc files it
# links against, on scratch (no shell, no package manager).
#
#   cargo build --release -p cairnd
#   docker build -t cairnd -f Dockerfile target/release
#
# glibc rather than musl: musl's memcpy/memmove halve API throughput (5.4k vs
# 9.9k req/s listing 50 workspaces on the bench host). For a fully static
# build use Dockerfile.static.
#
# OpenTofu is not bundled: cairnd downloads (checksum-verified) the version a
# stack asks for into /data/engines, or you mount one at
# /data/engines/<version>/tofu.
FROM debian:13-slim AS base
COPY --from=alpine:3.22 /etc/ssl/certs/ca-certificates.crt /out/etc/ssl/certs/ca-certificates.crt
COPY cairnd /out/cairnd
RUN mkdir -p /out/lib /out/lib64 /out/data /out/tmp \
 && cp /lib/x86_64-linux-gnu/libc.so.6 /lib/x86_64-linux-gnu/libm.so.6 /lib/x86_64-linux-gnu/libgcc_s.so.1 /out/lib/ \
 && cp /lib64/ld-linux-x86-64.so.2 /out/lib64/ \
 && chown 65532:65532 /out/data && chmod 1777 /out/tmp

# One layer: fewer overlay mounts, faster `docker run`.
FROM scratch
COPY --from=base /out/ /
ENV LD_LIBRARY_PATH=/lib CAIRN_DATA=/data CAIRN_LISTEN=0.0.0.0:8833
USER 65532:65532
VOLUME /data
EXPOSE 8833
ENTRYPOINT ["/cairnd"]
