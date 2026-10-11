# consistent-relay — a rendezvous point so a phone can reach a loopback-bound
# gateway. Stateless: scale it to one small instance and restart freely.
FROM rust:1-slim AS build
WORKDIR /src
COPY . .
RUN cargo build --release -p consistent-world-relay

FROM debian:stable-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/consistent-relay /usr/local/bin/consistent-relay
# CONSISTENT_RELAY_SECRET is required and has no default — the process refuses
# to start without one, so an open relay cannot be deployed by accident.
ENV PORT=8787
EXPOSE 8787
CMD ["consistent-relay"]
