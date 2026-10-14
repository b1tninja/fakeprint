//! The attributes the IPP face reports: the printer's (from the profile and the live status),
//! the jobs' (from the shared job engine), and the filtering of both by `requested-attributes`.
//!
//! Job-template validation lives here too, because it is the same lists: a value is supported
//! exactly when the printer advertises it in the matching `*-supported` attribute.

use std::collections::BTreeSet;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use uuid::Uuid;

use super::caps::{
    kebab, media_dimensions, source_keyword, Caps, OPERATIONS_SUPPORTED, ORIENTATION_LANDSCAPE,
    ORIENTATION_PORTRAIT, ORIENTATION_REVERSE_LANDSCAPE, ORIENTATION_REVERSE_PORTRAIT,
    QUALITY_DRAFT, QUALITY_HIGH, QUALITY_NORMAL,
};
use super::codec::{Attr, Member, Value};
use crate::profile::PrinterProfile;
use crate::wsd::ctx::{PrintCtx, PrinterStatusSnapshot};
use crate::wsd::jobs::unix_ms;
use crate::wsd::types::{JobState, JobStateReason, PrintJob, PrinterState, Severity};

/// IPP versions the server answers.
pub const VERSIONS_SUPPORTED: [&str; 3] = ["1.0", "1.1", "2.0"];

/// Job-template attributes the printer honours (the others are `unsupported`).
pub const JOB_TEMPLATE_NAMES: [&str; 12] = [
    "copies",
    "sides",
    "orientation-requested",
    "print-quality",
    "printer-resolution",
    "media",
    "media-col",
    "print-color-mode",
    "number-up",
    "job-priority",
    "job-hold-until",
    "finishings",
];

/// `job-creation-attributes-supported`: the job-template attributes plus the creation-time
/// operation attributes the printer reads.
pub fn job_creation_attributes() -> Vec<String> {
    let mut v: Vec<String> = JOB_TEMPLATE_NAMES.iter().map(|s| s.to_string()).collect();
    v.push("output-bin".into());
    v.push("ipp-attribute-fidelity".into());
    v.push("job-name".into());
    v.sort();
    v
}

/// Everything attribute building needs to know, borrowed for one request.
pub struct Env<'a> {
    pub ctx: &'a PrintCtx,
    pub profile: &'a PrinterProfile,
    pub caps: &'a Caps,
    pub printer_uri: &'a str,
    pub printer_uuid: Uuid,
    pub now: SystemTime,
    pub started: SystemTime,
    pub language: &'a str,
}

fn secs_since(later: SystemTime, earlier: SystemTime) -> i32 {
    i32::try_from(later.duration_since(earlier).unwrap_or_default().as_secs()).unwrap_or(i32::MAX)
}

/// The IPP `dateTime` octets of an instant, in UTC.
pub fn date_time(t: SystemTime) -> Value {
    let ms = unix_ms(t);
    let (secs, milli) = (ms / 1000, ms % 1000);
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let z = i64::try_from(days).unwrap_or(0) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = u8::try_from(doy - (153 * mp + 2) / 5 + 1).unwrap_or(1);
    let month = u8::try_from(if mp < 10 { mp + 3 } else { mp - 9 }).unwrap_or(1);
    let year = u16::try_from(yoe + era * 400 + i64::from(month <= 2)).unwrap_or(1970);
    let mut d = [0u8; 11];
    d[..2].copy_from_slice(&year.to_be_bytes());
    d[2] = month;
    d[3] = day;
    d[4] = u8::try_from(rem / 3600).unwrap_or(0);
    d[5] = u8::try_from(rem % 3600 / 60).unwrap_or(0);
    d[6] = u8::try_from(rem % 60).unwrap_or(0);
    d[7] = u8::try_from(milli / 100).unwrap_or(0);
    d[8] = b'+';
    Value::DateTime(d)
}

fn media_size(x: i32, y: i32) -> Member {
    Member {
        name: "media-size".into(),
        values: vec![Value::Collection(vec![
            Member {
                name: "x-dimension".into(),
                values: vec![Value::Integer(x)],
            },
            Member {
                name: "y-dimension".into(),
                values: vec![Value::Integer(y)],
            },
        ])],
    }
}

fn media_col_of(size: &str, extra: Vec<Member>) -> Value {
    let mut m = Vec::new();
    if let Some((x, y)) = media_dimensions(size) {
        m.push(media_size(x, y));
    }
    m.extend(extra);
    Value::Collection(m)
}

fn member(name: &str, v: Value) -> Member {
    Member {
        name: name.into(),
        values: vec![v],
    }
}

// ---------------------------------------------------------------------------------------------
// printer-state-reasons
// ---------------------------------------------------------------------------------------------

/// The IPP `printer-state-reasons` keywords for the live status (`none` when all is well).
pub fn printer_state_reasons(
    snap: &PrinterStatusSnapshot,
    profile: &PrinterProfile,
) -> Vec<String> {
    let has_toner = profile
        .consumables()
        .iter()
        .any(|c| c.kind.eq_ignore_ascii_case("toner"));
    let mut out: Vec<String> = Vec::new();
    for c in &snap.active_conditions {
        let base = match c.name.as_str() {
            "CoverOpen" => "cover-open",
            "InterlockOpen" | "DoorOpen" => "door-open",
            "Jam" => "media-jam",
            "InputTraySupplyEmpty" => "media-empty",
            "InputTraySupplyLow" => "media-low",
            "InputTrayMissing" => "input-tray-missing",
            "InputTrayMediaSizeChange" | "InputTrayMediaTypeChange" => "media-needed",
            "OutputTrayFull" => "output-area-full",
            "OutputTrayAlmostFull" => "output-area-almost-full",
            "OutputTrayMissing" => "output-tray-missing",
            "ConsumableLow" if has_toner => "toner-low",
            "ConsumableLow" => "marker-supply-low",
            "ConsumableEmpty" if has_toner => "toner-empty",
            "ConsumableEmpty" => "marker-supply-empty",
            "WasteReceptacleAlmostFull" => "marker-waste-almost-full",
            "WasteReceptacleFull" => "marker-waste-full",
            "FuserOverTemperature" => "fuser-over-temp",
            "FuserUnderTemperature" => "fuser-under-temp",
            _ => "other",
        };
        let suffix = match c.severity {
            Severity::Critical => "-error",
            Severity::Warning => "-warning",
            Severity::Informational => "-report",
        };
        let kw = format!("{base}{suffix}");
        if !out.contains(&kw) {
            out.push(kw);
        }
    }
    if !snap.accepting_jobs {
        out.push("offline-report".into());
    }
    if out.is_empty() {
        out.push("none".into());
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Printer attributes
// ---------------------------------------------------------------------------------------------

/// The printer's attributes as `(printer-description, job-template)`.
#[allow(clippy::vec_init_then_push)]
pub fn printer_attributes(env: &Env<'_>) -> (Vec<Attr>, Vec<Attr>) {
    let p = env.profile;
    let c = env.caps;
    let snap = env.ctx.status();
    let timing = env.ctx.timing();
    let state = match snap.state {
        PrinterState::Idle => 3,
        PrinterState::Processing => 4,
        PrinterState::Stopped => 5,
    };
    let mut d: Vec<Attr> = Vec::new();
    d.push(Attr::new(
        "printer-uri-supported",
        Value::uri(env.printer_uri),
    ));
    d.push(Attr::keywords("uri-security-supported", &["none"]));
    d.push(Attr::keywords("uri-authentication-supported", &["none"]));
    d.push(Attr::new("printer-name", Value::name(p.friendly_name())));
    d.push(Attr::new(
        "printer-location",
        Value::text(p.identity.location.clone()),
    ));
    d.push(Attr::new(
        "printer-info",
        Value::text(p.identity.info.clone()),
    ));
    d.push(Attr::new(
        "printer-make-and-model",
        Value::text(format!("{} {}", p.identity.manufacturer, p.identity.model)),
    ));
    d.push(Attr::new("printer-device-id", Value::text(p.device_id())));
    d.push(Attr::new(
        "printer-uuid",
        Value::uri(format!("urn:uuid:{}", env.printer_uuid)),
    ));
    d.push(Attr::enum_("printer-state", state));
    d.push(Attr::keywords(
        "printer-state-reasons",
        &printer_state_reasons(&snap, p),
    ));
    d.push(Attr::new("printer-state-message", Value::text("")));
    d.push(Attr::boolean(
        "printer-is-accepting-jobs",
        snap.accepting_jobs,
    ));
    d.push(Attr::int(
        "queued-job-count",
        i32::try_from(snap.queued_jobs).unwrap_or(i32::MAX),
    ));
    d.push(Attr::int(
        "printer-up-time",
        secs_since(env.now, env.started).max(1),
    ));
    d.push(Attr::new("printer-current-time", date_time(env.now)));
    d.push(Attr::keywords(
        "ipp-versions-supported",
        &VERSIONS_SUPPORTED,
    ));
    d.push(Attr::keywords(
        "ipp-features-supported",
        &["ipp-everywhere"],
    ));
    d.push(Attr::many(
        "operations-supported",
        OPERATIONS_SUPPORTED
            .iter()
            .map(|o| Value::Enum(*o))
            .collect(),
    ));
    d.push(Attr::new(
        "charset-configured",
        Value::Charset("utf-8".into()),
    ));
    d.push(Attr::many(
        "charset-supported",
        vec![
            Value::Charset("utf-8".into()),
            Value::Charset("us-ascii".into()),
        ],
    ));
    d.push(Attr::new(
        "natural-language-configured",
        Value::NaturalLanguage(env.language.into()),
    ));
    d.push(Attr::new(
        "generated-natural-language-supported",
        Value::NaturalLanguage(env.language.into()),
    ));
    d.push(Attr::new(
        "document-format-default",
        Value::mime(c.default_format.clone()),
    ));
    d.push(Attr::many(
        "document-format-supported",
        c.formats.iter().map(|f| Value::mime(f.clone())).collect(),
    ));
    d.push(Attr::keywords("pdl-override-supported", &["attempted"]));
    d.push(Attr::keywords("compression-supported", &c.compressions));
    d.push(Attr::boolean("color-supported", c.color));
    d.push(Attr::int("pages-per-minute", c.ppm));
    if let Some(ppm) = c.ppm_color {
        d.push(Attr::int("pages-per-minute-color", ppm));
    }
    d.push(Attr::boolean(
        "multiple-document-jobs-supported",
        c.multiple_documents,
    ));
    d.push(Attr::int(
        "multiple-operation-time-out",
        i32::try_from(timing.ipp_multiple_operation_timeout.as_secs().max(1)).unwrap_or(i32::MAX),
    ));
    d.push(Attr::keywords(
        "multiple-operation-timeout-action",
        &["abort-job"],
    ));
    d.push(Attr::keywords(
        "job-creation-attributes-supported",
        &job_creation_attributes(),
    ));
    d.push(Attr::boolean("job-ids-supported", false));
    d.push(Attr::keywords(
        "which-jobs-supported",
        &["completed", "not-completed"],
    ));
    d.push(Attr::new(
        "job-k-octets-supported",
        Value::range(0, i32::MAX),
    ));
    d.push(Attr::boolean("page-ranges-supported", false));
    d.push(Attr::keywords(
        "media-col-supported",
        &["media-size", "media-type", "media-color", "media-source"],
    ));
    d.push(Attr::many(
        "media-size-supported",
        c.media
            .iter()
            .filter_map(|m| media_dimensions(m))
            .map(|(x, y)| {
                Value::Collection(vec![
                    member("x-dimension", Value::Integer(x)),
                    member("y-dimension", Value::Integer(y)),
                ])
            })
            .collect(),
    ));
    if !c.media_types.is_empty() {
        d.push(Attr::keywords("media-type-supported", &c.media_types));
    }
    if !c.media_colors.is_empty() {
        d.push(Attr::keywords("media-color-supported", &c.media_colors));
    }
    if !c.sources.is_empty() {
        d.push(Attr::keywords("media-source-supported", &c.sources));
    }
    // ready media: bins that still hold paper
    let ready: Vec<&crate::profile::InputBin> =
        p.inputs().iter().filter(|b| b.level != 0).collect();
    if !ready.is_empty() {
        let mut names: Vec<Value> = Vec::new();
        for b in &ready {
            let v = Value::keyword(b.media_size.clone());
            if !names.contains(&v) {
                names.push(v);
            }
        }
        d.push(Attr::many("media-ready", names));
        d.push(Attr::many(
            "media-col-ready",
            ready
                .iter()
                .map(|b| {
                    let mut extra = Vec::new();
                    if let Some(t) = &b.media_type {
                        extra.push(member("media-type", Value::keyword(t.clone())));
                    }
                    if let Some(col) = &b.media_color {
                        extra.push(member("media-color", Value::keyword(col.clone())));
                    }
                    extra.push(member(
                        "media-source",
                        Value::keyword(source_keyword(&b.name)),
                    ));
                    media_col_of(&b.media_size, extra)
                })
                .collect(),
        ));
    }
    // markers
    let cons = p.consumables();
    if !cons.is_empty() {
        d.push(Attr::many(
            "marker-names",
            cons.iter().map(|m| Value::name(m.name.clone())).collect(),
        ));
        d.push(Attr::many(
            "marker-types",
            cons.iter()
                .map(|m| Value::keyword(marker_type(&m.kind)))
                .collect(),
        ));
        d.push(Attr::many(
            "marker-colors",
            cons.iter().map(|m| Value::name(marker_color(m))).collect(),
        ));
        d.push(Attr::many(
            "marker-levels",
            cons.iter().map(|m| Value::Integer(m.level)).collect(),
        ));
        d.push(Attr::many(
            "marker-low-levels",
            cons.iter().map(|_| Value::Integer(10)).collect(),
        ));
        d.push(Attr::many(
            "marker-high-levels",
            cons.iter().map(|_| Value::Integer(100)).collect(),
        ));
    }

    // job template: defaults and supported lists
    let mut t: Vec<Attr> = Vec::new();
    t.push(Attr::int("copies-default", c.default_copies));
    t.push(Attr::new(
        "copies-supported",
        Value::range(c.copies.0, c.copies.1),
    ));
    t.push(Attr::new(
        "sides-default",
        Value::keyword(c.default_sides.clone()),
    ));
    t.push(Attr::keywords("sides-supported", &c.sides));
    t.push(Attr::enum_(
        "orientation-requested-default",
        c.default_orientation,
    ));
    t.push(Attr::many(
        "orientation-requested-supported",
        c.orientations.iter().map(|o| Value::Enum(*o)).collect(),
    ));
    t.push(Attr::enum_("print-quality-default", c.default_quality));
    t.push(Attr::many(
        "print-quality-supported",
        c.qualities.iter().map(|o| Value::Enum(*o)).collect(),
    ));
    t.push(Attr::new(
        "printer-resolution-default",
        Value::dpi(c.default_resolution.0, c.default_resolution.1),
    ));
    t.push(Attr::many(
        "printer-resolution-supported",
        c.resolutions.iter().map(|r| Value::dpi(r.0, r.1)).collect(),
    ));
    t.push(Attr::new(
        "media-default",
        Value::keyword(c.default_media.clone()),
    ));
    t.push(Attr::keywords("media-supported", &c.media));
    t.push(Attr::new(
        "media-col-default",
        media_col_of(&c.default_media, Vec::new()),
    ));
    t.push(Attr::new(
        "print-color-mode-default",
        Value::keyword(c.default_color_mode.clone()),
    ));
    t.push(Attr::keywords("print-color-mode-supported", &c.color_modes));
    t.push(Attr::int("number-up-default", c.default_number_up));
    t.push(Attr::many(
        "number-up-supported",
        c.number_up.iter().map(|n| Value::Integer(*n)).collect(),
    ));
    t.push(Attr::int("job-priority-default", c.default_priority));
    t.push(Attr::int("job-priority-supported", c.priority.1));
    t.push(Attr::keywords("job-hold-until-default", &["no-hold"]));
    t.push(Attr::keywords("job-hold-until-supported", &["no-hold"]));
    t.push(Attr::enum_("finishings-default", 3));
    t.push(Attr::many("finishings-supported", vec![Value::Enum(3)]));
    if let Some(first) = c.outputs.first() {
        t.push(Attr::new(
            "output-bin-default",
            Value::keyword(first.clone()),
        ));
        t.push(Attr::keywords("output-bin-supported", &c.outputs));
    }
    (d, t)
}

fn marker_type(kind: &str) -> String {
    match kind {
        "Toner" => "toner".into(),
        "Ink" => "ink-cartridge".into(),
        "WasteToner" => "waste-toner".into(),
        "WasteInk" => "waste-ink".into(),
        other => kebab(other),
    }
}

fn marker_color(m: &crate::profile::Consumable) -> String {
    if let Some(rgb) = &m.rgb {
        return format!("#{}", rgb.trim_start_matches('#').to_ascii_uppercase());
    }
    match m.color.as_deref() {
        Some("Black") => "#000000",
        Some("Cyan") => "#00FFFF",
        Some("Magenta") => "#FF00FF",
        Some("Yellow") => "#FFFF00",
        _ => "none",
    }
    .to_string()
}

// ---------------------------------------------------------------------------------------------
// Job attributes
// ---------------------------------------------------------------------------------------------

/// Every job-description attribute name a job can report.
pub const JOB_DESCRIPTION_NAMES: [&str; 20] = [
    "job-id",
    "job-uri",
    "job-uuid",
    "job-state",
    "job-state-reasons",
    "job-state-message",
    "job-name",
    "job-originating-user-name",
    "job-printer-uri",
    "job-printer-up-time",
    "time-at-creation",
    "time-at-processing",
    "time-at-completed",
    "date-time-at-creation",
    "number-of-documents",
    "job-impressions-completed",
    "job-media-sheets-completed",
    "job-k-octets",
    "job-k-octets-processed",
    "number-of-intervening-jobs",
];

/// The job's UUID: the printer's UUID with its low 16 bits and the job id in the last 6 bytes,
/// so it stays in the fake namespace.
pub fn job_uuid(printer: Uuid, job_id: u32) -> Uuid {
    let mut b = *printer.as_bytes();
    b[10] = b[14];
    b[11] = b[15];
    b[12..16].copy_from_slice(&job_id.to_be_bytes());
    Uuid::from_bytes(b)
}

/// `job-uri` of a job: the printer URI and the job id.
pub fn job_uri(printer_uri: &str, id: u32) -> String {
    format!("{}/{id}", printer_uri.trim_end_matches('/'))
}

/// The IPP view of a job's state: `(job-state enum, job-state-reasons)`.
pub fn job_state_view(job: &PrintJob) -> (i32, Vec<String>) {
    use JobStateReason as R;
    let mut out: Vec<String> = Vec::new();
    let mut add = |s: &str| {
        if !out.iter().any(|x| x == s) {
            out.push(s.to_string());
        }
    };
    let state = match job.state {
        JobState::Pending => 3,
        JobState::PendingHeld => 4,
        JobState::Processing | JobState::Started | JobState::Terminating => 5,
        JobState::ProcessingStopped => 6,
        JobState::Completed => 9,
        JobState::Canceled => 7,
        JobState::Aborted => 8,
    };
    if job.state == JobState::Terminating {
        add("processing-to-stop-point");
    }
    if job.state == JobState::PendingHeld {
        add("job-hold-until-specified");
    }
    for r in &job.reasons {
        match r {
            R::JobIncoming => {
                add("job-incoming");
                if job.state == JobState::Pending && !job.last_document_received {
                    add("job-data-insufficient");
                }
            }
            R::JobPrinting => add("job-printing"),
            R::JobCompletedSuccessfully => add("job-completed-successfully"),
            R::JobCompletedWithWarnings => add("job-completed-with-warnings"),
            R::JobCompletedWithErrors => add("job-completed-with-errors"),
            R::JobCanceledByUser => add("job-canceled-by-user"),
            R::JobCanceledAtDevice => add("job-canceled-at-device"),
            R::PrinterStopped => add("printer-stopped"),
            R::DocumentTimeoutError => {
                add("aborted-by-system");
                add("submission-interrupted");
            }
            R::DocumentFormatError | R::UnsupportedDocumentFormat => {
                add("aborted-by-system");
                add("document-format-error");
            }
            R::CompressionError | R::UnsupportedCompression => {
                add("aborted-by-system");
                add("compression-error");
            }
            R::DocumentTransferError | R::DocumentAccessError => {
                add("aborted-by-system");
                add("document-access-error");
            }
            R::JobPasswordWait => add("job-password-wait"),
            R::None => {}
        }
    }
    if out.is_empty() {
        out.push("none".into());
    }
    (state, out)
}

/// A job's attributes as `(job-description, job-template)`.
pub fn job_attributes(env: &Env<'_>, job: &PrintJob) -> (Vec<Attr>, Vec<Attr>) {
    let (state, reasons) = job_state_view(job);
    let rel = |t: SystemTime| secs_since(t, env.started).max(0);
    let two_sided = job.ipp.as_ref().is_some_and(|i| i.two_sided());
    let sheets = i32::try_from(job.media_sheets_completed).unwrap_or(i32::MAX);
    let total_octets: u64 = job.documents.iter().map(|d| d.size).sum();
    let mut d: Vec<Attr> = Vec::new();
    d.push(Attr::int(
        "job-id",
        i32::try_from(job.id).unwrap_or(i32::MAX),
    ));
    d.push(Attr::new(
        "job-uri",
        Value::uri(job_uri(env.printer_uri, job.id)),
    ));
    d.push(Attr::new(
        "job-uuid",
        Value::uri(format!("urn:uuid:{}", job_uuid(env.printer_uuid, job.id))),
    ));
    d.push(Attr::enum_("job-state", state));
    d.push(Attr::keywords("job-state-reasons", &reasons));
    d.push(Attr::new(
        "job-state-message",
        Value::text(reasons[0].clone()),
    ));
    d.push(Attr::new("job-name", Value::name(job.name.clone())));
    d.push(Attr::new(
        "job-originating-user-name",
        Value::name(if job.user.is_empty() {
            "anonymous".to_string()
        } else {
            job.user.clone()
        }),
    ));
    d.push(Attr::new("job-printer-uri", Value::uri(env.printer_uri)));
    d.push(Attr::int(
        "job-printer-up-time",
        secs_since(env.now, env.started).max(1),
    ));
    d.push(Attr::int("time-at-creation", rel(job.created)));
    d.push(Attr::new(
        "time-at-processing",
        job.started
            .map_or_else(Value::no_value, |t| Value::Integer(rel(t))),
    ));
    d.push(Attr::new(
        "time-at-completed",
        job.ended
            .map_or_else(Value::no_value, |t| Value::Integer(rel(t))),
    ));
    d.push(Attr::new("date-time-at-creation", date_time(job.created)));
    d.push(Attr::int(
        "number-of-documents",
        i32::try_from(job.documents.len()).unwrap_or(i32::MAX),
    ));
    d.push(Attr::int(
        "job-impressions-completed",
        sheets.saturating_mul(if two_sided { 2 } else { 1 }),
    ));
    d.push(Attr::int("job-media-sheets-completed", sheets));
    d.push(Attr::int(
        "job-k-octets",
        i32::try_from(total_octets.div_ceil(1024)).unwrap_or(i32::MAX),
    ));
    d.push(Attr::int(
        "job-k-octets-processed",
        i32::try_from(job.k_octets_processed()).unwrap_or(i32::MAX),
    ));
    if job.state == JobState::Pending || job.state == JobState::PendingHeld {
        let ahead = env
            .ctx
            .jobs
            .active()
            .iter()
            .filter(|j| j.id < job.id)
            .count();
        d.push(Attr::int(
            "number-of-intervening-jobs",
            i32::try_from(ahead).unwrap_or(i32::MAX),
        ));
    }
    let t = job
        .ipp
        .as_ref()
        .map(|i| i.attrs.clone())
        .unwrap_or_default();
    (d, t)
}

// ---------------------------------------------------------------------------------------------
// requested-attributes
// ---------------------------------------------------------------------------------------------

/// What `requested-attributes` selected.
pub struct Selection {
    pub attrs: Vec<Attr>,
    /// Requested names that are neither a group keyword nor an attribute this object type has.
    pub unknown: Vec<String>,
}

/// Select from `description` and `template` by the `requested-attributes` names.
///
/// `requested == None` means the operation's default: everything, or `default_names` when the
/// operation has a shorter default (Get-Jobs). Group keywords: `all`, `job-template`,
/// `printer-description` / `job-description` (the description attributes), plus the
/// `job-template`-like groups that this server has nothing for (`document-description`,
/// `subscription-description`, ... select nothing and are not errors). `known` lists every
/// attribute name the object type can have, so a present-but-absent one is not "unsupported".
pub fn select(
    requested: Option<&[String]>,
    default_names: Option<&[&str]>,
    description: Vec<Attr>,
    template: Vec<Attr>,
    known: &BTreeSet<String>,
) -> Selection {
    let mut all = false;
    let mut want_desc = false;
    let mut want_template = false;
    let mut names: BTreeSet<String> = BTreeSet::new();
    let mut unknown: Vec<String> = Vec::new();
    match requested {
        None => match default_names {
            None => all = true,
            Some(list) => names.extend(list.iter().map(|s| s.to_string())),
        },
        Some(list) => {
            for n in list {
                match n.as_str() {
                    "all" => all = true,
                    "job-template" => want_template = true,
                    "printer-description" | "job-description" => want_desc = true,
                    "document-description"
                    | "document-template"
                    | "subscription-description"
                    | "subscription-template"
                    | "event-notification-attributes" => {}
                    other => {
                        if !known.contains(other) && !unknown.iter().any(|u| u == other) {
                            unknown.push(other.to_string());
                        }
                        names.insert(other.to_string());
                    }
                }
            }
        }
    }
    let mut attrs: Vec<Attr> = Vec::new();
    for a in description {
        if all || want_desc || names.contains(&a.name) {
            attrs.push(a);
        }
    }
    for a in template {
        if all || want_template || names.contains(&a.name) {
            attrs.push(a);
        }
    }
    Selection { attrs, unknown }
}

/// Names a printer can report (for `known` of [`select`]).
pub fn printer_known_names(env: &Env<'_>) -> BTreeSet<String> {
    let (d, t) = printer_attributes(env);
    d.into_iter().chain(t).map(|a| a.name).collect()
}

/// Names a job can report.
pub fn job_known_names() -> BTreeSet<String> {
    let mut s: BTreeSet<String> = JOB_DESCRIPTION_NAMES
        .iter()
        .map(|n| n.to_string())
        .collect();
    s.extend(JOB_TEMPLATE_NAMES.iter().map(|n| n.to_string()));
    s.insert("output-bin".into());
    s
}

// ---------------------------------------------------------------------------------------------
// Job-template validation
// ---------------------------------------------------------------------------------------------

/// Check one job-template attribute against the printer's lists. `None`: acceptable. `Some(a)`:
/// the attribute as it goes into the unsupported-attributes group (offending values only; for
/// an unknown attribute the out-of-band `unsupported` value).
pub fn check_template_attr(caps: &Caps, a: &Attr) -> Option<Attr> {
    let single_bad = a.values.len() != 1;
    let v = a.values.first()?;
    let whole = || Some(a.clone());
    let one = |ok: bool| if ok && !single_bad { None } else { whole() };
    match a.name.as_str() {
        "copies" => {
            one(matches!(v, Value::Integer(n) if (caps.copies.0..=caps.copies.1).contains(n)))
        }
        "sides" => one(matches!(v, Value::Keyword(k) if caps.sides.contains(k))),
        "orientation-requested" => {
            one(matches!(v, Value::Enum(e) if caps.orientations.contains(e)))
        }
        "print-quality" => one(matches!(v, Value::Enum(e) if caps.qualities.contains(e))),
        "printer-resolution" => one(
            matches!(v, Value::Resolution { x, y, units: 3 } if caps.resolutions.contains(&(*x, *y))),
        ),
        "media" => one(matches!(v, Value::Keyword(k) | Value::Name(k) if caps.media.contains(k))),
        "media-col" => {
            if single_bad {
                return whole();
            }
            check_media_col(caps, v).map(|bad| Attr::new(a.name.clone(), bad))
        }
        "print-color-mode" => one(matches!(v, Value::Keyword(k) if caps.color_modes.contains(k))),
        "number-up" => one(matches!(v, Value::Integer(n) if caps.number_up.contains(n))),
        "job-priority" => {
            one(matches!(v, Value::Integer(n) if (caps.priority.0..=caps.priority.1).contains(n)))
        }
        "job-hold-until" => one(matches!(v, Value::Keyword(k) if k == "no-hold")),
        "finishings" => {
            let bad: Vec<Value> = a
                .values
                .iter()
                .filter(|x| !matches!(x, Value::Enum(3)))
                .cloned()
                .collect();
            if bad.is_empty() {
                None
            } else {
                Some(Attr::many(a.name.clone(), bad))
            }
        }
        "output-bin" => one(matches!(v, Value::Keyword(k) if caps.outputs.contains(k))),
        _ => Some(Attr::unsupported(a.name.clone())),
    }
}

/// `None` when the collection is acceptable, else the collection reduced to the unsupported
/// members (a `media-size` member with the offending dimensions inside).
fn check_media_col(caps: &Caps, v: &Value) -> Option<Value> {
    let Value::Collection(members) = v else {
        return Some(v.clone());
    };
    let mut bad: Vec<Member> = Vec::new();
    for m in members {
        let ok = match m.name.as_str() {
            "media-size" => match m.values.as_slice() {
                [Value::Collection(dims)] => {
                    let get = |n: &str| {
                        dims.iter()
                            .find(|d| d.name == n)
                            .and_then(|d| d.values.first())
                            .and_then(Value::as_int)
                    };
                    match (get("x-dimension"), get("y-dimension")) {
                        (Some(x), Some(y)) if dims.len() == 2 => caps
                            .media
                            .iter()
                            .filter_map(|n| media_dimensions(n))
                            .any(|d| d == (x, y)),
                        _ => false,
                    }
                }
                _ => false,
            },
            "media-type" => {
                matches!(m.values.as_slice(), [Value::Keyword(k) | Value::Name(k)] if caps.media_types.contains(k))
            }
            "media-color" => {
                matches!(m.values.as_slice(), [Value::Keyword(k) | Value::Name(k)] if caps.media_colors.contains(k))
            }
            "media-source" => {
                matches!(m.values.as_slice(), [Value::Keyword(k)] if caps.sources.contains(k))
            }
            _ => false,
        };
        if !ok {
            bad.push(m.clone());
        }
    }
    if bad.is_empty() {
        None
    } else {
        Some(Value::Collection(bad))
    }
}

/// The WS-Print ticket words for the accepted IPP attributes, so the job shows a typed ticket
/// on the control plane whichever protocol created it.
pub fn ticket_for(
    caps: &Caps,
    attrs: &[Attr],
    job_name: &str,
    user: &str,
) -> crate::wsd::ticket::PrintTicket {
    use crate::wsd::ticket::{
        DocumentProcessing, Honored, JobDescription, JobProcessing, NumberUp, PrintTicket,
        Resolution,
    };
    let get = |n: &str| attrs.iter().find(|a| a.name == n).and_then(|a| a.first());
    let mut job = JobProcessing::default();
    if let Some(n) = get("copies").and_then(Value::as_int) {
        job.copies = Some(Honored::soft(u32::try_from(n).unwrap_or(1)));
    }
    if let Some(n) = get("job-priority").and_then(Value::as_int) {
        job.priority = Some(Honored::soft(u32::try_from(n).unwrap_or(1)));
    }
    let mut doc = DocumentProcessing::default();
    let size = match (get("media"), get("media-col")) {
        (Some(m), _) => m.as_str().map(str::to_string),
        (None, Some(Value::Collection(ms))) => ms
            .iter()
            .find(|m| m.name == "media-size")
            .and_then(|m| m.values.first())
            .and_then(Value::as_collection)
            .and_then(|dims| {
                let g = |n: &str| {
                    dims.iter()
                        .find(|d| d.name == n)
                        .and_then(|d| d.values.first())
                        .and_then(Value::as_int)
                };
                let want = (g("x-dimension")?, g("y-dimension")?);
                caps.media
                    .iter()
                    .find(|n| media_dimensions(n) == Some(want))
                    .cloned()
            }),
        _ => None,
    };
    doc.media_size_name = size.map(Honored::soft);
    doc.orientation = get("orientation-requested")
        .and_then(Value::as_int)
        .and_then(|o| match o {
            ORIENTATION_PORTRAIT => Some("Portrait"),
            ORIENTATION_LANDSCAPE => Some("Landscape"),
            ORIENTATION_REVERSE_LANDSCAPE => Some("ReverseLandscape"),
            ORIENTATION_REVERSE_PORTRAIT => Some("ReversePortrait"),
            _ => None,
        })
        .map(|s| Honored::soft(s.to_string()));
    doc.print_quality = get("print-quality")
        .and_then(Value::as_int)
        .and_then(|q| match q {
            QUALITY_DRAFT => Some("Draft"),
            QUALITY_NORMAL => Some("Normal"),
            QUALITY_HIGH => Some("High"),
            _ => None,
        })
        .map(|s| Honored::soft(s.to_string()));
    doc.sides = get("sides")
        .and_then(Value::as_str)
        .and_then(|s| match s {
            "one-sided" => Some("OneSided"),
            "two-sided-long-edge" => Some("TwoSidedLongEdge"),
            "two-sided-short-edge" => Some("TwoSidedShortEdge"),
            _ => None,
        })
        .map(|s| Honored::soft(s.to_string()));
    if let Some(Value::Resolution { x, y, .. }) = get("printer-resolution") {
        doc.resolution = Some(Resolution {
            must_honor: false,
            width: u32::try_from(*x).unwrap_or(0),
            height: u32::try_from(*y).ok(),
        });
    }
    if let Some(n) = get("number-up").and_then(Value::as_int) {
        doc.number_up = Some(NumberUp {
            pages_per_sheet: Honored::soft(u32::try_from(n).unwrap_or(1)),
            direction: Honored::soft("RightDown".to_string()),
        });
    }
    PrintTicket {
        description: Some(JobDescription {
            name: job_name.to_string(),
            user: user.to_string(),
        }),
        job,
        document: doc,
    }
}

/// Seconds as the `Duration` `printer-up-time` style helpers need in tests.
pub fn epoch_plus(secs: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps() -> Caps {
        let mut p = PrinterProfile::generic();
        p.documents.formats = vec!["application/pdf".into()];
        Caps::from_profile(&p)
    }

    #[test]
    fn date_time_is_utc_and_well_formed() {
        let Value::DateTime(d) = date_time(epoch_plus(1_767_225_600 + 3661)) else {
            panic!()
        };
        assert_eq!(u16::from_be_bytes([d[0], d[1]]), 2026);
        assert_eq!((d[2], d[3], d[4], d[5], d[6]), (1, 1, 1, 1, 1));
        assert_eq!((d[8], d[9], d[10]), (b'+', 0, 0));
    }

    #[test]
    fn template_attributes_are_checked_against_the_lists() {
        let c = caps();
        let ok = |a: Attr| check_template_attr(&c, &a).is_none();
        assert!(ok(Attr::int("copies", 2)));
        assert!(!ok(Attr::int("copies", 0)));
        assert!(!ok(Attr::int("copies", 1000)));
        assert!(!ok(Attr::new("copies", Value::keyword("x"))));
        assert!(!ok(Attr::many(
            "copies",
            vec![Value::Integer(1), Value::Integer(2)]
        )));
        assert!(ok(Attr::new("sides", Value::keyword("one-sided"))));
        assert!(!ok(Attr::new(
            "sides",
            Value::keyword("two-sided-long-edge")
        )));
        assert!(ok(Attr::enum_("orientation-requested", 4)));
        assert!(!ok(Attr::enum_("orientation-requested", 5)));
        assert!(ok(Attr::enum_("print-quality", 5)));
        assert!(ok(Attr::new("printer-resolution", Value::dpi(600, 600))));
        assert!(!ok(Attr::new("printer-resolution", Value::dpi(1200, 1200))));
        assert!(ok(Attr::new("media", Value::keyword("iso_a4_210x297mm"))));
        assert!(!ok(Attr::new("media", Value::keyword("iso_a0_841x1189mm"))));
        assert!(ok(Attr::new("job-hold-until", Value::keyword("no-hold"))));
        assert!(!ok(Attr::new(
            "job-hold-until",
            Value::keyword("indefinite")
        )));
        assert!(!ok(Attr::enum_("finishings", 4)));
        let un = check_template_attr(&c, &Attr::int("x-nonsense", 1)).unwrap();
        assert_eq!(un.values, [Value::OutOfBand(0x10)]);
    }

    #[test]
    fn media_col_reports_only_the_offending_members() {
        let c = caps();
        let col = Value::Collection(vec![
            media_size(21590, 27940),
            member("media-type", Value::keyword("hologram")),
        ]);
        let bad = check_template_attr(&c, &Attr::new("media-col", col)).unwrap();
        let members = bad.values[0].as_collection().unwrap();
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].name, "media-type");
        let good = Value::Collection(vec![media_size(21000, 29700)]);
        assert!(check_template_attr(&c, &Attr::new("media-col", good)).is_none());
        let wrong_size = Value::Collection(vec![media_size(1, 2)]);
        assert!(check_template_attr(&c, &Attr::new("media-col", wrong_size)).is_some());
    }

    #[test]
    fn selection_groups_names_and_unknowns() {
        let d = vec![Attr::int("a", 1), Attr::int("b", 2)];
        let t = vec![Attr::int("c", 3)];
        let known: BTreeSet<String> = ["a", "b", "c", "d"].iter().map(|s| s.to_string()).collect();
        fn names(s: &Selection) -> Vec<&str> {
            s.attrs.iter().map(|a| a.name.as_str()).collect()
        }
        let s = select(None, None, d.clone(), t.clone(), &known);
        assert_eq!(names(&s), ["a", "b", "c"]);
        let s = select(None, Some(&["b"]), d.clone(), t.clone(), &known);
        assert_eq!(names(&s), ["b"]);
        let req = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let s = select(
            Some(&req(&["job-template"])),
            None,
            d.clone(),
            t.clone(),
            &known,
        );
        assert_eq!(names(&s), ["c"]);
        let s = select(
            Some(&req(&["printer-description", "zzz", "d"])),
            None,
            d.clone(),
            t.clone(),
            &known,
        );
        assert_eq!(names(&s), ["a", "b"]);
        assert_eq!(s.unknown, ["zzz"]);
    }

    #[test]
    fn job_state_view_maps_every_engine_state() {
        let mut j = PrintJob::new(1, "n", "u");
        assert_eq!(
            job_state_view(&j),
            (
                3,
                vec!["job-incoming".into(), "job-data-insufficient".into()]
            )
        );
        j.state = JobState::Terminating;
        j.reasons = vec![JobStateReason::JobCanceledByUser];
        let (s, r) = job_state_view(&j);
        assert_eq!(s, 5);
        assert_eq!(r, ["processing-to-stop-point", "job-canceled-by-user"]);
        j.state = JobState::Aborted;
        j.reasons = vec![JobStateReason::DocumentTimeoutError];
        assert_eq!(
            job_state_view(&j),
            (
                8,
                vec!["aborted-by-system".into(), "submission-interrupted".into()]
            )
        );
        j.state = JobState::Completed;
        j.reasons = vec![JobStateReason::JobCompletedSuccessfully];
        assert_eq!(job_state_view(&j).0, 9);
    }

    #[test]
    fn job_uuids_stay_in_the_fake_namespace() {
        let p = Uuid::parse_str("00000000-0000-4000-8000-0000000000f4").unwrap();
        let j = job_uuid(p, 7);
        assert!(crate::lab::uuid_is_fake(&j));
        assert_ne!(job_uuid(p, 8), j);
    }
}
