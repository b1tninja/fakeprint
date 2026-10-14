//! The device registry (`devices.json`): schema types, capability fingerprints and the store.
//!
//! docs/multi-device.md section 4: the serde schema, the fingerprint functions, the locked atomic
//! [`RegistryStore::update`] and the install matching rules ([`match_printer`], section 3.4).
//!
//! Unknown fields survive a load/save round trip (every struct carries a flattened `extra` map),
//! and the legacy single-row file (`added.pnp`) loads unchanged.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use uuid::Uuid;

use crate::profile::PrinterProfile;
use crate::wsd::printer_model;

/// `%LOCALAPPDATA%\fakeprint\devices.json`; `FAKEPRINT_REGISTRY` overrides it (tests and CI).
pub fn registry_path() -> PathBuf {
    match std::env::var_os("FAKEPRINT_REGISTRY") {
        Some(p) if !p.is_empty() => PathBuf::from(p),
        _ => crate::lab::default_registry_path(),
    }
}

// ---------------------------------------------------------------------------------------------
// Schema (section 4.2)
// ---------------------------------------------------------------------------------------------

/// Install state of one printer (section 4.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InstallState {
    /// Never contacted by a peer.
    #[default]
    New,
    /// A peer contacted it, but no Windows printer is associated.
    Probed,
    /// A Windows printer matching by UUID (or IPP URL) was reported.
    Installed,
    /// Was installed; the published capabilities changed or Windows no longer lists it.
    Stale,
}

/// How the Windows printer reaches the emulated one (section 3.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InstallKind {
    Wsd,
    Ipp,
}

/// A printer as the Windows side reports it (input to `installed` / `reconcile`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowsPrinter {
    #[serde(default)]
    pub name: String,
    /// Port name or URL, e.g. `WSD-00000000-0000-4000-8000-0000000000a1`.
    #[serde(default)]
    pub port: String,
    #[serde(default)]
    pub driver: String,
    /// Opaque id (PnP / spooler), informational.
    #[serde(default)]
    pub id: String,
    /// The printer's Location property. For a WSD printer Windows sets it to the device
    /// endpoint URL (`http://<host>:<port>/WebServices/Device`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub location: String,
    /// The device's WSD UUID as Windows records it: a WSD port is named `WSD-<random guid>`, and
    /// the UUID is the value `Printer UUID` under
    /// `HKLM\SYSTEM\CurrentControlSet\Control\Print\Monitors\WSD Port\Ports\<port>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wsd_uuid: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// The `install` block of a registry row and of `DeviceInfo` (section 4.2).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Install {
    #[serde(default)]
    pub state: InstallState,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
    /// RFC 3339 UTC timestamps.
    #[serde(default)]
    pub first_probed_at: Option<String>,
    #[serde(default)]
    pub last_seen_at: Option<String>,
    #[serde(default)]
    pub last_peer: Option<String>,
    #[serde(default)]
    pub installed_at: Option<String>,
    #[serde(default)]
    pub verified_at: Option<String>,
    #[serde(default)]
    pub kind: Option<InstallKind>,
    #[serde(default)]
    pub windows: WindowsPrinter,
    #[serde(default)]
    pub published_fingerprint_at_install: Option<String>,
    #[serde(default)]
    pub stale_from: Option<String>,
    #[serde(default)]
    pub stale_at: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Install {
    /// `stale` for a profile change (section 4.4): the Add must be repeated.
    pub fn needs_readd(&self) -> bool {
        self.state == InstallState::Stale
            && self
                .reason
                .as_deref()
                .is_some_and(|r| !is_offline_reason(r))
    }
}

/// Reason text of a stale device that is merely offline (not an Add request).
pub const REASON_NOT_LISTED: &str = "not listed by Windows";
/// Reason prefix of a stale device whose published capabilities changed.
pub const REASON_PROFILE_CHANGED: &str = "profile changed";

/// True for the `not listed by Windows` reason (offline, not a request to Add).
pub fn is_offline_reason(reason: &str) -> bool {
    reason == REASON_NOT_LISTED
}

/// What the profile publishes, in lists (section 2.2 `config.summary`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigSummary {
    #[serde(default)]
    pub formats: Vec<String>,
    #[serde(default)]
    pub color_modes: Vec<String>,
    #[serde(default)]
    pub sides: Vec<String>,
    #[serde(default)]
    pub media_sizes: Vec<String>,
    #[serde(default)]
    pub input_bins: Vec<String>,
    #[serde(default)]
    pub versions: BTreeMap<String, bool>,
}

/// Fingerprints and summary of a printer's configuration (registry `config` and
/// `DeviceInfo.config`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigInfo {
    /// Hash of the whole effective profile (informational, never used for staleness).
    #[serde(default)]
    pub fingerprint: String,
    /// [`capability_fingerprint`]: what a Windows client caches at Add time.
    #[serde(default)]
    pub published_fingerprint: String,
    /// First 12 hex digits of the published fingerprint (`DeviceInfo` only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub class: Option<String>,
    #[serde(default)]
    pub summary: ConfigSummary,
}

impl ConfigInfo {
    /// Compute the block for a profile as published under `friendly_name`.
    pub fn for_profile(profile: &PrinterProfile, friendly_name: &str) -> Self {
        let published = capability_fingerprint(profile, friendly_name);
        Self {
            fingerprint: config_fingerprint(profile),
            class: Some(capability_class(&published)),
            published_fingerprint: published,
            summary: config_summary(profile),
        }
    }
}

/// `added` mirror of the legacy single-row file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Added {
    #[serde(default)]
    pub pnp: bool,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// One row: one emulated printer. Rows with the same `listen` belong to one process.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RegistryDevice {
    /// Device id (slug).
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub listen: String,
    #[serde(default)]
    pub advertise: String,
    #[serde(default)]
    pub device_uuid: Option<Uuid>,
    /// Explicit only; otherwise derived.
    #[serde(default)]
    pub ipp_uuid: Option<Uuid>,
    #[serde(default)]
    pub profile: String,
    #[serde(default)]
    pub friendly_name: Option<String>,
    /// Multicast Hello/Probe for this printer. Absent (`None`) = inherit the daemon's
    /// `--discovery` flag; only written when set explicitly for this printer (it used to be
    /// persisted as the inherited value, which then overrode later runs). A stored `false`
    /// without `discovery_explicit` is such a stale inherited value and means "inherit".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discovery: Option<bool>,
    /// The `discovery` value was set explicitly for this printer (a stored `false` wins).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub discovery_explicit: bool,
    #[serde(default)]
    pub jetdirect_port: Option<u16>,
    #[serde(default)]
    pub exe: String,
    #[serde(default)]
    pub autostart: bool,
    /// Runtime state applied after start (faults / timing / levels).
    #[serde(default)]
    pub harness: Option<Value>,
    #[serde(default)]
    pub added: Added,
    #[serde(default)]
    pub config: ConfigInfo,
    #[serde(default)]
    pub install: Install,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// `port_policy` of the registry (fakeprint = odd ports).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortPolicy {
    pub base: u16,
    pub step: u16,
}

impl Default for PortPolicy {
    fn default() -> Self {
        Self {
            base: crate::lab::LAB_PORT,
            step: 2,
        }
    }
}

/// The whole `devices.json` document.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Registry {
    /// LAN IP used when a row has no `advertise`.
    #[serde(default)]
    pub advertise_default: String,
    #[serde(default)]
    pub port_policy: PortPolicy,
    /// Optional explicit install order (device ids).
    #[serde(default)]
    pub queue_order: Vec<String>,
    #[serde(default)]
    pub devices: Vec<RegistryDevice>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Registry {
    /// Parse a registry document. The legacy single-object file (one row, no `devices` array)
    /// is accepted as a registry of that one row.
    pub fn parse(text: &str) -> anyhow::Result<Self> {
        let v: Value = serde_json::from_str(text)?;
        if v.get("devices").is_none() && v.get("listen").is_some() {
            let row: RegistryDevice = serde_json::from_value(v)?;
            return Ok(Self {
                devices: vec![row],
                ..Self::default()
            });
        }
        Ok(serde_json::from_value(v)?)
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_default()
    }

    pub fn device(&self, name: &str) -> Option<&RegistryDevice> {
        self.devices.iter().find(|d| d.name == name)
    }
}

/// `POST /v1/reconcile` result (section 3.4): what the Windows listing means for the registry.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ReconcileResult {
    /// Device ids newly or still `installed`.
    #[serde(default)]
    pub installed: Vec<String>,
    /// Device ids marked stale because Windows no longer lists them (offline).
    #[serde(default)]
    pub stale: Vec<String>,
    /// Windows printers that match no registry device (never touched).
    #[serde(default)]
    pub foreign: Vec<WindowsPrinter>,
}

// ---------------------------------------------------------------------------------------------
// Fingerprints (section 4.3)
// ---------------------------------------------------------------------------------------------

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Hash over what a Windows client caches at Add time: the model's published fingerprint
/// (description, configuration, default ticket, capabilities, version sections; levels zeroed)
/// plus the identity Windows binds to (`friendly_name`, manufacturer, model, IEEE 1284 id).
/// Status, conditions, levels, faults, timing, firmware, location and serial are excluded.
/// Format `fnv64:<16 hex>`.
pub fn capability_fingerprint(profile: &PrinterProfile, friendly_name: &str) -> String {
    let base = printer_model::published_fingerprint(profile);
    let text = format!(
        "{base}\nname={friendly_name}\nmanufacturer={}\nmodel={}\ndevice_id={}\n",
        profile.identity.manufacturer,
        profile.identity.model,
        profile.identity.device_id.as_deref().unwrap_or("")
    );
    format!("fnv64:{:016x}", fnv1a64(text.as_bytes()))
}

/// Hash of the whole effective profile (canonical key order), runtime knobs included.
/// Informational; never makes a device stale.
pub fn config_fingerprint(profile: &PrinterProfile) -> String {
    // `serde_json::Value` objects are key-sorted, which makes the text canonical.
    let text = serde_json::to_value(profile)
        .map(|v| v.to_string())
        .unwrap_or_default();
    format!("fnv64:{:016x}", fnv1a64(text.as_bytes()))
}

/// The capability class handle: the first 12 hex digits of a published fingerprint.
pub fn capability_class(published_fingerprint: &str) -> String {
    published_fingerprint
        .strip_prefix("fnv64:")
        .unwrap_or(published_fingerprint)
        .chars()
        .take(12)
        .collect()
}

/// The `config.summary` lists of a profile.
pub fn config_summary(profile: &PrinterProfile) -> ConfigSummary {
    let inputs: Vec<String> = match &profile.inputs {
        Some(list) => list.iter().map(|b| b.name.clone()).collect(),
        None => vec!["Tray1".to_string()],
    };
    ConfigSummary {
        formats: profile.documents.formats.clone(),
        color_modes: profile.description.color_modes.clone(),
        sides: profile.print.sides.clone(),
        media_sizes: profile.print.media_sizes.clone(),
        input_bins: inputs,
        versions: BTreeMap::from([
            ("v11".to_string(), profile.versions.v11),
            ("v12".to_string(), profile.versions.v12),
            ("v20".to_string(), profile.versions.v20),
        ]),
    }
}

// ---------------------------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------------------------

/// The registry file (or an in-memory registry for embedded servers). M0: read-only.
#[derive(Debug, Clone)]
pub struct RegistryStore {
    path: Option<PathBuf>,
}

impl RegistryStore {
    /// A store backed by `path`.
    pub fn open(path: impl Into<PathBuf>) -> Self {
        Self {
            path: Some(path.into()),
        }
    }

    /// The in-memory store of an embedded server.
    pub fn in_memory() -> Self {
        Self { path: None }
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Read the registry (missing file: empty registry).
    pub fn load(&self) -> anyhow::Result<Registry> {
        let Some(p) = &self.path else {
            return Ok(Registry::default());
        };
        match std::fs::read_to_string(p) {
            Ok(t) => Registry::parse(&t),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Registry::default()),
            Err(e) => Err(e.into()),
        }
    }

    /// Locked read-modify-write with an atomic replace (section 4.1): an in-process mutex, an OS
    /// file lock on `<file>.lock`, a fresh read, the mutation, then a temp file in the same
    /// directory, fsync and rename. In-memory stores just run the mutation on a scratch copy.
    /// If `mutate` fails nothing is written.
    pub fn update<F>(&self, mutate: F) -> anyhow::Result<Registry>
    where
        F: FnOnce(&mut Registry) -> anyhow::Result<()>,
    {
        use anyhow::Context;
        let Some(path) = &self.path else {
            let mut reg = Registry::default();
            mutate(&mut reg)?;
            return Ok(reg);
        };
        static PROCESS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = PROCESS_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = path
            .parent()
            .filter(|d| !d.as_os_str().is_empty())
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let mut lock_name = path.file_name().unwrap_or_default().to_os_string();
        lock_name.push(".lock");
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.join(lock_name))
            .context("opening the registry lock file")?;
        lock.lock().context("locking the registry")?;
        let result = (|| {
            let mut reg = self.load()?;
            mutate(&mut reg)?;
            let mut tmp_name = path.file_name().unwrap_or_default().to_os_string();
            tmp_name.push(format!(".{}.tmp", std::process::id()));
            let tmp = dir.join(tmp_name);
            {
                use std::io::Write;
                let mut f = std::fs::File::create(&tmp)?;
                f.write_all(reg.to_json().as_bytes())?;
                f.sync_all()?;
            }
            std::fs::rename(&tmp, path).inspect_err(|_| {
                let _ = std::fs::remove_file(&tmp);
            })?;
            Ok(reg)
        })();
        let _ = lock.unlock();
        result
    }
}

// ---------------------------------------------------------------------------------------------
// Time
// ---------------------------------------------------------------------------------------------

/// Now as RFC 3339 UTC (`2026-10-13T08:05:09Z`).
pub fn now_rfc3339() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    rfc3339_from_unix(secs)
}

/// Unix seconds as RFC 3339 UTC.
pub fn rfc3339_from_unix(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // civil-from-days (Howard Hinnant)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

// ---------------------------------------------------------------------------------------------
// Install matching (section 3.4)
// ---------------------------------------------------------------------------------------------

/// What a device exposes for matching a Windows printer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub id: String,
    pub device_uuid: Uuid,
    /// `host:port` the server advertises.
    pub authority: String,
    /// Path of the device's IPP endpoint (`/ipp/print` or `/d/<id>/ipp/print`).
    pub ipp_path: String,
    /// Path of the device's WSD endpoint (`/WebServices/Device` or `/d/<id>/WebServices/Device`).
    pub device_path: String,
}

/// The outcome of matching one Windows printer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrinterMatch {
    /// The port carries exactly this device's WSD UUID.
    Wsd(String),
    /// The port is this device's IPP URL.
    Ipp(String),
    /// Anything else; never touched.
    Foreign(String),
}

/// Every distinct UUID (8-4-4-4-12 hex, any case) occurring in `text`, in order of appearance.
pub fn port_uuids(text: &str) -> Vec<Uuid> {
    let b = text.as_bytes();
    let mut out: Vec<Uuid> = Vec::new();
    let mut i = 0;
    while i + 36 <= b.len() {
        let w = &b[i..i + 36];
        let ok = w.iter().enumerate().all(|(k, c)| {
            if matches!(k, 8 | 13 | 18 | 23) {
                *c == b'-'
            } else {
                c.is_ascii_hexdigit()
            }
        });
        if ok {
            if let Some(u) = std::str::from_utf8(w)
                .ok()
                .and_then(|t| Uuid::parse_str(t).ok())
            {
                if !out.contains(&u) {
                    out.push(u);
                }
            }
            i += 36;
        } else {
            i += 1;
        }
    }
    out
}

/// Parse a UUID leniently (`{..}`, `urn:uuid:..`, any case).
fn parse_uuid_lenient(s: &str) -> Option<Uuid> {
    let t = s.trim().trim_matches(|c| c == '{' || c == '}');
    let t = t.strip_prefix("urn:uuid:").unwrap_or(t);
    Uuid::parse_str(t).ok()
}

/// `http://host:port/path` -> (`host:port` lower case, path without a trailing slash).
fn split_url(url: &str) -> Option<(String, String)> {
    let url = url.trim();
    for scheme in ["http://", "https://", "ipp://", "ipps://"] {
        let has = url
            .get(..scheme.len())
            .is_some_and(|h| h.eq_ignore_ascii_case(scheme));
        if has {
            let rest = &url[scheme.len()..];
            let (auth, path) = match rest.find('/') {
                Some(i) => (&rest[..i], &rest[i..]),
                None => (rest, "/"),
            };
            let path = path.split(['?', '#']).next().unwrap_or("/");
            return Some((
                auth.to_ascii_lowercase(),
                path.trim_end_matches('/').to_string(),
            ));
        }
    }
    None
}

/// Match a Windows printer to a device, never by name (section 3.4). Evidence, any of which
/// identifies a device and all of which must agree:
///
/// * the printer's recorded WSD UUID (`wsd_uuid`, the `Printer UUID` of its WSD port);
/// * the single UUID in the port string (`WSD-<uuid>`; Windows itself names a WSD port
///   `WSD-<random guid>`, which matches nothing and is then ignored);
/// * the `Location` equal to the device's endpoint URL on this server's host:port;
/// * the port being exactly the device's IPP URL.
///
/// Two devices named by different evidence, a port naming several UUIDs, or an explicit
/// `wsd_uuid` that is not one of ours make the printer foreign.
pub fn match_printer(p: &WindowsPrinter, candidates: &[Candidate]) -> PrinterMatch {
    let uuids = port_uuids(&p.port);
    if uuids.len() > 1 {
        return PrinterMatch::Foreign("the port names more than one UUID".into());
    }
    let find_uuid = |u: &Uuid| candidates.iter().find(|c| c.device_uuid == *u);
    let mut ids: Vec<&str> = Vec::new();
    if let Some(c) = uuids.first().and_then(find_uuid) {
        ids.push(&c.id);
    }
    if let Some(w) = p.wsd_uuid.as_deref().filter(|w| !w.trim().is_empty()) {
        match parse_uuid_lenient(w).as_ref().and_then(find_uuid) {
            Some(c) => ids.push(&c.id),
            None => {
                return PrinterMatch::Foreign(
                    "its recorded WSD UUID is not a fakeprint device".into(),
                )
            }
        }
    }
    if let Some((auth, path)) = split_url(&p.location) {
        if let Some(c) = candidates.iter().find(|c| {
            c.authority.eq_ignore_ascii_case(&auth) && c.device_path.eq_ignore_ascii_case(&path)
        }) {
            ids.push(&c.id);
        }
    }
    ids.dedup();
    let mut distinct: Vec<&str> = Vec::new();
    for i in ids {
        if !distinct.contains(&i) {
            distinct.push(i);
        }
    }
    match distinct.as_slice() {
        [one] => return PrinterMatch::Wsd((*one).to_string()),
        [] => {}
        _ => return PrinterMatch::Foreign("its identifiers name different devices".into()),
    }
    if let Some((auth, path)) = split_url(&p.port) {
        if let Some(c) = candidates.iter().find(|c| {
            c.authority.eq_ignore_ascii_case(&auth) && c.ipp_path.eq_ignore_ascii_case(&path)
        }) {
            return PrinterMatch::Ipp(c.id.clone());
        }
    }
    PrinterMatch::Foreign("matches no fakeprint device by UUID or IPP URL".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_single_row_loads_and_unknown_fields_survive() {
        let text = r#"{"listen":"0.0.0.0:3921","name":"default","added":{"pnp":true,"x":1},"mystery":[1]}"#;
        let reg = Registry::parse(text).unwrap();
        assert_eq!(reg.devices.len(), 1);
        assert!(reg.devices[0].added.pnp);
        let out = serde_json::to_value(&reg).unwrap();
        assert_eq!(out["devices"][0]["mystery"], serde_json::json!([1]));
        assert_eq!(out["devices"][0]["added"]["x"], 1);
    }

    #[test]
    fn install_defaults_and_states_serialize_lowercase() {
        let i = Install::default();
        assert_eq!(serde_json::to_value(&i).unwrap()["state"], "new");
        assert!(!i.needs_readd());
        let stale = Install {
            state: InstallState::Stale,
            reason: Some("profile changed: formats".into()),
            ..Install::default()
        };
        assert!(stale.needs_readd());
        let offline = Install {
            state: InstallState::Stale,
            reason: Some(REASON_NOT_LISTED.into()),
            ..Install::default()
        };
        assert!(!offline.needs_readd());
    }

    #[test]
    fn fingerprint_ignores_levels_but_not_name() {
        let p = PrinterProfile::generic();
        let a = capability_fingerprint(&p, "fakeprint a");
        assert_eq!(a, capability_fingerprint(&p, "fakeprint a"));
        assert_ne!(a, capability_fingerprint(&p, "fakeprint b"));
        assert!(a.starts_with("fnv64:"));
        assert_eq!(capability_class(&a).len(), 12);
    }

    fn cand(id: &str, uuid: &str, path: &str) -> Candidate {
        Candidate {
            id: id.into(),
            device_uuid: Uuid::parse_str(uuid).unwrap(),
            authority: "192.168.1.5:3921".into(),
            ipp_path: path.into(),
            device_path: path.replace("/ipp/print", "/WebServices/Device"),
        }
    }

    fn win(port: &str) -> WindowsPrinter {
        WindowsPrinter {
            name: "whatever".into(),
            port: port.into(),
            ..WindowsPrinter::default()
        }
    }

    #[test]
    fn matching_rules() {
        let a = "00000000-0000-4000-8000-0000000000a1";
        let b = "00000000-0000-4000-8000-0000000000b1";
        let cs = [cand("a", a, "/ipp/print"), cand("b", b, "/d/b/ipp/print")];
        assert_eq!(
            match_printer(&win(&format!("WSD-{a}")), &cs),
            PrinterMatch::Wsd("a".into())
        );
        assert_eq!(
            match_printer(&win(&format!("WSD-{}", b.to_uppercase())), &cs),
            PrinterMatch::Wsd("b".into())
        );
        assert_eq!(
            match_printer(&win("http://192.168.1.5:3921/d/b/ipp/print"), &cs),
            PrinterMatch::Ipp("b".into())
        );
        assert_eq!(
            match_printer(&win("ipp://192.168.1.5:3921/ipp/print/"), &cs),
            PrinterMatch::Ipp("a".into())
        );
        // foreign: TCPMON, other host, other path, two UUIDs, unknown UUID, look-alike name
        for port in [
            "192.168.1.5:9100".to_string(),
            "http://192.168.1.9:3921/ipp/print".to_string(),
            "http://192.168.1.5:3921/ipp/print2".to_string(),
            format!("WSD-{a}-WSD-{b}"),
            "WSD-00000000-0000-4000-8000-0000000000c1".to_string(),
            "fakeprint a".to_string(),
        ] {
            assert!(
                matches!(match_printer(&win(&port), &cs), PrinterMatch::Foreign(_)),
                "{port}"
            );
        }
    }

    #[test]
    fn windows_wsd_ports_match_by_recorded_uuid_or_location() {
        let a = "00000000-0000-4000-8000-0000000000a1";
        let b = "00000000-0000-4000-8000-0000000000b1";
        let cs = [cand("a", a, "/ipp/print"), cand("b", b, "/d/b/ipp/print")];
        let random_port = "WSD-3f2b9c1e-7d44-4a55-9e0b-1c2d3e4f5a6b";
        // Windows: random port guid, the device UUID only in the registry value
        let mut p = win(random_port);
        p.wsd_uuid = Some(a.to_uppercase());
        assert_eq!(match_printer(&p, &cs), PrinterMatch::Wsd("a".into()));
        p.wsd_uuid = Some(format!("urn:uuid:{b}"));
        assert_eq!(match_printer(&p, &cs), PrinterMatch::Wsd("b".into()));
        // a wrong / foreign UUID never matches, even with a matching Location
        p.wsd_uuid = Some("11111111-2222-4333-8444-555555555555".into());
        p.location = "http://192.168.1.5:3921/WebServices/Device".into();
        assert!(matches!(match_printer(&p, &cs), PrinterMatch::Foreign(_)));
        // no UUID at all: the Location is the device endpoint URL
        let mut p = win(random_port);
        p.location = "http://192.168.1.5:3921/d/b/WebServices/Device/".into();
        assert_eq!(match_printer(&p, &cs), PrinterMatch::Wsd("b".into()));
        p.location = "http://192.168.1.5:3921/WebServices/Device".into();
        assert_eq!(match_printer(&p, &cs), PrinterMatch::Wsd("a".into()));
        // Location on another host or path does not match
        for loc in [
            "http://192.168.1.9:3921/WebServices/Device",
            "http://192.168.1.5:3921/WebServices/Other",
            "http://192.168.1.5:3923/WebServices/Device",
            "fakeprint a",
            "",
        ] {
            let mut p = win(random_port);
            p.location = loc.into();
            assert!(
                matches!(match_printer(&p, &cs), PrinterMatch::Foreign(_)),
                "{loc}"
            );
        }
        // disagreeing evidence is ambiguous: UUID of a, Location of b
        let mut p = win(random_port);
        p.wsd_uuid = Some(a.into());
        p.location = "http://192.168.1.5:3921/d/b/WebServices/Device".into();
        assert!(matches!(match_printer(&p, &cs), PrinterMatch::Foreign(_)));
        // a look-alike name with nothing else is never a match
        let mut p = win("USB001");
        p.name = "fakeprint a".into();
        assert!(matches!(match_printer(&p, &cs), PrinterMatch::Foreign(_)));
        // TCPMON stays foreign even with a bogus uuid-less location
        let mut p = win("192.168.1.5:9100");
        p.location = "http://192.168.9.9:3921/WebServices/Device".into();
        assert!(matches!(match_printer(&p, &cs), PrinterMatch::Foreign(_)));
    }

    #[test]
    fn rfc3339_formats_known_instants() {
        assert_eq!(rfc3339_from_unix(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339_from_unix(1_782_864_000), "2026-07-01T00:00:00Z");
        assert_eq!(
            rfc3339_from_unix(951_782_400 + 86_399),
            "2000-02-29T23:59:59Z"
        );
    }

    #[test]
    fn update_is_atomic_preserves_unknown_fields_and_loses_no_rows() {
        let dir = std::env::temp_dir().join(format!("fakeprint-reg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("devices.json");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            &path,
            r#"{"future":{"x":1},"devices":[{"name":"old","listen":"x","extra_row":true}]}"#,
        )
        .unwrap();
        let store = RegistryStore::open(&path);
        let threads: Vec<_> = (0..8)
            .map(|i| {
                let s = store.clone();
                std::thread::spawn(move || {
                    s.update(|r| {
                        r.devices.push(RegistryDevice {
                            name: format!("d{i}"),
                            ..RegistryDevice::default()
                        });
                        Ok(())
                    })
                    .unwrap();
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        let reg = store.load().unwrap();
        assert_eq!(reg.devices.len(), 9, "no rows lost");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"future\"") && text.contains("extra_row"));
        // a failing mutation writes nothing
        assert!(store
            .update(|r| {
                r.devices.clear();
                anyhow::bail!("no")
            })
            .is_err());
        assert_eq!(store.load().unwrap().devices.len(), 9);
        let leftovers = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .count();
        assert_eq!(leftovers, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_registry_file_is_empty() {
        let s = RegistryStore::open(std::env::temp_dir().join("fakeprint-no-such-registry.json"));
        assert!(s.load().unwrap().devices.is_empty());
    }
}
