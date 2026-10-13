//! Print tickets: parsing, validation against the printer's declared capabilities, and the
//! effective ticket.
//!
//! The ticket in `CreatePrintJob` (and the per-document `DocumentProcessing` of `SendDocument`)
//! is read into a typed [`PrintTicket`]; there is no string scraping. [`Capabilities`] is
//! extracted from the printer model's `PrinterCapabilities` and `DefaultPrintTicket`
//! sections, so what the printer advertises and what it accepts cannot drift apart.
//!
//! Semantics (`docs/reference/must-honor.md`):
//!
//! * a value the printer supports is used as asked;
//! * an unsupported value or element with `MustHonor=true` rejects the request
//!   (`InvalidArgs`, naming the element); no job is created;
//! * an unsupported value without `MustHonor` is replaced by the printer's default when that
//!   default is allowed, else dropped (`Substitution` records which);
//! * two `MustHonor=true` values that a configured [`ConflictRule`] declares incompatible
//!   reject the request with `ClientErrorConflictingRequiredParameters`;
//! * print has no "validate" step and no answer to carry substitutions, so the *effective*
//!   ticket (what the printer will actually do, defaults filled in) is stored on the job and
//!   returned by `GetJobElements(PrintTicket)`.
//!
//! Vendor extension elements (`xs:any ##other`) in a ticket are ignored and not echoed.

use std::collections::BTreeMap;

use super::ctx::{ConflictRule, PrintLimits, Quirks};
use super::fault::{FaultKind, PrintFault};
use super::types::{wprt, PrinterModel};
use super::xml::{ns, Element, QName};

/// The 2006/01 namespace of an early draft; request elements in it are read like the final one.
const LEGACY_NS: &str = "http://schemas.microsoft.com/windows/2006/01/wdp/print";

/// Child element by local name, in the print namespace (or the early-draft one).
pub fn child<'a>(el: &'a Element, local: &str) -> Option<&'a Element> {
    el.children
        .iter()
        .find(|c| c.name.local == local && (c.name.ns == ns::PRINT || c.name.ns == LEGACY_NS))
}

/// All children with the local name.
pub fn children<'a>(el: &'a Element, local: &'a str) -> impl Iterator<Item = &'a Element> + 'a {
    el.children.iter().filter(move |c| {
        c.name.local == local && (c.name.ns == ns::PRINT || c.name.ns == LEGACY_NS)
    })
}

/// Whether `el` is the print element `local`.
pub fn is_print(el: &Element, local: &str) -> bool {
    el.name.local == local && (el.name.ns == ns::PRINT || el.name.ns == LEGACY_NS)
}

fn bad(msg: impl Into<String>) -> PrintFault {
    PrintFault::invalid_args(msg)
}

fn must_honor(el: &Element) -> Result<bool, PrintFault> {
    for q in [
        QName::new(ns::PRINT, "MustHonor"),
        QName::new(LEGACY_NS, "MustHonor"),
    ] {
        if let Some(v) = el.attr(&q) {
            return match v.trim() {
                "true" | "1" => Ok(true),
                "false" | "0" => Ok(false),
                other => Err(bad(format!(
                    "{}: MustHonor is not a boolean: {other:?}",
                    el.name.local
                ))),
            };
        }
    }
    Ok(false)
}

fn no_children(el: &Element) -> Result<(), PrintFault> {
    if el.children.is_empty() {
        Ok(())
    } else {
        Err(bad(format!(
            "{} must not have child elements",
            el.name.local
        )))
    }
}

fn token(el: &Element) -> Result<String, PrintFault> {
    no_children(el)?;
    let t = el.text_trimmed();
    if t.is_empty() || t.len() > 255 || t.chars().any(char::is_whitespace) {
        return Err(bad(format!("{} is not a valid token", el.name.local)));
    }
    Ok(t.to_string())
}

fn int(el: &Element) -> Result<i64, PrintFault> {
    no_children(el)?;
    el.text_trimmed()
        .parse::<i64>()
        .map_err(|_| bad(format!("{} is not an integer", el.name.local)))
}

fn boolean(el: &Element) -> Result<bool, PrintFault> {
    no_children(el)?;
    match el.text_trimmed() {
        "true" | "1" => Ok(true),
        "false" | "0" => Ok(false),
        _ => Err(bad(format!("{} is not a boolean", el.name.local))),
    }
}

fn ext_pattern(s: &str) -> bool {
    // \w+:[\w_\-\.]+
    match s.split_once(':') {
        Some((a, b)) => {
            !a.is_empty()
                && a.chars().all(|c| c.is_alphanumeric() || c == '_')
                && !b.is_empty()
                && b.chars()
                    .all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.'))
        }
        None => false,
    }
}

fn closed(name: &str, value: String, wkv: &[&str]) -> Result<String, PrintFault> {
    if wkv.contains(&value.as_str()) || ext_pattern(&value) {
        Ok(value)
    } else {
        Err(bad(format!("{name}: {value:?} is not a defined value")))
    }
}

const ORIENTATION: &[&str] = &[
    "Landscape",
    "Portrait",
    "ReverseLandscape",
    "ReversePortrait",
];
const SIDES: &[&str] = &["OneSided", "TwoSidedLongEdge", "TwoSidedShortEdge"];
const QUALITY: &[&str] = &["Draft", "High", "Normal", "Photo"];
const DIRECTION: &[&str] = &["RightDown", "DownRight", "LeftDown", "DownLeft"];

// ---------------------------------------------------------------------------------------------
// Typed ticket
// ---------------------------------------------------------------------------------------------

/// A value with its `MustHonor` flag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Honored<T> {
    pub value: T,
    pub must_honor: bool,
}

impl<T> Honored<T> {
    pub fn new(value: T, must_honor: bool) -> Self {
        Self { value, must_honor }
    }
    pub fn soft(value: T) -> Self {
        Self {
            value,
            must_honor: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobDescription {
    pub name: String,
    pub user: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Staple {
    pub location: Honored<String>,
    pub angle: Option<Honored<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HolePunch {
    pub must_honor: bool,
    pub edge: String,
    pub pattern: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JobProcessing {
    pub copies: Option<Honored<u32>>,
    pub collate: Option<Honored<bool>>,
    pub jog_offset: Option<Honored<bool>>,
    pub staple: Option<Staple>,
    pub hole_punch: Option<HolePunch>,
    pub priority: Option<Honored<u32>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NumberUp {
    pub pages_per_sheet: Honored<u32>,
    pub direction: Honored<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolution {
    pub must_honor: bool,
    pub width: u32,
    pub height: Option<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DocumentProcessing {
    pub media_size_name: Option<Honored<String>>,
    pub media_type: Option<Honored<String>>,
    pub media_color: Option<Honored<String>>,
    pub number_up: Option<NumberUp>,
    pub orientation: Option<Honored<String>>,
    pub resolution: Option<Resolution>,
    pub print_quality: Option<Honored<String>>,
    pub sides: Option<Honored<String>>,
}

/// `wprt:PrintTicketType`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PrintTicket {
    pub description: Option<JobDescription>,
    pub job: JobProcessing,
    pub document: DocumentProcessing,
}

fn honored_token(el: &Element, wkv: Option<&[&str]>) -> Result<Honored<String>, PrintFault> {
    let mh = must_honor(el)?;
    let t = token(el)?;
    let t = match wkv {
        Some(list) => closed(&el.name.local, t, list)?,
        None => t,
    };
    Ok(Honored::new(t, mh))
}

fn honored_uint(el: &Element, min: i64, max: i64) -> Result<Honored<u32>, PrintFault> {
    let mh = must_honor(el)?;
    let v = int(el)?;
    if v < min || v > max {
        return Err(bad(format!(
            "{} must be between {min} and {max}",
            el.name.local
        )));
    }
    Ok(Honored::new(u32::try_from(v).unwrap_or(u32::MAX), mh))
}

impl DocumentProcessing {
    /// Parse a `DocumentProcessing` element.
    pub fn parse(el: &Element) -> Result<DocumentProcessing, PrintFault> {
        let mut d = DocumentProcessing::default();
        if let Some(e) = child(el, "MediaSizeName") {
            d.media_size_name = Some(honored_token(e, None)?);
        }
        if let Some(e) = child(el, "MediaType") {
            d.media_type = Some(honored_token(e, None)?);
        }
        if let Some(e) = child(el, "MediaColor") {
            d.media_color = Some(honored_token(e, None)?);
        }
        if let Some(e) = child(el, "NumberUp") {
            let pages =
                child(e, "PagesPerSheet").ok_or_else(|| bad("NumberUp without PagesPerSheet"))?;
            let dir = child(e, "Direction").ok_or_else(|| bad("NumberUp without Direction"))?;
            d.number_up = Some(NumberUp {
                pages_per_sheet: honored_uint(pages, 1, i64::from(u32::MAX))?,
                direction: honored_token(dir, Some(DIRECTION))?,
            });
        }
        if let Some(e) = child(el, "Orientation") {
            d.orientation = Some(honored_token(e, Some(ORIENTATION))?);
        }
        if let Some(e) = child(el, "Resolution") {
            let w = child(e, "Width").ok_or_else(|| bad("Resolution without Width"))?;
            let width = u32::try_from(int(w)?).map_err(|_| bad("Resolution Width out of range"))?;
            let height = match child(e, "Height") {
                Some(h) => Some(
                    u32::try_from(int(h)?).map_err(|_| bad("Resolution Height out of range"))?,
                ),
                None => None,
            };
            d.resolution = Some(Resolution {
                must_honor: must_honor(e)?,
                width,
                height,
            });
        }
        if let Some(e) = child(el, "PrintQuality") {
            d.print_quality = Some(honored_token(e, Some(QUALITY))?);
        }
        if let Some(e) = child(el, "Sides") {
            d.sides = Some(honored_token(e, Some(SIDES))?);
        }
        Ok(d)
    }

    fn tokens(&mut self) -> Vec<(&'static str, &mut Option<Honored<String>>)> {
        vec![
            ("MediaSizeName", &mut self.media_size_name),
            ("MediaType", &mut self.media_type),
            ("MediaColor", &mut self.media_color),
            ("Orientation", &mut self.orientation),
            ("PrintQuality", &mut self.print_quality),
            ("Sides", &mut self.sides),
        ]
    }

    /// Serialize in schema order; `None` when nothing is set.
    pub fn to_element(&self) -> Option<Element> {
        let mut el = Element::new(ns::PRINT, "DocumentProcessing");
        let tok = |name: &str, v: &Option<Honored<String>>| -> Option<Element> {
            v.as_ref().map(|h| honored_el(name, &h.value, h.must_honor))
        };
        for (n, v) in [
            ("MediaSizeName", &self.media_size_name),
            ("MediaType", &self.media_type),
            ("MediaColor", &self.media_color),
        ] {
            el.children.extend(tok(n, v));
        }
        if let Some(n) = &self.number_up {
            el.children.push(
                Element::new(ns::PRINT, "NumberUp")
                    .with_child(honored_el(
                        "PagesPerSheet",
                        &n.pages_per_sheet.value.to_string(),
                        n.pages_per_sheet.must_honor,
                    ))
                    .with_child(honored_el(
                        "Direction",
                        &n.direction.value,
                        n.direction.must_honor,
                    )),
            );
        }
        el.children.extend(tok("Orientation", &self.orientation));
        if let Some(r) = &self.resolution {
            let mut e = Element::new(ns::PRINT, "Resolution").with_child(Element::text_el(
                ns::PRINT,
                "Width",
                r.width.to_string(),
            ));
            if let Some(h) = r.height {
                e.children
                    .push(Element::text_el(ns::PRINT, "Height", h.to_string()));
            }
            el.children.push(mh_attr(e, r.must_honor));
        }
        el.children.extend(tok("PrintQuality", &self.print_quality));
        el.children.extend(tok("Sides", &self.sides));
        if el.children.is_empty() {
            None
        } else {
            Some(el)
        }
    }
}

fn mh_attr(el: Element, mh: bool) -> Element {
    if mh {
        el.with_attr_ns(QName::new(ns::PRINT, "MustHonor"), "true")
    } else {
        el
    }
}

fn honored_el(name: &str, value: &str, mh: bool) -> Element {
    mh_attr(Element::text_el(ns::PRINT, name, value), mh)
}

impl PrintTicket {
    /// Parse a `PrintTicket` element. `Err` is the `InvalidArgs` fault to send.
    pub fn parse(
        el: &Element,
        limits: &PrintLimits,
        quirks: &Quirks,
    ) -> Result<PrintTicket, PrintFault> {
        let mut t = PrintTicket::default();
        match child(el, "JobDescription") {
            Some(jd) => {
                let name = match child(jd, "JobName") {
                    Some(n) => n.text_trimmed().to_string(),
                    None if quirks.lenient_job_description => "job".to_string(),
                    None => return Err(bad("JobDescription without JobName")),
                };
                let user = match child(jd, "JobOriginatingUserName") {
                    Some(n) => n.text_trimmed().to_string(),
                    None if quirks.lenient_job_description => String::new(),
                    None => return Err(bad("JobDescription without JobOriginatingUserName")),
                };
                if name.len() > limits.max_name || user.len() > limits.max_name {
                    return Err(bad(
                        "JobName or JobOriginatingUserName is longer than 255 characters",
                    ));
                }
                t.description = Some(JobDescription { name, user });
            }
            None if quirks.lenient_job_description => {}
            None => return Err(bad("PrintTicket without JobDescription")),
        }
        if let Some(jp) = child(el, "JobProcessing") {
            if let Some(e) = child(jp, "Copies") {
                t.job.copies = Some(honored_uint(e, 1, i64::from(i32::MAX))?);
            }
            if let Some(f) = child(jp, "JobFinishings") {
                if let Some(e) = child(f, "Collate") {
                    t.job.collate = Some(Honored::new(boolean(e)?, must_honor(e)?));
                }
                if let Some(e) = child(f, "JogOffset") {
                    t.job.jog_offset = Some(Honored::new(boolean(e)?, must_honor(e)?));
                }
                if let Some(s) = child(f, "Staple") {
                    let loc = child(s, "Location").ok_or_else(|| bad("Staple without Location"))?;
                    let angle = match child(s, "Angle") {
                        Some(a) => Some(honored_token(a, None)?),
                        None => None,
                    };
                    t.job.staple = Some(Staple {
                        location: honored_token(loc, None)?,
                        angle,
                    });
                }
                if let Some(h) = child(f, "HolePunch") {
                    let edge = child(h, "Edge").ok_or_else(|| bad("HolePunch without Edge"))?;
                    let pat =
                        child(h, "Pattern").ok_or_else(|| bad("HolePunch without Pattern"))?;
                    t.job.hole_punch = Some(HolePunch {
                        must_honor: must_honor(h)?,
                        edge: token(edge)?,
                        pattern: token(pat)?,
                    });
                }
            }
            if let Some(e) = child(jp, "Priority") {
                t.job.priority = Some(honored_uint(e, 1, 100)?);
            }
        }
        if let Some(dp) = child(el, "DocumentProcessing") {
            t.document = DocumentProcessing::parse(dp)?;
        }
        Ok(t)
    }

    /// The typed fields as a JSON object (for the control plane). Every key is present;
    /// a field the ticket does not carry is `null`. Honored values are
    /// `{"value": .., "must_honor": ..}`; `must_honor` lists the keys whose flag was set.
    pub fn to_json(&self) -> serde_json::Value {
        use serde_json::{json, Value};
        fn hv<T: serde::Serialize>(h: &Option<Honored<T>>) -> Value {
            h.as_ref().map_or(
                Value::Null,
                |h| json!({"value": &h.value, "must_honor": h.must_honor}),
            )
        }
        let j = &self.job;
        let d = &self.document;
        let mut flagged: Vec<&str> = Vec::new();
        let mut note = |key: &'static str, on: bool| {
            if on {
                flagged.push(key);
            }
        };
        note("copies", j.copies.as_ref().is_some_and(|h| h.must_honor));
        note("collate", j.collate.as_ref().is_some_and(|h| h.must_honor));
        note(
            "jog_offset",
            j.jog_offset.as_ref().is_some_and(|h| h.must_honor),
        );
        note(
            "priority",
            j.priority.as_ref().is_some_and(|h| h.must_honor),
        );
        note(
            "staple",
            j.staple.as_ref().is_some_and(|s| {
                s.location.must_honor || s.angle.as_ref().is_some_and(|a| a.must_honor)
            }),
        );
        note(
            "hole_punch",
            j.hole_punch.as_ref().is_some_and(|h| h.must_honor),
        );
        note(
            "media_size",
            d.media_size_name.as_ref().is_some_and(|h| h.must_honor),
        );
        note(
            "media_type",
            d.media_type.as_ref().is_some_and(|h| h.must_honor),
        );
        note(
            "media_color",
            d.media_color.as_ref().is_some_and(|h| h.must_honor),
        );
        note(
            "number_up",
            d.number_up
                .as_ref()
                .is_some_and(|n| n.pages_per_sheet.must_honor || n.direction.must_honor),
        );
        note(
            "orientation",
            d.orientation.as_ref().is_some_and(|h| h.must_honor),
        );
        note(
            "resolution",
            d.resolution.as_ref().is_some_and(|r| r.must_honor),
        );
        note(
            "quality",
            d.print_quality.as_ref().is_some_and(|h| h.must_honor),
        );
        note("sides", d.sides.as_ref().is_some_and(|h| h.must_honor));
        json!({
            "job_name": self.description.as_ref().map(|x| x.name.clone()),
            "user": self.description.as_ref().map(|x| x.user.clone()),
            "copies": hv(&j.copies),
            "collate": hv(&j.collate),
            "jog_offset": hv(&j.jog_offset),
            "priority": hv(&j.priority),
            "staple": j.staple.as_ref().map(|s| json!({
                "location": {"value": s.location.value, "must_honor": s.location.must_honor},
                "angle": hv(&s.angle),
            })),
            "hole_punch": j.hole_punch.as_ref().map(|h| json!({
                "edge": h.edge, "pattern": h.pattern, "must_honor": h.must_honor,
            })),
            "media_size": hv(&d.media_size_name),
            "media_type": hv(&d.media_type),
            "media_color": hv(&d.media_color),
            "number_up": d.number_up.as_ref().map(|n| json!({
                "pages_per_sheet": n.pages_per_sheet.value,
                "direction": n.direction.value,
                "must_honor": n.pages_per_sheet.must_honor || n.direction.must_honor,
            })),
            "orientation": hv(&d.orientation),
            "resolution": d.resolution.as_ref().map(|r| json!({
                "width": r.width, "height": r.height, "must_honor": r.must_honor,
            })),
            "quality": hv(&d.print_quality),
            "sides": hv(&d.sides),
            "must_honor": flagged,
        })
    }

    /// Effective copies (1 when unspecified).
    pub fn copies(&self) -> u32 {
        self.job.copies.as_ref().map_or(1, |c| c.value)
    }

    /// Serialize as a `PrintTicket` element in schema order.
    pub fn to_element(&self) -> Element {
        let mut el = Element::new(ns::PRINT, "PrintTicket");
        if let Some(d) = &self.description {
            el.children.push(
                Element::new(ns::PRINT, "JobDescription")
                    .with_child(Element::text_el(ns::PRINT, "JobName", d.name.as_str()))
                    .with_child(Element::text_el(
                        ns::PRINT,
                        "JobOriginatingUserName",
                        d.user.as_str(),
                    )),
            );
        }
        let j = &self.job;
        let mut jp = Element::new(ns::PRINT, "JobProcessing");
        if let Some(c) = &j.copies {
            jp.children
                .push(honored_el("Copies", &c.value.to_string(), c.must_honor));
        }
        let mut fin = Element::new(ns::PRINT, "JobFinishings");
        if let Some(c) = &j.collate {
            fin.children
                .push(honored_el("Collate", &c.value.to_string(), c.must_honor));
        }
        if let Some(c) = &j.jog_offset {
            fin.children
                .push(honored_el("JogOffset", &c.value.to_string(), c.must_honor));
        }
        if let Some(s) = &j.staple {
            let mut e = Element::new(ns::PRINT, "Staple").with_child(honored_el(
                "Location",
                &s.location.value,
                s.location.must_honor,
            ));
            if let Some(a) = &s.angle {
                e.children.push(honored_el("Angle", &a.value, a.must_honor));
            }
            fin.children.push(e);
        }
        if let Some(h) = &j.hole_punch {
            fin.children.push(mh_attr(
                Element::new(ns::PRINT, "HolePunch")
                    .with_child(Element::text_el(ns::PRINT, "Edge", h.edge.as_str()))
                    .with_child(Element::text_el(ns::PRINT, "Pattern", h.pattern.as_str())),
                h.must_honor,
            ));
        }
        if !fin.children.is_empty() {
            jp.children.push(fin);
        }
        if let Some(p) = &j.priority {
            jp.children
                .push(honored_el("Priority", &p.value.to_string(), p.must_honor));
        }
        if !jp.children.is_empty() {
            el.children.push(jp);
        }
        el.children.extend(self.document.to_element());
        el
    }
}

// ---------------------------------------------------------------------------------------------
// Capabilities
// ---------------------------------------------------------------------------------------------

/// What the printer declares it accepts, read from its model.
#[derive(Debug, Clone, Default)]
pub struct Capabilities {
    /// A `PrinterCapabilities` section exists. When false nothing is restricted.
    pub known: bool,
    /// `DocumentValues/DocumentDescription/Format` (None: not declared).
    pub formats: Option<Vec<String>>,
    /// `DocumentValues/DocumentDescription/Compression`.
    pub compressions: Option<Vec<String>>,
    pub copies: Option<(i64, i64)>,
    pub priority: Option<(i64, i64)>,
    /// Allowed tokens per DocumentProcessing leaf name.
    pub tokens: BTreeMap<String, Vec<String>>,
    /// `NumberUp` is declared; allowed pages-per-sheet values when listed.
    pub number_up: Option<Vec<i64>>,
    /// `Resolution` is declared; listed (width, height) pairs when present.
    pub resolutions: Option<Vec<(i64, Option<i64>)>>,
    /// `JobFinishings` is declared.
    pub finishings: bool,
    /// Defaults from `DefaultPrintTicket`.
    pub defaults: PrintTicket,
}

fn allowed_values(el: &Element) -> Vec<String> {
    let mut out = Vec::new();
    for av in children(el, "AllowedValue") {
        out.extend(av.text.split_whitespace().map(str::to_string));
    }
    out
}

fn range(el: &Element) -> Option<(i64, i64)> {
    let max = child(el, "MaxValue")?.text_trimmed().parse().ok()?;
    let min = child(el, "MinValue")
        .and_then(|m| m.text_trimmed().parse().ok())
        .unwrap_or(1);
    Some((min, max))
}

impl Capabilities {
    /// Read the capabilities and defaults from a model.
    pub fn from_model(model: &dyn PrinterModel) -> Capabilities {
        let mut c = Capabilities::default();
        if let Some(cap) = model.element(&wprt("PrinterCapabilities")) {
            c.known = true;
            if let Some(jp) = child(&cap, "JobValues").and_then(|v| child(v, "JobProcessing")) {
                c.copies = child(jp, "Copies").and_then(range);
                c.priority = child(jp, "Priority").and_then(range);
                c.finishings = child(jp, "JobFinishings").is_some();
            }
            if let Some(dp) = child(&cap, "JobValues").and_then(|v| child(v, "DocumentProcessing"))
            {
                for leaf in [
                    "MediaSizeName",
                    "MediaType",
                    "MediaColor",
                    "Orientation",
                    "PrintQuality",
                    "Sides",
                ] {
                    if let Some(e) = child(dp, leaf) {
                        c.tokens.insert(leaf.to_string(), allowed_values(e));
                    }
                }
                if let Some(n) = child(dp, "NumberUp") {
                    let mut v = Vec::new();
                    for d in n.descendants() {
                        if d.name.local == "AllowedValue" || d.name.local == "PagesPerSheet" {
                            v.extend(
                                d.text
                                    .split_whitespace()
                                    .filter_map(|t| t.parse::<i64>().ok()),
                            );
                        }
                    }
                    c.number_up = Some(v);
                }
                if let Some(r) = child(dp, "Resolution") {
                    let mut pairs = Vec::new();
                    let mut entries: Vec<&Element> = r
                        .descendants()
                        .into_iter()
                        .filter(|d| d.name.local == "Width")
                        .collect();
                    entries.dedup_by(|a, b| std::ptr::eq(*a, *b));
                    for w in entries {
                        pairs.push((w.text_trimmed().parse().unwrap_or(0), None));
                    }
                    c.resolutions = Some(
                        pairs
                            .into_iter()
                            .filter(|(w, _): &(i64, Option<i64>)| *w > 0)
                            .collect(),
                    );
                }
            }
            if let Some(d) =
                child(&cap, "DocumentValues").and_then(|v| child(v, "DocumentDescription"))
            {
                c.formats = child(d, "Format").map(allowed_values);
                c.compressions = child(d, "Compression").map(allowed_values);
            }
        }
        if let Some(def) = model.element(&wprt("DefaultPrintTicket")) {
            // Defaults are lenient: anything unparsable is ignored.
            let l = PrintLimits::default();
            let q = Quirks {
                lenient_job_description: true,
                ..Quirks::default()
            };
            if let Ok(t) = PrintTicket::parse(&def, &l, &q) {
                c.defaults = t;
            }
        }
        c
    }

    /// Formats the printer accepts (empty when the model declares none: unrestricted).
    pub fn format_list(&self) -> Vec<String> {
        self.formats.clone().unwrap_or_default()
    }

    /// Compression values the printer accepts (`None` only when the model declares none).
    pub fn compression_list(&self) -> Vec<String> {
        self.compressions
            .clone()
            .unwrap_or_else(|| vec!["None".to_string()])
    }

    pub fn format_ok(&self, f: &str) -> bool {
        match &self.formats {
            Some(l) if !l.is_empty() => l.iter().any(|x| x == f),
            _ => true,
        }
    }

    pub fn compression_ok(&self, c: &str) -> bool {
        match &self.compressions {
            Some(l) if !l.is_empty() => l.iter().any(|x| x == c),
            _ => c == "None",
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------------------------

/// A soft value that was replaced or dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Substitution {
    pub element: String,
    pub requested: String,
    /// `None`: the element was dropped.
    pub applied: Option<String>,
}

/// Result of validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Validated {
    pub effective: PrintTicket,
    pub substitutions: Vec<Substitution>,
}

fn unsupported(element: &str, value: &str) -> PrintFault {
    bad(format!(
        "{element}: the printer does not support {value:?} and MustHonor is set"
    ))
}

/// Validate (and complete) a ticket. See the module documentation for the rules.
pub fn validate(
    ticket: &PrintTicket,
    caps: &Capabilities,
    conflicts: &[ConflictRule],
) -> Result<Validated, PrintFault> {
    let mut t = ticket.clone();
    let mut subs = Vec::new();
    validate_job(&mut t.job, caps, &mut subs)?;
    validate_document(&mut t.document, caps, &mut subs)?;
    check_conflicts(&t, conflicts)?;
    Ok(Validated {
        effective: t,
        substitutions: subs,
    })
}

/// Validate a per-document `DocumentProcessing` the same way.
pub fn validate_document(
    d: &mut DocumentProcessing,
    caps: &Capabilities,
    subs: &mut Vec<Substitution>,
) -> Result<(), PrintFault> {
    let defaults = caps.defaults.document.clone();
    let mut defaults = defaults;
    let default_tokens: BTreeMap<&str, Option<Honored<String>>> = defaults
        .tokens()
        .into_iter()
        .map(|(n, v)| (n, v.clone()))
        .collect();
    for (name, slot) in d.tokens() {
        let allowed = caps.tokens.get(name);
        let ok =
            |v: &str| -> bool { !caps.known || allowed.is_some_and(|l| l.iter().any(|x| x == v)) };
        let default = default_tokens
            .get(name)
            .and_then(|o| o.as_ref())
            .map(|h| h.value.clone())
            .filter(|v| ok(v));
        match slot.take() {
            Some(h) if ok(&h.value) => *slot = Some(h),
            Some(h) if h.must_honor => return Err(unsupported(name, &h.value)),
            Some(h) => {
                subs.push(Substitution {
                    element: name.to_string(),
                    requested: h.value,
                    applied: default.clone(),
                });
                *slot = default.map(Honored::soft);
            }
            None => *slot = default.map(Honored::soft),
        }
    }
    if let Some(n) = d.number_up.take() {
        let ok = match &caps.number_up {
            _ if !caps.known => true,
            None => n.pages_per_sheet.value == 1,
            Some(l) if l.is_empty() => true,
            Some(l) => l.contains(&i64::from(n.pages_per_sheet.value)),
        };
        if ok {
            d.number_up = Some(n);
        } else if n.pages_per_sheet.must_honor || n.direction.must_honor {
            return Err(unsupported(
                "NumberUp",
                &n.pages_per_sheet.value.to_string(),
            ));
        } else {
            subs.push(Substitution {
                element: "NumberUp".into(),
                requested: n.pages_per_sheet.value.to_string(),
                applied: None,
            });
        }
    }
    if let Some(r) = d.resolution.take() {
        let ok = match &caps.resolutions {
            _ if !caps.known => true,
            None => false,
            Some(l) if l.is_empty() => true,
            Some(l) => l.iter().any(|(w, _)| *w == i64::from(r.width)),
        };
        if ok {
            d.resolution = Some(r);
        } else if r.must_honor {
            return Err(unsupported("Resolution", &r.width.to_string()));
        } else {
            subs.push(Substitution {
                element: "Resolution".into(),
                requested: r.width.to_string(),
                applied: None,
            });
        }
    }
    Ok(())
}

fn in_range(v: u32, r: Option<(i64, i64)>, known: bool) -> bool {
    match r {
        Some((lo, hi)) => (lo..=hi).contains(&i64::from(v)),
        None => !known || v == 1,
    }
}

fn validate_job(
    j: &mut JobProcessing,
    caps: &Capabilities,
    subs: &mut Vec<Substitution>,
) -> Result<(), PrintFault> {
    let def_copies = caps
        .defaults
        .job
        .copies
        .as_ref()
        .map(|c| c.value)
        .filter(|v| in_range(*v, caps.copies, caps.known));
    match j.copies.take() {
        Some(c) if in_range(c.value, caps.copies, caps.known) => j.copies = Some(c),
        Some(c) if c.must_honor => return Err(unsupported("Copies", &c.value.to_string())),
        Some(c) => {
            subs.push(Substitution {
                element: "Copies".into(),
                requested: c.value.to_string(),
                applied: def_copies.map(|v| v.to_string()),
            });
            j.copies = def_copies.map(Honored::soft);
        }
        None => j.copies = def_copies.map(Honored::soft),
    }
    let def_pri = caps
        .defaults
        .job
        .priority
        .as_ref()
        .map(|c| c.value)
        .filter(|v| in_range(*v, caps.priority, caps.known));
    match j.priority.take() {
        Some(p) if in_range(p.value, caps.priority, caps.known) => j.priority = Some(p),
        Some(p) if p.must_honor => return Err(unsupported("Priority", &p.value.to_string())),
        Some(p) => {
            subs.push(Substitution {
                element: "Priority".into(),
                requested: p.value.to_string(),
                applied: def_pri.map(|v| v.to_string()),
            });
            j.priority = def_pri.map(Honored::soft);
        }
        None => j.priority = def_pri.map(Honored::soft),
    }
    // Finishings: the capability list is unspecified in the package; "off" values are always
    // fine, any real finishing needs the section to be declared.
    let off = |on: bool| !on;
    if let Some(c) = j.collate.take() {
        keep_or_reject(
            "Collate",
            c.must_honor,
            off(c.value) || caps.finishings || !caps.known,
            subs,
            c.value.to_string(),
        )?
        .then(|| j.collate = Some(c));
    }
    if let Some(c) = j.jog_offset.take() {
        keep_or_reject(
            "JogOffset",
            c.must_honor,
            off(c.value) || caps.finishings || !caps.known,
            subs,
            c.value.to_string(),
        )?
        .then(|| j.jog_offset = Some(c));
    }
    if let Some(s) = j.staple.take() {
        let ok = s.location.value == "None" || caps.finishings || !caps.known;
        keep_or_reject(
            "Staple",
            s.location.must_honor,
            ok,
            subs,
            s.location.value.clone(),
        )?
        .then(|| j.staple = Some(s));
    }
    if let Some(h) = j.hole_punch.take() {
        let ok = h.edge == "None" || caps.finishings || !caps.known;
        keep_or_reject("HolePunch", h.must_honor, ok, subs, h.edge.clone())?
            .then(|| j.hole_punch = Some(h));
    }
    Ok(())
}

fn keep_or_reject(
    name: &str,
    must: bool,
    ok: bool,
    subs: &mut Vec<Substitution>,
    requested: String,
) -> Result<bool, PrintFault> {
    if ok {
        Ok(true)
    } else if must {
        Err(unsupported(name, &requested))
    } else {
        subs.push(Substitution {
            element: name.to_string(),
            requested,
            applied: None,
        });
        Ok(false)
    }
}

fn leaf_value(t: &PrintTicket, leaf: &str) -> Option<(String, bool)> {
    let tok = |v: &Option<Honored<String>>| v.as_ref().map(|h| (h.value.clone(), h.must_honor));
    match leaf {
        "MediaSizeName" => tok(&t.document.media_size_name),
        "MediaType" => tok(&t.document.media_type),
        "MediaColor" => tok(&t.document.media_color),
        "Orientation" => tok(&t.document.orientation),
        "PrintQuality" => tok(&t.document.print_quality),
        "Sides" => tok(&t.document.sides),
        "Copies" => t
            .job
            .copies
            .as_ref()
            .map(|c| (c.value.to_string(), c.must_honor)),
        "Priority" => t
            .job
            .priority
            .as_ref()
            .map(|c| (c.value.to_string(), c.must_honor)),
        "PagesPerSheet" => t.document.number_up.as_ref().map(|n| {
            (
                n.pages_per_sheet.value.to_string(),
                n.pages_per_sheet.must_honor,
            )
        }),
        _ => None,
    }
}

fn check_conflicts(t: &PrintTicket, rules: &[ConflictRule]) -> Result<(), PrintFault> {
    for r in rules {
        let a = leaf_value(t, &r.a.0);
        let b = leaf_value(t, &r.b.0);
        if let (Some((av, am)), Some((bv, bm))) = (a, b) {
            if am && bm && av == r.a.1 && bv == r.b.1 {
                return Err(
                    PrintFault::new(FaultKind::ClientErrorConflictingRequiredParameters)
                        .with_reason(format!(
                            "{}={} and {}={} cannot both be honored",
                            r.a.0, av, r.b.0, bv
                        )),
                );
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wsd::xml::{parse_str, XmlLimits};

    const HEAD: &str =
        r#"<wprt:PrintTicket xmlns:wprt="http://schemas.microsoft.com/windows/2006/08/wdp/print">"#;

    fn ticket_xml(body: &str) -> Element {
        parse_str(
            &format!("{HEAD}{body}</wprt:PrintTicket>"),
            &XmlLimits::SMALL,
        )
        .unwrap()
    }

    const JD: &str = "<wprt:JobDescription><wprt:JobName>n</wprt:JobName><wprt:JobOriginatingUserName>u</wprt:JobOriginatingUserName></wprt:JobDescription>";

    fn caps() -> Capabilities {
        let mut c = Capabilities {
            known: true,
            copies: Some((1, 99)),
            priority: Some((1, 100)),
            ..Capabilities::default()
        };
        c.tokens.insert(
            "Sides".into(),
            vec!["OneSided".into(), "TwoSidedLongEdge".into()],
        );
        c.tokens.insert(
            "MediaSizeName".into(),
            vec!["iso_a4_210x297mm".into(), "na_letter_8.5x11in".into()],
        );
        c.tokens.insert(
            "MediaType".into(),
            vec!["stationery".into(), "photographic".into()],
        );
        c.tokens
            .insert("Orientation".into(), vec!["Portrait".into()]);
        c.tokens
            .insert("PrintQuality".into(), vec!["Normal".into(), "High".into()]);
        c.defaults.document.sides = Some(Honored::soft("OneSided".into()));
        c.defaults.document.media_size_name = Some(Honored::soft("iso_a4_210x297mm".into()));
        c.defaults.job.copies = Some(Honored::soft(1));
        c
    }

    fn run(body: &str) -> Result<Validated, PrintFault> {
        let t = PrintTicket::parse(
            &ticket_xml(&format!("{JD}{body}")),
            &PrintLimits::default(),
            &Quirks::default(),
        )?;
        validate(&t, &caps(), &[])
    }

    fn kind(r: Result<Validated, PrintFault>) -> Option<FaultKind> {
        r.unwrap_err().kind()
    }

    #[test]
    fn parses_and_round_trips_a_full_ticket() {
        let el = ticket_xml(&format!(
            "{JD}<wprt:JobProcessing><wprt:Copies wprt:MustHonor=\"true\">2</wprt:Copies><wprt:JobFinishings><wprt:Collate>true</wprt:Collate><wprt:Staple><wprt:Location>StapleTopLeft</wprt:Location></wprt:Staple></wprt:JobFinishings><wprt:Priority>5</wprt:Priority></wprt:JobProcessing>\
             <wprt:DocumentProcessing><wprt:MediaSizeName>iso_a4_210x297mm</wprt:MediaSizeName><wprt:NumberUp><wprt:PagesPerSheet>2</wprt:PagesPerSheet><wprt:Direction>RightDown</wprt:Direction></wprt:NumberUp><wprt:Orientation>Portrait</wprt:Orientation><wprt:Resolution><wprt:Width>600</wprt:Width><wprt:Height>600</wprt:Height></wprt:Resolution><wprt:Sides>OneSided</wprt:Sides></wprt:DocumentProcessing>"
        ));
        let t = PrintTicket::parse(&el, &PrintLimits::default(), &Quirks::default()).unwrap();
        assert_eq!(t.copies(), 2);
        assert!(t.job.copies.as_ref().unwrap().must_honor);
        let again =
            PrintTicket::parse(&t.to_element(), &PrintLimits::default(), &Quirks::default());
        assert_eq!(again.unwrap(), t);
    }

    #[test]
    fn parse_error_matrix() {
        let l = PrintLimits::default();
        let q = Quirks::default();
        let cases = [
            "",                                                                                          // no JobDescription
            "<wprt:JobDescription><wprt:JobName>n</wprt:JobName></wprt:JobDescription>",                   // no user
            "<wprt:JobDescription><wprt:JobOriginatingUserName>u</wprt:JobOriginatingUserName></wprt:JobDescription>",
        ];
        for c in cases {
            assert_eq!(
                PrintTicket::parse(&ticket_xml(c), &l, &q)
                    .unwrap_err()
                    .kind(),
                Some(FaultKind::InvalidArgs),
                "{c}"
            );
        }
        for body in [
            "<wprt:JobProcessing><wprt:Copies>0</wprt:Copies></wprt:JobProcessing>",
            "<wprt:JobProcessing><wprt:Copies>two</wprt:Copies></wprt:JobProcessing>",
            "<wprt:JobProcessing><wprt:Priority>101</wprt:Priority></wprt:JobProcessing>",
            "<wprt:JobProcessing><wprt:Copies wprt:MustHonor=\"maybe\">1</wprt:Copies></wprt:JobProcessing>",
            "<wprt:DocumentProcessing><wprt:Sides>Duplex</wprt:Sides></wprt:DocumentProcessing>",
            "<wprt:DocumentProcessing><wprt:Orientation>Sideways</wprt:Orientation></wprt:DocumentProcessing>",
            "<wprt:DocumentProcessing><wprt:MediaType>two words</wprt:MediaType></wprt:DocumentProcessing>",
            "<wprt:DocumentProcessing><wprt:NumberUp><wprt:PagesPerSheet>2</wprt:PagesPerSheet></wprt:NumberUp></wprt:DocumentProcessing>",
        ] {
            let el = ticket_xml(&format!("{JD}{body}"));
            assert_eq!(PrintTicket::parse(&el, &l, &q).unwrap_err().kind(), Some(FaultKind::InvalidArgs), "{body}");
        }
        // extension values are accepted
        let el = ticket_xml(&format!("{JD}<wprt:DocumentProcessing><wprt:Sides>vendor:Booklet</wprt:Sides></wprt:DocumentProcessing>"));
        assert!(PrintTicket::parse(&el, &l, &q).is_ok());
    }

    #[test]
    fn lenient_job_description_is_opt_in() {
        let el = ticket_xml("");
        let q = Quirks {
            lenient_job_description: true,
            ..Quirks::default()
        };
        let t = PrintTicket::parse(&el, &PrintLimits::default(), &q).unwrap();
        assert!(t.description.is_none());
    }

    #[test]
    fn supported_values_pass_unchanged_and_defaults_fill_in() {
        let v = run("<wprt:JobProcessing><wprt:Copies>3</wprt:Copies></wprt:JobProcessing><wprt:DocumentProcessing><wprt:Sides>TwoSidedLongEdge</wprt:Sides></wprt:DocumentProcessing>").unwrap();
        assert_eq!(v.effective.copies(), 3);
        assert_eq!(
            v.effective.document.sides.as_ref().unwrap().value,
            "TwoSidedLongEdge"
        );
        assert_eq!(
            v.effective.document.media_size_name.as_ref().unwrap().value,
            "iso_a4_210x297mm"
        );
        assert!(v.substitutions.is_empty());
    }

    #[test]
    fn must_honor_unsupported_value_rejects() {
        for body in [
            "<wprt:DocumentProcessing><wprt:Sides wprt:MustHonor=\"true\">TwoSidedShortEdge</wprt:Sides></wprt:DocumentProcessing>",
            "<wprt:DocumentProcessing><wprt:Sides wprt:MustHonor=\"1\">TwoSidedShortEdge</wprt:Sides></wprt:DocumentProcessing>",
            "<wprt:JobProcessing><wprt:Copies wprt:MustHonor=\"true\">100</wprt:Copies></wprt:JobProcessing>",
            "<wprt:JobProcessing><wprt:Priority wprt:MustHonor=\"true\">0</wprt:Priority></wprt:JobProcessing>",
            "<wprt:DocumentProcessing><wprt:MediaColor wprt:MustHonor=\"true\">pink</wprt:MediaColor></wprt:DocumentProcessing>",
            "<wprt:DocumentProcessing><wprt:Resolution wprt:MustHonor=\"true\"><wprt:Width>1200</wprt:Width></wprt:Resolution></wprt:DocumentProcessing>",
            "<wprt:DocumentProcessing><wprt:NumberUp><wprt:PagesPerSheet wprt:MustHonor=\"true\">4</wprt:PagesPerSheet><wprt:Direction>RightDown</wprt:Direction></wprt:NumberUp></wprt:DocumentProcessing>",
            "<wprt:JobProcessing><wprt:JobFinishings><wprt:Staple><wprt:Location wprt:MustHonor=\"true\">StapleTopLeft</wprt:Location></wprt:Staple></wprt:JobFinishings></wprt:JobProcessing>",
        ] {
            assert_eq!(kind(run(body)), Some(FaultKind::InvalidArgs), "{body}");
        }
    }

    #[test]
    fn soft_unsupported_value_is_substituted_or_dropped() {
        let v = run("<wprt:JobProcessing><wprt:Copies>500</wprt:Copies></wprt:JobProcessing><wprt:DocumentProcessing><wprt:Sides>TwoSidedShortEdge</wprt:Sides><wprt:MediaColor>pink</wprt:MediaColor><wprt:Resolution><wprt:Width>1200</wprt:Width></wprt:Resolution></wprt:DocumentProcessing>").unwrap();
        assert_eq!(v.effective.copies(), 1);
        assert_eq!(
            v.effective.document.sides.as_ref().unwrap().value,
            "OneSided"
        );
        assert!(v.effective.document.media_color.is_none());
        assert!(v.effective.document.resolution.is_none());
        let by: BTreeMap<_, _> = v
            .substitutions
            .iter()
            .map(|s| (s.element.as_str(), s.applied.clone()))
            .collect();
        assert_eq!(by["Copies"], Some("1".into()));
        assert_eq!(by["Sides"], Some("OneSided".into()));
        assert_eq!(by["MediaColor"], None);
        assert_eq!(by["Resolution"], None);
    }

    #[test]
    fn conflict_rule_needs_must_honor_on_both() {
        let rule = ConflictRule {
            a: ("MediaType".into(), "photographic".into()),
            b: ("MediaSizeName".into(), "na_letter_8.5x11in".into()),
        };
        let body = |m1: &str, m2: &str| {
            format!("<wprt:DocumentProcessing><wprt:MediaSizeName {m2}>na_letter_8.5x11in</wprt:MediaSizeName><wprt:MediaType {m1}>photographic</wprt:MediaType></wprt:DocumentProcessing>")
        };
        let mh = "wprt:MustHonor=\"true\"";
        let parse = |b: String| {
            PrintTicket::parse(
                &ticket_xml(&format!("{JD}{b}")),
                &PrintLimits::default(),
                &Quirks::default(),
            )
            .unwrap()
        };
        let err = validate(&parse(body(mh, mh)), &caps(), std::slice::from_ref(&rule)).unwrap_err();
        assert_eq!(
            err.kind(),
            Some(FaultKind::ClientErrorConflictingRequiredParameters)
        );
        assert!(validate(&parse(body(mh, "")), &caps(), std::slice::from_ref(&rule)).is_ok());
        assert!(validate(&parse(body("", "")), &caps(), &[rule]).is_ok());
    }

    #[test]
    fn unknown_model_restricts_nothing() {
        let t = PrintTicket::parse(
            &ticket_xml(&format!("{JD}<wprt:DocumentProcessing><wprt:Sides wprt:MustHonor=\"true\">TwoSidedShortEdge</wprt:Sides></wprt:DocumentProcessing>")),
            &PrintLimits::default(),
            &Quirks::default(),
        )
        .unwrap();
        assert!(validate(&t, &Capabilities::default(), &[]).is_ok());
    }

    struct M(Vec<(&'static str, Element)>);
    impl PrinterModel for M {
        fn element(&self, name: &QName) -> Option<Element> {
            self.0
                .iter()
                .find(|(n, _)| *n == name.local)
                .map(|(_, e)| e.clone())
        }
    }

    #[test]
    fn capabilities_are_read_from_the_model() {
        let ns = "xmlns:wprt=\"http://schemas.microsoft.com/windows/2006/08/wdp/print\"";
        let cap = parse_str(&format!("<wprt:PrinterCapabilities {ns}><wprt:JobValues><wprt:JobProcessing><wprt:Copies><wprt:MinValue>1</wprt:MinValue><wprt:MaxValue>9</wprt:MaxValue></wprt:Copies></wprt:JobProcessing><wprt:DocumentProcessing><wprt:Sides><wprt:AllowedValue>OneSided TwoSidedLongEdge</wprt:AllowedValue></wprt:Sides></wprt:DocumentProcessing></wprt:JobValues><wprt:DocumentValues><wprt:DocumentDescription><wprt:Compression><wprt:AllowedValue>None</wprt:AllowedValue></wprt:Compression><wprt:Format><wprt:AllowedValue>application/pdf</wprt:AllowedValue><wprt:AllowedValue>application/octet-stream</wprt:AllowedValue></wprt:Format></wprt:DocumentDescription></wprt:DocumentValues></wprt:PrinterCapabilities>"), &XmlLimits::SMALL).unwrap();
        let def = parse_str(&format!("<wprt:DefaultPrintTicket {ns}><wprt:JobProcessing><wprt:Copies>1</wprt:Copies></wprt:JobProcessing><wprt:DocumentProcessing><wprt:Sides>OneSided</wprt:Sides></wprt:DocumentProcessing></wprt:DefaultPrintTicket>"), &XmlLimits::SMALL).unwrap();
        let c = Capabilities::from_model(&M(vec![
            ("PrinterCapabilities", cap),
            ("DefaultPrintTicket", def),
        ]));
        assert!(c.known);
        assert_eq!(c.copies, Some((1, 9)));
        assert_eq!(c.tokens["Sides"], vec!["OneSided", "TwoSidedLongEdge"]);
        assert!(c.format_ok("application/pdf") && !c.format_ok("image/png"));
        assert!(c.compression_ok("None") && !c.compression_ok("Gzip"));
        assert_eq!(c.defaults.document.sides.unwrap().value, "OneSided");
        // a model without capabilities leaves formats open and compression at None
        let none = Capabilities::from_model(&M(vec![]));
        assert!(
            !none.known
                && none.format_ok("x/y")
                && none.compression_ok("None")
                && !none.compression_ok("Gzip")
        );
    }

    #[test]
    fn to_json_carries_typed_fields_and_must_honor_flags() {
        let el = ticket_xml(&format!(
            "{JD}<wprt:JobProcessing><wprt:Copies wprt:MustHonor=\"true\">2</wprt:Copies></wprt:JobProcessing><wprt:DocumentProcessing><wprt:Sides>OneSided</wprt:Sides><wprt:Resolution><wprt:Width>300</wprt:Width></wprt:Resolution></wprt:DocumentProcessing>"
        ));
        let t = PrintTicket::parse(&el, &PrintLimits::default(), &Quirks::default()).unwrap();
        let j = t.to_json();
        assert_eq!(j["job_name"], "n");
        assert_eq!(j["user"], "u");
        assert_eq!(j["copies"]["value"], 2);
        assert_eq!(j["copies"]["must_honor"], true);
        assert_eq!(j["sides"]["value"], "OneSided");
        assert_eq!(j["sides"]["must_honor"], false);
        assert_eq!(j["resolution"]["width"], 300);
        assert!(j["resolution"]["height"].is_null());
        assert!(j["orientation"].is_null());
        assert_eq!(j["must_honor"], serde_json::json!(["copies"]));
    }
}
