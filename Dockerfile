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
# Default bundled plugins (gutenberg.wasm + reader viewer). Prebuilt; committed to repo.
COPY plugins /plugins
ENV DATA_DIR=/data
ENV BIND=0.0.0.0:8080
ENV PLUGINS_DIR=/plugins
EXPOSE 8080
# /data is a mounted volume on the host (config DB + downloaded books live there).
VOLUME ["/data"]
ENTRYPOINT ["/shelfarrs"]
