# Hub image: one process serving JMAP/SSE, the sync core, and the SQLCipher
# database on a mounted volume. TLS is terminated by a reverse proxy in front
# of this container; only expose the mapped port to that proxy.
#
# Build:  docker build -t siskin-hub .
# Run:    docker run --rm -p 127.0.0.1:18080:8080 \
#           -v /srv/siskin/data:/data \
#           -v /srv/siskin/secrets/master-key:/run/secrets/master-key:ro \
#           -e EYRIE_HUB_MASTER_KEY_FILE=/run/secrets/master-key \
#           siskin-hub
#
# The database lives at /data/hub.db, the file secret store at /data/secrets.
# Put the master key on a different volume (or inject it as above) and back up
# both independently: without the master key the database is unrecoverable.

# Vendored SQLCipher builds OpenSSL from source, which needs perl, make and a C
# toolchain; the rust image ships them and cmake is added explicitly.
FROM rust:1-bookworm AS builder
RUN sed -i 's|deb.debian.org|mirrors.ustc.edu.cn|g' /etc/apt/sources.list.d/debian.sources \
 && apt-get update \
 && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
      cmake pkg-config libdbus-1-dev \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY spike ./spike
# The keyring dependency links libdbus on Linux even when the Hub uses the file
# secret store. Cache mounts keep re-builds cheap; the binary is copied out of
# the cache mount so the next stage can still see it.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p mail-hub \
 && cp /src/target/release/mail-hub /usr/local/bin/mail-hub

FROM debian:bookworm-slim
RUN sed -i 's|deb.debian.org|mirrors.ustc.edu.cn|g' /etc/apt/sources.list.d/debian.sources \
 && apt-get update \
 && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
      ca-certificates curl libdbus-1-3 \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 10001 --home-dir /data --shell /usr/sbin/nologin siskin \
 && mkdir -p /data \
 && chown siskin:siskin /data

COPY --from=builder /usr/local/bin/mail-hub /usr/local/bin/mail-hub

USER siskin
ENV EYRIE_HUB_DB=/data/hub.db \
    EYRIE_HUB_BIND=0.0.0.0:8080 \
    EYRIE_HUB_SECRET_DIR=/data/secrets \
    EYRIE_HUB_SWEEP_SECONDS=120
# Binding 0.0.0.0 is the container network; publish it on loopback or behind
# the TLS proxy only.
EXPOSE 8080
VOLUME ["/data"]

HEALTHCHECK --interval=30s --timeout=3s --start-period=20s \
  CMD curl -fsS http://127.0.0.1:8080/healthz >/dev/null || exit 1

ENTRYPOINT ["mail-hub"]
