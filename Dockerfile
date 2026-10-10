# syntax=docker/dockerfile:1.7
#
# Cross-compiles every release binary on the build machine's own CPU with cargo-zigbuild
# (no QEMU), then assembles tiny images from them.
#
#   docker build -t contraire .                                    # image for this machine
#   docker buildx build --platform linux/arm64 -t contraire:arm64 .  # e.g. for a Raspberry Pi
#   docker buildx build --target dist --output dist .              # all binaries into ./dist
#
# Run it (the Docker socket is root-equivalent; set a password when exposing the port):
#   docker run -d -p 8083:8083 -v /var/run/docker.sock:/var/run/docker.sock contraire
#   (SELinux hosts such as Fedora also need: --security-opt label=disable)

ARG ZIGBUILD_IMAGE=ghcr.io/rust-cross/cargo-zigbuild:latest

FROM --platform=$BUILDPLATFORM ${ZIGBUILD_IMAGE} AS build
ARG TARGETS="x86_64-unknown-linux-musl aarch64-unknown-linux-musl armv7-unknown-linux-musleabihf x86_64-pc-windows-gnu"
RUN rustup target add $TARGETS
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src src
COPY static static
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    set -eu; mkdir -p /out; \
    for t in $TARGETS; do \
      cargo zigbuild --release --locked --target "$t"; \
      case "$t" in \
        x86_64-unknown-linux-musl)       n=contraire-linux-amd64 ;; \
        aarch64-unknown-linux-musl)      n=contraire-linux-arm64 ;; \
        armv7-unknown-linux-musleabihf)  n=contraire-linux-armv7 ;; \
        x86_64-pc-windows-gnu)           n=contraire-windows-amd64.exe ;; \
      esac; \
      cp "target/$t/release/contraire$(case $t in *windows*) echo .exe;; esac)" "/out/$n"; \
    done; \
    cd /out && sha256sum * > SHA256SUMS && ls -l

# All binaries plus checksums: docker buildx build --target dist --output dist .
FROM scratch AS dist
COPY --from=build /out/ /

# Pick the binary for the image's platform (runs on the build platform, so no emulation).
FROM --platform=$BUILDPLATFORM busybox:1.36 AS pick
ARG TARGETARCH
ARG TARGETVARIANT
COPY --from=build /out/ /out/
RUN case "$TARGETARCH$TARGETVARIANT" in \
      amd64*) a=amd64 ;; arm64*) a=arm64 ;; armv7) a=armv7 ;; \
      *) echo "unsupported platform $TARGETARCH$TARGETVARIANT" >&2; exit 1 ;; \
    esac; cp /out/contraire-linux-$a /contraire

# Runtime: a single static binary, nothing else.
FROM scratch AS runtime
COPY --from=pick /contraire /contraire
EXPOSE 8083
ENTRYPOINT ["/contraire"]
