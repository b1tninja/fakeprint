# Sibling: fakescan

**fakeprint** and **[fakescan](../../fakescan)** are companion lab devices for Windows print and scan testing. Same host can run both; keep ports and UUIDs distinct.

| | **fakescan** | **fakeprint** |
|--|--------------|---------------|
| Role | Fake **scanner** | Fake **printer** |
| Default TCP | `:3920` | `:3921` |
| Wire (Phase 1) | WSD Scan + WIA | WSD Print + WSDMON |
| Lab ground truth | **Hard loopback** — upload JFIF → exact RetrieveImage echo | **Capture-assert** — SOAP/print → inbox / `GET /v1/jobs` |
| Python package | `python/` → `fakescan-lab`, `fakescan-ctl` | `python/` → `fakeprint-lab` |
| PowerShell | `scripts/fakescan.ps1` | `scripts/fakeprint.ps1` |
| Registry | `%LOCALAPPDATA%\fakescan\devices.json` | `%LOCALAPPDATA%\fakeprint\devices.json` |
| Named pipe | `\\.\pipe\fakescan-<name>` | `\\.\pipe\fakeprint-<name>` |
| MCP | `fakescan-mcp` | `fakeprint-mcp` |
| Port band | **Even**: 3920, 3922, … | **Odd**: 3921, 3923, … |
| Default UUID | `…000f2` | `…000f3` |
| Docs | [fakescan docs](../../fakescan/docs/README.md) | this tree |

## Do not copy across

| Belongs in fakescan only | Belongs in fakeprint only |
|--------------------------|---------------------------|
| `/v1/loopback`, pages, pattern, effects, scenario | Job capture inbox + `/v1/jobs` |
| Feeder / jam / WIA acquire glue | Print job FSM / SendDocument |
| Glass + ADF **sources** (and hardware / manual duplex) | Printer **input trays** (`InputBins` / Tray1) |
| `pdfcraft-cli scan` lab path | Spooler / Add printer lab path |

## Co-existence on one host

Both can run simultaneously. The port bands, UUIDs, registries, pipes, and MCP entries are disjoint. Windows PnP sees two separate devices.

```
fakescan.exe --lab-name default --listen 0.0.0.0:3920 --device-uuid …000f2
fakeprint.exe --lab-name default --listen 0.0.0.0:3921 --device-uuid …000f3
```

## Do not stop the sibling

A fakeprint script never stops a fakescan process, and a fakescan script never stops a fakeprint process. `remove` only removes this product's PnP nodes. The registries live in separate `%LOCALAPPDATA%` folders. Restarting the other product would force a person to Add that device again.

Prefer `/v1/ctl` over stopping this process too. See [lab-instance.md](lab-instance.md).

Shared by design (copy patterns, not crates): WS-Discovery, Transfer Get shape, “person clicks Add device,” optional Python control client.

Marks use the same idea: device art plus a two-letter app monogram (**Fs** on the scanner glass, **Fp** on the printer face). fakeprint’s LCD and logo subtitle read **PC LOAD LETTER**.

## Lab instance control

Each product gets its own single-instance guard, registry, named pipe, and optional MCP.

| | fakescan | fakeprint |
|--|----------|-----------|
| Lock | `%TEMP%\fakescan-<port>.lock` | `%TEMP%\fakeprint-<port>.lock` |
| Registry | `%LOCALAPPDATA%\fakescan\devices.json` | `%LOCALAPPDATA%\fakeprint\devices.json` |
| Named pipe | `\\.\pipe\fakescan-<name>` | `\\.\pipe\fakeprint-<name>` |
| MCP | `fakescan-mcp` | `fakeprint-mcp` |
| Autostart task | `FakescanLab` | `FakeprintLab` |
| Ctl endpoint | `POST /v1/ctl` | `POST /v1/ctl` + hot `POST /v1/profile` |

No shared registry, no shared supervisor. A script managing fakeprint never reads
`%LOCALAPPDATA%\fakescan` and never kills fakescan processes, and vice versa.

- Scan lab harness: [fakescan lab-harness](../../fakescan/docs/lab-harness.md)
- Print lab harness: [lab-harness.md](lab-harness.md)
- Print lab instance control: [lab-instance.md](lab-instance.md)
