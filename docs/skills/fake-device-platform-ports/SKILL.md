---
name: fake-device-platform-ports
description: Build and test a print or scan capability on a new platform (Linux, macOS, or another stack) by developing against a fake device and a known-good reference client at the same time. Use when asked to port printing or scanning to CUPS/IPP, SANE/eSCL, macOS Image Capture or similar, to add a fake of the device for that protocol, to compare an implementation with a reference client, or to prove that every fault is handled.
---

# Developing platform print/scan support with a fake device and a reference client

This guide comes from the Windows work (WIA scanning, WS-Print printing, the `fakescan` and `fakeprint`
emulators, and the cross-client batteries). It tells you how to repeat the method for Linux, macOS or any other
stack. It names no real printer or scanner on purpose: **real hardware is never the reference, and your tests never
touch it.**

## 1. Why the method works

Three things, used together:

1. **A fake device** that speaks the protocol on the loopback interface, with every behaviour a real device can
   show scripted: capabilities, state, levels, timing and faults. It is deterministic, free, fast and safe. A real
   device is none of those: it is slow, stateful, shared, sometimes busy, and sometimes wrong in ways that are not
   in the spec. A fake lets you test the 99 situations a real device will not give you on demand (paper jam in the
   middle of page 3, a dropped connection, a reply that never comes, a `500` with an HTML page).
2. **A known-good reference client**: the platform's own built-in way of talking to the same device (Windows' own
   spooler and WIA, `lp`/`ipptool` on Linux, the system print stack and Image Capture on macOS). It defines "correct"
   for the parts the specification leaves open, and it is the oracle for what bytes and pixels should arrive.
3. **A battery that runs both clients against the same fake** and compares what the *device* received and what the
   *client* reported. Where they differ, one of them is wrong; the spec decides which.

"As good as the reference" then has a precise meaning: for every capability and every injected fault, our client
produces the same device-visible result (document bytes, ticket, job outcome) and an equally good typed error as
the reference does. A difference is a bug in ours, a bug in the fake, or a documented platform quirk.

## 2. Layout to copy

Keep four layers, each in its own place, so a new platform adds a column and not a rewrite:

| Layer | What it is | Windows example | Where it lives |
|---|---|---|---|
| Neutral core | types, errors (`kind`, `is_retryable`), settings, a `Backend` trait and registry | `craft-print`, `craft-scan` | shared library repo, no OS code, no unsafe |
| Protocol layer | SOAP/IPP/eSCL messages, parsers, transport with a policy | `craft-wsd` | shared library repo |
| Platform component | OS APIs, possibly `unsafe`, registers itself into the core | WIA COM crate, `winprint` | its own crate/repo, outside the shared libs |
| App host | commands, CLI, i18n, GUI wiring | pdfcraft | the application |

Test tooling is **separate** and never shipped with the product:

| Piece | Role | Windows example |
|---|---|---|
| Fake device (library + daemon) | the emulator; embedded (in-process, port 0, no discovery) and daemon (many devices, registry, tray) modes | `fakeprint`, `fakescan` |
| Fake bindings | drive the fake from the language of the tests | `fakeprint-python`, `fakescan-python` |
| Reference-client wrapper | the platform's own client behind a small API, with safety guards | `fakeprint.winprint` (ctypes, spooler) |
| Cross-client harness | the battery and its oracles | `pdfcraft-fakeprint`, `pdfcraft-fakescan` |

Rules of thumb: the harness and fake repos stay outside the product repo; the product repo contains only unit tests
with in-memory fakes. Everything that needs a daemon, Windows/macOS/Linux services or a GUI is opt-in and skips
cleanly with a reason.

## 3. Build order (do it in this order)

1. **Write the contract first.** One document (`docs/multi-device.md` in `fakeprint` is the model) that fixes the
   device model, the HTTP/IPC surface, discovery, the registry, the tray/UI model, the work-package split and the
   test plan. Agents that implement in parallel need file ownership and interfaces spelled out, or they collide.
2. **Parse the specification, do not hand-type it.** Generate the operation/action tables, enums and fault lists
   from the schema or the spec text, and validate every reply the fake sends against it (a switch such as
   `FAKEPRINT_VALIDATE_REPLIES=1`). Keep licensed spec files out of git; write your own words in tracked docs.
3. **Make the fake strict.** It must reject what the spec says is invalid (a missing required ticket element, a
   wrong document format, a bad job id) with the spec's exact fault. A lenient fake lets a sloppy client look good.
4. **Add the knobs** (section 5) before writing the client: if you cannot provoke a situation you cannot test it.
5. **Write the client against the fake**, with a typed error model and a transport trait so tests can inject
   in-memory transports.
6. **Wrap the reference client** (section 4) and write the parity battery (section 6).
7. **Only then** touch real hardware, once, by hand, with the owner's explicit say, and treat what you learn as a
   *named quirk mode*, not as the new truth (section 8).

## 4. The reference client, per platform

The reference is whatever the platform already ships, driven through its public interface. Wrap it so the harness
can call it, and give the wrapper the same guards as the product (explicit target, explicit confirmation).

**Printing**
* **Linux:** CUPS. Reference: `lp`/`lpr` into a queue that points at the fake (`ipp://127.0.0.1:<port>/...`), `ipptool`
  for raw IPP operations (Get-Printer-Attributes, Validate-Job, Create-Job, Send-Document, Cancel-Job). Fake: an IPP
  Everywhere (driverless) printer: `ipp/ipps` operations, `printer-state`, `printer-state-reasons`, `job-state`,
  supply levels, media/sides/colour attributes, `document-format-supported`. `fakeprint` already has an `ipp/` module
  and a JetDirect raw-9100 listener to build on. Discovery on Linux is mDNS/DNS-SD (`_ipp._tcp`, `_ipps._tcp`): treat
  multicast like Windows treats WS-Discovery (below).
* **macOS:** the same CUPS stack plus `lpstat`, `lpadmin`, and the system print dialog; AirPrint is IPP. Reference:
  `lp`, `lpadmin -p ... -v ipp://127.0.0.1:<port>/...` for a queue on the fake, and the OS spooler's own output.
* **Windows (done):** spooler + WSD/IPP class driver; WS-Print client; reference = GDI print of known pixels through a
  queue that points at the fake, measured exactly (geometry, JPEG re-encoding, ticket Windows sent).

**Scanning**
* **Linux:** SANE (`scanimage -L`, `-d`, `--format`) with `sane-airscan` for eSCL/WSD; fake: an **eSCL** scanner
  (`/eSCL/ScannerCapabilities`, `/ScanJobs`, `/NextDocument`, status) on loopback. Reference = `scanimage` through
  `sane-airscan` against the fake.
* **macOS:** ImageCaptureCore / Image Capture app / `icdiscover`-style tooling over eSCL (AirScan); same fake.
* **Windows (done):** WIA through Windows' own automation as the second client, plus the blind `diag` page analyzer
  that reads a scanned test page and says what was done to the pixels.

Write down, for each reference, what it will **not** tell you (for example "the OS driver recompresses to JPEG", "the
queue rescales the page") and encode it as a measured tolerance, never a guess.

## 5. What the fake must be able to do

Drive every item from the test code, per device, at run time:

* **Capabilities (published):** formats and compression, colour modes, duplex/sides, media sizes and bins, copies,
  resolutions, version flags, feeder/tray/glass for scanners. Change of any of these is a different "capability
  class" and needs a re-install on platforms that cache capabilities.
* **State (runtime):** levels (toner/ink/paper/trays), conditions and state reasons (paper out, cover open, jam,
  toner low, offline/not accepting jobs, needs attention), and the transitions between them.
* **Timing:** instant, realistic and default presets plus per-phase delays, so races (cancel after completion, poll
  while a job ends) are reproducible.
* **Fault injection**, as data, per operation and endpoint: `skip N`, `count N`, `delay_ms`, then one of
  *pass* | *protocol fault with code/subcode/reason/detail* | *HTTP status and body (including an HTML error page)* |
  *close the connection* (before the reply, after part of it, never reply). Also: oversize replies, wrong
  namespaces, duplicate elements, truncated multipart, a document that arrives corrupt, `fail_next_document`.
* **Capture:** keep every received document and ticket (name, user, format, bytes, per-document file) so tests can
  compare what the device saw with what was sent.
* **Strictness switch:** schema validation of every outgoing reply, with a counter in the status API.
* **Identity discipline:** all fake devices live in one UUID namespace (`00000000-0000-4000-8000-...`). Any safety
  guard asks "does this device's UUID start with the prefix" and never "does its name look fake".

Checklist of faults to cover for any protocol: no reply, closed connection, reset mid-body, `4xx`, `5xx` with HTML,
timeout during upload, timeout during poll, cancel races (before start, during, after end), unknown job id, busy,
not accepting jobs, unsupported format/option, option that needs a missing element, duplicate submit, huge document,
zero-byte document, hostile strings in names (control characters, markup, very long), subscription expiry,
discovery replies from the wrong address.

## 6. The parity battery

Build it as parametrised pytest (or the platform's equivalent) with one section per question:

1. **Pass-through byte identity:** every format the fake lists, at 0/1 B, 1 KB, 64 KB, 3 MB and 24 MB, arrives
   byte-identical through *every* client (ours, the independent second client, the reference).
2. **Pixel exactness:** deterministic patterns (ramps, primaries, 1-px checkerboards and grids, odd widths such as
   1, 2, 3, 7, 255, 257), decoded by an **independent decoder you wrote from the format description** and by a
   second library; assert every pixel for lossless paths and an evidence-based tolerance for lossy ones (record the
   measured max/mean error per block, then set the tolerance 30-50 % above it).
3. **Ticket parity:** the same options through each client produce the same stored ticket.
4. **Reduced and odd tickets:** minimal, empty and over-full tickets against the strict fake.
5. **Fault parity:** the same injected fault gives an equivalent *typed* outcome in each client.
6. **Geometry from paper (optional, opt-in):** print a self-describing test page, scan it back, and measure the
   path (the fakescan `diag` page and `print_scan_loop` do exactly this).

Label every oracle honestly in `docs/oracles.md`: *independent*, *regression-only* (a stored hash of our own output),
or *statistical*. A test that compares our output with our own earlier output is a regression test, not proof.

## 7. Safety rules that carry over to every platform

* **Never contact real hardware in tests.** Loopback or in-process only. Real devices are touched only by the
  owner's explicit instruction in chat, one action at a time, and never routinely. Reads before writes.
* **Multicast/mDNS discovery is opt-in** and exercised through fake sockets in unit tests. A daemon that announces a
  device on the LAN does so only when asked, and announces **one not-yet-installed device at a time** (the OS needs
  a human to add each one; never auto-add, never script the add).
* **Consent is code, not convention:** a send needs an explicit confirmation value that is checked before any I/O
  and is never remembered between jobs.
* **A network policy limits where we connect** (numeric local addresses; no hostnames, redirects or proxies); it is
  not an off switch and it is not configurable away.
* **No personal data in anything published:** device UUIDs/serials/MACs/models of real hardware, user names, LAN
  addresses of real devices. Real scans or prints are personal fixtures and stay out of repos. Licensed spec files
  and anything mechanically derived from them are gitignored; tracked docs are in your own words.
* **No `unsafe` in shared libraries.** OS-API code lives in a small component outside them and registers itself.
* **Paths and shells:** quote paths with spaces or backslashes, prefer forward slashes in Git Bash, point build
  output at a scratch directory and delete it when done (disks fill up).

## 8. "The device is not the reference"

A real device that behaves differently from the spec and from the reference client is a **quirk**, not a model of
correctness. Record it as a named, opt-in mode (for example a ticket shape that omits elements some firmware cannot
answer), keep the default path spec-correct, say in the docs *which* behaviour is the quirk in vendor-neutral words,
and keep brand-specific findings in private notes. Measure first (a bare request with no document, jobs cancelled),
change one variable at a time, and stop when the printer stops answering rather than hammering it.

## 9. Working with several agents

* One contract document, then work packages with exact file ownership and an agreed interface module first.
* One agent runs builds in its own scratch target directory; never rebuild a binary a daemon is running from.
* Agents report test counts and what they could **not** verify; treat unverified items as open, never as done.
* Keep a hand-off document per phase (what exists, heads and paths, rules, gaps, next steps) so the next agent does not
  have to infer them.
* Give each platform its own work-package: fake protocol module, reference-client wrapper, battery section,
  documentation. A new platform should add files, not edit shared ones.

## 10. Definition of done for a platform port

* The client passes the strict-fake battery, including **every** item of the fault checklist, with typed errors.
* The parity battery is green against the reference client for bytes, pixels (within measured tolerance), tickets and
  faults, or each difference is documented as a platform quirk with evidence.
* The fake runs in embedded mode for unit tests and as a daemon for system-level tests, with the tray/UI/registry
  features the platform needs, and nothing in either mode touches the network beyond loopback unless explicitly asked.
* Docs say what is verified, what is not (for example "not seen on paper", "not run on a real desktop") and how to
  re-run each check.
* Published text is vendor-neutral and free of personal identifiers.

## 11. Where the working examples are

* Fake printer, device manager, tray, web UI, injection and timing: the `fakeprint` repo (`docs/multi-device.md`,
  `docs/injection.md`, `docs/profiles.md`, `docs/windows.md`).
* Python bindings and the reference-client wrapper: `fakeprint-python` (`fakeprint.winprint`, `fakeprint.manager`,
  `fakeprint.lab`).
* Fake scanner and the `diag` page: the `fakescan` repo (`docs/multi-device.md`, `docs/patterns.md`) and
  `fakescan-python`.
* The cross-client batteries and their oracle notes live outside the product repos (a `pdfcraft-fakeprint`-style
  harness): sections a-f above, `docs/oracles.md`.
