# ---- build ---------------------------------------------------------------
FROM rust:1-bookworm AS build
WORKDIR /src
# Cache dependencies first.
COPY Cargo.toml Cargo.lock ./
COPY crates/tzibbur-api/Cargo.toml crates/tzibbur-api/Cargo.toml
COPY crates/bridge/Cargo.toml crates/bridge/Cargo.toml
RUN mkdir -p crates/tzibbur-api/src crates/bridge/src \
 && echo 'pub fn _p() {}' > crates/tzibbur-api/src/lib.rs \
 && echo 'fn main() {}' > crates/bridge/src/main.rs \
 && cargo build --release -p tzibbur-telegram-bridge \
 && rm -rf crates/tzibbur-api/src crates/bridge/src
COPY crates ./crates
RUN touch crates/tzibbur-api/src/lib.rs crates/bridge/src/main.rs \
 && cargo build --release -p tzibbur-telegram-bridge

# ---- runtime -------------------------------------------------------------
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl && rm -rf /var/lib/apt/lists/* \
 && useradd -r -u 10001 -d /data bridge && mkdir -p /data && chown bridge:bridge /data
COPY --from=build /src/target/release/tzibbur-telegram-bridge /usr/local/bin/tzibbur-telegram-bridge
USER bridge
ENV BRIDGE_DATA_DIR=/data BRIDGE_LISTEN=0.0.0.0:8080 RUST_LOG=info
VOLUME ["/data"]
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=5s --start-period=20s CMD curl -fsS http://127.0.0.1:8080/health || exit 1
ENTRYPOINT ["/usr/local/bin/tzibbur-telegram-bridge"]
