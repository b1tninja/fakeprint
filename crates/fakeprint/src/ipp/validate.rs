//! The reply-validation switch of the IPP face.
//!
//! The switch is the WS-Print one (`FAKEPRINT_VALIDATE_REPLIES`, `--validate-replies`,
//! `ServerConfig::validate_replies`): when it is on, every IPP reply the server sends is parsed
//! again and checked for what RFC 8011 requires of a reply. A violation is never fatal: it is
//! logged, counted, and the newest messages are kept for `GET /v1/status`
//! (`ipp_validation`) and `ServerHandle::ipp_validation_problems`.
//!
//! Checked: version and request id echo; the operation-attributes group first, with
//! `attributes-charset` then `attributes-natural-language`; `status-message` on errors; group
//! order (operation, unsupported, then job or printer groups); an unsupported-attributes group
//! exactly when the status says attributes or values were ignored or refused; the attributes
//! each successful operation must return (the printer's mandatory description attributes in a
//! full Get-Printer-Attributes, the four job attributes after a job is created, `job-id` in
//! every Get-Jobs entry); the value syntax of the well-known attributes; and the parallel
//! arrays (`uri-security-supported` / `uri-authentication-supported` against
//! `printer-uri-supported`, the `marker-*` lists against each other).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use super::codec::{self, op, status, tag, Attr, Group, Message};
use crate::wsd::validate::{ReplyValidator, ValidationStats};

const KEEP: usize = 64;

/// Value tags the well-known attributes must use.
const SYNTAX: &[(&str, &[u8])] = &[
    ("printer-uri-supported", &[tag::URI]),
    ("uri-security-supported", &[tag::KEYWORD]),
    ("uri-authentication-supported", &[tag::KEYWORD]),
    (
        "printer-name",
        &[tag::NAME_WITHOUT_LANGUAGE, tag::NAME_WITH_LANGUAGE],
    ),
    (
        "printer-info",
        &[tag::TEXT_WITHOUT_LANGUAGE, tag::TEXT_WITH_LANGUAGE],
    ),
    (
        "printer-location",
        &[tag::TEXT_WITHOUT_LANGUAGE, tag::TEXT_WITH_LANGUAGE],
    ),
    (
        "printer-make-and-model",
        &[tag::TEXT_WITHOUT_LANGUAGE, tag::TEXT_WITH_LANGUAGE],
    ),
    ("printer-uuid", &[tag::URI]),
    ("printer-state", &[tag::ENUM]),
    ("printer-state-reasons", &[tag::KEYWORD]),
    ("printer-is-accepting-jobs", &[tag::BOOLEAN]),
    ("queued-job-count", &[tag::INTEGER]),
    ("printer-up-time", &[tag::INTEGER]),
    ("printer-current-time", &[tag::DATE_TIME]),
    ("ipp-versions-supported", &[tag::KEYWORD]),
    ("operations-supported", &[tag::ENUM]),
    ("charset-configured", &[tag::CHARSET]),
    ("charset-supported", &[tag::CHARSET]),
    ("natural-language-configured", &[tag::NATURAL_LANGUAGE]),
    (
        "generated-natural-language-supported",
        &[tag::NATURAL_LANGUAGE],
    ),
    ("document-format-default", &[tag::MIME_MEDIA_TYPE]),
    ("document-format-supported", &[tag::MIME_MEDIA_TYPE]),
    ("pdl-override-supported", &[tag::KEYWORD]),
    ("compression-supported", &[tag::KEYWORD]),
    ("color-supported", &[tag::BOOLEAN]),
    ("pages-per-minute", &[tag::INTEGER]),
    ("multiple-operation-time-out", &[tag::INTEGER]),
    ("copies-supported", &[tag::RANGE_OF_INTEGER]),
    ("sides-supported", &[tag::KEYWORD]),
    ("orientation-requested-supported", &[tag::ENUM]),
    ("print-quality-supported", &[tag::ENUM]),
    ("printer-resolution-supported", &[tag::RESOLUTION]),
    (
        "media-supported",
        &[tag::KEYWORD, tag::NAME_WITHOUT_LANGUAGE],
    ),
    ("media-col-ready", &[tag::BEG_COLLECTION]),
    ("marker-levels", &[tag::INTEGER]),
    ("marker-names", &[tag::NAME_WITHOUT_LANGUAGE]),
    ("marker-types", &[tag::KEYWORD]),
    ("job-id", &[tag::INTEGER]),
    ("job-uri", &[tag::URI]),
    ("job-uuid", &[tag::URI]),
    ("job-state", &[tag::ENUM]),
    ("job-state-reasons", &[tag::KEYWORD]),
    (
        "job-name",
        &[tag::NAME_WITHOUT_LANGUAGE, tag::NAME_WITH_LANGUAGE],
    ),
    (
        "job-originating-user-name",
        &[tag::NAME_WITHOUT_LANGUAGE, tag::NAME_WITH_LANGUAGE],
    ),
    ("job-printer-uri", &[tag::URI]),
    ("time-at-creation", &[tag::INTEGER]),
    ("time-at-processing", &[tag::INTEGER, tag::NO_VALUE]),
    ("time-at-completed", &[tag::INTEGER, tag::NO_VALUE]),
    ("number-of-documents", &[tag::INTEGER]),
    ("job-impressions-completed", &[tag::INTEGER]),
    ("job-media-sheets-completed", &[tag::INTEGER]),
    ("status-message", &[tag::TEXT_WITHOUT_LANGUAGE]),
];

/// Attributes a full Get-Printer-Attributes reply must carry (RFC 8011 section 5.4).
const PRINTER_REQUIRED: [&str; 19] = [
    "printer-uri-supported",
    "uri-security-supported",
    "uri-authentication-supported",
    "printer-name",
    "printer-state",
    "printer-state-reasons",
    "ipp-versions-supported",
    "operations-supported",
    "charset-configured",
    "charset-supported",
    "natural-language-configured",
    "generated-natural-language-supported",
    "document-format-default",
    "document-format-supported",
    "printer-is-accepting-jobs",
    "queued-job-count",
    "pdl-override-supported",
    "printer-up-time",
    "compression-supported",
];

/// Counts and keeps the violations. Cheap to share.
pub struct IppValidator {
    switch: Arc<ReplyValidator>,
    checked: AtomicU64,
    problems: AtomicU64,
    recent: Mutex<VecDeque<String>>,
}

impl IppValidator {
    /// The validator follows the on/off switch of the WS-Print `ReplyValidator`.
    pub fn new(switch: Arc<ReplyValidator>) -> Self {
        Self {
            switch,
            checked: AtomicU64::new(0),
            problems: AtomicU64::new(0),
            recent: Mutex::new(VecDeque::new()),
        }
    }

    pub fn enabled(&self) -> bool {
        self.switch.enabled()
    }

    pub fn clear(&self) {
        self.checked.store(0, Ordering::Relaxed);
        self.problems.store(0, Ordering::Relaxed);
        self.lock().clear();
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, VecDeque<String>> {
        match self.recent.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        }
    }

    pub fn recent_problems(&self) -> Vec<String> {
        self.lock().iter().cloned().collect()
    }

    pub fn stats(&self) -> ValidationStats {
        ValidationStats {
            enabled: self.enabled(),
            checked: self.checked.load(Ordering::Relaxed),
            problems: self.problems.load(Ordering::Relaxed),
            recent: self.recent_problems(),
        }
    }

    /// Check one reply against its request (when the request parsed). No-op when off.
    pub fn check(&self, request: Option<&Message>, reply: &[u8]) {
        if !self.enabled() {
            return;
        }
        self.checked.fetch_add(1, Ordering::Relaxed);
        for p in problems(request, reply) {
            tracing::error!(problem = %p, "IPP reply violates RFC 8011");
            self.problems.fetch_add(1, Ordering::Relaxed);
            let mut r = self.lock();
            if r.len() >= KEEP {
                r.pop_front();
            }
            r.push_back(p);
        }
    }
}

fn names(g: &Group) -> Vec<&str> {
    g.attrs.iter().map(|a| a.name.as_str()).collect()
}

fn check_syntax(g: &Group, out: &mut Vec<String>) {
    for a in &g.attrs {
        if a.values.is_empty() {
            out.push(format!("{}: attribute without a value", a.name));
        }
        if let Some((_, tags)) = SYNTAX.iter().find(|s| s.0 == a.name) {
            for v in &a.values {
                if !tags.contains(&v.tag()) {
                    out.push(format!(
                        "{}: value tag {:#04x} is not one of {:02x?}",
                        a.name,
                        v.tag(),
                        tags
                    ));
                }
            }
        }
        let tags_of_first = a.values.first().map(|v| v.tag());
        if a.values.iter().any(|v| Some(v.tag()) != tags_of_first)
            && !a
                .values
                .iter()
                .all(|v| matches!(v.tag(), tag::KEYWORD | tag::NAME_WITHOUT_LANGUAGE))
        {
            out.push(format!("{}: values of mixed syntax", a.name));
        }
    }
}

fn len_of(g: &Group, n: &str) -> Option<usize> {
    g.get(n).map(|a: &Attr| a.values.len())
}

/// Every violation found in `reply`.
pub fn problems(request: Option<&Message>, reply: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let m = match Message::parse(reply) {
        Ok(m) => m,
        Err(e) => return vec![format!("the reply does not parse: {e}")],
    };
    if !matches!(m.version, (1, 0) | (1, 1) | (2, 0)) {
        out.push(format!(
            "version {}.{} is not an IPP version",
            m.version.0, m.version.1
        ));
    }
    if let Some(r) = request {
        if m.request_id != r.request_id {
            out.push(format!(
                "request-id {} does not echo the request's {}",
                m.request_id, r.request_id
            ));
        }
        if matches!(r.version, (1, 0) | (1, 1) | (2, 0)) && m.version != r.version {
            out.push("the reply version differs from the request version".into());
        }
    }
    let Some(first) = m.groups.first() else {
        out.push("no attribute groups".into());
        return out;
    };
    if first.tag != tag::OPERATION_ATTRIBUTES {
        out.push("the first group is not the operation attributes".into());
    }
    if names(first).get(..2) != Some(&["attributes-charset", "attributes-natural-language"]) {
        out.push("attributes-charset and attributes-natural-language must come first".into());
    }
    let mut tags: Vec<u8> = m.groups.iter().map(|g| g.tag).collect();
    tags.dedup();
    let unsup = m.group(tag::UNSUPPORTED_ATTRIBUTES);
    if let Some(pos) = m
        .groups
        .iter()
        .position(|g| g.tag == tag::UNSUPPORTED_ATTRIBUTES)
    {
        if pos != 1 {
            out.push(
                "the unsupported-attributes group must follow the operation attributes".into(),
            );
        }
    }
    for t in [
        tag::OPERATION_ATTRIBUTES,
        tag::UNSUPPORTED_ATTRIBUTES,
        tag::PRINTER_ATTRIBUTES,
    ] {
        if m.groups.iter().filter(|g| g.tag == t).count() > 1 {
            out.push(format!("group {t:#04x} appears more than once"));
        }
    }
    for g in &m.groups {
        check_syntax(g, &mut out);
    }
    let st = m.code;
    if st != status::OK && first.get("status-message").is_none() && !codec::is_success(st) {
        out.push("an error reply without status-message".into());
    }
    let needs_unsupported = matches!(
        st,
        status::OK_IGNORED_OR_SUBSTITUTED
            | status::CLIENT_ATTRIBUTES_OR_VALUES_NOT_SUPPORTED
            | status::CLIENT_DOCUMENT_FORMAT_NOT_SUPPORTED
            | status::CLIENT_COMPRESSION_NOT_SUPPORTED
    );
    if needs_unsupported && unsup.is_none_or(|g| g.attrs.is_empty()) {
        out.push(format!(
            "{} without attributes in the unsupported-attributes group",
            codec::status_name(st)
        ));
    }
    if !needs_unsupported && codec::is_success(st) && unsup.is_some() {
        out.push("an unsupported-attributes group on a reply that has nothing unsupported".into());
    }
    if st >= 0x0400 {
        if m.groups
            .iter()
            .any(|g| matches!(g.tag, tag::JOB_ATTRIBUTES | tag::PRINTER_ATTRIBUTES))
        {
            out.push("an error reply carries job or printer attributes".into());
        }
        return out;
    }
    let Some(req) = request else { return out };
    let jobs: Vec<&Group> = m
        .groups
        .iter()
        .filter(|g| g.tag == tag::JOB_ATTRIBUTES)
        .collect();
    let printer = m.group(tag::PRINTER_ATTRIBUTES);
    match req.code {
        op::GET_PRINTER_ATTRIBUTES => {
            let Some(p) = printer else {
                out.push("Get-Printer-Attributes without a printer group".into());
                return out;
            };
            let full = req
                .group(tag::OPERATION_ATTRIBUTES)
                .and_then(|g| g.get("requested-attributes"))
                .is_none_or(|a| a.values.iter().any(|v| v.as_str() == Some("all")));
            if full {
                for n in PRINTER_REQUIRED {
                    if p.get(n).is_none() {
                        out.push(format!("required printer attribute {n} is missing"));
                    }
                }
            }
            let uris = len_of(p, "printer-uri-supported");
            for n in ["uri-security-supported", "uri-authentication-supported"] {
                if full && len_of(p, n) != uris {
                    out.push(format!("{n} is not parallel to printer-uri-supported"));
                }
            }
            let markers = [
                "marker-names",
                "marker-types",
                "marker-levels",
                "marker-colors",
            ];
            let lens: Vec<_> = markers.iter().filter_map(|n| len_of(p, n)).collect();
            if lens.windows(2).any(|w| w[0] != w[1]) {
                out.push("the marker-* lists differ in length".into());
            }
            if !jobs.is_empty() {
                out.push("Get-Printer-Attributes returned a job group".into());
            }
        }
        op::PRINT_JOB | op::CREATE_JOB | op::SEND_DOCUMENT => match jobs.as_slice() {
            [j] => {
                for n in ["job-id", "job-uri", "job-state", "job-state-reasons"] {
                    if j.get(n).is_none() {
                        out.push(format!("job attribute {n} is missing from the reply"));
                    }
                }
            }
            other => out.push(format!("expected one job group, found {}", other.len())),
        },
        op::GET_JOB_ATTRIBUTES => {
            if jobs.len() != 1 {
                out.push(format!("expected one job group, found {}", jobs.len()));
            }
        }
        op::GET_JOBS => {
            let asked_id = req
                .group(tag::OPERATION_ATTRIBUTES)
                .and_then(|g| g.get("requested-attributes"))
                .is_none_or(|a| {
                    a.values
                        .iter()
                        .any(|v| matches!(v.as_str(), Some("job-id" | "all" | "job-description")))
                });
            if asked_id {
                for j in &jobs {
                    if j.get("job-id").is_none() {
                        out.push("a Get-Jobs entry has no job-id".into());
                    }
                }
            }
        }
        op::VALIDATE_JOB | op::CANCEL_JOB if !jobs.is_empty() || printer.is_some() => {
            out.push("this operation returns no job or printer group".into());
        }
        _ => {}
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipp::codec::Value;

    fn reply(st: u16, build: impl FnOnce(&mut Message)) -> Vec<u8> {
        let mut m = Message::new((2, 0), st, 5);
        let mut g = Group::new(tag::OPERATION_ATTRIBUTES);
        g.push(Attr::new(
            "attributes-charset",
            Value::Charset("utf-8".into()),
        ));
        g.push(Attr::new(
            "attributes-natural-language",
            Value::NaturalLanguage("en".into()),
        ));
        m.groups.push(g);
        build(&mut m);
        m.encode()
    }

    fn req(code: u16) -> Message {
        let mut m = Message::new((2, 0), code, 5);
        m.groups.push(Group::new(tag::OPERATION_ATTRIBUTES));
        m
    }

    #[test]
    fn a_minimal_ok_passes_and_the_common_defects_are_named() {
        let r = req(op::VALIDATE_JOB);
        assert!(problems(Some(&r), &reply(0, |_| {})).is_empty());
        // wrong request id
        let mut bad = req(op::VALIDATE_JOB);
        bad.request_id = 6;
        assert!(problems(Some(&bad), &reply(0, |_| {}))[0].contains("request-id"));
        // charset second
        let mut m = Message::new((2, 0), 0, 5);
        let mut g = Group::new(tag::OPERATION_ATTRIBUTES);
        g.push(Attr::new(
            "attributes-natural-language",
            Value::NaturalLanguage("en".into()),
        ));
        g.push(Attr::new(
            "attributes-charset",
            Value::Charset("utf-8".into()),
        ));
        m.groups.push(g);
        assert!(problems(Some(&r), &m.encode())
            .iter()
            .any(|p| p.contains("must come first")));
        // error without status-message, ok-ignored without the unsupported group
        assert!(problems(Some(&r), &reply(0x0404, |_| {}))
            .iter()
            .any(|p| p.contains("status-message")));
        assert!(problems(Some(&r), &reply(1, |_| {}))
            .iter()
            .any(|p| p.contains("unsupported-attributes")));
        // wrong syntax
        let out = reply(0, |m| {
            let mut j = Group::new(tag::JOB_ATTRIBUTES);
            j.push(Attr::new("job-id", Value::keyword("x")));
            m.groups.push(j);
        });
        let pr = req(op::CREATE_JOB);
        let p = problems(Some(&pr), &out);
        assert!(p.iter().any(|x| x.contains("job-id: value tag")), "{p:?}");
        assert!(p.iter().any(|x| x.contains("job-uri is missing")), "{p:?}");
        // garbage
        assert!(!problems(None, &[1, 2, 3]).is_empty());
    }

    #[test]
    fn counters_follow_the_switch() {
        let sw = Arc::new(ReplyValidator::new(false));
        let v = IppValidator::new(sw.clone());
        v.check(None, &[1]);
        assert_eq!(v.stats().checked, 0);
        sw.set_enabled(true);
        v.check(None, &[1]);
        v.check(None, &reply(0, |_| {}));
        let s = v.stats();
        assert_eq!((s.checked, s.problems), (2, 1));
        v.clear();
        assert_eq!(v.stats().checked, 0);
    }
}
