# Spall sandbox server + download web UI.
#
# Build context is the repository root:
#   docker build -f docker/Dockerfile -t spall-server .
#
# docker/client/ must hold the Windows client bundle (sandbox-client.exe and
# assets/audio). The release workflow fills it from a Windows build. Without it
# the image still runs the server, and the web UI reports that no client is bundled.

ARG SERVER_SOURCE=builder

FROM rust:1.96.1-bookworm AS builder
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p sandbox --bin sandbox-server \
    && cp target/release/sandbox-server /sandbox-server

# The release workflow builds the server binary with a cached cargo build and
# builds this image with --build-arg SERVER_SOURCE=prebuilt, which skips the
# `builder` stage above; docker/server-bin/sandbox-server must exist then.
FROM scratch AS prebuilt
COPY docker/server-bin/sandbox-server /sandbox-server

# `COPY --from` cannot expand a variable, so pick the source through a stage.
FROM ${SERVER_SOURCE} AS server

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends python3 ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=server /sandbox-server /usr/local/bin/sandbox-server
COPY docker/webui/app.py /opt/spall/webui/app.py
COPY docker/entrypoint.sh /usr/local/bin/entrypoint.sh
COPY docker/client/ /opt/spall/client/
# The winter scenario loads the cabin, tower and shed from fixtures/ at the path baked in
# at build time: /src/examples/sandbox/../../fixtures. Recreate that layout.
COPY fixtures/ /src/fixtures/
RUN mkdir -p /src/examples/sandbox
RUN chmod +x /usr/local/bin/entrypoint.sh     && useradd --system --home-dir /data --shell /usr/sbin/nologin spall     && mkdir -p /data /run/spall     && chown spall /data /run/spall
USER spall

# 7777/udp: QUIC game traffic. 8080/tcp: download web UI.
EXPOSE 7777/udp 8080/tcp
VOLUME ["/data"]
HEALTHCHECK --interval=30s --timeout=3s --start-period=20s \
    CMD python3 -c "import urllib.request as u; u.urlopen('http://127.0.0.1:8080/healthz', timeout=2)" || exit 1
ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
