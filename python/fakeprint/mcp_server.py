"""fakeprint-mcp — stdio MCP server, attach-only to a running fakeprint instance.

Attach-only design:
- Never spawns or kills fakeprint.
- Fails fast (raises) if fakeprint is not reachable.
- Never touches fakescan ports or %LOCALAPPDATA%\\fakescan.
- Port band: fakeprint uses odd ports (3921, 3923, …).

Usage (stdio, for Cursor / claude-desktop):
    fakeprint-mcp [--base-url http://127.0.0.1:3921]

One Cursor server entry per running fakeprint instance.  Do not share this
server between fakescan and fakeprint — use fakescan-mcp for fakescan.
"""

from __future__ import annotations

import argparse
import json
import sys
from typing import Any

try:
    from mcp.server.fastmcp import FastMCP
except ImportError:
    print(
        "fakeprint[mcp] not installed.\n"
        "  pip install -e './python[mcp]'\n"
        "Requires: mcp>=1.0",
        file=sys.stderr,
    )
    raise SystemExit(1)

from .client import DEFAULT_BASE_URL, Client

_CAVEAT = (
    "Attach-only: this tool reads the running fakeprint instance; "
    "it does not spawn or kill any process. "
    "Use scripts/fakeprint.ps1 ensure to start fakeprint first."
)


def _make_server(base_url: str) -> FastMCP:
    mcp = FastMCP(
        "fakeprint-mcp",
        instructions=(
            f"Attach-only MCP for fakeprint at {base_url}. "
            "fakeprint uses ODD ports (3921, 3923, …). "
            "fakescan uses EVEN ports — use fakescan-mcp for that product. "
            "Never spawn or kill fakeprint processes. "
            "Use scripts/fakeprint.ps1 ensure to start fakeprint first."
        ),
    )

    def _client() -> Client:
        c = Client(base_url)
        if not c.health():
            raise RuntimeError(
                f"fakeprint not reachable at {base_url}. "
                "Start it: .\\scripts\\fakeprint.ps1 ensure"
            )
        return c

    @mcp.tool(description=f"GET /health — is fakeprint up? {_CAVEAT}")
    def health() -> dict[str, Any]:
        with Client(base_url) as c:
            ok = c.health()
        return {"ok": ok, "base_url": base_url}

    @mcp.tool(
        description=(
            "GET /v1/status — profile, UUID, WSD hellos, active jobs, captured jobs, "
            f"available profiles. {_CAVEAT}"
        )
    )
    def status() -> dict[str, Any]:
        with _client() as c:
            return c.status().raw

    @mcp.tool(
        description=(
            "GET /v1/jobs — list captured print jobs (job_id, path, bytes, format). "
            f"{_CAVEAT}"
        )
    )
    def jobs() -> list[dict[str, Any]]:
        with _client() as c:
            return [
                {
                    "job_id": j.job_id,
                    "path": j.path,
                    "bytes": j.bytes,
                    "document_format": j.document_format,
                    "peer": j.peer,
                }
                for j in c.jobs()
            ]

    @mcp.tool(
        description=(
            "POST /v1/reset — clear in-memory jobs (inbox .bin/.json files on disk are kept). "
            f"{_CAVEAT}"
        )
    )
    def reset() -> dict[str, Any]:
        with _client() as c:
            return c.reset().raw

    @mcp.tool(
        description=(
            "GET /v1/ctl — identity: name, port, uuid, pipe path, supported ctl cmds. "
            f"{_CAVEAT}"
        )
    )
    def ctl_info() -> Any:
        with _client() as c:
            return c.ctl_info()

    @mcp.tool(
        description=(
            "POST /v1/ctl — dispatch a control command. "
            "cmd: status | health | jobs | reset | profile | save. "
            "For 'profile', pass args={'id': 'generic-wsd'}. "
            f"{_CAVEAT}"
        )
    )
    def ctl(cmd: str, args: dict[str, Any] | None = None) -> Any:
        with _client() as c:
            return c.ctl(cmd, **(args or {}))

    @mcp.tool(
        description=(
            "POST /v1/profile — hot-swap the active capability profile without restarting fakeprint. "
            "profile_id must be a JSON filename stem in the profiles/ directory. "
            f"{_CAVEAT}"
        )
    )
    def set_profile(profile_id: str) -> dict[str, Any]:
        with _client() as c:
            return c.set_profile(profile_id).raw

    return mcp


def main(argv: list[str] | None = None) -> None:
    p = argparse.ArgumentParser(
        prog="fakeprint-mcp",
        description="fakeprint MCP server (stdio, attach-only)",
    )
    p.add_argument(
        "--base-url",
        default=DEFAULT_BASE_URL,
        help=f"fakeprint base URL (default {DEFAULT_BASE_URL})",
    )
    args = p.parse_args(argv)
    mcp = _make_server(args.base_url)
    mcp.run(transport="stdio")


if __name__ == "__main__":
    main()
