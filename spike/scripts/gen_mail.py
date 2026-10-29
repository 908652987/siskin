#!/usr/bin/env python3
"""Generate a Dovecot Maildir with N synthetic messages for the IMAP spike."""
import argparse
import os
import random
import time

random.seed(7)

DOMAINS = ["example.com", "mail.test", "corp.test", "news.test"]
SUBJECTS = [
    "Weekly report", "Re: budget review", "Invoice #{n}", "Meeting notes",
    "Deployment failed", "Re: [PATCH] fix parser", "Your order shipped",
    "Quarterly planning", "Alert: disk usage", "Hello from {n}",
]


def build_message(i: int) -> bytes:
    sender = f"user{i % 97}@{random.choice(DOMAINS)}"
    rcpt = "vmail@example.com"
    subject = random.choice(SUBJECTS).format(n=i)
    ts = int(time.time()) - (10000 - i) * 60
    body = ("This is spike message %d. It exists only to measure IMAP sync cost. "
            "No personal data.\n") % i
    headers = [
        f"From: Sender {i % 97} <{sender}>",
        f"To: <{rcpt}>",
        f"Subject: {subject}",
        f"Date: {time.strftime('%a, %d %b %Y %H:%M:%S +0000', time.gmtime(ts))}",
        f"Message-ID: <spike-{i}@mail.test>",
        "MIME-Version: 1.0",
        'Content-Type: text/plain; charset="utf-8"',
        "Content-Transfer-Encoding: 8bit",
    ]
    return ("\r\n".join(headers) + "\r\n\r\n" + body).encode()


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--root", required=True, help="Maildir root")
    ap.add_argument("--count", type=int, default=10000)
    args = ap.parse_args()

    for sub in ("cur", "new", "tmp"):
        os.makedirs(os.path.join(args.root, sub), exist_ok=True)

    base = int(time.time()) - args.count
    for i in range(args.count):
        data = build_message(i)
        name = f"{base + i}.M{i}P1.spike,S={len(data)}"
        with open(os.path.join(args.root, "new", name), "wb") as fh:
            fh.write(data)
    print(f"wrote {args.count} messages to {args.root}/new")


if __name__ == "__main__":
    main()
