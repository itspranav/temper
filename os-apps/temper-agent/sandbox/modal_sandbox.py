#!/usr/bin/env python3
"""Modal sandbox provisioner for Temper Agent.

Creates a Modal sandbox with an HTTP server inside that implements the sandbox API:
  GET  /v1/fs/file?path=...   → read file
  PUT  /v1/fs/file?path=...   → write file
  POST /v1/processes/run       → execute bash command
  GET  /health                 → health check

The sandbox server code is injected into the sandbox at creation time.
Returns the tunnel URL that the tool_runner WASM module uses.

Usage:
  modal setup  # one-time auth setup
  python3 modal_sandbox.py [--timeout 600]
"""

import argparse
import json
import sys
import time

try:
    import modal
except ImportError:
    print("Error: modal package not installed. Run: pip install modal", file=sys.stderr)
    sys.exit(1)

# The sandbox server code — injected into the Modal sandbox container
SANDBOX_SERVER_CODE = r'''
import json, os, subprocess, sys
from http.server import HTTPServer, BaseHTTPRequestHandler
from urllib.parse import urlparse, parse_qs

WORKDIR = "/workspace"

class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        parsed = urlparse(self.path)
        if parsed.path == "/health":
            self._json(200, {"status": "ok"})
            return
        if parsed.path == "/v1/fs/file":
            path = parse_qs(parsed.query).get("path", [None])[0]
            if not path:
                self._json(400, {"error": "missing path"})
                return
            full = path if path.startswith("/") else os.path.join(WORKDIR, path)
            if not os.path.isfile(full):
                self._json(404, {"error": f"not found: {path}"})
                return
            with open(full, "r") as f:
                content = f.read()
            self.send_response(200)
            self.send_header("Content-Type", "text/plain")
            self.end_headers()
            self.wfile.write(content.encode())
            return
        self._json(404, {"error": "unknown"})

    def do_PUT(self):
        parsed = urlparse(self.path)
        if parsed.path == "/v1/fs/file":
            path = parse_qs(parsed.query).get("path", [None])[0]
            if not path:
                self._json(400, {"error": "missing path"})
                return
            full = path if path.startswith("/") else os.path.join(WORKDIR, path)
            length = int(self.headers.get("Content-Length", 0))
            body = self.rfile.read(length).decode() if length else ""
            os.makedirs(os.path.dirname(full), exist_ok=True)
            with open(full, "w") as f:
                f.write(body)
            self._json(200, {"status": "ok", "path": path})
            return
        self._json(404, {"error": "unknown"})

    def do_POST(self):
        parsed = urlparse(self.path)
        if parsed.path == "/v1/processes/run":
            length = int(self.headers.get("Content-Length", 0))
            body = self.rfile.read(length).decode() if length else "{}"
            req = json.loads(body)
            cmd = req.get("command", "")
            cwd = req.get("workdir", WORKDIR)
            if not cmd:
                self._json(400, {"error": "missing command"})
                return
            try:
                r = subprocess.run(cmd, shell=True, capture_output=True, text=True, timeout=60, cwd=cwd)
                self._json(200, {"stdout": r.stdout, "stderr": r.stderr, "exit_code": r.returncode})
            except subprocess.TimeoutExpired:
                self._json(200, {"stdout": "", "stderr": "timeout", "exit_code": -1})
            return
        self._json(404, {"error": "unknown"})

    def _json(self, status, data):
        body = json.dumps(data).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, fmt, *args):
        pass  # suppress logs

os.makedirs(WORKDIR, exist_ok=True)
server = HTTPServer(("0.0.0.0", 8080), Handler)
print("Sandbox server ready on :8080", flush=True)
server.serve_forever()
'''


def create_sandbox(timeout: int = 600) -> dict:
    """Create a Modal sandbox with the HTTP server running inside."""
    app = modal.App.lookup("temper-sandbox-broker", create_if_missing=True)

    image = modal.Image.debian_slim(python_version="3.12")

    sb = modal.Sandbox.create(
        "python3", "-c", SANDBOX_SERVER_CODE,
        image=image,
        timeout=timeout,
        encrypted_ports=[8080],
        cpu=1.0,
        memory=512,
        app=app,
    )

    # Wait for tunnel to be ready
    print("Waiting for sandbox tunnel...", file=sys.stderr)
    for _ in range(30):
        tunnels = sb.tunnels()
        if 8080 in tunnels:
            tunnel = tunnels[8080]
            url = tunnel.url
            print(f"Sandbox ready: {url}", file=sys.stderr)
            return {
                "sandbox_id": sb.object_id,
                "sandbox_url": url,
                "tunnel_port": 8080,
            }
        time.sleep(1)

    raise RuntimeError("Sandbox tunnel not ready after 30 seconds")


def main():
    parser = argparse.ArgumentParser(description="Create Modal sandbox for Temper Agent")
    parser.add_argument("--timeout", type=int, default=600, help="Sandbox timeout in seconds")
    parser.add_argument("--json", action="store_true", help="Output JSON")
    args = parser.parse_args()

    result = create_sandbox(args.timeout)

    if args.json:
        print(json.dumps(result))
    else:
        print(f"Sandbox ID:  {result['sandbox_id']}")
        print(f"Sandbox URL: {result['sandbox_url']}")
        print(f"\nUse this URL as sandbox_url when configuring the agent.")


if __name__ == "__main__":
    main()
