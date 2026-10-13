//! Element builders: the print service's bodies as namespaced `xml::Element` trees.
//!
//! Every builder returns a tree whose children follow the schema's sequence order (the
//! tables in `print_schema` are the law; each builder is tested against them). Text values
//! come from the profile or the live state; nothing is assembled by string concatenation.
//!
//! Also here the validator, re-exported:
//!
//! * [`schema`], the validator re-exported for the rest of the crate;

use std::time::SystemTime;

use super::ctx::{format_datetime, ConditionRecord, PrinterStatusSnapshot};
use super::types::{
    DeviceCondition, JobState as WireJobState, PrintEvent, PrintJob, PrinterModel, PrinterState,
    Severity,
};
use super::xml::{ns, Element, PrefixMap, QName};
use crate::profile::{Consumable, PrinterProfile};

/// The reply validator and value domains (see `print_schema`).
pub mod schema {
    pub use crate::wsd::print_schema::*;
}

// ---------------------------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------------------------

fn w(name: &str) -> Element {
    Element::new(ns::PRINT, name)
}

fn wt(name: &str, text: impl Into<String>) -> Element {
    Element::text_el(ns::PRINT, name, text)
}

fn wb(name: &str, v: bool) -> Element {
    wt(name, if v { "true" } else { "false" })
}

fn wn(name: &str, n: impl std::fmt::Display) -> Element {
    wt(name, n.to_string())
}

fn localized(name: &str, text: &str, lang: &str) -> Element {
    wt(name, text).with_attr_ns(QName::new(ns::XML, "lang"), lang)
}

/// `wprt:<name>` qualified name.
pub fn wprt(name: &str) -> QName {
    QName::new(ns::PRINT, name)
}

/// Serialize with the conventional prefixes (for logs and string-based callers).
pub fn to_xml(el: &Element) -> String {
    el.to_xml(&PrefixMap::wsd())
}

// ---------------------------------------------------------------------------------------------
// Printer sections
// ---------------------------------------------------------------------------------------------

/// `PrinterDescription`.
pub fn printer_description(p: &PrinterProfile) -> Element {
    let lang = p.identity.language.as_str();
    let mut d = w("PrinterDescription")
        .with_child(wb("ColorSupported", p.color_supported()))
        .with_child(wt("DeviceId", p.device_id()))
        .with_child(wb(
            "MultipleDocumentJobsSupported",
            p.description.multiple_document_jobs,
        ))
        .with_child(wn("PagesPerMinute", p.description.pages_per_minute));
    if let Some(c) = p.description.pages_per_minute_color {
        d = d.with_child(wn("PagesPerMinuteColor", c));
    }
    d = d
        .with_child(localized("PrinterName", p.friendly_name(), lang))
        .with_child(localized("PrinterInfo", &p.identity.info, lang))
        .with_child(localized("PrinterLocation", &p.identity.location, lang));
    if p.versions.v11 {
        if let Some(id) = &p.extensions.model_id {
            d = d.with_child(Element::text_el(ns::PRINT11, "DeviceModelId", id.as_str()));
        }
    }
    d
}

fn consumable_entry(c: &Consumable, v11: bool) -> Element {
    let mut e = w("ConsumableEntry")
        .with_attr("Name", c.name.as_str())
        .with_child(wt("Type", c.kind.as_str()));
    if let Some(col) = &c.color {
        e = e.with_child(wt("Color", col.as_str()));
    }
    e = e.with_child(wn("Level", c.level));
    if let Some(m) = &c.model {
        e = e.with_child(wt("Model", m.as_str()));
    }
    if v11 {
        if let Some(rgb) = &c.rgb {
            e = e.with_child(Element::text_el(
                ns::PRINT11,
                "ColorRepresentation",
                rgb.to_ascii_uppercase(),
            ));
        }
    }
    e
}

/// `PrinterConfiguration`: event rate, storage, consumables, bins, finishings, version flags.
pub fn printer_configuration(p: &PrinterProfile) -> Element {
    let mut c =
        w("PrinterConfiguration").with_child(wn("PrinterEventRate", p.description.event_rate));
    if !p.storage.is_empty() {
        c = c.with_child(w("Storage").with_children(p.storage.iter().map(|s| {
            w("StorageEntry")
                .with_attr("Name", s.name.as_str())
                .with_child(wt("Type", s.kind.as_str()))
                .with_child(wn("Size", s.size_mb))
                .with_child(wn("Free", s.free))
        })));
    }
    if !p.consumables().is_empty() {
        c = c.with_child(
            w("Consumables").with_children(
                p.consumables()
                    .iter()
                    .map(|k| consumable_entry(k, p.versions.v11)),
            ),
        );
    }
    if !p.inputs().is_empty() {
        c = c.with_child(w("InputBins").with_children(p.inputs().iter().map(|b| {
            let mut e = w("InputBinEntry")
                .with_attr("Name", b.name.as_str())
                .with_child(wt("FeedDirection", b.feed_direction.as_str()))
                .with_child(wt("MediaSize", b.media_size.as_str()));
            if let Some(t) = &b.media_type {
                e = e.with_child(wt("MediaType", t.as_str()));
            }
            if let Some(col) = &b.media_color {
                e = e.with_child(wt("MediaColor", col.as_str()));
            }
            e.with_child(wn("Capacity", b.capacity))
                .with_child(wn("Level", b.level))
        })));
    }
    if let Some(f) = &p.finishings {
        let duplex = f.duplexer.unwrap_or_else(|| p.duplex());
        c = c.with_child(finishings(
            f.collation,
            f.jog_offset,
            duplex,
            f.stapler,
            f.hole_punch,
        ));
    } else if p.duplex() {
        c = c.with_child(finishings(false, false, true, false, false));
    }
    if !p.outputs().is_empty() {
        c = c.with_child(w("OutputBins").with_children(p.outputs().iter().map(|b| {
            w("OutputBinEntry")
                .with_attr("Name", b.name.as_str())
                .with_child(wn("Capacity", b.capacity))
                .with_child(wn("Level", b.level))
        })));
    }
    // extension elements (other namespaces), after the schema's own children
    if p.versions.v11 {
        c = c.with_child(Element::text_el(ns::PRINT11, "SupportsWSPrintv11", "true"));
    }
    if p.versions.v12 {
        c = c.with_child(Element::text_el(ns::PRINT12, "SupportsWSPrintV12", "true"));
    }
    if p.versions.v20 {
        c = c.with_child(Element::text_el(ns::PRINT20, "SupportsWSPrintV20", "true"));
        c = c.with_child(print_device_capabilities_change_id(p));
    }
    c
}

fn finishings(collate: bool, jog: bool, duplex: bool, staple: bool, punch: bool) -> Element {
    w("Finishings")
        .with_child(wb("CollationSupported", collate))
        .with_child(wb("JogOffsetSupported", jog))
        .with_child(wb("DuplexerInstalled", duplex))
        .with_child(wb("StaplerInstalled", staple))
        .with_child(wb("HolePunchInstalled", punch))
}

/// `DefaultPrintTicket`.
pub fn default_print_ticket(p: &PrinterProfile) -> Element {
    let d = &p.default_ticket;
    let mut dp = w("DocumentProcessing");
    if let Some(v) = &d.media_size {
        dp = dp.with_child(wt("MediaSizeName", v.as_str()));
    }
    if let Some(v) = &d.media_type {
        dp = dp.with_child(wt("MediaType", v.as_str()));
    }
    if let Some(v) = &d.media_color {
        dp = dp.with_child(wt("MediaColor", v.as_str()));
    }
    if let Some(n) = d.pages_per_sheet {
        let dir = p
            .print
            .number_up_directions
            .first()
            .map_or("RightDown", String::as_str);
        dp = dp.with_child(
            w("NumberUp")
                .with_child(wn("PagesPerSheet", n))
                .with_child(wt("Direction", dir)),
        );
    }
    if let Some(v) = &d.orientation {
        dp = dp.with_child(wt("Orientation", v.as_str()));
    }
    if let Some(r) = &d.resolution {
        dp = dp.with_child(
            w("Resolution")
                .with_child(wn("Width", r.width))
                .with_child(wn("Height", r.height())),
        );
    }
    if let Some(v) = &d.quality {
        dp = dp.with_child(wt("PrintQuality", v.as_str()));
    }
    if let Some(v) = &d.sides {
        dp = dp.with_child(wt("Sides", v.as_str()));
    }
    w("DefaultPrintTicket")
        .with_child(
            w("JobDescription")
                .with_child(wt("JobName", d.job_name.as_str()))
                .with_child(wt("JobOriginatingUserName", d.user_name.as_str())),
        )
        .with_child(
            w("JobProcessing")
                .with_child(wn("Copies", d.copies))
                .with_child(wn("Priority", d.priority)),
        )
        .with_child(dp)
}

fn allowed_tokens(name: &str, values: &[String]) -> Element {
    w(name).with_children(values.iter().map(|v| wt("AllowedValue", v.as_str())))
}

fn int_range(name: &str, r: &crate::profile::Range) -> Element {
    w(name)
        .with_child(wn("MinValue", r.min))
        .with_child(wn("MaxValue", r.max))
}

/// `PrinterCapabilities`: only what the profile declares, so only what the printer accepts.
pub fn printer_capabilities(p: &PrinterProfile) -> Element {
    let o = &p.print;
    let mut dp =
        w("DocumentProcessing").with_child(allowed_tokens("MediaSizeName", &o.media_sizes));
    dp = dp.with_child(allowed_tokens("MediaType", &o.media_types));
    if !o.media_colors.is_empty() {
        dp = dp.with_child(allowed_tokens("MediaColor", &o.media_colors));
    }
    dp = dp
        .with_child(
            w("NumberUp")
                .with_child(
                    w("PagesPerSheet")
                        .with_children(o.pages_per_sheet.iter().map(|n| wn("AllowedValue", n))),
                )
                .with_child(allowed_tokens("Direction", &o.number_up_directions)),
        )
        .with_child(allowed_tokens("Orientation", &o.orientations))
        .with_child(w("Resolution").with_children(o.resolutions.iter().map(|r| {
            w("AllowedValue")
                .with_child(wn("Width", r.width))
                .with_child(wn("Height", r.height()))
        })))
        .with_child(allowed_tokens("PrintQuality", &o.qualities))
        .with_child(allowed_tokens("Sides", &o.sides));
    w("PrinterCapabilities")
        .with_child(
            w("JobValues")
                .with_child(
                    w("JobProcessing")
                        .with_child(int_range("Copies", &o.copies))
                        .with_child(int_range("Priority", &o.priority)),
                )
                .with_child(dp),
        )
        .with_child(
            w("DocumentValues").with_child(
                w("DocumentDescription")
                    .with_child(allowed_tokens("Compression", &p.documents.compressions))
                    .with_child(
                        w("Format").with_children(
                            p.documents
                                .formats
                                .iter()
                                .map(|f| wt("AllowedValue", f.as_str())),
                        ),
                    ),
            ),
        )
}

// ---------------------------------------------------------------------------------------------
// Printer status and conditions
// ---------------------------------------------------------------------------------------------

/// Group word of a condition component string; also maps the older lab words.
pub fn component_group(raw: &str) -> &'static str {
    let g = raw.split('/').next().unwrap_or(raw);
    match g {
        "PrintService" | "Printer" => "PrintService",
        "InputBin" | "InputTray" => "InputBin",
        "MediaPath" => "MediaPath",
        "OutputBin" | "OutputTray" => "OutputBin",
        "Consumable" => "Consumable",
        "CoverDoor" | "Cover" => "CoverDoor",
        _ => "PrintService",
    }
}

fn default_component_name(group: &str) -> &'static str {
    match group {
        "InputBin" => "Tray1",
        "OutputBin" => "Bin1",
        "Consumable" => "Toner",
        "CoverDoor" => "Cover1",
        "MediaPath" => "MediaPath",
        _ => "PrintService",
    }
}

/// `(group, name)` of a `DeviceCondition.component` string (`Group` or `Group/Name`).
pub fn split_component(raw: &str) -> (String, String) {
    let group = component_group(raw);
    let name = match raw.split_once('/') {
        Some((_, n)) if !n.is_empty() => n
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || matches!(c, '.' | '-' | '_' | ':') {
                    c
                } else {
                    '-'
                }
            })
            .collect(),
        _ => default_component_name(group).to_string(),
    };
    (group.to_string(), name)
}

fn condition_children(c: &DeviceCondition) -> Vec<Element> {
    let (group, name) = split_component(&c.component);
    vec![
        wt("Time", c.time.as_str()),
        wt("Name", c.name.as_str()),
        w("Component")
            .with_child(wt("Group", group))
            .with_child(wt("Name", name)),
        wt("Severity", c.severity.token()),
    ]
}

/// `DeviceCondition` (an active condition, or the body of a condition event).
pub fn device_condition(c: &DeviceCondition) -> Element {
    w("DeviceCondition")
        .with_attr("Id", c.id.to_string())
        .with_children(condition_children(c))
}

/// `ConditionHistoryEntry` of a cleared condition (`None` while it is still active).
pub fn condition_history_entry(r: &ConditionRecord) -> Option<Element> {
    let cleared = r.cleared.as_ref()?;
    Some(
        w("ConditionHistoryEntry")
            .with_attr("Id", r.condition.id.to_string())
            .with_children(condition_children(&r.condition))
            .with_child(wt("ClearTime", cleared.as_str())),
    )
}

/// `DeviceConditionCleared` (the body of a condition-cleared event).
pub fn device_condition_cleared(id: u32, cleared: &str) -> Element {
    w("DeviceConditionCleared")
        .with_child(wn("ConditionId", id))
        .with_child(wt("ConditionClearTime", cleared))
}

fn reasons_element(reasons: &[String]) -> Element {
    let mut r = w("PrinterStateReasons");
    if reasons.is_empty() {
        r = r.with_child(wt("PrinterStateReason", "None"));
    } else {
        r = r.with_children(reasons.iter().map(|x| wt("PrinterStateReason", x.as_str())));
    }
    r
}

/// `PrinterStatus` from a live snapshot. `volatile` marks it `wprt20:Volatile` (do not
/// cache); `local_time` adds the non-schema `PrinterLocalTime` quirk.
pub fn printer_status(s: &PrinterStatusSnapshot, volatile: bool, local_time: bool) -> Element {
    let mut st = w("PrinterStatus")
        .with_child(wt("PrinterCurrentTime", s.current_time.as_str()))
        .with_child(wt("PrinterState", s.state.token()));
    if let Some(r) = s.primary_reason.as_ref().filter(|r| r.as_str() != "None") {
        st = st.with_child(wt("PrinterPrimaryStateReason", r.as_str()));
    }
    st = st
        .with_child(reasons_element(&s.reasons))
        .with_child(wn("QueuedJobCount", s.queued_jobs));
    if !s.active_conditions.is_empty() {
        st = st.with_child(
            w("ActiveCondition").with_children(s.active_conditions.iter().map(device_condition)),
        );
    }
    let history: Vec<Element> = s
        .condition_history
        .iter()
        .filter_map(condition_history_entry)
        .collect();
    if !history.is_empty() {
        st = st.with_child(w("ConditionHistory").with_children(history));
    }
    // the lab quirk goes last so every schema element keeps its place
    if local_time {
        st = st.with_child(wt("PrinterLocalTime", s.current_time.as_str()));
    }
    if volatile {
        st = st.with_attr_ns(QName::new(ns::PRINT20, "Volatile"), "true");
    }
    st
}

/// `StatusSummary` (the body of a status summary event).
pub fn status_summary(state: PrinterState, primary: Option<&str>, reasons: &[String]) -> Element {
    let mut s = w("StatusSummary").with_child(wt("PrinterState", state.token()));
    if let Some(p) = primary.filter(|p| *p != "None") {
        s = s.with_child(wt("PrinterPrimaryStateReason", p));
    }
    s.with_child(reasons_element(reasons))
}

// ---------------------------------------------------------------------------------------------
// v1.1 / v1.2 / v2.0 sections
// ---------------------------------------------------------------------------------------------

/// `wprt11:DriverConfiguration`, when the profile configures one.
pub fn driver_configuration(p: &PrinterProfile) -> Option<Element> {
    let d = p.extensions.driver_config.as_ref()?;
    let mut e = Element::new(ns::PRINT11, "DriverConfiguration");
    if let Some(f) = &d.gpd_config_file {
        e = e.with_child(Element::text_el(ns::PRINT11, "GPDConfigFile", f.as_str()));
    }
    if let Some(f) = &d.ppd_config_file {
        e = e.with_child(Element::text_el(ns::PRINT11, "PPDConfigFile", f.as_str()));
    }
    Some(e)
}

/// `wprt20:PrintDeviceCapabilitiesChangeID`.
pub fn print_device_capabilities_change_id(p: &PrinterProfile) -> Element {
    let id = p
        .extensions
        .v20
        .as_ref()
        .map_or_else(|| "1".to_string(), |v| v.capabilities_change_id.clone());
    Element::text_el(ns::PRINT20, "PrintDeviceCapabilitiesChangeID", id)
        .with_attr_ns(QName::new(ns::PRINT20, "Volatile"), "true")
}

/// `wprt20:PrinterFormats`.
pub fn printer_formats(p: &PrinterProfile) -> Element {
    let v = p.extensions.v20.clone().unwrap_or_default();
    let list = |name: &str, vals: &[String]| {
        Element::new(ns::PRINT20, name)
            .with_children(vals.iter().map(|x| wt("AllowedValue", x.as_str())))
    };
    Element::new(ns::PRINT20, "PrinterFormats")
        .with_child(list(
            "PrintDeviceCapabilitiesFormats",
            &v.capabilities_formats,
        ))
        .with_child(list("PrintJobTicketFormats", &v.ticket_formats))
        .with_child(list("PrintDeviceResourceFormats", &v.resource_formats))
}

// ---------------------------------------------------------------------------------------------
// Jobs
// ---------------------------------------------------------------------------------------------

fn job_reasons(job: &PrintJob) -> Element {
    let mut r = w("JobStateReasons");
    if job.reasons.is_empty() {
        r = r.with_child(wt("JobStateReason", "None"));
    } else {
        r = r.with_children(job.reasons.iter().map(|x| wt("JobStateReason", x.token())));
    }
    r
}

fn job_counters(job: &PrintJob) -> Vec<Element> {
    vec![
        wn("KOctetsProcessed", job.k_octets_processed()),
        wn("MediaSheetsCompleted", job.media_sheets_completed),
        wn("NumberOfDocuments", job.number_of_documents()),
    ]
}

/// `JobStatus` (also the body of a job status event).
pub fn job_status(job: &PrintJob) -> Element {
    w("JobStatus")
        .with_child(wn("JobId", job.id))
        .with_child(wt("JobState", job.state.token()))
        .with_child(job_reasons(job))
        .with_children(job_counters(job))
}

/// `JobSummary` (one row of the active and history lists).
pub fn job_summary(job: &PrintJob) -> Element {
    w("JobSummary")
        .with_child(wn("JobId", job.id))
        .with_child(wt("JobState", job.state.token()))
        .with_child(job_reasons(job))
        .with_child(wt("JobName", job.name.as_str()))
        .with_child(wt("JobOriginatingUserName", job.user.as_str()))
        .with_children(job_counters(job))
}

/// `JobEndState` (the body of a job end state event).
pub fn job_end_state(job: &PrintJob) -> Element {
    let mut reasons = w("JobCompletedStateReasons");
    if job.reasons.is_empty() {
        reasons = reasons.with_child(wt("JobStateReason", "None"));
    } else {
        reasons =
            reasons.with_children(job.reasons.iter().map(|x| wt("JobStateReason", x.token())));
    }
    w("JobEndState")
        .with_child(wn("JobId", job.id))
        .with_child(wt("JobCompletedState", job.state.token()))
        .with_child(reasons)
        .with_child(wt("JobName", job.name.as_str()))
        .with_child(wt("JobOriginatingUserName", job.user.as_str()))
        .with_children(job_counters(job))
}

/// `PrintTicket` of a job: the stored ticket renamed into place, else one built from the
/// job's name and user.
pub fn job_print_ticket(job: &PrintJob) -> Element {
    match &job.ticket {
        Some(t) => {
            let mut t = t.clone();
            t.name = wprt("PrintTicket");
            t
        }
        None => w("PrintTicket").with_child(
            w("JobDescription")
                .with_child(wt("JobName", job.name.as_str()))
                .with_child(wt("JobOriginatingUserName", job.user.as_str())),
        ),
    }
}

/// `Documents` of a job; `None` when it has none yet.
pub fn job_documents(job: &PrintJob) -> Option<Element> {
    if job.documents.is_empty() {
        return None;
    }
    Some(w("Documents").with_children(job.documents.iter().map(|d| {
        let mut desc = w("DocumentDescription")
            .with_child(wn("DocumentId", d.id))
            .with_child(wt("Compression", d.compression.as_str()))
            .with_child(wt("Format", d.format.as_str()));
        if let Some(n) = &d.name {
            desc = desc.with_child(wt("DocumentName", n.as_str()));
        }
        if let Some(u) = &d.url {
            desc = desc.with_child(wt("DocumentUrl", u.as_str()));
        }
        w("Document").with_child(desc)
    })))
}

// ---------------------------------------------------------------------------------------------
// Operation bodies
// ---------------------------------------------------------------------------------------------

fn element_data(name: &QName, section: Option<Element>) -> Element {
    let e = w("ElementData")
        .with_attr_qname("Name", name.clone())
        .with_attr("Valid", if section.is_some() { "true" } else { "false" });
    match section {
        Some(s) => e.with_child(s),
        None => e,
    }
}

/// The `Name` values of a `GetPrinterElements` / `GetJobElements` request body, as QNames.
/// Fails when the list is empty or a name cannot be resolved to a QName.
pub fn requested_qnames(request: &Element) -> Result<Vec<QName>, String> {
    let list = request
        .child(ns::PRINT, "RequestedElements")
        .ok_or("RequestedElements is missing")?;
    let mut out = Vec::new();
    for n in list.children.iter().filter(|c| c.is(ns::PRINT, "Name")) {
        match n.text_qname() {
            Some(q) => out.push(q),
            None => return Err(format!("{:?} is not a qualified name", n.text_trimmed())),
        }
    }
    if out.is_empty() {
        return Err("RequestedElements names nothing".into());
    }
    Ok(out)
}

/// `GetPrinterElementsResponse`: one `ElementData` per requested name, in request order;
/// names the model does not know come back `Valid="false"` and empty.
pub fn printer_elements_response(model: &dyn PrinterModel, requested: &[QName]) -> Element {
    w("GetPrinterElementsResponse").with_child(
        w("PrinterElements")
            .with_children(requested.iter().map(|q| element_data(q, model.element(q)))),
    )
}

/// `GetJobElementsResponse` for a job that exists: `JobStatus`, `PrintTicket`, `Documents`.
pub fn job_elements_response(job: &PrintJob, requested: &[QName]) -> Element {
    w("GetJobElementsResponse").with_child(w("JobElements").with_children(requested.iter().map(
        |q| {
            let section = if q.ns == ns::PRINT {
                match q.local.as_str() {
                    "JobStatus" => Some(job_status(job)),
                    "PrintTicket" => Some(job_print_ticket(job)),
                    "Documents" => job_documents(job),
                    _ => None,
                }
            } else {
                None
            };
            element_data(q, section)
        },
    )))
}

/// `CreatePrintJobResponse`; `token` adds the vendor `JobToken` (quirk, off by default).
pub fn create_print_job_response(job_id: u32, token: Option<&str>) -> Element {
    let mut r = w("CreatePrintJobResponse").with_child(wn("JobId", job_id));
    if let Some(t) = token {
        r = r.with_child(wt("JobToken", t));
    }
    r
}

/// `GetActiveJobsResponse`.
pub fn active_jobs_response(jobs: &[PrintJob]) -> Element {
    w("GetActiveJobsResponse")
        .with_child(w("ActiveJobs").with_children(jobs.iter().map(job_summary)))
}

/// `GetJobHistoryResponse`.
pub fn job_history_response(jobs: &[PrintJob]) -> Element {
    w("GetJobHistoryResponse")
        .with_child(w("JobHistory").with_children(jobs.iter().map(job_summary)))
}

/// An empty response element: `SendDocumentResponse`, `AddDocumentResponse`,
/// `CancelJobResponse`, `SetEventRateResponse`.
pub fn empty_response(name: &str) -> Element {
    w(name)
}

/// `SetPrinterElementsResponse` (v1.2): `(path, result)` rows, results from
/// `Succeeded`, `ElementReadOnly`, `IncompatibleType`, `InvalidElementPath`,
/// `UnknownElementPath`, `UnknownFailure`.
pub fn set_printer_elements_response(rows: &[(String, String)]) -> Element {
    let e = |n: &str| Element::new(ns::PRINT12, n);
    e("SetPrinterElementsResponse").with_child(e("ElementsToSetResults").with_children(
        rows.iter().map(|(path, res)| {
            e("ElementToSetResult")
                .with_child(Element::text_el(ns::PRINT12, "ElementPath", path.as_str()))
                .with_child(Element::text_el(ns::PRINT12, "Result", res.as_str()))
        }),
    ))
}

// ---------------------------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------------------------

fn change_rank(name: &str) -> usize {
    [
        "PrinterDescription",
        "PrinterConfiguration",
        "DefaultPrintTicket",
        "PrinterCapabilities",
    ]
    .iter()
    .position(|n| *n == name)
    .unwrap_or(usize::MAX)
}

/// The body element of an event, in schema order.
pub fn event_body(ev: &PrintEvent) -> Element {
    match ev {
        PrintEvent::PrinterElementsChange { changes } => {
            let mut known: Vec<Element> = changes
                .iter()
                .filter(|c| c.name.ns == ns::PRINT && change_rank(&c.name.local) != usize::MAX)
                .cloned()
                .collect();
            known.sort_by_key(|c| change_rank(&c.name.local));
            let foreign = changes
                .iter()
                .filter(|c| c.name.ns != ns::PRINT)
                .take(1)
                .cloned();
            w("PrinterElementsChangeEvent").with_child(
                w("ElementChanges")
                    .with_children(known)
                    .with_children(foreign),
            )
        }
        PrintEvent::PrinterStatusSummary {
            state,
            primary_reason,
            reasons,
        } => w("PrinterStatusSummaryEvent").with_child(status_summary(
            *state,
            primary_reason.as_deref(),
            reasons,
        )),
        PrintEvent::PrinterStatusCondition { condition } => {
            w("PrinterStatusConditionEvent").with_child(device_condition(condition))
        }
        PrintEvent::PrinterStatusConditionCleared {
            condition_id,
            cleared,
        } => w("PrinterStatusConditionClearedEvent")
            .with_child(device_condition_cleared(*condition_id, cleared)),
        PrintEvent::JobStatus { job } => w("JobStatusEvent").with_child(job_status(job)),
        PrintEvent::JobEndState { job } => w("JobEndStateEvent").with_child(job_end_state(job)),
    }
}

// ---------------------------------------------------------------------------------------------
// Reply self-check
// ---------------------------------------------------------------------------------------------

/// Validate a reply against the tables and describe every problem. For the debug/test switch
/// that makes a non-conforming reply fail loudly.
pub fn check_reply(
    target: schema::Target<'_>,
    reply: &Element,
    opts: &schema::Options,
) -> Result<(), String> {
    let v = schema::validate_body_with(target, reply, opts);
    if v.is_empty() {
        Ok(())
    } else {
        Err(v
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("; "))
    }
}

/// `xs:dateTime` text of a time (re-export of the context's formatter).
pub fn datetime(t: SystemTime) -> String {
    format_datetime(t)
}

/// Keep `Severity` and `WireJobState` in the public vocabulary of this module.
pub fn severity_token(s: Severity) -> &'static str {
    s.token()
}

/// Token of a job state.
pub fn job_state_token(s: WireJobState) -> &'static str {
    s.token()
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::schema::{validate_body, Options, Target};
    use super::*;
    use crate::wsd::types::{JobStateReason, PrintOp};
    use crate::wsd::xml::{parse_str, XmlLimits};

    fn ok(target: Target<'_>, el: &Element) {
        let v = validate_body(target, el);
        assert!(v.is_empty(), "{v:#?}\n{}", to_xml(el));
        // and again after a text round trip, as the wire would carry it
        let back = parse_str(&el.to_document(&PrefixMap::wsd()), &XmlLimits::SMALL).unwrap();
        let v = validate_body(target, &back);
        assert!(v.is_empty(), "after round trip: {v:#?}");
    }

    fn rich() -> PrinterProfile {
        let raw = r#"{"schema_version":2,"id":"r","label":"Rich",
          "versions":{"v11":true,"v12":true,"v20":true},
          "print":{"sides":["OneSided","TwoSidedLongEdge"],"media_colors":["white"],"media_sizes":["iso_a4_210x297mm","custom_label_4x6in"]},
          "default_ticket":{"pages_per_sheet":1},
          "storage":[{"name":"Mem","kind":"RAM","size_mb":256}],
          "inputs":[{"name":"Tray1","media_size":"iso_a4_210x297mm"},{"name":"Bypass","media_size":"iso_a4_210x297mm","media_type":null,"media_color":null}],
          "outputs":[{"name":"Face-down"}],
          "consumables":[{"name":"K","level":50,"color":"Black","model":"TN-1","rgb":"000000"}],
          "extensions":{"model_id":"11111111-2222-4333-8444-555555555555","driver_config":{"gpd_config_file":"a.gpd"},"v20":{}}}"#;
        PrinterProfile::from_json(raw).unwrap()
    }

    fn snap() -> PrinterStatusSnapshot {
        PrinterStatusSnapshot {
            current_time: "2026-10-13T12:00:00Z".into(),
            state: PrinterState::Stopped,
            primary_reason: Some("MediaEmpty".into()),
            reasons: vec!["MediaEmpty".into(), "AttentionRequired".into()],
            queued_jobs: 2,
            active_conditions: vec![DeviceCondition {
                id: 3,
                time: "2026-10-13T11:59:00Z".into(),
                name: "InputTraySupplyEmpty".into(),
                component: "InputTray".into(),
                severity: Severity::Critical,
            }],
            condition_history: vec![ConditionRecord {
                condition: DeviceCondition {
                    id: 2,
                    time: "2026-10-13T11:00:00Z".into(),
                    name: "Jam".into(),
                    component: "MediaPath/Path1".into(),
                    severity: Severity::Critical,
                },
                cleared: Some("2026-10-13T11:05:00Z".into()),
            }],
            accepting_jobs: false,
        }
    }

    fn job() -> PrintJob {
        let mut j = PrintJob::new(5, "Report", "justin");
        j.state = WireJobState::Processing;
        j.reasons = vec![JobStateReason::JobPrinting, JobStateReason::JobIncoming];
        j.octets_processed = 5000;
        j.documents.push(crate::wsd::types::JobDocument {
            id: 1,
            name: Some("a.pdf".into()),
            format: "application/pdf".into(),
            compression: "None".into(),
            size: 5000,
            url: Some("http://x/a.pdf".into()),
            capture_path: None,
        });
        j
    }

    #[test]
    fn every_section_validates_for_plain_and_rich_profiles() {
        for p in [PrinterProfile::new("g", "Generic"), rich()] {
            ok(
                Target::Section("PrinterDescription"),
                &printer_description(&p),
            );
            ok(
                Target::Section("PrinterConfiguration"),
                &printer_configuration(&p),
            );
            ok(
                Target::Section("DefaultPrintTicket"),
                &default_print_ticket(&p),
            );
            ok(
                Target::Section("PrinterCapabilities"),
                &printer_capabilities(&p),
            );
            ok(
                Target::Section("PrinterStatus"),
                &printer_status(&snap(), p.versions.v20, false),
            );
        }
    }

    #[test]
    fn status_quirk_and_vendor_attribute() {
        let el = printer_status(&snap(), false, true);
        assert!(!validate_body(Target::Section("PrinterStatus"), &el).is_empty());
        let mut o = Options::STRICT;
        o.quirk_printer_local_time = true;
        assert!(schema::validate_body_with(Target::Section("PrinterStatus"), &el, &o).is_empty());
    }

    #[test]
    fn children_follow_schema_order() {
        let names = |e: &Element| {
            e.children
                .iter()
                .map(|c| c.name.local.clone())
                .collect::<Vec<_>>()
        };
        let p = rich();
        assert_eq!(
            names(&printer_description(&p)),
            [
                "ColorSupported",
                "DeviceId",
                "MultipleDocumentJobsSupported",
                "PagesPerMinute",
                "PagesPerMinuteColor",
                "PrinterName",
                "PrinterInfo",
                "PrinterLocation",
                "DeviceModelId"
            ]
        );
        assert_eq!(
            names(&printer_configuration(&p)),
            [
                "PrinterEventRate",
                "Storage",
                "Consumables",
                "InputBins",
                "Finishings",
                "OutputBins",
                "SupportsWSPrintv11",
                "SupportsWSPrintV12",
                "SupportsWSPrintV20",
                "PrintDeviceCapabilitiesChangeID"
            ]
        );
        let status = printer_status(&snap(), false, false);
        assert_eq!(
            names(&status),
            [
                "PrinterCurrentTime",
                "PrinterState",
                "PrinterPrimaryStateReason",
                "PrinterStateReasons",
                "QueuedJobCount",
                "ActiveCondition",
                "ConditionHistory"
            ]
        );
        let bin = &printer_configuration(&p)
            .child(ns::PRINT, "InputBins")
            .unwrap()
            .children[0]
            .clone();
        assert_eq!(
            names(bin),
            [
                "FeedDirection",
                "MediaSize",
                "MediaType",
                "MediaColor",
                "Capacity",
                "Level"
            ]
        );
        let tk = default_print_ticket(&p);
        assert_eq!(
            names(&tk),
            ["JobDescription", "JobProcessing", "DocumentProcessing"]
        );
        assert_eq!(
            names(tk.child(ns::PRINT, "DocumentProcessing").unwrap()),
            [
                "MediaSizeName",
                "MediaType",
                "NumberUp",
                "Orientation",
                "Resolution",
                "PrintQuality",
                "Sides"
            ]
        );
    }

    #[test]
    fn capabilities_are_honest_about_the_profile() {
        let mut p = PrinterProfile::new("g", "G");
        p.documents.formats = vec!["application/pdf".into()];
        p.print.sides = vec!["OneSided".into()];
        let xml = to_xml(&printer_capabilities(&p));
        assert!(
            xml.contains("application/pdf")
                && !xml.contains("TwoSided")
                && !xml.contains("pwg-raster")
        );
        let duplex = rich();
        assert!(to_xml(&printer_capabilities(&duplex)).contains("TwoSidedLongEdge"));
    }

    #[test]
    fn job_bodies_validate() {
        let j = job();
        ok(Target::Section("JobStatus"), &job_status(&j));
        ok(Target::Section("JobSummary"), &job_summary(&j));
        ok(Target::Section("JobEndState"), &job_end_state(&j));
        ok(Target::Section("PrintTicket"), &job_print_ticket(&j));
        ok(Target::Section("Documents"), &job_documents(&j).unwrap());
        let names = [
            wprt("JobStatus"),
            wprt("PrintTicket"),
            wprt("Documents"),
            wprt("Bogus"),
        ];
        let resp = job_elements_response(&j, &names);
        ok(Target::Response(PrintOp::GetJobElements), &resp);
        let valid: Vec<_> = resp
            .child(ns::PRINT, "JobElements")
            .unwrap()
            .children
            .iter()
            .map(|e| e.attr_local("Valid").map(str::to_string))
            .collect();
        assert_eq!(
            valid,
            [
                Some("true".into()),
                Some("true".into()),
                Some("true".into()),
                Some("false".into())
            ]
        );
        ok(
            Target::Response(PrintOp::GetActiveJobs),
            &active_jobs_response(&[j.clone(), job()]),
        );
        ok(
            Target::Response(PrintOp::GetActiveJobs),
            &active_jobs_response(&[]),
        );
        ok(
            Target::Response(PrintOp::GetJobHistory),
            &job_history_response(&[j]),
        );
    }

    #[test]
    fn simple_responses_and_events_validate() {
        ok(
            Target::Response(PrintOp::CreatePrintJob),
            &create_print_job_response(9, None),
        );
        assert!(!validate_body(
            Target::Response(PrintOp::CreatePrintJob),
            &create_print_job_response(9, Some("t"))
        )
        .is_empty());
        for (op, name) in [
            (PrintOp::SendDocument, "SendDocumentResponse"),
            (PrintOp::AddDocument, "AddDocumentResponse"),
            (PrintOp::CancelJob, "CancelJobResponse"),
            (PrintOp::SetEventRate, "SetEventRateResponse"),
        ] {
            ok(Target::Response(op), &empty_response(name));
        }
        ok(
            Target::Response(PrintOp::SetPrinterElements),
            &set_printer_elements_response(&[("a/b".into(), "Succeeded".into())]),
        );
        let s = snap();
        let events = [
            PrintEvent::PrinterStatusSummary {
                state: s.state,
                primary_reason: s.primary_reason.clone(),
                reasons: s.reasons.clone(),
            },
            PrintEvent::PrinterStatusCondition {
                condition: s.active_conditions[0].clone(),
            },
            PrintEvent::PrinterStatusConditionCleared {
                condition_id: 3,
                cleared: "2026-10-13T12:01:00Z".into(),
            },
            PrintEvent::JobStatus { job: job() },
            PrintEvent::JobEndState { job: job() },
            PrintEvent::PrinterElementsChange {
                changes: vec![
                    printer_capabilities(&rich()),
                    printer_configuration(&rich()),
                ],
            },
        ];
        for ev in &events {
            let body = event_body(ev);
            ok(Target::Event(ev.name()), &body);
            assert_eq!(body.name.local, ev.name());
        }
    }

    #[test]
    fn v2_sections_and_extension_elements_validate() {
        let p = rich();
        for e in [
            printer_formats(&p),
            print_device_capabilities_change_id(&p),
            driver_configuration(&p).unwrap(),
        ] {
            let v = schema::validate_root(&e, &Options::STRICT);
            assert!(v.is_empty(), "{v:?}");
        }
    }

    #[test]
    fn get_printer_elements_response_follows_the_request() {
        let model = crate::wsd::printer_model::ProfilePrinterModel::new(rich());
        let req = parse_str(
            r#"<p:GetPrinterElementsRequest xmlns:p="http://schemas.microsoft.com/windows/2006/08/wdp/print">
                 <p:RequestedElements><p:Name>p:PrinterStatus</p:Name><p:Name>p:Nope</p:Name><p:Name>p:PrinterCapabilities</p:Name></p:RequestedElements>
               </p:GetPrinterElementsRequest>"#,
            &XmlLimits::SMALL,
        )
        .unwrap();
        ok(Target::Request(PrintOp::GetPrinterElements), &req);
        let names = requested_qnames(&req).unwrap();
        let resp = printer_elements_response(&model, &names);
        ok(Target::Response(PrintOp::GetPrinterElements), &resp);
        let data = &resp.child(ns::PRINT, "PrinterElements").unwrap().children;
        assert_eq!(data.len(), 3);
        assert_eq!(data[0].attr_local("Valid"), Some("true"));
        assert_eq!(data[1].attr_local("Valid"), Some("false"));
        assert!(data[1].children.is_empty());
        assert!(requested_qnames(&w("GetPrinterElementsRequest")).is_err());
    }

    #[test]
    fn component_words_map_to_schema_groups() {
        assert_eq!(
            split_component("InputTray"),
            ("InputBin".into(), "Tray1".into())
        );
        assert_eq!(
            split_component("Cover"),
            ("CoverDoor".into(), "Cover1".into())
        );
        assert_eq!(
            split_component("Printer"),
            ("PrintService".into(), "PrintService".into())
        );
        assert_eq!(
            split_component("Consumable/Black ink"),
            ("Consumable".into(), "Black-ink".into())
        );
        assert_eq!(
            split_component("???"),
            ("PrintService".into(), "PrintService".into())
        );
    }
}
