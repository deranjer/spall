# Spall server container

One image runs the sandbox server and a small download page. Players open the page,
download a Windows client bundle, and run its `connect.bat`, which joins the server.

## Run the stack

```bash
cd docker
SPALL_PUBLIC_ADDRESS=your.host.or.ip docker compose up -d
```

- Open `http://your.host.or.ip:8080` to download the client.
- Forward **UDP 7777** as well as TCP 8080. The game uses QUIC over UDP.
- Logs: `docker compose logs -f spall-server`. Server JSON log: `/data/logs/server.jsonl`.

Settings are environment variables in [compose.yaml](compose.yaml):
`SPALL_PUBLIC_ADDRESS` (IPv4 address or host name, optional `:port`; required unless you open the page by IPv4 address), `SPALL_SEED`,
`SPALL_WORLD_SIZE` (cells of 0.25 m; multiple of 32, 128 to 4096), `SPALL_SEASON`,
`SPALL_MAX_CLIENTS`. The world, join token and logs live in the `spall-data` volume.

## Security

The join token is the only credential. The download page gives it to anyone who can
reach port 8080, so anyone who can reach the page can play. Put the page behind a
VPN or reverse-proxy authentication if the server is not for public play. Rotate the
token by deleting `join.token` from the volume and restarting; players must download
a new bundle.

## Build the image

The image bundles the Windows client, staged in `docker/client/` (not in git):

```powershell
cargo build --release -p sandbox --features client --bin sandbox-client
New-Item -ItemType Directory -Force docker/client/assets | Out-Null
Copy-Item target/release/sandbox-client.exe docker/client/
Copy-Item -Recurse examples/sandbox/assets/audio docker/client/assets/
docker build -f docker/Dockerfile -t spall-server .
```

Without `docker/client/`, the server still runs and the download page reports that no
client is bundled.

## Release

The **release-server** workflow (Actions tab, manual trigger, from `main` only) builds
the Windows client, then builds and pushes `ghcr.io/<owner>/spall-server` with the
version you enter and `latest`.

## Limits

- The client bundle is Windows-only. The GPU client does not run in the container.
- IPv6 is not supported; the client dials IPv4 only.
- The client connects by IP address. The download page resolves a host name once,
  when it builds the bundle.
- The server has no per-player credentials; the shared join token gates entry.
- Not verified here: a full image build and an end-to-end join from another machine.
