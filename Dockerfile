# rfx_p25 voice node.
#
# Two stages: a builder that needs cmake (libopus is compiled from source by
# opusic-sys) and a runtime that needs almost nothing, because libopus links
# statically and codec2 is pure Rust.
#
# Built for an Ubuntu host running Pelican. The node needs to reach FXServer's
# voice port outbound, and to be reachable on its control port from FXServer
# and later on its stream port from players' game clients.

FROM rust:1-bookworm AS builder

RUN apt-get update \
 && apt-get install -y --no-install-recommends cmake \
 && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Dependencies first, so a source-only change does not rebuild libopus.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs \
 && cargo build --release 2>/dev/null || true \
 && rm -rf src

COPY build.rs ./
COPY proto ./proto
COPY src ./src
RUN touch src/main.rs && cargo build --release


FROM debian:bookworm-slim

RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --no-create-home --uid 10001 voiced

COPY --from=builder /build/target/release/rfx-voiced /usr/local/bin/rfx-voiced

USER voiced

# FXServer's voice server - the tap connects OUT to this.
ENV VOICED_HOST=127.0.0.1 \
    VOICED_PORT=30120 \
    VOICED_USER="[999] radiotap" \
    VOICED_CONTROL=0.0.0.0:8787 \
    VOICED_STREAM=0.0.0.0:8788 \
    VOICED_TAP_CHANNEL=75534

# 8787 control, in from FXServer only.
# 8788 audio, in from PLAYERS' game clients - this one needs a public
#      allocation, not just a host-local binding.
EXPOSE 8787 8788

# The platform. When these are set the node pulls its server list from
# InteropHQ and caches the last good response; without them it runs on
# servers.json, which is how it works standalone.
ENV VOICED_PLATFORM_URL="" \
    VOICED_PLATFORM_KEY="" \
    VOICED_CACHE=/data/servers.cache.json

ENTRYPOINT ["/usr/local/bin/rfx-voiced"]
