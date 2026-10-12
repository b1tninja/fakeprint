"""Minimal WS-Print SOAP helpers for CI (no Windows spooler).

Flow: CreatePrintJob → SendDocument (base64 DocumentData) → optional EndJob.
Ground truth = bytes sent; expect via GET /v1/jobs + inbox file.
"""

from __future__ import annotations

import base64
import re
import uuid
from dataclasses import dataclass
from pathlib import Path

import httpx

PRINT_NS = "http://schemas.microsoft.com/windows/2006/08/wdp/print"


def _envelope(action_local: str, body_xml: str) -> str:
    action = f"{PRINT_NS}/{action_local}"
    msg_id = str(uuid.uuid4())
    return f"""<?xml version="1.0" encoding="utf-8"?>
<soap:Envelope xmlns:soap="http://www.w3.org/2003/05/soap-envelope"
  xmlns:wsa="http://schemas.xmlsoap.org/ws/2004/08/addressing"
  xmlns:wprt="{PRINT_NS}">
  <soap:Header>
    <wsa:Action>{action}</wsa:Action>
    <wsa:MessageID>urn:uuid:{msg_id}</wsa:MessageID>
  </soap:Header>
  <soap:Body>
{body_xml}
  </soap:Body>
</soap:Envelope>
"""


@dataclass
class PrintJobRef:
    job_id: int


def _post(client: httpx.Client, base_url: str, body: str) -> str:
    r = client.post(
        f"{base_url.rstrip('/')}/PrintService",
        content=body.encode("utf-8"),
        headers={"Content-Type": "application/soap+xml; charset=utf-8"},
    )
    r.raise_for_status()
    return r.text


def get_printer_elements(client: httpx.Client, base_url: str) -> str:
    body = """    <wprt:GetPrinterElementsRequest>
      <wprt:RequestedElements>
        <wprt:Name>wprt:PrinterDescription</wprt:Name>
      </wprt:RequestedElements>
    </wprt:GetPrinterElementsRequest>"""
    return _post(client, base_url, _envelope("GetPrinterElements", body))


def create_print_job(
    client: httpx.Client,
    base_url: str,
    *,
    job_name: str = "fakeprint-lab",
    document_format: str = "application/pdf",
) -> PrintJobRef:
    body = f"""    <wprt:CreatePrintJobRequest>
      <wprt:PrintTicket>
        <wprt:JobDescription>
          <wprt:JobName>{job_name}</wprt:JobName>
        </wprt:JobDescription>
        <wprt:DocumentDescription>
          <wprt:DocumentFormat>{document_format}</wprt:DocumentFormat>
        </wprt:DocumentDescription>
      </wprt:PrintTicket>
    </wprt:CreatePrintJobRequest>"""
    text = _post(client, base_url, _envelope("CreatePrintJob", body))
    m = re.search(r"<[^:>]*:?JobId>(\d+)</", text)
    if not m:
        raise RuntimeError(f"CreatePrintJob missing JobId: {text[:400]}")
    return PrintJobRef(job_id=int(m.group(1)))


def send_document(
    client: httpx.Client,
    base_url: str,
    job: PrintJobRef,
    payload: bytes,
) -> None:
    b64 = base64.b64encode(payload).decode("ascii")
    body = f"""    <wprt:SendDocumentRequest>
      <wprt:JobId>{job.job_id}</wprt:JobId>
      <wprt:DocumentData>{b64}</wprt:DocumentData>
    </wprt:SendDocumentRequest>"""
    text = _post(client, base_url, _envelope("SendDocument", body))
    if "SendDocumentResponse" not in text and "Fault" in text:
        raise RuntimeError(f"SendDocument fault: {text[:500]}")


def end_job(client: httpx.Client, base_url: str, job: PrintJobRef) -> None:
    body = f"""    <wprt:EndJobRequest>
      <wprt:JobId>{job.job_id}</wprt:JobId>
    </wprt:EndJobRequest>"""
    _post(client, base_url, _envelope("EndJob", body))


def print_and_capture(
    base_url: str,
    payload: bytes,
    *,
    document_format: str = "application/pdf",
    timeout: float = 60.0,
) -> PrintJobRef:
    """CreatePrintJob + SendDocument + EndJob. Caller asserts via Client.jobs()."""
    with httpx.Client(timeout=timeout) as http:
        job = create_print_job(http, base_url, document_format=document_format)
        send_document(http, base_url, job, payload)
        end_job(http, base_url, job)
        return job


def guess_format(path: Path) -> str:
    ext = path.suffix.lower()
    return {
        ".pdf": "application/pdf",
        ".oxps": "application/oxps",
        ".xps": "application/vnd.ms-xpsdocument",
        ".jpg": "image/jpeg",
        ".jpeg": "image/jpeg",
        ".png": "image/png",
        ".tif": "image/tiff",
        ".tiff": "image/tiff",
    }.get(ext, "application/octet-stream")
