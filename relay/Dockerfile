# Origin-gated iroh relay for walkie-songie. Built from source (replaces the
# previous image, which just downloaded the stock iroh-relay release binary).
#
# TLS is terminated by traefik upstream, so this serves plain HTTP on :3340.
FROM rust:1-bookworm AS build
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /build/target/release/walkie-relay /usr/local/bin/walkie-relay
# Same non-root uid the previous relay image used.
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/walkie-relay"]
