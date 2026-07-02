# Build a fully-static musl binary, ship it on scratch.
FROM rust:1.92-alpine AS builder
RUN apk add --no-cache musl-dev
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY migrations ./migrations
# migrations are embedded into the binary at compile time (sqlx::migrate!).
RUN cargo build --release

FROM scratch
COPY --from=builder /app/target/release/shelfarr-rs /shelfarr-rs
COPY assets /assets
ENV DATA_DIR=/data
ENV BIND=0.0.0.0:8080
EXPOSE 8080
# /data is a mounted volume on the host (config DB + plugins live there).
VOLUME ["/data"]
ENTRYPOINT ["/shelfarr-rs"]
