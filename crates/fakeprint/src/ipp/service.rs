//! The IPP operations.
//!
//! [`IppService::handle`] takes the body of one HTTP request and returns the body of the reply.
//! Jobs live in the same engine as WS-Print jobs (`ctx.engine`), so a job created here shows
//! in `GET /v1/jobs`, honours the same timing presets and fault plan, and can be seen over
//! either protocol.
//!
//! Strictness, in the order the checks run: the message must parse (`codec`); the version must
//! be 1.0, 1.1 or 2.0; the request id must be nonzero; the first group must be the operation
//! attributes and its first two attributes `attributes-charset` and
//! `attributes-natural-language`; the charset must be one the printer lists; the operation must
//! exist; the target (`printer-uri`, or `job-uri` / `job-id`) must name this printer; the known
//! operation attributes must have the right syntax and cardinality. Only then do the printer's
//! own limits apply (accepting jobs, document format, compression, job-template values).

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::SystemTime;

use uuid::Uuid;

use super::attrs::{self, Env};
use super::caps::Caps;
use super::codec::{self, op, status, tag, Attr, Group, Message, Value};
use super::job::IppJobData;
use crate::profile::PrinterProfile;
use crate::wsd::ctx::PrintCtx;
use crate::wsd::fault::{FaultKind, PrintFault};
use crate::wsd::jobs::{capture_meta_json, CancelSource, DocumentIn};
use crate::wsd::types::{JobState, NewJob, PrintJob};

/// The charsets the printer accepts.
const CHARSETS: [&str; 2] = ["utf-8", "us-ascii"];

/// Limits of the character-string syntaxes (RFC 8011 section 5.1).
const MAX_NAME: usize = 255;
const MAX_TEXT: usize = 1023;
const MAX_URI: usize = 1023;
const MAX_KEYWORD: usize = 255;

/// A refusal: the status, a message and the attributes for the unsupported-attributes group.
#[derive(Debug, Clone, PartialEq)]
pub struct Reject {
    pub status: u16,
    pub message: String,
    pub unsupported: Vec<Attr>,
}

impl Reject {
    pub fn new(status: u16, message: impl Into<String>) -> Reject {
        Reject {
            status,
            message: message.into(),
            unsupported: Vec::new(),
        }
    }

    fn bad(message: impl Into<String>) -> Reject {
        Reject::new(status::CLIENT_BAD_REQUEST, message)
    }

    fn with(mut self, a: Attr) -> Reject {
        self.unsupported.push(a);
        self
    }
}

/// A successful answer before it is encoded.
#[derive(Debug, Default)]
struct Reply {
    status: u16,
    message: Option<String>,
    unsupported: Vec<Attr>,
    groups: Vec<Group>,
}

/// What [`IppService::handle`] hands back.
#[derive(Debug, Clone)]
pub struct Outcome {
    pub bytes: Vec<u8>,
    /// The operation id of the request (0 when the request could not be parsed).
    pub op: u16,
    /// The status code of the reply.
    pub status: u16,
    /// The request, when it parsed (injection and validation use it).
    pub request_id: u32,
    pub version: (u8, u8),
}

/// One printer's IPP face for one request.
#[derive(Clone)]
pub struct IppService {
    pub ctx: Arc<PrintCtx>,
    pub profile: PrinterProfile,
    pub printer_uri: String,
    pub uuid: Uuid,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Syn {
    Uri,
    Name,
    Text,
    Bool,
    Mime,
    Keyword,
    Int,
    Lang,
}

/// The operation attributes this server reads: `(name, syntax, 1setOf)`.
const OP_ATTRS: [(&str, Syn, bool); 20] = [
    ("printer-uri", Syn::Uri, false),
    ("job-uri", Syn::Uri, false),
    ("job-id", Syn::Int, false),
    ("requesting-user-name", Syn::Name, false),
    ("job-name", Syn::Name, false),
    ("document-name", Syn::Name, false),
    ("ipp-attribute-fidelity", Syn::Bool, false),
    ("document-format", Syn::Mime, false),
    ("document-natural-language", Syn::Lang, false),
    ("compression", Syn::Keyword, false),
    ("last-document", Syn::Bool, false),
    ("message", Syn::Text, false),
    ("requested-attributes", Syn::Keyword, true),
    ("which-jobs", Syn::Keyword, false),
    ("my-jobs", Syn::Bool, false),
    ("limit", Syn::Int, false),
    ("job-k-octets", Syn::Int, false),
    ("job-impressions", Syn::Int, false),
    ("job-media-sheets", Syn::Int, false),
    ("status-message", Syn::Text, false),
];

/// Operation attributes each operation accepts (beyond the first two).
fn allowed(op: u16) -> &'static [&'static str] {
    match op {
        op::PRINT_JOB | op::VALIDATE_JOB => &[
            "printer-uri",
            "requesting-user-name",
            "job-name",
            "ipp-attribute-fidelity",
            "document-name",
            "document-format",
            "document-natural-language",
            "compression",
            "job-k-octets",
            "job-impressions",
            "job-media-sheets",
        ],
        op::CREATE_JOB => &[
            "printer-uri",
            "requesting-user-name",
            "job-name",
            "ipp-attribute-fidelity",
            "job-k-octets",
            "job-impressions",
            "job-media-sheets",
        ],
        op::SEND_DOCUMENT => &[
            "printer-uri",
            "job-id",
            "job-uri",
            "requesting-user-name",
            "document-name",
            "document-format",
            "document-natural-language",
            "compression",
            "last-document",
        ],
        op::CANCEL_JOB => &[
            "printer-uri",
            "job-id",
            "job-uri",
            "requesting-user-name",
            "message",
        ],
        op::GET_JOB_ATTRIBUTES => &[
            "printer-uri",
            "job-id",
            "job-uri",
            "requesting-user-name",
            "requested-attributes",
        ],
        op::GET_JOBS => &[
            "printer-uri",
            "requesting-user-name",
            "limit",
            "requested-attributes",
            "which-jobs",
            "my-jobs",
        ],
        op::GET_PRINTER_ATTRIBUTES => &[
            "printer-uri",
            "requesting-user-name",
            "requested-attributes",
            "document-format",
        ],
        _ => &[],
    }
}

fn syntax_ok(s: Syn, v: &Value) -> bool {
    match s {
        Syn::Uri => matches!(v, Value::Uri(_)),
        Syn::Name => v.is_name(),
        Syn::Text => v.is_text(),
        Syn::Bool => matches!(v, Value::Boolean(_)),
        Syn::Mime => matches!(v, Value::MimeType(_)),
        Syn::Keyword => matches!(v, Value::Keyword(_)),
        Syn::Int => matches!(v, Value::Integer(_)),
        Syn::Lang => matches!(v, Value::NaturalLanguage(_)),
    }
}

fn too_long(v: &Value) -> bool {
    let len = v.as_str().map_or(0, str::len);
    match v {
        Value::Name(_) | Value::NameWithLanguage { .. } => len > MAX_NAME,
        Value::Text(_) | Value::TextWithLanguage { .. } => len > MAX_TEXT,
        Value::Uri(_) => len > MAX_URI,
        Value::Keyword(_) | Value::MimeType(_) => len > MAX_KEYWORD,
        _ => false,
    }
}

fn str_of<'a>(g: &'a Group, name: &str) -> Option<&'a str> {
    g.get(name).and_then(Attr::first).and_then(Value::as_str)
}

fn bool_of(g: &Group, name: &str) -> Option<bool> {
    g.get(name).and_then(Attr::first).and_then(Value::as_bool)
}

fn int_of(g: &Group, name: &str) -> Option<i32> {
    g.get(name).and_then(Attr::first).and_then(Value::as_int)
}

/// `(scheme, path)` of an absolute URI; the path has query, fragment and a trailing `/` removed.
fn split_uri(u: &str) -> Option<(String, String)> {
    let (scheme, rest) = u.split_once("://")?;
    if scheme.is_empty() {
        return None;
    }
    let path_start = rest.find('/');
    let path = path_start.map_or("", |i| &rest[i..]);
    let path = path.split(['?', '#']).next().unwrap_or("");
    Some((
        scheme.to_ascii_lowercase(),
        path.trim_end_matches('/').to_string(),
    ))
}

fn fault_to_reject(f: &PrintFault) -> Reject {
    let kind = f.subcode.as_ref().and_then(FaultKind::from_subcode);
    let st = match kind {
        Some(FaultKind::ServerErrorInternalError) => status::SERVER_INTERNAL_ERROR,
        Some(FaultKind::ServerErrorTemporaryError) => status::SERVER_TEMPORARY_ERROR,
        Some(FaultKind::ServerErrorNotAcceptingJobs) if f.reason.contains("too many") => {
            status::SERVER_BUSY
        }
        Some(FaultKind::ServerErrorNotAcceptingJobs) => status::SERVER_NOT_ACCEPTING_JOBS,
        Some(FaultKind::ClientErrorJobIdNotFound) => status::CLIENT_NOT_FOUND,
        Some(FaultKind::ClientErrorMultipleDocumentsNotSupported) => {
            status::SERVER_MULTIPLE_DOCUMENT_JOBS_NOT_SUPPORTED
        }
        Some(FaultKind::ServerErrorDocumentActive) => status::SERVER_BUSY,
        Some(
            FaultKind::ClientErrorLastDocumentAlreadySent
            | FaultKind::ServerErrorJobCancelled
            | FaultKind::OperationFailed,
        ) => status::CLIENT_NOT_POSSIBLE,
        Some(FaultKind::InvalidArgs) => status::CLIENT_BAD_REQUEST,
        _ => status::SERVER_INTERNAL_ERROR,
    };
    Reject::new(st, f.reason.to_string())
}

fn wsd_compression(c: &str) -> &'static str {
    match c {
        "gzip" => "Gzip",
        "deflate" => "Deflate",
        "compress" => "Compress",
        _ => "None",
    }
}

/// What a create-style operation (Print-Job, Create-Job, Validate-Job) resolved to.
struct Submission {
    name: String,
    user: String,
    fidelity: bool,
    doc_format: String,
    compression: String,
    doc_name: Option<String>,
    requesting_user: Option<String>,
    job_name: Option<String>,
    accepted: Vec<Attr>,
    /// Unsupported attributes (job-template ones and ignored operation attributes).
    unsupported: Vec<Attr>,
}

impl IppService {
    fn language(&self) -> String {
        self.profile.identity.language.to_ascii_lowercase()
    }

    fn with_env<R>(&self, f: impl FnOnce(&Env<'_>) -> R) -> R {
        let caps = Caps::from_profile(&self.profile);
        let language = self.language();
        let env = Env {
            ctx: &self.ctx,
            profile: &self.profile,
            caps: &caps,
            printer_uri: &self.printer_uri,
            printer_uuid: self.uuid,
            now: self.ctx.clock.now(),
            started: self.ctx.ipp.started,
            language: &language,
        };
        f(&env)
    }

    /// Encode a reply message for a request.
    fn encode(&self, version: (u8, u8), request_id: u32, r: Reply) -> Vec<u8> {
        let v = if matches!(version, (1, 0) | (1, 1) | (2, 0)) {
            version
        } else {
            (2, 0)
        };
        let mut m = Message::new(v, r.status, request_id);
        let mut g = Group::new(tag::OPERATION_ATTRIBUTES);
        g.push(Attr::new(
            "attributes-charset",
            Value::Charset("utf-8".into()),
        ));
        g.push(Attr::new(
            "attributes-natural-language",
            Value::NaturalLanguage(self.language()),
        ));
        let msg = r
            .message
            .or_else(|| (r.status != status::OK).then(|| codec::status_name(r.status)));
        if let Some(t) = msg {
            g.push(Attr::new("status-message", Value::Text(t)));
        }
        m.groups.push(g);
        if !r.unsupported.is_empty() {
            m.groups.push(Group {
                tag: tag::UNSUPPORTED_ATTRIBUTES,
                attrs: r.unsupported,
            });
        }
        m.groups.extend(r.groups);
        m.encode()
    }

    /// A reply carrying only a status (what an injected `ipp_status` sends, and the answer to
    /// a message that cannot be processed).
    pub fn status_reply(
        &self,
        version: (u8, u8),
        request_id: u32,
        code: u16,
        message: Option<String>,
    ) -> Vec<u8> {
        self.encode(
            version,
            request_id,
            Reply {
                status: code,
                message,
                ..Reply::default()
            },
        )
    }

    /// Process one request body.
    pub fn handle(&self, body: &[u8]) -> Outcome {
        let msg = match Message::parse(body) {
            Ok(m) => m,
            Err(e) => {
                let version = e.version.unwrap_or((2, 0));
                let request_id = e.request_id.unwrap_or(0);
                let st = if matches!(version, (1, 0) | (1, 1) | (2, 0)) {
                    status::CLIENT_BAD_REQUEST
                } else {
                    status::SERVER_VERSION_NOT_SUPPORTED
                };
                return Outcome {
                    bytes: self.status_reply(version, request_id, st, Some(e.to_string())),
                    op: 0,
                    status: st,
                    request_id,
                    version,
                };
            }
        };
        let (version, request_id, opid) = (msg.version, msg.request_id, msg.code);
        let result = self.check_envelope(&msg).and_then(|()| self.dispatch(&msg));
        let reply = match result {
            Ok(r) => r,
            Err(rej) => Reply {
                status: rej.status,
                message: Some(rej.message),
                unsupported: rej.unsupported,
                groups: Vec::new(),
            },
        };
        let st = reply.status;
        Outcome {
            bytes: self.encode(version, request_id, reply),
            op: opid,
            status: st,
            request_id,
            version,
        }
    }

    fn check_envelope(&self, m: &Message) -> Result<(), Reject> {
        if !matches!(m.version, (1, 0) | (1, 1) | (2, 0)) {
            return Err(Reject::new(
                status::SERVER_VERSION_NOT_SUPPORTED,
                format!(
                    "IPP version {}.{} is not supported",
                    m.version.0, m.version.1
                ),
            ));
        }
        if m.request_id == 0 || m.request_id > 0x7fff_ffff {
            return Err(Reject::bad("request-id must be 1 to 2^31-1"));
        }
        let Some(first) = m.groups.first() else {
            return Err(Reject::bad("the request has no attribute groups"));
        };
        if first.tag != tag::OPERATION_ATTRIBUTES {
            return Err(Reject::bad(
                "the first attribute group must be the operation attributes",
            ));
        }
        let mut seen = BTreeSet::new();
        for g in &m.groups {
            if !seen.insert(g.tag) {
                return Err(Reject::bad(format!(
                    "attribute group {:#04x} appears twice",
                    g.tag
                )));
            }
        }
        let cs = first.attrs.first();
        match cs {
            Some(a) if a.name == "attributes-charset" => match a.values.as_slice() {
                [Value::Charset(c)] => {
                    if !CHARSETS.iter().any(|x| x.eq_ignore_ascii_case(c)) {
                        return Err(Reject::new(
                            status::CLIENT_CHARSET_NOT_SUPPORTED,
                            format!("charset {c:?} is not supported"),
                        ));
                    }
                }
                _ => return Err(Reject::bad("attributes-charset must be one charset value")),
            },
            _ => {
                return Err(Reject::bad(
                    "attributes-charset must be the first operation attribute",
                ))
            }
        }
        match first.attrs.get(1) {
            Some(a)
                if a.name == "attributes-natural-language"
                    && matches!(a.values.as_slice(), [Value::NaturalLanguage(_)]) => {}
            Some(a) if a.name == "attributes-natural-language" => {
                return Err(Reject::bad(
                    "attributes-natural-language must be one naturalLanguage value",
                ))
            }
            _ => {
                return Err(Reject::bad(
                    "attributes-natural-language must be the second operation attribute",
                ))
            }
        }
        Ok(())
    }

    fn dispatch(&self, m: &Message) -> Result<Reply, Reject> {
        match m.code {
            op::GET_PRINTER_ATTRIBUTES => self.get_printer_attributes(m),
            op::VALIDATE_JOB => self.validate_job(m),
            op::PRINT_JOB => self.print_job(m),
            op::CREATE_JOB => self.create_job(m),
            op::SEND_DOCUMENT => self.send_document(m),
            op::CANCEL_JOB => self.cancel_job(m),
            op::GET_JOB_ATTRIBUTES => self.get_job_attributes(m),
            op::GET_JOBS => self.get_jobs(m),
            other => Err(Reject::new(
                status::SERVER_OPERATION_NOT_SUPPORTED,
                format!(
                    "operation {} ({other:#06x}) is not supported",
                    codec::op_name(other).unwrap_or("unknown")
                ),
            )),
        }
    }

    // ----- targets and operation attributes ------------------------------------------------

    fn printer_path(&self) -> String {
        split_uri(&self.printer_uri)
            .map(|u| u.1)
            .unwrap_or_default()
    }

    fn check_scheme(scheme: &str, uri_attr: &Attr) -> Result<(), Reject> {
        if matches!(scheme, "ipp" | "ipps" | "http" | "https") {
            Ok(())
        } else {
            Err(Reject::new(
                status::CLIENT_URI_SCHEME_NOT_SUPPORTED,
                format!("URI scheme {scheme:?} is not supported"),
            )
            .with(uri_attr.clone()))
        }
    }

    /// `printer-uri` must name this printer: scheme `ipp`/`ipps`/`http`/`https`, any host and
    /// port, and exactly this device's path.
    fn check_printer_uri(&self, g: &Group) -> Result<(), Reject> {
        let a = g
            .get("printer-uri")
            .ok_or_else(|| Reject::bad("printer-uri is required"))?;
        let uri = a.first().and_then(Value::as_str).unwrap_or("");
        let (scheme, path) = split_uri(uri)
            .ok_or_else(|| Reject::bad(format!("printer-uri {uri:?} is not an absolute URI")))?;
        Self::check_scheme(&scheme, a)?;
        if path != self.printer_path() {
            return Err(
                Reject::new(status::CLIENT_NOT_FOUND, format!("no printer at {uri:?}"))
                    .with(a.clone()),
            );
        }
        Ok(())
    }

    /// The job a Send-Document / Cancel-Job / Get-Job-Attributes names: `job-uri`, or
    /// `printer-uri` and `job-id`.
    fn job_target(&self, g: &Group) -> Result<u32, Reject> {
        if let Some(a) = g.get("job-uri") {
            if g.get("job-id").is_some() {
                return Err(Reject::bad("job-uri and job-id are alternatives, not both"));
            }
            let uri = a.first().and_then(Value::as_str).unwrap_or("");
            let (scheme, path) = split_uri(uri)
                .ok_or_else(|| Reject::bad(format!("job-uri {uri:?} is not an absolute URI")))?;
            Self::check_scheme(&scheme, a)?;
            let base = self.printer_path();
            let id = path
                .strip_prefix(&base)
                .and_then(|r| r.strip_prefix('/'))
                .and_then(|r| r.parse::<u32>().ok())
                .filter(|i| *i >= 1);
            return id.ok_or_else(|| {
                Reject::new(status::CLIENT_NOT_FOUND, format!("no job at {uri:?}")).with(a.clone())
            });
        }
        self.check_printer_uri(g)?;
        let id =
            int_of(g, "job-id").ok_or_else(|| Reject::bad("job-id (or job-uri) is required"))?;
        u32::try_from(id)
            .ok()
            .filter(|i| *i >= 1)
            .ok_or_else(|| Reject::bad("job-id must be 1 or more"))
    }

    /// Check the known operation attributes of `m` against the operation's list: syntax,
    /// cardinality and length. Returns the attributes that are not part of this operation
    /// (they are ignored and reported as unsupported).
    fn check_op_attrs(&self, m: &Message) -> Result<Vec<Attr>, Reject> {
        let g = m
            .group(tag::OPERATION_ATTRIBUTES)
            .ok_or_else(|| Reject::bad("no operation attributes"))?;
        let ok = allowed(m.code);
        let mut ignored = Vec::new();
        for a in g.attrs.iter().skip(2) {
            let known = OP_ATTRS.iter().find(|k| k.0 == a.name);
            let Some(&(_, syn, multi)) = known.filter(|_| ok.contains(&a.name.as_str())) else {
                ignored.push(a.clone());
                continue;
            };
            if !multi && a.values.len() != 1 {
                return Err(Reject::bad(format!(
                    "{} must have exactly one value",
                    a.name
                )));
            }
            for v in &a.values {
                if !syntax_ok(syn, v) {
                    return Err(Reject::bad(format!(
                        "{} has a value of the wrong syntax",
                        a.name
                    )));
                }
                if too_long(v) {
                    return Err(Reject::new(
                        status::CLIENT_REQUEST_VALUE_TOO_LONG,
                        format!("a value of {} is too long", a.name),
                    )
                    .with(a.clone()));
                }
            }
        }
        if let Some(l) = int_of(g, "limit") {
            if l < 1 {
                return Err(Reject::bad("limit must be 1 or more"));
            }
        }
        Ok(ignored)
    }

    fn requested(g: &Group) -> Option<Vec<String>> {
        g.get("requested-attributes").map(|a| {
            a.values
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
    }

    // ----- printer operations --------------------------------------------------------------

    fn get_printer_attributes(&self, m: &Message) -> Result<Reply, Reject> {
        let ignored = self.check_op_attrs(m)?;
        let g = m.group(tag::OPERATION_ATTRIBUTES).expect("checked");
        self.check_printer_uri(g)?;
        let req = Self::requested(g);
        let sel = self.with_env(|env| {
            let (d, t) = attrs::printer_attributes(env);
            attrs::select(req.as_deref(), None, d, t, &attrs::printer_known_names(env))
        });
        let mut unsupported = ignored;
        if !sel.unknown.is_empty() {
            unsupported.push(Attr::keywords("requested-attributes", &sel.unknown));
        }
        let mut pg = Group::new(tag::PRINTER_ATTRIBUTES);
        pg.attrs = sel.attrs;
        Ok(Reply {
            status: if unsupported.is_empty() {
                status::OK
            } else {
                status::OK_IGNORED_OR_SUBSTITUTED
            },
            unsupported,
            groups: vec![pg],
            ..Reply::default()
        })
    }

    // ----- job submission ------------------------------------------------------------------

    /// The checks Print-Job, Create-Job and Validate-Job share. Nothing is created.
    fn prepare(&self, m: &Message) -> Result<Submission, Reject> {
        let ignored = self.check_op_attrs(m)?;
        let g = m.group(tag::OPERATION_ATTRIBUTES).expect("checked");
        self.check_printer_uri(g)?;
        // the printer's own state: faults first, then not-accepting
        self.ctx
            .engine
            .check_service_faults()
            .map_err(|f| fault_to_reject(&f))?;
        if !self.ctx.engine.status_accepting() {
            return Err(Reject::new(
                status::SERVER_NOT_ACCEPTING_JOBS,
                "the printer is not accepting jobs",
            ));
        }
        let caps = Caps::from_profile(&self.profile);
        let fidelity = bool_of(g, "ipp-attribute-fidelity").unwrap_or(false);
        let doc_format = str_of(g, "document-format")
            .map(str::to_string)
            .unwrap_or_else(|| caps.default_format.clone());
        let compression = str_of(g, "compression").unwrap_or("none").to_string();
        let mut unsupported = ignored;
        // operation-level refusals apply whatever the fidelity says
        if let Some(a) = g.get("document-format") {
            if !caps.format_ok(&doc_format) {
                let mut rej = Reject::new(
                    status::CLIENT_DOCUMENT_FORMAT_NOT_SUPPORTED,
                    format!("document-format {doc_format:?} is not supported"),
                )
                .with(a.clone());
                rej.unsupported.extend(unsupported);
                return Err(rej);
            }
        }
        if let Some(a) = g.get("compression") {
            if !caps.compressions.contains(&compression) {
                let mut rej = Reject::new(
                    status::CLIENT_COMPRESSION_NOT_SUPPORTED,
                    format!("compression {compression:?} is not supported"),
                )
                .with(a.clone());
                rej.unsupported.extend(unsupported);
                return Err(rej);
            }
        }
        let mut accepted = Vec::new();
        let mut template_bad = Vec::new();
        if let Some(jg) = m.group(tag::JOB_ATTRIBUTES) {
            for a in &jg.attrs {
                match attrs::check_template_attr(&caps, a) {
                    None => accepted.push(a.clone()),
                    Some(bad) => template_bad.push(bad),
                }
            }
        }
        unsupported.extend(template_bad.iter().cloned());
        if fidelity && !template_bad.is_empty() {
            return Err(Reject {
                status: status::CLIENT_ATTRIBUTES_OR_VALUES_NOT_SUPPORTED,
                message: "ipp-attribute-fidelity is true and attributes are not supported".into(),
                unsupported,
            });
        }
        let requesting_user = str_of(g, "requesting-user-name").map(str::to_string);
        let job_name = str_of(g, "job-name").map(str::to_string);
        let doc_name = str_of(g, "document-name").map(str::to_string);
        Ok(Submission {
            name: job_name
                .clone()
                .or_else(|| doc_name.clone())
                .unwrap_or_else(|| "Untitled".into()),
            user: requesting_user
                .clone()
                .unwrap_or_else(|| "anonymous".into()),
            fidelity,
            doc_format,
            compression,
            doc_name,
            requesting_user,
            job_name,
            accepted,
            unsupported,
        })
    }

    fn ok_or_substituted(unsupported: &[Attr]) -> u16 {
        if unsupported.is_empty() {
            status::OK
        } else {
            status::OK_IGNORED_OR_SUBSTITUTED
        }
    }

    fn validate_job(&self, m: &Message) -> Result<Reply, Reject> {
        let s = self.prepare(m)?;
        Ok(Reply {
            status: Self::ok_or_substituted(&s.unsupported),
            unsupported: s.unsupported,
            ..Reply::default()
        })
    }

    fn new_job(&self, m: &Message, s: &Submission) -> Result<u32, Reject> {
        let caps = Caps::from_profile(&self.profile);
        let ticket = attrs::ticket_for(&caps, &s.accepted, &s.name, &s.user);
        let data = IppJobData {
            version: m.version,
            attrs: s.accepted.clone(),
            fidelity: s.fidelity,
            job_name: s.job_name.clone(),
            requesting_user: s.requesting_user.clone(),
        };
        let timeout = self.ctx.timing().ipp_multiple_operation_timeout;
        self.ctx
            .engine
            .create_job(
                NewJob {
                    name: s.name.clone(),
                    user: s.user.clone(),
                    ticket: Some(ticket.to_element()),
                    ipp: Some(Arc::new(data.clone())),
                    document_timeout: Some(timeout),
                },
                data.copies(),
            )
            .map_err(|f| fault_to_reject(&f))
    }

    /// The group a job-creating reply carries.
    fn job_reply_group(&self, job: &PrintJob) -> Group {
        let (state, reasons) = attrs::job_state_view(job);
        let mut g = Group::new(tag::JOB_ATTRIBUTES);
        g.push(Attr::int(
            "job-id",
            i32::try_from(job.id).unwrap_or(i32::MAX),
        ));
        g.push(Attr::new(
            "job-uri",
            Value::uri(attrs::job_uri(&self.printer_uri, job.id)),
        ));
        g.push(Attr::new(
            "job-uuid",
            Value::uri(format!("urn:uuid:{}", attrs::job_uuid(self.uuid, job.id))),
        ));
        g.push(Attr::enum_("job-state", state));
        g.push(Attr::keywords("job-state-reasons", &reasons));
        g
    }

    fn current(&self, id: u32) -> Result<PrintJob, Reject> {
        self.ctx
            .jobs
            .get(id)
            .ok_or_else(|| Reject::new(status::CLIENT_NOT_FOUND, format!("job {id} not found")))
    }

    fn print_job(&self, m: &Message) -> Result<Reply, Reject> {
        let s = self.prepare(m)?;
        if m.data.is_empty() {
            return Err(Reject::bad("Print-Job without document data"));
        }
        if m.data.len() > self.ctx.config.limits.max_document {
            return Err(Reject::new(
                status::CLIENT_REQUEST_ENTITY_TOO_LARGE,
                "the document is larger than the printer accepts",
            ));
        }
        let id = self.new_job(m, &s)?;
        if let Err(rej) = self.store_document(
            id,
            &m.data,
            &s.doc_format,
            &s.compression,
            s.doc_name.clone(),
            true,
        ) {
            let _ = self.ctx.engine.cancel(id, CancelSource::Device);
            if let Some(i) = &self.ctx.inbox {
                i.discard_job(id);
            }
            return Err(rej);
        }
        let job = self.current(id)?;
        Ok(Reply {
            status: Self::ok_or_substituted(&s.unsupported),
            unsupported: s.unsupported,
            groups: vec![self.job_reply_group(&job)],
            ..Reply::default()
        })
    }

    fn create_job(&self, m: &Message) -> Result<Reply, Reject> {
        let s = self.prepare(m)?;
        let id = self.new_job(m, &s)?;
        let job = self.current(id)?;
        Ok(Reply {
            status: Self::ok_or_substituted(&s.unsupported),
            unsupported: s.unsupported,
            groups: vec![self.job_reply_group(&job)],
            ..Reply::default()
        })
    }

    /// Write the document to the inbox and hand it to the engine.
    fn store_document(
        &self,
        id: u32,
        data: &[u8],
        format: &str,
        compression: &str,
        name: Option<String>,
        last: bool,
    ) -> Result<(), Reject> {
        let multi = self.ctx.config.multiple_document_jobs;
        self.ctx
            .engine
            .precheck_document(id, last, multi)
            .map_err(|f| fault_to_reject(&f))?;
        let job = self.current(id)?;
        let wsd_comp = wsd_compression(compression);
        let doc_id = job.number_of_documents().saturating_add(1);
        let mut capture_path = None;
        if let Some(inbox) = &self.ctx.inbox {
            let extra = capture_meta_json(&job, doc_id, format, wsd_comp);
            let cap = inbox
                .capture_with(id, doc_id, data, format, "ipp", &extra)
                .map_err(|e| {
                    Reject::new(
                        status::SERVER_INTERNAL_ERROR,
                        format!("capture failed: {e:#}"),
                    )
                })?;
            capture_path = Some(cap);
        }
        let doc = DocumentIn {
            name,
            format: format.to_string(),
            compression: wsd_comp.to_string(),
            size: data.len() as u64,
            capture_path: capture_path
                .as_ref()
                .map(|c| std::path::PathBuf::from(&c.path)),
            last,
            ..DocumentIn::default()
        };
        match self.ctx.engine.commit_document(id, doc, multi) {
            Ok(_) => Ok(()),
            Err(f) => {
                if let Some(c) = &capture_path {
                    let _ = std::fs::remove_file(&c.path);
                    let _ = std::fs::remove_file(&c.meta_path);
                }
                Err(fault_to_reject(&f))
            }
        }
    }

    fn send_document(&self, m: &Message) -> Result<Reply, Reject> {
        let ignored = self.check_op_attrs(m)?;
        let g = m.group(tag::OPERATION_ATTRIBUTES).expect("checked");
        let id = self.job_target(g)?;
        let last = bool_of(g, "last-document")
            .ok_or_else(|| Reject::bad("last-document is required in Send-Document"))?;
        self.ctx
            .engine
            .check_service_faults()
            .map_err(|f| fault_to_reject(&f))?;
        let job = self.visible_job(id)?;
        if job.state.is_terminal() || job.state == JobState::Terminating {
            return Err(Reject::new(
                status::CLIENT_NOT_POSSIBLE,
                format!("job {id} has ended or is being cancelled"),
            ));
        }
        let caps = Caps::from_profile(&self.profile);
        let unsupported = ignored;
        let format = str_of(g, "document-format")
            .map(str::to_string)
            .unwrap_or_else(|| caps.default_format.clone());
        if let Some(a) = g.get("document-format") {
            if !caps.format_ok(&format) {
                return Err(Reject::new(
                    status::CLIENT_DOCUMENT_FORMAT_NOT_SUPPORTED,
                    format!("document-format {format:?} is not supported"),
                )
                .with(a.clone()));
            }
        }
        let compression = str_of(g, "compression").unwrap_or("none").to_string();
        if let Some(a) = g.get("compression") {
            if !caps.compressions.contains(&compression) {
                return Err(Reject::new(
                    status::CLIENT_COMPRESSION_NOT_SUPPORTED,
                    format!("compression {compression:?} is not supported"),
                )
                .with(a.clone()));
            }
        }
        if m.data.len() > self.ctx.config.limits.max_document {
            return Err(Reject::new(
                status::CLIENT_REQUEST_ENTITY_TOO_LARGE,
                "the document is larger than the printer accepts",
            ));
        }
        if m.data.is_empty() {
            if !last {
                return Err(Reject::bad(
                    "Send-Document without data must be the last document",
                ));
            }
            self.ctx
                .engine
                .close_job(id)
                .map_err(|f| fault_to_reject(&f))?;
        } else {
            self.store_document(
                id,
                &m.data,
                &format,
                &compression,
                str_of(g, "document-name").map(str::to_string),
                last,
            )?;
        }
        let job = self.current(id)?;
        Ok(Reply {
            status: Self::ok_or_substituted(&unsupported),
            unsupported,
            groups: vec![self.job_reply_group(&job)],
            ..Reply::default()
        })
    }

    // ----- job queries ---------------------------------------------------------------------

    /// Whether an IPP client can still see the job: running jobs always, finished ones for
    /// the configured retention.
    fn retained(&self, job: &PrintJob, now: SystemTime) -> bool {
        match (job.state.is_terminal(), job.ended) {
            (true, Some(ended)) => {
                let keep = self.ctx.timing().ipp_history_retention;
                now.duration_since(ended).unwrap_or_default() < keep
            }
            _ => true,
        }
    }

    fn visible_job(&self, id: u32) -> Result<PrintJob, Reject> {
        let job = self.current(id)?;
        if self.retained(&job, self.ctx.clock.now()) {
            Ok(job)
        } else {
            Err(Reject::new(
                status::CLIENT_NOT_FOUND,
                format!("job {id} is no longer remembered"),
            ))
        }
    }

    fn job_group(
        &self,
        env: &Env<'_>,
        job: &PrintJob,
        sel: Option<&[String]>,
    ) -> (Group, Vec<String>) {
        let (d, t) = attrs::job_attributes(env, job);
        let s = attrs::select(
            sel,
            if sel.is_none() {
                Some(&["job-id", "job-uri"])
            } else {
                None
            },
            d,
            t,
            &attrs::job_known_names(),
        );
        let mut g = Group::new(tag::JOB_ATTRIBUTES);
        g.attrs = s.attrs;
        (g, s.unknown)
    }

    fn get_job_attributes(&self, m: &Message) -> Result<Reply, Reject> {
        let ignored = self.check_op_attrs(m)?;
        let g = m.group(tag::OPERATION_ATTRIBUTES).expect("checked");
        let id = self.job_target(g)?;
        let job = self.visible_job(id)?;
        let req = Self::requested(g);
        let (jg, unknown) = self.with_env(|env| {
            let (d, t) = attrs::job_attributes(env, &job);
            let s = attrs::select(req.as_deref(), None, d, t, &attrs::job_known_names());
            let mut g = Group::new(tag::JOB_ATTRIBUTES);
            g.attrs = s.attrs;
            (g, s.unknown)
        });
        let mut unsupported = ignored;
        if !unknown.is_empty() {
            unsupported.push(Attr::keywords("requested-attributes", &unknown));
        }
        Ok(Reply {
            status: Self::ok_or_substituted(&unsupported),
            unsupported,
            groups: vec![jg],
            ..Reply::default()
        })
    }

    fn get_jobs(&self, m: &Message) -> Result<Reply, Reject> {
        let ignored = self.check_op_attrs(m)?;
        let g = m.group(tag::OPERATION_ATTRIBUTES).expect("checked");
        self.check_printer_uri(g)?;
        let which = str_of(g, "which-jobs").unwrap_or("not-completed");
        if which != "completed" && which != "not-completed" {
            return Err(Reject::new(
                status::CLIENT_ATTRIBUTES_OR_VALUES_NOT_SUPPORTED,
                format!("which-jobs {which:?} is not supported"),
            )
            .with(
                g.get("which-jobs")
                    .cloned()
                    .unwrap_or_else(|| Attr::unsupported("which-jobs")),
            ));
        }
        let my_jobs = bool_of(g, "my-jobs").unwrap_or(false);
        let user = str_of(g, "requesting-user-name");
        if my_jobs && user.is_none() {
            return Err(Reject::bad("my-jobs needs requesting-user-name"));
        }
        let limit = int_of(g, "limit").and_then(|l| usize::try_from(l).ok());
        let now = self.ctx.clock.now();
        let completed = which == "completed";
        let mut jobs: Vec<PrintJob> = if completed {
            self.ctx.jobs.history()
        } else {
            self.ctx.jobs.active()
        };
        jobs.retain(|j| self.retained(j, now));
        jobs.retain(|j| {
            !my_jobs || user.is_some_and(|u| j.user == u || (j.user.is_empty() && u == "anonymous"))
        });
        if completed {
            // most recently finished first
            jobs.sort_by(|a, b| b.ended.cmp(&a.ended).then(b.id.cmp(&a.id)));
        } else {
            jobs.sort_by_key(|j| j.id);
        }
        if let Some(l) = limit {
            jobs.truncate(l);
        }
        let req = Self::requested(g);
        let mut unknown: Vec<String> = Vec::new();
        let groups = self.with_env(|env| {
            jobs.iter()
                .map(|j| {
                    let (g, u) = self.job_group(env, j, req.as_deref());
                    for n in u {
                        if !unknown.contains(&n) {
                            unknown.push(n);
                        }
                    }
                    g
                })
                .collect::<Vec<_>>()
        });
        if jobs.is_empty() {
            // unknown names are still reported on an empty list
            if let Some(r) = &req {
                let known = attrs::job_known_names();
                let groupish = |n: &str| {
                    matches!(
                        n,
                        "all"
                            | "job-template"
                            | "job-description"
                            | "printer-description"
                            | "document-description"
                            | "document-template"
                            | "subscription-description"
                            | "subscription-template"
                            | "event-notification-attributes"
                    )
                };
                for n in r {
                    if !known.contains(n) && !groupish(n) && !unknown.contains(n) {
                        unknown.push(n.clone());
                    }
                }
            }
        }
        let mut unsupported = ignored;
        if !unknown.is_empty() {
            unsupported.push(Attr::keywords("requested-attributes", &unknown));
        }
        Ok(Reply {
            status: Self::ok_or_substituted(&unsupported),
            unsupported,
            groups,
            ..Reply::default()
        })
    }

    fn cancel_job(&self, m: &Message) -> Result<Reply, Reject> {
        let ignored = self.check_op_attrs(m)?;
        let g = m.group(tag::OPERATION_ATTRIBUTES).expect("checked");
        let id = self.job_target(g)?;
        let job = self.visible_job(id)?;
        if job.state.is_terminal() {
            return Err(Reject::new(
                status::CLIENT_NOT_POSSIBLE,
                format!("job {id} has already ended ({})", job.state.token()),
            ));
        }
        if job.state == JobState::Terminating {
            return Err(Reject::new(
                status::CLIENT_NOT_POSSIBLE,
                format!("job {id} is already being cancelled"),
            ));
        }
        self.ctx
            .engine
            .cancel(id, CancelSource::User)
            .map_err(|f| fault_to_reject(&f))?;
        if let Some(i) = &self.ctx.inbox {
            i.discard_job(id);
        }
        Ok(Reply {
            status: Self::ok_or_substituted(&ignored),
            unsupported: ignored,
            ..Reply::default()
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::wsd::ctx::{ManualClock, PrintConfig, PrintTiming};
    use crate::wsd::printer_model::ProfilePrinterModel;
    use std::time::Duration;

    pub struct Rig {
        pub svc: IppService,
        pub clock: Arc<ManualClock>,
        pub dir: std::path::PathBuf,
    }

    impl Drop for Rig {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    pub const URI: &str = "ipp://127.0.0.1:3921/ipp/print";

    pub fn rig_with(timing: PrintTiming, mut profile: PrinterProfile) -> Rig {
        let clock = Arc::new(ManualClock::new());
        let dir = std::env::temp_dir().join(format!("fakeprint-ipp-{}", Uuid::new_v4()));
        profile.documents.formats = vec![
            "application/octet-stream".into(),
            "application/pdf".into(),
            "image/jpeg".into(),
            "image/pwg-raster".into(),
        ];
        let cfg = PrintConfig {
            multiple_document_jobs: profile.description.multiple_document_jobs,
            timing,
            ..PrintConfig::default()
        };
        let p2 = profile.clone();
        let ctx = PrintCtx::build(cfg, clock.clone(), move |status| {
            Arc::new(ProfilePrinterModel::with_status(p2, status))
        });
        let inbox = Arc::new(crate::state::Inbox::new(dir.join("inbox")).unwrap());
        let ctx = ctx.with_inbox(inbox);
        Rig {
            svc: IppService {
                ctx: Arc::new(ctx),
                profile,
                printer_uri: URI.into(),
                uuid: Uuid::parse_str("00000000-0000-4000-8000-0000000000f4").unwrap(),
            },
            clock,
            dir,
        }
    }

    pub fn rig() -> Rig {
        rig_with(PrintTiming::instant(), PrinterProfile::generic())
    }

    pub fn request(opcode: u16, op_attrs: Vec<Attr>, job_attrs: Vec<Attr>, data: &[u8]) -> Vec<u8> {
        let mut m = Message::new((2, 0), opcode, 1);
        let mut g = Group::new(tag::OPERATION_ATTRIBUTES);
        g.push(Attr::new(
            "attributes-charset",
            Value::Charset("utf-8".into()),
        ));
        g.push(Attr::new(
            "attributes-natural-language",
            Value::NaturalLanguage("en".into()),
        ));
        g.attrs.extend(op_attrs);
        m.groups.push(g);
        if !job_attrs.is_empty() {
            let mut j = Group::new(tag::JOB_ATTRIBUTES);
            j.attrs = job_attrs;
            m.groups.push(j);
        }
        m.data = data.to_vec();
        m.encode()
    }

    pub fn printer_uri() -> Attr {
        Attr::new("printer-uri", Value::uri(URI))
    }

    pub fn call(r: &Rig, bytes: &[u8]) -> Message {
        let out = r.svc.handle(bytes);
        let m = Message::parse(&out.bytes).expect("reply parses");
        assert_eq!(m.code, out.status);
        m
    }

    pub fn job_id(m: &Message) -> i32 {
        m.group(tag::JOB_ATTRIBUTES)
            .and_then(|g| g.get("job-id"))
            .and_then(Attr::first)
            .and_then(Value::as_int)
            .expect("job-id")
    }

    fn print(r: &Rig, extra_job: Vec<Attr>) -> Message {
        call(
            r,
            &request(
                op::PRINT_JOB,
                vec![
                    printer_uri(),
                    Attr::new("document-format", Value::mime("application/pdf")),
                ],
                extra_job,
                b"%PDF-1.4 fake",
            ),
        )
    }

    #[test]
    fn print_job_creates_a_shared_engine_job_and_completes_on_the_next_tick() {
        let r = rig();
        let m = print(&r, vec![Attr::int("copies", 2)]);
        assert_eq!(m.code, status::OK);
        let id = job_id(&m);
        let j = r.svc.ctx.jobs.get(u32::try_from(id).unwrap()).unwrap();
        assert_eq!(j.documents.len(), 1);
        assert!(j.ipp.is_some() && j.ticket.is_some());
        r.clock.advance(Duration::from_millis(10));
        let m = call(
            &r,
            &request(
                op::GET_JOB_ATTRIBUTES,
                vec![printer_uri(), Attr::int("job-id", id)],
                vec![],
                b"",
            ),
        );
        assert_eq!(m.code, status::OK);
        let g = m.group(tag::JOB_ATTRIBUTES).unwrap();
        assert_eq!(int_of(g, "job-state"), Some(9));
        assert_eq!(int_of(g, "number-of-documents"), Some(1));
        assert_eq!(int_of(g, "copies"), Some(2));
    }

    #[test]
    fn envelope_violations_are_bad_requests() {
        let r = rig();
        let bad = |bytes: Vec<u8>, want: u16| {
            let m = call(&r, &bytes);
            assert_eq!(m.code, want, "{}", codec::status_name(m.code));
        };
        // request-id 0
        let mut b = request(op::GET_PRINTER_ATTRIBUTES, vec![printer_uri()], vec![], b"");
        b[4..8].copy_from_slice(&0u32.to_be_bytes());
        bad(b, status::CLIENT_BAD_REQUEST);
        // wrong version
        let mut b = request(op::GET_PRINTER_ATTRIBUTES, vec![printer_uri()], vec![], b"");
        b[0] = 3;
        bad(b, status::SERVER_VERSION_NOT_SUPPORTED);
        // charset not first
        let mut m = Message::new((2, 0), op::GET_PRINTER_ATTRIBUTES, 5);
        let mut g = Group::new(tag::OPERATION_ATTRIBUTES);
        g.push(printer_uri());
        g.push(Attr::new(
            "attributes-charset",
            Value::Charset("utf-8".into()),
        ));
        g.push(Attr::new(
            "attributes-natural-language",
            Value::NaturalLanguage("en".into()),
        ));
        m.groups.push(g);
        bad(m.encode(), status::CLIENT_BAD_REQUEST);
        // unsupported charset
        let mut m = Message::new((2, 0), op::GET_PRINTER_ATTRIBUTES, 5);
        let mut g = Group::new(tag::OPERATION_ATTRIBUTES);
        g.push(Attr::new(
            "attributes-charset",
            Value::Charset("utf-16".into()),
        ));
        g.push(Attr::new(
            "attributes-natural-language",
            Value::NaturalLanguage("en".into()),
        ));
        g.push(printer_uri());
        m.groups.push(g);
        bad(m.encode(), status::CLIENT_CHARSET_NOT_SUPPORTED);
        // job group first
        let mut m = Message::new((2, 0), op::PRINT_JOB, 5);
        m.groups.push(Group::new(tag::JOB_ATTRIBUTES));
        bad(m.encode(), status::CLIENT_BAD_REQUEST);
        // garbage
        bad(
            vec![2, 0, 0, 0x0b, 0, 0, 0, 1, 0x21],
            status::CLIENT_BAD_REQUEST,
        );
        // unknown operation
        bad(
            request(0x4001, vec![printer_uri()], vec![], b""),
            status::SERVER_OPERATION_NOT_SUPPORTED,
        );
        bad(
            request(op::PRINT_URI, vec![printer_uri()], vec![], b""),
            status::SERVER_OPERATION_NOT_SUPPORTED,
        );
    }

    #[test]
    fn the_reply_echoes_version_and_request_id_and_leads_with_charset_and_language() {
        let r = rig();
        let mut b = request(op::GET_PRINTER_ATTRIBUTES, vec![printer_uri()], vec![], b"");
        b[0] = 1;
        b[1] = 1;
        b[4..8].copy_from_slice(&77u32.to_be_bytes());
        let m = call(&r, &b);
        assert_eq!((m.version, m.request_id), ((1, 1), 77));
        let g = &m.groups[0];
        assert_eq!(g.tag, tag::OPERATION_ATTRIBUTES);
        assert_eq!(g.attrs[0].name, "attributes-charset");
        assert_eq!(g.attrs[1].name, "attributes-natural-language");
    }

    #[test]
    fn printer_uri_must_name_this_printer() {
        let r = rig();
        let ask = |uri: &str| {
            call(
                &r,
                &request(
                    op::GET_PRINTER_ATTRIBUTES,
                    vec![Attr::new("printer-uri", Value::uri(uri))],
                    vec![],
                    b"",
                ),
            )
            .code
        };
        assert_eq!(ask("ipp://other-host:631/ipp/print"), status::OK);
        assert_eq!(ask("http://127.0.0.1:3921/ipp/print/"), status::OK);
        assert_eq!(
            ask("ipp://127.0.0.1:3921/d/x/ipp/print"),
            status::CLIENT_NOT_FOUND
        );
        assert_eq!(
            ask("ftp://127.0.0.1/ipp/print"),
            status::CLIENT_URI_SCHEME_NOT_SUPPORTED
        );
        assert_eq!(ask("not a uri"), status::CLIENT_BAD_REQUEST);
        // missing printer-uri
        let m = call(
            &r,
            &request(op::GET_PRINTER_ATTRIBUTES, vec![], vec![], b""),
        );
        assert_eq!(m.code, status::CLIENT_BAD_REQUEST);
    }

    #[test]
    fn operation_attribute_syntax_and_cardinality() {
        let r = rig();
        let code = |extra: Attr| {
            call(
                &r,
                &request(op::GET_JOBS, vec![printer_uri(), extra], vec![], b""),
            )
            .code
        };
        assert_eq!(code(Attr::int("limit", 0)), status::CLIENT_BAD_REQUEST);
        assert_eq!(
            code(Attr::new("limit", Value::keyword("x"))),
            status::CLIENT_BAD_REQUEST
        );
        assert_eq!(
            code(Attr::many(
                "which-jobs",
                vec![Value::keyword("a"), Value::keyword("b")]
            )),
            status::CLIENT_BAD_REQUEST
        );
        assert_eq!(
            code(Attr::new("my-jobs", Value::Boolean(true))),
            status::CLIENT_BAD_REQUEST,
            "my-jobs without requesting-user-name"
        );
        assert_eq!(
            code(Attr::new("which-jobs", Value::keyword("bogus"))),
            status::CLIENT_ATTRIBUTES_OR_VALUES_NOT_SUPPORTED
        );
        assert_eq!(
            code(Attr::new("job-name", Value::name("x"))),
            status::OK_IGNORED_OR_SUBSTITUTED,
            "an operation attribute of another operation is ignored and reported"
        );
        assert_eq!(
            code(Attr::new(
                "requesting-user-name",
                Value::name("n".repeat(300))
            )),
            status::CLIENT_REQUEST_VALUE_TOO_LONG
        );
    }

    #[test]
    fn get_printer_attributes_filters_and_reports_unknown_names() {
        let r = rig();
        let ask = |names: &[&str]| {
            call(
                &r,
                &request(
                    op::GET_PRINTER_ATTRIBUTES,
                    vec![printer_uri(), Attr::keywords("requested-attributes", names)],
                    vec![],
                    b"",
                ),
            )
        };
        let m = ask(&["printer-state", "queued-job-count"]);
        let pg = m.group(tag::PRINTER_ATTRIBUTES).unwrap();
        assert_eq!(pg.attrs.len(), 2);
        let m = ask(&["job-template"]);
        let pg = m.group(tag::PRINTER_ATTRIBUTES).unwrap();
        assert!(pg.get("sides-supported").is_some() && pg.get("printer-state").is_none());
        let m = ask(&["printer-description"]);
        let pg = m.group(tag::PRINTER_ATTRIBUTES).unwrap();
        assert!(pg.get("printer-state").is_some() && pg.get("copies-default").is_none());
        let m = ask(&["printer-state", "no-such-attribute"]);
        assert_eq!(m.code, status::OK_IGNORED_OR_SUBSTITUTED);
        let ug = m.group(tag::UNSUPPORTED_ATTRIBUTES).unwrap();
        assert_eq!(ug.attrs[0].name, "requested-attributes");
        assert_eq!(ug.attrs[0].values, [Value::keyword("no-such-attribute")]);
        // group order: operation, unsupported, printer
        assert_eq!(
            m.groups.iter().map(|g| g.tag).collect::<Vec<_>>(),
            [1, 5, 4]
        );
        // all (default)
        let m = call(
            &r,
            &request(op::GET_PRINTER_ATTRIBUTES, vec![printer_uri()], vec![], b""),
        );
        let pg = m.group(tag::PRINTER_ATTRIBUTES).unwrap();
        for n in [
            "printer-uri-supported",
            "operations-supported",
            "document-format-supported",
            "multiple-operation-time-out",
            "job-creation-attributes-supported",
            "ipp-features-supported",
            "printer-up-time",
            "printer-current-time",
        ] {
            assert!(pg.get(n).is_some(), "{n}");
        }
    }

    #[test]
    fn job_template_values_outside_the_lists_follow_the_fidelity_rule() {
        let r = rig();
        let bad = vec![
            Attr::int("copies", 1000),
            Attr::new("sides", Value::keyword("two-sided-long-edge")),
            Attr::int("x-bogus", 1),
        ];
        // fidelity false: accepted, status ok-ignored, offenders listed, bogus is out-of-band
        let m = call(
            &r,
            &request(
                op::PRINT_JOB,
                vec![printer_uri()],
                [vec![Attr::int("job-priority", 7)], bad.clone()].concat(),
                b"%PDF",
            ),
        );
        assert_eq!(m.code, status::OK_IGNORED_OR_SUBSTITUTED);
        let ug = m.group(tag::UNSUPPORTED_ATTRIBUTES).unwrap();
        let names: Vec<&str> = ug.attrs.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["copies", "sides", "x-bogus"]);
        assert_eq!(ug.attrs[0].values, [Value::Integer(1000)]);
        assert_eq!(ug.attrs[2].values, [Value::OutOfBand(tag::UNSUPPORTED)]);
        let id = u32::try_from(job_id(&m)).unwrap();
        let ipp = r.svc.ctx.jobs.get(id).unwrap().ipp.unwrap();
        assert_eq!(ipp.attrs.len(), 1, "only the accepted attribute is kept");
        // fidelity true: refused, nothing created
        let before = r.svc.ctx.jobs.active().len() + r.svc.ctx.jobs.history().len();
        let m = call(
            &r,
            &request(
                op::PRINT_JOB,
                vec![printer_uri(), Attr::boolean("ipp-attribute-fidelity", true)],
                bad,
                b"%PDF",
            ),
        );
        assert_eq!(m.code, status::CLIENT_ATTRIBUTES_OR_VALUES_NOT_SUPPORTED);
        assert_eq!(m.group(tag::UNSUPPORTED_ATTRIBUTES).unwrap().attrs.len(), 3);
        assert!(m.group(tag::JOB_ATTRIBUTES).is_none());
        assert_eq!(
            r.svc.ctx.jobs.active().len() + r.svc.ctx.jobs.history().len(),
            before
        );
        // Validate-Job behaves the same without creating anything
        let m = call(
            &r,
            &request(
                op::VALIDATE_JOB,
                vec![printer_uri(), Attr::boolean("ipp-attribute-fidelity", true)],
                vec![Attr::int("copies", 5)],
                b"",
            ),
        );
        assert_eq!(m.code, status::OK);
    }

    #[test]
    fn format_and_compression_refusals_ignore_fidelity() {
        let r = rig();
        let m = call(
            &r,
            &request(
                op::PRINT_JOB,
                vec![
                    printer_uri(),
                    Attr::new("document-format", Value::mime("image/png")),
                ],
                vec![],
                b"x",
            ),
        );
        assert_eq!(m.code, status::CLIENT_DOCUMENT_FORMAT_NOT_SUPPORTED);
        assert_eq!(
            m.group(tag::UNSUPPORTED_ATTRIBUTES).unwrap().attrs[0].name,
            "document-format"
        );
        let m = call(
            &r,
            &request(
                op::PRINT_JOB,
                vec![
                    printer_uri(),
                    Attr::new("compression", Value::keyword("gzip")),
                ],
                vec![],
                b"x",
            ),
        );
        assert_eq!(m.code, status::CLIENT_COMPRESSION_NOT_SUPPORTED);
        // no data, too much data
        let m = call(
            &r,
            &request(op::PRINT_JOB, vec![printer_uri()], vec![], b""),
        );
        assert_eq!(m.code, status::CLIENT_BAD_REQUEST);
    }

    #[test]
    fn create_job_then_send_document_with_last_document_and_the_timeout() {
        let r = rig();
        let m = call(
            &r,
            &request(
                op::CREATE_JOB,
                vec![
                    printer_uri(),
                    Attr::new("job-name", Value::name("two parts")),
                ],
                vec![],
                b"",
            ),
        );
        assert_eq!(m.code, status::OK);
        let id = job_id(&m);
        let g = m.group(tag::JOB_ATTRIBUTES).unwrap();
        assert_eq!(int_of(g, "job-state"), Some(3));
        let reasons: Vec<_> = g
            .get("job-state-reasons")
            .unwrap()
            .values
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert_eq!(reasons, ["job-incoming", "job-data-insufficient"]);
        let send = |last: Option<bool>, data: &[u8]| {
            let mut attrs = vec![printer_uri(), Attr::int("job-id", id)];
            if let Some(l) = last {
                attrs.push(Attr::boolean("last-document", l));
            }
            call(&r, &request(op::SEND_DOCUMENT, attrs, vec![], data))
        };
        assert_eq!(send(None, b"x").code, status::CLIENT_BAD_REQUEST);
        assert_eq!(send(Some(false), b"first").code, status::OK);
        // a document is printing now: second one right after is fine with instant timing
        r.clock.advance(Duration::from_millis(5));
        let m = send(Some(true), b"second");
        assert_eq!(m.code, status::OK);
        r.clock.advance(Duration::from_millis(5));
        let j = r.svc.ctx.jobs.get(u32::try_from(id).unwrap()).unwrap();
        assert_eq!(j.state, JobState::Completed);
        assert_eq!(j.documents.len(), 2);
        // a third is not possible
        assert_eq!(send(Some(true), b"third").code, status::CLIENT_NOT_POSSIBLE);
    }

    #[test]
    fn empty_last_document_closes_a_multi_document_job() {
        let r = rig();
        let id = job_id(&call(
            &r,
            &request(op::CREATE_JOB, vec![printer_uri()], vec![], b""),
        ));
        let send = |last: bool, data: &[u8]| {
            call(
                &r,
                &request(
                    op::SEND_DOCUMENT,
                    vec![
                        printer_uri(),
                        Attr::int("job-id", id),
                        Attr::boolean("last-document", last),
                    ],
                    vec![],
                    data,
                ),
            )
            .code
        };
        assert_eq!(send(false, b"doc"), status::OK);
        assert_eq!(send(false, b""), status::CLIENT_BAD_REQUEST);
        r.clock.advance(Duration::from_millis(5));
        assert_eq!(send(true, b""), status::OK);
        r.clock.advance(Duration::from_millis(5));
        let j = r.svc.ctx.jobs.get(u32::try_from(id).unwrap()).unwrap();
        assert_eq!(j.state, JobState::Completed);
        assert_eq!(j.documents.len(), 1);
    }

    #[test]
    fn the_multiple_operation_timeout_aborts_a_job_whose_second_operation_never_comes() {
        let r = rig();
        let m = call(
            &r,
            &request(op::CREATE_JOB, vec![printer_uri()], vec![], b""),
        );
        let id = job_id(&m);
        // advertised
        let pa = call(
            &r,
            &request(
                op::GET_PRINTER_ATTRIBUTES,
                vec![
                    printer_uri(),
                    Attr::keywords("requested-attributes", &["multiple-operation-time-out"]),
                ],
                vec![],
                b"",
            ),
        );
        assert_eq!(
            int_of(
                pa.group(tag::PRINTER_ATTRIBUTES).unwrap(),
                "multiple-operation-time-out"
            ),
            Some(120)
        );
        r.clock.advance(Duration::from_secs(119));
        let state = |r: &Rig| {
            let m = call(
                r,
                &request(
                    op::GET_JOB_ATTRIBUTES,
                    vec![printer_uri(), Attr::int("job-id", id)],
                    vec![],
                    b"",
                ),
            );
            let g = m.group(tag::JOB_ATTRIBUTES).unwrap();
            (
                int_of(g, "job-state").unwrap(),
                g.get("job-state-reasons")
                    .unwrap()
                    .values
                    .iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect::<Vec<_>>(),
            )
        };
        assert_eq!(state(&r).0, 3);
        r.clock.advance(Duration::from_secs(2));
        let (s, reasons) = state(&r);
        assert_eq!(s, 8);
        assert_eq!(reasons, ["aborted-by-system", "submission-interrupted"]);
        // the late Send-Document is refused
        let m = call(
            &r,
            &request(
                op::SEND_DOCUMENT,
                vec![
                    printer_uri(),
                    Attr::int("job-id", id),
                    Attr::boolean("last-document", true),
                ],
                vec![],
                b"late",
            ),
        );
        assert_eq!(m.code, status::CLIENT_NOT_POSSIBLE);
    }

    #[test]
    fn cancel_job_matrix() {
        let r = rig_with(PrintTiming::realistic(), PrinterProfile::generic());
        let cancel = |id: i32| {
            call(
                &r,
                &request(
                    op::CANCEL_JOB,
                    vec![printer_uri(), Attr::int("job-id", id)],
                    vec![],
                    b"",
                ),
            )
            .code
        };
        assert_eq!(cancel(99), status::CLIENT_NOT_FOUND);
        let id = job_id(&print(&r, vec![]));
        r.clock.advance(Duration::from_millis(100)); // processing now (2 s print)
        assert_eq!(cancel(id), status::OK);
        // being cancelled (0.5 s terminating): the job is processing with a stop point
        let m = call(
            &r,
            &request(
                op::GET_JOB_ATTRIBUTES,
                vec![printer_uri(), Attr::int("job-id", id)],
                vec![],
                b"",
            ),
        );
        let g = m.group(tag::JOB_ATTRIBUTES).unwrap();
        assert_eq!(int_of(g, "job-state"), Some(5));
        assert_eq!(
            cancel(id),
            status::CLIENT_NOT_POSSIBLE,
            "already being cancelled"
        );
        r.clock.advance(Duration::from_secs(1));
        let m = call(
            &r,
            &request(
                op::GET_JOB_ATTRIBUTES,
                vec![printer_uri(), Attr::int("job-id", id)],
                vec![],
                b"",
            ),
        );
        assert_eq!(
            int_of(m.group(tag::JOB_ATTRIBUTES).unwrap(), "job-state"),
            Some(7)
        );
        assert_eq!(cancel(id), status::CLIENT_NOT_POSSIBLE, "terminal");
        // completed job
        let id2 = job_id(&print(&r, vec![]));
        r.clock.advance(Duration::from_secs(10));
        assert_eq!(cancel(id2), status::CLIENT_NOT_POSSIBLE);
        // the cancelled job's capture left the inbox
        assert!(r
            .svc
            .ctx
            .inbox
            .as_ref()
            .unwrap()
            .captured()
            .iter()
            .all(|c| i64::from(c.job_id) != i64::from(id)));
    }

    #[test]
    fn get_jobs_filters_orders_and_limits() {
        let r = rig();
        let mk = |user: &str| {
            let m = call(
                &r,
                &request(
                    op::PRINT_JOB,
                    vec![
                        printer_uri(),
                        Attr::new("requesting-user-name", Value::name(user)),
                    ],
                    vec![],
                    b"%PDF",
                ),
            );
            job_id(&m)
        };
        let a = mk("alice");
        r.clock.advance(Duration::from_millis(5));
        let b = mk("bob");
        r.clock.advance(Duration::from_millis(5));
        let c = mk("alice");
        // all three completed after the ticks
        r.clock.advance(Duration::from_millis(5));
        let ids = |m: &Message| -> Vec<i32> {
            m.groups
                .iter()
                .filter(|g| g.tag == tag::JOB_ATTRIBUTES)
                .map(|g| int_of(g, "job-id").unwrap())
                .collect()
        };
        let ask = |extra: Vec<Attr>| {
            call(
                &r,
                &request(
                    op::GET_JOBS,
                    [vec![printer_uri()], extra].concat(),
                    vec![],
                    b"",
                ),
            )
        };
        assert!(ids(&ask(vec![])).is_empty(), "nothing is pending");
        let all = ask(vec![Attr::new("which-jobs", Value::keyword("completed"))]);
        assert_eq!(ids(&all), [c, b, a], "most recently finished first");
        let mine = ask(vec![
            Attr::new("which-jobs", Value::keyword("completed")),
            Attr::boolean("my-jobs", true),
            Attr::new("requesting-user-name", Value::name("alice")),
        ]);
        assert_eq!(ids(&mine), [c, a]);
        let lim = ask(vec![
            Attr::new("which-jobs", Value::keyword("completed")),
            Attr::int("limit", 1),
        ]);
        assert_eq!(ids(&lim), [c]);
        // default attributes are job-id and job-uri only
        let g = &lim.groups[1];
        assert_eq!(
            g.attrs.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(),
            ["job-id", "job-uri"]
        );
        let sel = ask(vec![
            Attr::new("which-jobs", Value::keyword("completed")),
            Attr::keywords(
                "requested-attributes",
                &["job-state", "job-originating-user-name"],
            ),
        ]);
        assert_eq!(sel.groups[1].attrs.len(), 2);
    }

    #[test]
    fn completed_jobs_are_forgotten_after_the_retention_window() {
        let r = rig();
        let id = job_id(&print(&r, vec![]));
        r.clock.advance(Duration::from_millis(5));
        let get = || {
            call(
                &r,
                &request(
                    op::GET_JOB_ATTRIBUTES,
                    vec![printer_uri(), Attr::int("job-id", id)],
                    vec![],
                    b"",
                ),
            )
            .code
        };
        assert_eq!(get(), status::OK);
        r.clock.advance(Duration::from_secs(299));
        assert_eq!(get(), status::OK);
        r.clock.advance(Duration::from_secs(2));
        assert_eq!(get(), status::CLIENT_NOT_FOUND);
        // the shared engine still has it (the WSD side sees it)
        assert!(r.svc.ctx.jobs.get(u32::try_from(id).unwrap()).is_some());
        let m = call(
            &r,
            &request(
                op::GET_JOBS,
                vec![
                    printer_uri(),
                    Attr::new("which-jobs", Value::keyword("completed")),
                ],
                vec![],
                b"",
            ),
        );
        assert!(m.group(tag::JOB_ATTRIBUTES).is_none());
    }

    #[test]
    fn printer_state_follows_conditions_and_the_accepting_flag() {
        let r = rig();
        let ask = |r: &Rig| {
            let m = call(
                r,
                &request(
                    op::GET_PRINTER_ATTRIBUTES,
                    vec![
                        printer_uri(),
                        Attr::keywords(
                            "requested-attributes",
                            &[
                                "printer-state",
                                "printer-state-reasons",
                                "printer-is-accepting-jobs",
                            ],
                        ),
                    ],
                    vec![],
                    b"",
                ),
            );
            let g = m.group(tag::PRINTER_ATTRIBUTES).unwrap().clone();
            (
                int_of(&g, "printer-state").unwrap(),
                g.get("printer-state-reasons")
                    .unwrap()
                    .values
                    .iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect::<Vec<_>>(),
                bool_of(&g, "printer-is-accepting-jobs").unwrap(),
            )
        };
        assert_eq!(ask(&r), (3, vec!["none".to_string()], true));
        r.svc.ctx.apply_faults(&crate::wsd::ctx::FaultBody {
            paper_out: Some(true),
            toner_low: Some(true),
            not_accepting_jobs: Some(true),
            ..Default::default()
        });
        let (state, reasons, accepting) = ask(&r);
        assert_eq!(state, 5);
        assert!(
            reasons.contains(&"media-empty-error".to_string()),
            "{reasons:?}"
        );
        assert!(
            reasons.contains(&"marker-supply-low-warning".to_string()),
            "{reasons:?}"
        );
        assert!(reasons.contains(&"offline-report".to_string()));
        assert!(!accepting);
        // not accepting: Print-Job, Create-Job and Validate-Job are refused
        for o in [op::PRINT_JOB, op::CREATE_JOB, op::VALIDATE_JOB] {
            let m = call(&r, &request(o, vec![printer_uri()], vec![], b"%PDF"));
            assert_eq!(m.code, status::SERVER_NOT_ACCEPTING_JOBS);
        }
    }

    #[test]
    fn engine_faults_map_to_server_errors() {
        let r = rig();
        let create = |r: &Rig| {
            call(
                r,
                &request(op::CREATE_JOB, vec![printer_uri()], vec![], b""),
            )
            .code
        };
        r.svc.ctx.apply_faults(&crate::wsd::ctx::FaultBody {
            temporary_error: Some(true),
            ..Default::default()
        });
        assert_eq!(create(&r), status::SERVER_TEMPORARY_ERROR);
        r.svc.ctx.apply_faults(&crate::wsd::ctx::FaultBody {
            temporary_error: Some(false),
            internal_error: Some(true),
            ..Default::default()
        });
        assert_eq!(create(&r), status::SERVER_INTERNAL_ERROR);
    }

    #[test]
    fn multi_document_jobs_can_be_switched_off_by_the_profile() {
        let mut p = PrinterProfile::generic();
        p.description.multiple_document_jobs = false;
        let r = rig_with(PrintTiming::instant(), p);
        let id = job_id(&call(
            &r,
            &request(op::CREATE_JOB, vec![printer_uri()], vec![], b""),
        ));
        let m = call(
            &r,
            &request(
                op::SEND_DOCUMENT,
                vec![
                    printer_uri(),
                    Attr::int("job-id", id),
                    Attr::boolean("last-document", false),
                ],
                vec![],
                b"x",
            ),
        );
        assert_eq!(m.code, status::SERVER_MULTIPLE_DOCUMENT_JOBS_NOT_SUPPORTED);
    }

    #[test]
    fn job_uri_targets_and_conflicts() {
        let r = rig();
        let id = job_id(&print(&r, vec![]));
        let uri = format!("{URI}/{id}");
        let get =
            |attrs: Vec<Attr>| call(&r, &request(op::GET_JOB_ATTRIBUTES, attrs, vec![], b"")).code;
        assert_eq!(
            get(vec![Attr::new("job-uri", Value::uri(uri.clone()))]),
            status::OK
        );
        assert_eq!(
            get(vec![
                Attr::new("job-uri", Value::uri(uri)),
                Attr::int("job-id", id)
            ]),
            status::CLIENT_BAD_REQUEST
        );
        assert_eq!(
            get(vec![Attr::new("job-uri", Value::uri(format!("{URI}/999")))]),
            status::CLIENT_NOT_FOUND
        );
        assert_eq!(
            get(vec![Attr::new(
                "job-uri",
                Value::uri("ipp://h/elsewhere/1")
            )]),
            status::CLIENT_NOT_FOUND
        );
        assert_eq!(get(vec![printer_uri()]), status::CLIENT_BAD_REQUEST);
        assert_eq!(
            get(vec![printer_uri(), Attr::int("job-id", 0)]),
            status::CLIENT_BAD_REQUEST
        );
    }
}
