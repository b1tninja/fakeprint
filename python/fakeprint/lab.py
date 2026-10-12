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
    sub.add_parser("ctl-info", help="GET /v1/ctl — identity + command list")

    ctl_p = sub.add_parser("ctl", help="POST /v1/ctl {cmd, args}")
    ctl_p.add_argument("ctl_cmd", metavar="CMD",
                       choices=["status", "health", "jobs", "reset", "profile", "save"])
    ctl_p.add_argument("--profile-id", help="profile id for 'ctl profile'")

    pr = sub.add_parser(
        "print-soap",
        help="SOAP print a file and assert capture bytes match (no Windows spooler)",
    )
    pr.add_argument("file", type=Path)
    pr.add_argument("--format", dest="document_format", default=None)

    pt = sub.add_parser(
        "print-test",
        help=(
            "SOAP print fixtures/testpage.pdf (visible Letter page) and assert "
            "magic marker FAKEPRINT-TESTPAGE-v1 in the capture"
        ),
    )
    pt.add_argument(
        "--file",
        type=Path,
        default=None,
        help="override PDF path (default: regenerate fixtures/testpage.pdf)",
    )
    pt.add_argument(
        "--no-rebuild",
        action="store_true",
        help="use existing fixture bytes instead of regenerating",
    )

    pc = sub.add_parser(
        "prove-capture",
        help=(
            "Score an inbox .bin (or latest job) for test-page proof: "
            "magic and/or signature RGB fills"
        ),
    )
    pc.add_argument(
        "file",
        type=Path,
        nargs="?",
        default=None,
        help="capture path (default: latest job from GET /v1/jobs)",
    )
    pc.add_argument("--job-id", type=int, default=0, help="score this job_id")

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
            return _jobs(c)
        if args.cmd == "ctl-info":
            import json
            info = c.ctl_info()
            print(json.dumps(info, indent=2, default=str))
            return 0
        if args.cmd == "ctl":
            import json
            ctl_args: dict = {}
            if args.ctl_cmd == "profile":
                if not args.profile_id:
                    print("--profile-id required for 'ctl profile'", file=sys.stderr)
                    return 1
                ctl_args["id"] = args.profile_id
            result = c.ctl(args.ctl_cmd, **ctl_args)
            print(json.dumps(result, indent=2, default=str))
            return 0
        if args.cmd == "print-soap":
            return _print_soap(c, args)
        if args.cmd == "print-test":
            return _print_test(c, args)
        if args.cmd == "prove-capture":
            return _prove_capture(c, args)
    return 2


def _jobs(c: Client) -> int:
    from .testpage import analyze_capture_file

    rows = c.jobs()
    if not rows:
        print("(no captured jobs)")
        return 0
    for j in rows:
        tag = ""
        try:
            p = Path(j.path)
            if p.is_file():
                proof = analyze_capture_file(p)
                tag = (
                    f" magic={int(proof.magic)} visual={int(proof.visual)} "
                    f"colors={proof.color_hits}/{proof.colors_expected}"
                )
        except OSError:
            pass
        print(
            f"job_id={j.job_id} bytes={j.bytes} format={j.document_format} "
            f"path={j.path}{tag}"
        )
    return 0


def _prove_capture(c: Client, args: argparse.Namespace) -> int:
    from .testpage import analyze_capture_file

    path: Path | None = args.file
    if path is None:
        rows = c.jobs()
        if not rows:
            print("no captured jobs", file=sys.stderr)
            return 1
        if args.job_id:
            hit = next((j for j in rows if j.job_id == args.job_id), None)
            if hit is None:
                print(f"job_id={args.job_id} not found", file=sys.stderr)
                return 1
        else:
            hit = max(rows, key=lambda j: j.job_id)
        path = Path(hit.path)
        print(f"job_id={hit.job_id} path={path}")
    if not path.is_file():
        print(f"not a file: {path}", file=sys.stderr)
        return 1
    proof = analyze_capture_file(path)
    print(
        f"ok={int(proof.ok)} magic={int(proof.magic)} visual={int(proof.visual)} "
        f"colors={proof.color_hits}/{proof.colors_expected} "
        f"flate={proof.flate_streams} bytes={proof.bytes}"
    )
    print(proof.note)
    return 0 if proof.ok else 1


def _print_test(c: Client, args: argparse.Namespace) -> int:
    from .testpage import MAGIC, build_testpage_pdf, fixture_path, write_fixture

    if args.file is not None:
        path = args.file
        if not path.is_file():
            print(f"not a file: {path}", file=sys.stderr)
            return 1
        payload = path.read_bytes()
    elif args.no_rebuild and fixture_path().is_file():
        path = fixture_path()
        payload = path.read_bytes()
    else:
        path = write_fixture()
        payload = path.read_bytes()

    if MAGIC.encode("ascii") not in payload:
        # Allow an override file that was built with the same marker.
        print(
            f"fixture missing magic {MAGIC!r}; regenerating",
            file=sys.stderr,
        )
        payload = build_testpage_pdf()
        path.write_bytes(payload)

    before = len(c.jobs())
    from .soap import print_and_capture

    job = print_and_capture(args.base_url, payload, document_format="application/pdf")
    rows = c.jobs()
    if len(rows) <= before:
        print("no new captured job", file=sys.stderr)
        return 1
    cap = next((j for j in reversed(rows) if j.job_id == job.job_id), rows[-1])
    got = cap.read_bytes()
    if got != payload:
        print(
            f"capture mismatch: sent={len(payload)} captured={len(got)}",
            file=sys.stderr,
        )
        return 1
    if MAGIC.encode("ascii") not in got:
        print(f"capture missing magic {MAGIC!r}", file=sys.stderr)
        return 1
    print(
        f"ok: job_id={cap.job_id} bytes={cap.bytes} path={cap.path} "
        f"magic={MAGIC} file={path}"
    )
    return 0


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
