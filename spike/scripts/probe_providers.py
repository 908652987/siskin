#!/usr/bin/env python3
"""Pre-auth IMAP capability probe for mainstream providers.

No credentials are used: this only verifies reachability, TLS and the
capability set advertised in the greeting. Any real sync behaviour still
requires an authorized account.
"""
import imaplib
import json
import socket
import ssl
import sys

HOSTS = {
    "gmail": "imap.gmail.com",
    "outlook": "outlook.office365.com",
    "icloud": "imap.mail.me.com",
    "fastmail": "imap.fastmail.com",
    "yahoo": "imap.mail.yahoo.com",
}


def probe(name: str, host: str) -> dict:
    ctx = ssl.create_default_context()
    try:
        conn = imaplib.IMAP4_SSL(host, 993, ssl_context=ctx, timeout=12)
        caps = [c.decode() if isinstance(c, bytes) else str(c) for c in conn.capabilities]
        result = {
            "provider": name,
            "host": host,
            "ok": True,
            "capabilities": caps,
            "has_idle": any(c.upper() == "IDLE" for c in caps),
            "has_condstore": any(c.upper() == "CONDSTORE" for c in caps),
            "has_qresync": any(c.upper() == "QRESYNC" for c in caps),
            "has_move": any(c.upper() == "MOVE" for c in caps),
            "auth": [c for c in caps if c.upper().startswith("AUTH=")],
            "greeting": conn.welcome.decode(errors="replace") if isinstance(conn.welcome, bytes) else str(conn.welcome),
        }
        conn.shutdown()
        return result
    except (socket.timeout, ssl.SSLError, OSError, imaplib.IMAP4.error) as exc:
        return {"provider": name, "host": host, "ok": False, "error": f"{type(exc).__name__}: {exc}"}


def main() -> int:
    out = [probe(name, host) for name, host in HOSTS.items()]
    print(json.dumps(out, indent=2))
    return 0 if all(r["ok"] for r in out) else 1


if __name__ == "__main__":
    sys.exit(main())
