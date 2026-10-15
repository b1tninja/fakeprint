# Raw request log

Every device keeps a bounded, in-memory ring of the exact exchanges on its wire endpoints, so a
protocol-level test can assert what a client really sent: the HTTP headers as written (case and
order), the SOAP envelope or IPP message, MTOM versus inline documents, chunking, the PrintTicket
as received, and what the validator changed.

This is a test fake. Bodies can contain user data (job names, user names, the first bytes of a
document). The log is never written to disk, is cleared by `DELETE /v1/requests` and by
`POST /v1/reset`, and `redact` masks peer addresses and credential headers.

## What is logged

| Direction | Source |
|---|---|
| `in` | every request to the device's WS-Print endpoints (`/WebServices/Device`, `/WebServices/PrinterService`, `/PrintService`), the directed WS-Discovery endpoint and `/ipp/print` (also under `/d/<id>/`) |
| `out` | every WS-Eventing delivery the device sends to a subscriber (`protocol: "event"`) |

Not logged: the control plane (`/v1/*`, `/health`, `/ui`) and the JetDirect listener.

Records are filed when the exchange completes, so `seq` is completion order (monotonic, never
reused, kept counting across `clear` and ring overflow); `t_mono_ms` / `t_wall` give the arrival
time. A poller using `since=<last seq>` therefore never misses a record; `in_flight` in the
listing counts requests still being served (slow injected delays).

## Bounds

`request_log` is configured per server (`ServerConfig::request_log`), changed per device at run
time (`POST /v1/requests/config`) and from the daemon command line:

| Field | Default | Meaning |
|---|---|---|
| `enabled` | `true` | switch; off: nothing is recorded and the hot path is one atomic load |
| `capacity` | 256 | records per device, drop-oldest; `0` means off (`--request-log N`, env `FAKEPRINT_REQUEST_LOG`) |
| `body_cap` | 262144 | stored bytes of one request body and of one reply body (`--request-log-body`) |
| `doc_head` | 16384 | stored bytes of a document-carrying request (`SendDocument`, `AddDocument`, `Print-Job`, `Send-Document`); the whole length and SHA-256 are kept instead of the rest |
| `max_bytes` | 33554432 | payload bytes the whole ring may hold; the oldest records go first (`--request-log-bytes`) |
| `redact` | `false` | peer becomes `redacted`; `Authorization`, `Proxy-Authorization`, `Cookie`, `Set-Cookie` values become `redacted`; no raw head is kept (`--request-log-redact`) |

Worst case memory per device is `max_bytes` plus the headers of the records (about 1 KiB each);
a request body is teed, not buffered, while it is being served (at most `body_cap` per
in-flight request). The default is on because this is a test fake; set `--request-log 0` for a
long-running daemon nobody inspects.

## Header fidelity

hyper lowercases header names and groups duplicates, so the server wraps the accepted sockets
in a pass-through tap that frames the HTTP/1.1 request heads (Content-Length, chunked, keep-alive
aware) and hands the raw head of each request to the log. `request.headers_raw` is `true` when
that worked: names keep their case, headers keep their wire order, duplicates stay separate,
and `GET /v1/requests/<seq>/raw` returns the real head bytes. When a request did not come through
a socket (an in-process call) or the tap lost track (HTTP/2, a head over 64 KiB), the headers
come from the parsed map (lowercase, `headers_raw: false`).

## Control API

Per device: `/d/<id>/v1/requests...`; the default device also answers the unprefixed routes.

* `GET /v1/requests?since=<seq>&limit=<n>&action=<name>&job=<id>&direction=in|out&bodies=0`
  JSON, oldest first; `limit` default 100. `action` matches the WSD short action
  (`CreatePrintJob`), the full action URI or the IPP operation (`Print-Job`), case-insensitive.
  `bodies=0` leaves out `base64` / `text`.
* `GET /v1/requests/<seq>` one record.
* `GET /v1/requests/<seq>/raw?part=request|response&what=all|body` the bytes
  (`application/octet-stream`; headers `X-Fakeprint-Truncated`, `-Length`, `-Sha256`). The raw
  request is the wire head plus the stored body; the response head is rebuilt from the status and
  headers.
* `DELETE /v1/requests` clear (answers `{cleared, next_seq}`).
* `GET|POST /v1/requests/config` read / patch the configuration (`{"enabled":true,"capacity":64}`);
  an unknown field is a 400. A smaller capacity drops the oldest records at once.
* `GET /v1/jobs` rows have `request_seqs` (records still in the ring), `ticket_received` (the
  `PrintTicket` text exactly as sent, cut out of the envelope) and `ticket_substitutions`.
  These survive `DELETE /v1/requests` (the seq links do not); `POST /v1/reset` drops them.

## Listing shape

```json
{
  "config": {"enabled": true, "capacity": 256, "body_cap": 262144, "doc_head": 16384, "max_bytes": 33554432, "redact": false},
  "count": 2, "stored_bytes": 5120, "dropped": 0, "next_seq": 3, "in_flight": 0,
  "records": [{
    "seq": 1, "direction": "in", "t_mono_ms": 12.5, "t_wall_unix_ms": 1760000000000, "t_wall": "2025-10-09T08:53:20.000Z",
    "peer": "127.0.0.1:50321", "method": "POST", "path": "/WebServices/PrinterService", "query": null, "version": "HTTP/1.1",
    "protocol": "wsd", "endpoint": "print",
    "action": "CreatePrintJob", "action_uri": "http://schemas.microsoft.com/windows/2006/08/wdp/print/CreatePrintJob",
    "message_id": "urn:uuid:...", "request_id": null,
    "ipp_operation": null, "ipp_operation_id": null, "ipp_status": null, "job_id": 1,
    "latency_us": 480, "fault_rule": null, "result": "ok",
    "request":  {"headers": [["User-Agent", "..."], ["Content-Type", "..."]], "headers_raw": true,
                 "body": {"length": 812, "stored": 812, "truncated": false, "document": false, "sha256": "...", "base64": "...", "text": "<?xml ..."}},
    "response": {"status": 200, "headers": [["content-type", "..."]], "body": {"length": 500, "stored": 500, "truncated": false, "document": false, "sha256": "...", "base64": "...", "text": "..."}},
    "ticket": {"received": "<wprt:PrintTicket>...</wprt:PrintTicket>",
               "substitutions": [{"field": "MediaSizeName", "requested": "iso_a0_841x1189mm", "applied": null, "reason": "..."}]},
    "ipp": null
  }]
}
```

* `result`: `ok`; `faulted` (a SOAP fault, an HTTP error status or an IPP error status, injected
  or the engine's own); `dropped` (no answer reached the client: injected connection close,
  a failed delivery).
* `fault_rule`: `<op or *>[<queue index>]:<effect>` of the injection rule that applied (see
  [injection.md](injection.md)), else `null`.
* `ticket` (WS-Print `CreatePrintJob`): `received` is the raw element text, `substitutions`
  the soft values the profile did not list (`applied` null = the element was dropped). The job's
  effective ticket cannot tell "asked for A4" from "asked for nothing"; the log can. For IPP,
  `substitutions` are the attributes in the reply's unsupported-attributes group and
  `ipp` has `request_attrs` / `response_attrs` (group, name, values as text).
* `direction: "out"`: `peer` is the subscriber address, `response.status` the subscriber's
  answer, `ticket.received` holds the transport error text when the delivery failed.

## Tests

`crates/fakeprint/tests/request_log.rs` (15 tests: WSD inline and MTOM, IPP Print-Job, injected
fault and drop, ring overflow, caps and truncation flags, header case/order/chunking, ticket
substitutions, clear, concurrency, disabled mode, redaction, per-device logs, outbound events),
plus unit tests in `src/reqlog.rs` (SHA-256 vectors, the ring, the tap framing).
