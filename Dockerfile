FROM rust:1-bookworm AS build
WORKDIR /src

# Cache the dependency graph before copying the source.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo "fn main() {}" > src/main.rs \
 && cargo build --release --locked \
 && rm -rf src target/release/shitmail-jmap-filter target/release/deps/shitmail_jmap_filter-*

COPY src ./src
COPY data ./data
RUN cargo build --release --locked

FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/*

COPY --from=build /src/target/release/shitmail-jmap-filter /usr/local/bin/shitmail-jmap-filter
COPY --from=build /src/data/public_suffix_list.dat /opt/shitmail-jmap-filter/public_suffix_list.dat

ENV STATE_DIR=/data \
    PSL_PATH=/opt/shitmail-jmap-filter/public_suffix_list.dat \
    HEALTHZ_PORT=8080
EXPOSE 8080
VOLUME ["/data"]
USER nobody:nogroup
CMD ["/usr/local/bin/shitmail-jmap-filter"]
