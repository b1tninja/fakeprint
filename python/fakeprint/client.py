"""HTTP client for fakeprint `:3921` control plane."""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path
from typing import Any

import httpx

DEFAULT_BASE_URL = "http://127.0.0.1:3921"


@dataclass(frozen=True)
class CapturedJob:
    job_id: int
    path: str
    meta_path: str
    bytes: int
    document_format: str
    peer: str

    @classmethod
    def from_json(cls, row: dict[str, Any]) -> CapturedJob:
        return cls(
            job_id=int(row.get("job_id", 0)),
            path=str(row.get("path") or ""),
            meta_path=str(row.get("meta_path") or ""),
            bytes=int(row.get("bytes", 0)),
            document_format=str(row.get("document_format") or ""),
            peer=str(row.get("peer") or ""),
        )

    def read_bytes(self) -> bytes:
        return Path(self.path).read_bytes()


@dataclass(frozen=True)
class Status:
    raw: dict[str, Any]

    @property
    def profile_id(self) -> str:
        return str(self.raw.get("profile_id", ""))

    @property
    def label(self) -> str:
        return str(self.raw.get("label", ""))

    @property
    def active_jobs(self) -> int:
        return int(self.raw.get("active_jobs", 0))

    @property
    def captured_jobs(self) -> int:
        return int(self.raw.get("captured_jobs", 0))


class Client:
    def __init__(
        self,
        base_url: str = DEFAULT_BASE_URL,
        *,
        timeout: float = 30.0,
        client: httpx.Client | None = None,
    ) -> None:
        self.base_url = base_url.rstrip("/")
        self._owns = client is None
        self._http = client or httpx.Client(timeout=timeout)

    def close(self) -> None:
        if self._owns:
            self._http.close()

    def __enter__(self) -> Client:
        return self

    def __exit__(self, *exc: object) -> None:
        self.close()

    def _url(self, path: str) -> str:
        if not path.startswith("/"):
            path = "/" + path
        return f"{self.base_url}{path}"

    def _json(self, method: str, path: str, body: dict | None = None) -> Any:
        r = self._http.request(method, self._url(path), json=body)
        r.raise_for_status()
        if not r.content:
            return None
        return r.json()

    def health(self) -> bool:
        r = self._http.get(self._url("/health"))
        return r.status_code == 200 and r.text.strip() == "ok"

    def status(self) -> Status:
        return Status(raw=self._json("GET", "/v1/status"))

    def reset(self) -> Status:
        """Clear in-memory jobs; does not delete inbox files on disk."""
        return Status(raw=self._json("POST", "/v1/reset"))

    def jobs(self) -> list[CapturedJob]:
        data = self._json("GET", "/v1/jobs")
        return [CapturedJob.from_json(row) for row in (data.get("jobs") or [])]
