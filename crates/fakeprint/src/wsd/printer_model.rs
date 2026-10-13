//! The profile-backed printer model.
//!
//! [`ProfilePrinterModel`] answers `GetPrinterElements`: for every section the profile's
//! versions enable it builds the element from the profile (what the printer is and accepts)
//! and, for `PrinterStatus`, from a live [`StatusSource`] (what it is doing). Capabilities
//! list only what the profile declares.
//!
//! The job engine of the print service is the normal status source (it owns jobs, timers and
//! raised conditions). For standalone use and tests, [`LocalStatus`] is a small status source
//! of its own that also implements [`ConditionSink`], so conditions can be raised and cleared
//! and the printer state follows: any `Critical` condition stops the printer, a pause stops
//! it, a job in progress makes it `Processing`, otherwise `Idle`.

use std::sync::{Arc, Mutex, MutexGuard, RwLock};
use std::time::SystemTime;

use crate::profile::{ConditionSpec, PrinterProfile};
use crate::wsd::ctx::{
    format_datetime, Clock, ConditionRecord, PrinterStatusSnapshot, Quirks, StatusSource,
    SystemClock, VersionFlags,
};
use crate::wsd::elements as el;
use crate::wsd::types::{
    DeviceCondition, EventBus, JobState, JobStore, PrintEvent, PrinterModel, PrinterState, Severity,
};
use crate::wsd::xml::{ns, Element, QName};

// ---------------------------------------------------------------------------------------------
// Mapping profile -> the service configuration
// ---------------------------------------------------------------------------------------------

/// The version switches the print service takes (`PrintConfig.versions`).
pub fn version_flags(p: &PrinterProfile) -> VersionFlags {
    VersionFlags {
        v12: p.versions.v12,
        v20: p.versions.v20,
    }
}

/// The vendor quirks the print service takes (`PrintConfig.quirks`).
pub fn service_quirks(p: &PrinterProfile) -> Quirks {
    Quirks {
        job_token: p.quirks.job_token,
        lenient_job_description: p.quirks.lenient_job_description,
        lenient_document_description: p.quirks.lenient_document_description,
    }
}

/// Validator options matching the profile's quirks (replies must pass these).
pub fn validator_options(p: &PrinterProfile) -> el::schema::Options {
    let mut o = el::schema::Options::STRICT;
    o.quirk_job_token = p.quirks.job_token;
    o.quirk_printer_local_time = p.quirks.printer_local_time;
    o
}

// ---------------------------------------------------------------------------------------------
// Condition knowledge
// ---------------------------------------------------------------------------------------------

/// The `PrinterStateReason` a condition shows as.
pub fn state_reason_for(condition: &str) -> &'static str {
    match condition {
        "CoverOpen" | "InterlockOpen" | "DoorOpen" => "DoorOpen",
        "Jam" => "MediaJam",
        "InputTraySupplyEmpty" => "MediaEmpty",
        "InputTraySupplyLow" => "MediaLow",
        "InputTrayMissing" | "InputTrayMediaSizeChange" | "InputTrayMediaTypeChange" => {
            "MediaNeeded"
        }
        "OutputTrayAlmostFull" => "OutputAreaAlmostFull",
        "OutputTrayFull" => "OutputAreaFull",
        "ConsumableLow" => "MarkerSupplyLow",
        "ConsumableEmpty" => "MarkerSupplyEmpty",
        _ => "AttentionRequired",
    }
}

/// The usual severity of a condition: running-low style warnings, everything else critical.
pub fn default_severity(condition: &str) -> Severity {
    match condition {
        "InputTraySupplyLow"
        | "OutputTrayAlmostFull"
        | "ConsumableLow"
        | "WasteReceptacleAlmostFull"
        | "InputTrayMediaSizeChange"
        | "InputTrayMediaTypeChange" => Severity::Warning,
        _ => Severity::Critical,
    }
}

/// The component group a condition is about.
pub fn default_group(condition: &str) -> &'static str {
    match condition {
        "CoverOpen" | "InterlockOpen" | "DoorOpen" => "CoverDoor",
        "Jam" | "FuserUnderTemperature" | "FuserOverTemperature" => "MediaPath",
        "InputTrayMissing"
        | "InputTrayMediaSizeChange"
        | "InputTrayMediaTypeChange"
        | "InputTraySupplyLow"
        | "InputTraySupplyEmpty" => "InputBin",
        "OutputTrayMissing" | "OutputTrayAlmostFull" | "OutputTrayFull" => "OutputBin",
        "ConsumableLow"
        | "ConsumableEmpty"
        | "WasteReceptacleAlmostFull"
        | "WasteReceptacleFull" => "Consumable",
        _ => "PrintService",
    }
}

fn severity_rank(s: Severity) -> u8 {
    match s {
        Severity::Informational => 0,
        Severity::Warning => 1,
        Severity::Critical => 2,
    }
}

pub fn parse_severity(s: &str) -> Option<Severity> {
    [
        Severity::Informational,
        Severity::Warning,
        Severity::Critical,
    ]
    .into_iter()
    .find(|v| v.token() == s)
}

/// Fill in the group, component name and severity of a profile condition from the profile's
/// bins and consumables. Returns `(name, "Group/Name", severity)`.
pub fn resolve_condition(p: &PrinterProfile, c: &ConditionSpec) -> (String, String, Severity) {
    let group = c
        .group
        .clone()
        .unwrap_or_else(|| default_group(&c.name).to_string());
    let name = c.component.clone().unwrap_or_else(|| {
        match group.as_str() {
            "InputBin" => p.inputs().first().map(|b| b.name.clone()),
            "OutputBin" => p.outputs().first().map(|b| b.name.clone()),
            "Consumable" => p.consumables().first().map(|k| k.name.clone()),
            _ => None,
        }
        .unwrap_or_default()
    });
    let component = if name.is_empty() {
        group
    } else {
        format!("{group}/{name}")
    };
    let severity = c
        .severity
        .as_deref()
        .and_then(parse_severity)
        .unwrap_or_else(|| default_severity(&c.name));
    (c.name.clone(), component, severity)
}

/// Printer state, primary reason and reasons for a set of active conditions.
pub fn derive_state(
    conditions: &[DeviceCondition],
    paused: bool,
    job_running: bool,
) -> (PrinterState, Option<String>, Vec<String>) {
    let mut ranked: Vec<&DeviceCondition> = conditions.iter().collect();
    ranked.sort_by(|a, b| {
        severity_rank(b.severity)
            .cmp(&severity_rank(a.severity))
            .then(a.id.cmp(&b.id))
    });
    let mut reasons: Vec<String> = Vec::new();
    if paused {
        reasons.push("Paused".into());
    }
    for c in &ranked {
        let r = state_reason_for(&c.name).to_string();
        if !reasons.contains(&r) {
            reasons.push(r);
        }
    }
    let stopped = paused || conditions.iter().any(|c| c.severity == Severity::Critical);
    let state = if stopped {
        PrinterState::Stopped
    } else if job_running {
        PrinterState::Processing
    } else {
        PrinterState::Idle
    };
    let primary = reasons.first().cloned();
    if reasons.is_empty() {
        reasons.push("None".into());
    }
    (state, primary, reasons)
}

// ---------------------------------------------------------------------------------------------
// Condition sink and a standalone status source
// ---------------------------------------------------------------------------------------------

/// What raises and clears printer conditions (a fault plan, a lab control, a test).
pub trait ConditionSink: Send + Sync {
    /// Raise a condition; returns it with its new id. `component` is `Group` or `Group/Name`.
    fn raise(&self, name: &str, component: &str, severity: Severity) -> DeviceCondition;
    /// Clear one condition; false when no such id is active.
    fn clear(&self, id: u32) -> bool;
    /// Clear every active condition with this name; returns how many.
    fn clear_named(&self, name: &str) -> usize;
    /// Clear everything.
    fn clear_all(&self) -> usize;
    /// Pause or resume the printer (state `Stopped`, reason `Paused`).
    fn set_paused(&self, paused: bool);
}

const HISTORY_CAP: usize = 16;

struct LocalInner {
    next_id: u32,
    paused: bool,
    active: Vec<DeviceCondition>,
    history: Vec<ConditionRecord>,
}

/// Conditions, pause flag and status derivation for a printer without a job engine.
pub struct LocalStatus {
    clock: Arc<dyn Clock>,
    jobs: Option<Arc<dyn JobStore>>,
    bus: Option<Arc<dyn EventBus>>,
    inner: Mutex<LocalInner>,
}

impl LocalStatus {
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            clock,
            jobs: None,
            bus: None,
            inner: Mutex::new(LocalInner {
                next_id: 1,
                paused: false,
                active: Vec::new(),
                history: Vec::new(),
            }),
        }
    }

    /// Count queued jobs from, and detect running jobs in, this store.
    pub fn with_jobs(mut self, jobs: Arc<dyn JobStore>) -> Self {
        self.jobs = Some(jobs);
        self
    }

    /// Publish condition and status summary events to this bus.
    pub fn with_events(mut self, bus: Arc<dyn EventBus>) -> Self {
        self.bus = Some(bus);
        self
    }

    fn lock(&self) -> MutexGuard<'_, LocalInner> {
        match self.inner.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        }
    }

    fn now(&self) -> String {
        format_datetime(self.clock.now())
    }

    fn summary(&self, g: &LocalInner) -> PrintEvent {
        let (state, primary, reasons) = derive_state(&g.active, g.paused, self.job_running());
        PrintEvent::PrinterStatusSummary {
            state,
            primary_reason: primary,
            reasons,
        }
    }

    fn job_running(&self) -> bool {
        self.jobs.as_ref().is_some_and(|j| {
            j.active()
                .iter()
                .any(|x| matches!(x.state, JobState::Processing | JobState::Started))
        })
    }

    fn publish(&self, events: Vec<PrintEvent>) {
        if let Some(bus) = &self.bus {
            for e in events {
                bus.publish(e);
            }
        }
    }
}

impl StatusSource for LocalStatus {
    fn status(&self) -> PrinterStatusSnapshot {
        let g = self.lock();
        let (state, primary, reasons) = derive_state(&g.active, g.paused, self.job_running());
        PrinterStatusSnapshot {
            current_time: self.now(),
            state,
            primary_reason: primary,
            reasons,
            queued_jobs: self
                .jobs
                .as_ref()
                .map_or(0, |j| u32::try_from(j.active().len()).unwrap_or(u32::MAX)),
            active_conditions: g.active.clone(),
            condition_history: g.history.clone(),
            accepting_jobs: !g.active.iter().any(|c| c.severity == Severity::Critical),
        }
    }
}

impl ConditionSink for LocalStatus {
    fn raise(&self, name: &str, component: &str, severity: Severity) -> DeviceCondition {
        let (cond, events) = {
            let mut g = self.lock();
            let id = g.next_id;
            g.next_id = g.next_id.saturating_add(1).max(1);
            let c = DeviceCondition {
                id,
                time: self.now(),
                name: name.to_string(),
                component: component.to_string(),
                severity,
            };
            g.active.push(c.clone());
            let events = vec![
                PrintEvent::PrinterStatusCondition {
                    condition: c.clone(),
                },
                self.summary(&g),
            ];
            (c, events)
        };
        self.publish(events);
        cond
    }

    fn clear(&self, id: u32) -> bool {
        let events = {
            let mut g = self.lock();
            let Some(at) = g.active.iter().position(|c| c.id == id) else {
                return false;
            };
            let c = g.active.remove(at);
            let cleared = self.now();
            g.history.push(ConditionRecord {
                condition: c,
                cleared: Some(cleared.clone()),
            });
            if g.history.len() > HISTORY_CAP {
                g.history.remove(0);
            }
            vec![
                PrintEvent::PrinterStatusConditionCleared {
                    condition_id: id,
                    cleared,
                },
                self.summary(&g),
            ]
        };
        self.publish(events);
        true
    }

    fn clear_named(&self, name: &str) -> usize {
        let ids: Vec<u32> = self
            .lock()
            .active
            .iter()
            .filter(|c| c.name == name)
            .map(|c| c.id)
            .collect();
        ids.into_iter().filter(|id| self.clear(*id)).count()
    }

    fn clear_all(&self) -> usize {
        let ids: Vec<u32> = self.lock().active.iter().map(|c| c.id).collect();
        ids.into_iter().filter(|id| self.clear(*id)).count()
    }

    fn set_paused(&self, paused: bool) {
        let events = {
            let mut g = self.lock();
            if g.paused == paused {
                return;
            }
            g.paused = paused;
            vec![self.summary(&g)]
        };
        self.publish(events);
    }
}

// ---------------------------------------------------------------------------------------------
// The model
// ---------------------------------------------------------------------------------------------

/// A [`PrinterModel`] built from a profile and a live status source.
pub struct ProfilePrinterModel {
    profile: RwLock<PrinterProfile>,
    status: Arc<dyn StatusSource>,
    local: Option<Arc<LocalStatus>>,
    events: Option<Arc<dyn EventBus>>,
}

impl ProfilePrinterModel {
    /// Standalone model: system clock, its own [`LocalStatus`], profile conditions preloaded.
    pub fn new(profile: PrinterProfile) -> Self {
        Self::standalone(profile, Arc::new(SystemClock))
    }

    /// Standalone model on a given clock.
    pub fn standalone(profile: PrinterProfile, clock: Arc<dyn Clock>) -> Self {
        let local = Arc::new(LocalStatus::new(clock));
        Self::over_local(profile, local)
    }

    /// Standalone model over a prepared [`LocalStatus`] (jobs, events). Preloads the
    /// profile's conditions.
    pub fn over_local(profile: PrinterProfile, local: Arc<LocalStatus>) -> Self {
        for c in &profile.conditions {
            let (name, component, severity) = resolve_condition(&profile, c);
            local.raise(&name, &component, severity);
        }
        Self {
            profile: RwLock::new(profile),
            status: local.clone(),
            local: Some(local),
            events: None,
        }
    }

    /// Model over an external status source (the job engine). Preloaded profile conditions
    /// are the caller's to raise: see [`ProfilePrinterModel::preloaded_conditions`].
    pub fn with_status(profile: PrinterProfile, status: Arc<dyn StatusSource>) -> Self {
        Self {
            profile: RwLock::new(profile),
            status,
            local: None,
            events: None,
        }
    }

    /// Publish `PrinterElementsChange` events (consumable levels, event rate) here.
    pub fn with_events(mut self, bus: Arc<dyn EventBus>) -> Self {
        self.events = Some(bus);
        self
    }

    fn read(&self) -> PrinterProfile {
        match self.profile.read() {
            Ok(g) => g.clone(),
            Err(p) => p.into_inner().clone(),
        }
    }

    /// A copy of the current profile (levels and event rate may have changed since loading).
    pub fn profile(&self) -> PrinterProfile {
        self.read()
    }

    /// The condition sink of a standalone model.
    pub fn conditions(&self) -> Option<Arc<dyn ConditionSink>> {
        self.local.clone().map(|l| l as Arc<dyn ConditionSink>)
    }

    /// The profile's start-up conditions resolved to `(name, "Group/Name", severity)`, for a
    /// fault plan or engine to raise.
    pub fn preloaded_conditions(&self) -> Vec<(String, String, Severity)> {
        let p = self.read();
        p.conditions
            .iter()
            .map(|c| resolve_condition(&p, c))
            .collect()
    }

    /// Live status (same as the underlying source).
    pub fn status(&self) -> PrinterStatusSnapshot {
        self.status.status()
    }

    /// `PrinterStatus` from the live status, honouring the profile's quirks and versions.
    pub fn printer_status(&self) -> Element {
        let p = self.read();
        el::printer_status(
            &self.status.status(),
            p.versions.v20,
            p.quirks.printer_local_time,
        )
    }

    /// Change a consumable's level; publishes a configuration change event. False if unknown
    /// or out of range (-1 to 100).
    pub fn set_consumable_level(&self, name: &str, level: i32) -> bool {
        if !(-1..=100).contains(&level) {
            return false;
        }
        let config = {
            let mut g = match self.profile.write() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            let Some(list) = g.consumables.as_mut() else {
                return false;
            };
            let Some(c) = list.iter_mut().find(|c| c.name == name) else {
                return false;
            };
            c.level = level;
            el::printer_configuration(&g)
        };
        self.publish_change(config);
        true
    }

    /// Set the `PrinterEventRate` (1..600 seconds); false when out of range.
    pub fn set_event_rate(&self, seconds: u32) -> bool {
        if !(1..=600).contains(&seconds) {
            return false;
        }
        let config = {
            let mut g = match self.profile.write() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            g.description.event_rate = seconds;
            el::printer_configuration(&g)
        };
        self.publish_change(config);
        true
    }

    fn publish_change(&self, config: Element) {
        if let Some(bus) = &self.events {
            bus.publish(PrintEvent::PrinterElementsChange {
                changes: vec![config],
            });
        }
    }

    /// Fingerprint of everything this printer publishes that a client may cache: description,
    /// configuration, default ticket, capabilities and the version sections. Status and
    /// levels are not part of it. A changed value means Windows' cached view is stale.
    pub fn published_fingerprint(&self) -> String {
        published_fingerprint(&self.read())
    }
}

fn section_names(p: &PrinterProfile) -> Vec<QName> {
    let mut v = vec![
        el::wprt("PrinterDescription"),
        el::wprt("PrinterConfiguration"),
        el::wprt("PrinterStatus"),
        el::wprt("DefaultPrintTicket"),
        el::wprt("PrinterCapabilities"),
    ];
    if p.versions.v11 {
        v.push(QName::new(ns::PRINT11, "SupportsWSPrintv11"));
        if p.extensions.driver_config.is_some() {
            v.push(QName::new(ns::PRINT11, "DriverConfiguration"));
        }
        if p.extensions.model_id.is_some() {
            v.push(QName::new(ns::PRINT11, "DeviceModelId"));
        }
    }
    if p.versions.v12 {
        v.push(QName::new(ns::PRINT12, "SupportsWSPrintV12"));
    }
    if p.versions.v20 {
        v.push(QName::new(ns::PRINT20, "SupportsWSPrintV20"));
        v.push(QName::new(ns::PRINT20, "PrinterFormats"));
        v.push(QName::new(ns::PRINT20, "PrintDeviceCapabilitiesChangeID"));
    }
    v
}

impl PrinterModel for ProfilePrinterModel {
    fn element(&self, name: &QName) -> Option<Element> {
        let p = self.read();
        let flag = |n: &str, uri: &str| Element::text_el(uri, n, "true");
        match (name.ns.as_str(), name.local.as_str()) {
            (ns::PRINT, "PrinterDescription") => Some(el::printer_description(&p)),
            (ns::PRINT, "PrinterConfiguration") => Some(el::printer_configuration(&p)),
            (ns::PRINT, "PrinterStatus") => Some(el::printer_status(
                &self.status.status(),
                p.versions.v20,
                p.quirks.printer_local_time,
            )),
            (ns::PRINT, "DefaultPrintTicket") => Some(el::default_print_ticket(&p)),
            (ns::PRINT, "PrinterCapabilities") => Some(el::printer_capabilities(&p)),
            (ns::PRINT11, "SupportsWSPrintv11") if p.versions.v11 => {
                Some(flag("SupportsWSPrintv11", ns::PRINT11))
            }
            (ns::PRINT11, "DriverConfiguration") if p.versions.v11 => el::driver_configuration(&p),
            (ns::PRINT11, "DeviceModelId") if p.versions.v11 => p
                .extensions
                .model_id
                .as_ref()
                .map(|id| Element::text_el(ns::PRINT11, "DeviceModelId", id.as_str())),
            (ns::PRINT12, "SupportsWSPrintV12") if p.versions.v12 => {
                Some(flag("SupportsWSPrintV12", ns::PRINT12))
            }
            (ns::PRINT20, "SupportsWSPrintV20") if p.versions.v20 => {
                Some(flag("SupportsWSPrintV20", ns::PRINT20))
            }
            (ns::PRINT20, "PrinterFormats") if p.versions.v20 => Some(el::printer_formats(&p)),
            (ns::PRINT20, "PrintDeviceCapabilitiesChangeID") if p.versions.v20 => {
                Some(el::print_device_capabilities_change_id(&p))
            }
            _ => None,
        }
    }

    fn sections(&self) -> Vec<QName> {
        section_names(&self.read())
    }
}

// ---------------------------------------------------------------------------------------------
// Fingerprint
// ---------------------------------------------------------------------------------------------

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// See [`ProfilePrinterModel::published_fingerprint`].
pub fn published_fingerprint(p: &PrinterProfile) -> String {
    let mut q = p.clone();
    // levels move at run time and are not capabilities
    if let Some(list) = q.consumables.as_mut() {
        for c in list {
            c.level = 0;
        }
    }
    if let Some(list) = q.inputs.as_mut() {
        for b in list {
            b.level = 0;
        }
    }
    if let Some(list) = q.outputs.as_mut() {
        for b in list {
            b.level = 0;
        }
    }
    let mut text = String::new();
    for e in [
        el::printer_description(&q),
        el::printer_configuration(&q),
        el::default_print_ticket(&q),
        el::printer_capabilities(&q),
    ] {
        text.push_str(&el::to_xml(&e));
        text.push('\n');
    }
    format!("fnv64:{:016x}", fnv1a64(text.as_bytes()))
}

/// Time value for tests and callers that need "now" in the model's format.
pub fn format_now(clock: &dyn Clock) -> String {
    format_datetime(clock.now())
}

/// The time of a clock as a `SystemTime` (convenience for callers holding a model).
pub fn clock_time(clock: &dyn Clock) -> SystemTime {
    clock.now()
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::wsd::ctx::ManualClock;
    use crate::wsd::elements::schema::{validate_body, Target};
    use crate::wsd::types::RecordingEventBus;

    fn profile(extra: &str) -> PrinterProfile {
        PrinterProfile::from_json(&format!(
            r#"{{"schema_version":2,"id":"m","label":"Model"{extra}}}"#
        ))
        .unwrap()
    }

    fn status_text(m: &ProfilePrinterModel) -> String {
        let s = m.element(&el::wprt("PrinterStatus")).unwrap();
        let v = validate_body(Target::Section("PrinterStatus"), &s);
        assert!(v.is_empty(), "{v:?}");
        el::to_xml(&s)
    }

    #[test]
    fn time_really_advances() {
        let clock = Arc::new(ManualClock::new());
        let m = ProfilePrinterModel::standalone(profile(""), clock.clone());
        let t0 = status_text(&m);
        clock.advance(Duration::from_secs(90));
        let t1 = status_text(&m);
        assert!(
            t0.contains("2026-01-01T00:00:00Z") && t1.contains("2026-01-01T00:01:30Z"),
            "{t0} {t1}"
        );
    }

    #[test]
    fn raising_and_clearing_conditions_changes_the_status() {
        let clock = Arc::new(ManualClock::new());
        let bus = Arc::new(RecordingEventBus::new());
        let local = Arc::new(LocalStatus::new(clock.clone()).with_events(bus.clone()));
        let m = ProfilePrinterModel::over_local(profile(""), local);
        let sink = m.conditions().unwrap();
        assert!(
            status_text(&m).contains("<wprt:PrinterState>Idle"),
            "{}",
            status_text(&m)
        );

        let warn = sink.raise("ConsumableLow", "Consumable/Black", Severity::Warning);
        let t = status_text(&m);
        assert!(
            t.contains("PrinterState>Idle")
                && t.contains("MarkerSupplyLow")
                && t.contains("ActiveCondition"),
            "{t}"
        );

        let crit = sink.raise("InputTraySupplyEmpty", "InputBin/Tray1", Severity::Critical);
        clock.advance(Duration::from_secs(5));
        let t = status_text(&m);
        assert!(t.contains("PrinterState>Stopped"), "{t}");
        assert!(
            t.contains("PrinterPrimaryStateReason>MediaEmpty"),
            "critical outranks warning: {t}"
        );
        assert_eq!(m.status().active_conditions.len(), 2);

        assert!(sink.clear(crit.id));
        assert!(!sink.clear(crit.id));
        let t = status_text(&m);
        assert!(
            t.contains("PrinterState>Idle")
                && t.contains("ConditionHistory")
                && t.contains("ClearTime"),
            "{t}"
        );
        assert_eq!(sink.clear_named("ConsumableLow"), 1);
        assert!(!status_text(&m).contains("ActiveCondition"));
        let _ = warn;

        sink.set_paused(true);
        let t = status_text(&m);
        assert!(
            t.contains("PrinterState>Stopped") && t.contains("Paused"),
            "{t}"
        );
        sink.set_paused(false);
        assert!(status_text(&m).contains("PrinterState>Idle"));

        let names: Vec<_> = bus.events().iter().map(|e| e.name()).collect();
        assert!(names.contains(&"PrinterStatusConditionEvent"));
        assert!(names.contains(&"PrinterStatusConditionClearedEvent"));
        assert!(names.contains(&"PrinterStatusSummaryEvent"));
    }

    #[test]
    fn preloaded_conditions_show_at_start() {
        let p = profile(
            r#","consumables":[{"name":"K","level":4,"color":"Black"}],
               "conditions":[{"name":"ConsumableLow"},{"name":"InputTraySupplyEmpty","component":"Tray1"}]"#,
        );
        let m = ProfilePrinterModel::new(p);
        let s = m.status();
        assert_eq!(s.active_conditions.len(), 2);
        assert_eq!(s.state, PrinterState::Stopped);
        assert_eq!(s.active_conditions[0].component, "Consumable/K");
        assert_eq!(s.active_conditions[1].component, "InputBin/Tray1");
        assert_eq!(s.primary_reason.as_deref(), Some("MediaEmpty"));
        status_text(&m);
    }

    #[test]
    fn version_flags_change_the_sections() {
        let names = |m: &ProfilePrinterModel| {
            m.sections()
                .iter()
                .map(|q| q.local.clone())
                .collect::<Vec<_>>()
        };
        let m10 = ProfilePrinterModel::new(profile(""));
        assert_eq!(
            names(&m10),
            [
                "PrinterDescription",
                "PrinterConfiguration",
                "PrinterStatus",
                "DefaultPrintTicket",
                "PrinterCapabilities"
            ]
        );
        assert!(m10
            .element(&QName::new(ns::PRINT20, "PrinterFormats"))
            .is_none());
        assert!(m10
            .element(&QName::new(ns::PRINT12, "SupportsWSPrintV12"))
            .is_none());

        let full = profile(
            r#","versions":{"v11":true,"v12":true,"v20":true},
               "extensions":{"model_id":"11111111-2222-4333-8444-555555555555","driver_config":{"ppd_config_file":"x.ppd"},"v20":{}}"#,
        );
        let m = ProfilePrinterModel::new(full.clone());
        let n = names(&m);
        for want in [
            "SupportsWSPrintv11",
            "DriverConfiguration",
            "DeviceModelId",
            "SupportsWSPrintV12",
            "SupportsWSPrintV20",
            "PrinterFormats",
            "PrintDeviceCapabilitiesChangeID",
        ] {
            assert!(n.contains(&want.to_string()), "{want} in {n:?}");
        }
        for q in m.sections() {
            let e = m
                .element(&q)
                .unwrap_or_else(|| panic!("{q} advertised but not answered"));
            let v = crate::wsd::elements::schema::validate_root(
                &e,
                &crate::wsd::elements::schema::Options::STRICT,
            );
            assert!(v.is_empty(), "{q}: {v:?}");
        }
        assert!(status_text(&m).contains("Volatile"));
        assert!(m.element(&el::wprt("Nope")).is_none());
        assert_eq!(
            version_flags(&full),
            VersionFlags {
                v12: true,
                v20: true
            }
        );
    }

    #[test]
    fn consumable_levels_and_event_rate_publish_changes() {
        let bus = Arc::new(RecordingEventBus::new());
        let p = profile(r#","consumables":[{"name":"K","level":50,"color":"Black"}]"#);
        let m = ProfilePrinterModel::new(p).with_events(bus.clone());
        let before = m.published_fingerprint();
        assert!(m.set_consumable_level("K", 7));
        assert!(!m.set_consumable_level("K", 101) && !m.set_consumable_level("nope", 1));
        assert!(m.set_event_rate(45) && !m.set_event_rate(0));
        let cfg = m.element(&el::wprt("PrinterConfiguration")).unwrap();
        assert!(el::to_xml(&cfg).contains(">7<") && el::to_xml(&cfg).contains(">45<"));
        assert_eq!(bus.events().len(), 2);
        assert_ne!(before, m.published_fingerprint(), "event rate is published");
        for ev in bus.events() {
            let body = el::event_body(&ev);
            assert!(validate_body(Target::Event(ev.name()), &body).is_empty());
        }
        // levels alone do not change the fingerprint
        let m2 = ProfilePrinterModel::new(profile(
            r#","consumables":[{"name":"K","level":50,"color":"Black"}]"#,
        ));
        let f = m2.published_fingerprint();
        m2.set_consumable_level("K", 3);
        assert_eq!(f, m2.published_fingerprint());
    }

    #[test]
    fn external_status_source_is_used() {
        struct Fixed;
        impl StatusSource for Fixed {
            fn status(&self) -> PrinterStatusSnapshot {
                PrinterStatusSnapshot {
                    current_time: "2030-05-06T07:08:09Z".into(),
                    state: PrinterState::Processing,
                    primary_reason: None,
                    reasons: vec!["None".into()],
                    queued_jobs: 4,
                    active_conditions: vec![],
                    condition_history: vec![],
                    accepting_jobs: true,
                }
            }
        }
        let m = ProfilePrinterModel::with_status(profile(""), Arc::new(Fixed));
        let t = status_text(&m);
        assert!(
            t.contains("2030-05-06T07:08:09Z") && t.contains("Processing") && t.contains(">4<"),
            "{t}"
        );
        assert!(m.conditions().is_none());
    }

    #[test]
    fn condition_defaults_are_consistent_with_the_schema() {
        for name in crate::wsd::elements::schema::well_known("ConditionNameType") {
            let r = state_reason_for(name);
            assert!(
                crate::wsd::elements::schema::check_value("PrinterStateReasonBaseType", r).is_ok(),
                "{name}"
            );
            let g = default_group(name);
            assert!(
                crate::wsd::elements::schema::check_value("ComponentGroupType", g).is_ok(),
                "{name}"
            );
        }
    }

    #[test]
    fn regression_status_has_no_local_time_unless_the_quirk_is_on() {
        let m = ProfilePrinterModel::new(profile(""));
        assert!(!status_text(&m).contains("PrinterLocalTime"));
        let q = ProfilePrinterModel::new(profile(r#","quirks":{"printer_local_time":true}"#));
        let s = q.element(&el::wprt("PrinterStatus")).unwrap();
        assert!(el::to_xml(&s).contains("PrinterLocalTime"));
        // strict validation (what replies must pass) rejects it; the profile-derived options allow it
        assert!(!validate_body(Target::Section("PrinterStatus"), &s).is_empty());
        let o = validator_options(&q.profile());
        assert!(crate::wsd::elements::schema::validate_body_with(
            Target::Section("PrinterStatus"),
            &s,
            &o
        )
        .is_empty());
    }

    #[test]
    fn regression_bins_follow_sequence_order_and_name_domain() {
        let m = ProfilePrinterModel::new(profile(""));
        let cfg = m.element(&el::wprt("PrinterConfiguration")).unwrap();
        let bins = cfg.child(ns::PRINT, "InputBins").unwrap();
        let entry = &bins.children[0];
        let order: Vec<_> = entry
            .children
            .iter()
            .map(|c| c.name.local.as_str())
            .collect();
        assert!(!order.contains(&"Installed"), "{order:?}");
        assert_eq!(
            order,
            [
                "FeedDirection",
                "MediaSize",
                "MediaType",
                "MediaColor",
                "Capacity",
                "Level"
            ]
        );
        let out = cfg.child(ns::PRINT, "OutputBins").unwrap();
        for e in bins.children.iter().chain(out.children.iter()) {
            let name = e.attr_local("Name").unwrap();
            let ty = if e.name.local == "InputBinEntry" {
                "InputBinNameType"
            } else {
                "OutputBinNameType"
            };
            assert!(
                crate::wsd::elements::schema::check_value(ty, name).is_ok(),
                "{name}"
            );
        }
        assert!(validate_body(Target::Section("PrinterConfiguration"), &cfg).is_empty());
        // and the old names are refused when a profile tries them
        assert!(PrinterProfile::from_json(
            r#"{"schema_version":2,"id":"x","label":"x","outputs":[{"name":"OutputBin1"}]}"#
        )
        .is_err());
    }
}
