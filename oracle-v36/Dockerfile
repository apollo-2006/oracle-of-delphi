# Multi-stage build for Oracle of Delphi — produces a slim runtime image with the
# core orchestrator + actuator daemon. The C++ audio engine and the HUD are
# built too but typically run on the host (audio needs the sound device; the
# HUD is served to a browser). ROCm/HIP is layered in a separate GPU image
# (see docs/DEPLOYMENT.md); this base runs the CPU/mock path so it boots
# anywhere for CI and smoke tests.

# ---- Rust build stage ----
FROM rust:1.83-slim AS rust-build
RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config libssl-dev cmake g++ && rm -rf /var/lib/apt/lists/*
WORKDIR /build
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY oracle-ipc ./oracle-ipc
COPY oracle-core ./oracle-core
COPY oracle-actd ./oracle-actd
RUN cargo build --release --bin oracle-core --bin oracle-actd

# ---- C++ audio build stage ----
FROM debian:bookworm-slim AS cpp-build
RUN apt-get update && apt-get install -y --no-install-recommends \
    cmake g++ libasound2-dev && rm -rf /var/lib/apt/lists/*
WORKDIR /build
COPY oracle-audio ./oracle-audio
RUN cmake -B oracle-audio/build -S oracle-audio -DORACLE_WITH_ALSA=ON \
    && cmake --build oracle-audio/build -j

# ---- Runtime image ----
FROM debian:bookworm-slim AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends \
    libasound2 ca-certificates && rm -rf /var/lib/apt/lists/*
RUN useradd -m -u 10001 oracle
COPY --from=rust-build /build/target/release/oracle-core /usr/local/bin/
COPY --from=rust-build /build/target/release/oracle-actd /usr/local/bin/
COPY --from=cpp-build /build/oracle-audio/build/oracle-audio /usr/local/bin/
COPY deploy/oracle.container.toml /etc/oracle/oracle.toml
USER oracle
ENV ORACLE_DB=/home/oracle/oracle.db
ENV RUST_LOG=oracle_core=info
EXPOSE 8770
# Default: run the orchestrator. actd runs as a sibling container/service.
ENTRYPOINT ["oracle-core"]
CMD ["run", "--config", "/etc/oracle/oracle.toml"]
