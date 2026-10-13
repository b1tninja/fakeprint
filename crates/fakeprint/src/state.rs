//! Shared process state: identity, the print context (jobs, timers, model) and the inbox.
//!
//! Job bookkeeping lives in [`crate::wsd::jobs::JobEngine`]; this module keeps the
//! lab-facing `World` (identity, profile, counters, the print context and the event manager),
//! the document [`Inbox`] and the small compatibility surface the IPP and JetDirect listeners
//! use (`create_job`, `capture_document`, `end_job`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde::Serialize;
use uuid::Uuid;

use crate::discovery::DiscoveryStats;
use crate::profile::PrinterProfile;
use crate::wsd::ctx::{
    Clock, PrintConfig, PrintCtx, PrintLimits, PrintTiming, SharedBus, StatusSource, SystemClock,
};
use crate::wsd::eventing::{AddressPolicy, EventManager, EventingConfig};
use crate::wsd::jobs::{CancelSource, DocumentIn, JobEngine};
use crate::wsd::metadata::{DeviceInfo, PrintVersions};
use crate::wsd::printer_model::{
    published_fingerprint, service_quirks, validator_options, version_flags, ProfilePrinterModel,
};
use crate::wsd::service::WsdService;
use crate::wsd::types::{wprt, EventBus, NewJob, PrintEvent, PrinterModel};
use crate::wsd::validate::ValidationStats;

#[derive(Debug, Clone, Serialize)]
pub struct CapturedJob {
    pub job_id: u32,
    /// 1-based document number inside the job.
    pub document_id: u32,
    pub path: String,
    pub meta_path: String,
    pub bytes: usize,
    pub document_format: String,
    pub peer: String,
}

/// Largest document the inbox writes (bytes).
pub const INBOX_MAX_DOCUMENT: usize = 256 << 20;
/// Captures remembered in memory.
const INBOX_MAX_LISTED: usize = 1000;

/// The capture directory. File names are built from numbers only (timestamp, job id,
/// document number, collision counter), never from client text, so nothing a client sends can
/// choose or traverse a path; files are created with `create_new`, so an existing capture is
/// never overwritten.
#[derive(Debug)]
pub struct Inbox {
    dir: PathBuf,
    max_document: usize,
    captured: Mutex<Vec<CapturedJob>>,
}

impl Inbox {
    pub fn new(dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&dir).with_context(|| format!("create inbox {}", dir.display()))?;
        Ok(Self {
            dir,
            max_document: INBOX_MAX_DOCUMENT,
            captured: Mutex::new(Vec::new()),
        })
    }

    pub fn with_max_document(mut self, max: usize) -> Self {
        self.max_document = max;
        self
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn list(&self) -> std::sync::MutexGuard<'_, Vec<CapturedJob>> {
        match self.captured.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        }
    }

    pub fn captured(&self) -> Vec<CapturedJob> {
        self.list().clone()
    }

    pub fn clear(&self) {
        self.list().clear();
    }

    /// Forget the captures of a job that was cancelled before it printed: they leave the
    /// captured list and their files (`.bin` and `.json`) are removed. Returns how many.
    pub fn discard_job(&self, job_id: u32) -> usize {
        let gone: Vec<CapturedJob> = {
            let mut list = self.list();
            let (gone, keep): (Vec<_>, Vec<_>) = list.drain(..).partition(|c| c.job_id == job_id);
            *list = keep;
            gone
        };
        for c in &gone {
            let _ = std::fs::remove_file(&c.path);
            let _ = std::fs::remove_file(&c.meta_path);
        }
        gone.len()
    }

    /// Write `payload` as `<ts>-<job>-<doc>[-<n>].bin` plus a `.json` sidecar.
    pub fn capture(
        &self,
        job_id: u32,
        document_id: u32,
        payload: &[u8],
        document_format: &str,
        peer: &str,
    ) -> Result<CapturedJob> {
        self.capture_with(
            job_id,
            document_id,
            payload,
            document_format,
            peer,
            &serde_json::Value::Null,
        )
    }

    /// [`Inbox::capture`] with `extra` (a JSON object) merged into the sidecar. Existing
    /// sidecar keys are never replaced by it.
    pub fn capture_with(
        &self,
        job_id: u32,
        document_id: u32,
        payload: &[u8],
        document_format: &str,
        peer: &str,
        extra: &serde_json::Value,
    ) -> Result<CapturedJob> {
        use std::io::Write;
        if payload.len() > self.max_document {
            bail!(
                "document of {} bytes exceeds the inbox limit {}",
                payload.len(),
                self.max_document
            );
        }
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let mut n = 0u32;
        let (bin_path, meta_path, mut bin_file) = loop {
            let suffix = if n == 0 {
                String::new()
            } else {
                format!("-{n}")
            };
            let stem = format!("{ts}-{job_id}-{document_id}{suffix}");
            let bin = self.dir.join(format!("{stem}.bin"));
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&bin)
            {
                Ok(f) => break (bin, self.dir.join(format!("{stem}.json")), f),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && n < 10_000 => n += 1,
                Err(e) => return Err(e).with_context(|| format!("create {}", bin.display())),
            }
        };
        bin_file
            .write_all(payload)
            .with_context(|| format!("write {}", bin_path.display()))?;
        drop(bin_file);
        let mut meta = serde_json::json!({
            "job_id": job_id,
            "document_id": document_id,
            "bytes": payload.len(),
            "document_format": document_format,
            "peer": peer,
            "path": bin_path.file_name().and_then(|s| s.to_str()).unwrap_or(""),
        });
        if let (Some(m), Some(x)) = (meta.as_object_mut(), extra.as_object()) {
            for (k, v) in x {
                m.entry(k.clone()).or_insert_with(|| v.clone());
            }
        }
        std::fs::write(&meta_path, serde_json::to_vec_pretty(&meta)?)
            .with_context(|| format!("write {}", meta_path.display()))?;
        let captured = CapturedJob {
            job_id,
            document_id,
            path: bin_path.display().to_string(),
            meta_path: meta_path.display().to_string(),
            bytes: payload.len(),
            document_format: document_format.to_string(),
            peer: peer.to_string(),
        };
        let mut list = self.list();
        list.push(captured.clone());
        if list.len() > INBOX_MAX_LISTED {
            list.remove(0);
        }
        Ok(captured)
    }
}

/// How a [`World`] is put together beyond the profile. `Default` is the daemon's setting.
#[derive(Clone)]
pub struct WorldOptions {
    /// Time source of the job engine and the event manager (tests inject a manual clock).
    pub clock: Arc<dyn Clock>,
    pub limits: PrintLimits,
    pub timing: PrintTiming,
    /// `Some(true/false)` forces reply validation on or off; `None` follows the environment
    /// (`FAKEPRINT_VALIDATE_REPLIES`).
    pub validate_replies: Option<bool>,
    /// Which `NotifyTo` addresses a Subscribe may use.
    pub address_policy: AddressPolicy,
    /// Push the current state to a new subscriber (the lab's WSDMON experiment). Off for
    /// embedded servers, where the event stream is exactly what the printer does.
    pub initial_events: bool,
    /// Initial `SetEventRate` value; `None`: the profile's `PrinterEventRate`.
    pub event_rate_secs: Option<u32>,
}

impl Default for WorldOptions {
    fn default() -> Self {
        Self {
            clock: Arc::new(SystemClock),
            limits: PrintLimits::default(),
            timing: PrintTiming::default(),
            validate_replies: None,
            address_policy: AddressPolicy::PrivateOnly,
            initial_events: true,
            event_rate_secs: None,
        }
    }
}

pub struct World {
    profiles_dir: PathBuf,
    inbox: Arc<Inbox>,
    profile: PrinterProfile,
    options: WorldOptions,
    /// Lab instance name (`--lab-name` / `FAKEPRINT_NAME`); matches registry row.
    lab_name: String,
    /// Device id inside a multi-device server (`default` for the legacy device).
    device_id: String,
    device_endpoint: String,
    service_endpoint: String,
    device_uuid: Uuid,
    /// UUID advertised in IPP Get-Printer-Attributes (`printer-uuid`).
    ///
    /// Must differ from `device_uuid` (the WSD UUID) so that
    /// `Add-Printer -IppURL` does not hit "printer already exists" because
    /// Windows finds an existing `SWD\DAFWSDProvider` entry for the same UUID.
    ipp_uuid: Uuid,
    discovery: Arc<DiscoveryStats>,
    /// The print service: model, job engine, events, clock, configuration.
    print: Arc<PrintCtx>,
    /// The concrete model behind `print.model` (profile hot-swap replaces both).
    model: Arc<ProfilePrinterModel>,
    /// Subscription table and event queue; installed as the context's event sink.
    events: Arc<EventManager>,
    /// Ids of the conditions the profile raised at start-up.
    preloaded: Vec<u32>,
    /// SOAP short-action hit counts (lab: prove Windows Transfer Get / Subscribe).
    wsd_actions: HashMap<String, u64>,
    /// IPP operation hit counts (lab: prove WSDMON probes /ipp/print).
    ipp_actions: HashMap<String, u64>,
}

#[derive(Debug, Serialize)]
pub struct Status {
    pub name: String,
    /// Device id inside a multi-device server (`default` for the legacy device).
    pub device_id: String,
    pub crate_version: &'static str,
    pub profile_id: String,
    pub label: String,
    pub device_endpoint: String,
    pub service_endpoint: String,
    pub device_uuid: Uuid,
    /// IPP printer-uuid (distinct from WSD device_uuid; used by `Add-Printer -IppURL`).
    pub ipp_uuid: Uuid,
    pub wsd_hellos_sent: u64,
    pub wsd_probes_answered: u64,
    pub wsd_actions: HashMap<String, u64>,
    pub ipp_actions: HashMap<String, u64>,
    pub active_jobs: usize,
    pub captured_jobs: usize,
    pub subscriptions: usize,
    pub printer_state: String,
    pub inbox_dir: String,
    pub available_profiles: Vec<String>,
    pub document_formats: Vec<String>,
    /// Outgoing-message schema validation (`FAKEPRINT_VALIDATE_REPLIES`).
    pub validation: ValidationStats,
    pub note: String,
}

/// The conditions a profile raises when the printer starts.
fn raise_preloaded(model: &ProfilePrinterModel, ctx: &PrintCtx) -> Vec<u32> {
    let mut ids = Vec::new();
    for (name, component, severity) in model.preloaded_conditions() {
        if let Some(c) = ctx
            .engine
            .raise_condition(&name, Some(&component), Some(severity))
        {
            ids.push(c.id);
        }
    }
    ids
}

/// The service configuration a profile implies, on top of `base` (limits, timing, endpoints).
fn config_for(p: &PrinterProfile, base: &PrintConfig) -> PrintConfig {
    let mut c = base.clone();
    c.multiple_document_jobs = p.description.multiple_document_jobs;
    c.versions = version_flags(p);
    c.quirks = service_quirks(p);
    c.add_document = p.documents.add_document;
    c
}

impl World {
    pub fn new(
        profiles_dir: PathBuf,
        inbox_dir: PathBuf,
        profile: PrinterProfile,
        discovery: Arc<DiscoveryStats>,
    ) -> Result<Self> {
        Self::with_options(
            profiles_dir,
            inbox_dir,
            profile,
            discovery,
            WorldOptions::default(),
        )
    }

    pub fn with_options(
        profiles_dir: PathBuf,
        inbox_dir: PathBuf,
        profile: PrinterProfile,
        discovery: Arc<DiscoveryStats>,
        options: WorldOptions,
    ) -> Result<Self> {
        let inbox =
            Arc::new(Inbox::new(inbox_dir)?.with_max_document(options.limits.max_document.max(1)));
        let base = PrintConfig {
            limits: options.limits.clone(),
            timing: options.timing.clone(),
            ..PrintConfig::default()
        };
        let config = config_for(&profile, &base);
        let bus = Arc::new(SharedBus::default());
        let engine = Arc::new(JobEngine::new(
            config.limits.clone(),
            config.timing.clone(),
            options.clock.clone(),
            bus.clone(),
        ));
        let status: Arc<dyn StatusSource> = engine.clone();
        let events_bus: Arc<dyn EventBus> = bus.clone();
        let model = Arc::new(
            ProfilePrinterModel::with_status(profile.clone(), status).with_events(events_bus),
        );
        let as_model: Arc<dyn PrinterModel> = model.clone();
        let print = PrintCtx::new(as_model, engine, bus, config).with_inbox(inbox.clone());
        if let Some(on) = options.validate_replies {
            print.validator.set_enabled(on);
        }
        print.validator.set_options(validator_options(&profile));
        print.engine.set_event_rate(
            options
                .event_rate_secs
                .unwrap_or(profile.description.event_rate),
        );
        let preloaded = raise_preloaded(&model, &print);
        let mut world = Self {
            profiles_dir,
            inbox,
            profile,
            options,
            lab_name: crate::lab::LAB_NAME.to_string(),
            device_id: "default".to_string(),
            device_endpoint: String::new(),
            service_endpoint: String::new(),
            device_uuid: Uuid::nil(),
            ipp_uuid: Uuid::nil(),
            discovery,
            events: Arc::new(EventManager::new(
                EventingConfig::new(print.config.service_endpoint.clone()),
                print.clock.clone(),
            )),
            print: Arc::new(print),
            model,
            preloaded,
            wsd_actions: HashMap::new(),
            ipp_actions: HashMap::new(),
        };
        world.install_events();
        Ok(world)
    }

    /// A fresh event manager for the current service endpoint, installed as the event sink.
    fn install_events(&mut self) {
        let mut cfg = EventingConfig::new(self.print.config.service_endpoint.clone());
        cfg.address_policy = self.options.address_policy;
        cfg.event_rate = Duration::from_secs(u64::from(self.print.event_rate_secs().max(1)));
        let manager = Arc::new(EventManager::new(cfg, self.print.clock.clone()));
        self.print.set_event_sink(manager.clone());
        self.events = manager;
    }

    /// The print-service context (shared; cheap to clone).
    pub fn print_ctx(&self) -> Arc<PrintCtx> {
        Arc::clone(&self.print)
    }

    /// The subscription manager (also the context's event sink).
    pub fn events(&self) -> Arc<EventManager> {
        Arc::clone(&self.events)
    }

    /// Everything one WSD request needs, as an owned snapshot (no lock is held afterwards).
    pub fn wsd_service(&self) -> WsdService {
        WsdService {
            ctx: Arc::clone(&self.print),
            events: Arc::clone(&self.events),
            model: Arc::clone(&self.model),
            info: self.device_info(),
            seed_subscribers: self.options.initial_events,
        }
    }

    /// The metadata answer's inputs, from the profile and the identity.
    pub fn device_info(&self) -> DeviceInfo {
        let p = &self.profile;
        let mut i = DeviceInfo::new(
            self.device_uuid,
            self.device_endpoint.clone(),
            self.service_endpoint.clone(),
        );
        i.manufacturer = p.identity.manufacturer.clone();
        i.model_name = p.identity.model.clone();
        i.model_number = Some("1".to_string());
        i.friendly_name = p.friendly_name().to_string();
        i.firmware_version = Some(p.identity.firmware_version.clone());
        i.serial_number = Some(
            p.identity
                .serial_number
                .clone()
                .unwrap_or_else(|| "FP-0001".to_string()),
        );
        i.print_versions = PrintVersions {
            v11: p.versions.v11,
            v12: p.versions.v12,
            v20: p.versions.v20,
        };
        i
    }

    /// Change the print configuration (quirks, version flags, conflict rules, ...). Timing and
    /// limits are fixed when the world is created.
    pub fn configure_print(&mut self, f: impl FnOnce(&mut PrintConfig)) {
        let mut cfg = self.print.config.clone();
        f(&mut cfg);
        self.print = Arc::new(self.print.with_config(cfg));
    }

    pub fn set_lab_name(&mut self, name: impl Into<String>) {
        self.lab_name = name.into();
    }

    pub fn set_device_id(&mut self, id: impl Into<String>) {
        self.device_id = id.into();
    }

    pub fn device_id(&self) -> &str {
        &self.device_id
    }

    /// Change a consumable level at run time (publishes a configuration event).
    pub fn set_consumable_level(&self, name: &str, level: i32) -> bool {
        self.model.set_consumable_level(name, level)
    }

    /// Change an input bin level at run time (publishes a configuration event).
    pub fn set_input_level(&self, name: &str, level: i32) -> bool {
        self.model.set_input_level(name, level)
    }

    pub fn lab_name(&self) -> &str {
        &self.lab_name
    }

    /// Hot-swap the active profile without restarting; WSD identity unchanged. Conditions the
    /// old profile raised are cleared, the new profile's are raised, and a subscriber learns
    /// of the changed description through a `PrinterElementsChange` event.
    pub fn hot_swap_profile(&mut self, id: &str) -> Result<()> {
        let p = PrinterProfile::load(&self.profiles_dir, id)?;
        self.swap_profile(p);
        Ok(())
    }

    /// Replace the active profile with an already loaded one (see [`World::hot_swap_profile`]).
    pub fn swap_profile(&mut self, p: PrinterProfile) {
        let changed = published_fingerprint(&p) != self.model.published_fingerprint();
        for id in std::mem::take(&mut self.preloaded) {
            self.print.engine.clear_condition(id);
        }
        let status: Arc<dyn StatusSource> = self.print.engine.clone();
        let model = Arc::new(
            ProfilePrinterModel::with_status(p.clone(), status)
                .with_events(self.print.events.clone()),
        );
        let as_model: Arc<dyn PrinterModel> = model.clone();
        let cfg = config_for(&p, &self.print.config);
        self.print = Arc::new(self.print.with_model(as_model).with_config(cfg));
        self.print.validator.set_options(validator_options(&p));
        self.model = model;
        self.profile = p;
        self.preloaded = raise_preloaded(&self.model, &self.print);
        if changed {
            let changes = [
                "PrinterDescription",
                "PrinterConfiguration",
                "DefaultPrintTicket",
                "PrinterCapabilities",
            ]
            .iter()
            .filter_map(|n| self.print.model.element(&wprt(n)))
            .collect();
            self.print
                .events
                .publish(PrintEvent::PrinterElementsChange { changes });
        }
    }

    pub fn set_identity(&mut self, device_ep: String, service_ep: String, uuid: Uuid) {
        self.device_endpoint = device_ep.clone();
        self.service_endpoint = service_ep.clone();
        self.device_uuid = uuid;
        self.configure_print(|c| {
            c.device_endpoint = device_ep;
            c.service_endpoint = service_ep;
        });
        self.install_events();
    }

    /// Override the UUID returned in IPP Get-Printer-Attributes (`printer-uuid`).
    /// Call after `set_identity`; defaults to `derive_ipp_uuid(device_uuid)`.
    pub fn set_ipp_uuid(&mut self, uuid: Uuid) {
        self.ipp_uuid = uuid;
    }

    /// UUID for the IPP endpoint (`printer-uuid` in Get-Printer-Attributes).
    ///
    /// Differs from `device_uuid` (WSD) so that `Add-Printer -IppURL` can
    /// create an Internet Port even when the WSD PnP device is already present.
    /// Falls back to a derive of `device_uuid` if never explicitly set.
    pub fn ipp_uuid(&self) -> Uuid {
        if self.ipp_uuid.is_nil() {
            crate::lab::derive_ipp_uuid(self.device_uuid)
        } else {
            self.ipp_uuid
        }
    }

    pub fn note_wsd_action(&mut self, short: &str) {
        *self.wsd_actions.entry(short.to_string()).or_insert(0) += 1;
    }

    pub fn note_ipp_action(&mut self, op: &str) {
        *self.ipp_actions.entry(op.to_string()).or_insert(0) += 1;
    }

    pub fn profile(&self) -> &PrinterProfile {
        &self.profile
    }

    pub fn device_endpoint(&self) -> &str {
        &self.device_endpoint
    }

    pub fn service_endpoint(&self) -> &str {
        &self.service_endpoint
    }

    pub fn device_uuid(&self) -> Uuid {
        self.device_uuid
    }

    /// The URL path prefix of this device: `""` for the default (legacy) device, `"/d/<id>"`
    /// for the others. Derived from the WSD device endpoint (`http://H:P<prefix>/WebServices/Device`).
    pub fn device_prefix(&self) -> String {
        let Some(rest) = self.device_endpoint.strip_prefix("http://") else {
            return String::new();
        };
        let Some(slash) = rest.find('/') else {
            return String::new();
        };
        let path = &rest[slash..];
        path.strip_suffix("/WebServices/Device")
            .unwrap_or("")
            .to_string()
    }

    /// `ipp://host:port<prefix>/ipp/print` derived from the WSD device endpoint. `device_prefix`
    /// is `""` for the default device and `"/d/<id>"` for the others (see [`World::device_prefix`]).
    /// WSDMON with CID:MS_IPP probes this URL to create the spooler queue.
    pub fn ipp_uri(&self, device_prefix: &str) -> String {
        // device_endpoint = "http://192.168.1.70:3921/WebServices/Device"
        if let Some(rest) = self.device_endpoint.strip_prefix("http://") {
            if let Some(slash) = rest.find('/') {
                let host_port = &rest[..slash];
                let prefix = device_prefix.trim_end_matches('/');
                return format!("ipp://{host_port}{prefix}/ipp/print");
            }
        }
        "ipp://127.0.0.1:3921/ipp/print".into()
    }

    /// A copy of the live profile (consumable and bin levels may have moved since loading).
    pub fn live_profile(&self) -> PrinterProfile {
        self.model.profile()
    }

    pub fn inbox_dir(&self) -> &Path {
        self.inbox.dir()
    }

    pub fn status(&self) -> Status {
        let (hellos, probes) = self.discovery.snapshot();
        let available = PrinterProfile::list_ids(&self.profiles_dir).unwrap_or_default();
        Status {
            name: self.lab_name.clone(),
            device_id: self.device_id.clone(),
            crate_version: env!("CARGO_PKG_VERSION"),
            profile_id: self.profile.id.clone(),
            label: self.profile.label.clone(),
            device_endpoint: self.device_endpoint.clone(),
            service_endpoint: self.service_endpoint.clone(),
            device_uuid: self.device_uuid,
            ipp_uuid: self.ipp_uuid(),
            wsd_hellos_sent: hellos,
            wsd_probes_answered: probes,
            wsd_actions: self.wsd_actions.clone(),
            ipp_actions: self.ipp_actions.clone(),
            active_jobs: self.print.engine.queued_jobs(),
            captured_jobs: self.inbox.captured().len(),
            subscriptions: self.events.subscription_count(),
            printer_state: self.print.status().state.token().to_string(),
            inbox_dir: self.inbox.dir().display().to_string(),
            available_profiles: available,
            document_formats: self.profile.documents.formats.clone(),
            validation: self.print.validator.stats(),
            note: "WS-Print GetPrinterElements / CreatePrintJob / SendDocument / GetJobElements"
                .into(),
        }
    }

    /// Lab reset: jobs, conditions (the profile's start-up ones come back), captures and
    /// counters. Subscriptions are kept.
    pub fn reset(&mut self) {
        self.print.engine.reset();
        self.print.inject.reset();
        self.inbox.clear();
        self.wsd_actions.clear();
        self.ipp_actions.clear();
        self.print.validator.clear();
        self.preloaded = raise_preloaded(&self.model, &self.print);
    }

    /// Create a job for the IPP / raw listeners. Returns 0 when the printer refuses (fault
    /// plan, job cap); `capture_document(0, ..)` then fails.
    pub fn create_job(&mut self, name: &str, _document_format: &str, user: &str) -> u32 {
        let new = NewJob {
            name: name.to_string(),
            user: user.to_string(),
            ticket: None,
        };
        self.print.engine.create_job(new, 1).unwrap_or(0)
    }

    /// Finish the job (data already complete).
    pub fn end_job(&mut self, job_id: u32) {
        let _ = self.print.engine.complete_now(job_id);
    }

    pub fn cancel_job(&mut self, job_id: u32) -> bool {
        let ok = self.print.engine.cancel(job_id, CancelSource::User).is_ok();
        if ok {
            self.inbox.discard_job(job_id);
        }
        ok
    }

    /// Write a complete document of the job to the inbox and hand it to the job engine.
    pub fn capture_document(
        &mut self,
        job_id: u32,
        payload: &[u8],
        document_format: &str,
        peer: &str,
    ) -> Result<CapturedJob> {
        let job = self
            .print
            .jobs
            .get(job_id)
            .with_context(|| format!("job {job_id} is not known"))?;
        let doc_id = job.number_of_documents().saturating_add(1);
        let extra = crate::wsd::jobs::capture_meta_json(&job, doc_id, document_format, "None");
        let cap =
            self.inbox
                .capture_with(job_id, doc_id, payload, document_format, peer, &extra)?;
        let doc = DocumentIn {
            format: document_format.to_string(),
            compression: "None".to_string(),
            size: payload.len() as u64,
            capture_path: Some(PathBuf::from(&cap.path)),
            last: true,
            ..DocumentIn::default()
        };
        self.print
            .engine
            .commit_document(job_id, doc, true)
            .map_err(|f| anyhow::anyhow!("job {job_id} refused the document: {f}"))?;
        Ok(cap)
    }

    pub fn captured(&self) -> Vec<CapturedJob> {
        self.inbox.captured()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wsd::types::JobState as WireJobState;

    struct TempDir(PathBuf);
    impl TempDir {
        fn new() -> Self {
            let p = std::env::temp_dir().join(format!("fakeprint-test-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&p).unwrap();
            Self(p)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn world(tmp: &TempDir) -> World {
        let mut profile = PrinterProfile::generic();
        profile.documents.formats = vec!["application/pdf".into()];
        World::new(
            tmp.0.clone(),
            tmp.0.join("inbox"),
            profile,
            Arc::new(DiscoveryStats::default()),
        )
        .unwrap()
    }

    #[test]
    fn capture_writes_bin_and_json() {
        let tmp = TempDir::new();
        let mut w = world(&tmp);
        let id = w.create_job("test", "application/pdf", "lab");
        let cap = w
            .capture_document(id, b"%PDF-1.4 fake", "application/pdf", "127.0.0.1:1")
            .unwrap();
        assert_eq!(cap.bytes, 13);
        assert!(Path::new(&cap.path).is_file());
        assert!(Path::new(&cap.meta_path).is_file());
        let ctx = w.print_ctx();
        assert_eq!(ctx.jobs.get(id).unwrap().state, WireJobState::Processing);
        w.end_job(id);
        assert_eq!(ctx.jobs.get(id).unwrap().state, WireJobState::Completed);
        assert_eq!(w.status().captured_jobs, 1);
    }

    #[test]
    fn two_documents_in_one_second_do_not_overwrite_each_other() {
        let tmp = TempDir::new();
        let inbox = Inbox::new(tmp.0.join("i")).unwrap();
        let a = inbox
            .capture(1, 1, b"first", "application/pdf", "p")
            .unwrap();
        let b = inbox
            .capture(1, 2, b"second", "application/pdf", "p")
            .unwrap();
        let c = inbox
            .capture(1, 2, b"third", "application/pdf", "p")
            .unwrap();
        assert_ne!(a.path, b.path);
        assert_ne!(b.path, c.path);
        assert_eq!(std::fs::read(&a.path).unwrap(), b"first");
        assert_eq!(std::fs::read(&b.path).unwrap(), b"second");
        assert_eq!(std::fs::read(&c.path).unwrap(), b"third");
        assert_eq!(inbox.captured().len(), 3);
    }

    #[test]
    fn inbox_names_are_numeric_and_size_is_capped() {
        let tmp = TempDir::new();
        let inbox = Inbox::new(tmp.0.join("i")).unwrap().with_max_document(4);
        assert!(inbox.capture(1, 1, b"12345", "x", "p").is_err());
        let ok = inbox
            .capture(7, 3, b"1234", "../../evil", "..\\peer")
            .unwrap();
        let name = Path::new(&ok.path)
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(name.ends_with("-7-3.bin"), "{name}");
        assert_eq!(Path::new(&ok.path).parent().unwrap(), inbox.dir());
    }

    #[test]
    fn capture_for_unknown_job_fails() {
        let tmp = TempDir::new();
        let mut w = world(&tmp);
        assert!(w.capture_document(0, b"x", "application/pdf", "p").is_err());
        assert!(w
            .capture_document(99, b"x", "application/pdf", "p")
            .is_err());
    }

    fn shipped(id: &str) -> PrinterProfile {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../profiles");
        PrinterProfile::load(&dir, id).unwrap()
    }

    #[test]
    fn profile_conditions_are_raised_at_start_and_follow_a_swap_and_a_reset() {
        let tmp = TempDir::new();
        let mut w = World::new(
            tmp.0.join("profiles"),
            tmp.0.join("inbox"),
            shipped("low-toner-paper-out"),
            Arc::new(DiscoveryStats::default()),
        )
        .unwrap();
        let ctx = w.print_ctx();
        let st = ctx.status();
        let names: Vec<&str> = st
            .active_conditions
            .iter()
            .map(|c| c.name.as_str())
            .collect();
        assert_eq!(names, ["ConsumableLow", "InputTraySupplyEmpty"]);
        assert_eq!(st.active_conditions[0].component, "Consumable/BlackToner");
        // a critical condition stops the printer; reasons say why
        assert_eq!(st.state.token(), "Stopped");
        assert!(st.reasons.iter().any(|r| r == "MediaEmpty"));
        assert!(st.reasons.iter().any(|r| r == "MarkerSupplyLow"));

        // reset keeps the profile's own conditions and drops the ones the lab added
        ctx.apply_faults(&crate::wsd::ctx::FaultBody {
            raise: vec![crate::wsd::ctx::RaiseSpec {
                name: "CoverOpen".into(),
                ..Default::default()
            }],
            ..Default::default()
        });
        assert_eq!(ctx.status().active_conditions.len(), 3);
        w.reset();
        assert_eq!(ctx.status().active_conditions.len(), 2);

        // swapping to a profile without conditions clears them; the model follows
        w.swap_profile(shipped("generic-wsd"));
        let ctx = w.print_ctx();
        assert!(ctx.status().active_conditions.is_empty());
        assert_eq!(ctx.status().state.token(), "Idle");
        assert_eq!(w.profile().id, "generic-wsd");
        assert!(ctx.model.element(&wprt("PrinterDescription")).is_some());
    }

    #[test]
    fn profile_flags_configure_the_service() {
        let tmp = TempDir::new();
        let mut p = PrinterProfile::generic();
        p.versions.v12 = true;
        p.quirks.job_token = true;
        p.documents.add_document = true;
        p.description.multiple_document_jobs = false;
        let mut w = World::new(
            tmp.0.clone(),
            tmp.0.join("inbox"),
            p,
            Arc::new(DiscoveryStats::default()),
        )
        .unwrap();
        let c = w.print_ctx().config.clone();
        assert!(c.versions.v12 && !c.versions.v20);
        assert!(c.quirks.job_token && c.add_document && !c.multiple_document_jobs);
        w.swap_profile(PrinterProfile::generic());
        let c = w.print_ctx().config.clone();
        assert!(
            !c.versions.v12 && !c.quirks.job_token && !c.add_document && c.multiple_document_jobs
        );
    }

    #[test]
    fn capture_with_merges_extra_keys_but_never_replaces_existing_ones() {
        let t = TempDir::new();
        let inbox = Inbox::new(t.0.clone()).unwrap();
        let extra = serde_json::json!({"name": "n", "seq": 1, "bytes": 999});
        let c = inbox
            .capture_with(3, 1, b"abc", "application/pdf", "wsd", &extra)
            .unwrap();
        let v: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&c.meta_path).unwrap()).unwrap();
        assert_eq!(v["name"], "n");
        assert_eq!(v["seq"], 1);
        assert_eq!(v["bytes"], 3);
        assert_eq!(v["document_format"], "application/pdf");
    }
}
