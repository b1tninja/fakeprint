# fakeprint WSD rewrite: gap analysis and work packages

Phase 1 output. Compares `crates/fakeprint/src` (state of 2026-10-13, main + uncommitted work)
with the WS-Print reference ([reference/](reference/README.md) and the locally generated
`reference/schema/` pages + `schema-model.json`). Our words; Microsoft material is not quoted.
Line numbers are approximate and will drift.

## Verdict in one paragraph

The WSD layer is a v1.0-shaped string-template stub: requests are read by substring search,
replies are format strings, faults carry no subcode, nothing is validated against the schema at
runtime (the XSD checker in `print_schema.rs` is used by tests only), state is a handful of
instant transitions, and eventing is fire-and-forget. fakescan's `wsd/` has the structure
fakeprint lacks (one XML tree path, builder functions, status matrix, subscription manager,
fault plan). The rewrite should be driven by the parsed schema model rather than hand-copied
element lists.

## File by file

### `wsd/envelope.rs` (425 lines)
- Header parse (Action, MessageID, RelatesTo, To, Identifier, mustUnderstand) works; matched by local name only, so wrong-namespace headers are accepted. Action falls back to the body element name (lenient, hides client bugs).
- Reply header check `reply_problems` is right but only used by `debug_assert`/tests.
- Missing: ReferenceParameters echo, FaultTo/ReplyTo routing, a real SOAP `MustUnderstand` fault.

### `wsd/soap.rs` (515 lines)
- WS-Addressing 2004/08 reply headers correct.
- **Fault is wrong**: always `soap:Sender`, no `Subcode`, no `Detail`, HTTP 500 and the generic fault action for everything. The 15 `wprt:` subcodes plus `wsa:ActionNotSupported` do not exist in code.
- **MTOM is a heuristic**: whole body buffered, boundary found by byte search, the largest binary-ish part is taken instead of resolving `xop:Include href="cid:"`, only the first include considered, no cid percent-decoding, any non-SOAP body is treated as the document, and a failed parse returns empty bytes that still "succeed". No size cap.
- Namespace prefixes are never resolved; names are compared by suffix.

### `wsd/elements.rs` (442 lines)
- PrinterDescription children and order match the schema; PrinterInfo text is a lab leftover.
- PrinterStatus: constant `Idle`, constant timestamp (`2026-01-01`), emits both the correct `PrinterCurrentTime` and the non-schema `PrinterLocalTime`; never reflects an active job; no primary reason, no conditions.
- PrinterConfiguration, DefaultPrintTicket, PrinterCapabilities: schema-valid constants, only the Format list comes from the profile; Compression fixed to None; no MediaColor/JobFinishings/Storage/Consumables.
- JobStatus names match the schema; JobState tokens limited to 5 of 9; reasons mostly `None`; counters fake.
- GetPrinterElements shape (`ElementData Name=/Valid=`) correct; unknown names -> `Valid=false` correct; names matched by suffix not QName; v1.1/1.2/2.0 sections requested -> `Valid=false` forever.

### `wsd/print_schema.rs` (1356 lines)
- Right idea (XSD-driven checker with lab allowances), but test-only, loads from the working tree via `CARGO_MANIFEST_DIR` at runtime and panics if absent, cannot validate requests, cannot generate. Replace by a loader of the generated `schema-model.json` (or embedded model) shared by validator and builders.

### `wsd/mod.rs` (1067 lines)
- Dispatch by last URL segment; no WSDL action table, no SOAPAction vs Action check, same handler for Device and PrinterService endpoints (ten route aliases in `main.rs`).
- CreatePrintJob: only JobName/UserName/Format extracted (substring); no ticket validation, no MustHonor, no Format/Compression check, no InvalidArgs, no NotAcceptingJobs; emits a `JobToken` that the schema does not have (deliberate lab extension; should become an opt-in quirk); ids sequential from 1, no 2^31-1 bound, reused after reset.
- SendDocument: `LastDocument` defaults to true when absent; missing JobId becomes 0 and the capture is still written; none of JobIdNotFound / LastDocumentAlreadySent / DocumentActive / JobCancelled / MultipleDocumentsNotSupported / Format / Compression; no 60 s timeout; state flips Processing -> Completed instantly.
- AddDocument stub (no fetch, no URL faults). CancelJob always succeeds and always emits JobEnd. GetJobHistory always empty although history exists. GetActiveJobs unordered (HashMap). GetJobElements unknown job -> `Valid=false` instead of `ClientErrorJobIdNotFound`. SetEventRate discards the value.
- Invented operations: `ValidatePrintTicket` (always `ValidTicket=true`), `EndJob`, an event `PrinterStatusChangedMessage`. None exist in any version.
- v1.1/1.2/2.0: not advertised, SetPrinterElements/CreatePrintJob2/GetPrintDeviceCapabilities/... fault as unimplemented.
- Eventing: Subscribe takes the first `Address` (may be EndTo), no Delivery/Dialect/Expires validation, always grants 8 h, no subscription table or expiry; Unsubscribe/Renew/GetStatus are unconditional echoes; no SubscriptionEnd. Events produced: StatusSummary, ElementsChange, JobEnd only (+ the invented one); JobStatusEvent, PrinterStatusConditionEvent, PrinterStatusConditionClearedEvent missing; payloads hard-code Idle and the constant time; no event-rate throttling; delivery is a raw TCP write that never reads the response, http only, no retry.
- Metadata: WS-Transfer Get answered with a plausible ThisModel/ThisDevice/Relationship (type `wprt:PrinterServiceType`, PnP-X ids); WS-MetadataExchange `GetMetadata` missing; serial number / firmware / model number hard-coded.
- Lab leftovers in the library path: fault and subscribe dumps written to fixed temp locations.

### `discovery.rs` (565 lines)
- Hello/Bye/ProbeMatches/ResolveMatches envelopes are right (AppSequence, MetadataVersion, Types `wsdp:Device wprt:PrintDeviceType`, XAddrs).
- Probe/Resolve are parsed by substring: Types match is a loose OR, Scopes not honoured, Resolve matches by UUID substring, no random response delay, ReplyTo ignored, IPv4 only, Hello retransmitted on a fixed 5 s timer forever, no directed discovery (`/StableWSDiscoveryEndpoint/schemas-xmlsoap-org_ws_2005_04_discovery` is not routed).

### `state.rs`, `profile.rs`, `main.rs`, `lab.rs`
- `state.rs`: one `World` behind a lock; 5 job states, transitions inside two methods, no timers, no Aborted/Terminating, invented end reasons; shared with IPP and JetDirect (`create_job`, `capture_document` signatures must stay stable). Capture file name `timestamp-jobid` collides for two documents in one second.
- `profile.rs`: nine flat fields; everything else is hard-coded in `elements.rs`. Needs ticket defaults, allowed values, bins, consumables, conditions, version flags, a status scenario and a fault plan.
- `main.rs`: router aliases to collapse into Device endpoint vs Print service endpoint; add the directed-discovery route.
- `lab.rs`: unaffected.

## Ten most important gaps

1. Faults: no Subcode/Detail, no mapping of the 15 `wprt:` codes, wrong status/action ([faults.md](reference/faults.md)).
2. No ticket/document validation: MustHonor, Format, Compression, InvalidArgs, ConflictingRequiredParameters ([must-honor.md](reference/must-honor.md)).
3. SendDocument job checks absent; a document for job 0 is silently captured.
4. MTOM/XOP parser is a heuristic with an "empty document succeeds" fallback and no limits.
5. Job state machine incomplete and instantaneous: 5 of 9 states, no 60 s document timeout, no Terminating/Aborted, no reasons ([operations.md](reference/operations.md)).
6. Eventing: no subscription table/expiry/validation, wrong NotifyTo extraction, 3 of 6 events missing, constant payloads, no rate control.
7. Invented surface: JobToken, ValidatePrintTicket, EndJob, PrinterStatusChangedMessage ([validate-vs-create.md](reference/validate-vs-create.md)).
8. Printer model is hard-coded rather than profile/schema driven; status time constant; capabilities not honest relative to what the code accepts.
9. Endpoint and action handling: Device and Print endpoints interchangeable, no `wsa:ActionNotSupported`, no SOAPAction check.
10. Discovery/metadata: substring Probe/Resolve, no scope/type rules, no directed discovery, no GetMetadata; v1.1/1.2/2.0 flags and operations unimplemented.

Also: requests parsed by string search not by tree; JobHistory stub; SetEventRate ignored;
`print_schema.rs` test-only and path-fragile; lab temp-file dumps in library code.

## What the Python side now provides to the rewrite

`fakeprint-python` (`fakeprint.wsd_schema`, `fakeprint.wsd_docs`) parses the whole package once and
writes `schema-model.json`: elements, types, particles with occurrence, attributes, enumerations
(flattened through unions), wildcards, operations with actions, fault enumeration. Rust code can
embed or load this JSON to (a) validate inbound bodies and outbound replies, (b) generate the
enum types and the operation/action table instead of typing them. Generated files stay
ignored by git; embed only data we derive ourselves, or load at build time from the user's
local copy (never commit it).

## Work packages (no file overlap)

Agree one small interface module first (owned by WP-A, written before the others start):
an XML element tree type (parse, find by QName, serialize with prefix map); a `PrintFault`
enum (the 15 subcodes + `ActionNotSupported`, `MustUnderstand`, Sender/Receiver, reason, detail)
with `to_envelope`; `SoapRequest`/`SoapReply`; the `PrintOp` enum with its action table and
endpoint kind; a `JobStore` trait and `PrintJob` struct (9 states, reasons, counters, documents,
ticket); an `EventBus` trait and the six-variant `PrintEvent`; a `PrinterModel` that answers
`element(qname)`.

| WP | Scope | Owns (create/rewrite) | Closes gaps |
|----|-------|-----------------------|-------------|
| **A. SOAP and transport** | One XML tree path, envelope/WS-A, fault builder, strict MTOM/XOP (boundary-based, cid resolution, size caps), shared interface types | `wsd/xml.rs`, `wsd/envelope.rs`, `wsd/soap.rs`, `wsd/mtom.rs`, `wsd/fault.rs`, `wsd/types.rs` | 1, 4, tree parsing |
| **B. Print operations and job state** | Dispatch by Action+endpoint, all v1.0 + v1.2 + v2.0 operations, ticket parsing/validation, MustHonor, job FSM with timers and ids, history, capture inbox | `wsd/mod.rs` (dispatch + wiring only), `wsd/ops.rs`, `wsd/ticket.rs`, `state.rs`, call-site edits in `ipp/mod.rs` and `jetdirect.rs` (keep `create_job`/`capture_document` signatures) | 2, 3, 5, 7, 9 (ops side) |
| **C. Printer model and schema** | Profile schema with allowed values/defaults/bins/consumables/conditions/version flags, `PrinterModel`, element builders (Description/Configuration/Status/DefaultTicket/Capabilities/JobStatus/JobSummary), schema validator loadable without the source tree, self-check of every outgoing body | `wsd/elements.rs`, `wsd/print_schema.rs`, `wsd/printer_model.rs`, `profile.rs`, `profiles/*` | 8 and honest capabilities |
| **D. Eventing, discovery, metadata** | Subscription manager (Subscribe/Renew/GetStatus/Unsubscribe/SubscriptionEnd, expiry, filters, rate limit, CompleteState vs cumulative buffering, delivery client), six event builders, XML Probe/Resolve, directed discovery, Transfer Get + GetMetadata moved out of `mod.rs`, router cleanup | `wsd/eventing.rs`, `wsd/events.rs`, `wsd/metadata.rs`, `discovery.rs`, `main.rs`, `lab.rs` (unchanged) | 6, 10, endpoint split |

Order: A's interface module first; then B, C, D in parallel; B registers its handlers through
one function exported from `wsd/mod.rs`, D owns all `main.rs` edits. Final integration: wire the
validator from C into every handler reply under a debug/test switch so a reply that does not
validate against the generated model fails the test suite.

Quirk policy: Brother (or any vendor) behaviour such as the `JobToken` extra element belongs in
named opt-in profile modes, not in the default path. The reference is the schema, the spec and
Windows measurements ([sources.md](reference/sources.md)).
