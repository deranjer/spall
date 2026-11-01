"""Download page for the Spall sandbox server.

Standard library only. Serves one page and one zip per request. The zip holds the
Windows client, its audio assets, the current server fingerprint and join token,
and a connect.bat that starts the client already pointed at this server.
"""

import html
import io
import ipaddress
import os
import socket
import sys
import threading
import zipfile
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import urlsplit

RUN_DIR = os.environ.get("SPALL_RUN_DIR", "/run/spall")
CLIENT_DIR = os.environ.get("SPALL_CLIENT_DIR", "/opt/spall/client")
JOIN_TOKEN_FILE = os.environ.get("SPALL_JOIN_TOKEN_FILE", "/data/join.token")
PUBLIC_PORT = int(os.environ.get("SPALL_PUBLIC_PORT", "7777"))
PUBLIC_ADDRESS = os.environ.get("SPALL_PUBLIC_ADDRESS", "").strip()
WEB_PORT = int(os.environ.get("SPALL_WEB_PORT", "8080"))

CLIENT_EXE = "sandbox-client.exe"
CONNECT_BAT = """@echo off
rem Starts the Spall client and connects it to the server this bundle came from.
rem The fingerprint and join token files must stay next to this script.
cd /d "%~dp0"
if not exist logs mkdir logs
"{exe}" --interactive --connect {address} --server-fingerprint server.fingerprint --join-token-file join.token --late-join --stream-regions --log-json logs\\client.jsonl
if errorlevel 1 pause
"""
README = """Spall client

1. Extract this folder anywhere on a Windows PC with a supported GPU.
2. Run connect.bat. The client starts and joins the server at {address}.

Keep sandbox-client.exe, assets, server.fingerprint, join.token and connect.bat together.
Controls: WASD move, Space jump, mouse to look, Escape releases the mouse.
Diagnostics are written to logs\\client.jsonl.
"""


def read_text(name):
    with open(os.path.join(RUN_DIR, name), encoding="ascii") as handle:
        return handle.read().strip()


def split_host_port(value):
    """Split "host", "host:port", "1.2.3.4:port" or "[v6]:port" into (host, port)."""
    try:
        parts = urlsplit("//" + value)
        return parts.hostname, parts.port or PUBLIC_PORT
    except ValueError:
        raise ValueError(f"cannot parse address {value!r}; use an IPv4 address or host name, optionally with :port") from None


def server_address(host_header):
    """The ip:port clients should dial. The client accepts only an IPv4 address.
    A configured SPALL_PUBLIC_ADDRESS may be a host name and is resolved here. The
    request Host header is untrusted, so it is used only when it is already an IPv4
    literal and never triggers a DNS lookup."""
    if PUBLIC_ADDRESS:
        host, port = split_host_port(PUBLIC_ADDRESS)
        if not host:
            raise ValueError("SPALL_PUBLIC_ADDRESS is not a host name or IPv4 address")
        try:
            ip = str(ipaddress.IPv4Address(host))
        except ValueError:
            ip = socket.gethostbyname(host)
    else:
        # The Host port is the web port, not the game port.
        host, _ = split_host_port(host_header)
        port = PUBLIC_PORT
        try:
            ip = str(ipaddress.IPv4Address(host))
        except ValueError:
            raise ValueError("open this page by IPv4 address, or set SPALL_PUBLIC_ADDRESS") from None
    return f"{ip}:{port}"


_bundle_cache = {}
_bundle_lock = threading.Lock()


def build_bundle(host_header):
    """Zip the bundle once per (address, fingerprint, token); rebuilt when any changes."""
    exe_path = os.path.join(CLIENT_DIR, CLIENT_EXE)
    if not os.path.isfile(exe_path):
        raise FileNotFoundError("this server image has no client bundled")
    address = server_address(host_header)
    fingerprint = read_text("server.fingerprint")
    with open(JOIN_TOKEN_FILE, encoding="ascii") as handle:
        token = handle.read().strip()

    key = (address, fingerprint, token)
    with _bundle_lock:
        if key in _bundle_cache:
            return _bundle_cache[key], address
        _bundle_cache.clear()
        body = zip_bundle(exe_path, address, fingerprint, token)
        _bundle_cache[key] = body
    return body, address


def zip_bundle(exe_path, address, fingerprint, token):
    buffer = io.BytesIO()
    with zipfile.ZipFile(buffer, "w", zipfile.ZIP_DEFLATED) as archive:
        archive.write(exe_path, CLIENT_EXE)
        assets = os.path.join(CLIENT_DIR, "assets")
        for root, _, files in os.walk(assets):
            for name in files:
                full = os.path.join(root, name)
                archive.write(full, os.path.relpath(full, CLIENT_DIR))
        archive.writestr("server.fingerprint", fingerprint + "\n")
        archive.writestr("join.token", token + "\n")
        archive.writestr("connect.bat", CONNECT_BAT.format(exe=CLIENT_EXE, address=address).replace("\n", "\r\n"))
        archive.writestr("README.txt", README.format(address=address).replace("\n", "\r\n"))
    return buffer.getvalue()


def page(message):
    ready = os.path.isfile(os.path.join(CLIENT_DIR, CLIENT_EXE))
    server_up = os.path.isfile(os.path.join(RUN_DIR, "server.fingerprint"))
    button = (
        '<a class="button" href="/download/spall-client.zip">Download Windows client</a>'
        if ready and server_up
        else '<p class="note">The client is not ready yet. Refresh when the server has started.</p>'
        if ready
        else '<p class="note">This image has no client bundled.</p>'
    )
    return f"""<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Spall server</title>
<style>
:root {{ color-scheme: light dark; --bg: #f6f6f4; --fg: #1d1d1b; --accent: #2f6f4e; }}
@media (prefers-color-scheme: dark) {{ :root {{ --bg: #161714; --fg: #e8e8e4; --accent: #6fbf8e; }} }}
body {{ margin: 0; font: 16px/1.5 system-ui, sans-serif; background: var(--bg); color: var(--fg); }}
main {{ max-width: 36rem; margin: 0 auto; padding: 16px; }}
.button {{ display: inline-block; padding: 12px 18px; border-radius: 6px; background: var(--accent); color: #fff; text-decoration: none; }}
.note {{ opacity: 0.8; }}
code {{ word-break: break-all; }}
</style>
</head>
<body>
<main>
<h1>Spall server</h1>
<p>{html.escape(message)}</p>
{button}
<p>The download includes a connect.bat that joins this server. Run it on a Windows PC with a supported GPU.</p>
</main>
</body>
</html>
"""


class Handler(BaseHTTPRequestHandler):
    server_version = "spall-webui"

    def do_GET(self):
        path = urlsplit(self.path).path
        if path == "/":
            self.send_body(200, "text/html; charset=utf-8", page("Download the client and join the running world.").encode("utf-8"))
        elif path == "/healthz":
            self.send_body(200, "text/plain; charset=utf-8", b"ok\n")
        elif path == "/download/spall-client.zip":
            try:
                body, _ = build_bundle(self.headers.get("Host", ""))
            except (OSError, ValueError) as error:
                message = f"Not available yet: {error}"
                self.send_body(503, "text/html; charset=utf-8", page(message).encode("utf-8"))
                return
            self.send_body(
                200,
                "application/zip",
                body,
                extra=[("Content-Disposition", 'attachment; filename="spall-client.zip"')],
            )
        else:
            self.send_body(404, "text/plain; charset=utf-8", b"not found\n")

    def send_body(self, status, content_type, body, extra=()):
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        for name, value in extra:
            self.send_header(name, value)
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, format, *args):
        sys.stderr.write("webui: %s - %s\n" % (self.address_string(), format % args))


def main():
    server = ThreadingHTTPServer(("0.0.0.0", WEB_PORT), Handler)
    print(f"webui: listening on 0.0.0.0:{WEB_PORT}", flush=True)
    server.serve_forever()


if __name__ == "__main__":
    main()
