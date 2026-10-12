"""CLI entry: `fakeprint-lab`."""

from __future__ import annotations

import argparse
import hashlib
import sys
from pathlib import Path

from .client import DEFAULT_BASE_URL, Client


def main(argv: list[str] | None = None) -> int:
    p = argparse.ArgumentParser(prog="fakeprint-lab", description="fakeprint lab harness")
    p.add_argument("--base-url", default=DEFAULT_BASE_URL)
    sub = p.add_subparsers(dest="cmd", required=True)

    sub.add_parser("health")
    sub.add_parser("status")
    sub.add_parser("reset", help="clear in-memory jobs (inbox files kept)")
    sub.add_parser("jobs", help="GET /v1/jobs")

    pr = sub.add_parser(
        "print-soap",
        help="SOAP print a file and assert capture bytes match (no Windows spooler)",
    )
    pr.add_argument("file", type=Path)
    pr.add_argument("--format", dest="document_format", default=None)

    args = p.parse_args(argv)
    with Client(args.base_url) as c:
        if args.cmd == "health":
            ok = c.health()
            print("ok" if ok else "fail")
            return 0 if ok else 1
        if args.cmd == "status":
            st = c.status()
            print(
                f"profile={st.profile_id} label={st.label!r} "
                f"active={st.active_jobs} captured={st.captured_jobs}"
            )
            return 0
        if args.cmd == "reset":
            c.reset()
            print("reset")
            return 0
        if args.cmd == "jobs":
            rows = c.jobs()
            if not rows:
                print("(no captured jobs)")
                return 0
            for j in rows:
                print(
                    f"job_id={j.job_id} bytes={j.bytes} format={j.document_format} path={j.path}"
                )
            return 0
        if args.cmd == "print-soap":
            return _print_soap(c, args)
    return 2


def _print_soap(c: Client, args: argparse.Namespace) -> int:
    from .soap import guess_format, print_and_capture

    path: Path = args.file
    if not path.is_file():
        print(f"not a file: {path}", file=sys.stderr)
        return 1
    payload = path.read_bytes()
    fmt = args.document_format or guess_format(path)
    before = len(c.jobs())
    job = print_and_capture(args.base_url, payload, document_format=fmt)
    rows = c.jobs()
    if len(rows) <= before:
        print("no new captured job", file=sys.stderr)
        return 1
    cap = next((j for j in reversed(rows) if j.job_id == job.job_id), rows[-1])
    got = cap.read_bytes()
    if got != payload:
        print(
            f"capture mismatch: sent={len(payload)} captured={len(got)} "
            f"sha256 sent={hashlib.sha256(payload).hexdigest()[:16]} "
            f"got={hashlib.sha256(got).hexdigest()[:16]}",
            file=sys.stderr,
        )
        return 1
    print(f"ok: job_id={cap.job_id} bytes={cap.bytes} path={cap.path}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
