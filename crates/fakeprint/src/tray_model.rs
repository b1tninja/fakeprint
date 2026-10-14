//! Pure tray model: icon level, tooltip, menu tree, action ids and the notification detector,
//! computed from the `GET /v1/devices` / `GET /v1/queue` JSON (docs/multi-device.md 5.2, 5.3).
//!
//! No platform code and no I/O: the shell ([`crate::tray`]) only polls, draws [`MenuModel`], and
//! runs [`TrayAction::request`] over loopback HTTP, so every decision is unit-tested here on every
//! platform. Everything that originates from a client (job names, friendly names, reasons) goes
//! through [`clean`] before it reaches a label or a notification.
//!
//! Menu item ids (the "action id table"):
//!
//! | id | action |
//! |----|--------|
//! | `dev:<id>:fault:<paper_out\|toner_low\|cover_open\|paper_jam\|offline>:<1\|0>` | set that fault on/off (the id carries the resulting value) |
//! | `dev:<id>:timing:<instant\|realistic\|default>` | timing preset |
//! | `dev:<id>:reset` / `dev:<id>:inbox` / `dev:<id>:remove` | reset, open the reported inbox folder, remove (confirm child) |
//! | `add:<profile>:<suggested_id>` | add a printer of that profile |
//! | `ui` / `settings` / `queue:advance` / `queue:skip` / `quit` | open the web UI, Settings, move the queue, quit |
//! | `sub:*`, `info:*`, `sep` | not actions ([`MenuId::parse`] returns `None`) |

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use base64::Engine as _;
use serde_json::Value;

use crate::manager::{DeviceInfo, QueueInfo};

/// Minimum gap between two notifications of the same kind for the same device.
pub const NOTIFY_INTERVAL: Duration = Duration::from_secs(600);
/// A `profile changed` stale must persist this long before it is treated as an Add request.
pub const PROFILE_STALE_GRACE: Duration = Duration::from_secs(20);
/// A printer must stay offline this long before a notification is raised.
pub const OFFLINE_CONFIRM: Duration = Duration::from_secs(6);
/// Minimum gap between two "job completed" notifications of one printer.
pub const JOB_NOTIFY_INTERVAL: Duration = Duration::from_secs(30);

/// Windows truncates tooltips at 127 UTF-16 units; stay below.
const TOOLTIP_MAX: usize = 120;
/// Longest client-supplied text shown in a label.
const LABEL_MAX: usize = 48;
/// At most this many profiles are offered under "Add printer".
const PROFILE_MENU_MAX: usize = 40;

// ---------------------------------------------------------------------------------------------
// Text helpers
// ---------------------------------------------------------------------------------------------

/// Make client-supplied text safe for a label or balloon: control characters (and bidi
/// overrides) become spaces, runs of whitespace collapse, the length is capped with an ellipsis.
pub fn clean(s: &str, max: usize) -> String {
    let mut out = String::new();
    let mut last_space = true;
    for ch in s.chars() {
        let bad =
            ch.is_control() || matches!(ch, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}');
        let ch = if bad || ch.is_whitespace() { ' ' } else { ch };
        if ch == ' ' {
            if !last_space {
                out.push(' ');
            }
            last_space = true;
        } else {
            out.push(ch);
            last_space = false;
        }
    }
    truncate(out.trim_end(), max)
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('\u{2026}');
    out
}

/// Escape a label for a Win32 menu, where `&` introduces a mnemonic.
pub fn win_label(s: &str) -> String {
    s.replace('&', "&&")
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

/// True for `[A-Za-z0-9_-]{1,32}` (the device id slug rule).
pub fn valid_device_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 32
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Profile ids: letters, digits, `-`, `_`, `.`; never a leading `.`; no `:` so ids stay parseable.
fn valid_profile_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && !id.starts_with('.')
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}

/// A folder path worth handing to the file manager: absolute, no control characters.
pub fn reportable_path(p: &str) -> bool {
    let b = p.as_bytes();
    if b.len() < 3 || p.chars().any(char::is_control) {
        return false;
    }
    let drive = b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/');
    drive || p.starts_with("\\\\") || p.starts_with('/')
}

/// Suggest a free device id for `profile`: `mono-laser`, `mono-laser-2`, ... (case-insensitive
/// against `existing`, never `default`, always a valid slug).
pub fn next_free_id(profile: &str, existing: &[String]) -> String {
    let slug: String = profile
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .take(26)
        .collect();
    let trimmed = slug.trim_matches('-');
    let base = if trimmed.is_empty() {
        "printer".to_string()
    } else {
        trimmed.to_string()
    };
    let taken: HashSet<String> = existing.iter().map(|e| e.to_ascii_lowercase()).collect();
    let free =
        |c: &str| !taken.contains(&c.to_ascii_lowercase()) && !c.eq_ignore_ascii_case("default");
    if free(&base) {
        return base;
    }
    (2u32..)
        .map(|n| format!("{base}-{n}"))
        .find(|c| free(c))
        .unwrap_or(base)
}

/// The Windows "Add" guidance in plain words (shared verbatim with the web UI and Python).
pub fn add_guidance(friendly_name: &str, position: usize, total: usize) -> String {
    let n = clean(friendly_name, 60);
    format!(
        "Add now: {n} ({position} of {total}). Open Settings > Bluetooth & devices > Printers & scanners > Add device, wait for \"{n}\" to appear and choose Add. Windows takes about 30 seconds; fakeprint announces the next printer only after this one is installed."
    )
}

/// The one-line queue label used in the menu.
pub fn add_now_line(friendly_name: &str, position: usize, total: usize) -> String {
    format!(
        "Add now: {} ({position} of {total})",
        clean(friendly_name, LABEL_MAX)
    )
}

/// A short menu-sized form of the guidance (a menu line cannot wrap).
pub const ADD_HINT_SHORT: &str =
    "In Windows: Settings > Printers & scanners > Add device (about 30 s)";

// ---------------------------------------------------------------------------------------------
// Inputs
// ---------------------------------------------------------------------------------------------

/// Icon colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// Every printer installed and no fault raised.
    Green,
    /// A printer needs an Add / Re-Add, is offline, waiting, or a fault is raised.
    Amber,
    /// The daemon could not be read.
    Red,
}

/// Install lifecycle (mirrors `registry::InstallState`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    New,
    Probed,
    Installed,
    Stale,
}

impl State {
    pub fn parse(s: &str) -> State {
        match s {
            "installed" => State::Installed,
            "probed" => State::Probed,
            "stale" => State::Stale,
            _ => State::New,
        }
    }
}

/// The tray's fault toggles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Fault {
    PaperOut,
    TonerLow,
    CoverOpen,
    PaperJam,
    /// `not_accepting_jobs` on the wire (Q4); never stops answering HTTP.
    Offline,
}

impl Fault {
    pub const ALL: [Fault; 5] = [
        Fault::PaperOut,
        Fault::TonerLow,
        Fault::CoverOpen,
        Fault::PaperJam,
        Fault::Offline,
    ];

    /// The token used inside menu ids.
    pub fn token(self) -> &'static str {
        match self {
            Fault::PaperOut => "paper_out",
            Fault::TonerLow => "toner_low",
            Fault::CoverOpen => "cover_open",
            Fault::PaperJam => "paper_jam",
            Fault::Offline => "offline",
        }
    }

    pub fn parse(s: &str) -> Option<Fault> {
        Fault::ALL.into_iter().find(|f| f.token() == s)
    }

    /// The `POST /d/{id}/v1/faults` field name.
    pub fn wire(self) -> &'static str {
        match self {
            Fault::Offline => "not_accepting_jobs",
            other => other.token(),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Fault::PaperOut => "Paper out",
            Fault::TonerLow => "Toner low",
            Fault::CoverOpen => "Door open",
            Fault::PaperJam => "Paper jam",
            Fault::Offline => "Offline",
        }
    }
}

/// `live.faults`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Faults {
    pub paper_out: bool,
    pub toner_low: bool,
    pub cover_open: bool,
    pub paper_jam: bool,
    pub offline: bool,
}

impl Faults {
    pub fn get(&self, f: Fault) -> bool {
        match f {
            Fault::PaperOut => self.paper_out,
            Fault::TonerLow => self.toner_low,
            Fault::CoverOpen => self.cover_open,
            Fault::PaperJam => self.paper_jam,
            Fault::Offline => self.offline,
        }
    }

    pub fn raised(&self) -> Vec<Fault> {
        Fault::ALL.into_iter().filter(|f| self.get(*f)).collect()
    }
}

/// Timing presets of `POST /d/{id}/v1/timing`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimingPreset {
    Instant,
    Realistic,
    Default,
}

impl TimingPreset {
    pub const ALL: [TimingPreset; 3] = [
        TimingPreset::Instant,
        TimingPreset::Realistic,
        TimingPreset::Default,
    ];

    pub fn token(self) -> &'static str {
        match self {
            TimingPreset::Instant => "instant",
            TimingPreset::Realistic => "realistic",
            TimingPreset::Default => "default",
        }
    }

    pub fn parse(s: &str) -> Option<TimingPreset> {
        TimingPreset::ALL.into_iter().find(|t| t.token() == s)
    }

    pub fn label(self) -> &'static str {
        match self {
            TimingPreset::Instant => "Instant",
            TimingPreset::Realistic => "Realistic",
            TimingPreset::Default => "Default",
        }
    }
}

/// A job as the tray needs it (`live.active_job`, `live.last_job`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobLine {
    pub id: u64,
    pub name: String,
    pub state: String,
}

impl JobLine {
    fn from_json(v: Option<&Value>) -> Option<JobLine> {
        let v = v?;
        Some(JobLine {
            id: v.get("id")?.as_u64()?,
            name: v.get("name").and_then(Value::as_str).unwrap_or("").into(),
            state: v.get("state").and_then(Value::as_str).unwrap_or("").into(),
        })
    }

    fn terminal(&self) -> bool {
        matches!(
            self.state.to_ascii_lowercase().as_str(),
            "completed" | "aborted" | "canceled" | "cancelled"
        )
    }
}

/// The fields of one printer the tray cares about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInput {
    pub id: String,
    pub friendly_name: String,
    pub state: State,
    pub needs_readd: bool,
    pub reason: Option<String>,
    /// How long the printer has been stale (from `install.stale_at`).
    pub stale_age: Option<Duration>,
    pub announcing: bool,
    pub queue_position: Option<usize>,
    pub inbox_dir: String,
    pub printer_state: String,
    pub active_jobs: usize,
    pub captured_jobs: usize,
    pub active_job: Option<JobLine>,
    pub last_job: Option<JobLine>,
    pub faults: Faults,
    /// `None` = `custom` (no radio selected).
    pub timing: Option<TimingPreset>,
}

impl DeviceInput {
    /// A minimal input (tests, defaults).
    pub fn new(id: &str, state: State) -> DeviceInput {
        DeviceInput {
            id: id.to_string(),
            friendly_name: id.to_string(),
            state,
            needs_readd: false,
            reason: None,
            stale_age: None,
            announcing: false,
            queue_position: None,
            inbox_dir: String::new(),
            printer_state: "idle".into(),
            active_jobs: 0,
            captured_jobs: 0,
            active_job: None,
            last_job: None,
            faults: Faults::default(),
            timing: Some(TimingPreset::Default),
        }
    }

    /// From one element of `GET /v1/devices`; `None` when `id` is missing or not a valid slug
    /// (ids end up in URLs, so a malformed one is dropped, never shown).
    pub fn from_json(v: &Value) -> Option<DeviceInput> {
        Self::from_json_at(v, now_unix())
    }

    /// [`DeviceInput::from_json`] with the current time injected (seconds since the epoch).
    pub fn from_json_at(v: &Value, now_unix: u64) -> Option<DeviceInput> {
        let id = v.get("id")?.as_str()?.to_string();
        if !valid_device_id(&id) {
            return None;
        }
        let s = |p: &str| v.pointer(p).and_then(Value::as_str);
        let b = |p: &str| v.pointer(p).and_then(Value::as_bool).unwrap_or(false);
        let n = |p: &str| {
            v.pointer(p)
                .and_then(Value::as_u64)
                .and_then(|n| usize::try_from(n).ok())
        };
        Some(DeviceInput {
            friendly_name: s("/friendly_name").unwrap_or(&id).to_string(),
            state: s("/install/state").map(State::parse).unwrap_or(State::New),
            needs_readd: b("/needs_readd"),
            reason: s("/needs_readd_reason")
                .or_else(|| s("/install/reason"))
                .map(str::to_string),
            stale_age: s("/install/stale_at").and_then(|t| age_of_iso(t, now_unix)),
            announcing: b("/announcing"),
            queue_position: n("/queue_position"),
            inbox_dir: s("/inbox_dir").unwrap_or("").to_string(),
            printer_state: s("/live/printer_state").unwrap_or("").to_ascii_lowercase(),
            active_jobs: n("/live/active_jobs").unwrap_or(0),
            captured_jobs: n("/live/captured_jobs").unwrap_or(0),
            active_job: JobLine::from_json(v.pointer("/live/active_job")),
            last_job: JobLine::from_json(v.pointer("/live/last_job")),
            faults: Faults {
                paper_out: b("/live/faults/paper_out"),
                toner_low: b("/live/faults/toner_low"),
                cover_open: b("/live/faults/cover_open"),
                paper_jam: b("/live/faults/paper_jam"),
                offline: b("/live/faults/offline"),
            },
            timing: s("/live/timing_preset").and_then(TimingPreset::parse),
            id,
        })
    }
}

impl From<&DeviceInfo> for DeviceInput {
    fn from(d: &DeviceInfo) -> Self {
        let job = |j: &Option<crate::manager::JobBrief>| {
            j.as_ref().map(|j| JobLine {
                id: u64::from(j.id),
                name: j.name.clone(),
                state: j.state.clone(),
            })
        };
        let state = match d.install.state {
            crate::registry::InstallState::New => State::New,
            crate::registry::InstallState::Probed => State::Probed,
            crate::registry::InstallState::Installed => State::Installed,
            crate::registry::InstallState::Stale => State::Stale,
        };
        DeviceInput {
            id: d.id.clone(),
            friendly_name: d.friendly_name.clone(),
            state,
            needs_readd: d.needs_readd,
            reason: d
                .needs_readd_reason
                .clone()
                .or_else(|| d.install.reason.clone()),
            stale_age: d
                .install
                .stale_at
                .as_deref()
                .and_then(|t| age_of_iso(t, now_unix())),
            announcing: d.announcing,
            queue_position: d.queue_position,
            inbox_dir: d.inbox_dir.clone(),
            printer_state: d.live.printer_state.to_ascii_lowercase(),
            active_jobs: d.live.active_jobs,
            captured_jobs: d.live.captured_jobs,
            active_job: job(&d.live.active_job),
            last_job: job(&d.live.last_job),
            faults: Faults {
                paper_out: d.live.faults.paper_out,
                toner_low: d.live.faults.toner_low,
                cover_open: d.live.faults.cover_open,
                paper_jam: d.live.faults.paper_jam,
                offline: d.live.faults.offline,
            },
            timing: TimingPreset::parse(&d.live.timing_preset),
        }
    }
}

/// The install queue as the tray shows it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QueueView {
    pub head: Option<String>,
    pub position: usize,
    pub total: usize,
}

impl QueueView {
    pub fn from_json(v: &Value) -> Option<QueueView> {
        if !v.is_object() {
            return None;
        }
        let head = v
            .get("head")
            .and_then(Value::as_str)
            .filter(|h| valid_device_id(h))
            .map(str::to_string);
        let num = |k: &str| {
            v.get(k)
                .and_then(Value::as_u64)
                .and_then(|n| usize::try_from(n).ok())
        };
        Some(QueueView {
            head,
            position: num("position").unwrap_or(1),
            total: num("total").unwrap_or(0),
        })
    }
}

impl From<&QueueInfo> for QueueView {
    fn from(q: &QueueInfo) -> Self {
        QueueView {
            head: q.head.clone(),
            position: q.position.unwrap_or(1),
            total: q.total,
        }
    }
}

/// A profile offered under "Add printer".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileChoice {
    pub id: String,
    pub label: String,
}

impl ProfileChoice {
    /// From the `GET /v1/profiles` array; entries without a usable id are skipped.
    pub fn list_from_json(v: &Value) -> Vec<ProfileChoice> {
        let Some(a) = v.as_array() else {
            return Vec::new();
        };
        a.iter()
            .filter_map(|p| {
                let id = p.get("id")?.as_str()?;
                if !valid_profile_id(id) {
                    return None;
                }
                let label = p
                    .get("description")
                    .or_else(|| p.get("label"))
                    .and_then(Value::as_str)
                    .filter(|l| !l.is_empty())
                    .unwrap_or(id);
                Some(ProfileChoice {
                    id: id.to_string(),
                    label: clean(label, LABEL_MAX),
                })
            })
            .take(PROFILE_MENU_MAX)
            .collect()
    }
}

// ---------------------------------------------------------------------------------------------
// Time
// ---------------------------------------------------------------------------------------------

/// Seconds since the Unix epoch for an RFC 3339 UTC time (`Z` or `+00:00`, fraction ignored).
fn parse_iso_unix(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' {
        return None;
    }
    let rest = s.get(19..)?;
    let rest = match rest.strip_prefix('.') {
        Some(r) => r.trim_start_matches(|c: char| c.is_ascii_digit()),
        None => rest,
    };
    if rest != "Z" && rest != "+00:00" {
        return None;
    }
    let n = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, m, d) = (n(0..4)?, n(5..7)?, n(8..10)?);
    let (hh, mm, ss) = (n(11..13)?, n(14..16)?, n(17..19)?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(days * 86_400 + hh * 3600 + mm * 60 + ss).ok()
}

/// Age of an RFC 3339 timestamp relative to `now_unix` (saturating at zero).
pub fn age_of_iso(iso: &str, now_unix: u64) -> Option<Duration> {
    parse_iso_unix(iso).map(|t| Duration::from_secs(now_unix.saturating_sub(t)))
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------------------------
// Summary
// ---------------------------------------------------------------------------------------------

/// Reason recorded when Windows no longer lists an installed printer: offline, not an Add request.
const OFFLINE_REASON: &str = "not listed by Windows";

fn is_offline(d: &DeviceInput) -> bool {
    d.state == State::Stale
        && d.reason
            .as_deref()
            .is_some_and(|r| r.trim_start().starts_with(OFFLINE_REASON))
}

/// Stale because the profile changed, for less than the grace period (unknown age = old).
fn is_settling(d: &DeviceInput) -> bool {
    d.state == State::Stale
        && d.reason
            .as_deref()
            .is_some_and(|r| r.trim_start().starts_with("profile changed"))
        && d.stale_age.is_some_and(|a| a < PROFILE_STALE_GRACE)
}

fn is_readd(d: &DeviceInput) -> bool {
    !is_offline(d) && !is_settling(d) && (d.needs_readd || d.state == State::Stale)
}

fn install_ok(d: &DeviceInput) -> bool {
    d.state == State::Installed && !d.needs_readd
}

/// Not the one the user should Add right now.
fn is_waiting(d: &DeviceInput) -> bool {
    d.queue_position.is_some_and(|p| p > 1)
}

/// How the install side of a printer reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Install {
    Installed,
    NeedsAdd,
    NeedsReadd,
    Waiting,
    Offline,
    Settling,
}

fn install_kind(d: &DeviceInput) -> Install {
    if install_ok(d) {
        Install::Installed
    } else if is_settling(d) {
        Install::Settling
    } else if is_offline(d) {
        Install::Offline
    } else if is_readd(d) {
        Install::NeedsReadd
    } else if is_waiting(d) {
        Install::Waiting
    } else {
        Install::NeedsAdd
    }
}

/// One printer as the summary and menu show it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceLine {
    pub id: String,
    /// The submenu header, e.g. `mono: installed`.
    pub text: String,
    pub install: Install,
    /// A human must Add / re-Add this one now (what notifications key on).
    pub needs_attention: bool,
    pub input: DeviceInput,
}

/// Counts shown in the tooltip.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Counts {
    pub total: usize,
    pub installed: usize,
    pub need_add: usize,
    pub need_readd: usize,
    /// Not installed and behind the queue head: nothing to do yet.
    pub waiting: usize,
    /// Registered, but Windows is not listing it: not a request to Add.
    pub offline: usize,
    /// Profile-change stale younger than [`PROFILE_STALE_GRACE`].
    pub settling: usize,
    /// Printers with at least one fault toggle raised.
    pub faulted: usize,
    /// Jobs currently being processed, over all printers.
    pub active_jobs: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Summary {
    pub level: Level,
    pub counts: Counts,
    pub devices: Vec<DeviceLine>,
    pub queue: Option<QueueView>,
    /// Set when `level` is Red.
    pub error: Option<String>,
}

fn status_text(d: &DeviceInput, kind: Install) -> String {
    match kind {
        Install::Installed => "installed".into(),
        Install::Settling => "profile changed, confirming".into(),
        Install::Offline => "offline".into(),
        Install::Waiting => "waiting in queue".into(),
        Install::NeedsAdd => "needs Add".into(),
        Install::NeedsReadd => match d.reason.as_deref().map(|r| clean(r, 60)) {
            Some(r) if !r.is_empty() => format!("needs re-Add ({r})"),
            _ => "needs re-Add".into(),
        },
    }
}

impl Summary {
    pub fn from_devices(devices: &[DeviceInput], queue: Option<&QueueInfo>) -> Summary {
        Self::build(devices, queue.map(QueueView::from))
    }

    pub fn build(devices: &[DeviceInput], queue: Option<QueueView>) -> Summary {
        let mut counts = Counts {
            total: devices.len(),
            ..Counts::default()
        };
        let mut lines = Vec::with_capacity(devices.len());
        for d in devices {
            let kind = install_kind(d);
            match kind {
                Install::Installed => counts.installed += 1,
                Install::Settling => counts.settling += 1,
                Install::Offline => counts.offline += 1,
                Install::NeedsReadd => counts.need_readd += 1,
                Install::Waiting => counts.waiting += 1,
                Install::NeedsAdd => counts.need_add += 1,
            }
            if !d.faults.raised().is_empty() {
                counts.faulted += 1;
            }
            counts.active_jobs += d.active_jobs;
            lines.push(DeviceLine {
                id: d.id.clone(),
                text: format!("{}: {}", d.id, status_text(d, kind)),
                install: kind,
                needs_attention: matches!(kind, Install::NeedsAdd | Install::NeedsReadd),
                input: d.clone(),
            });
        }
        let amber = counts.total - counts.installed + counts.faulted > 0;
        Summary {
            level: if amber { Level::Amber } else { Level::Green },
            counts,
            devices: lines,
            queue: queue.filter(|q| q.total > 0 || q.head.is_some()),
            error: None,
        }
    }

    /// The daemon could not be read.
    pub fn error(msg: impl Into<String>) -> Summary {
        Summary {
            level: Level::Red,
            counts: Counts::default(),
            devices: Vec::new(),
            queue: None,
            error: Some(clean(&msg.into(), 80)),
        }
    }

    pub fn icon_level(&self) -> Level {
        self.level
    }

    /// Parse the bodies of `GET /v1/devices` and `GET /v1/queue` (raw HTTP/1.x responses).
    /// Any problem with the device list is a Red summary; a bad queue is simply no queue.
    pub fn from_http_responses(devices: &[u8], queue: Option<&[u8]>) -> Summary {
        let body = match http_body(devices) {
            Ok(b) => b,
            Err(e) => return Summary::error(e),
        };
        let Value::Array(a) = body else {
            return Summary::error("unexpected /v1/devices body");
        };
        let now = now_unix();
        let devs: Vec<DeviceInput> = a
            .iter()
            .filter_map(|v| DeviceInput::from_json_at(v, now))
            .collect();
        let q = queue
            .and_then(|raw| http_body(raw).ok())
            .and_then(|v| QueueView::from_json(&v));
        Summary::build(&devs, q)
    }

    /// The tooltip (at most 120 characters).
    pub fn tooltip(&self) -> String {
        let c = &self.counts;
        let s = match self.level {
            Level::Red => format!(
                "fakeprint: error ({})",
                self.error.as_deref().unwrap_or("unknown")
            ),
            _ if c.total == 0 => "fakeprint: no printers".to_string(),
            _ => {
                let mut parts = vec![format!("{} installed", c.installed)];
                for (n, what) in [
                    (c.need_add, "need Add"),
                    (c.need_readd, "need re-Add"),
                    (c.waiting, "waiting"),
                    (c.offline, "offline"),
                    (c.settling, "settling"),
                ] {
                    if n > 0 {
                        parts.push(format!("{n} {what}"));
                    }
                }
                if c.active_jobs > 0 {
                    parts.push(format!(
                        "{} active job{}",
                        c.active_jobs,
                        plural(c.active_jobs)
                    ));
                }
                if c.faulted > 0 {
                    parts.push(format!("{} with faults", c.faulted));
                }
                format!(
                    "fakeprint: {} printer{}, {}",
                    c.total,
                    plural(c.total),
                    parts.join(", ")
                )
            }
        };
        truncate(&s, TOOLTIP_MAX)
    }

    /// The inbox path of a printer, only as the daemon reported it and only if it is usable.
    pub fn inbox_for(&self, id: &str) -> Option<&str> {
        self.devices
            .iter()
            .find(|d| d.id == id)
            .map(|d| d.input.inbox_dir.as_str())
            .filter(|p| reportable_path(p))
    }

    fn friendly_of(&self, id: &str) -> String {
        self.devices
            .iter()
            .find(|d| d.id == id)
            .map(|d| d.input.friendly_name.clone())
            .unwrap_or_else(|| id.to_string())
    }

    fn ids(&self) -> Vec<String> {
        self.devices.iter().map(|d| d.id.clone()).collect()
    }

    /// The menu (top to bottom as in docs/multi-device.md 5.3).
    pub fn menu(&self, profiles: &[ProfileChoice]) -> MenuModel {
        let mut items = Vec::new();
        if let Some(e) = &self.error {
            items.push(MenuItem::info(
                "info:error",
                format!("fakeprint is not reachable: {e}"),
            ));
        } else if self.devices.is_empty() {
            items.push(MenuItem::info("info:none", "No printers"));
        }
        for d in &self.devices {
            items.push(device_submenu(d));
        }
        if self.error.is_none() {
            items.push(MenuItem::separator());
            let existing = self.ids();
            let children: Vec<MenuItem> = if profiles.is_empty() {
                vec![MenuItem::info("info:noprofiles", "No profiles found")]
            } else {
                profiles
                    .iter()
                    .map(|p| {
                        let id = MenuId::add(&p.id, &next_free_id(&p.id, &existing));
                        MenuItem::action(id, format!("{} ({})", p.label, p.id))
                    })
                    .collect()
            };
            items.push(MenuItem::submenu("sub:add", "Add printer", children));
            if let Some(q) = &self.queue {
                if let Some(head) = &q.head {
                    items.push(MenuItem::info(
                        "info:queue",
                        add_now_line(&self.friendly_of(head), q.position.max(1), q.total.max(1)),
                    ));
                    items.push(MenuItem::info("info:queue-hint", ADD_HINT_SHORT));
                    items.push(MenuItem::action(MenuId::queue_skip(), "Skip this printer"));
                    items.push(MenuItem::action(MenuId::queue_advance(), "Advance queue"));
                }
            }
        }
        items.push(MenuItem::separator());
        items.push(MenuItem::action(MenuId::ui(), "Open device manager"));
        items.push(MenuItem::action(
            MenuId::settings(),
            "Open Printers & scanners settings",
        ));
        items.push(MenuItem::submenu(
            "sub:quit",
            "Quit fakeprint",
            vec![MenuItem::action(
                MenuId::quit(),
                "Yes, stop all printers and quit",
            )],
        ));
        MenuModel { items }
    }
}

/// Parse a raw HTTP/1.x response with a 200 status and a JSON body.
pub fn http_body(raw: &[u8]) -> Result<Value, String> {
    let Some(split) = raw.windows(4).position(|w| w == b"\r\n\r\n") else {
        return Err("no HTTP response".into());
    };
    let head = String::from_utf8_lossy(&raw[..split]);
    let status = head.lines().next().unwrap_or("");
    if status.split_whitespace().nth(1) != Some("200") {
        return Err(format!("HTTP {}", clean(status, 40)));
    }
    serde_json::from_slice::<Value>(&raw[split + 4..]).map_err(|e| format!("bad JSON: {e}"))
}

fn device_submenu(d: &DeviceLine) -> MenuItem {
    let i = &d.input;
    let id = &d.id;
    let mut ch: Vec<MenuItem> = Vec::new();
    let mut state = format!(
        "State: {}",
        if i.printer_state.is_empty() {
            "unknown"
        } else {
            &i.printer_state
        }
    );
    let raised: Vec<&str> = i.faults.raised().iter().map(|f| f.label()).collect();
    if !raised.is_empty() {
        state.push_str(&format!(" ({})", raised.join(", ")));
    }
    ch.push(MenuItem::info(format!("info:{id}:state"), state));
    ch.push(MenuItem::info(
        format!("info:{id}:jobs"),
        format!(
            "Jobs: {} active, {} captured",
            i.active_jobs, i.captured_jobs
        ),
    ));
    if let Some(j) = &i.active_job {
        ch.push(MenuItem::info(
            format!("info:{id}:active"),
            format!(
                "Active: {} ({})",
                clean(&j.name, LABEL_MAX),
                clean(&j.state, 20)
            ),
        ));
    }
    if d.needs_attention {
        ch.push(MenuItem::info(
            format!("info:{id}:guidance"),
            ADD_HINT_SHORT,
        ));
    }
    ch.push(MenuItem::separator());
    let mut inbox = MenuItem::action(MenuId::dev(id, "inbox"), "Open inbox folder");
    inbox.enabled = reportable_path(&i.inbox_dir);
    ch.push(inbox);
    ch.push(MenuItem::separator());
    for f in Fault::ALL {
        let on = i.faults.get(f);
        ch.push(MenuItem::check(MenuId::fault(id, f, !on), f.label(), on));
    }
    ch.push(MenuItem::separator());
    let timing: Vec<MenuItem> = TimingPreset::ALL
        .into_iter()
        .map(|t| MenuItem::check(MenuId::timing(id, t), t.label(), i.timing == Some(t)))
        .collect();
    ch.push(MenuItem::submenu(
        format!("sub:{id}:timing"),
        "Timing",
        timing,
    ));
    ch.push(MenuItem::action(MenuId::dev(id, "reset"), "Reset printer"));
    ch.push(MenuItem::submenu(
        format!("sub:{id}:remove"),
        "Remove printer...",
        vec![MenuItem::action(
            MenuId::dev(id, "remove"),
            format!("Yes, remove {id} (keeps its registry row)"),
        )],
    ));
    MenuItem::submenu(format!("sub:{id}"), d.text.clone(), ch)
}

// ---------------------------------------------------------------------------------------------
// Menu tree
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuId(pub String);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemKind {
    Item,
    Separator,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuItem {
    pub id: MenuId,
    pub kind: ItemKind,
    pub label: String,
    pub enabled: bool,
    /// `Some` for check / radio items.
    pub checked: Option<bool>,
    pub children: Vec<MenuItem>,
}

impl MenuItem {
    fn base(id: impl Into<String>, label: impl Into<String>) -> MenuItem {
        MenuItem {
            id: MenuId(id.into()),
            kind: ItemKind::Item,
            label: label.into(),
            enabled: true,
            checked: None,
            children: Vec::new(),
        }
    }
    fn action(id: MenuId, label: impl Into<String>) -> MenuItem {
        MenuItem::base(id.0, label)
    }
    fn info(id: impl Into<String>, label: impl Into<String>) -> MenuItem {
        MenuItem {
            enabled: false,
            ..MenuItem::base(id, label)
        }
    }
    fn check(id: MenuId, label: impl Into<String>, on: bool) -> MenuItem {
        MenuItem {
            checked: Some(on),
            ..MenuItem::base(id.0, label)
        }
    }
    fn submenu(
        id: impl Into<String>,
        label: impl Into<String>,
        children: Vec<MenuItem>,
    ) -> MenuItem {
        MenuItem {
            children,
            ..MenuItem::base(id, label)
        }
    }
    pub fn separator() -> MenuItem {
        MenuItem {
            kind: ItemKind::Separator,
            enabled: false,
            ..MenuItem::base("sep", "")
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MenuModel {
    pub items: Vec<MenuItem>,
}

impl MenuModel {
    /// Every item (depth first).
    pub fn walk(&self) -> Vec<&MenuItem> {
        fn go<'a>(items: &'a [MenuItem], out: &mut Vec<&'a MenuItem>) {
            for i in items {
                out.push(i);
                go(&i.children, out);
            }
        }
        let mut out = Vec::new();
        go(&self.items, &mut out);
        out
    }
}

impl MenuId {
    pub fn dev(id: &str, what: &str) -> MenuId {
        MenuId(format!("dev:{id}:{what}"))
    }
    pub fn fault(id: &str, f: Fault, on: bool) -> MenuId {
        MenuId(format!("dev:{id}:fault:{}:{}", f.token(), u8::from(on)))
    }
    pub fn timing(id: &str, t: TimingPreset) -> MenuId {
        MenuId(format!("dev:{id}:timing:{}", t.token()))
    }
    pub fn add(profile: &str, suggested: &str) -> MenuId {
        MenuId(format!("add:{profile}:{suggested}"))
    }
    pub fn ui() -> MenuId {
        MenuId("ui".into())
    }
    pub fn settings() -> MenuId {
        MenuId("settings".into())
    }
    pub fn queue_advance() -> MenuId {
        MenuId("queue:advance".into())
    }
    pub fn queue_skip() -> MenuId {
        MenuId("queue:skip".into())
    }
    pub fn quit() -> MenuId {
        MenuId("quit".into())
    }

    /// Decode an id; `None` for headers, info lines and anything malformed.
    pub fn parse(s: &str) -> Option<TrayAction> {
        let parts: Vec<&str> = s.split(':').collect();
        match parts.as_slice() {
            ["ui"] => Some(TrayAction::OpenUi),
            ["settings"] => Some(TrayAction::OpenSettings),
            ["quit"] => Some(TrayAction::Quit),
            ["queue", "advance"] => Some(TrayAction::AdvanceQueue),
            ["queue", "skip"] => Some(TrayAction::SkipQueue),
            ["add", profile, id] if valid_profile_id(profile) && valid_device_id(id) => {
                Some(TrayAction::AddDevice {
                    profile: (*profile).into(),
                    suggested_id: (*id).into(),
                })
            }
            ["dev", id, rest @ ..] if valid_device_id(id) => {
                let device = (*id).to_string();
                match rest {
                    ["reset"] => Some(TrayAction::ResetDevice { device }),
                    ["inbox"] => Some(TrayAction::OpenInbox { device }),
                    ["remove"] => Some(TrayAction::RemoveDevice { device }),
                    ["timing", t] => Some(TrayAction::SetTiming {
                        device,
                        preset: TimingPreset::parse(t)?,
                    }),
                    ["fault", f, v] => Some(TrayAction::SetFault {
                        device,
                        fault: Fault::parse(f)?,
                        on: match *v {
                            "1" => true,
                            "0" => false,
                            _ => return None,
                        },
                    }),
                    _ => None,
                }
            }
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Actions and the HTTP mapping
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrayAction {
    SetFault {
        device: String,
        fault: Fault,
        on: bool,
    },
    SetTiming {
        device: String,
        preset: TimingPreset,
    },
    ResetDevice {
        device: String,
    },
    /// Path = that printer's reported `inbox_dir` only ([`Summary::inbox_for`]).
    OpenInbox {
        device: String,
    },
    /// The menu already asked for confirmation (the id lives on the confirm child).
    RemoveDevice {
        device: String,
    },
    AddDevice {
        profile: String,
        suggested_id: String,
    },
    OpenUi,
    OpenSettings,
    AdvanceQueue,
    SkipQueue,
    Quit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpMethod {
    Get,
    Post,
    Delete,
}

impl HttpMethod {
    pub fn as_str(self) -> &'static str {
        match self {
            HttpMethod::Get => "GET",
            HttpMethod::Post => "POST",
            HttpMethod::Delete => "DELETE",
        }
    }
}

/// A loopback HTTP call the shell performs on the tray's behalf.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpRequest {
    pub method: HttpMethod,
    pub path: String,
    /// JSON text, if any.
    pub body: Option<String>,
}

impl HttpRequest {
    fn post(path: String, body: Value) -> HttpRequest {
        HttpRequest {
            method: HttpMethod::Post,
            path,
            body: Some(body.to_string()),
        }
    }
}

impl TrayAction {
    /// The HTTP call that carries the action out. `None` for purely local actions (open the UI,
    /// open the inbox folder, quit) and for ids that fail validation.
    pub fn request(&self) -> Option<HttpRequest> {
        use serde_json::json;
        match self {
            TrayAction::SetFault { device, fault, on } if valid_device_id(device) => {
                let mut m = serde_json::Map::new();
                m.insert(fault.wire().to_string(), Value::Bool(*on));
                Some(HttpRequest::post(
                    format!("/d/{device}/v1/faults"),
                    Value::Object(m),
                ))
            }
            TrayAction::SetTiming { device, preset } if valid_device_id(device) => {
                Some(HttpRequest::post(
                    format!("/d/{device}/v1/timing"),
                    json!({ "preset": preset.token() }),
                ))
            }
            TrayAction::ResetDevice { device } if valid_device_id(device) => Some(
                HttpRequest::post(format!("/d/{device}/v1/reset"), json!({})),
            ),
            TrayAction::RemoveDevice { device } if valid_device_id(device) => Some(HttpRequest {
                method: HttpMethod::Delete,
                path: format!("/v1/devices/{device}"),
                body: None,
            }),
            TrayAction::AddDevice {
                profile,
                suggested_id,
            } if valid_profile_id(profile) && valid_device_id(suggested_id) => {
                Some(HttpRequest::post(
                    "/v1/devices".into(),
                    json!({ "id": suggested_id, "profile": profile }),
                ))
            }
            TrayAction::OpenSettings => Some(HttpRequest::post(
                "/v1/ctl".into(),
                json!({ "cmd": "open_settings", "args": { "page": "printers" } }),
            )),
            TrayAction::AdvanceQueue => {
                Some(HttpRequest::post("/v1/queue/advance".into(), json!({})))
            }
            TrayAction::SkipQueue => Some(HttpRequest::post("/v1/queue/skip".into(), json!({}))),
            _ => None,
        }
    }
}

impl HttpRequest {
    /// The raw HTTP/1.0 request bytes for a loopback call to `127.0.0.1:<port>`
    /// (`Connection: close`, JSON `Content-Length` when there is a body).
    pub fn to_bytes(&self) -> Vec<u8> {
        let body = self.body.as_deref().unwrap_or("");
        let mut head = format!(
            "{} {} HTTP/1.0\r\nHost: 127.0.0.1\r\nConnection: close\r\n",
            self.method.as_str(),
            self.path
        );
        if self.method == HttpMethod::Post || !body.is_empty() {
            head.push_str(&format!(
                "Content-Type: application/json\r\nContent-Length: {}\r\n",
                body.len()
            ));
        }
        head.push_str("\r\n");
        let mut out = head.into_bytes();
        out.extend_from_slice(body.as_bytes());
        out
    }
}

// ---------------------------------------------------------------------------------------------
// Notifications
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NotificationKind {
    /// A printer needs an Add or Re-Add in Windows.
    NeedsAdd,
    /// A printer is now the one to Add (queue head change).
    QueueHead,
    /// A printer went offline (Windows no longer lists it, or the Offline fault is on).
    WentOffline,
    /// A job reached a final state.
    JobCompleted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    pub kind: NotificationKind,
    pub devices: Vec<String>,
    /// One-line body (already cleaned).
    pub detail: String,
    /// The one-off start-up summary.
    pub startup: bool,
}

impl Notification {
    pub fn title(&self) -> &'static str {
        match self.kind {
            NotificationKind::NeedsAdd | NotificationKind::QueueHead => "Add printer required",
            NotificationKind::WentOffline => "Printer offline",
            NotificationKind::JobCompleted => "Print job finished",
        }
    }

    /// Clicking the balloon opens Printers & scanners (only for Add requests).
    pub fn opens_settings(&self) -> bool {
        matches!(
            self.kind,
            NotificationKind::NeedsAdd | NotificationKind::QueueHead
        )
    }
}

fn is_offlineish(d: &DeviceLine) -> bool {
    d.install == Install::Offline || d.input.faults.offline
}

/// Detects transitions between summaries. Time is injected (`now` = any monotonic offset).
#[derive(Debug, Default)]
pub struct TransitionDetector {
    started: bool,
    interval: Option<Duration>,
    attention: HashSet<String>,
    last_head: Option<String>,
    /// Offline since (first seen offline) and whether this episode already notified.
    offline: HashMap<String, (Duration, bool)>,
    last_job: HashMap<String, u64>,
    last_notified: HashMap<(String, NotificationKind), Duration>,
}

impl TransitionDetector {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_interval(interval: Duration) -> Self {
        Self {
            interval: Some(interval),
            ..Self::default()
        }
    }

    fn interval(&self) -> Duration {
        self.interval.unwrap_or(NOTIFY_INTERVAL)
    }

    fn limited(&self, id: &str, kind: NotificationKind, now: Duration, gap: Duration) -> bool {
        self.last_notified
            .get(&(id.to_string(), kind))
            .is_some_and(|t| now.saturating_sub(*t) < gap)
    }

    fn mark(&mut self, ids: &[String], kind: NotificationKind, now: Duration) {
        for id in ids {
            self.last_notified.insert((id.clone(), kind), now);
        }
    }

    /// Feed the latest summary; returns the notifications that are due. An error summary changes
    /// nothing (so a flapping daemon never re-notifies).
    pub fn observe(&mut self, s: &Summary, now: Duration) -> Vec<Notification> {
        if s.level == Level::Red {
            return Vec::new();
        }
        let first = !self.started;
        self.started = true;
        let mut out = Vec::new();
        let gap = self.interval();

        // Queue head change (not on the first observation: the start-up summary covers it).
        let head = s.queue.as_ref().and_then(|q| q.head.clone());
        let mut head_notified: Vec<String> = Vec::new();
        if !first && head != self.last_head {
            if let (Some(h), Some(q)) = (&head, &s.queue) {
                let ids = vec![h.clone()];
                if !self.limited(h, NotificationKind::QueueHead, now, gap) {
                    out.push(Notification {
                        kind: NotificationKind::QueueHead,
                        detail: add_guidance(&s.friendly_of(h), q.position.max(1), q.total.max(1)),
                        devices: ids.clone(),
                        startup: false,
                    });
                    self.mark(&ids, NotificationKind::QueueHead, now);
                    head_notified = ids;
                }
            }
        }
        self.last_head = head;

        // Needs Add / Re-Add (head-only by construction: waiting printers are not attention).
        let cur: Vec<&DeviceLine> = s.devices.iter().filter(|d| d.needs_attention).collect();
        let mut fire = Vec::new();
        for d in &cur {
            if self.attention.contains(&d.id) || head_notified.contains(&d.id) {
                continue;
            }
            if !self.limited(&d.id, NotificationKind::NeedsAdd, now, gap) {
                fire.push(d.id.clone());
            }
        }
        self.attention = cur.iter().map(|d| d.id.clone()).collect();
        if !fire.is_empty() {
            self.mark(&fire, NotificationKind::NeedsAdd, now);
            let detail = if first {
                format!(
                    "{} printer{} need{} an Add in Windows: {}.",
                    fire.len(),
                    plural(fire.len()),
                    if fire.len() == 1 { "s" } else { "" },
                    fire.join(", ")
                )
            } else {
                format!("Add or re-Add needed: {}.", fire.join(", "))
            };
            out.push(Notification {
                kind: NotificationKind::NeedsAdd,
                devices: fire,
                detail,
                startup: first,
            });
        }

        // Went offline, after OFFLINE_CONFIRM of staying offline.
        let mut off_fire = Vec::new();
        let present: HashSet<&str> = s.devices.iter().map(|d| d.id.as_str()).collect();
        self.offline.retain(|k, _| present.contains(k.as_str()));
        for d in &s.devices {
            if !is_offlineish(d) {
                self.offline.remove(&d.id);
                continue;
            }
            let entry = self.offline.entry(d.id.clone()).or_insert((now, first));
            if !entry.1 && now.saturating_sub(entry.0) >= OFFLINE_CONFIRM {
                entry.1 = true;
                off_fire.push(d.id.clone());
            }
        }
        off_fire.retain(|id| !self.limited(id, NotificationKind::WentOffline, now, gap));
        if !off_fire.is_empty() {
            self.mark(&off_fire, NotificationKind::WentOffline, now);
            out.push(Notification {
                kind: NotificationKind::WentOffline,
                detail: format!("Offline: {}.", off_fire.join(", ")),
                devices: off_fire,
                startup: false,
            });
        }

        // A job reaching a final state.
        for d in &s.devices {
            let Some(j) = d.input.last_job.as_ref() else {
                continue;
            };
            let prev = self.last_job.insert(d.id.clone(), j.id);
            let is_new = prev.is_some_and(|p| p != j.id);
            if first || !is_new || !j.terminal() {
                continue;
            }
            if self.limited(
                &d.id,
                NotificationKind::JobCompleted,
                now,
                JOB_NOTIFY_INTERVAL,
            ) {
                continue;
            }
            self.mark(
                std::slice::from_ref(&d.id),
                NotificationKind::JobCompleted,
                now,
            );
            out.push(Notification {
                kind: NotificationKind::JobCompleted,
                detail: format!(
                    "{}: {} ({})",
                    d.id,
                    clean(&j.name, LABEL_MAX),
                    clean(&j.state, 20)
                ),
                devices: vec![d.id.clone()],
                startup: false,
            });
        }
        out
    }
}

// ---------------------------------------------------------------------------------------------
// Notification helper script (PowerShell NotifyIcon balloon) and icon pixels
// ---------------------------------------------------------------------------------------------

const HELPER_TEMPLATE: &str = r#"Add-Type -AssemblyName System.Windows.Forms, System.Drawing
$script:clicked = $false
$ni = New-Object System.Windows.Forms.NotifyIcon
$ni.Icon = [System.Drawing.SystemIcons]::Information
$ni.Text = 'fakeprint'
$ni.Visible = $true
# Windows 11 raises a spurious click shortly after the balloon appears; ignore the first 2.5 s.
$script:shownAt = [datetime]::UtcNow
$open = { if (([datetime]::UtcNow - $script:shownAt).TotalSeconds -lt 2.5) { return }; __ACTION__ $script:clicked = $true }
$ni.add_BalloonTipClicked($open)
$ni.add_Click($open)
$script:shownAt = [datetime]::UtcNow
$ni.ShowBalloonTip(15000, 'fakeprint - __TITLE__', '__DETAIL__', [System.Windows.Forms.ToolTipIcon]::Info)
$end = (Get-Date).AddSeconds(__HOLD__)
while (-not $script:clicked -and (Get-Date) -lt $end) {
    [System.Windows.Forms.Application]::DoEvents()
    Start-Sleep -Milliseconds 100
}
Start-Sleep -Milliseconds 300
$ni.Visible = $false
$ni.Dispose()
"#;

/// Make `s` safe inside a PowerShell single-quoted string: every quote-like character (PowerShell
/// also treats the typographic single quotes as quotes) is doubled, control characters become
/// spaces, the length is capped (balloon bodies are limited to ~255 characters).
pub fn ps_single_quoted(s: &str) -> String {
    let mut out = String::new();
    for ch in s.chars().take(200) {
        match ch {
            '\'' | '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => out.push_str("''"),
            c if c.is_control() => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

/// The helper script for `n`, held alive `hold_secs` (min 5).
pub fn notify_script(n: &Notification, hold_secs: u32) -> String {
    let body = if n.opens_settings() {
        format!("{} Click to open Settings.", n.detail)
    } else {
        n.detail.clone()
    };
    let action = if n.opens_settings() {
        "Start-Process 'ms-settings:printers';"
    } else {
        ""
    };
    HELPER_TEMPLATE
        .replace("__ACTION__", action)
        .replace("__TITLE__", &ps_single_quoted(n.title()))
        .replace("__DETAIL__", &ps_single_quoted(&body))
        .replace("__HOLD__", &hold_secs.max(5).to_string())
}

/// Base64 of the UTF-16LE script, the form `powershell -EncodedCommand` expects.
pub fn encode_command(script: &str) -> String {
    let bytes: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

pub const ICON_SIZE: u32 = 32;

pub fn level_color(level: Level) -> [u8; 3] {
    match level {
        Level::Green => [46, 160, 67],
        Level::Amber => [232, 163, 23],
        Level::Red => [214, 48, 49],
    }
}

/// The fakeprint mark, 32x32 raw RGBA with straight alpha (`docs/assets/render_icons.py` writes it).
const LOGO_RGBA: &[u8] = include_bytes!("../../../docs/assets/fakeprint-tray-32.rgba");

/// Centre and radius of the status dot drawn over the logo's bottom-right corner.
pub const DOT_CENTER: (i32, i32) = (24, 24);
pub const DOT_RADIUS: i32 = 6;

/// RGBA pixels of the fakeprint logo with a status dot in the colour of `level` (32x32).
pub fn icon_rgba(level: Level) -> Vec<u8> {
    let [r, g, b] = level_color(level);
    let mut px = LOGO_RGBA.to_vec();
    debug_assert_eq!(px.len(), (ICON_SIZE * ICON_SIZE * 4) as usize);
    for y in 0..ICON_SIZE as i32 {
        for x in 0..ICON_SIZE as i32 {
            let (dx, dy) = (x - DOT_CENTER.0, y - DOT_CENTER.1);
            let d2 = dx * dx + dy * dy;
            let i = ((y as u32 * ICON_SIZE + x as u32) * 4) as usize;
            if d2 <= DOT_RADIUS * DOT_RADIUS {
                px[i..i + 4].copy_from_slice(&[r, g, b, 255]);
            } else if d2 <= (DOT_RADIUS + 1) * (DOT_RADIUS + 1) {
                // A one-pixel light ring keeps the dot readable on any tray background.
                px[i..i + 4].copy_from_slice(&[255, 255, 255, 255]);
            }
        }
    }
    px
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn dev(id: &str, state: State) -> DeviceInput {
        DeviceInput::new(id, state)
    }
    fn readd(id: &str, reason: &str) -> DeviceInput {
        let mut d = dev(id, State::Installed);
        d.needs_readd = true;
        d.reason = Some(reason.into());
        d
    }
    fn offline(id: &str) -> DeviceInput {
        let mut d = dev(id, State::Stale);
        d.reason = Some("not listed by Windows".into());
        d
    }
    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }
    fn q(head: &str, pos: usize, total: usize) -> QueueView {
        QueueView {
            head: Some(head.into()),
            position: pos,
            total,
        }
    }
    fn sum(d: &[DeviceInput]) -> Summary {
        Summary::build(d, None)
    }

    #[test]
    fn levels_for_every_state() {
        assert_eq!(sum(&[]).level, Level::Green);
        assert_eq!(sum(&[dev("a", State::Installed)]).level, Level::Green);
        for st in [State::New, State::Probed, State::Stale] {
            assert_eq!(
                sum(&[dev("a", State::Installed), dev("b", st)]).level,
                Level::Amber
            );
        }
        assert_eq!(sum(&[readd("a", "profile changed")]).level, Level::Amber);
        assert_eq!(sum(&[offline("a")]).level, Level::Amber);
        let mut f = dev("a", State::Installed);
        f.faults.toner_low = true;
        assert_eq!(sum(&[f]).level, Level::Amber);
        assert_eq!(Summary::error("boom").level, Level::Red);
        assert_eq!(Summary::error("boom").icon_level(), Level::Red);
    }

    #[test]
    fn offline_and_settling_are_not_add_requests() {
        let s = sum(&[offline("a")]);
        assert_eq!(s.devices[0].text, "a: offline");
        assert!(!s.devices[0].needs_attention);
        assert_eq!(
            (s.counts.offline, s.counts.need_add, s.counts.need_readd),
            (1, 0, 0)
        );

        let mut p = dev("b", State::Stale);
        p.reason = Some("profile changed; remove and re-Add".into());
        p.stale_age = Some(secs(5));
        let s = sum(std::slice::from_ref(&p));
        assert_eq!(s.counts.settling, 1);
        assert!(!s.devices[0].needs_attention);
        assert!(s.devices[0].text.contains("confirming"));
        p.stale_age = Some(secs(25));
        let s = sum(std::slice::from_ref(&p));
        assert_eq!(s.counts.need_readd, 1);
        assert!(s.devices[0].needs_attention);
        p.stale_age = None; // unknown age = old
        assert!(sum(&[p]).devices[0].needs_attention);
    }

    #[test]
    fn header_texts() {
        let mut waiting = dev("w", State::New);
        waiting.queue_position = Some(3);
        let mut head = dev("h", State::New);
        head.queue_position = Some(1);
        let s = sum(&[
            dev("i", State::Installed),
            head,
            waiting,
            readd("g", "profile changed"),
            dev("old", State::Stale),
        ]);
        let t: Vec<&str> = s.devices.iter().map(|d| d.text.as_str()).collect();
        assert_eq!(
            t,
            [
                "i: installed",
                "h: needs Add",
                "w: waiting in queue",
                "g: needs re-Add (profile changed)",
                "old: needs re-Add"
            ]
        );
        assert_eq!(
            s.counts,
            Counts {
                total: 5,
                installed: 1,
                need_add: 1,
                need_readd: 2,
                waiting: 1,
                ..Counts::default()
            }
        );
        assert!(!s.devices[2].needs_attention);
    }

    #[test]
    fn tooltip_forms_and_length() {
        assert_eq!(sum(&[]).tooltip(), "fakeprint: no printers");
        assert_eq!(
            sum(&[dev("a", State::Installed)]).tooltip(),
            "fakeprint: 1 printer, 1 installed"
        );
        let mut busy = dev("c", State::Installed);
        busy.active_jobs = 2;
        busy.faults.paper_jam = true;
        let s = sum(&[
            dev("a", State::Installed),
            dev("b", State::New),
            offline("o"),
            busy,
        ]);
        assert_eq!(
            s.tooltip(),
            "fakeprint: 4 printers, 2 installed, 1 need Add, 1 offline, 2 active jobs, 1 with faults"
        );
        assert_eq!(
            Summary::error("daemon down").tooltip(),
            "fakeprint: error (daemon down)"
        );
        let many: Vec<DeviceInput> = (0..40).map(|i| dev(&format!("p{i}"), State::New)).collect();
        assert!(sum(&many).tooltip().chars().count() <= 120);
        assert!(Summary::error("x".repeat(500)).tooltip().chars().count() <= 120);
    }

    #[test]
    fn menu_per_printer() {
        let mut d = dev("mono", State::Installed);
        d.inbox_dir = "C:\\inbox\\mono".into();
        d.active_jobs = 1;
        d.captured_jobs = 3;
        d.active_job = Some(JobLine {
            id: 4,
            name: "report.pdf".into(),
            state: "Processing".into(),
        });
        d.faults.toner_low = true;
        d.timing = Some(TimingPreset::Realistic);
        let m = sum(&[d]).menu(&[]);
        let top = &m.items[0];
        assert_eq!(top.label, "mono: installed");
        let labels: Vec<&str> = top.children.iter().map(|c| c.label.as_str()).collect();
        assert!(labels.contains(&"State: idle (Toner low)"), "{labels:?}");
        assert!(labels.contains(&"Jobs: 1 active, 3 captured"));
        assert!(labels.contains(&"Active: report.pdf (Processing)"));
        assert!(labels.contains(&"Open inbox folder"));
        for l in [
            "Paper out",
            "Toner low",
            "Door open",
            "Paper jam",
            "Offline",
        ] {
            assert!(labels.contains(&l), "{l}");
        }
        let toner = top
            .children
            .iter()
            .find(|c| c.label == "Toner low")
            .unwrap();
        assert_eq!(toner.checked, Some(true));
        // Clicking a checked toggle turns it off; an unchecked one turns it on.
        assert_eq!(toner.id.0, "dev:mono:fault:toner_low:0");
        let paper = top
            .children
            .iter()
            .find(|c| c.label == "Paper out")
            .unwrap();
        assert_eq!(paper.checked, Some(false));
        assert_eq!(paper.id.0, "dev:mono:fault:paper_out:1");
        let timing = top.children.iter().find(|c| c.label == "Timing").unwrap();
        let checked: Vec<&str> = timing
            .children
            .iter()
            .filter(|c| c.checked == Some(true))
            .map(|c| c.label.as_str())
            .collect();
        assert_eq!(checked, ["Realistic"]);
        assert!(labels.contains(&"Reset printer"));
        assert!(labels.contains(&"Remove printer..."));
        // No guidance for an installed printer.
        assert!(!labels.iter().any(|l| l.contains("Add device")));
    }

    #[test]
    fn custom_timing_selects_no_radio_and_inbox_needs_a_real_path() {
        let mut d = dev("a", State::Installed);
        d.timing = None;
        let m = sum(&[d]).menu(&[]);
        let top = &m.items[0];
        let timing = top.children.iter().find(|c| c.label == "Timing").unwrap();
        assert!(timing.children.iter().all(|c| c.checked == Some(false)));
        let inbox = top
            .children
            .iter()
            .find(|c| c.label == "Open inbox folder")
            .unwrap();
        assert!(!inbox.enabled, "no reported inbox path");
    }

    #[test]
    fn guidance_only_when_attention_is_needed() {
        let mut head = dev("a", State::New);
        head.queue_position = Some(1);
        let mut w = dev("b", State::New);
        w.queue_position = Some(2);
        let m = sum(&[head, w]).menu(&[]);
        let has = |i: usize| {
            m.items[i]
                .children
                .iter()
                .any(|c| c.id.0.ends_with(":guidance"))
        };
        assert!(has(0));
        assert!(!has(1));
    }

    #[test]
    fn top_level_items_and_queue_line() {
        let mut a = dev("mono", State::New);
        a.friendly_name = "fakeprint mono".into();
        a.queue_position = Some(2);
        let s = Summary::build(&[dev("x", State::Installed), a], Some(q("mono", 2, 5)));
        let profiles = vec![
            ProfileChoice {
                id: "mono-laser".into(),
                label: "Mono laser".into(),
            },
            ProfileChoice {
                id: "generic-wsd".into(),
                label: "Generic".into(),
            },
        ];
        let m = s.menu(&profiles);
        let labels: Vec<&str> = m.items.iter().map(|i| i.label.as_str()).collect();
        assert!(labels.contains(&"Add printer"));
        assert!(labels.contains(&"Add now: fakeprint mono (2 of 5)"));
        assert!(labels.contains(&"Skip this printer"));
        assert!(labels.contains(&"Advance queue"));
        assert!(labels.contains(&"Open device manager"));
        assert!(labels.contains(&"Open Printers & scanners settings"));
        assert!(labels.contains(&"Quit fakeprint"));
        let add = m.items.iter().find(|i| i.label == "Add printer").unwrap();
        assert_eq!(add.children.len(), 2);
        assert_eq!(add.children[0].id.0, "add:mono-laser:mono-laser");
        // No Windows Add item exists anywhere.
        for i in m.walk() {
            let l = i.label.to_ascii_lowercase();
            assert!(
                !l.contains("add-printer") && !l.contains("install now"),
                "{l}"
            );
        }
        // No queue, no queue lines.
        let m2 = sum(&[dev("x", State::Installed)]).menu(&[]);
        assert!(!m2.walk().iter().any(|i| i.label.starts_with("Add now")));
        // An empty profile list is explained, not empty.
        let add = m2.items.iter().find(|i| i.label == "Add printer").unwrap();
        assert_eq!(add.children[0].label, "No profiles found");
        assert!(!add.children[0].enabled);
    }

    #[test]
    fn error_menu_still_has_quit_and_settings() {
        let m = Summary::error("refused").menu(&[]);
        assert!(m.items[0].label.contains("not reachable"));
        let ids: Vec<&str> = m.walk().iter().map(|i| i.id.0.as_str()).collect();
        assert!(ids.contains(&"quit") && ids.contains(&"settings") && ids.contains(&"ui"));
        assert!(!ids.iter().any(|i| i.starts_with("add:")));
    }

    #[test]
    fn every_action_id_round_trips() {
        let m = Summary::build(
            &[dev("mono", State::Installed), dev("c-2", State::New)],
            Some(q("c-2", 1, 1)),
        )
        .menu(&[ProfileChoice {
            id: "mono-laser".into(),
            label: "m".into(),
        }]);
        let mut n = 0;
        for i in m.walk() {
            let act = MenuId::parse(&i.id.0);
            let inert =
                i.id.0.starts_with("sub:") || i.id.0.starts_with("info:") || i.id.0 == "sep";
            if inert {
                assert!(act.is_none(), "{}", i.id.0);
            } else {
                assert!(act.is_some(), "unparsable action id {}", i.id.0);
                n += 1;
            }
        }
        assert!(n >= 20, "{n}");
        assert_eq!(
            MenuId::parse("dev:a:fault:offline:1"),
            Some(TrayAction::SetFault {
                device: "a".into(),
                fault: Fault::Offline,
                on: true
            })
        );
        assert_eq!(
            MenuId::fault("a", Fault::PaperJam, false).0,
            "dev:a:fault:paper_jam:0"
        );
    }

    #[test]
    fn malformed_ids_are_rejected() {
        for bad in [
            "",
            "dev",
            "dev::reset",
            "dev:a b:reset",
            "dev:a/../b:reset",
            "dev:a:reset:extra",
            "dev:a:fault:nope:1",
            "dev:a:fault:paper_out:2",
            "dev:a:fault:paper_out",
            "dev:a:timing:fast",
            "dev:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa:reset",
            "add::x",
            "add:p:",
            "add:.hidden:x",
            "add:p q:x",
            "queue",
            "queue:x",
            "quit:now",
            "sub:a",
            "info:a",
            "sep",
            "UI",
        ] {
            assert_eq!(MenuId::parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn http_mapping() {
        let r = |s: &str| MenuId::parse(s).unwrap().request();
        let req = r("dev:mono:fault:offline:1").unwrap();
        assert_eq!(
            (req.method, req.path.as_str(), req.body.as_deref()),
            (
                HttpMethod::Post,
                "/d/mono/v1/faults",
                Some(r#"{"not_accepting_jobs":true}"#)
            )
        );
        let req = r("dev:mono:fault:cover_open:0").unwrap();
        assert_eq!(req.body.as_deref(), Some(r#"{"cover_open":false}"#));
        let req = r("dev:mono:timing:instant").unwrap();
        assert_eq!(req.path, "/d/mono/v1/timing");
        assert_eq!(req.body.as_deref(), Some(r#"{"preset":"instant"}"#));
        let req = r("dev:mono:reset").unwrap();
        assert_eq!(
            (req.method, req.path.as_str()),
            (HttpMethod::Post, "/d/mono/v1/reset")
        );
        let req = r("dev:mono:remove").unwrap();
        assert_eq!(
            (req.method, req.path.as_str(), req.body),
            (HttpMethod::Delete, "/v1/devices/mono", None)
        );
        let req = r("add:mono-laser:mono-laser-2").unwrap();
        assert_eq!(req.path, "/v1/devices");
        let body: Value = serde_json::from_str(req.body.as_deref().unwrap()).unwrap();
        assert_eq!(body, json!({"id": "mono-laser-2", "profile": "mono-laser"}));
        let req = r("settings").unwrap();
        assert_eq!(req.path, "/v1/ctl");
        let body: Value = serde_json::from_str(req.body.as_deref().unwrap()).unwrap();
        assert_eq!(body["cmd"], "open_settings");
        assert_eq!(body["args"]["page"], "printers");
        assert_eq!(r("queue:advance").unwrap().path, "/v1/queue/advance");
        assert_eq!(r("queue:skip").unwrap().path, "/v1/queue/skip");
        // Local actions carry no HTTP call.
        for local in ["ui", "quit", "dev:mono:inbox"] {
            assert_eq!(r(local), None, "{local}");
        }
        // Hand-built actions with hostile ids are refused too.
        let evil = TrayAction::ResetDevice {
            device: "../v1/devices".into(),
        };
        assert_eq!(evil.request(), None);
        assert_eq!(HttpMethod::Delete.as_str(), "DELETE");
        assert_eq!(HttpMethod::Get.as_str(), "GET");
    }

    #[test]
    fn raw_request_bytes() {
        let r = MenuId::parse("dev:mono:timing:instant")
            .unwrap()
            .request()
            .unwrap();
        let text = String::from_utf8(r.to_bytes()).unwrap();
        assert!(text.starts_with("POST /d/mono/v1/timing HTTP/1.0\r\n"));
        assert!(text.contains("Content-Length: 20\r\n"), "{text}");
        assert!(text.ends_with("\r\n\r\n{\"preset\":\"instant\"}"));
        let d = MenuId::parse("dev:mono:remove").unwrap().request().unwrap();
        let text = String::from_utf8(d.to_bytes()).unwrap();
        assert!(text.starts_with("DELETE /v1/devices/mono HTTP/1.0\r\n"));
        assert!(!text.contains("Content-Length"));
    }

    #[test]
    fn next_free_id_rules() {
        let ex = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(next_free_id("mono-laser", &[]), "mono-laser");
        assert_eq!(
            next_free_id("mono-laser", &ex(&["mono-laser"])),
            "mono-laser-2"
        );
        assert_eq!(
            next_free_id("mono-laser", &ex(&["Mono-Laser", "mono-laser-2"])),
            "mono-laser-3"
        );
        assert_eq!(next_free_id("a.b c", &[]), "a-b-c");
        assert_eq!(next_free_id("...", &[]), "printer");
        assert_eq!(next_free_id("default", &[]), "default-2");
        let long = next_free_id(&"x".repeat(80), &ex(&[&"x".repeat(26)]));
        assert!(valid_device_id(&long), "{long}");
    }

    #[test]
    fn guidance_text_is_the_contract_text() {
        assert_eq!(
            add_guidance("fakeprint mono", 2, 5),
            "Add now: fakeprint mono (2 of 5). Open Settings > Bluetooth & devices > Printers & scanners > Add device, wait for \"fakeprint mono\" to appear and choose Add. Windows takes about 30 seconds; fakeprint announces the next printer only after this one is installed."
        );
        assert_eq!(
            add_now_line("fakeprint mono", 2, 5),
            "Add now: fakeprint mono (2 of 5)"
        );
        // Hostile friendly names are cleaned.
        assert!(!add_guidance("a\r\nb\u{202E}c", 1, 1).contains('\n'));
    }

    #[test]
    fn hostile_text_never_reaches_labels() {
        let mut d = dev("a", State::Installed);
        d.active_jobs = 1;
        d.active_job = Some(JobLine {
            id: 1,
            name: "evil\r\n\u{202E}name&x".repeat(20),
            state: "Proc\nessing".into(),
        });
        let m = sum(&[d]).menu(&[]);
        for i in m.walk() {
            assert!(!i.label.chars().any(|c| c.is_control()), "{:?}", i.label);
            assert!(!i.label.contains('\u{202E}'));
            assert!(i.label.chars().count() <= 120);
        }
        assert_eq!(win_label("Printers & scanners"), "Printers && scanners");
        assert_eq!(clean("  a \t b\n", 10), "a b");
        assert_eq!(clean("abcdefghij", 5), "abcd\u{2026}");
    }

    #[test]
    fn open_inbox_accepts_only_a_reported_path() {
        let mut d = dev("a", State::Installed);
        d.inbox_dir = "C:\\Users\\x\\inbox\\a".into();
        let mut e = dev("b", State::Installed);
        e.inbox_dir = "relative\\dir".into();
        let mut f = dev("c", State::Installed);
        f.inbox_dir = "C:\\x\u{0}y".into();
        let s = sum(&[d, e, f]);
        assert_eq!(s.inbox_for("a"), Some("C:\\Users\\x\\inbox\\a"));
        assert_eq!(s.inbox_for("b"), None);
        assert_eq!(s.inbox_for("c"), None);
        assert_eq!(s.inbox_for("nope"), None);
        assert!(reportable_path("\\\\srv\\share\\in"));
        assert!(reportable_path("D:/in"));
        assert!(!reportable_path("-rf"));
    }

    #[test]
    fn from_json_is_tolerant() {
        let v = json!({
            "id": "mono", "friendly_name": "fakeprint mono", "needs_readd": true,
            "needs_readd_reason": "profile changed", "announcing": true, "queue_position": 2,
            "inbox_dir": "C:\\i",
            "install": {"state": "installed", "stale_at": "2026-01-01T00:00:00.123Z"},
            "live": {"printer_state": "Processing", "active_jobs": 1, "captured_jobs": 2,
                     "active_job": {"id": 3, "name": "a.pdf", "state": "Processing"},
                     "last_job": null, "timing_preset": "custom",
                     "faults": {"paper_out": true, "unknown_future": true},
                     "future": 1},
            "extra_top_level": [1, 2]
        });
        let d = DeviceInput::from_json_at(&v, 1_767_225_600 + 7).unwrap();
        assert_eq!(d.id, "mono");
        assert!(d.needs_readd && d.announcing);
        assert_eq!(d.queue_position, Some(2));
        assert_eq!(d.printer_state, "processing");
        assert_eq!(d.timing, None);
        assert!(d.faults.paper_out && !d.faults.offline);
        assert_eq!(d.stale_age, Some(secs(7)));
        assert_eq!(d.active_job.as_ref().unwrap().name, "a.pdf");
        // Only an id: everything else defaulted.
        let m = DeviceInput::from_json(&json!({"id": "x"})).unwrap();
        assert_eq!(m.state, State::New);
        assert_eq!(m.friendly_name, "x");
        // Missing, non-string and malformed ids are dropped.
        for bad in [
            json!({}),
            json!({"id": 5}),
            json!({"id": "a b"}),
            json!({"id": "../x"}),
            json!({"id": ""}),
        ] {
            assert!(DeviceInput::from_json(&bad).is_none(), "{bad}");
        }
        assert_eq!(QueueView::from_json(&json!([])), None);
        assert_eq!(
            QueueView::from_json(&json!({"head": "bad id", "total": 2}))
                .unwrap()
                .head,
            None
        );
    }

    #[test]
    fn from_deviceinfo_matches_from_json() {
        let json = json!({
            "id": "mono", "default": false,
            "uuid": "00000000-0000-4000-8000-0000000000a1",
            "ipp_uuid": "00000000-0000-4000-8000-0000000000a2",
            "profile": "mono-laser", "friendly_name": "fakeprint mono", "base_path": "/d/mono",
            "device_endpoint": null, "service_endpoint": null, "ipp_endpoint": null,
            "jetdirect_port": null, "inbox_dir": "C:\\i", "announcing": true, "queue_position": 1,
            "needs_readd": false, "needs_readd_reason": null,
            "install": {"state": "probed"},
            "config": {"fingerprint": "", "published_fingerprint": "", "summary": {}},
            "live": {"printer_state": "Idle", "state_reasons": [], "accepting_jobs": true,
                     "active_jobs": 0, "captured_jobs": 1, "active_job": null,
                     "last_job": {"id": 1, "name": "j", "state": "Completed"},
                     "faults": {"paper_out": false, "toner_low": false, "cover_open": false,
                                "paper_jam": false, "offline": true},
                     "timing_preset": "instant", "consumables": [], "inputs": []}
        });
        let info: DeviceInfo = serde_json::from_value(json.clone()).expect("DeviceInfo shape");
        let a = DeviceInput::from(&info);
        let b = DeviceInput::from_json(&json).unwrap();
        assert_eq!(a, b);
        assert_eq!(a.timing, Some(TimingPreset::Instant));
        assert!(a.faults.offline);
        let qi = QueueInfo {
            head: Some("mono".into()),
            position: Some(1),
            total: 2,
            ..QueueInfo::empty()
        };
        let s = Summary::from_devices(&[a], Some(&qi));
        assert_eq!(s.queue.unwrap().total, 2);
    }

    #[test]
    fn http_response_parsing() {
        let ok = b"HTTP/1.0 200 OK\r\ncontent-type: application/json\r\n\r\n[{\"id\":\"a\",\"install\":{\"state\":\"installed\"}},{\"id\":\"b\"},{\"id\":\"bad id\"}]";
        let qok = b"HTTP/1.0 200 OK\r\n\r\n{\"head\":\"b\",\"position\":1,\"total\":1}";
        let s = Summary::from_http_responses(ok, Some(qok));
        assert_eq!(s.level, Level::Amber);
        assert_eq!(s.devices.len(), 2, "malformed id dropped");
        assert_eq!(s.queue.as_ref().unwrap().head.as_deref(), Some("b"));
        // A broken queue is no queue, not a Red summary.
        let s = Summary::from_http_responses(ok, Some(b"HTTP/1.0 501 x\r\n\r\n{}"));
        assert_eq!(s.level, Level::Amber);
        assert!(s.queue.is_none());
        for bad in [
            &b"HTTP/1.0 500 x\r\n\r\n[]"[..],
            b"garbage",
            b"HTTP/1.0 200 OK\r\n\r\n{}",
            b"HTTP/1.0 200 OK\r\n\r\nnot json",
        ] {
            assert_eq!(Summary::from_http_responses(bad, None).level, Level::Red);
        }
        let ps = ProfileChoice::list_from_json(&json!([
            {"id": "mono-laser", "description": "Mono\nlaser"},
            {"id": "a:b"}, {"nope": 1}, {"id": "../x"}, {"id": "plain"}
        ]));
        assert_eq!(
            ps,
            [
                ProfileChoice {
                    id: "mono-laser".into(),
                    label: "Mono laser".into()
                },
                ProfileChoice {
                    id: "plain".into(),
                    label: "plain".into()
                },
            ]
        );
        assert!(ProfileChoice::list_from_json(&json!({})).is_empty());
    }

    // ---- detector ----

    #[test]
    fn startup_summary_only_if_needed() {
        let mut d = TransitionDetector::new();
        assert!(d
            .observe(&sum(&[dev("a", State::Installed)]), secs(0))
            .is_empty());
        let mut d = TransitionDetector::new();
        let n = d.observe(
            &sum(&[dev("a", State::New), dev("b", State::Stale)]),
            secs(0),
        );
        assert_eq!(n.len(), 1);
        assert!(n[0].startup && n[0].kind == NotificationKind::NeedsAdd);
        assert_eq!(n[0].devices, ["a", "b"]);
        assert!(n[0].detail.contains("2 printers") && n[0].detail.contains("a, b"));
        assert!(n[0].opens_settings());
    }

    #[test]
    fn needs_add_transition_and_rate_limit() {
        let mut d = TransitionDetector::with_interval(secs(600));
        let ok = sum(&[dev("a", State::Installed), dev("b", State::Installed)]);
        let a_bad = sum(&[dev("a", State::Stale), dev("b", State::Installed)]);
        let both = sum(&[dev("a", State::Stale), dev("b", State::Stale)]);
        d.observe(&ok, secs(0));
        let n = d.observe(&a_bad, secs(100));
        assert_eq!(n.len(), 1);
        assert!(!n[0].startup);
        // No repeat while it stays in attention.
        assert!(d.observe(&a_bad, secs(105)).is_empty());
        // a flaps back and re-enters within 10 min: suppressed.
        d.observe(&ok, secs(110));
        assert!(d.observe(&a_bad, secs(120)).is_empty());
        // b enters: only b.
        let n = d.observe(&both, secs(130));
        assert_eq!(n[0].devices, ["b"]);
        // a flaps again after its window.
        d.observe(&ok, secs(140));
        let n = d.observe(&a_bad, secs(100 + 601));
        assert_eq!(n[0].devices, ["a"]);
    }

    #[test]
    fn profile_flap_shorter_than_grace_never_notifies_and_persistent_does_once() {
        let stale = |age: u64| {
            let mut d = dev("a", State::Stale);
            d.needs_readd = true;
            d.reason = Some("profile changed; re-Add".into());
            d.stale_age = Some(secs(age));
            sum(&[d])
        };
        let ok = sum(&[dev("a", State::Installed)]);
        let mut d = TransitionDetector::new();
        assert!(d.observe(&ok, secs(0)).is_empty());
        assert!(d.observe(&stale(3), secs(3)).is_empty());
        assert!(d.observe(&stale(15), secs(15)).is_empty());
        assert!(d.observe(&ok, secs(18)).is_empty());
        let mut d = TransitionDetector::new();
        d.observe(&ok, secs(0));
        assert!(d.observe(&stale(5), secs(5)).is_empty());
        assert_eq!(d.observe(&stale(20), secs(20)).len(), 1);
        assert!(d.observe(&stale(30), secs(30)).is_empty());
    }

    #[test]
    fn waiting_printers_do_not_notify_until_they_become_head() {
        let mk = |a_pos: usize, b_pos: usize, head: &str| {
            let mut a = dev("a", State::New);
            a.queue_position = Some(a_pos);
            let mut b = dev("b", State::New);
            b.queue_position = Some(b_pos);
            Summary::build(&[a, b], Some(q(head, 1, 2)))
        };
        let mut d = TransitionDetector::new();
        let n = d.observe(&mk(1, 2, "a"), secs(0));
        assert_eq!(n.len(), 1, "startup names only the head");
        assert_eq!(n[0].devices, ["a"]);
        assert!(d.observe(&mk(1, 2, "a"), secs(5)).is_empty());
        // a gets installed, b becomes head: ONE queue-head notification with the guidance, and
        // no duplicate needs-Add for b.
        let a_done = dev("a", State::Installed);
        let mut b = dev("b", State::New);
        b.queue_position = Some(1);
        let n = d.observe(&Summary::build(&[a_done, b], Some(q("b", 1, 1))), secs(40));
        assert_eq!(n.len(), 1);
        assert_eq!(n[0].kind, NotificationKind::QueueHead);
        assert_eq!(n[0].devices, ["b"]);
        assert!(
            n[0].detail.starts_with("Add now: b (1 of 1)."),
            "{}",
            n[0].detail
        );
    }

    #[test]
    fn offline_notifies_after_confirmation_once_per_episode() {
        let mut d = TransitionDetector::with_interval(secs(1));
        let ok = sum(&[dev("a", State::Installed)]);
        d.observe(&ok, secs(0));
        let off = sum(&[offline("a")]);
        assert!(d.observe(&off, secs(10)).is_empty(), "debounce");
        assert!(d.observe(&off, secs(13)).is_empty());
        let n = d.observe(&off, secs(16));
        assert_eq!(n.len(), 1);
        assert_eq!(n[0].kind, NotificationKind::WentOffline);
        assert!(!n[0].opens_settings());
        assert!(d.observe(&off, secs(60)).is_empty(), "once per episode");
        // A blip shorter than the confirmation never notifies.
        d.observe(&ok, secs(70));
        assert!(d.observe(&off, secs(71)).is_empty());
        d.observe(&ok, secs(72));
        assert!(d.observe(&ok, secs(100)).is_empty());
        // The Offline fault counts too.
        let mut f = dev("a", State::Installed);
        f.faults.offline = true;
        let fs = sum(&[f]);
        d.observe(&fs, secs(200));
        assert_eq!(d.observe(&fs, secs(207)).len(), 1);
    }

    #[test]
    fn job_completion_is_detected_not_at_startup_and_not_for_active_jobs() {
        let with = |id: u64, state: &str| {
            let mut a = dev("a", State::Installed);
            a.last_job = Some(JobLine {
                id,
                name: "evil'; Remove-Item\r\n".into(),
                state: state.into(),
            });
            sum(&[a])
        };
        let mut d = TransitionDetector::new();
        assert!(
            d.observe(&with(1, "Completed"), secs(0)).is_empty(),
            "history at start"
        );
        assert!(d.observe(&with(1, "Completed"), secs(2)).is_empty());
        let n = d.observe(&with(2, "Completed"), secs(5));
        assert_eq!(n.len(), 1);
        assert_eq!(n[0].kind, NotificationKind::JobCompleted);
        assert!(!n[0].detail.contains('\n'));
        // Within the job interval: debounced.
        assert!(d.observe(&with(3, "Aborted"), secs(10)).is_empty());
        // Non-final states never notify.
        assert!(d.observe(&with(4, "Processing"), secs(100)).is_empty());
        assert_eq!(d.observe(&with(5, "Canceled"), secs(200)).len(), 1);
    }

    #[test]
    fn error_summary_changes_nothing() {
        let mut d = TransitionDetector::new();
        let bad = sum(&[dev("a", State::New)]);
        assert_eq!(d.observe(&bad, secs(0)).len(), 1);
        assert!(d.observe(&Summary::error("x"), secs(1)).is_empty());
        assert!(d.observe(&bad, secs(2)).is_empty());
    }

    // ---- time, quoting, icons ----

    #[test]
    fn iso_age_parsing() {
        assert_eq!(age_of_iso("1970-01-01T00:00:10Z", 25), Some(secs(15)));
        assert_eq!(
            age_of_iso("2026-01-01T00:00:00Z", 1_767_225_600 + 7),
            Some(secs(7))
        );
        assert_eq!(
            age_of_iso("2026-01-01T00:00:00.5Z", 1_767_225_600 + 7),
            Some(secs(7))
        );
        assert_eq!(
            age_of_iso("2026-01-01T00:00:00+00:00", 1_767_225_600 + 7),
            Some(secs(7))
        );
        assert_eq!(
            age_of_iso("2020-02-29T00:00:00Z", 1_582_934_400 + 60),
            Some(secs(60))
        );
        assert_eq!(age_of_iso("garbage", 5), None);
        assert_eq!(age_of_iso("2026-01-01T00:00:00+02:00", 5), None);
        assert_eq!(age_of_iso("2026-13-01T00:00:00Z", 5), None);
        assert_eq!(age_of_iso("2026-01-01T00:00:00Z", 0), Some(secs(0)));
    }

    fn decode(enc: &str) -> String {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(enc)
            .unwrap();
        let u: Vec<u16> = bytes
            .chunks(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        String::from_utf16(&u).unwrap()
    }

    #[test]
    fn powershell_quoting_is_injection_safe() {
        let evil = "x'); Remove-Item C:\\ -Recurse; ('\u{2019}\r\nnext";
        let qd = ps_single_quoted(evil);
        assert!(!qd.contains('\n') && !qd.contains('\r') && !qd.contains('\u{2019}'));
        assert_eq!(qd.replace("''", "").matches('\'').count(), 0);
        assert!(ps_single_quoted(&"a".repeat(1000)).len() <= 200);
        let n = Notification {
            kind: NotificationKind::NeedsAdd,
            devices: vec!["a".into()],
            detail: evil.into(),
            startup: false,
        };
        let script = notify_script(&n, 5);
        assert!(script.contains(&format!(
            "'{} Click to open Settings.'",
            ps_single_quoted(evil)
        )));
        assert!(script.contains("Start-Process 'ms-settings:printers';"));
        assert!(!script.contains("__"));
        assert_eq!(decode(&encode_command(&script)), script);
        let j = Notification {
            kind: NotificationKind::JobCompleted,
            ..n
        };
        let script = notify_script(&j, 120);
        assert!(
            !script.contains("Start-Process"),
            "job balloons open nothing"
        );
        assert!(script.contains("AddSeconds(120)"));
    }

    #[test]
    fn icon_buffers() {
        let mut seen = HashSet::new();
        for lvl in [Level::Green, Level::Amber, Level::Red] {
            let px = icon_rgba(lvl);
            assert_eq!(px.len(), (ICON_SIZE * ICON_SIZE * 4) as usize);
            assert!(px.chunks(4).any(|p| p[3] == 255), "opaque pixels");
            assert!(px.chunks(4).any(|p| p[3] == 0), "transparent corners");
            let i = ((DOT_CENTER.1 as u32 * ICON_SIZE + DOT_CENTER.0 as u32) * 4) as usize;
            assert_eq!(&px[i..i + 3], &level_color(lvl));
            seen.insert(px);
        }
        assert_eq!(seen.len(), 3);
    }
}
