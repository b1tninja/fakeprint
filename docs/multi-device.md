# Multi-device fakeprint and the printer device manager (design contract)

One fakeprint process emulates **N printers at once**, each with its own profile, UUIDs, endpoints,
inbox, job engine and fault state. A persistent registry and a manager turn it into a *device manager*:
printers are added, removed, inspected and (for Windows) installed one at a time, from a tray icon, a
small web page, the command line and Python. This mirrors what `D:\code\fakescan` already has
(`docs/multi-device.md` there); section 9 lists what is scanner-specific and must not be copied.

This file is the contract between three implementation work packages (section 8). If you must deviate,
change this file in the same change and say so in your report. Status: design; M0 has landed (below), the rest is not
implemented yet. Nobody commits; nobody runs `git stash`.

**Owner decisions** (answers to section 11): Q1 yes (`--manage` defaults to `127.0.0.1:3921`, discovery off; bare
`fakeprint` keeps `0.0.0.0` + discovery on); Q4 the tray "Offline" toggle means `not_accepting_jobs` (plus a
stopped/offline printer state) only and never stops answering HTTP; Q7 yes (lab + MCP are ported into
`fakeprint-python`, the old `D:/code/fakeprint/python` is left alone); Q10 ghost clean-up deferred; Q8 `ensure`
loses its auto-install and nothing ever auto-Adds; Q2, Q3, Q5, Q6, Q9 take the proposed defaults.

## M0 status (landed)

Shared types, stubs and Cargo features are in place so WP-T and WP-P can start without touching shared files.
Compiles; `cargo test -p fakeprint` and `clippy --all-targets -D warnings` are green.

- `server.rs`: `DeviceConfig` (`new`, `derived`), `DEFAULT_DEVICE_ID`, `valid_device_id`, `ResolvedDevice`,
  `ServerConfig.{devices, registry, manage}`, `ServerConfig::{with_devices, managed, resolved_devices}`
  (inheritance, synthesized `default` device, inbox rule, IPP-UUID derivation and all section 1.1 validation),
  `router_with(world, opts, Option<DeviceCtx>)`, `ServerHandle::{manager, devices, device}`, `GET /v1/devices`.
  `serve` runs exactly one device in M0 (`NotYet` for more, and for `manage`).
- `manager.rs`: `DeviceManager`, `DeviceHandle`, `DeviceCtx`, `DeviceInfo`/`DeviceLive`/`QueueInfo` serde types,
  `SettingsLauncher`, `NotYet`. Real for the default device: `list`, `get`, `default_device`, `queue` (empty).
  `NotYet` errors (HTTP 501 later): `add remove remove_row reconfigure` (M1), `mark_* forget reconcile_with_windows
  queue_reorder` (M2).
- `registry.rs`: `devices.json` serde schema (`Registry`, `RegistryDevice`, `Install`, `InstallState`,
  `WindowsPrinter`, `ConfigInfo`, ...), `capability_fingerprint`, `config_fingerprint`, `capability_class`,
  `RegistryStore` (`load` real, `update` M2), `registry_path()` honoring `FAKEPRINT_REGISTRY`.
- `tray_model.rs` (always compiled), `tray.rs` (`--features tray`, Windows), `ui.rs` (default feature `ui`): empty
  documented stubs owned by WP-T. `lib.rs` declares them. Cargo: `default = ["ui"]`, `tray` pulls in the
  optional Windows-only `tray-icon` + `native-windows-gui`; nothing new is compiled in the default build.
- `World::ipp_uri(prefix)` and `World::device_prefix()` (the IPP path follows the device endpoint prefix),
  `World::live_profile()`, `lab::derive_device_uuid`.

## M1 status (landed)

Multi-device core works in process and over HTTP; `cargo test -p fakeprint` (incl. `tests/multi_device.rs`,
`tests/multi_device_m1.rs`), clippy `-D warnings`, `--no-default-features` and `--features tray` checks are green.

- `serve` builds every resolved device (`server::build_device`: world, directed-discovery service, router,
  ticker, event pump, optional JetDirect listener); `ServerHandle::router()` is now the **manager router**
  (`manager_http::manager_router`): `/health`, `/v1/devices` (GET list, POST add), `DELETE /v1/devices/{id}[?forget=true]`,
  `POST /v1/devices/{id}/reconfigure {profile}`, `GET /v1/profiles`, `GET /v1/queue`, dynamic `/d/{id}[/...]` dispatch
  (404 `{"error":"unknown device"}`), every other path = the default device. Mutations are loopback-peer only (403);
  a request without peer info (in-process `oneshot`) is allowed.
- Per-device additions: `POST /d/{id}/v1/levels` (also `/v1/levels` for the default device), `Status.device_id`,
  `POST /v1/ctl` honors `args.device`, `/v1/profile {id}` goes through `DeviceManager::reconfigure`.
- `DeviceManager::{add, remove, remove_row, reconfigure}` are real; errors are `manager::DeviceError`
  (`BadRequest` 400, `NotFound` 404, `Conflict` 409), `NotYet` is 501; `manager::error_status(&anyhow::Error)`.
  The default device cannot be removed (400). `DeviceManager::world_of(id)`, `ServerConfig::resolve_device`,
  `manager_http::{manager_router, profile_summaries, peer_allowed}` are public.
- JetDirect: owned by the device builder. The default device gets the daemon port (9100, daemon only, soft failure as
  before); any device (embedded too) may opt in with an explicit `jetdirect_port` (`Some(0)` = ephemeral, the real port is
  reported in `DeviceInfo.jetdirect_port`); duplicate ports and the HTTP port are 409. Listeners bind the **listen IP**
  (was `0.0.0.0`). Removing a device closes its listener.
- Not yet (M2): peer-contact `probe`, install state, hub/queue (non-default devices do not multicast; the default device
  keeps the old single announcer), registry-backed `manage`, ctl/pipe manager commands. The pipe still serves the default world.
- `RouterOptions` unchanged; new dependency `tower` (util) is now a normal dependency (already in the tree).

## M2 status (landed)

- `registry.rs`: `RegistryStore::update` (in-process mutex + OS lock on `<file>.lock`, fresh read, temp file + fsync +
  rename; unknown fields and `queue_order` kept), `match_printer` / `port_uuids` / `Candidate` / `PrinterMatch`
  (section 3.4), `now_rfc3339`.
- `manager.rs`: install state machine (`mark_probed`, `mark_installed` refused unless UUID/IPP URL match,
  `mark_installed_legacy`, `mark_stale`, `forget`, `refresh_config` = staleness + healing, `reconcile_with_windows`),
  persistence of every change to the registry row (managed daemon only), `registry()`, the serial install queue
  (`queue`, `queue_step`, `queue_advance`, `queue_skip`, `queue_reorder`, `set_queue_timers`, `enable_queue`) and
  `attach_hub`. `DeviceCtx::new` / throttled `probe` (30 s) is wired into every WSD, IPP and directed-discovery request
  of a device (middleware), so `new -> probed` is recorded by the daemon itself.
  `DeviceInfo.announcing` / `queue_position` are real. `QueueInfo.position` is "n of total" for the head (installed
  devices count as done), `total` is the number of devices; `remaining` excludes the head. A stale device whose reason is
  `not listed by Windows` is offline: not queued, and it keeps announcing.
- `discovery.rs`: `DiscoveryHub` (one socket, members, `register/unregister/set_announcing/is_announcing/on_peer/shutdown`,
  Hello on start, Bye on stop; `spawn` takes any `DiscoverySocket`). The old single announcer is gone. The hub exists only
  in a daemon with discovery on (`daemon.discovery` or a row/device `discovery: true`); otherwise the queue is just an order.
- HTTP (loopback-only except GET): `GET /v1/registry`, `POST /v1/reconcile`, `POST /v1/devices/{id}/installed`,
  `POST /v1/devices/{id}/forget`, `POST /v1/queue {order}`, `POST /v1/queue/advance`, `POST /v1/queue/skip`, `/ui` (WP-T's page).
  ctl commands (`POST /v1/ctl` and the named pipe, which now serves the manager and honors `args.device`): `devices
  device_add device_remove reconfigure installed mark_added forget reconcile levels queue queue_advance queue_skip
  queue_reorder registry profiles open_settings shutdown` (`shutdown` needs `confirm:true`, `open_settings` only `printers`).
- `ServerConfig::manage` is implemented: the devices are the registry rows of `listen` (first = default); a first run
  creates the legacy `default` row (UUID `...f3`). A row's install state survives restarts; a row whose profile changed
  while the daemon was down becomes `stale` (`profile changed`). `ServerConfig.discovery_timings` (tests use immediate).
- `main.rs`: `--manage` (defaults `127.0.0.1:3921`, discovery off; `--discovery` opt in), `--no-discovery`, `--device
  ID=PROFILE[@UUID]`, `--no-tray`, `--registry`, `fakeprint [--profiles P ...] daemon install|uninstall|status`
  (`daemon.rs`, pure command builders; `schtasks` only runs when a human invokes it). Bare `fakeprint` keeps `0.0.0.0` +
  discovery. The tray starts for `--manage` with `--features tray`. The instance lock records the default device UUID.
- `scripts`: `ensure` no longer installs anything and no longer marks `added.pnp` from a name heuristic.
- The legacy default device (`id == "default"`, first) keeps `inbox_dir` itself as its inbox even when listed explicitly.

Deviations from the text above: (1) `tray` is **not** a default feature yet (Cargo cannot default a feature per
target and the owner wants no new heavy default deps); flip it at integration. (2) `RouterOptions` is unchanged
(the python binding builds it as a literal); the manager context goes through `router_with`. (3)
`live.printer_state` is the wire token (`Idle`, `Processing`, ...), same as `/v1/status`, not lower case. (4)
`lab::derive_device_uuid` clears the lowest bit of the hash so a derived IPP UUID (+1) can never equal another
derived WSD UUID. (5) The `ctl`/pipe/`main.rs` flag work (`--manage`, Q1 defaults) is still M2; `main.rs` is untouched.

## 0. Rules that shape everything (from the lab, not negotiable)

1. **One daemon, N printers.** One port, one registry, one instance lock, one named pipe, one discovery
   loop. Process-per-printer is rejected (ports, firewall rules, locks; the expensive thing is the
   Windows Add, not the process).
2. **Two modes.** *Daemon* (`fakeprint --manage`): long-lived, registry-backed, discovery optional, the
   only mode that ever touches Windows. *Embedded* (`ServerConfig::embedded`, Python `Server`): port 0,
   loopback, no registry, no discovery, no JetDirect, no pipe, no lock, no tray; devices live and die
   with the test.
3. **Install per capability class, not per test.** Only a change in the *published capability
   fingerprint* needs a Windows re-Add (section 4.3). Levels, conditions, faults, timing, jobs, inbox
   content are runtime state and never make a device stale.
4. **Windows adds one new WSD printer at a time (~30 s each).** A serial install queue (section 3.3):
   never announce several uninstalled printers together.
5. **Never auto-Add anything.** No code path of the daemon, tray, UI, ctl, Python or harness calls
   `Add-Printer`, `Add-PrinterPort`, `pnputil` or similar. "Add in Windows" only opens
   `ms-settings:printers` and shows instructions. (Consequence for existing scripts: see 4.6.)
6. **Never confused with real hardware.** Every name and UUID is derived from fakeprint's own prefix and
   UUID namespace (section 1.3). Ownership of a Windows object is decided by UUID found in its port/id
   string, never by name. A Windows printer whose UUID is not in the registry is `foreign` and is
   reported only, never touched, never printed to by the harness.
7. **Loopback by default.** Embedded: loopback only (enforced). Daemon: the `--manage` default listen is
   `127.0.0.1:3921`; LAN exposure needs an explicit `listen` (row or `--listen`) and WS-Discovery
   multicast needs an explicit `discovery: true` (row) or `--discovery`. The plain no-flag
   `fakeprint` invocation keeps today's behaviour (`0.0.0.0:3921`, discovery on) for backward
   compatibility; see open question Q1.

## 1. Device model

### 1.1 Config structs (`server.rs`, WP-M)

```rust
pub struct DeviceConfig {
    pub id: String,                     // slug [A-Za-z0-9_-]{1,32}; "default" = legacy device
    pub profile: String,                // profile id (profiles/<id>.json)
    pub device_uuid: Uuid,              // WSD UUID; default device keeps lab::LAB_DEVICE_UUID (...00f3)
    pub ipp_uuid: Option<Uuid>,         // None: lab::derive_ipp_uuid(device_uuid)
    pub friendly_name: Option<String>,  // None: default device = profile label, others "fakeprint <id>"
    pub inbox_dir: Option<PathBuf>,     // None: see 1.4
    pub timing: Option<PrintTiming>,    // None: inherit ServerConfig.timing
    pub limits: Option<PrintLimits>,    // None: inherit ServerConfig.limits
    pub jetdirect_port: Option<u16>,    // see 2.4; None = no RAW listener for this device
    pub discovery: Option<bool>,        // None: inherit server (daemon: row value)
}
impl DeviceConfig { pub fn new(id, profile, uuid) -> Self; pub fn derived(id, profile) -> Self }

pub struct ServerConfig {               // existing fields stay; these are added
    pub devices: Vec<DeviceConfig>,     // first = default device. Empty = one synthesized "default"
    pub registry: Option<PathBuf>,      // daemon only; None for embedded
    pub manage: bool,                   // daemon: devices come from registry rows for this `listen`
}
```

- Existing per-server fields (`profile`, `profile_name`, `device_uuid`, `ipp_uuid`, `inbox_dir`, `limits`,
  `timing`, `clock`, `tick`, `validate_replies`, `address_policy`, `initial_events`, `event_rate_secs`,
  `wsd`, `ipp`, `directed_discovery`) become **defaults inherited by every device**; a `DeviceConfig`
  field overrides them. With `devices` empty, `serve` synthesizes `DeviceConfig { id: "default", ..}`
  from those legacy fields, so every existing caller (main.rs, fakeprint-python, tests) keeps working
  byte for byte.
- `ServerConfig::embedded(inbox)`: unchanged signature and meaning (one default device); gains
  `fn with_devices(self, Vec<DeviceConfig>) -> Self`. Still forces `registry: None`, `manage: false`,
  discovery/probe/JetDirect/pipe/lock off.
- `ServerConfig::daemon(profiles, inbox)`: unchanged; `registry` stays `None` and `manage` false until
  `main.rs` sets them for `--manage`. New `ServerConfig::managed(profiles, inbox, registry_path)` sets
  `manage: true`, `registry: Some(..)`, `devices: []` (the manager loads the rows for `listen`).
- Validation in `serve`/`DeviceManager::add` (errors are readable strings, HTTP 400/409): id slug rule;
  duplicate id; duplicate `device_uuid`; **no device UUID may equal another device's UUID or IPP UUID**
  (so `...f3` + `...f4` clashes are caught); unknown profile; friendly name not starting with
  `fakeprint` (case-insensitive) unless it is the legacy default device (prefix rule, rule 6).

### 1.2 Per-device state

Each running device owns, with no shared mutable state between devices: its `World` (profile, inbox,
discovery counters), `PrintCtx`/`JobEngine` (jobs, faults, conditions, timing, injector, validator),
`EventManager` (subscriptions and the event pump task), a ticker task (`spawn_ticker`), its prebuilt
`Router` (the existing `server::router`, unchanged and unprefixed), and its endpoint URLs. Shared across
devices: listener, clock, `ServerConfig` defaults, registry store, instance lock, pipe, discovery hub
(3.1), JetDirect listeners.

### 1.3 Identity derivation

- Default device: UUID `...00f3`, IPP UUID `...00f4`, friendly name = profile label (unchanged).
- Others without an explicit UUID: `00000000-0000-4000-8000-<12 hex>` where the 48 bits are FNV-1a of the
  string `"fakeprint:" + id` (the salt keeps fakeprint ids from colliding with fakescan's derivation of
  the same id). Stable across restarts and machines. A stored registry UUID always wins over derivation
  (re-add of a known id never changes identity).
- IPP UUID: `derive_ipp_uuid(device_uuid)` unless set. Registry rows store it when explicit.
- Endpoints (advertised host `H:P`): default device `http://H:P/WebServices/Device`,
  `.../WebServices/PrinterService`, `ipp://H:P/ipp/print`; device `x`: the same with `/d/x` inserted
  after the authority (`http://H:P/d/x/WebServices/Device`, `ipp://H:P/d/x/ipp/print`).
  `World::ipp_uri()` today hard-codes `/ipp/print`; WP-M changes it to derive the path from the device
  endpoint prefix.

### 1.4 Inbox

Synthesized legacy default: `ServerConfig.inbox_dir` itself (unchanged). Explicit `devices`:
`<inbox_dir>/<id>/` for every device unless `DeviceConfig.inbox_dir` is set. `Inbox` keeps an in-memory
list (it never scans the directory), so nesting under a parent is safe. `DeviceInfo.inbox_dir` reports
the absolute path (the tray's "Open inbox" uses exactly this string and nothing else).

### 1.5 Library API additions (`server.rs`, `manager.rs`, WP-M)

```rust
impl ServerHandle {
    pub fn manager(&self) -> Arc<DeviceManager>;
    pub async fn devices(&self) -> Vec<DeviceInfo>;
    pub async fn device(&self, id: &str) -> Option<DeviceHandle>;
    // existing single-device accessors (device_uuid, world, ctx, inject, injector, events, validator,
    // inbox_dir, device_endpoint, service_endpoint, router) keep returning the DEFAULT device's.
    // router() returns the manager router (with /d/{id} dispatch), so Router::nest keeps working.
}
pub struct DeviceHandle {               // cheap clone
    pub info: DeviceInfo,               // see 2.2 for the JSON shape
    pub world: SharedWorld,             // plus the same helpers as ServerHandle, per device:
}                                       // ctx(), events(), injector(), inject(), validator(), inbox_dir(),
                                        // device_endpoint(), service_endpoint(), ipp_endpoint(), router()
pub struct DeviceManager { .. }         // manager.rs
impl DeviceManager {
    pub fn new(cfg: ManagerConfig) -> (Arc<Self>, watch::Receiver<bool> /* exit request */);
    pub async fn list(&self) -> Vec<DeviceInfo>;
    pub async fn get(&self, id: &str) -> Option<DeviceHandle>;
    pub async fn default_device(&self) -> Option<DeviceHandle>;
    pub async fn add(&self, dc: DeviceConfig) -> Result<DeviceInfo>;
    pub async fn remove(&self, id: &str) -> Result<()>;                 // keep row
    pub async fn remove_row(&self, id: &str, drop_row: bool) -> Result<()>;
    pub async fn reconfigure(&self, id: &str, profile_id: &str) -> Result<DeviceInfo>;
    pub async fn mark_probed(&self, id: &str, peer: &str) -> Result<bool>;
    pub async fn mark_installed(&self, id: &str, win: WindowsPrinter) -> Result<()>;
    pub async fn mark_installed_legacy(&self, id: &str) -> Result<()>;   // added.pnp
    pub async fn mark_stale(&self, id: &str, reason: &str) -> Result<()>;
    pub async fn forget(&self, id: &str) -> Result<()>;
    pub fn reconcile_with_windows(&self, listing: &[WindowsPrinter]) -> Result<ReconcileResult>;
    pub async fn queue(&self) -> QueueInfo;                              // section 3.3
    pub async fn queue_advance(&self) -> QueueInfo;
    pub async fn queue_skip(&self) -> QueueInfo;
    pub async fn queue_reorder(&self, ids: &[String]) -> Result<QueueInfo>;
    pub fn set_settings_launcher(&self, l: Arc<dyn SettingsLauncher>);   // tests never open windows
    pub async fn shutdown(&self);                                        // Bye for announcing devices
}
```

`DeviceCtx { manager: Weak<DeviceManager>, id: String }` is installed as an `Extension` on each device
router so the WSD/IPP handlers can report peer contact (`ctx.probe(peer)`, throttled to once per 30 s
per device) and `/v1/ctl` can run manager commands without a back-pointer cycle. `wsd::print_service`
and `ipp::print_handler` gain an optional `ConnectInfo` + `DeviceCtx` and call `probe`.

## 2. HTTP surface

Routing is dynamic (devices can be added at runtime): a single dispatcher looks the device up in the
manager at request time; nothing is nested statically (same constraint as fakescan).

### 2.1 Route table

| Path | Meaning |
|------|---------|
| `GET /health`, `GET /` (per device presentation page at `/d/{id}/`) | liveness; `/` is the default device's page |
| `GET /ui`, `GET /ui/` | operator web page (5.4), loopback peers only (403 otherwise) |
| `GET /v1/profiles` | `[{id, label, formats[], color_modes[], sides[], media_sizes[], input_bins[], versions{v11,v12,v20}, description}]` from the profiles dir |
| `GET /v1/devices` | list of `DeviceInfo` (2.2) |
| `POST /v1/devices` | add: `{id, profile, device_uuid?, friendly_name?, jetdirect_port?}` -> 201 `DeviceInfo`; 400 bad input, 409 duplicate id/uuid |
| `DELETE /v1/devices/{id}[?forget=true]` | stop (Bye if announcing); `forget=true` also drops the registry row |
| `POST /v1/devices/{id}/reconfigure` | `{profile}`: swap profile; marks `stale` only if the published fingerprint changed |
| `POST /v1/devices/{id}/installed` | `{windows:{name,port,driver?,id?}, kind?}` (3.4) |
| `POST /v1/devices/{id}/forget` | drop install info, keep the row |
| `GET /v1/registry` | the registry document (loopback only) |
| `POST /v1/reconcile` | `{printers:[WindowsPrinter..]}` -> `ReconcileResult` |
| `GET /v1/queue`, `POST /v1/queue` | install queue (3.3); `POST {order:[ids]}` reorders |
| `POST /v1/queue/advance`, `POST /v1/queue/skip` | move the head on |
| `GET\|POST /v1/ctl` | ctl surface scoped to the default device; `args.device` selects another |
| `/v1/*`, `/WebServices/Device[/]`, `/WebServices/PrinterService[/]`, `/PrintService`, `/ipp/print[/]`, `/discovery` (directed) | **legacy aliases of the default (first) device**, behaviour unchanged |
| `/d/{id}/v1/*` | the full existing control plane for device `{id}` (status, jobs, jobs/{id}, reset, profile, faults, timing, ctl) plus the new `levels` (2.3) |
| `/d/{id}/WebServices/Device[/]`, `/d/{id}/WebServices/PrinterService[/]`, `/d/{id}/PrintService`, `/d/{id}/ipp/print[/]`, `/d/{id}/<directed discovery path>` | that device's WSD, IPP and directed-discovery endpoints |

Unknown `{id}` -> 404 `{"error":"unknown device"}`. The dispatcher strips `/d/{id}` and hands the
request to the device's own pre-built router, so `endpoint_kind_for_path` (path ends with `/device`)
and request injection (`endpoint: device|print_service|discovery|ipp`) work unchanged and per device.

Access rule (differs from fakescan, which only guards `/ui`): because fakeprint is often bound to the
LAN, **all device-set and manager mutations are loopback-peer only** (`POST/DELETE /v1/devices*`,
`/v1/queue*`, `/v1/reconcile`, `/v1/registry`, ctl commands `open_settings`/`shutdown`/`device_*`),
403 otherwise. The per-device control plane (`/d/{id}/v1/*`, legacy `/v1/*`) stays open as today so a
second lab host can drive faults and read jobs.

### 2.2 `DeviceInfo` (row of `GET /v1/devices`; the interface WP-M, WP-T, WP-P all code against)

```jsonc
{
  "id": "mono", "default": false,
  "uuid": "00000000-0000-4000-8000-0000000000a1",     // WSD device uuid
  "ipp_uuid": "00000000-0000-4000-8000-0000000000a2",
  "profile": "mono-laser", "friendly_name": "fakeprint mono",
  "base_path": "/d/mono",                               // "" for the default device
  "device_endpoint": "http://H:P/d/mono/WebServices/Device",   // null when wsd is off
  "service_endpoint": "http://H:P/d/mono/WebServices/PrinterService",
  "ipp_endpoint": "ipp://H:P/d/mono/ipp/print",        // null when ipp is off
  "jetdirect_port": null,
  "inbox_dir": "C:\\...\\inbox\\mono",
  "announcing": false, "queue_position": 2,            // 3.3; null when installed or embedded
  "needs_readd": false, "needs_readd_reason": null,
  "install": { /* registry install block, 4.2 */ },
  "config": { "fingerprint": "fnv64:..", "published_fingerprint": "fnv64:..", "class": "3fa9c2d1e07b",
              "summary": { "formats": [..], "color_modes": [..], "sides": [..], "media_sizes": [..],
                           "input_bins": [..], "versions": {"v11":false,"v12":false,"v20":false} } },
  "live": {                                            // runtime state; never fingerprinted
    "printer_state": "idle", "state_reasons": ["none"],
    "accepting_jobs": true,
    "active_jobs": 0, "captured_jobs": 3,
    "active_job": null,                                // {id, name, state} of the oldest active job
    "last_job": {"id": 3, "name": "report.pdf", "state": "Completed"},
    "faults": {"paper_out": false, "toner_low": false, "cover_open": false, "paper_jam": false,
               "offline": false},                      // "offline" = not_accepting_jobs (Q4)
    "timing_preset": "default",                        // instant | realistic | default | custom
    "consumables": [{"name": "Black", "level": 80}],
    "inputs": [{"name": "Tray1", "level": 100}]
  }
}
```

`class` is the first 12 hex digits of the published fingerprint: the *capability class* handle a
harness uses to pick "an installed printer of this shape". `GET /d/{id}/v1/status` (existing `Status`)
is unchanged and additionally carries `device_id`.

### 2.3 New per-device control calls

- `POST /d/{id}/v1/levels` `{consumables:{"Black":15}, inputs:{"Tray1":0}}` -> `live` block. Wraps the
  existing `ProfilePrinterModel::set_consumable_level` (bins need the same setter; WP-M adds it).
  Runtime only, publishes `PrinterElementsChange`, never touches fingerprints.
- `POST /d/{id}/v1/faults` already takes `paper_out`, `toner_low`, `cover_open`, `paper_jam`,
  `not_accepting_jobs`, `reset`, ...; the tray toggles map onto those names (5.3).
- `POST /d/{id}/v1/timing {"preset": ...}` unchanged. `POST /d/{id}/v1/reset` unchanged (runtime reset;
  re-applies no profile, changes no install state).

### 2.4 IPP and JetDirect per device

- **IPP** needs no extra port: it is one more path on the HTTP port, `/d/{id}/ipp/print`; each device
  answers with its own `printer-uri` and `printer-uuid` (IPP UUID, never equal to the WSD UUID). The
  legacy `/ipp/print` is the default device.
- **JetDirect :9100 is per host, not per device.** Decision: the RAW listener is owned by the manager,
  not by a world. The **default device alone** gets `:9100` (today's behaviour, daemon only). Any other
  device may opt in with an explicit `jetdirect_port` (row field / `POST /v1/devices` body); the manager
  binds one listener per distinct port, refuses a port used by another device or by the HTTP listener, and
  routes each listener's bytes to its device's world. Windows can create TCPMON ports on a non-9100
  number (`Add-PrinterPort -PortNumber`), so a per-device port is usable; it is just never allocated
  automatically. JetDirect binds the **listen IP** (loopback unless `listen` is LAN), not `0.0.0.0`
  (today's hard-coded bind is a loopback-default violation; WP-M fixes it). Embedded: never.
  Removing a device closes its listener.

## 3. Discovery per device

### 3.1 Discovery hub (`discovery.rs`, WP-M)

One shared UDP endpoint per server (the multicast send socket and, with `probe_listen`, the :3702
receive socket), many `DiscoveryService` members. Rationale: with several sockets reusing :3702 it is
unspecified which one receives a unicast datagram, so per-device sockets (fakescan's approach) cannot
answer Probes reliably. The hub receives every datagram once and offers it to each *announcing*
member; a member answers only for its own UUID/types. API sketch:

```rust
pub struct DiscoveryHub { .. }
impl DiscoveryHub {
    pub async fn start(cfg: HubConfig) -> Result<Arc<Self>>;       // binds sockets once
    pub fn register(&self, member: Arc<DiscoveryService>, cfg: &DiscoveryConfig); // Hello burst if announcing
    pub fn unregister(&self, uuid: Uuid);                                          // Bye burst
    pub fn set_announcing(&self, uuid: Uuid, on: bool);                            // Hello on false->true, Bye on true->false
    pub fn on_peer(&self, f: Arc<dyn Fn(Uuid, IpAddr) + Send + Sync>);             // Probe/Resolve -> DeviceCtx::probe
    pub async fn shutdown(&self);                                                  // Bye for announcing members only
}
```

The pure `DiscoveryCore`/`DiscoveryService` (messages, AppSequence, scopes, types, quirks) are reused
untouched. Each member keeps its own `InstanceId`/`MessageNumber`; `InstanceId` must rise after every
daemon restart (existing `boot_sequence_id`). Embedded servers create no hub.

### 3.2 XAddrs and per-device behaviour

- XAddrs of a device = its `device_endpoint` (default device: legacy `/WebServices/Device`; others:
  `/d/{id}/WebServices/Device`). Types stay `wsdp:Device wprt:PrintDeviceType`. Hello/Bye per device;
  Probe/Resolve answered per device (Resolve by that device's EPR UUID only; a type-matching Probe is
  answered by every announcing member, each with its own ProbeMatch).
- **Directed discovery** (`POST` to the directed path, pure HTTP, no multicast) is per device
  (`/d/{id}/...`) and legacy-aliased for the default device. It answers regardless of `announcing`
  (only the harness uses it; Windows never sees it).
- Peer contact: a Probe/Resolve addressed to a device and any WSD/IPP request on its endpoints call
  `DeviceCtx::probe(peer)` (throttled), which is what flips `new -> probed` (4.2).

### 3.3 Serial install queue

Operational fact: Windows cannot cope with several new WSD printers appearing together; each Add takes
~30 s.

- **Announce rule.** A device multicasts Hello (and answers Probe/Resolve) iff discovery is enabled for
  it **and** (`install.state == installed` **or** it is the current queue head). Every other
  non-installed device is *dormant*: registered, SOAP/IPP endpoints answer, but silent on the wire. Bye is
  sent only for devices that were announcing.
- **Queue** = registry devices whose state is `new`, `probed` or `stale` (excluding the reason
  `not listed by Windows`, which is offline, not an Add request), in registry order or in the explicit
  order set by `POST /v1/queue`. Head = first. At daemon start with five un-added printers exactly one
  announces.
- **Advance.** When the head becomes `installed`, wait `settle` (default 5 s), then promote the next and
  send its Hello burst. If the head is not installed after `head_timeout` (default 180 s, Q5) it rotates
  to the back and is shown `skipped (timed out)`. `advance` and `skip` do the same by hand.
- **Detection.** Primary: the Windows-side reports (`installed` / `reconcile`, 3.4) which Python and the
  harness send. Secondary (daemon, Windows only, read-only, unelevated): while a head is waiting, poll a
  read-only listing of printers every ~5 s and match by UUID only (the same matching as 3.4). A failure
  of that poll is logged and ignored; it never marks anything installed by name.
- **Surfaces.** `GET /v1/queue` -> `{head, remaining:[ids], position, total, head_since, head_timeout_s,
  settle_s}`; `DeviceInfo.announcing` and `queue_position`; tray and UI show `Add now: mono (2 of 5)`.
- Embedded servers never announce, so they have no queue (the endpoints answer with an empty queue).
  Probe-listen/multicast stay off unless explicitly enabled (rule 7).

### 3.4 What "installed" means for a printer

Observed on the Windows side, reported in, never inferred by name:

```jsonc
// WindowsPrinter (input to installed/reconcile; produced by Python winprint or the daemon's read-only poll)
{ "name": "Fake WSD printer", "port": "WSD-3f2b9c1e-7d44-4a55-9e0b-1c2d3e4f5a6b",
  "driver": "Microsoft IPP Class Driver", "id": "<opaque>",
  "wsd_uuid": "00000000-0000-4000-8000-0000000000a1",              // optional, see below
  "location": "http://192.168.1.70:3921/WebServices/Device" }      // optional
```

**Measured on Windows:** a WSD printer's port is named `WSD-<random port guid>`, *not* by the device UUID. The device
UUID is the value `Printer UUID` under `HKLM\SYSTEM\CurrentControlSet\Control\Print\Monitors\WSD Port\Ports\<port>`
(next to `Install Protocol`, `Print Service Type`, `WSD PortId`), the printer's `Location` property is the device
endpoint URL, and PnP nodes `SWD\DAFWSDPROVIDER\URN:UUID:<uuid>` exist. Python reads the registry value read-only and
sends it as `wsd_uuid`; `location` comes from the printer's `pLocation`.

Matching (`registry::match_printer`; never by name). Evidence that identifies a device: (a) `wsd_uuid` equal to its WSD
UUID (any case, optional `{}` / `urn:uuid:`), (a') the one UUID in the port string equal to it (older `WSD-<uuid>`
ports), (a'') `location` equal to its device endpoint URL (our `host:port`, path `<base>/WebServices/Device`, trailing
slash and case ignored) -> install `kind: "wsd"`; (b) the port is an `http(s)`/`ipp(s)` URL whose authority is this
server's advertised host:port and whose path is exactly the device's IPP path -> `kind: "ipp"`. All evidence present must
agree: two different devices named, a port naming two UUIDs, or an explicit `wsd_uuid` that is not one of ours make the
printer **foreign** (as does everything else: TCPMON `host:9100`, look-alike names, a UUID not in the registry). Foreign
printers are listed in `ReconcileResult.foreign` and never installed, scanned or printed to by a harness. `installed` is
refused unless (a) or (b) holds. A random port guid that matches nothing is simply ignored.

Registry rows belong to a **port**, not to an exact listen address: `--listen 127.0.0.1:3921` and `--listen 0.0.0.0:3921`
read the same rows (the instance lock is per port); the row's `listen` is rewritten to the current address, and several
rows of one device under different addresses of the port collapse into one (the exact-address row, else the most recently
seen). Before this rule, restarting with another `--listen` hid the printers added at run time.

## 4. Registry, manager process, ctl

### 4.1 File and store (`registry.rs`, WP-M)

`%LOCALAPPDATA%\fakeprint\devices.json` (override `FAKEPRINT_REGISTRY`; inside the crate's own unit
tests `registry_path()` panics when the variable is unset, so tests cannot reach the real file). One row
per emulated printer; rows with the same `listen` belong to one process. Writes go through
`RegistryStore::update`: in-process mutex + OS file lock on `devices.json.lock`, fresh read, mutation,
atomic replace (temp file in the same directory, fsync, rename). PowerShell writers use the same lock
primitive. Unknown fields are preserved on rewrite; timestamps RFC 3339 UTC. Existing single-row files
(with `added.pnp`) load unchanged. `lab::registry_path()` moves into the new module.

### 4.2 Schema

```jsonc
{
  "advertise_default": "",                       // LAN IP when a row has none
  "port_policy": { "base": 3921, "step": 2 },    // fakeprint = odd ports
  "queue_order": ["mono", "color"],              // optional explicit install order
  "devices": [{
    "name": "mono",                              // device id (the default device of a --lab-name instance uses the lab name)
    "listen": "127.0.0.1:3921", "advertise": "",
    "device_uuid": "00000000-0000-4000-8000-0000000000a1",
    "ipp_uuid": null,                            // explicit only
    "profile": "mono-laser", "friendly_name": "fakeprint mono",
    "discovery": null,                           // absent = inherit the daemon's --discovery; written only when set per printer (with "discovery_explicit": true). A bare stored false without the marker (older files) means inherit.
    "jetdirect_port": null,
    "exe": "", "autostart": false,
    "harness": null,                             // optional runtime state applied after start (faults/timing/levels)
    "added": { "pnp": false },                   // legacy mirror of install.state == installed
    "config": { "fingerprint": "fnv64:..", "published_fingerprint": "fnv64:..",
                "summary": { /* same as DeviceInfo.config.summary */ } },
    "install": {
      "state": "new",                            // new | probed | installed | stale
      "reason": null, "note": null,
      "first_probed_at": null, "last_seen_at": null, "last_peer": null,
      "installed_at": null, "verified_at": null,
      "kind": null,                              // "wsd" | "ipp"
      "windows": { "name": "", "port": "", "driver": "", "id": "" },
      "published_fingerprint_at_install": null,
      "stale_from": null, "stale_at": null
    }
  }]
}
```

### 4.3 Capability fingerprint

- `config.published_fingerprint` = hash over what a Windows client caches at Add time: the existing
  `printer_model::published_fingerprint` input (PrinterDescription, PrinterConfiguration,
  DefaultPrintTicket, PrinterCapabilities, v1.1/v1.2/v2.0 sections, with consumable/bin **levels
  zeroed**) **plus** the identity Windows binds to: `friendly_name`, manufacturer, model, IEEE 1284
  device id (`ThisModel`/`ThisDevice`). Excluded: status, conditions, levels, faults, timing, firmware
  version, location, serial (Q6 to confirm against a live Add). Hash stays `fnv64:` (no new dependency;
  WP-M adds `capability_fingerprint(profile, friendly_name)` next to the existing function).
- `config.fingerprint` = hash of the whole effective profile JSON (canonical key order), including
  runtime-only knobs such as levels; informational, never used for staleness.
- Formats, duplex (`sides`), colour modes, media sizes, input bins/output bins, version flags
  (v1.1/1.2/2.0) and printer description all live inside the published hash by construction.

### 4.4 State rules (same shape as fakescan)

- `new -> probed`: first peer contact recorded by the daemon itself (throttled `last_seen_at`, no
  per-SOAP-call rewrites).
- `probed|new -> installed`: only via a Windows-side report (`installed` / `reconcile`); sets
  `kind`, `windows`, `installed_at`, `verified_at`, `published_fingerprint_at_install`, mirrors
  `added.pnp = true`. Refused unless the match rule (3.4) holds.
- `installed -> stale`: reconcile finds no matching printer (reason `not listed by Windows`, treated as
  *offline*, not a request to Add), or a profile change alters the published fingerprint (reason prefix
  `profile changed`).
- Healing: a `profile changed` stale whose published fingerprint equals
  `published_fingerprint_at_install` again goes back to `installed` (applies on `/v1/devices/{id}/reconfigure`,
  `POST /d/{id}/v1/profile`, ctl `profile`, and daemon start-up reload). `not listed by Windows` heals
  only by a reconcile that sees the device. `stale_from`/`stale_at` as in fakescan; the tray holds a
  `profile changed` stale for 20 s before treating it as an Add request.
- `POST /d/{id}/v1/profile` and ctl `profile` go through `DeviceManager::reconfigure` for every served
  device, so a bare-router path without a manager is only for library mounts.
- Embedded servers keep the same state machine in an in-memory store.

### 4.5 Manager process (`main.rs`, `daemon.rs`, `lab.rs`, `pipe.rs`; WP-M)

- `fakeprint --manage [--listen L] [--advertise A] [--profiles P] [--inbox I] [--discovery]
  [--probe-listen] [--no-tray]`: loads rows whose `listen` equals L (default `127.0.0.1:3921`), starts
  each with its stored UUID/profile (never rewrites a UUID), applies the queue rule, starts the tray if
  interactive (WP-T hook, 5.2). `--device ID=PROFILE[@UUID]` stays for ad-hoc multi-printer runs without
  a registry. Flags `--lab-name`, `--no-lock`, `--no-jetdirect`, `--jetdirect-port` keep working.
- **Instance lock**: one per listen port (`fakeprint-<port>.lock` in the temp dir, stale-PID reclaim),
  not per device. The lock file currently writes the lab UUID; it writes the default device's UUID.
- **Named pipe**: one per server, `\\.\pipe\fakeprint-<lab_name>`, same JSON-line protocol as `POST
  /v1/ctl`; the pipe task takes `Arc<DeviceManager>` instead of one `SharedWorld`, `args.device`
  selects the printer, default = default device.
- **Scheduled-task helper** (`daemon.rs`): `fakeprint daemon install | uninstall | status`, a thin
  `schtasks` wrapper creating a per-user ONLOGON, LIMITED task `fakeprint-daemon` that runs
  `"<exe>" --manage --profiles "<abs>" [--listen L] [--advertise A] [--discovery]`. Not a Windows
  service (no elevation; the registry is per user; Add is per user). The command builder and arg lists
  are pure and unit-tested; real `schtasks` is not run in tests.
- `scripts/fakeprint.ps1 ensure` starts one process per distinct `listen` (all rows on it),
  attach-first via `/health`, never kills; `-WhatIf` prints the plan. `list` shows install state.
  The 3921/odd-port band rule stays; fakescan's processes, registry, pipe and firewall rules are never
  touched.

### 4.6 Conflict with today's scripts (must be fixed in WP-M)

`docs/lab-instance.md` says `ensure` auto-runs the non-interactive IPP install (`Add-Printer -IppURL`)
when elevated and `added.pnp` is false. That is an auto-Add and violates rule 5. WP-M changes `ensure`
to never install; `install-ipp` stays as an explicit, human-invoked admin command (`install-ipp -Name
ID`, using that device's IPP URL and UUID), and the manager never calls it.

### 4.7 ctl commands

`status health jobs reset profile faults timing save` (existing) plus: `devices`, `device_add`,
`device_remove`, `reconfigure`, `installed`, `mark_added`, `forget`, `reconcile`, `levels`, `queue`,
`queue_advance`, `queue_skip`, `queue_reorder`, `registry`, `profiles`, `open_settings`, `shutdown`.
`device_*`, `queue_*`, `reconcile`, `registry`, `open_settings`, `shutdown` need a manager context and a
loopback peer. `open_settings {page:"printers"}` -> `{opened:bool}` is the only launchable target
(`cmd /C start "" ms-settings:printers`, injectable launcher). `shutdown` requires `{confirm:true}`.
`GET /v1/ctl` lists `cmds`, `port`, `uuid`, `base_url`, `pipe`, `name`.

## 5. Tray model, tray shell and web UI

### 5.1 Split

- `tray_model.rs` (WP-T): pure, no platform code, no I/O beyond reading a clock passed in; unit-tested
  on every platform.
- `tray.rs` (WP-T): Windows shell, cargo feature `tray` (default on for Windows), crates `tray-icon` +
  `native-windows-gui` (safe message pump; the workspace forbids `unsafe`). Own thread, own message
  loop; init failure or panic is logged and the daemon carries on headless. Started only from `main.rs`
  for `--manage`/`--lab-name` in an interactive session, never from `serve`, never embedded, never with
  `--no-wsd`/`--no-tray`.
- The shell reads `GET /v1/devices` on the daemon's own loopback port every ~2 s (as fakescan does; no
  in-process coupling) and executes actions by calling the HTTP API it already documents.

### 5.2 `tray_model` interface

```rust
pub enum Level { Green, Amber, Red }
pub struct DeviceInput { /* from DeviceInfo JSON: id, friendly_name, state, needs_readd, reason,
    stale_age, announcing, queue_position, inbox_dir, live{..} */ }
impl DeviceInput { pub fn from_json(&serde_json::Value) -> Option<Self>; }

pub struct Summary { pub level: Level, pub counts: Counts, pub devices: Vec<DeviceLine>,
                     pub queue: Option<QueueView>, pub error: Option<String> }
impl Summary {
    pub fn from_devices(&[DeviceInput], queue: Option<&QueueInfo>) -> Summary;
    pub fn error(msg) -> Summary;
    pub fn tooltip(&self) -> String;               // <= 120 chars
    pub fn icon_level(&self) -> Level;
    pub fn menu(&self, profiles: &[ProfileChoice]) -> MenuModel;
}
pub struct MenuModel { pub items: Vec<MenuItem> }
pub struct MenuItem { pub id: MenuId, pub label: String, pub enabled: bool,
                      pub checked: Option<bool>, pub children: Vec<MenuItem> }
pub struct MenuId(pub String);                      // "dev:<id>:fault:paper_out", "dev:<id>:reset", ...
pub enum TrayAction {                               // MenuId::parse(&str) -> Option<TrayAction>
    SetFault { device: String, fault: Fault, on: bool },
    SetTiming { device: String, preset: TimingPreset },
    ResetDevice { device: String },
    OpenInbox { device: String },                   // path = that device's reported inbox_dir only
    RemoveDevice { device: String },                // shell must confirm first
    AddDevice { profile: String, suggested_id: String },
    OpenUi, OpenSettings, AdvanceQueue, SkipQueue, Quit,
}
pub fn next_free_id(profile: &str, existing: &[String]) -> String;   // "mono-laser", "mono-laser-2"..
pub struct TransitionDetector { .. }                // observe(&Summary, now) -> Option<Notification>
pub const NOTIFY_INTERVAL: Duration;                // 10 min per device
pub const PROFILE_STALE_GRACE: Duration;            // 20 s
pub fn add_guidance(friendly_name: &str, position: usize, total: usize) -> String;
```

### 5.3 What the tray shows and does

Icon: green = every printer installed and no active fault; amber = at least one printer needs an Add or
Re-Add (new/probed/stale/needs_readd, not offline or settling) or a fault is raised; red = the daemon
cannot be read. Tooltip e.g. `fakeprint: 3 printers, 1 installed, 1 need Add, 1 offline`.

Menu (top to bottom):

1. One **submenu per printer**, header `mono: installed` (or `needs Add`, `needs re-Add (profile
   changed)`, `offline`, `profile changed, confirming`, `waiting in queue`). Children:
   - disabled status lines: `State: idle`, `Jobs: 1 active, 3 captured`, `Active: report.pdf
     (Processing)` (only when present);
   - `Open inbox folder`;
   - checkable fault toggles **Paper out**, **Toner low**, **Door open**, **Jam**, **Offline**
     (`paper_out`, `toner_low`, `cover_open`, `paper_jam`, `not_accepting_jobs` via `POST
     /d/{id}/v1/faults`); checked state reflects `live.faults`; toggles never change install state;
   - `Timing` submenu with radio items Instant / Realistic / Default (`POST /d/{id}/v1/timing`);
   - `Reset printer` (`POST /d/{id}/v1/reset`);
   - `Remove printer...` (shell asks for confirmation; `DELETE /v1/devices/{id}` without `forget`);
   - when attention is needed: a disabled **guidance line** (`add_guidance`).
2. Separator, `Add printer` submenu: one entry per profile from `GET /v1/profiles`
   (`POST /v1/devices {id: next_free_id, profile}`; the new printer is dormant until it reaches the queue
   head, so adding several cannot flood Windows). The tray never starts a Windows Add.
3. `Add now: mono (2 of 5)` (disabled) and `Skip this printer` / `Advance queue` while a queue exists.
4. `Open device manager` (`http://127.0.0.1:<port>/ui`), `Open Printers & scanners` (`ctl
   open_settings`), `Quit fakeprint` > confirmation (stops every printer, Bye for announcing ones; the
   Windows association survives a restart with the same listen + UUIDs).

**"Add in Windows" guidance text** (constant in `tray_model`, reused verbatim by UI and Python):
`Add now: <friendly name> (<n> of <total>). Open Settings > Bluetooth & devices > Printers & scanners >
Add device, wait for "<friendly name>" to appear and choose Add. Windows takes about 30 seconds; fakeprint
announces the next printer only after this one is installed.` Notification: fire on transition into
needs-Add/Re-Add for the **queue head only**, at most once per device per 10 minutes, plus one summary at
start if anything needs an Add; click opens `ms-settings:printers` (detached hidden PowerShell balloon
helper as fakescan, with the same injection-safe quoting).

### 5.4 Web UI (`ui/index.html`, `ui.rs`; WP-T)

Single self-contained page (inline CSS and vanilla JS, no CDN, no build step, relative URLs only),
embedded with `include_str!`, loopback-only (403 otherwise), `Cache-Control: no-store`. Polls `GET
/v1/devices` and `GET /v1/queue` every 2 s (paused while hidden). Per printer: id, friendly name, UUIDs,
profile, install state (colour coded), `needs_readd` + reason, `announcing`/queue position, endpoints
(copy buttons: device, service, IPP), live state, jobs table from `GET /d/{id}/v1/jobs` (name, user, state,
bytes, file), inbox path, fault toggles, levels sliders (`POST /d/{id}/v1/levels`), timing preset,
Reset, Remove (confirm + "forget registry row"), Reconfigure profile (warns if installed and the target
changes the published fingerprint, using `/v1/profiles` summaries), Add printer (profile picker,
optional id/UUID/name), the guidance text with an "Open Printers & scanners" button (ctl
`open_settings`), and a note that reconcile runs from Python/CLI because the daemon does not enumerate
Windows printers. `ui.rs` exports `INDEX_HTML`, `ui_page`, `list_profiles`, `profile_summaries`,
`check_open_settings`, `is_loopback_peer` (copied shape from fakescan), and WP-M wires them into the
manager router.

## 6. Python (`D:\code\fakeprint-python`, WP-P)

### 6.1 API

```python
from fakeprint import Server, Device
with Server(devices=[Device("mono", profile="mono-laser"),
                     Device("color", profile="color-inkjet-duplex")]) as srv:
    d = srv.devices["mono"]
    d.uuid; d.ipp_uuid; d.friendly_name; d.profile; d.inbox
    d.device_endpoint; d.service_endpoint; d.ipp_endpoint     # /d/mono/... (default device: legacy paths)
    d.printer()          # PrintClient bound to d.service_endpoint
    d.client()           # control Client scoped to /d/mono (faults, timing, levels, jobs, reset)
    d.documents(job_id); d.captured(); d.inject(...); d.set_profile("generic-wsd")
    srv.add_device(Device("late", "generic-wsd")); srv.remove_device("late")
    srv.manager          # Manager bound to this server (in-memory install state, no registry file)
Server() -> one "default" device (today's behaviour); srv.printer()/client() are the default device's.
Client(base_url, device="mono"); PrintClient(srv.devices["mono"].service_endpoint)
```

`Server.__init__` keeps every existing keyword; `devices` is additive and mutually exclusive with
`device_uuid`/`ipp_uuid`/`name` (those apply to the implicit default device).

### 6.2 Modules (mirroring `fakescan-python`; each independently testable without a daemon)

| Module | Content |
|--------|---------|
| `server.py` | `Device`, `DeviceHandle`, `Server.devices`, `add_device`, `remove_device`, `manager`, `open_ui` |
| `registry.py` | typed `DeviceRow/Install/Config/WindowsPrinter/ReconcileResult`, `load()/parse()/from_server()`, `port_uuids()`, `match_printers_to_devices()` (3.4 rules), `reconcile()` |
| `manager.py` | `Manager(target)`: `devices() device() registry() add() remove(forget=) reconfigure() installed() forget() reconcile() wait_installed() reconcile_from_windows() profiles() ui_url() open_ui() open_add_device_settings() status_table() summary_level() queue() advance() skip() reorder(ids) levels()` |
| `ui.py` | `ProfileInfo`, `StatusRow`, `summary_level`, `format_status_table` (same level semantics as `tray_model`) |
| `tray.py` | `find_daemon_exe`, `ensure_daemon()` (attach-first, idempotent; spawns `fakeprint --manage` found via `FAKEPRINT_EXE`, PATH or known build dirs; the tray lives in the daemon) |
| `ctl.py` | `fakeprint-ctl` subcommands `ui devices add remove status queue levels` |
| `lab.py`, `mcp_server.py` | lab CLI (`up` adds all classes but only the queue head announces; `wait` walks the queue printing `Add 2 of 5: mono now`, bounded per device) and MCP tools over the manager (attach-only; device-scoped faults/levels/jobs; no tool adds anything to Windows) |

Reconcile input comes from any iterable of mappings; `registry.py` calls `winprint.list_printers()` only
if that module provides it (import-guarded; `winprint.py` is an unrelated work in progress and is not
owned by this contract).

### 6.3 Native API the binding needs (`src/lib.rs`, WP-P; consumes WP-M)

- `Server(..., devices=[(id, profile, uuid_or_None, friendly_or_None)])` builds
  `ServerConfig::embedded(..).with_devices(..)`; unchanged otherwise.
- `.devices` property: list of `(id, uuid, ipp_uuid, profile, friendly_name, inbox_dir)`.
- Existing per-server methods (`status_json`, `captured_json`, `print_jobs_json`, `documents`,
  `device_cancel`, `reset`, `set_profile(_json)`, `validation_problems`, `validation_problem_count`,
  `inject*`) gain an optional `device=None` argument (default device).
- `control(method, path, body)` dispatches through `ServerHandle::router()` (the manager router) so
  `/d/{id}/v1/...` and `/v1/devices` work in process without a socket.
- No registry, discovery or tray calls from the binding: embedded rule.

## 7. Behaviour for the harness (summary)

The harness (pdfcraft's lab, other tools) attaches to the daemon, never starts, stops or removes
installed printers, picks an installed printer by capability `class`, applies the test's runtime state
through `/d/{id}/v1/*`, and resets that state in teardown. A class that is not installed is a **skip**
carrying the exact Add instructions (friendly name, UUID, position in queue); nothing is added
automatically. Breadth tests use embedded multi-device servers and never touch Windows.

## 8. Work packages and file ownership

Everything is under `D:\code\fakeprint` unless noted. Each agent builds with its own
`CARGO_TARGET_DIR` (never a directory holding a running `fakeprint.exe`). Nobody commits.

### WP-M: Rust multi-device core (one agent)

Owns: `crates/fakeprint/src/{server.rs, state.rs, lab.rs, main.rs, pipe.rs, discovery.rs, jetdirect.rs,
lib.rs, profile.rs}`, `src/ipp/mod.rs`, `src/wsd/{mod.rs, ctx.rs, printer_model.rs}`, new
`src/{manager.rs, registry.rs, daemon.rs, ctl.rs}`, `Cargo.toml` (including the `tray` feature and
Windows-only deps, declared at M0 so WP-T never edits it), `crates/fakeprint/tests/{multi_device.rs,
registry_install.rs, queue.rs}` plus edits to existing tests, `scripts/{fakeprint.ps1, Fakeprint.psm1}`,
`docs/{lab-instance.md, architecture.md, windows.md}` updates (this file stays shared; section changes
are announced in reports).

Delivers: 1 (device model, config, identity, inbox), 2 (routes, dispatch, loopback guards, levels,
JetDirect policy), 3 (hub, queue), 4 (registry, manager, ctl, pipe, daemon task, `ensure` fix).

### WP-T: tray model, tray shell, web UI

Owns: `crates/fakeprint/src/{tray_model.rs, tray.rs, ui.rs}`, `crates/fakeprint/src/ui/index.html`,
`crates/fakeprint/tests/ui.rs`, `crates/fakeprint/assets/` (icon pixels if any), `docs/tray-ui.md`
(optional user-facing notes). Reads but does not edit `server.rs`/`manager.rs`.

Delivers: section 5. Public items WP-M calls: `tray::spawn(TrayConfig{port, quit}) -> Option<TrayHandle>`
(cfg-gated), `ui::{ui_page, list_profiles, INDEX_HTML, check_open_settings}`.

### WP-P: fakeprint-python adoption and harness hooks (`D:\code\fakeprint-python`)

Owns: `src/lib.rs`, `python/fakeprint/{server.py, client.py, soap.py (device scoping only), manager.py,
registry.py, ui.py, tray.py, ctl.py, lab.py, mcp_server.py, __init__.py, _native.pyi}`,
`python/tests/*` (new files), `pyproject.toml` scripts, `README.md`, `docs/needs-from-rust.md`. Not owned:
`python/fakeprint/winprint.py` and the `wpd_*` modules (separate work).

Delivers: section 6 and section 7 hooks.

### Order and interfaces

| Step | Who | Gate / interface |
|------|-----|------------------|
| M0 | WP-M | Land, first and small: `DeviceConfig`/`ServerConfig` fields (default-preserving), `DeviceInfo`/`QueueInfo`/`WindowsPrinter` serde types, empty `tray_model.rs`/`tray.rs`/`ui.rs` stubs and `lib.rs` module lines, Cargo feature and deps. Everyone codes against the JSON shapes in 2.2, 3.3, 3.4, 4.2. |
| M1 | WP-M | Manager + dispatch + per-device build + embedded multi-device; `GET/POST /v1/devices`, `/d/{id}/...`. Unblocks P2 and T2. |
| T1 | WP-T | `tray_model` + `index.html` against recorded JSON fixtures (no server needed). |
| P1 | WP-P | `registry.py`, `ui.py`, `manager.py` parsing/matching against fixtures; no native change. |
| M2 | WP-M | Registry, stale/heal, hub, queue, ctl, pipe, daemon task, scripts. |
| T2 | WP-T | `tray.rs` shell + `ui.rs` routes wired into `manager_router` (after M1). |
| P2 | WP-P | Native `devices`/`device=` API, `Server.devices`, live manager tests (after M1). |
| Integration | all | Cross-check section 2.2 fixtures, run the test plan below, one real daemon smoke test by the user (Windows Add is human). |

## 9. What fakescan has that is scanner-specific (do NOT copy)

Feeder/tray capacity (`feeder-*` profiles, `409` over capacity), `POST /v1/pages|feeder|pattern|slow|
jpeg-com|effects|scenario|loopback|push-scan`, ticket log (`/v1/tickets`, `ValidateScanTicket`/
`CreateScanJob`), image generation and codecs (`image`, `tiff`, `qrcode`, `jpeg-encoder`, `fax`,
`openjpeg2`), WIA glue (WIA ids like `{GUID}\000N`, `WiaInfo`, `lab acquire-dib`, `loopback-wia`),
`GetScannerElements` and its `published_capability_xml`, `ScannerService`/`ScanAvailable` events, Brother
defaults (`brother-wsd` default profile, sticky FEED_READY, Brother-specific quirks and its
read-only-access rules), the even port band and `...f2` lab UUID, `added.pnp` meaning "WIA confirmed",
fakescan's `derived_uuid` (re-salted here), "Imaging" wording in guidance text, the hard-loopback
product promise, and the PnP ghost clean-up machinery (section 13 there: Image nodes, `fdPHost` bounce,
WinRT AEP unpair, UAC helper). The printer equivalent (WSD print nodes, spooler queue and port removal)
is deferred and not part of these work packages; `Remove-Printer` of a fakeprint-owned queue would be a
separate, explicitly confirmed feature.

What is copied in shape: `DeviceManager`, dynamic `/d/{id}` dispatch, `RegistryStore`, install states and
healing, `DeviceCtx::probe`, `tray_model` structure and `TransitionDetector`, `ui.rs` loopback guard,
logon-task helper, ctl/pipe sharing one dispatcher.

## 10. Test plan (no real network, no Windows)

Embedded multi-device (`tests/multi_device.rs`, tower `oneshot` on `ServerHandle::router()` and a
loopback port 0 server): two devices with different profiles have distinct UUIDs, IPP UUIDs, endpoints,
inboxes; a job via `/d/a/PrinterService` appears only in `/d/a/v1/jobs`; faults, timing, levels and reset
on one do not affect the other; legacy `/v1/*`, `/WebServices/Device`, `/ipp/print` hit the default
device; unknown id -> 404; `Get` metadata of `/d/b/WebServices/Device` advertises `/d/b/...` endpoints and
`ipp://.../d/b/ipp/print`; injection rules are per device; runtime add/remove; duplicate id/uuid and
UUID/IPP-UUID clash rejected; friendly-name prefix rule; derived UUIDs are stable and differ from
fakescan's for the same id; no listener on 9100 or UDP 3702 in embedded mode; non-default JetDirect port
refused when already in use.

Registry unit tests (`registry.rs`, temp file via `FAKEPRINT_REGISTRY`, never the real path): old
single-row file loads and round-trips with unknown fields preserved; atomic write and concurrent writers
lose no rows; capability fingerprint changes with formats/duplex/colour/media/bins/version flags/friendly
name but not with levels, faults, firmware, timing; a `config.fingerprint`-only change never goes stale;
state machine new->probed->installed->stale and healing when the profile is switched back; `installed`
refused for foreign/ambiguous ports; WSD-port and IPP-URL matching, TCPMON and two-UUID ports reported
as foreign; peer-contact throttle; reconcile marks offline reasons separately.

Queue/discovery (`tests/queue.rs` with the existing `DiscoveryCore` + a fake socket and manual clock):
five un-added devices -> exactly one announces; head installed -> settle delay -> next Hello; head
timeout rotates; `skip`/`advance`/`reorder`; installed devices keep announcing; Bye only for announcing
members; Probe is answered only by announcing members; directed discovery answers for a dormant device;
embedded never announces.

Manager/ctl/pipe: ctl commands via the shared dispatcher; loopback guard (non-loopback peer -> 403 on
mutations, per-device control stays open); `open_settings` through an injected launcher (only
`printers`); `daemon::task_command`/`create_args` string tests; named-pipe path format; `shutdown`
requires `confirm`.

`tray_model` (no GUI): level and counts for every state combination; offline and settling do not count
as Add requests; tooltip length; menu tree per printer (checked toggles from `live.faults`, timing radio,
guidance only when attention is needed, queue line `Add now: x (2 of 5)`); `MenuId` <-> `TrayAction`
round trip and rejection of malformed ids; `next_free_id`; open-inbox accepts only a reported path;
`TransitionDetector` (head-only notification, 10 minute rate limit, profile-flap grace, start summary);
`from_json` tolerance of missing/unknown fields; icon pixel buffers; PowerShell quoting of notification
text. `ui.rs`: `/ui` 200 for loopback and 403 otherwise, page contains no `http://`/`https://`
references, `/v1/profiles` shape.

Python (`python/tests`, no daemon, no network): fixtures of `DeviceInfo`/registry parse; matching rules;
`Manager` against an in-process `Server`; `Server(devices=[..])` end to end with `PrintClient` on two
printers; `status_table` and `summary_level` agree with the Rust semantics on shared fixture cases;
`ensure_daemon` with a stubbed spawner; MCP tools registered and attach-only.

## 11. Open questions (need the user's call)

- **Q1** Daemon exposure: is a `--manage` default of `127.0.0.1:3921` with discovery off acceptable
  (LAN needs explicit `listen`/`discovery`), while the bare `fakeprint` command keeps `0.0.0.0` + discovery?
- **Q2** JetDirect: default device only on :9100, others only by explicit `jetdirect_port` (proposed), or
  never more than one RAW listener at all?
- **Q3** Mutations on the device set are loopback-only (proposed); do you need LAN hosts to add/remove
  printers?
- **Q4** What does the tray's "Offline" toggle mean on the wire: `not_accepting_jobs` (printer says no) or
  also stop answering HTTP/Bye (truly unreachable)? Proposed: the former only.
- **Q5** Queue timing defaults: settle 5 s, head timeout 180 s. A printer Add installs a driver; is 180 s
  enough?
- **Q6** Is `friendly_name`/model part of what Windows caches (so a rename needs a re-Add)? Proposed yes
  (in the published fingerprint); needs one live check.
- **Q7** Python home for lab/MCP: port them into `fakeprint-python` (proposed, like fakescan-python) and
  leave `D:\code\fakeprint\python` (old `fakeprint-lab`) untouched, or move it?
- **Q8** `ensure` currently auto-installs the IPP queue when elevated (4.6). OK to remove that behaviour?
- **Q9** Discovery hub (one shared socket) instead of fakescan's per-device announcers: accepted deviation?
- **Q10** Ghost clean-up for printers (fakescan section 13) is out of scope here; schedule it later?
