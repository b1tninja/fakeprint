//! The IPP wire encoding (RFC 8010), used for requests and responses alike.
//!
//! A message is a fixed header (version, operation id or status code, request id), a series of
//! attribute groups (each opened by a delimiter tag, holding attributes made of a value tag, a
//! name and a value, where a zero-length name continues the previous attribute as one more
//! value), the `end-of-attributes` tag, and then the document data, which is whatever remains of
//! the body.
//!
//! [`Message::parse`] is strict: it refuses truncated lengths, fixed-size syntaxes of the wrong
//! size, text that is not UTF-8, a repeated attribute name inside a group, a continuation value
//! without an attribute to continue, reserved delimiter tags, a missing `end-of-attributes`, and
//! collections that are not closed. Every refusal is a [`ParseError`] that the service turns into
//! `client-error-bad-request`. What the operations require on top of that (the first two
//! operation attributes, the target, syntaxes of the known attributes) is checked one level up,
//! in `service`.

use std::fmt;

use serde_json::{json, Value as Json};

/// Delimiter and value tags (RFC 8010 section 3.5).
pub mod tag {
    pub const OPERATION_ATTRIBUTES: u8 = 0x01;
    pub const JOB_ATTRIBUTES: u8 = 0x02;
    pub const END_OF_ATTRIBUTES: u8 = 0x03;
    pub const PRINTER_ATTRIBUTES: u8 = 0x04;
    pub const UNSUPPORTED_ATTRIBUTES: u8 = 0x05;
    /// Highest delimiter tag RFC 8010 / PWG 5100.x define (system attributes).
    pub const LAST_DELIMITER: u8 = 0x0a;

    pub const UNSUPPORTED: u8 = 0x10;
    pub const UNKNOWN: u8 = 0x12;
    pub const NO_VALUE: u8 = 0x13;
    pub const NOT_SETTABLE: u8 = 0x15;
    pub const DELETE_ATTRIBUTE: u8 = 0x16;
    pub const ADMIN_DEFINE: u8 = 0x17;

    pub const INTEGER: u8 = 0x21;
    pub const BOOLEAN: u8 = 0x22;
    pub const ENUM: u8 = 0x23;

    pub const OCTET_STRING: u8 = 0x30;
    pub const DATE_TIME: u8 = 0x31;
    pub const RESOLUTION: u8 = 0x32;
    pub const RANGE_OF_INTEGER: u8 = 0x33;
    pub const BEG_COLLECTION: u8 = 0x34;
    pub const TEXT_WITH_LANGUAGE: u8 = 0x35;
    pub const NAME_WITH_LANGUAGE: u8 = 0x36;
    pub const END_COLLECTION: u8 = 0x37;

    pub const TEXT_WITHOUT_LANGUAGE: u8 = 0x41;
    pub const NAME_WITHOUT_LANGUAGE: u8 = 0x42;
    pub const KEYWORD: u8 = 0x44;
    pub const URI: u8 = 0x45;
    pub const URI_SCHEME: u8 = 0x46;
    pub const CHARSET: u8 = 0x47;
    pub const NATURAL_LANGUAGE: u8 = 0x48;
    pub const MIME_MEDIA_TYPE: u8 = 0x49;
    pub const MEMBER_ATTR_NAME: u8 = 0x4a;
}

/// Operation ids (RFC 8011 section 5.2.15 and the IPP registry).
pub mod op {
    pub const PRINT_JOB: u16 = 0x0002;
    pub const PRINT_URI: u16 = 0x0003;
    pub const VALIDATE_JOB: u16 = 0x0004;
    pub const CREATE_JOB: u16 = 0x0005;
    pub const SEND_DOCUMENT: u16 = 0x0006;
    pub const SEND_URI: u16 = 0x0007;
    pub const CANCEL_JOB: u16 = 0x0008;
    pub const GET_JOB_ATTRIBUTES: u16 = 0x0009;
    pub const GET_JOBS: u16 = 0x000a;
    pub const GET_PRINTER_ATTRIBUTES: u16 = 0x000b;
}

/// The operations this server implements, as `(id, IPP name, counter name, injection name)`.
pub const OPERATIONS: [(u16, &str, &str, &str); 8] = [
    (op::PRINT_JOB, "Print-Job", "PrintJob", "print_job"),
    (
        op::VALIDATE_JOB,
        "Validate-Job",
        "ValidateJob",
        "validate_job",
    ),
    (op::CREATE_JOB, "Create-Job", "CreateJob", "create_job"),
    (
        op::SEND_DOCUMENT,
        "Send-Document",
        "SendDocument",
        "send_document",
    ),
    (op::CANCEL_JOB, "Cancel-Job", "CancelJob", "cancel_job"),
    (
        op::GET_JOB_ATTRIBUTES,
        "Get-Job-Attributes",
        "GetJobAttributes",
        "get_job_attributes",
    ),
    (op::GET_JOBS, "Get-Jobs", "GetJobs", "get_jobs"),
    (
        op::GET_PRINTER_ATTRIBUTES,
        "Get-Printer-Attributes",
        "GetPrinterAttributes",
        "get_printer_attributes",
    ),
];

/// Operations that exist in IPP but that this server answers with `operation-not-supported`.
pub const KNOWN_UNSUPPORTED_OPERATIONS: [(u16, &str); 2] =
    [(op::PRINT_URI, "Print-URI"), (op::SEND_URI, "Send-URI")];

/// Name used by the injection rules and the action counters for an operation id.
pub fn op_inject_name(id: u16) -> String {
    OPERATIONS
        .iter()
        .find(|o| o.0 == id)
        .map(|o| o.3.to_string())
        .or_else(|| {
            KNOWN_UNSUPPORTED_OPERATIONS
                .iter()
                .find(|o| o.0 == id)
                .map(|o| o.1.to_ascii_lowercase().replace('-', "_"))
        })
        .unwrap_or_else(|| format!("op_{id:04x}"))
}

/// The counter name (`PrintJob`, ...) of an operation id; `Other` when it is not implemented.
pub fn op_counter_name(id: u16) -> &'static str {
    OPERATIONS
        .iter()
        .find(|o| o.0 == id)
        .map(|o| o.2)
        .unwrap_or("Other")
}

/// The IPP name (`Print-Job`) of an operation id, if the registry entry is known here.
pub fn op_name(id: u16) -> Option<&'static str> {
    OPERATIONS
        .iter()
        .find(|o| o.0 == id)
        .map(|o| o.1)
        .or_else(|| {
            KNOWN_UNSUPPORTED_OPERATIONS
                .iter()
                .find(|o| o.0 == id)
                .map(|o| o.1)
        })
}

/// Status codes (RFC 8011 section 5.2.? `status-code` registry).
pub mod status {
    pub const OK: u16 = 0x0000;
    pub const OK_IGNORED_OR_SUBSTITUTED: u16 = 0x0001;
    pub const OK_CONFLICTING_ATTRIBUTES: u16 = 0x0002;
    pub const CLIENT_BAD_REQUEST: u16 = 0x0400;
    pub const CLIENT_FORBIDDEN: u16 = 0x0401;
    pub const CLIENT_NOT_AUTHENTICATED: u16 = 0x0402;
    pub const CLIENT_NOT_AUTHORIZED: u16 = 0x0403;
    pub const CLIENT_NOT_POSSIBLE: u16 = 0x0404;
    pub const CLIENT_TIMEOUT: u16 = 0x0405;
    pub const CLIENT_NOT_FOUND: u16 = 0x0406;
    pub const CLIENT_GONE: u16 = 0x0407;
    pub const CLIENT_REQUEST_ENTITY_TOO_LARGE: u16 = 0x0408;
    pub const CLIENT_REQUEST_VALUE_TOO_LONG: u16 = 0x0409;
    pub const CLIENT_DOCUMENT_FORMAT_NOT_SUPPORTED: u16 = 0x040a;
    pub const CLIENT_ATTRIBUTES_OR_VALUES_NOT_SUPPORTED: u16 = 0x040b;
    pub const CLIENT_URI_SCHEME_NOT_SUPPORTED: u16 = 0x040c;
    pub const CLIENT_CHARSET_NOT_SUPPORTED: u16 = 0x040d;
    pub const CLIENT_CONFLICTING_ATTRIBUTES: u16 = 0x040e;
    pub const CLIENT_COMPRESSION_NOT_SUPPORTED: u16 = 0x040f;
    pub const CLIENT_COMPRESSION_ERROR: u16 = 0x0410;
    pub const CLIENT_DOCUMENT_FORMAT_ERROR: u16 = 0x0411;
    pub const CLIENT_DOCUMENT_ACCESS_ERROR: u16 = 0x0412;
    pub const SERVER_INTERNAL_ERROR: u16 = 0x0500;
    pub const SERVER_OPERATION_NOT_SUPPORTED: u16 = 0x0501;
    pub const SERVER_SERVICE_UNAVAILABLE: u16 = 0x0502;
    pub const SERVER_VERSION_NOT_SUPPORTED: u16 = 0x0503;
    pub const SERVER_DEVICE_ERROR: u16 = 0x0504;
    pub const SERVER_TEMPORARY_ERROR: u16 = 0x0505;
    pub const SERVER_NOT_ACCEPTING_JOBS: u16 = 0x0506;
    pub const SERVER_BUSY: u16 = 0x0507;
    pub const SERVER_JOB_CANCELED: u16 = 0x0508;
    pub const SERVER_MULTIPLE_DOCUMENT_JOBS_NOT_SUPPORTED: u16 = 0x0509;
    pub const SERVER_PRINTER_IS_DEACTIVATED: u16 = 0x050a;
}

/// Every status code with its registry name (kebab case), for `ipp_status` injection and logs.
pub const STATUS_NAMES: [(u16, &str); 33] = [
    (status::OK, "successful-ok"),
    (
        status::OK_IGNORED_OR_SUBSTITUTED,
        "successful-ok-ignored-or-substituted-attributes",
    ),
    (
        status::OK_CONFLICTING_ATTRIBUTES,
        "successful-ok-conflicting-attributes",
    ),
    (status::CLIENT_BAD_REQUEST, "client-error-bad-request"),
    (status::CLIENT_FORBIDDEN, "client-error-forbidden"),
    (
        status::CLIENT_NOT_AUTHENTICATED,
        "client-error-not-authenticated",
    ),
    (status::CLIENT_NOT_AUTHORIZED, "client-error-not-authorized"),
    (status::CLIENT_NOT_POSSIBLE, "client-error-not-possible"),
    (status::CLIENT_TIMEOUT, "client-error-timeout"),
    (status::CLIENT_NOT_FOUND, "client-error-not-found"),
    (status::CLIENT_GONE, "client-error-gone"),
    (
        status::CLIENT_REQUEST_ENTITY_TOO_LARGE,
        "client-error-request-entity-too-large",
    ),
    (
        status::CLIENT_REQUEST_VALUE_TOO_LONG,
        "client-error-request-value-too-long",
    ),
    (
        status::CLIENT_DOCUMENT_FORMAT_NOT_SUPPORTED,
        "client-error-document-format-not-supported",
    ),
    (
        status::CLIENT_ATTRIBUTES_OR_VALUES_NOT_SUPPORTED,
        "client-error-attributes-or-values-not-supported",
    ),
    (
        status::CLIENT_URI_SCHEME_NOT_SUPPORTED,
        "client-error-uri-scheme-not-supported",
    ),
    (
        status::CLIENT_CHARSET_NOT_SUPPORTED,
        "client-error-charset-not-supported",
    ),
    (
        status::CLIENT_CONFLICTING_ATTRIBUTES,
        "client-error-conflicting-attributes",
    ),
    (
        status::CLIENT_COMPRESSION_NOT_SUPPORTED,
        "client-error-compression-not-supported",
    ),
    (
        status::CLIENT_COMPRESSION_ERROR,
        "client-error-compression-error",
    ),
    (
        status::CLIENT_DOCUMENT_FORMAT_ERROR,
        "client-error-document-format-error",
    ),
    (
        status::CLIENT_DOCUMENT_ACCESS_ERROR,
        "client-error-document-access-error",
    ),
    (status::SERVER_INTERNAL_ERROR, "server-error-internal-error"),
    (
        status::SERVER_OPERATION_NOT_SUPPORTED,
        "server-error-operation-not-supported",
    ),
    (
        status::SERVER_SERVICE_UNAVAILABLE,
        "server-error-service-unavailable",
    ),
    (
        status::SERVER_VERSION_NOT_SUPPORTED,
        "server-error-version-not-supported",
    ),
    (status::SERVER_DEVICE_ERROR, "server-error-device-error"),
    (
        status::SERVER_TEMPORARY_ERROR,
        "server-error-temporary-error",
    ),
    (
        status::SERVER_NOT_ACCEPTING_JOBS,
        "server-error-not-accepting-jobs",
    ),
    (status::SERVER_BUSY, "server-error-busy"),
    (status::SERVER_JOB_CANCELED, "server-error-job-canceled"),
    (
        status::SERVER_MULTIPLE_DOCUMENT_JOBS_NOT_SUPPORTED,
        "server-error-multiple-document-jobs-not-supported",
    ),
    (
        status::SERVER_PRINTER_IS_DEACTIVATED,
        "server-error-printer-is-deactivated",
    ),
];

/// Registry name of a status code (`client-error-not-found`), or `status-0x....` when unknown.
pub fn status_name(code: u16) -> String {
    STATUS_NAMES
        .iter()
        .find(|s| s.0 == code)
        .map(|s| s.1.to_string())
        .unwrap_or_else(|| format!("status-{code:#06x}"))
}

/// A status code from its registry name, a decimal number or `0x` hex.
pub fn status_from_str(s: &str) -> Option<u16> {
    let t = s.trim();
    if let Some(c) = STATUS_NAMES.iter().find(|n| n.1.eq_ignore_ascii_case(t)) {
        return Some(c.0);
    }
    if let Some(h) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        return u16::from_str_radix(h, 16).ok();
    }
    t.parse::<u16>().ok()
}

/// Whether a status code is a success (below 0x0100).
pub fn is_success(code: u16) -> bool {
    code < 0x0100
}

// ---------------------------------------------------------------------------------------------
// Values
// ---------------------------------------------------------------------------------------------

/// One member of a collection value.
#[derive(Debug, Clone, PartialEq)]
pub struct Member {
    pub name: String,
    pub values: Vec<Value>,
}

/// One attribute value.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Integer(i32),
    Boolean(bool),
    Enum(i32),
    OctetString(Vec<u8>),
    DateTime([u8; 11]),
    /// `units`: 3 = dots per inch, 4 = dots per centimetre.
    Resolution {
        x: i32,
        y: i32,
        units: u8,
    },
    Range {
        lower: i32,
        upper: i32,
    },
    TextWithLanguage {
        language: String,
        text: String,
    },
    NameWithLanguage {
        language: String,
        name: String,
    },
    Text(String),
    Name(String),
    Keyword(String),
    Uri(String),
    UriScheme(String),
    Charset(String),
    NaturalLanguage(String),
    MimeType(String),
    Collection(Vec<Member>),
    /// `unsupported`, `unknown`, `no-value`, ... (the tag, no value bytes).
    OutOfBand(u8),
    /// A value tag this codec has no syntax for (kept verbatim).
    Raw {
        tag: u8,
        data: Vec<u8>,
    },
}

impl Value {
    pub fn keyword(s: impl Into<String>) -> Value {
        Value::Keyword(s.into())
    }
    pub fn text(s: impl Into<String>) -> Value {
        Value::Text(s.into())
    }
    pub fn name(s: impl Into<String>) -> Value {
        Value::Name(s.into())
    }
    pub fn uri(s: impl Into<String>) -> Value {
        Value::Uri(s.into())
    }
    pub fn mime(s: impl Into<String>) -> Value {
        Value::MimeType(s.into())
    }
    pub fn dpi(x: i32, y: i32) -> Value {
        Value::Resolution { x, y, units: 3 }
    }
    pub fn range(lower: i32, upper: i32) -> Value {
        Value::Range { lower, upper }
    }
    pub fn no_value() -> Value {
        Value::OutOfBand(tag::NO_VALUE)
    }

    /// The wire tag.
    pub fn tag(&self) -> u8 {
        match self {
            Value::Integer(_) => tag::INTEGER,
            Value::Boolean(_) => tag::BOOLEAN,
            Value::Enum(_) => tag::ENUM,
            Value::OctetString(_) => tag::OCTET_STRING,
            Value::DateTime(_) => tag::DATE_TIME,
            Value::Resolution { .. } => tag::RESOLUTION,
            Value::Range { .. } => tag::RANGE_OF_INTEGER,
            Value::TextWithLanguage { .. } => tag::TEXT_WITH_LANGUAGE,
            Value::NameWithLanguage { .. } => tag::NAME_WITH_LANGUAGE,
            Value::Text(_) => tag::TEXT_WITHOUT_LANGUAGE,
            Value::Name(_) => tag::NAME_WITHOUT_LANGUAGE,
            Value::Keyword(_) => tag::KEYWORD,
            Value::Uri(_) => tag::URI,
            Value::UriScheme(_) => tag::URI_SCHEME,
            Value::Charset(_) => tag::CHARSET,
            Value::NaturalLanguage(_) => tag::NATURAL_LANGUAGE,
            Value::MimeType(_) => tag::MIME_MEDIA_TYPE,
            Value::Collection(_) => tag::BEG_COLLECTION,
            Value::OutOfBand(t) => *t,
            Value::Raw { tag, .. } => *tag,
        }
    }

    /// The string of any character-string syntax (text and name with language included).
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Text(s)
            | Value::Name(s)
            | Value::Keyword(s)
            | Value::Uri(s)
            | Value::UriScheme(s)
            | Value::Charset(s)
            | Value::NaturalLanguage(s)
            | Value::MimeType(s) => Some(s),
            Value::TextWithLanguage { text, .. } => Some(text),
            Value::NameWithLanguage { name, .. } => Some(name),
            _ => None,
        }
    }

    pub fn as_int(&self) -> Option<i32> {
        match self {
            Value::Integer(i) | Value::Enum(i) => Some(*i),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Boolean(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_collection(&self) -> Option<&[Member]> {
        match self {
            Value::Collection(m) => Some(m),
            _ => None,
        }
    }

    /// Whether the value is a `name` (with or without language).
    pub fn is_name(&self) -> bool {
        matches!(self, Value::Name(_) | Value::NameWithLanguage { .. })
    }

    /// Whether the value is a `text` (with or without language).
    pub fn is_text(&self) -> bool {
        matches!(self, Value::Text(_) | Value::TextWithLanguage { .. })
    }

    /// Whether the value is a `keyword` or a `name` (the syntax of `media` and similar).
    pub fn is_keyword_or_name(&self) -> bool {
        matches!(self, Value::Keyword(_)) || self.is_name()
    }

    /// A JSON rendering for the control plane.
    pub fn to_json(&self) -> Json {
        match self {
            Value::Integer(i) | Value::Enum(i) => json!(i),
            Value::Boolean(b) => json!(b),
            Value::OctetString(b) => json!(format!("octets({})", b.len())),
            Value::DateTime(d) => json!(format_date_time(d)),
            Value::Resolution { x, y, units } => {
                json!({"x": x, "y": y, "units": if *units == 3 { "dpi" } else { "dpcm" }})
            }
            Value::Range { lower, upper } => json!({"lower": lower, "upper": upper}),
            Value::TextWithLanguage { language, text } => json!({"lang": language, "text": text}),
            Value::NameWithLanguage { language, name } => json!({"lang": language, "name": name}),
            Value::Collection(members) => Json::Object(
                members
                    .iter()
                    .map(|m| (m.name.clone(), values_json(&m.values)))
                    .collect(),
            ),
            Value::OutOfBand(t) => json!(oob_name(*t)),
            Value::Raw { tag, data } => json!(format!("raw({tag:#04x},{} bytes)", data.len())),
            other => json!(other.as_str().unwrap_or("")),
        }
    }
}

fn values_json(v: &[Value]) -> Json {
    if v.len() == 1 {
        v[0].to_json()
    } else {
        Json::Array(v.iter().map(Value::to_json).collect())
    }
}

fn oob_name(t: u8) -> &'static str {
    match t {
        tag::UNSUPPORTED => "unsupported",
        tag::UNKNOWN => "unknown",
        tag::NO_VALUE => "no-value",
        tag::NOT_SETTABLE => "not-settable",
        tag::DELETE_ATTRIBUTE => "delete-attribute",
        tag::ADMIN_DEFINE => "admin-define",
        _ => "out-of-band",
    }
}

fn format_date_time(d: &[u8; 11]) -> String {
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{}{}{:02}:{:02}",
        u16::from_be_bytes([d[0], d[1]]),
        d[2],
        d[3],
        d[4],
        d[5],
        d[6],
        d[7],
        char::from(d[8]),
        d[9],
        d[10]
    )
}

/// An attribute: a name and one or more values (`1setOf`).
#[derive(Debug, Clone, PartialEq)]
pub struct Attr {
    pub name: String,
    pub values: Vec<Value>,
}

impl Attr {
    pub fn new(name: impl Into<String>, value: Value) -> Attr {
        Attr {
            name: name.into(),
            values: vec![value],
        }
    }

    pub fn many(name: impl Into<String>, values: Vec<Value>) -> Attr {
        Attr {
            name: name.into(),
            values,
        }
    }

    pub fn keywords<S: AsRef<str>>(name: impl Into<String>, list: &[S]) -> Attr {
        Attr::many(
            name,
            list.iter()
                .map(|s| Value::Keyword(s.as_ref().to_string()))
                .collect(),
        )
    }

    pub fn int(name: impl Into<String>, v: i32) -> Attr {
        Attr::new(name, Value::Integer(v))
    }

    pub fn enum_(name: impl Into<String>, v: i32) -> Attr {
        Attr::new(name, Value::Enum(v))
    }

    pub fn boolean(name: impl Into<String>, v: bool) -> Attr {
        Attr::new(name, Value::Boolean(v))
    }

    pub fn first(&self) -> Option<&Value> {
        self.values.first()
    }

    /// The attribute `name` marked unsupported (out-of-band `unsupported`, no value).
    pub fn unsupported(name: impl Into<String>) -> Attr {
        Attr::new(name, Value::OutOfBand(tag::UNSUPPORTED))
    }

    pub fn to_json(&self) -> Json {
        values_json(&self.values)
    }
}

/// An attribute group.
#[derive(Debug, Clone, PartialEq)]
pub struct Group {
    pub tag: u8,
    pub attrs: Vec<Attr>,
}

impl Group {
    pub fn new(tag: u8) -> Group {
        Group {
            tag,
            attrs: Vec::new(),
        }
    }

    pub fn get(&self, name: &str) -> Option<&Attr> {
        self.attrs.iter().find(|a| a.name == name)
    }

    pub fn push(&mut self, a: Attr) {
        self.attrs.push(a);
    }

    pub fn to_json(&self) -> Json {
        Json::Object(
            self.attrs
                .iter()
                .map(|a| (a.name.clone(), a.to_json()))
                .collect(),
        )
    }
}

/// A whole IPP message (request or response).
#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    pub version: (u8, u8),
    /// Operation id in a request, status code in a response.
    pub code: u16,
    pub request_id: u32,
    pub groups: Vec<Group>,
    /// The bytes after `end-of-attributes` (the document, for Print-Job and Send-Document).
    pub data: Vec<u8>,
}

/// Why a message could not be parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub message: String,
    pub offset: usize,
    /// The header, when it was readable (so the reply can echo the request id).
    pub version: Option<(u8, u8)>,
    pub request_id: Option<u32>,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} (at byte {})", self.message, self.offset)
    }
}

impl std::error::Error for ParseError {}

/// Largest nesting of collections accepted.
const MAX_DEPTH: usize = 8;
/// Largest number of values of one attribute.
const MAX_VALUES: usize = 4096;

struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
    version: Option<(u8, u8)>,
    request_id: Option<u32>,
}

impl<'a> Cursor<'a> {
    fn err(&self, message: impl Into<String>) -> ParseError {
        ParseError {
            message: message.into(),
            offset: self.pos,
            version: self.version,
            request_id: self.request_id,
        }
    }

    fn u8(&mut self) -> Result<u8, ParseError> {
        let b = *self
            .buf
            .get(self.pos)
            .ok_or_else(|| self.err("message ends inside an attribute"))?;
        self.pos += 1;
        Ok(b)
    }

    fn u16(&mut self) -> Result<u16, ParseError> {
        let b = self.take(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], ParseError> {
        let end = self
            .pos
            .checked_add(n)
            .filter(|e| *e <= self.buf.len())
            .ok_or_else(|| self.err(format!("a length of {n} bytes runs past the message end")))?;
        let s = &self.buf[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    fn string(&self, bytes: &[u8], what: &str) -> Result<String, ParseError> {
        std::str::from_utf8(bytes)
            .map(str::to_string)
            .map_err(|_| self.err(format!("{what} is not valid UTF-8")))
    }

    /// Read `name-length name value-length value` after the value tag.
    fn name_and_value(&mut self) -> Result<(&'a [u8], &'a [u8]), ParseError> {
        let nl = usize::from(self.u16()?);
        let name = self.take(nl)?;
        let vl = usize::from(self.u16()?);
        let value = self.take(vl)?;
        Ok((name, value))
    }

    fn value(&mut self, t: u8, data: &[u8], depth: usize) -> Result<Value, ParseError> {
        let fixed = |n: usize, what: &str, this: &Self| -> Result<(), ParseError> {
            if data.len() == n {
                Ok(())
            } else {
                Err(this.err(format!(
                    "{what} value is {} bytes, it must be {n}",
                    data.len()
                )))
            }
        };
        let i32_at =
            |o: usize| i32::from_be_bytes([data[o], data[o + 1], data[o + 2], data[o + 3]]);
        Ok(match t {
            tag::INTEGER => {
                fixed(4, "integer", self)?;
                Value::Integer(i32_at(0))
            }
            tag::ENUM => {
                fixed(4, "enum", self)?;
                Value::Enum(i32_at(0))
            }
            tag::BOOLEAN => {
                fixed(1, "boolean", self)?;
                match data[0] {
                    0 => Value::Boolean(false),
                    1 => Value::Boolean(true),
                    other => return Err(self.err(format!("boolean value {other} is not 0 or 1"))),
                }
            }
            tag::OCTET_STRING => {
                if data.len() > 1023 {
                    return Err(self.err("octetString value is longer than 1023 bytes"));
                }
                Value::OctetString(data.to_vec())
            }
            tag::DATE_TIME => {
                fixed(11, "dateTime", self)?;
                let mut d = [0u8; 11];
                d.copy_from_slice(data);
                Value::DateTime(d)
            }
            tag::RESOLUTION => {
                fixed(9, "resolution", self)?;
                let units = data[8];
                if units != 3 && units != 4 {
                    return Err(self.err(format!("resolution units {units} is not 3 or 4")));
                }
                Value::Resolution {
                    x: i32_at(0),
                    y: i32_at(4),
                    units,
                }
            }
            tag::RANGE_OF_INTEGER => {
                fixed(8, "rangeOfInteger", self)?;
                Value::Range {
                    lower: i32_at(0),
                    upper: i32_at(4),
                }
            }
            tag::TEXT_WITH_LANGUAGE | tag::NAME_WITH_LANGUAGE => {
                let what = if t == tag::TEXT_WITH_LANGUAGE {
                    "textWithLanguage"
                } else {
                    "nameWithLanguage"
                };
                let bad = |this: &Self| this.err(format!("{what} value has inconsistent lengths"));
                if data.len() < 4 {
                    return Err(bad(self));
                }
                let ll = usize::from(u16::from_be_bytes([data[0], data[1]]));
                if data.len() < 2 + ll + 2 {
                    return Err(bad(self));
                }
                let tl = usize::from(u16::from_be_bytes([data[2 + ll], data[3 + ll]]));
                if data.len() != 4 + ll + tl {
                    return Err(bad(self));
                }
                let language = self.string(&data[2..2 + ll], what)?;
                let text = self.string(&data[4 + ll..], what)?;
                if t == tag::TEXT_WITH_LANGUAGE {
                    Value::TextWithLanguage { language, text }
                } else {
                    Value::NameWithLanguage {
                        language,
                        name: text,
                    }
                }
            }
            tag::TEXT_WITHOUT_LANGUAGE => Value::Text(self.string(data, "text")?),
            tag::NAME_WITHOUT_LANGUAGE => Value::Name(self.string(data, "name")?),
            tag::KEYWORD => Value::Keyword(self.ascii(data, "keyword")?),
            tag::URI => Value::Uri(self.string(data, "uri")?),
            tag::URI_SCHEME => Value::UriScheme(self.ascii(data, "uriScheme")?),
            tag::CHARSET => Value::Charset(self.ascii(data, "charset")?),
            tag::NATURAL_LANGUAGE => Value::NaturalLanguage(self.ascii(data, "naturalLanguage")?),
            tag::MIME_MEDIA_TYPE => Value::MimeType(self.ascii(data, "mimeMediaType")?),
            tag::BEG_COLLECTION => {
                if !data.is_empty() {
                    return Err(self.err("begCollection carries a value"));
                }
                if depth >= MAX_DEPTH {
                    return Err(self.err("collections are nested too deeply"));
                }
                Value::Collection(self.collection(depth + 1)?)
            }
            tag::END_COLLECTION | tag::MEMBER_ATTR_NAME => {
                return Err(self.err(format!("value tag {t:#04x} outside a collection")))
            }
            tag::UNSUPPORTED
            | tag::UNKNOWN
            | tag::NO_VALUE
            | tag::NOT_SETTABLE
            | tag::DELETE_ATTRIBUTE
            | tag::ADMIN_DEFINE => {
                if !data.is_empty() {
                    return Err(self.err(format!("out-of-band value {t:#04x} carries data")));
                }
                Value::OutOfBand(t)
            }
            other => Value::Raw {
                tag: other,
                data: data.to_vec(),
            },
        })
    }

    fn ascii(&self, data: &[u8], what: &str) -> Result<String, ParseError> {
        if !data.is_ascii() {
            return Err(self.err(format!("{what} value is not US-ASCII")));
        }
        self.string(data, what)
    }

    /// The members of a collection, after its `begCollection`; consumes `endCollection`.
    fn collection(&mut self, depth: usize) -> Result<Vec<Member>, ParseError> {
        let mut members: Vec<Member> = Vec::new();
        loop {
            let t = self.u8()?;
            let (name, data) = self.name_and_value()?;
            if !name.is_empty() {
                return Err(self.err("an element of a collection has a name"));
            }
            match t {
                tag::END_COLLECTION => {
                    if !data.is_empty() {
                        return Err(self.err("endCollection carries a value"));
                    }
                    return Ok(members);
                }
                tag::MEMBER_ATTR_NAME => {
                    let n = self.string(data, "member name")?;
                    if n.is_empty() {
                        return Err(self.err("collection member without a name"));
                    }
                    if members.iter().any(|m| m.name == n) {
                        return Err(self.err(format!("collection member {n:?} appears twice")));
                    }
                    members.push(Member {
                        name: n,
                        values: Vec::new(),
                    });
                }
                t if t < 0x10 => return Err(self.err("delimiter tag inside a collection")),
                t => {
                    let v = self.value(t, data, depth)?;
                    let Some(m) = members.last_mut() else {
                        return Err(self.err("collection value before any member name"));
                    };
                    if m.values.len() >= MAX_VALUES {
                        return Err(self.err("too many values"));
                    }
                    m.values.push(v);
                }
            }
        }
    }
}

impl Message {
    pub fn new(version: (u8, u8), code: u16, request_id: u32) -> Message {
        Message {
            version,
            code,
            request_id,
            groups: Vec::new(),
            data: Vec::new(),
        }
    }

    /// Parse a message. The document data (everything after `end-of-attributes`) is returned
    /// in [`Message::data`].
    pub fn parse(buf: &[u8]) -> Result<Message, ParseError> {
        let mut c = Cursor {
            buf,
            pos: 0,
            version: None,
            request_id: None,
        };
        if buf.len() < 8 {
            return Err(c.err(format!(
                "the message is {} bytes, the header alone is 8",
                buf.len()
            )));
        }
        let version = (buf[0], buf[1]);
        let code = u16::from_be_bytes([buf[2], buf[3]]);
        let request_id = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
        c.pos = 8;
        c.version = Some(version);
        c.request_id = Some(request_id);
        let mut groups: Vec<Group> = Vec::new();
        loop {
            let t = c.u8()?;
            match t {
                tag::END_OF_ATTRIBUTES => break,
                0x00 | 0x0b..=0x0f => {
                    return Err(c.err(format!("reserved delimiter tag {t:#04x}")));
                }
                t if t <= tag::LAST_DELIMITER => groups.push(Group::new(t)),
                t => {
                    let (name, data) = c.name_and_value()?;
                    let Some(group) = groups.last_mut() else {
                        return Err(c.err("an attribute appears before any attribute group"));
                    };
                    let value = c.value(t, data, 0)?;
                    if name.is_empty() {
                        let Some(prev) = group.attrs.last_mut() else {
                            return Err(c.err("an additional value has no attribute to continue"));
                        };
                        if prev.values.len() >= MAX_VALUES {
                            return Err(c.err("too many values"));
                        }
                        prev.values.push(value);
                    } else {
                        let n = c.string(name, "attribute name")?;
                        if group.attrs.iter().any(|a| a.name == n) {
                            return Err(c.err(format!("attribute {n:?} appears twice in a group")));
                        }
                        group.attrs.push(Attr {
                            name: n,
                            values: vec![value],
                        });
                    }
                }
            }
        }
        Ok(Message {
            version,
            code,
            request_id,
            groups,
            data: buf[c.pos..].to_vec(),
        })
    }

    /// The first group with this tag.
    pub fn group(&self, t: u8) -> Option<&Group> {
        self.groups.iter().find(|g| g.tag == t)
    }

    /// Encode the message: header, groups, `end-of-attributes`, data.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(512 + self.data.len());
        out.push(self.version.0);
        out.push(self.version.1);
        out.extend_from_slice(&self.code.to_be_bytes());
        out.extend_from_slice(&self.request_id.to_be_bytes());
        for g in &self.groups {
            out.push(g.tag);
            for a in &g.attrs {
                encode_attr(&mut out, a);
            }
        }
        out.push(tag::END_OF_ATTRIBUTES);
        out.extend_from_slice(&self.data);
        out
    }
}

fn put_u16(out: &mut Vec<u8>, n: usize) {
    out.extend_from_slice(&u16::try_from(n).unwrap_or(u16::MAX).to_be_bytes());
}

fn encode_attr(out: &mut Vec<u8>, a: &Attr) {
    for (i, v) in a.values.iter().enumerate() {
        let name = if i == 0 { a.name.as_bytes() } else { &[][..] };
        encode_value(out, name, v);
    }
}

fn encode_value(out: &mut Vec<u8>, name: &[u8], v: &Value) {
    out.push(v.tag());
    put_u16(out, name.len());
    out.extend_from_slice(name);
    let body = |out: &mut Vec<u8>, b: &[u8]| {
        put_u16(out, b.len());
        out.extend_from_slice(b);
    };
    match v {
        Value::Integer(i) | Value::Enum(i) => body(out, &i.to_be_bytes()),
        Value::Boolean(b) => body(out, &[u8::from(*b)]),
        Value::OctetString(b) => body(out, b),
        Value::DateTime(d) => body(out, d),
        Value::Resolution { x, y, units } => {
            let mut b = Vec::with_capacity(9);
            b.extend_from_slice(&x.to_be_bytes());
            b.extend_from_slice(&y.to_be_bytes());
            b.push(*units);
            body(out, &b);
        }
        Value::Range { lower, upper } => {
            let mut b = Vec::with_capacity(8);
            b.extend_from_slice(&lower.to_be_bytes());
            b.extend_from_slice(&upper.to_be_bytes());
            body(out, &b);
        }
        Value::TextWithLanguage { language, text }
        | Value::NameWithLanguage {
            language,
            name: text,
        } => {
            let mut b = Vec::new();
            put_u16(&mut b, language.len());
            b.extend_from_slice(language.as_bytes());
            put_u16(&mut b, text.len());
            b.extend_from_slice(text.as_bytes());
            body(out, &b);
        }
        Value::Text(s)
        | Value::Name(s)
        | Value::Keyword(s)
        | Value::Uri(s)
        | Value::UriScheme(s)
        | Value::Charset(s)
        | Value::NaturalLanguage(s)
        | Value::MimeType(s) => body(out, s.as_bytes()),
        Value::OutOfBand(_) => body(out, &[]),
        Value::Raw { data, .. } => body(out, data),
        Value::Collection(members) => {
            body(out, &[]);
            for m in members {
                out.push(tag::MEMBER_ATTR_NAME);
                put_u16(out, 0);
                body(out, m.name.as_bytes());
                for mv in &m.values {
                    encode_value(out, &[], mv);
                }
            }
            out.push(tag::END_COLLECTION);
            put_u16(out, 0);
            put_u16(out, 0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg() -> Message {
        let mut m = Message::new((2, 0), op::PRINT_JOB, 7);
        let mut g = Group::new(tag::OPERATION_ATTRIBUTES);
        g.push(Attr::new(
            "attributes-charset",
            Value::Charset("utf-8".into()),
        ));
        g.push(Attr::new(
            "attributes-natural-language",
            Value::NaturalLanguage("en".into()),
        ));
        g.push(Attr::many(
            "requested-attributes",
            vec![Value::keyword("a"), Value::keyword("b")],
        ));
        m.groups.push(g);
        let mut j = Group::new(tag::JOB_ATTRIBUTES);
        j.push(Attr::int("copies", 3));
        j.push(Attr::new("printer-resolution", Value::dpi(600, 300)));
        j.push(Attr::new("x", Value::range(1, 9)));
        j.push(Attr::new(
            "media-col",
            Value::Collection(vec![
                Member {
                    name: "media-size".into(),
                    values: vec![Value::Collection(vec![
                        Member {
                            name: "x-dimension".into(),
                            values: vec![Value::Integer(21590)],
                        },
                        Member {
                            name: "y-dimension".into(),
                            values: vec![Value::Integer(27940)],
                        },
                    ])],
                },
                Member {
                    name: "media-type".into(),
                    values: vec![Value::keyword("stationery")],
                },
            ]),
        ));
        j.push(Attr::new(
            "job-name",
            Value::NameWithLanguage {
                language: "en".into(),
                name: "caf\u{e9}".into(),
            },
        ));
        j.push(Attr::new("flag", Value::Boolean(true)));
        j.push(Attr::new("none", Value::no_value()));
        m.groups.push(j);
        m.data = b"%PDF-1.4".to_vec();
        m
    }

    #[test]
    fn a_message_round_trips_through_the_encoding() {
        let m = msg();
        let bytes = m.encode();
        let back = Message::parse(&bytes).expect("parses");
        assert_eq!(back, m);
        assert_eq!(back.data, b"%PDF-1.4");
    }

    #[test]
    fn the_header_is_big_endian() {
        let bytes = msg().encode();
        assert_eq!(&bytes[..8], &[2, 0, 0, 2, 0, 0, 0, 7]);
        assert_eq!(bytes[8], tag::OPERATION_ATTRIBUTES);
    }

    #[test]
    fn strictness_matrix() {
        let ok = msg().encode();
        // truncation anywhere inside the attributes is refused (never a panic)
        for cut in 0..ok.len() - 8 {
            let r = Message::parse(&ok[..cut]);
            // cutting inside the document data is fine, anywhere before end-of-attributes is not
            if cut < ok.len() - 8 {
                assert!(r.is_err(), "cut at {cut} was accepted");
            }
        }
        let header = [2u8, 0, 0, 0x0b, 0, 0, 0, 1];
        let with = |tail: &[u8]| {
            let mut v = header.to_vec();
            v.extend_from_slice(tail);
            Message::parse(&v)
        };
        // no end-of-attributes
        assert!(with(&[tag::OPERATION_ATTRIBUTES]).is_err());
        // attribute before a group
        assert!(with(&[tag::INTEGER, 0, 1, b'a', 0, 4, 0, 0, 0, 1, 3]).is_err());
        // reserved delimiter
        assert!(with(&[0x00, 3]).is_err());
        assert!(with(&[0x0c, 3]).is_err());
        // wrong fixed sizes and a bad boolean
        assert!(with(&[1, tag::INTEGER, 0, 1, b'a', 0, 3, 0, 0, 1, 3]).is_err());
        assert!(with(&[1, tag::BOOLEAN, 0, 1, b'a', 0, 1, 2, 3]).is_err());
        assert!(with(&[1, tag::ENUM, 0, 1, b'a', 0, 5, 0, 0, 0, 0, 1, 3]).is_err());
        // continuation without an attribute
        assert!(with(&[1, tag::INTEGER, 0, 0, 0, 4, 0, 0, 0, 1, 3]).is_err());
        // duplicate attribute name in a group
        let dup = [
            1,
            tag::BOOLEAN,
            0,
            1,
            b'a',
            0,
            1,
            1,
            tag::BOOLEAN,
            0,
            1,
            b'a',
            0,
            1,
            0,
            3,
        ];
        assert!(with(&dup).is_err());
        // invalid UTF-8 in text, non-ASCII keyword
        assert!(with(&[
            1,
            tag::TEXT_WITHOUT_LANGUAGE,
            0,
            1,
            b'a',
            0,
            2,
            0xc3,
            0x28,
            3
        ])
        .is_err());
        assert!(with(&[1, tag::KEYWORD, 0, 1, b'a', 0, 1, 0xe9, 3]).is_err());
        // out-of-band with data
        assert!(with(&[1, tag::NO_VALUE, 0, 1, b'a', 0, 1, 0, 3]).is_err());
        // unclosed collection
        assert!(with(&[1, tag::BEG_COLLECTION, 0, 1, b'a', 0, 0, 3]).is_err());
        // short header
        assert!(Message::parse(&[2, 0, 0]).is_err());
        // an empty well-formed request parses and keeps the trailing data
        let m = with(&[1, 3, 9, 9]).unwrap();
        assert_eq!(m.data, [9, 9]);
        assert_eq!(m.groups.len(), 1);
    }

    #[test]
    fn errors_carry_the_header_for_the_reply() {
        let e = Message::parse(&[2, 1, 0, 4, 0, 0, 1, 2, 0x00]).unwrap_err();
        assert_eq!(e.version, Some((2, 1)));
        assert_eq!(e.request_id, Some(258));
        let e = Message::parse(&[2, 1]).unwrap_err();
        assert_eq!(e.request_id, None);
    }

    #[test]
    fn collections_nest_but_not_without_limit() {
        let mut v = Value::Collection(vec![]);
        for _ in 0..MAX_DEPTH + 2 {
            v = Value::Collection(vec![Member {
                name: "m".into(),
                values: vec![v],
            }]);
        }
        let mut m = Message::new((2, 0), 0x0b, 1);
        let mut g = Group::new(tag::OPERATION_ATTRIBUTES);
        g.push(Attr::new("c", v));
        m.groups.push(g);
        assert!(Message::parse(&m.encode()).is_err());
    }

    #[test]
    fn status_and_operation_tables() {
        assert_eq!(status_name(0x0406), "client-error-not-found");
        assert_eq!(status_from_str("server-error-busy"), Some(0x0507));
        assert_eq!(status_from_str("0x0404"), Some(0x0404));
        assert_eq!(status_from_str("1283"), Some(1283));
        assert_eq!(status_from_str("nonsense"), None);
        assert_eq!(op_name(op::GET_JOBS), Some("Get-Jobs"));
        assert_eq!(op_inject_name(op::SEND_DOCUMENT), "send_document");
        assert_eq!(op_inject_name(0x4001), "op_4001");
        assert!(is_success(1) && !is_success(0x0400));
        // names are unique
        let mut names: Vec<&str> = STATUS_NAMES.iter().map(|s| s.1).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), STATUS_NAMES.len());
    }

    #[test]
    fn json_rendering() {
        let m = msg();
        let j = m.groups[1].to_json();
        assert_eq!(j["copies"], 3);
        assert_eq!(j["media-col"]["media-size"]["x-dimension"], 21590);
        assert_eq!(j["printer-resolution"]["x"], 600);
        assert_eq!(j["none"], "no-value");
    }
}
