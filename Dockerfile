# ---- web ----
FROM node:22-alpine AS web
WORKDIR /web
COPY web/package.json web/package-lock.json ./
RUN npm ci
COPY web/ ./
RUN npm run build

# ---- server ----
FROM rust:1.92-alpine AS server
# perl + make build the vendored (statically linked) OpenSSL used by webauthn-rs.
RUN apk add --no-cache musl-dev perl make
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main(){}' > src/main.rs && cargo build --release && rm -rf src
COPY src ./src
COPY migrations ./migrations
RUN touch src/main.rs && cargo build --release

# ---- runtime ----
FROM alpine:3.20
RUN apk add --no-cache ca-certificates && adduser -D -u 10001 app && mkdir -p /data && chown app /data
COPY --from=server /src/target/release/listaculous /usr/local/bin/listaculous
COPY --from=web /web/dist /app/static
USER app
ENV STATIC_DIR=/app/static \
    DATABASE_URL=sqlite:///data/listaculous.db?mode=rwc \
    LISTACULOUS_BIND=0.0.0.0:8080
VOLUME /data
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=3s CMD wget -qO- http://127.0.0.1:8080/healthz || exit 1
CMD ["listaculous"]
