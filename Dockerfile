# Consistent CEO gateway — multi-stage Rust build → slim Debian runtime.
#
# One container, nothing else to run: the database is embedded and lives in
# the /var/lib/consistent-world volume with everything else.
#
#   docker run -d --name consistent -p 127.0.0.1:18800:18800 \
#     -v consistent:/var/lib/consistent-world ghcr.io/al-baqshi/consistent.ceo
#
# then open http://127.0.0.1:18800. Publish the port on 127.0.0.1 as above: the
# image generates its own machine token (CONSISTENT_WORLD_PROVISION_TOKEN=1),
# which is right for your own computer and wrong for a server other people
# reach. For that, set GATEWAY_AUTH_TOKEN yourself and unset the opt-in.
#
# `docker compose up` runs the same image against a separate SurrealDB server.

# --- Dashboard (SvelteKit static SPA) -----------------------------------------
# crates/gateway/build.rs copies sveltfront/build into crates/gateway/dashboard_build
# for include_dir!, so the SPA must be built BEFORE cargo runs. Without this stage
# the image serves the "Dashboard not embedded" fallback page.
FROM node:20-bookworm-slim AS ui
WORKDIR /app/sveltfront
COPY sveltfront/package*.json ./
RUN npm ci
COPY sveltfront ./
# Installer links on the /download page default to the GitHub releases;
# override with --build-arg for a fork or a mirror.
ARG VITE_DOWNLOAD_BASE=
ENV VITE_DOWNLOAD_BASE=$VITE_DOWNLOAD_BASE
RUN npm run build

# --- Builder ------------------------------------------------------------------
FROM rust:1-bookworm AS builder
WORKDIR /app

# Install build-time native deps (none beyond the toolchain right now, kept here
# for easy future additions like libsqlite3-dev etc.).
RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config \
    libasound2-dev \
    && rm -rf /var/lib/apt/lists/*

# Workspace + manifests first for layer caching.
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY plugins ./plugins
# Compiled into the binary via include_str! (director/agent seed).
COPY agents_seed.json ./

# The built SPA, copied AFTER the Rust sources so nothing clobbers it. build.rs
# looks for it at ../../sveltfront/build relative to crates/gateway — i.e. exactly
# /app/sveltfront/build with this WORKDIR.
COPY --from=ui /app/sveltfront/build ./sveltfront/build

# Build only the gateway binary, with the dashboard embedded. Note cargo runs
# rustc from the workspace root (/app), which is what makes the
# include_dir!("crates/gateway/dashboard_build") path in dashboard.rs resolve.
RUN cargo build --release -p consistent-world-gateway --features dashboard

# --- Runtime ------------------------------------------------------------------
FROM debian:bookworm-slim

# ca-certificates: outbound HTTPS to LLM providers.
# libasound2: dynamic dep of the audio crate (voice features); harmless if unused.
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    libasound2 \
    && rm -rf /var/lib/apt/lists/*

# Non-root user for the gateway process.
RUN useradd --system --uid 1001 --home /var/lib/consistent-world --create-home consistent-world

COPY --from=builder /app/target/release/consistent-world /usr/local/bin/consistent-world

# Container-tuned default config: embedded database in the volume, Ollama on
# the host via host.docker.internal. Compose points it at a SurrealDB server
# with SURREAL_URL instead.
COPY docker/consistent-world.toml /etc/consistent-world/consistent_world.toml

ENV CONSISTENT_WORLD_CONFIG=/etc/consistent-world/consistent_world.toml
# Generate a machine token into the volume on first start (see header).
ENV CONSISTENT_WORLD_PROVISION_TOKEN=1
USER consistent-world
VOLUME /var/lib/consistent-world
EXPOSE 18800

HEALTHCHECK --interval=10s --timeout=3s --start-period=20s --retries=3 \
    CMD ["/bin/sh", "-c", "/usr/local/bin/consistent-world --version || exit 1"]

ENTRYPOINT ["/usr/local/bin/consistent-world"]
CMD ["start"]
