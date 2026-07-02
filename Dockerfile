# Build a fully-static musl binary, ship it on scratch.
FROM rust:1.92-alpine AS builder
RUN apk add --no-cache musl-dev
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY src ./src
COPY migrations ./migrations
# migrations are embedded into the binary at compile time (sqlx::migrate!).
RUN cargo build --release

FROM scratch
COPY --from=builder /app/target/release/shelfarrs /shelfarrs
COPY assets /assets
# Read-only bundled default plugins, seeded into the /data volume on first run.
COPY plugins /app/plugins
ENV BIND=0.0.0.0:8080
# Everything mutable lives under /data so one volume mount survives every redeploy:
#   /data/shelfarr.db  (progress, users, config)   /data/books  (downloads)
#   /data/plugins      (installed + seeded defaults)
ENV DATA_DIR=/data
ENV SEED_PLUGINS_DIR=/app/plugins
EXPOSE 8080
VOLUME ["/data"]
ENTRYPOINT ["/shelfarrs"]
