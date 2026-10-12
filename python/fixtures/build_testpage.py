"""Regenerate fixtures/testpage.pdf (stdlib only).

  python python/fixtures/build_testpage.py
"""

from __future__ import annotations

import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))

from fakeprint.testpage import MAGIC, write_fixture  # noqa: E402


def main() -> int:
    path = write_fixture(Path(__file__).resolve().parent / "testpage.pdf")
    data = path.read_bytes()
    print(f"wrote {path} ({len(data)} bytes)")
    print(f"magic={MAGIC!r} present={MAGIC.encode() in data}")
    return 0 if MAGIC.encode() in data else 1


if __name__ == "__main__":
    raise SystemExit(main())
