# syntax=docker/dockerfile:1.7
#
# Multi-arch image (linux/amd64 + linux/arm64) built fast on any one machine:
# - the web UI is arch-independent, so it's built once on the build machine's own platform;
# - Rust is cross-compiled natively with cargo-zigbuild (no QEMU emulation), with the Cargo
#   registry and target dir kept in BuildKit cache mounts, so rebuilds only compile what changed;
# - the runtime stage has no RUN steps, so nothing executes under emulation.
#
#   docker buildx build --platform linux/amd64,linux/arm64 -t listaculous .

ARG ZIGBUILD_IMAGE=ghcr.io/rust-cross/cargo-zigbuild@sha256:eb5578cfc06e070a09deb7b5852474bd128a4e561f9f123261b5ed2405e478b1
ARG NODE_IMAGE=node:22-alpine@sha256:fe4f6d74e20370acb04ad889077d14d0f64a9c8ea6a42bc60cddf1e844699914
ARG ALPINE_IMAGE=alpine:3.20@sha256:d9e853e87e55526f6b2917df91a2115c36dd7c696a35be12163d44e6e2a4b6bc

# ---- web (built once, on the build machine's platform) ----
FROM --platform=$BUILDPLATFORM ${NODE_IMAGE} AS web
WORKDIR /web
COPY web/package.json web/package-lock.json ./
RUN --mount=type=cache,id=lc-npm,target=/root/.npm npm ci --prefer-offline --no-audit --no-fund
COPY web/ ./
RUN npm run build

# ---- deps (arch-independent, so BuildKit runs it once for all platforms) ----
# Fetches every crate into an ordinary image layer (not a cache mount): it's rebuilt only when
# Cargo.lock changes, can't be garbage-collected out from under a cached layer, and the per-arch
# builds below read it concurrently without racing.
FROM --platform=$BUILDPLATFORM ${ZIGBUILD_IMAGE} AS deps
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src && echo 'fn main() {}' > src/main.rs && cargo fetch --locked && rm -rf src

# ---- server (cross-compiled for the target platform) ----
FROM deps AS server
ARG TARGETARCH
COPY src ./src
COPY migrations ./migrations
COPY openapi.yaml ./openapi.yaml
# Dependencies come from the deps layer (offline); each arch keeps its own target-dir cache, so
# only changed code recompiles.
RUN --mount=type=cache,id=lc-target-${TARGETARCH},target=/src/target \
    set -eu; \
    case "$TARGETARCH" in \
      amd64) TRIPLE=x86_64-unknown-linux-musl ;; \
      arm64) TRIPLE=aarch64-unknown-linux-musl ;; \
      *) echo "unsupported arch $TARGETARCH" >&2; exit 1 ;; \
    esac; \
    CARGO_NET_OFFLINE=true cargo zigbuild --release --locked --target "$TRIPLE"; \
    mkdir -p /out /data; \
    cp "target/$TRIPLE/release/listaculous" /out/listaculous

# ---- runtime (no RUN steps: nothing runs under emulation) ----
FROM ${ALPINE_IMAGE}
LABEL org.opencontainers.image.title="Listaculous" \
      org.opencontainers.image.description="Lightweight, self-hosted newsletter manager" \
      org.opencontainers.image.source="https://github.com/listaculous/listaculous" \
      org.opencontainers.image.licenses="AGPL-3.0-or-later"
COPY --from=server /out/listaculous /usr/local/bin/listaculous
COPY --from=web /web/dist /app/static
# Writable data directory owned by the unprivileged runtime user.
COPY --from=server --chown=10001:10001 /data /data
USER 10001:10001
ENV STATIC_DIR=/app/static \
    DATABASE_URL=sqlite:///data/listaculous.db?mode=rwc \
    LISTACULOUS_BIND=0.0.0.0:8080
VOLUME /data
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=3s CMD wget -qO- http://127.0.0.1:8080/healthz || exit 1
CMD ["listaculous"]
