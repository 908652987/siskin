#!/usr/bin/env python3
"""Tiny logging HTTP server used to detect outbound requests from a WebView."""
import http.server
import os
import sys
import threading

ROOT = sys.argv[1]
LOG = sys.argv[2]
PORT = int(sys.argv[3]) if len(sys.argv) > 3 else 8765


class Handler(http.server.SimpleHTTPRequestHandler):
    def __init__(self, *args, **kwargs):
        super().__init__(*args, directory=ROOT, **kwargs)

    def log_message(self, fmt, *args):  # noqa: A003
        with open(LOG, "a", encoding="utf-8") as fh:
            fh.write(f"{self.path}\n")

    def do_GET(self):  # noqa: N802
        with open(LOG, "a", encoding="utf-8") as fh:
            fh.write(f"HIT {self.path}\n")
        return super().do_GET()


def main() -> None:
    open(LOG, "w").close()
    server = http.server.ThreadingHTTPServer(("127.0.0.1", PORT), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    print("ready", flush=True)
    try:
        threading.Event().wait()
    except KeyboardInterrupt:
        server.shutdown()


if __name__ == "__main__":
    main()
