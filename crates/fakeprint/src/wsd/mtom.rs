//! MTOM/XOP: strict `multipart/related` parsing and building.
//!
//! Parsing is driven by the boundary in the `Content-Type` header (never by searching for
//! something that looks like a boundary), validates the boundary syntax, requires the closing
//! delimiter, rejects empty and duplicate-id parts, decodes each part's transfer encoding, and
//! resolves `xop:Include` references by `Content-ID`. Document bytes are returned untouched.
//! All sizes are capped by [`MtomLimits`]. Nothing here panics on hostile input.

use std::fmt;

use base64::alphabet;
use base64::engine::general_purpose::{GeneralPurpose, GeneralPurposeConfig};
use base64::engine::DecodePaddingMode;
use base64::Engine;

/// Why a multipart body or a content type was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MtomErrorKind {
    ContentType,
    Boundary,
    /// The closing delimiter or a part terminator is missing.
    Truncated,
    Header,
    Empty,
    DuplicateId,
    TooLarge,
    TooMany,
    Encoding,
    RootPart,
    /// An `xop:Include` that names no part, or a malformed `cid:` URI.
    Cid,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MtomError {
    pub kind: MtomErrorKind,
    pub message: String,
}

impl MtomError {
    fn new(kind: MtomErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

impl fmt::Display for MtomError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "mtom {:?}: {}", self.kind, self.message)
    }
}

impl std::error::Error for MtomError {}

type Result<T> = std::result::Result<T, MtomError>;

/// Caps for one multipart body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MtomLimits {
    /// Whole multipart body.
    pub max_body: usize,
    pub max_parts: usize,
    /// Bytes of header block per part.
    pub max_header_bytes: usize,
    /// Header lines per part.
    pub max_headers: usize,
    /// Decoded bytes per part.
    pub max_part: usize,
}

impl Default for MtomLimits {
    fn default() -> Self {
        Self {
            max_body: 256 << 20,
            max_parts: 32,
            max_header_bytes: 8 * 1024,
            max_headers: 24,
            max_part: 256 << 20,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Content-Type
// ---------------------------------------------------------------------------------------------

/// A parsed `Content-Type`: lowercase `type/subtype` plus parameters (names lowercased,
/// quoted values unquoted).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentType {
    pub essence: String,
    pub params: Vec<(String, String)>,
}

const MAX_CONTENT_TYPE: usize = 4096;

fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || "!#$%&'*+-.^_`|~".contains(c)
}

fn is_token(s: &str) -> bool {
    !s.is_empty() && s.chars().all(is_token_char)
}

impl ContentType {
    pub fn parse(raw: &str) -> Result<ContentType> {
        let bad = |m: &str| MtomError::new(MtomErrorKind::ContentType, m);
        if raw.len() > MAX_CONTENT_TYPE {
            return Err(bad("Content-Type too long"));
        }
        if raw.chars().any(|c| c == '\r' || c == '\n' || c == '\0') {
            return Err(bad("control character in Content-Type"));
        }
        let mut pieces = split_params(raw)?.into_iter();
        let essence = pieces
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        let (t, s) = essence
            .split_once('/')
            .ok_or_else(|| bad("media type needs type/subtype"))?;
        if !is_token(t) || !is_token(s) {
            return Err(bad("bad media type"));
        }
        let mut params = Vec::new();
        for piece in pieces {
            let piece = piece.trim();
            if piece.is_empty() {
                continue;
            }
            let (name, value) = piece
                .split_once('=')
                .ok_or_else(|| bad("parameter without '='"))?;
            let name = name.trim().to_ascii_lowercase();
            if !is_token(&name) {
                return Err(bad("bad parameter name"));
            }
            let value = unquote(value.trim())?;
            if params.iter().any(|(n, _)| n == &name) {
                return Err(bad("duplicate parameter"));
            }
            params.push((name, value));
        }
        Ok(ContentType { essence, params })
    }

    pub fn param(&self, name: &str) -> Option<&str> {
        self.params
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn charset(&self) -> Option<&str> {
        self.param("charset")
    }

    pub fn boundary(&self) -> Option<&str> {
        self.param("boundary")
    }

    pub fn is_multipart_related(&self) -> bool {
        self.essence == "multipart/related"
    }
}

/// Split on `;` outside double quotes.
fn split_params(raw: &str) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut escape = false;
    for c in raw.chars() {
        if quoted {
            cur.push(c);
            if escape {
                escape = false;
            } else if c == '\\' {
                escape = true;
            } else if c == '"' {
                quoted = false;
            }
        } else if c == '"' {
            quoted = true;
            cur.push(c);
        } else if c == ';' {
            out.push(std::mem::take(&mut cur));
        } else {
            cur.push(c);
        }
    }
    if quoted {
        return Err(MtomError::new(
            MtomErrorKind::ContentType,
            "unterminated quoted string",
        ));
    }
    out.push(cur);
    Ok(out)
}

fn unquote(v: &str) -> Result<String> {
    let Some(inner) = v.strip_prefix('"') else {
        if v.contains('"') {
            return Err(MtomError::new(MtomErrorKind::ContentType, "stray quote"));
        }
        return Ok(v.to_string());
    };
    let inner = inner
        .strip_suffix('"')
        .ok_or_else(|| MtomError::new(MtomErrorKind::ContentType, "unterminated quoted string"))?;
    let mut out = String::new();
    let mut escape = false;
    for c in inner.chars() {
        if escape {
            out.push(c);
            escape = false;
        } else if c == '\\' {
            escape = true;
        } else if c == '"' {
            return Err(MtomError::new(MtomErrorKind::ContentType, "stray quote"));
        } else {
            out.push(c);
        }
    }
    if escape {
        return Err(MtomError::new(
            MtomErrorKind::ContentType,
            "dangling escape",
        ));
    }
    Ok(out)
}

/// RFC 2046 boundary: 1 to 70 characters of the `bchars` set, not ending in a space.
pub fn valid_boundary(b: &str) -> bool {
    !b.is_empty()
        && b.len() <= 70
        && !b.ends_with(' ')
        && b.chars()
            .all(|c| c.is_ascii_alphanumeric() || "'()+_,-./:=? ".contains(c))
}

// ---------------------------------------------------------------------------------------------
// Parts
// ---------------------------------------------------------------------------------------------

/// One body part, transfer encoding already removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Part {
    /// Header lines (names lowercased, folded lines joined).
    pub headers: Vec<(String, String)>,
    /// `Content-ID` without the angle brackets.
    pub content_id: Option<String>,
    pub content_type: Option<ContentType>,
    pub data: Vec<u8>,
}

impl Part {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

/// A parsed `multipart/related` body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Multipart {
    pub parts: Vec<Part>,
    /// Index of the root (envelope) part.
    pub root: usize,
}

impl Multipart {
    /// Parse `body` as the `multipart/related` described by `ct`.
    pub fn parse(ct: &ContentType, body: &[u8], limits: &MtomLimits) -> Result<Multipart> {
        if !ct.is_multipart_related() {
            return Err(MtomError::new(
                MtomErrorKind::ContentType,
                "not multipart/related",
            ));
        }
        if let Some(t) = ct.param("type") {
            let t = t.to_ascii_lowercase();
            if t != "application/xop+xml" && t != "application/soap+xml" {
                return Err(MtomError::new(
                    MtomErrorKind::ContentType,
                    "multipart type is not application/xop+xml",
                ));
            }
        }
        let boundary = ct
            .boundary()
            .ok_or_else(|| MtomError::new(MtomErrorKind::Boundary, "no boundary parameter"))?;
        if !valid_boundary(boundary) {
            return Err(MtomError::new(MtomErrorKind::Boundary, "invalid boundary"));
        }
        let parts = split_parts(boundary, body, limits)?;
        let root = match ct.param("start") {
            Some(start) => {
                let want = normalize_cid(start);
                parts
                    .iter()
                    .position(|p| p.content_id.as_deref() == Some(want.as_str()))
                    .ok_or_else(|| MtomError::new(MtomErrorKind::RootPart, "start names no part"))?
            }
            None => 0,
        };
        let root_part = &parts[root];
        match root_part.content_type.as_ref().map(|c| c.essence.as_str()) {
            Some("application/xop+xml") | Some("application/soap+xml") => {}
            _ => {
                return Err(MtomError::new(
                    MtomErrorKind::RootPart,
                    "root part is not application/xop+xml",
                ))
            }
        }
        Ok(Multipart { parts, root })
    }

    pub fn root_part(&self) -> &Part {
        &self.parts[self.root]
    }

    /// The part with this Content-ID (brackets optional).
    pub fn by_content_id(&self, cid: &str) -> Option<&Part> {
        let want = normalize_cid(cid);
        self.parts
            .iter()
            .find(|p| p.content_id.as_deref() == Some(want.as_str()))
    }

    /// Resolve an `xop:Include/@href` (`cid:` URI, percent-encoded) to its part.
    pub fn resolve_href(&self, href: &str) -> Result<&Part> {
        let cid = cid_from_href(href)?;
        self.by_content_id(&cid)
            .ok_or_else(|| MtomError::new(MtomErrorKind::Cid, format!("no part for cid {cid}")))
    }
}

/// `Content-ID` value without whitespace and angle brackets.
pub fn normalize_cid(raw: &str) -> String {
    let t = raw.trim();
    let t = t.strip_prefix('<').unwrap_or(t);
    let t = t.strip_suffix('>').unwrap_or(t);
    t.trim().to_string()
}

/// The content id named by a `cid:` URI.
pub fn cid_from_href(href: &str) -> Result<String> {
    let href = href.trim();
    let rest = if href.len() >= 4 && href[..4].eq_ignore_ascii_case("cid:") {
        &href[4..]
    } else {
        return Err(MtomError::new(MtomErrorKind::Cid, "href is not a cid: URI"));
    };
    let decoded = percent_decode(rest)
        .ok_or_else(|| MtomError::new(MtomErrorKind::Cid, "bad percent-encoding in cid"))?;
    let cid = normalize_cid(&decoded);
    if cid.is_empty() {
        return Err(MtomError::new(MtomErrorKind::Cid, "empty cid"));
    }
    Ok(cid)
}

/// `cid:` URI for a content id, percent-encoding anything outside the unreserved set and `@`.
pub fn cid_href(cid: &str) -> String {
    let mut out = String::from("cid:");
    for b in cid.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~@".contains(&b) {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn percent_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = s.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

// ---------------------------------------------------------------------------------------------
// Splitting
// ---------------------------------------------------------------------------------------------

fn find(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || from >= hay.len() || hay.len() - from < needle.len() {
        return None;
    }
    let first = needle[0];
    let last_start = hay.len() - needle.len();
    let mut i = from;
    while i <= last_start {
        let off = hay[i..=last_start].iter().position(|b| *b == first)?;
        i += off;
        if &hay[i..i + needle.len()] == needle {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// After a delimiter at `end` (index just past it): `Some(true)` when it is the closing
/// delimiter (`--`), `Some(false)` when followed by optional padding then CRLF, with the
/// index of the first byte after that CRLF; `None` when neither (not a real delimiter).
fn delimiter_tail(body: &[u8], end: usize) -> Option<(bool, usize)> {
    if body[end..].starts_with(b"--") {
        return Some((true, end + 2));
    }
    let mut i = end;
    while i < body.len() && (body[i] == b' ' || body[i] == b'\t') {
        i += 1;
    }
    if body[i..].starts_with(b"\r\n") {
        Some((false, i + 2))
    } else {
        None
    }
}

fn split_parts(boundary: &str, body: &[u8], limits: &MtomLimits) -> Result<Vec<Part>> {
    if body.len() > limits.max_body {
        return Err(MtomError::new(
            MtomErrorKind::TooLarge,
            "multipart body too large",
        ));
    }
    let delim: Vec<u8> = [b"--".as_slice(), boundary.as_bytes()].concat();
    let line_delim: Vec<u8> = [b"\r\n".as_slice(), &delim].concat();

    // first delimiter: at the very start or after a CRLF (preamble is skipped)
    let mut at = None;
    if body.starts_with(&delim) {
        if let Some((_, _)) = delimiter_tail(body, delim.len()) {
            at = Some(delim.len());
        }
    }
    if at.is_none() {
        let mut from = 0;
        while let Some(i) = find(body, &line_delim, from) {
            let end = i + line_delim.len();
            if delimiter_tail(body, end).is_some() {
                at = Some(end);
                break;
            }
            from = i + 1;
        }
    }
    let mut end_of_delim =
        at.ok_or_else(|| MtomError::new(MtomErrorKind::Boundary, "boundary not found in body"))?;

    let mut parts: Vec<Part> = Vec::new();
    loop {
        let (closing, content_start) = delimiter_tail(body, end_of_delim)
            .ok_or_else(|| MtomError::new(MtomErrorKind::Truncated, "bad delimiter line"))?;
        if closing {
            break;
        }
        if parts.len() >= limits.max_parts {
            return Err(MtomError::new(MtomErrorKind::TooMany, "too many parts"));
        }
        // header block
        let (header_text, data_start) = if body[content_start..].starts_with(b"\r\n") {
            ("", content_start + 2)
        } else {
            let hdr_end = find(body, b"\r\n\r\n", content_start).ok_or_else(|| {
                MtomError::new(MtomErrorKind::Truncated, "part headers not terminated")
            })?;
            if hdr_end - content_start > limits.max_header_bytes {
                return Err(MtomError::new(
                    MtomErrorKind::TooLarge,
                    "part headers too large",
                ));
            }
            let text = std::str::from_utf8(&body[content_start..hdr_end])
                .map_err(|_| MtomError::new(MtomErrorKind::Header, "part headers are not UTF-8"))?;
            (text, hdr_end + 4)
        };
        // data: up to the next real delimiter. The CRLF before a delimiter belongs to the
        // delimiter, so an empty part has its delimiter right at `data_start`.
        let (data_end, next_end) = if body[data_start..].starts_with(&delim)
            && delimiter_tail(body, data_start + delim.len()).is_some()
        {
            (data_start, data_start + delim.len())
        } else {
            let mut from = data_start;
            loop {
                match find(body, &line_delim, from) {
                    None => {
                        return Err(MtomError::new(
                            MtomErrorKind::Truncated,
                            "closing delimiter missing",
                        ))
                    }
                    Some(i) => {
                        let e = i + line_delim.len();
                        if delimiter_tail(body, e).is_some() {
                            break (i, e);
                        }
                        from = i + 1;
                    }
                }
            }
        };
        let raw = &body[data_start..data_end];
        if raw.len() > limits.max_part {
            return Err(MtomError::new(MtomErrorKind::TooLarge, "part too large"));
        }
        let part = make_part(header_text, raw, limits)?;
        if let Some(id) = &part.content_id {
            if parts
                .iter()
                .any(|p| p.content_id.as_deref() == Some(id.as_str()))
            {
                return Err(MtomError::new(
                    MtomErrorKind::DuplicateId,
                    format!("duplicate Content-ID {id}"),
                ));
            }
        }
        parts.push(part);
        end_of_delim = next_end;
    }
    if parts.is_empty() {
        return Err(MtomError::new(MtomErrorKind::Empty, "no parts"));
    }
    Ok(parts)
}

fn make_part(header_text: &str, raw: &[u8], limits: &MtomLimits) -> Result<Part> {
    let bad = |m: &str| MtomError::new(MtomErrorKind::Header, m);
    // unfold
    let mut lines: Vec<String> = Vec::new();
    if !header_text.is_empty() {
        for line in header_text.split("\r\n") {
            if line.contains('\r') || line.contains('\n') || line.contains('\0') {
                return Err(bad("bare CR or LF in part headers"));
            }
            if line.starts_with(' ') || line.starts_with('\t') {
                match lines.last_mut() {
                    Some(prev) => {
                        prev.push(' ');
                        prev.push_str(line.trim());
                    }
                    None => return Err(bad("continuation line without a header")),
                }
            } else {
                lines.push(line.to_string());
            }
        }
    }
    if lines.len() > limits.max_headers {
        return Err(MtomError::new(
            MtomErrorKind::TooMany,
            "too many part headers",
        ));
    }
    let mut headers: Vec<(String, String)> = Vec::new();
    for line in lines {
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| bad("header without ':'"))?;
        let name = name.to_ascii_lowercase();
        if !is_token(&name) {
            return Err(bad("bad header name"));
        }
        if matches!(
            name.as_str(),
            "content-id" | "content-type" | "content-transfer-encoding"
        ) && headers.iter().any(|(n, _)| n == &name)
        {
            return Err(bad("duplicate header"));
        }
        headers.push((name, value.trim().to_string()));
    }
    let get = |n: &str| {
        headers
            .iter()
            .find(|(h, _)| h == n)
            .map(|(_, v)| v.as_str())
    };
    let content_id = match get("content-id") {
        Some(v) => {
            let id = normalize_cid(v);
            if id.is_empty()
                || id
                    .chars()
                    .any(|c| c.is_whitespace() || c == '<' || c == '>')
            {
                return Err(bad("bad Content-ID"));
            }
            Some(id)
        }
        None => None,
    };
    let content_type = match get("content-type") {
        Some(v) => Some(ContentType::parse(v)?),
        None => None,
    };
    let encoding = get("content-transfer-encoding")
        .map(|v| v.trim().to_ascii_lowercase())
        .unwrap_or_else(|| "binary".to_string());
    let data = match encoding.as_str() {
        "binary" | "8bit" | "7bit" => raw.to_vec(),
        "base64" => decode_base64_bytes(raw, limits.max_part)?,
        other => {
            return Err(MtomError::new(
                MtomErrorKind::Encoding,
                format!("unsupported Content-Transfer-Encoding {other}"),
            ))
        }
    };
    if data.is_empty() {
        return Err(MtomError::new(MtomErrorKind::Empty, "empty part"));
    }
    Ok(Part {
        headers,
        content_id,
        content_type,
        data,
    })
}

// ---------------------------------------------------------------------------------------------
// base64
// ---------------------------------------------------------------------------------------------

const BASE64: GeneralPurpose = GeneralPurpose::new(
    &alphabet::STANDARD,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

fn decode_base64_bytes(raw: &[u8], max: usize) -> Result<Vec<u8>> {
    let cleaned: Vec<u8> = raw
        .iter()
        .copied()
        .filter(|b| !matches!(b, b' ' | b'\t' | b'\r' | b'\n'))
        .collect();
    if cleaned.len() / 4 * 3 > max.saturating_add(3) {
        return Err(MtomError::new(
            MtomErrorKind::TooLarge,
            "decoded part too large",
        ));
    }
    BASE64
        .decode(&cleaned)
        .map_err(|e| MtomError::new(MtomErrorKind::Encoding, format!("bad base64: {e}")))
}

/// Decode `xs:base64Binary` text (whitespace allowed, padding optional), refusing results
/// larger than `max` bytes.
pub fn decode_base64(text: &str, max: usize) -> Result<Vec<u8>> {
    decode_base64_bytes(text.as_bytes(), max)
}

/// Encode as standard padded base64.
pub fn encode_base64(data: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(data)
}

// ---------------------------------------------------------------------------------------------
// Building
// ---------------------------------------------------------------------------------------------

/// An attachment of an outgoing message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutPart {
    /// Content-ID without brackets.
    pub content_id: String,
    pub content_type: String,
    pub data: Vec<u8>,
}

/// A finished multipart reply: `content_type` for the HTTP header, `body` for the payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuiltMultipart {
    pub content_type: String,
    pub body: Vec<u8>,
}

/// Build `multipart/related; type="application/xop+xml"` with the envelope as root part.
/// `action` is added to the root part's content type (SOAP 1.2 `action` parameter).
pub fn build_multipart(
    root_xml: &[u8],
    root_cid: &str,
    action: Option<&str>,
    attachments: &[OutPart],
) -> Result<BuiltMultipart> {
    let cid_ok = |c: &str| {
        !c.is_empty()
            && c.chars()
                .all(|ch| ch.is_ascii_graphic() && ch != '<' && ch != '>')
    };
    if !cid_ok(root_cid) || attachments.iter().any(|a| !cid_ok(&a.content_id)) {
        return Err(MtomError::new(MtomErrorKind::Header, "bad Content-ID"));
    }
    for att in attachments {
        ContentType::parse(&att.content_type)?;
        if att.data.is_empty() {
            return Err(MtomError::new(MtomErrorKind::Empty, "empty attachment"));
        }
    }
    let mut boundary = String::new();
    for _ in 0..8 {
        let candidate = format!("uuid:{}", uuid::Uuid::new_v4());
        let marker = format!("--{candidate}");
        let clash = find(root_xml, marker.as_bytes(), 0).is_some()
            || attachments
                .iter()
                .any(|a| find(&a.data, marker.as_bytes(), 0).is_some());
        if !clash {
            boundary = candidate;
            break;
        }
    }
    if boundary.is_empty() {
        return Err(MtomError::new(MtomErrorKind::Boundary, "no free boundary"));
    }
    let mut body: Vec<u8> = Vec::with_capacity(
        root_xml.len()
            + attachments
                .iter()
                .map(|a| a.data.len() + 200)
                .sum::<usize>()
            + 400,
    );
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    let mut root_ct =
        String::from("application/xop+xml; charset=utf-8; type=\"application/soap+xml\"");
    if let Some(a) = action {
        root_ct.push_str("; action=\"");
        root_ct.push_str(&a.replace('\\', "\\\\").replace('"', "\\\""));
        root_ct.push('"');
    }
    body.extend_from_slice(format!("Content-Type: {root_ct}\r\n").as_bytes());
    body.extend_from_slice(b"Content-Transfer-Encoding: binary\r\n");
    body.extend_from_slice(format!("Content-ID: <{root_cid}>\r\n\r\n").as_bytes());
    body.extend_from_slice(root_xml);
    for att in attachments {
        body.extend_from_slice(format!("\r\n--{boundary}\r\n").as_bytes());
        body.extend_from_slice(format!("Content-Type: {}\r\n", att.content_type).as_bytes());
        body.extend_from_slice(b"Content-Transfer-Encoding: binary\r\n");
        body.extend_from_slice(format!("Content-ID: <{}>\r\n\r\n", att.content_id).as_bytes());
        body.extend_from_slice(&att.data);
    }
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    let content_type = format!(
        "multipart/related; type=\"application/xop+xml\"; boundary=\"{boundary}\"; start=\"<{root_cid}>\"; start-info=\"application/soap+xml\""
    );
    Ok(BuiltMultipart { content_type, body })
}

#[cfg(test)]
mod tests {
    use super::*;

    const CT: &str =
        "multipart/related; type=\"application/xop+xml\"; boundary=\"uuid:b1\"; start=\"<root@x>\"";

    fn pkg(xml: &str, parts: &[(&str, &[u8])]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(b"--uuid:b1\r\nContent-Type: application/xop+xml; charset=utf-8; type=\"application/soap+xml\"\r\nContent-ID: <root@x>\r\n\r\n");
        b.extend_from_slice(xml.as_bytes());
        for (cid, data) in parts {
            b.extend_from_slice(format!("\r\n--uuid:b1\r\nContent-Type: application/octet-stream\r\nContent-Transfer-Encoding: binary\r\nContent-ID: <{cid}>\r\n\r\n").as_bytes());
            b.extend_from_slice(data);
        }
        b.extend_from_slice(b"\r\n--uuid:b1--\r\n");
        b
    }

    fn parse(ct: &str, body: &[u8]) -> Result<Multipart> {
        Multipart::parse(&ContentType::parse(ct)?, body, &MtomLimits::default())
    }

    #[test]
    fn content_type_parameters() {
        let ct = ContentType::parse("Multipart/Related; Boundary=\"a;b\\\"c\"; type=x").unwrap();
        assert_eq!(ct.essence, "multipart/related");
        assert_eq!(ct.boundary(), Some("a;b\"c"));
        assert!(ContentType::parse("nonsense").is_err());
        assert!(ContentType::parse("a/b; x=\"open").is_err());
        assert!(ContentType::parse("a/b; x=1; x=2").is_err());
        assert!(ContentType::parse("a/b\r\nX: y").is_err());
        assert_eq!(
            ContentType::parse("text/xml; charset=ISO-8859-1")
                .unwrap()
                .charset(),
            Some("ISO-8859-1")
        );
    }

    #[test]
    fn parses_binary_parts_untouched() {
        let data: Vec<u8> = (0..=255u8).chain(*b"\r\n--uuid:b1x\r\n").collect();
        let mp = parse(CT, &pkg("<e/>", &[("doc@x", &data)])).unwrap();
        assert_eq!(mp.parts.len(), 2);
        assert_eq!(mp.root_part().data, b"<e/>");
        assert_eq!(mp.by_content_id("<doc@x>").unwrap().data, data);
        assert_eq!(mp.resolve_href("cid:doc@x").unwrap().data, data);
        assert_eq!(mp.resolve_href("CID:doc%40x").unwrap().data, data);
    }

    #[test]
    fn cid_mismatch_and_bad_hrefs() {
        let mp = parse(CT, &pkg("<e/>", &[("doc@x", b"abc")])).unwrap();
        assert_eq!(
            mp.resolve_href("cid:other@x").unwrap_err().kind,
            MtomErrorKind::Cid
        );
        assert_eq!(
            mp.resolve_href("http://x").unwrap_err().kind,
            MtomErrorKind::Cid
        );
        assert_eq!(
            mp.resolve_href("cid:%zz").unwrap_err().kind,
            MtomErrorKind::Cid
        );
        assert_eq!(
            mp.resolve_href("cid:").unwrap_err().kind,
            MtomErrorKind::Cid
        );
    }

    #[test]
    fn truncated_and_hostile_bodies() {
        let good = pkg("<e/>", &[("d", b"abc")]);
        for cut in 0..good.len() {
            let _ = parse(CT, &good[..cut]);
        }
        assert!(parse(CT, &good[..good.len() - 4]).is_err());
        assert!(parse(CT, b"no boundary here").is_err());
        assert!(parse(CT, b"").is_err());
        let mp = parse(CT, &pkg("<e/>", &[("d", b"x\r\n--uuid:b1junk\r\ny")])).unwrap();
        assert_eq!(mp.parts[1].data, b"x\r\n--uuid:b1junk\r\ny");
    }

    #[test]
    fn boundary_injection_and_syntax() {
        for bad in [
            "multipart/related; boundary=\"a\r\nb\"",
            "multipart/related; boundary=\"\"",
            "multipart/related",
        ] {
            let r = ContentType::parse(bad)
                .and_then(|c| Multipart::parse(&c, b"--a\r\n", &MtomLimits::default()));
            assert!(r.is_err(), "{bad}");
        }
        let long = format!("multipart/related; boundary={}", "a".repeat(71));
        assert_eq!(
            parse(&long, b"--x").unwrap_err().kind,
            MtomErrorKind::Boundary
        );
        assert!(!valid_boundary("a b "));
        assert!(!valid_boundary("a\"b"));
        assert!(valid_boundary("uuid:1234-ab+c"));
    }

    #[test]
    fn rejects_empty_duplicate_and_unknown_encodings() {
        assert_eq!(
            parse(CT, &pkg("<e/>", &[("d", b"")])).unwrap_err().kind,
            MtomErrorKind::Empty
        );
        assert_eq!(
            parse(CT, &pkg("<e/>", &[("d", b"a"), ("d", b"b")]))
                .unwrap_err()
                .kind,
            MtomErrorKind::DuplicateId
        );
        let qp = b"--uuid:b1\r\nContent-Type: application/xop+xml\r\nContent-ID: <root@x>\r\nContent-Transfer-Encoding: quoted-printable\r\n\r\nabc\r\n--uuid:b1--\r\n";
        assert_eq!(parse(CT, qp).unwrap_err().kind, MtomErrorKind::Encoding);
        let b64 = b"--uuid:b1\r\nContent-Type: application/xop+xml\r\nContent-ID: <root@x>\r\nContent-Transfer-Encoding: base64\r\n\r\nPGUv\r\nPg==\r\n--uuid:b1--\r\n";
        assert_eq!(parse(CT, b64).unwrap().root_part().data, b"<e/>");
        let bad64 = b"--uuid:b1\r\nContent-Type: application/xop+xml\r\nContent-ID: <root@x>\r\nContent-Transfer-Encoding: base64\r\n\r\n!!!!\r\n--uuid:b1--\r\n";
        assert_eq!(parse(CT, bad64).unwrap_err().kind, MtomErrorKind::Encoding);
    }

    #[test]
    fn root_part_selection_and_type() {
        let ct = "multipart/related; boundary=\"uuid:b1\"; start=\"<nope>\"";
        assert_eq!(
            parse(ct, &pkg("<e/>", &[])).unwrap_err().kind,
            MtomErrorKind::RootPart
        );
        let body = b"--uuid:b1\r\nContent-Type: image/png\r\n\r\nxx\r\n--uuid:b1--\r\n";
        assert_eq!(
            parse("multipart/related; boundary=\"uuid:b1\"", body)
                .unwrap_err()
                .kind,
            MtomErrorKind::RootPart
        );
        let wrong = "multipart/related; type=\"text/html\"; boundary=\"uuid:b1\"";
        assert_eq!(
            parse(wrong, &pkg("<e/>", &[])).unwrap_err().kind,
            MtomErrorKind::ContentType
        );
        assert!(parse("multipart/mixed; boundary=\"uuid:b1\"", &pkg("<e/>", &[])).is_err());
    }

    #[test]
    fn limits_are_enforced() {
        let ctype = ContentType::parse(CT).unwrap();
        let l = MtomLimits {
            max_parts: 1,
            ..MtomLimits::default()
        };
        let r = Multipart::parse(&ctype, &pkg("<e/>", &[("d", b"a")]), &l);
        assert_eq!(r.unwrap_err().kind, MtomErrorKind::TooMany);
        let l = MtomLimits {
            max_part: 2,
            ..MtomLimits::default()
        };
        let r = Multipart::parse(&ctype, &pkg("<e/>", &[("d", b"abc")]), &l);
        assert_eq!(r.unwrap_err().kind, MtomErrorKind::TooLarge);
        let l = MtomLimits {
            max_body: 10,
            ..MtomLimits::default()
        };
        assert!(Multipart::parse(&ctype, &pkg("<e/>", &[]), &l).is_err());
        let many: String = (0..40).map(|i| format!("X-{i}: v\r\n")).collect();
        let body = format!("--uuid:b1\r\nContent-Type: application/xop+xml\r\nContent-ID: <root@x>\r\n{many}\r\nabc\r\n--uuid:b1--\r\n");
        assert_eq!(
            parse(CT, body.as_bytes()).unwrap_err().kind,
            MtomErrorKind::TooMany
        );
    }

    #[test]
    fn build_and_parse_round_trip() {
        let doc: Vec<u8> = (0..1000u32).map(|i| (i % 251) as u8).collect();
        let built = build_multipart(
            b"<e/>",
            "root@y",
            Some("urn:act"),
            &[OutPart {
                content_id: "d1@y".into(),
                content_type: "application/octet-stream".into(),
                data: doc.clone(),
            }],
        )
        .unwrap();
        let ct = ContentType::parse(&built.content_type).unwrap();
        let mp = Multipart::parse(&ct, &built.body, &MtomLimits::default()).unwrap();
        assert_eq!(mp.root_part().data, b"<e/>");
        assert_eq!(
            mp.root_part()
                .content_type
                .as_ref()
                .unwrap()
                .param("action"),
            Some("urn:act")
        );
        assert_eq!(mp.resolve_href(&cid_href("d1@y")).unwrap().data, doc);
        assert!(build_multipart(b"<e/>", "a b", None, &[]).is_err());
        let empty = OutPart {
            content_id: "x".into(),
            content_type: "a/b".into(),
            data: vec![],
        };
        assert!(build_multipart(b"<e/>", "r", None, &[empty]).is_err());
    }

    #[test]
    fn base64_inline() {
        assert_eq!(decode_base64("JVBERi0=", 100).unwrap(), b"%PDF-");
        assert_eq!(decode_base64("JVBE\n Ri0", 100).unwrap(), b"%PDF-");
        assert!(decode_base64("JVBERi0=", 2).is_err());
        assert!(decode_base64("@@@", 10).is_err());
        assert_eq!(cid_href("a b@c"), "cid:a%20b@c");
    }
}
