#!/usr/bin/env python3
"""Compare Rust `rrule` output against python-dateutil as an independent oracle."""
import json
import sys
from datetime import timezone

from dateutil.rrule import rrulestr


def norm(dt) -> str:
    if dt.tzinfo is not None:
        dt = dt.astimezone(timezone.utc)
    return dt.strftime("%Y-%m-%dT%H:%M:%SZ")


def main() -> int:
    cases_path = sys.argv[1]
    rust_path = sys.argv[2]
    with open(cases_path, encoding="utf-8") as fh:
        cases = json.load(fh)
    with open(rust_path, encoding="utf-8") as fh:
        rust = {row["id"]: row for row in json.load(fh)}

    failures = 0
    summary = []
    for case in cases:
        cid = case["id"]
        limit = case["limit"]
        try:
            rule = rrulestr(case["ics"])
            expected = [norm(dt) for dt in rule[:limit]]
        except Exception as exc:  # noqa: BLE001
            summary.append({"id": cid, "ok": False, "error": f"oracle: {exc}"})
            failures += 1
            continue

        got = rust.get(cid, {})
        actual = got.get("occurrences", [])
        ok = bool(got.get("ok")) and actual == expected
        if not ok:
            failures += 1
            summary.append(
                {
                    "id": cid,
                    "ok": False,
                    "expected": expected[:10],
                    "actual": actual[:10],
                    "rust_error": got.get("error"),
                }
            )
        else:
            summary.append({"id": cid, "ok": True, "count": len(expected)})

    print(json.dumps({"cases": len(cases), "failures": failures, "results": summary}, indent=2))
    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
