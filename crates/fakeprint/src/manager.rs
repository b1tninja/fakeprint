//! The device manager: the set of printers one fakeprint process emulates, their summaries and
//! (later) their install state and install queue. See docs/multi-device.md sections 1.5, 2.2, 3.3.
//!
//! The manager holds the devices [`crate::server::serve`] built, their install state (persisted
//! in the registry for a managed daemon, in memory otherwise), the serial install queue and the
//! discovery hub membership. Calls that are still planned return a [`NotYet`] error; detect it
//! with `err.downcast_ref::<NotYet>()` (the HTTP layer maps it to `501 Not Implemented`).
//! Windows is only ever *observed*: nothing here adds, removes or installs a printer.

use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock as StdRwLock, Weak};
use std::time::Duration;

use anyhow::Result;
use axum::Router;
use serde::{Deserialize, Serialize};
use tokio::sync::{oneshot, watch};
use tokio::task::{AbortHandle, JoinHandle};
use tokio::time::Instant;
use tracing::warn;
use uuid::Uuid;

use crate::discovery::{DiscoveryHub, DiscoveryService, DiscoveryStats};
use crate::registry::{
    is_offline_reason, match_printer, now_rfc3339, Added, Candidate, ConfigInfo, Install,
    InstallKind, InstallState, PrinterMatch, ReconcileResult, Registry, RegistryDevice,
    RegistryStore, WindowsPrinter, REASON_NOT_LISTED, REASON_PROFILE_CHANGED,
};
use crate::server::{DeviceConfig, ResolvedDevice, ServerConfig};
use crate::wsd::ctx::{PrintCondition, PrintCtx, PrintTiming};
use crate::wsd::eventing::EventManager;
use crate::wsd::inject::{InjectRule, Injector};
use crate::wsd::validate::ReplyValidator;
use crate::wsd::SharedWorld;

/// A manager feature that is specified but not built yet. `step` is the WP-M step that delivers it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotYet {
    pub what: &'static str,
    pub step: &'static str,
}

impl NotYet {
    pub fn new(what: &'static str, step: &'static str) -> Self {
        Self { what, step }
    }
}

impl fmt::Display for NotYet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "not implemented yet: {} (planned for WP-M step {})",
            self.what, self.step
        )
    }
}

impl std::error::Error for NotYet {}

/// A refused manager request, with the HTTP status it maps to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceError {
    /// 400: malformed input, unknown profile, forbidden operation.
    BadRequest(String),
    /// 404: unknown device.
    NotFound(String),
    /// 409: duplicate id / UUID / port.
    Conflict(String),
}

impl fmt::Display for DeviceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DeviceError::BadRequest(m) | DeviceError::NotFound(m) | DeviceError::Conflict(m) => {
                f.write_str(m)
            }
        }
    }
}

impl std::error::Error for DeviceError {}

impl DeviceError {
    /// HTTP status code (400, 404, 409).
    pub fn status(&self) -> u16 {
        match self {
            DeviceError::BadRequest(_) => 400,
            DeviceError::NotFound(_) => 404,
            DeviceError::Conflict(_) => 409,
        }
    }
}

/// The HTTP status of any error the manager returns: [`DeviceError`] 400/404/409,
/// [`NotYet`] 501, anything else 500.
pub fn error_status(e: &anyhow::Error) -> u16 {
    if let Some(d) = e.downcast_ref::<DeviceError>() {
        d.status()
    } else if e.downcast_ref::<NotYet>().is_some() {
        501
    } else {
        500
    }
}

// ---------------------------------------------------------------------------------------------
// JSON shapes (section 2.2, 3.3)
// ---------------------------------------------------------------------------------------------

/// `{id, name, state}` of a job in [`DeviceLive`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobBrief {
    pub id: u32,
    pub name: String,
    pub state: String,
}

/// `live.faults`: the tray toggles. `offline` is `not_accepting_jobs` (Q4), never "stop answering".
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FaultFlags {
    pub paper_out: bool,
    pub toner_low: bool,
    pub cover_open: bool,
    pub paper_jam: bool,
    pub offline: bool,
}

/// A consumable or input bin with its level.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LevelEntry {
    pub name: String,
    pub level: i32,
}

/// `DeviceInfo.live`: runtime state, never fingerprinted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeviceLive {
    pub printer_state: String,
    pub state_reasons: Vec<String>,
    pub accepting_jobs: bool,
    pub active_jobs: usize,
    pub captured_jobs: usize,
    pub active_job: Option<JobBrief>,
    pub last_job: Option<JobBrief>,
    pub faults: FaultFlags,
    /// `instant` | `realistic` | `default` | `custom`.
    pub timing_preset: String,
    pub consumables: Vec<LevelEntry>,
    pub inputs: Vec<LevelEntry>,
}

/// One row of `GET /v1/devices` (the interface of WP-M, WP-T and WP-P).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub id: String,
    pub default: bool,
    /// WSD device UUID.
    pub uuid: Uuid,
    pub ipp_uuid: Uuid,
    pub profile: String,
    pub friendly_name: String,
    /// `""` for the default device, `/d/<id>` for the others.
    pub base_path: String,
    /// `null` when WSD is off.
    pub device_endpoint: Option<String>,
    pub service_endpoint: Option<String>,
    /// `null` when IPP is off.
    pub ipp_endpoint: Option<String>,
    pub jetdirect_port: Option<u16>,
    /// Absolute inbox path (the tray's "Open inbox" uses exactly this string).
    pub inbox_dir: String,
    /// Multicasting Hello / answering Probe (section 3.3).
    pub announcing: bool,
    /// 1-based place in the install queue; `null` when installed or embedded.
    pub queue_position: Option<usize>,
    pub needs_readd: bool,
    pub needs_readd_reason: Option<String>,
    pub install: Install,
    pub config: ConfigInfo,
    pub live: DeviceLive,
}

/// `GET /v1/queue` (section 3.3). Embedded servers always report an empty queue.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueInfo {
    /// The device currently announced for an Add.
    pub head: Option<String>,
    /// The ids waiting behind the head.
    pub remaining: Vec<String>,
    /// 1-based position of the head (`null` when the queue is empty).
    pub position: Option<usize>,
    /// Head plus remaining.
    pub total: usize,
    /// RFC 3339 time the head became head.
    pub head_since: Option<String>,
    pub head_timeout_s: u64,
    pub settle_s: u64,
}

impl QueueInfo {
    /// The empty queue with the default timers (settle 5 s, head timeout 180 s).
    pub fn empty() -> Self {
        Self {
            head: None,
            remaining: Vec::new(),
            position: None,
            total: 0,
            head_since: None,
            head_timeout_s: DEFAULT_HEAD_TIMEOUT.as_secs(),
            settle_s: DEFAULT_SETTLE.as_secs(),
        }
    }
}

/// Wait after the head is installed before the next device announces (Q5).
pub const DEFAULT_SETTLE: Duration = Duration::from_secs(5);
/// A head that is not installed after this long rotates to the back (Q5).
pub const DEFAULT_HEAD_TIMEOUT: Duration = Duration::from_secs(180);

// ---------------------------------------------------------------------------------------------
// Manager
// ---------------------------------------------------------------------------------------------

/// How the manager is built.
#[derive(Debug, Clone, Default)]
pub struct ManagerConfig {
    /// Registry file (daemon); `None`: in-memory install state (embedded).
    pub registry: Option<std::path::PathBuf>,
    /// Daemon: devices come from the registry rows of this `listen`.
    pub manage: bool,
    /// Embedded: no announcing, no queue, no registry.
    pub embedded: bool,
    pub profile_dir: Option<std::path::PathBuf>,
    pub inbox_dir: std::path::PathBuf,
    /// Profile a `POST /v1/devices` without `profile` gets.
    pub default_profile: Option<String>,
}

#[derive(Default)]
struct QueueState {
    /// Explicit install order (registry `queue_order`, `POST /v1/queue`, rotations).
    order: Vec<String>,
    head: Option<String>,
    head_since: Option<Instant>,
    head_since_ts: Option<String>,
    settle_until: Option<Instant>,
}

/// Opens Windows Settings pages. Tests install a recording launcher; only `"printers"` is ever
/// launchable (`ms-settings:printers`).
pub trait SettingsLauncher: Send + Sync {
    /// Returns whether a window was opened.
    fn open(&self, page: &str) -> bool;
}

/// The launcher used until one is installed: opens nothing.
struct NoLauncher;

impl SettingsLauncher for NoLauncher {
    fn open(&self, _page: &str) -> bool {
        false
    }
}

/// Background tasks of one device (ticker, event pump, JetDirect listener).
pub(crate) struct DeviceTasks {
    pub stop_pump: Mutex<Option<oneshot::Sender<()>>>,
    pub pump: Mutex<Option<JoinHandle<()>>>,
    pub aborts: Mutex<Vec<AbortHandle>>,
}

impl DeviceTasks {
    /// Ask the pump to end (it ends every subscription) and abort ticker and listeners.
    pub fn signal_stop(&self) {
        if let Some(tx) = self
            .stop_pump
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
        {
            let _ = tx.send(());
        }
        for a in self
            .aborts
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .drain(..)
        {
            a.abort();
        }
    }

    /// Wait for the pump to finish its final flush.
    pub async fn join(&self) {
        let h = self.pump.lock().unwrap_or_else(|p| p.into_inner()).take();
        if let Some(h) = h {
            let _ = h.await;
        }
    }
}

/// Process-wide facts every device is built from.
pub(crate) struct Shared {
    pub cfg: ServerConfig,
    pub daemon: crate::server::DaemonOptions,
    pub base_url: String,
    pub adv_ip: IpAddr,
    pub listen_ip: IpAddr,
    pub http_port: u16,
}

/// Everything the manager keeps per device.
#[derive(Clone)]
pub(crate) struct ManagedDevice {
    pub cfg: ResolvedDevice,
    pub wsd: bool,
    pub ipp: bool,
    pub world: SharedWorld,
    pub events: Arc<EventManager>,
    pub validator: Arc<ReplyValidator>,
    pub injector: Arc<Injector>,
    pub router: Router,
    pub install: Install,
    /// Fingerprints and summary as last computed (persisted to the registry row).
    pub config: ConfigInfo,
    pub stats: Arc<DiscoveryStats>,
    pub discovery: Option<Arc<DiscoveryService>>,
    pub tasks: Arc<DeviceTasks>,
}

/// Installed as an `Extension` on a device router so handlers can reach the manager without a
/// reference cycle (section 1.5). `probe` is the hook for peer contact (M2).
#[derive(Clone)]
pub struct DeviceCtx {
    pub manager: Weak<DeviceManager>,
    pub id: String,
    last_probe: Arc<Mutex<Option<std::time::Instant>>>,
}

/// Peer contact is reported at most this often per device.
pub const PROBE_THROTTLE: Duration = Duration::from_secs(30);

impl DeviceCtx {
    pub fn new(manager: Weak<DeviceManager>, id: impl Into<String>) -> Self {
        Self {
            manager,
            id: id.into(),
            last_probe: Arc::new(Mutex::new(None)),
        }
    }

    /// The manager, if it is still alive.
    pub fn manager(&self) -> Option<Arc<DeviceManager>> {
        self.manager.upgrade()
    }

    /// Record peer contact (`new -> probed`, `last_seen_at`), throttled to once per
    /// [`PROBE_THROTTLE`] per device.
    pub async fn probe(&self, peer: IpAddr) {
        {
            let mut last = self.last_probe.lock().unwrap_or_else(|p| p.into_inner());
            if last.is_some_and(|t| t.elapsed() < PROBE_THROTTLE) {
                return;
            }
            *last = Some(std::time::Instant::now());
        }
        if let Some(m) = self.manager() {
            let _ = m.mark_probed(&self.id, &peer.to_string()).await;
        }
    }
}

/// The printers of one process.
pub struct DeviceManager {
    me: Weak<DeviceManager>,
    shared: OnceLock<Arc<Shared>>,
    /// Serializes add / remove / reconfigure.
    change: tokio::sync::Mutex<()>,
    cfg: ManagerConfig,
    store: Option<RegistryStore>,
    hub: Mutex<Option<Arc<DiscoveryHub>>>,
    queue_active: AtomicBool,
    /// The daemon's `--discovery` flag: what a device without its own setting inherits.
    discovery_default: AtomicBool,
    queue: Mutex<QueueState>,
    timers: Mutex<(Duration, Duration)>,
    devices: StdRwLock<Vec<ManagedDevice>>,
    exit: watch::Sender<bool>,
    launcher: Mutex<Arc<dyn SettingsLauncher>>,
}

impl DeviceManager {
    /// A manager with no devices yet; the receiver turns `true` when a client asks the process
    /// to exit (ctl `shutdown`, tray Quit).
    pub fn new(cfg: ManagerConfig) -> (Arc<Self>, watch::Receiver<bool>) {
        let (exit, rx) = watch::channel(false);
        let m = Arc::new_cyclic(|me| Self {
            me: me.clone(),
            shared: OnceLock::new(),
            change: tokio::sync::Mutex::new(()),
            store: cfg.registry.clone().map(RegistryStore::open),
            hub: Mutex::new(None),
            queue_active: AtomicBool::new(false),
            discovery_default: AtomicBool::new(false),
            queue: Mutex::new(QueueState::default()),
            timers: Mutex::new((DEFAULT_SETTLE, DEFAULT_HEAD_TIMEOUT)),
            cfg,
            devices: StdRwLock::new(Vec::new()),
            exit,
            launcher: Mutex::new(Arc::new(NoLauncher)),
        });
        (m, rx)
    }

    /// The configuration the manager was built with.
    pub fn config(&self) -> &ManagerConfig {
        &self.cfg
    }

    pub(crate) fn set_shared(&self, s: Shared) {
        let _ = self.shared.set(Arc::new(s));
    }

    pub(crate) fn shared_arc(&self) -> Result<Arc<Shared>> {
        self.shared()
    }

    fn shared(&self) -> Result<Arc<Shared>> {
        self.shared
            .get()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("the manager is not attached to a running server"))
    }

    /// The (unprefixed) router of a device.
    pub(crate) fn router_for(&self, id: &str) -> Option<Router> {
        self.snapshot()
            .into_iter()
            .find(|d| d.cfg.id == id)
            .map(|d| d.router)
    }

    /// The default device's router (legacy unprefixed aliases).
    pub(crate) fn default_router(&self) -> Option<Router> {
        self.snapshot()
            .into_iter()
            .find(|d| d.cfg.is_default)
            .map(|d| d.router)
    }

    /// The shared world of a device.
    pub fn world_of(&self, id: &str) -> Option<SharedWorld> {
        self.snapshot()
            .into_iter()
            .find(|d| d.cfg.id == id)
            .map(|d| d.world)
    }

    pub(crate) fn default_managed(&self) -> Option<ManagedDevice> {
        self.snapshot().into_iter().find(|d| d.cfg.is_default)
    }

    /// Stop every device's tasks without waiting (used by `Drop`).
    pub(crate) fn stop_devices_sync(&self) {
        for d in self.snapshot() {
            d.tasks.signal_stop();
        }
    }

    /// Stop every device's tasks and wait for the event pumps.
    pub(crate) async fn stop_devices(&self) {
        let all = self.snapshot();
        for d in &all {
            d.tasks.signal_stop();
        }
        for d in &all {
            d.tasks.join().await;
        }
    }

    /// Called by [`crate::server::serve`] for each device it built.
    pub(crate) fn register(&self, d: ManagedDevice) {
        let mut g = self.devices.write().unwrap_or_else(|p| p.into_inner());
        g.retain(|x| x.cfg.id != d.cfg.id);
        g.push(d);
    }

    fn snapshot(&self) -> Vec<ManagedDevice> {
        self.devices
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// Every device with fresh live state, default device first.
    pub async fn list(&self) -> Vec<DeviceInfo> {
        let mut out = Vec::new();
        for d in self.snapshot() {
            out.push(self.info(&d).await);
        }
        out
    }

    /// One device by id.
    pub async fn get(&self, id: &str) -> Option<DeviceHandle> {
        let d = self.snapshot().into_iter().find(|d| d.cfg.id == id)?;
        let info = self.info(&d).await;
        Some(DeviceHandle::from_managed(&d, info))
    }

    /// The default (first) device.
    pub async fn default_device(&self) -> Option<DeviceHandle> {
        let d = self.snapshot().into_iter().find(|d| d.cfg.is_default)?;
        let info = self.info(&d).await;
        Some(DeviceHandle::from_managed(&d, info))
    }

    // ---- device set (M1) ----

    /// Add and start a printer (validated like the initial device set: slug id, unique id and
    /// UUIDs, `fakeprint` name prefix, known profile, free JetDirect port).
    pub async fn add(&self, dc: DeviceConfig) -> Result<DeviceInfo> {
        let shared = self.shared()?;
        let me = self
            .me
            .upgrade()
            .ok_or_else(|| anyhow::anyhow!("the manager is shutting down"))?;
        let _g = self.change.lock().await;
        let existing: Vec<ResolvedDevice> = self.snapshot().into_iter().map(|d| d.cfg).collect();
        let r = shared.cfg.resolve_device(dc, false, false, &existing)?;
        let d = crate::server::build_device(&shared, &me, r).await?;
        self.register(d.clone());
        self.hub_register(&d);
        self.persist(&d.cfg.id);
        self.queue_step();
        Ok(self.info(&d).await)
    }

    /// Stop a printer (its subscriptions end, its JetDirect listener closes) and keep its
    /// registry row. The default device cannot be removed.
    pub async fn remove(&self, id: &str) -> Result<()> {
        self.remove_row(id, false).await
    }

    /// Stop a printer; `drop_row` also deletes its registry row (M2 for the row part).
    pub async fn remove_row(&self, id: &str, drop_row: bool) -> Result<()> {
        let _g = self.change.lock().await;
        let d = {
            let mut g = self.devices.write().unwrap_or_else(|p| p.into_inner());
            let Some(i) = g.iter().position(|d| d.cfg.id == id) else {
                return Err(DeviceError::NotFound(format!("unknown device {id:?}")).into());
            };
            if g[i].cfg.is_default {
                return Err(
                    DeviceError::BadRequest("the default device cannot be removed".into()).into(),
                );
            }
            g.remove(i)
        };
        if let Some(h) = self.hub() {
            h.unregister(d.cfg.device_uuid);
        }
        d.tasks.signal_stop();
        d.tasks.join().await;
        if drop_row {
            if let (Some(store), Ok(shared)) = (&self.store, self.shared()) {
                let listen = shared.cfg.listen;
                let name = id.to_string();
                if let Err(e) = store.update(|reg| {
                    reg.devices
                        .retain(|r| !(r.name == name && row_listens(r, listen)));
                    reg.queue_order.retain(|x| x != &name);
                    Ok(())
                }) {
                    warn!(error = %e, "could not drop the registry row");
                }
            }
        }
        self.queue_step();
        Ok(())
    }

    /// Swap a printer's profile (`<profiles>/<profile_id>.json`). Runtime state is kept; the
    /// published fingerprint in the returned `config` tells whether Windows needs a re-Add.
    pub async fn reconfigure(&self, id: &str, profile_id: &str) -> Result<DeviceInfo> {
        let shared = self.shared()?;
        let _g = self.change.lock().await;
        let d = self
            .snapshot()
            .into_iter()
            .find(|d| d.cfg.id == id)
            .ok_or_else(|| DeviceError::NotFound(format!("unknown device {id:?}")))?;
        let (mut profile, _) = crate::server::load_profile(&shared.cfg, profile_id, false)
            .map_err(|e| DeviceError::BadRequest(format!("{e:#}")))?;
        if let Some(n) = &d.cfg.friendly_name {
            profile.identity.friendly_name = Some(n.clone());
        }
        d.world.write().await.swap_profile(profile);
        let updated = {
            let mut g = self.devices.write().unwrap_or_else(|p| p.into_inner());
            let Some(m) = g.iter_mut().find(|x| x.cfg.id == id) else {
                return Err(DeviceError::NotFound(format!("unknown device {id:?}")).into());
            };
            m.cfg.profile = profile_id.to_string();
            m.clone()
        };
        drop(_g);
        self.refresh_config(id).await?;
        let updated = self
            .snapshot()
            .into_iter()
            .find(|d| d.cfg.id == id)
            .unwrap_or(updated);
        Ok(self.info(&updated).await)
    }

    // ---- install state ----

    fn with_device<R>(&self, id: &str, f: impl FnOnce(&mut ManagedDevice) -> R) -> Result<R> {
        let mut g = self.devices.write().unwrap_or_else(|p| p.into_inner());
        let d = g
            .iter_mut()
            .find(|d| d.cfg.id == id)
            .ok_or_else(|| DeviceError::NotFound(format!("unknown device {id:?}")))?;
        Ok(f(d))
    }

    fn after_change(&self, id: &str) {
        self.persist(id);
        self.queue_step();
    }

    /// Record peer contact: `new -> probed` on the first one, and `last_seen_at` / `last_peer`.
    /// Returns whether the state changed. Callers throttle (see [`DeviceCtx::probe`]).
    pub async fn mark_probed(&self, id: &str, peer: &str) -> Result<bool> {
        let changed = self.with_device(id, |d| {
            let now = now_rfc3339();
            let i = &mut d.install;
            let changed = i.state == InstallState::New;
            if changed {
                i.state = InstallState::Probed;
            }
            if i.first_probed_at.is_none() {
                i.first_probed_at = Some(now.clone());
            }
            i.last_seen_at = Some(now);
            i.last_peer = Some(peer.to_string());
            changed
        })?;
        self.after_change(id);
        Ok(changed)
    }

    /// Peer contact by WSD UUID (WS-Discovery Probe / Resolve answered by the hub).
    pub async fn probe_uuid(&self, uuid: Uuid, peer: IpAddr) {
        let id = self
            .snapshot()
            .into_iter()
            .find(|d| d.cfg.device_uuid == uuid)
            .map(|d| d.cfg.id);
        if let Some(id) = id {
            let _ = self.mark_probed(&id, &peer.to_string()).await;
        }
    }

    fn candidates(&self) -> Vec<Candidate> {
        let authority = self
            .shared()
            .ok()
            .map(|s| {
                s.base_url
                    .strip_prefix("http://")
                    .unwrap_or(&s.base_url)
                    .to_string()
            })
            .unwrap_or_default();
        self.snapshot()
            .iter()
            .map(|d| Candidate {
                id: d.cfg.id.clone(),
                device_uuid: d.cfg.device_uuid,
                authority: authority.clone(),
                ipp_path: format!("{}/ipp/print", d.cfg.base_path),
                device_path: format!("{}/WebServices/Device", d.cfg.base_path),
            })
            .collect()
    }

    fn apply_installed(
        d: &mut ManagedDevice,
        kind: Option<InstallKind>,
        win: Option<WindowsPrinter>,
    ) {
        let now = now_rfc3339();
        let i = &mut d.install;
        if i.state != InstallState::Installed || i.installed_at.is_none() {
            i.installed_at = Some(now.clone());
        }
        i.state = InstallState::Installed;
        i.reason = None;
        i.stale_from = None;
        i.stale_at = None;
        i.kind = kind;
        if let Some(w) = win {
            i.windows = w;
        }
        i.verified_at = Some(now);
        i.published_fingerprint_at_install = Some(d.config.published_fingerprint.clone());
    }

    /// Windows reported a printer for this device. Refused unless its port carries exactly this
    /// device's WSD UUID or is exactly its IPP URL (section 3.4).
    pub async fn mark_installed(&self, id: &str, win: WindowsPrinter) -> Result<()> {
        let all = self.candidates();
        let me: Vec<Candidate> = all.into_iter().filter(|c| c.id == id).collect();
        if me.is_empty() {
            return Err(DeviceError::NotFound(format!("unknown device {id:?}")).into());
        }
        let kind = match match_printer(&win, &me) {
            PrinterMatch::Wsd(_) => InstallKind::Wsd,
            PrinterMatch::Ipp(_) => InstallKind::Ipp,
            PrinterMatch::Foreign(why) => {
                return Err(DeviceError::BadRequest(format!(
                    "refused: the Windows printer {:?} (port {:?}) is not this device: {why}",
                    win.name, win.port
                ))
                .into())
            }
        };
        self.with_device(id, |d| Self::apply_installed(d, Some(kind), Some(win)))?;
        self.after_change(id);
        Ok(())
    }

    /// Legacy `added.pnp = true`: installed without Windows details.
    pub async fn mark_installed_legacy(&self, id: &str) -> Result<()> {
        self.with_device(id, |d| Self::apply_installed(d, None, None))?;
        self.after_change(id);
        Ok(())
    }

    fn set_stale(d: &mut ManagedDevice, reason: &str) {
        let i = &mut d.install;
        if i.state != InstallState::Stale {
            i.stale_from = Some(
                match i.state {
                    InstallState::New => "new",
                    InstallState::Probed => "probed",
                    InstallState::Installed => "installed",
                    InstallState::Stale => "stale",
                }
                .to_string(),
            );
            i.stale_at = Some(now_rfc3339());
        }
        i.state = InstallState::Stale;
        i.reason = Some(reason.to_string());
    }

    /// `installed -> stale` with a reason (`not listed by Windows` is offline, anything else
    /// asks for a re-Add).
    pub async fn mark_stale(&self, id: &str, reason: &str) -> Result<()> {
        self.with_device(id, |d| Self::set_stale(d, reason))?;
        self.after_change(id);
        Ok(())
    }

    /// Drop install info, keep the row.
    pub async fn forget(&self, id: &str) -> Result<()> {
        self.with_device(id, |d| d.install = Install::default())?;
        self.after_change(id);
        Ok(())
    }

    /// Recompute the fingerprints after a profile or name change and apply the staleness rules:
    /// `installed` whose published fingerprint left `published_fingerprint_at_install` becomes
    /// `stale` (`profile changed`); such a stale device whose fingerprint is back heals to
    /// `installed`. Levels, faults and timing never matter.
    pub async fn refresh_config(&self, id: &str) -> Result<()> {
        let d = self
            .snapshot()
            .into_iter()
            .find(|d| d.cfg.id == id)
            .ok_or_else(|| DeviceError::NotFound(format!("unknown device {id:?}")))?;
        let config = current_config(&d).await;
        self.with_device(id, |d| {
            d.config = config.clone();
            let fp = &config.published_fingerprint;
            let at = d.install.published_fingerprint_at_install.clone();
            match d.install.state {
                InstallState::Installed => match at {
                    Some(at) if &at != fp => Self::set_stale(
                        d,
                        &format!("{REASON_PROFILE_CHANGED}: published capabilities differ"),
                    ),
                    None => d.install.published_fingerprint_at_install = Some(fp.clone()),
                    _ => {}
                },
                InstallState::Stale
                    if d.install
                        .reason
                        .as_deref()
                        .is_some_and(|r| r.starts_with(REASON_PROFILE_CHANGED))
                        && at.as_ref() == Some(fp) =>
                {
                    d.install.state = InstallState::Installed;
                    d.install.reason = None;
                    d.install.stale_from = None;
                    d.install.stale_at = None;
                }
                _ => {}
            }
        })?;
        self.after_change(id);
        Ok(())
    }

    /// Match a Windows printer listing against the devices (by UUID or exact IPP URL, never by
    /// name). Matching devices become `installed` (unless a re-Add is pending); installed devices
    /// Windows no longer lists become `stale` (`not listed by Windows`, offline); everything else
    /// is reported as foreign and never touched.
    pub fn reconcile_with_windows(&self, listing: &[WindowsPrinter]) -> Result<ReconcileResult> {
        let cands = self.candidates();
        let mut out = ReconcileResult::default();
        let mut matched: Vec<(String, InstallKind, WindowsPrinter)> = Vec::new();
        for p in listing {
            match match_printer(p, &cands) {
                PrinterMatch::Wsd(id) => matched.push((id, InstallKind::Wsd, p.clone())),
                PrinterMatch::Ipp(id) => matched.push((id, InstallKind::Ipp, p.clone())),
                PrinterMatch::Foreign(_) => out.foreign.push(p.clone()),
            }
        }
        let ids: Vec<String> = self.snapshot().iter().map(|d| d.cfg.id.clone()).collect();
        for id in ids {
            let m = matched.iter().find(|(m, _, _)| *m == id).cloned();
            let res = self.with_device(&id, |d| {
                let readd = d.install.needs_readd();
                match m {
                    Some((_, kind, win)) => {
                        if readd {
                            return None;
                        }
                        Self::apply_installed(d, Some(kind), Some(win));
                        Some(true)
                    }
                    None if d.install.state == InstallState::Installed => {
                        Self::set_stale(d, REASON_NOT_LISTED);
                        Some(false)
                    }
                    None => None,
                }
            })?;
            match res {
                Some(true) => out.installed.push(id.clone()),
                Some(false) => out.stale.push(id.clone()),
                None => {}
            }
            self.persist(&id);
        }
        self.queue_step();
        Ok(out)
    }

    /// The registry document: the file for a managed daemon, otherwise rows synthesized from the
    /// running devices.
    pub async fn registry(&self) -> Registry {
        if let Some(store) = &self.store {
            if let Ok(r) = store.load() {
                return r;
            }
        }
        let listen = self
            .shared()
            .map(|s| s.cfg.listen.to_string())
            .unwrap_or_default();
        let mut reg = Registry::default();
        for d in self.snapshot() {
            let mut row = RegistryDevice::default();
            fill_row(&mut row, &d, &listen);
            reg.devices.push(row);
        }
        reg.queue_order = self
            .queue
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .order
            .clone();
        reg
    }

    fn persist(&self, id: &str) {
        let Some(store) = &self.store else { return };
        let Some(d) = self.snapshot().into_iter().find(|d| d.cfg.id == id) else {
            return;
        };
        let Ok(shared) = self.shared() else { return };
        let listen = shared.cfg.listen;
        if let Err(e) = store.update(|reg| {
            let i = match reg
                .devices
                .iter()
                .position(|r| r.name == d.cfg.id && row_listens(r, listen))
            {
                Some(i) => i,
                None => {
                    reg.devices.push(RegistryDevice::default());
                    reg.devices.len() - 1
                }
            };
            fill_row(&mut reg.devices[i], &d, &listen.to_string());
            // rows of the same device under another address of this port collapse into this one
            let mut k = 0;
            reg.devices.retain(|r| {
                let keep = k == i || !(r.name == d.cfg.id && row_listens(r, listen));
                k += 1;
                keep
            });
            Ok(())
        }) {
            warn!(error = %e, device = %id, "could not write the registry");
        }
    }

    /// Install state loaded from a registry row at start-up (legacy `added.pnp` counts as
    /// installed); the staleness rules are applied by [`DeviceManager::refresh_config`].
    pub(crate) fn set_install(&self, id: &str, mut install: Install, added: &Added) {
        let _ = self.with_device(id, |d| {
            if install.state == InstallState::New && added.pnp {
                install.state = InstallState::Installed;
            }
            d.install = install;
        });
    }

    pub(crate) fn set_queue_order(&self, order: Vec<String>) {
        self.queue.lock().unwrap_or_else(|p| p.into_inner()).order = order;
    }

    // ---- discovery hub and install queue ----

    fn hub(&self) -> Option<Arc<DiscoveryHub>> {
        self.hub.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    fn hub_register(&self, d: &ManagedDevice) {
        if let (Some(hub), Some(svc)) = (self.hub(), &d.discovery) {
            hub.register(
                d.cfg.device_uuid,
                Arc::clone(svc),
                Arc::clone(&d.stats),
                false,
            );
        }
    }

    /// Attach the discovery hub: every device becomes a (silent) member and the install queue
    /// becomes active. The daemon calls this with the real multicast hub; tests with a fake socket.
    pub fn attach_hub(&self, hub: Arc<DiscoveryHub>) {
        let weak = self.me.clone();
        hub.on_peer(Arc::new(move |uuid, ip| {
            if let Some(m) = weak.upgrade() {
                if let Ok(rt) = tokio::runtime::Handle::try_current() {
                    rt.spawn(async move { m.probe_uuid(uuid, ip).await });
                }
            }
        }));
        *self.hub.lock().unwrap_or_else(|p| p.into_inner()) = Some(hub);
        for d in self.snapshot() {
            self.hub_register(&d);
        }
        self.queue_active.store(true, Ordering::SeqCst);
        self.queue_step();
    }

    /// Set what devices without a per-device `discovery` setting inherit (the daemon's
    /// `--discovery` flag; `serve` sets it, tests with a fake hub set it by hand).
    pub fn set_discovery_default(&self, on: bool) {
        self.discovery_default.store(on, Ordering::SeqCst);
        self.queue_step();
    }

    /// Turn the install queue on without a hub (a daemon with discovery off still has an
    /// "Add now" order for the tray and the UI).
    pub fn enable_queue(&self) {
        self.queue_active.store(true, Ordering::SeqCst);
        self.queue_step();
    }

    /// Settle and head-timeout timers (defaults 5 s and 180 s).
    pub fn set_queue_timers(&self, settle: Duration, head_timeout: Duration) {
        *self.timers.lock().unwrap_or_else(|p| p.into_inner()) = (settle, head_timeout);
    }

    fn is_waiting(d: &ManagedDevice) -> bool {
        match d.install.state {
            InstallState::New | InstallState::Probed => true,
            InstallState::Stale => !d.install.reason.as_deref().is_some_and(is_offline_reason),
            InstallState::Installed => false,
        }
    }

    fn waiting_in(devs: &[ManagedDevice], order: &[String]) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for id in order {
            if devs.iter().any(|d| &d.cfg.id == id && Self::is_waiting(d)) && !out.contains(id) {
                out.push(id.clone());
            }
        }
        for d in devs {
            if Self::is_waiting(d) && !out.contains(&d.cfg.id) {
                out.push(d.cfg.id.clone());
            }
        }
        out
    }

    /// Advance the queue state machine (promote a head, settle after an install, rotate on
    /// timeout) and apply the announce rule to the hub members. Cheap; called after every state
    /// change and once a second by the daemon.
    pub fn queue_step(&self) {
        if !self.queue_active.load(Ordering::SeqCst) {
            return;
        }
        let devs = self.snapshot();
        let (settle, timeout) = *self.timers.lock().unwrap_or_else(|p| p.into_inner());
        let head = {
            let mut q = self.queue.lock().unwrap_or_else(|p| p.into_inner());
            let waiting = Self::waiting_in(&devs, &q.order);
            let now = Instant::now();
            if let Some(h) = q.head.clone() {
                if !waiting.contains(&h) {
                    match q.settle_until {
                        None => q.settle_until = Some(now + settle),
                        Some(t) if now >= t => {
                            q.head = None;
                            q.head_since = None;
                            q.head_since_ts = None;
                            q.settle_until = None;
                        }
                        _ => {}
                    }
                } else {
                    q.settle_until = None;
                    if q.head_since
                        .is_some_and(|t| now.duration_since(t) >= timeout)
                    {
                        let mut order: Vec<String> =
                            waiting.iter().filter(|x| **x != h).cloned().collect();
                        order.push(h);
                        q.order = order;
                        q.head = None;
                        q.head_since = None;
                    }
                }
            }
            if q.head.is_none() && q.settle_until.is_none() {
                let waiting = Self::waiting_in(&devs, &q.order);
                if let Some(first) = waiting.first() {
                    q.head = Some(first.clone());
                    q.head_since = Some(now);
                    q.head_since_ts = Some(now_rfc3339());
                }
            }
            q.head.clone()
        };
        self.apply_announce(&devs, head.as_deref());
    }

    fn apply_announce(&self, devs: &[ManagedDevice], head: Option<&str>) {
        let Some(hub) = self.hub() else { return };
        let global = self.discovery_default.load(Ordering::SeqCst);
        for d in devs {
            if d.discovery.is_none() {
                continue;
            }
            let enabled = d.cfg.discovery.unwrap_or(global);
            let installed_like = d.install.state == InstallState::Installed
                || (d.install.state == InstallState::Stale
                    && d.install.reason.as_deref().is_some_and(is_offline_reason));
            let want = enabled && (installed_like || head == Some(d.cfg.id.as_str()));
            hub.set_announcing(d.cfg.device_uuid, want);
        }
    }

    /// Ids waiting for an Add, head first.
    fn waiting_ids(&self) -> Vec<String> {
        let devs = self.snapshot();
        let q = self.queue.lock().unwrap_or_else(|p| p.into_inner());
        let mut w = Self::waiting_in(&devs, &q.order);
        if let Some(h) = &q.head {
            if let Some(i) = w.iter().position(|x| x == h) {
                let h = w.remove(i);
                w.insert(0, h);
            }
        }
        w
    }

    /// The install queue (section 3.3). Embedded servers report the empty queue.
    pub async fn queue(&self) -> QueueInfo {
        if !self.queue_active.load(Ordering::SeqCst) {
            return QueueInfo::empty();
        }
        self.queue_step();
        let total = self.snapshot().len();
        let waiting = self.waiting_ids();
        let (settle, timeout) = *self.timers.lock().unwrap_or_else(|p| p.into_inner());
        let q = self.queue.lock().unwrap_or_else(|p| p.into_inner());
        let head = q.head.clone().filter(|h| waiting.contains(h));
        let remaining: Vec<String> = waiting
            .iter()
            .filter(|x| Some(*x) != head.as_ref())
            .cloned()
            .collect();
        QueueInfo {
            position: head.as_ref().map(|_| total - waiting.len() + 1),
            head_since: head.as_ref().and(q.head_since_ts.clone()),
            head,
            remaining,
            total,
            head_timeout_s: timeout.as_secs(),
            settle_s: settle.as_secs(),
        }
    }

    fn rotate_head(&self) {
        {
            let devs = self.snapshot();
            let mut q = self.queue.lock().unwrap_or_else(|p| p.into_inner());
            let waiting = Self::waiting_in(&devs, &q.order);
            if let Some(h) = q.head.clone().filter(|h| waiting.contains(h)) {
                let mut order: Vec<String> = waiting.iter().filter(|x| **x != h).cloned().collect();
                order.push(h);
                q.order = order;
                q.head = None;
                q.head_since = None;
                q.settle_until = None;
            }
        }
        self.queue_step();
    }

    /// Move the head on by hand: it goes to the back and the next device is announced.
    pub async fn queue_advance(&self) -> QueueInfo {
        self.rotate_head();
        self.queue().await
    }

    /// Skip the head (same as [`DeviceManager::queue_advance`]).
    pub async fn queue_skip(&self) -> QueueInfo {
        self.rotate_head();
        self.queue().await
    }

    /// Set the explicit install order; the first waiting device becomes the head.
    pub async fn queue_reorder(&self, ids: &[String]) -> Result<QueueInfo> {
        let devs = self.snapshot();
        for id in ids {
            if !devs.iter().any(|d| &d.cfg.id == id) {
                return Err(DeviceError::BadRequest(format!("unknown device {id:?}")).into());
            }
        }
        {
            let mut q = self.queue.lock().unwrap_or_else(|p| p.into_inner());
            q.order = ids.to_vec();
            q.head = None;
            q.head_since = None;
            q.settle_until = None;
        }
        if let (Some(store), true) = (&self.store, !ids.is_empty()) {
            let order = ids.to_vec();
            if let Err(e) = store.update(|reg| {
                reg.queue_order = order;
                Ok(())
            }) {
                warn!(error = %e, "could not write the registry");
            }
        }
        self.queue_step();
        Ok(self.queue().await)
    }

    /// The device summary with the manager-level fields (`announcing`, `queue_position`).
    async fn info(&self, d: &ManagedDevice) -> DeviceInfo {
        let mut i = device_info(d).await;
        if let Some(h) = self.hub() {
            i.announcing = h.is_announcing(d.cfg.device_uuid);
        }
        if self.queue_active.load(Ordering::SeqCst) {
            i.queue_position = self
                .waiting_ids()
                .iter()
                .position(|x| x == &d.cfg.id)
                .map(|p| p + 1);
        }
        i
    }

    // ---- process ----

    /// Install the launcher `open_settings` uses (tests never open windows).
    pub fn set_settings_launcher(&self, l: Arc<dyn SettingsLauncher>) {
        *self.launcher.lock().unwrap_or_else(|p| p.into_inner()) = l;
    }

    /// Open a Settings page through the installed launcher; only `"printers"` is allowed.
    pub fn open_settings(&self, page: &str) -> bool {
        if page != "printers" {
            return false;
        }
        let l = self
            .launcher
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        l.open(page)
    }

    /// Ask the process to exit (the receiver from [`DeviceManager::new`] turns `true`).
    pub fn request_exit(&self) {
        let _ = self.exit.send(true);
    }

    /// Ask the process to exit, say Bye for the announcing devices and stop every device
    /// (their subscriptions end).
    pub async fn shutdown(&self) {
        self.request_exit();
        let hub = self.hub.lock().unwrap_or_else(|p| p.into_inner()).take();
        if let Some(h) = hub {
            h.shutdown().await;
        }
        self.stop_devices().await;
    }
}

// ---------------------------------------------------------------------------------------------
// Device handle
// ---------------------------------------------------------------------------------------------

/// A cheap, cloneable view of one device. `info` is a snapshot taken when the handle was made;
/// call [`DeviceManager::get`] again for fresh live state.
#[derive(Clone)]
pub struct DeviceHandle {
    pub info: DeviceInfo,
    pub world: SharedWorld,
    events: Arc<EventManager>,
    validator: Arc<ReplyValidator>,
    injector: Arc<Injector>,
    router: Router,
}

impl DeviceHandle {
    pub(crate) fn from_managed(d: &ManagedDevice, info: DeviceInfo) -> Self {
        Self {
            info,
            world: Arc::clone(&d.world),
            events: Arc::clone(&d.events),
            validator: Arc::clone(&d.validator),
            injector: Arc::clone(&d.injector),
            router: d.router.clone(),
        }
    }

    /// The device's print context (jobs, engine, model, clock).
    pub async fn ctx(&self) -> Arc<PrintCtx> {
        self.world.read().await.print_ctx()
    }

    pub fn events(&self) -> Arc<EventManager> {
        Arc::clone(&self.events)
    }

    pub fn injector(&self) -> Arc<Injector> {
        Arc::clone(&self.injector)
    }

    pub fn validator(&self) -> Arc<ReplyValidator> {
        Arc::clone(&self.validator)
    }

    /// Arm a request-injection rule on this device.
    pub fn inject(&self, rule: InjectRule) -> Result<(), String> {
        self.injector.add(rule)
    }

    pub fn inbox_dir(&self) -> &std::path::Path {
        std::path::Path::new(&self.info.inbox_dir)
    }

    pub fn device_endpoint(&self) -> Option<&str> {
        self.info.device_endpoint.as_deref()
    }

    pub fn service_endpoint(&self) -> Option<&str> {
        self.info.service_endpoint.as_deref()
    }

    pub fn ipp_endpoint(&self) -> Option<&str> {
        self.info.ipp_endpoint.as_deref()
    }

    /// This device's own routes (unprefixed, the dispatcher strips `/d/<id>`).
    pub fn router(&self) -> Router {
        self.router.clone()
    }
}

// ---------------------------------------------------------------------------------------------
// Building DeviceInfo
// ---------------------------------------------------------------------------------------------

/// A row belongs to this server when it names the same **port**. One daemon owns a port (the
/// instance lock is per port), and `127.0.0.1:3921` vs `0.0.0.0:3921` is a deployment choice, not
/// a different set of printers: matching the exact address hid rows after a restart with another
/// `--listen`.
fn row_listens(r: &RegistryDevice, listen: SocketAddr) -> bool {
    r.listen
        .parse::<SocketAddr>()
        .map(|a| a.port() == listen.port())
        .unwrap_or(r.listen.is_empty())
}

fn fill_row(row: &mut RegistryDevice, d: &ManagedDevice, listen: &str) {
    let derived_ipp = crate::lab::derive_ipp_uuid(d.cfg.device_uuid);
    row.name = d.cfg.id.clone();
    row.listen = listen.to_string();
    row.device_uuid = Some(d.cfg.device_uuid);
    row.ipp_uuid = (d.cfg.ipp_uuid != derived_ipp).then_some(d.cfg.ipp_uuid);
    row.profile = d.cfg.profile.clone();
    row.friendly_name = d.cfg.friendly_name.clone();
    // only an explicit per-device choice is stored; the inherited value never is
    row.discovery = d.cfg.discovery;
    row.discovery_explicit = d.cfg.discovery.is_some();
    if !d.cfg.is_default {
        row.jetdirect_port = d.cfg.jetdirect_port;
    }
    row.added.pnp = d.install.state == InstallState::Installed;
    row.config = d.config.clone();
    row.config.class = None;
    row.install = d.install.clone();
}

async fn current_config(d: &ManagedDevice) -> ConfigInfo {
    let profile = d.world.read().await.live_profile();
    let friendly = d
        .cfg
        .friendly_name
        .clone()
        .or_else(|| profile.identity.friendly_name.clone())
        .unwrap_or_else(|| profile.label.clone());
    ConfigInfo::for_profile(&profile, &friendly)
}

/// Opens `ms-settings:printers` through `cmd /C start` (Windows). The only Windows action
/// fakeprint takes; it shows the page where the human adds the printer.
#[cfg(windows)]
pub struct WindowsSettingsLauncher;

#[cfg(windows)]
impl SettingsLauncher for WindowsSettingsLauncher {
    fn open(&self, page: &str) -> bool {
        use std::os::windows::process::CommandExt;
        if page != "printers" {
            return false;
        }
        std::process::Command::new("cmd")
            .args(["/C", "start", "", "ms-settings:printers"])
            .creation_flags(0x0800_0000)
            .spawn()
            .is_ok()
    }
}

/// What [`world_device_info`] needs to know besides the world.
#[derive(Debug, Clone)]
pub struct DeviceMeta {
    pub id: String,
    pub is_default: bool,
    pub wsd: bool,
    pub ipp: bool,
    pub jetdirect_port: Option<u16>,
    /// Friendly-name override (`None`: the profile's).
    pub friendly_name: Option<String>,
}

impl DeviceMeta {
    /// The default device with WSD and IPP on (library mounts without a manager).
    pub fn default_device() -> Self {
        Self {
            id: crate::server::DEFAULT_DEVICE_ID.to_string(),
            is_default: true,
            wsd: true,
            ipp: true,
            jetdirect_port: None,
            friendly_name: None,
        }
    }
}

async fn device_info(d: &ManagedDevice) -> DeviceInfo {
    let meta = DeviceMeta {
        id: d.cfg.id.clone(),
        is_default: d.cfg.is_default,
        wsd: d.wsd,
        ipp: d.ipp,
        jetdirect_port: d.cfg.jetdirect_port,
        friendly_name: d.cfg.friendly_name.clone(),
    };
    world_device_info(&d.world, &meta, &d.install).await
}

fn timing_preset(t: &PrintTiming) -> &'static str {
    if *t == PrintTiming::instant() {
        "instant"
    } else if *t == PrintTiming::realistic() {
        "realistic"
    } else if *t == PrintTiming::default() {
        "default"
    } else {
        "custom"
    }
}

fn brief(j: &crate::wsd::types::PrintJob) -> JobBrief {
    JobBrief {
        id: j.id,
        name: j.name.clone(),
        state: j.state.token().to_string(),
    }
}

/// Build the `DeviceInfo` of a running world. Used by the manager and by the `GET /v1/devices`
/// fallback of a router mounted without a manager.
pub async fn world_device_info(
    world: &SharedWorld,
    meta: &DeviceMeta,
    install: &Install,
) -> DeviceInfo {
    let (profile, device_ep, service_ep, uuid, ipp_uuid, prefix, ipp_uri, inbox, captured, ctx) = {
        let w = world.read().await;
        let prefix = w.device_prefix();
        (
            w.live_profile(),
            w.device_endpoint().to_string(),
            w.service_endpoint().to_string(),
            w.device_uuid(),
            w.ipp_uuid(),
            prefix.clone(),
            w.ipp_uri(&prefix),
            w.inbox_dir().display().to_string(),
            w.captured().len(),
            w.print_ctx(),
        )
    };
    let friendly = meta
        .friendly_name
        .clone()
        .or_else(|| profile.identity.friendly_name.clone())
        .unwrap_or_else(|| profile.label.clone());

    let st = ctx.status();
    let plan = ctx.engine.faults();
    let has = |c: PrintCondition| st.active_conditions.iter().any(|a| a.id == c.id());
    let mut active = ctx.jobs.active();
    active.sort_by_key(|j| j.id);
    let mut done = ctx.jobs.history();
    done.sort_by_key(|j| j.id);
    let levels_c: Vec<LevelEntry> = profile
        .consumables
        .iter()
        .flatten()
        .map(|c| LevelEntry {
            name: c.name.clone(),
            level: c.level,
        })
        .collect();
    let levels_i: Vec<LevelEntry> = profile
        .inputs
        .iter()
        .flatten()
        .map(|b| LevelEntry {
            name: b.name.clone(),
            level: b.level,
        })
        .collect();
    let live = DeviceLive {
        printer_state: st.state.token().to_string(),
        state_reasons: st.reasons.clone(),
        accepting_jobs: st.accepting_jobs,
        active_jobs: ctx.engine.queued_jobs(),
        captured_jobs: captured,
        active_job: active.first().map(brief),
        last_job: done.last().map(brief),
        faults: FaultFlags {
            paper_out: has(PrintCondition::PaperOut),
            toner_low: has(PrintCondition::TonerLow),
            cover_open: has(PrintCondition::CoverOpen),
            paper_jam: has(PrintCondition::PaperJam),
            offline: plan.not_accepting_jobs,
        },
        timing_preset: timing_preset(&ctx.timing()).to_string(),
        consumables: levels_c,
        inputs: levels_i,
    };
    DeviceInfo {
        id: meta.id.clone(),
        default: meta.is_default,
        uuid,
        ipp_uuid,
        profile: profile.id.clone(),
        config: ConfigInfo::for_profile(&profile, &friendly),
        friendly_name: friendly,
        base_path: prefix,
        device_endpoint: meta.wsd.then_some(device_ep),
        service_endpoint: meta.wsd.then_some(service_ep),
        ipp_endpoint: meta.ipp.then_some(ipp_uri),
        jetdirect_port: meta.jetdirect_port,
        inbox_dir: inbox,
        announcing: false,
        queue_position: None,
        needs_readd: install.needs_readd(),
        needs_readd_reason: install
            .needs_readd()
            .then(|| install.reason.clone())
            .flatten(),
        install: install.clone(),
        live,
    }
}

/// The device configs of a managed daemon: the registry rows whose `listen` equals the server's
/// (registry order; the first is the default device), or, on a first run with none, the one
/// legacy `default` device from the server's own fields. Also returns the rows by device id.
/// Rows with an invalid id are skipped with a warning.
pub fn devices_from_registry(
    cfg: &ServerConfig,
    reg: &Registry,
) -> (Vec<DeviceConfig>, Vec<(String, RegistryDevice)>) {
    let mut devices = Vec::new();
    let mut rows = Vec::new();
    let mut chosen: Vec<&RegistryDevice> = Vec::new();
    for row in reg.devices.iter().filter(|r| row_listens(r, cfg.listen)) {
        // the same device listed under several addresses of this port: keep the row with the
        // exact listen address, else the most recently seen one
        let exact = |r: &RegistryDevice| r.listen.parse::<SocketAddr>().ok() == Some(cfg.listen);
        match chosen.iter().position(|c| c.name == row.name) {
            None => chosen.push(row),
            Some(i) => {
                let better = if exact(row) != exact(chosen[i]) {
                    exact(row)
                } else {
                    row.install.last_seen_at > chosen[i].install.last_seen_at
                };
                if better {
                    chosen[i] = row;
                }
            }
        }
    }
    for row in chosen {
        if !crate::server::valid_device_id(&row.name) {
            warn!(name = %row.name, "skipping a registry row with an invalid device id");
            continue;
        }
        let profile = if row.profile.is_empty() {
            cfg.profile_name.clone()
        } else {
            row.profile.clone()
        };
        let uuid = row
            .device_uuid
            .unwrap_or_else(|| crate::lab::derive_device_uuid(&row.name));
        let mut dc = DeviceConfig::new(row.name.clone(), profile, uuid);
        dc.ipp_uuid = row.ipp_uuid;
        dc.friendly_name = row.friendly_name.clone().filter(|n| !n.is_empty());
        dc.jetdirect_port = row.jetdirect_port;
        // an old row's bare `false` was an inherited value (see `RegistryDevice::discovery`)
        dc.discovery = match row.discovery {
            Some(v) if row.discovery_explicit || v => Some(v),
            _ => None,
        };
        devices.push(dc);
        rows.push((row.name.clone(), row.clone()));
    }
    if devices.is_empty() {
        let mut dc = DeviceConfig::new(
            crate::server::DEFAULT_DEVICE_ID,
            cfg.profile_name.clone(),
            cfg.device_uuid,
        );
        dc.ipp_uuid = cfg.ipp_uuid;
        devices.push(dc);
    }
    (devices, rows)
}
