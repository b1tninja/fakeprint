//! Namespace-aware XML element tree for the WSD layer.
//!
//! One parse path (`parse`, `parse_str`) and one serialize path (`Element::to_xml`).
//! The parser is deliberately small and hostile-input safe:
//!
//! * DOCTYPE declarations are refused, so no entity can expand (no billion laughs);
//!   only the five predefined entities and numeric character references are decoded.
//! * Input size, depth, element count, attribute count and name/value lengths are capped
//!   by [`XmlLimits`]; nothing is allocated before the size check.
//! * Namespaces are resolved while parsing; an unbound prefix is an error.
//! * Control characters that XML 1.0 forbids are rejected.
//! * Never panics: every failure is an [`XmlError`].
//!
//! Serialization declares every namespace once on the root element, using the preferred
//! prefix from a [`PrefixMap`] (or a generated `nsN`), and never uses a default namespace.

use std::borrow::Cow;
use std::fmt;
use std::sync::Arc;

use quick_xml::events::{BytesStart, Event};
use quick_xml::reader::Reader;

/// Namespace URIs used by the WSD stack.
pub mod ns {
    pub const XML: &str = "http://www.w3.org/XML/1998/namespace";
    pub const XMLNS: &str = "http://www.w3.org/2000/xmlns/";
    pub const SOAP12: &str = "http://www.w3.org/2003/05/soap-envelope";
    pub const SOAP11: &str = "http://schemas.xmlsoap.org/soap/envelope/";
    pub const WSA: &str = "http://schemas.xmlsoap.org/ws/2004/08/addressing";
    pub const WSE: &str = "http://schemas.xmlsoap.org/ws/2004/08/eventing";
    pub const WXF: &str = "http://schemas.xmlsoap.org/ws/2004/09/transfer";
    pub const MEX: &str = "http://schemas.xmlsoap.org/ws/2004/09/mex";
    pub const WSD: &str = "http://schemas.xmlsoap.org/ws/2005/04/discovery";
    pub const WSDP: &str = "http://schemas.xmlsoap.org/ws/2006/02/devprof";
    pub const PRINT: &str = "http://schemas.microsoft.com/windows/2006/08/wdp/print";
    pub const PRINT11: &str = "http://schemas.microsoft.com/windows/2010/06/wdp/printv11";
    pub const PRINT12: &str = "http://schemas.microsoft.com/windows/2012/10/wdp/printV12";
    pub const PRINT20: &str = "http://schemas.microsoft.com/windows/2014/04/wdp/printV20";
    pub const XOP: &str = "http://www.w3.org/2004/08/xop/include";
    pub const XSI: &str = "http://www.w3.org/2001/XMLSchema-instance";
}

// ---------------------------------------------------------------------------------------------
// Errors and limits
// ---------------------------------------------------------------------------------------------

/// Why a document was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XmlErrorKind {
    /// Input or a text/attribute value is larger than the limit.
    TooLarge,
    /// Nesting deeper than the limit.
    TooDeep,
    /// Too many elements, attributes or namespace declarations.
    TooMany,
    /// A DOCTYPE (and therefore any entity definition) was present.
    Dtd,
    /// Not well-formed.
    Syntax,
    /// Bad byte encoding or unsupported charset.
    Encoding,
    /// Unbound prefix, reserved prefix misuse, duplicate expanded attribute name.
    Namespace,
    /// Unknown entity or character reference to a forbidden character.
    Entity,
    /// More than one root element, or text after the root.
    Trailing,
    /// No element at all.
    Empty,
}

/// A refused document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XmlError {
    pub kind: XmlErrorKind,
    pub message: String,
    /// Byte offset in the decoded text when known.
    pub offset: Option<usize>,
}

impl XmlError {
    fn new(kind: XmlErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            offset: None,
        }
    }
    fn at(mut self, offset: u64) -> Self {
        self.offset = usize::try_from(offset).ok();
        self
    }
}

impl fmt::Display for XmlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "xml {:?}: {}", self.kind, self.message)?;
        if let Some(at) = self.offset {
            write!(f, " (offset {at})")?;
        }
        Ok(())
    }
}

impl std::error::Error for XmlError {}

/// Resource caps for one parse. Every field is enforced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct XmlLimits {
    /// Bytes of input accepted (before decoding).
    pub max_input: usize,
    /// Element nesting depth (root is depth 1).
    pub max_depth: usize,
    /// Total elements.
    pub max_nodes: usize,
    /// Attributes (excluding namespace declarations) on one element.
    pub max_attrs: usize,
    /// Namespace declarations on one element.
    pub max_ns_decls: usize,
    /// Bytes in an element or attribute name.
    pub max_name: usize,
    /// Bytes in one attribute value.
    pub max_attr_value: usize,
    /// Bytes of text directly inside one element.
    pub max_text: usize,
}

impl XmlLimits {
    /// Control messages (envelopes without inline documents).
    pub const SMALL: XmlLimits = XmlLimits {
        max_input: 1 << 20,
        max_depth: 64,
        max_nodes: 20_000,
        max_attrs: 32,
        max_ns_decls: 32,
        max_name: 256,
        max_attr_value: 16 * 1024,
        max_text: 1 << 20,
    };
    /// Envelopes that may carry an inline base64 document.
    pub const INLINE_DOCUMENT: XmlLimits = XmlLimits {
        max_input: 96 << 20,
        max_depth: 64,
        max_nodes: 20_000,
        max_attrs: 32,
        max_ns_decls: 32,
        max_name: 256,
        max_attr_value: 16 * 1024,
        max_text: 96 << 20,
    };
}

impl Default for XmlLimits {
    fn default() -> Self {
        Self::SMALL
    }
}

// ---------------------------------------------------------------------------------------------
// Tree types
// ---------------------------------------------------------------------------------------------

/// Expanded name. An empty `ns` means "no namespace".
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct QName {
    pub ns: String,
    pub local: String,
}

impl QName {
    pub fn new(ns: &str, local: &str) -> Self {
        Self {
            ns: ns.to_string(),
            local: local.to_string(),
        }
    }
    /// A name in no namespace.
    pub fn unqualified(local: &str) -> Self {
        Self::new("", local)
    }
    pub fn is(&self, ns: &str, local: &str) -> bool {
        self.ns == ns && self.local == local
    }
}

impl fmt::Display for QName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.ns.is_empty() {
            write!(f, "{}", self.local)
        } else {
            write!(f, "{{{}}}{}", self.ns, self.local)
        }
    }
}

/// An attribute. `qvalue` is set when the value is a QName (serialized as `prefix:local`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attr {
    pub name: QName,
    pub value: String,
    pub qvalue: Option<QName>,
}

#[derive(Debug)]
struct Scope {
    parent: Option<Arc<Scope>>,
    /// `(prefix, uri)`; prefix `""` is the default namespace; an empty uri un-declares.
    decls: Vec<(String, String)>,
}

impl Scope {
    fn lookup(self: &Arc<Self>, prefix: &str) -> Option<String> {
        let mut cur: Option<&Arc<Scope>> = Some(self);
        while let Some(scope) = cur {
            if let Some((_, uri)) = scope.decls.iter().rev().find(|(p, _)| p == prefix) {
                return if uri.is_empty() {
                    None
                } else {
                    Some(uri.clone())
                };
            }
            cur = scope.parent.as_ref();
        }
        None
    }
}

/// An element. Text is the concatenation of the element's direct character data; when an
/// element has children, whitespace-only text is dropped.
#[derive(Debug, Clone)]
pub struct Element {
    pub name: QName,
    pub attrs: Vec<Attr>,
    pub children: Vec<Element>,
    pub text: String,
    /// When set, the text is this QName (serialized with a declared prefix).
    pub qtext: Option<QName>,
    /// Namespace declarations written on this element in the source (`prefix`, `uri`).
    pub declared: Vec<(String, String)>,
    scope: Option<Arc<Scope>>,
}

impl PartialEq for Element {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
            && self.attrs == other.attrs
            && self.children == other.children
            && self.text == other.text
            && self.qtext == other.qtext
    }
}
impl Eq for Element {}

impl Element {
    /// Element named `local` in namespace `ns` (`""` for none).
    pub fn new(ns: &str, local: &str) -> Self {
        Self::from_qname(QName::new(ns, local))
    }

    pub fn from_qname(name: QName) -> Self {
        Self {
            name,
            attrs: Vec::new(),
            children: Vec::new(),
            text: String::new(),
            qtext: None,
            declared: Vec::new(),
            scope: None,
        }
    }

    /// `<ns:local>text</ns:local>`.
    pub fn text_el(ns: &str, local: &str, text: impl Into<String>) -> Self {
        Self::new(ns, local).with_text(text)
    }

    pub fn with_text(mut self, text: impl Into<String>) -> Self {
        self.text = text.into();
        self
    }

    /// Text that is a QName; the serializer declares and uses a prefix for it.
    pub fn with_qtext(mut self, q: QName) -> Self {
        self.qtext = Some(q);
        self
    }

    /// Unqualified attribute.
    pub fn with_attr(mut self, local: &str, value: impl Into<String>) -> Self {
        self.attrs.push(Attr {
            name: QName::unqualified(local),
            value: value.into(),
            qvalue: None,
        });
        self
    }

    /// Namespace-qualified attribute.
    pub fn with_attr_ns(mut self, name: QName, value: impl Into<String>) -> Self {
        self.attrs.push(Attr {
            name,
            value: value.into(),
            qvalue: None,
        });
        self
    }

    /// Unqualified attribute whose value is a QName.
    pub fn with_attr_qname(mut self, local: &str, value: QName) -> Self {
        self.attrs.push(Attr {
            name: QName::unqualified(local),
            value: String::new(),
            qvalue: Some(value),
        });
        self
    }

    pub fn with_child(mut self, child: Element) -> Self {
        self.children.push(child);
        self
    }

    pub fn with_children(mut self, kids: impl IntoIterator<Item = Element>) -> Self {
        self.children.extend(kids);
        self
    }

    pub fn is(&self, ns: &str, local: &str) -> bool {
        self.name.is(ns, local)
    }

    /// First direct child with this name.
    pub fn find(&self, name: &QName) -> Option<&Element> {
        self.children.iter().find(|c| &c.name == name)
    }

    /// Like [`Element::find`] taking namespace and local name.
    pub fn child(&self, ns: &str, local: &str) -> Option<&Element> {
        self.children.iter().find(|c| c.name.is(ns, local))
    }

    /// All direct children with this name.
    pub fn find_all<'a>(&'a self, name: &'a QName) -> impl Iterator<Item = &'a Element> + 'a {
        self.children.iter().filter(move |c| &c.name == name)
    }

    /// Follow a path of child names from this element.
    pub fn path(&self, names: &[QName]) -> Option<&Element> {
        let mut cur = self;
        for n in names {
            cur = cur.find(n)?;
        }
        Some(cur)
    }

    /// First descendant (pre-order, including `self`) with this name. Iterative.
    pub fn descendant(&self, name: &QName) -> Option<&Element> {
        self.descendants().into_iter().find(|e| &e.name == name)
    }

    /// Pre-order list of this element and all descendants. Iterative.
    pub fn descendants(&self) -> Vec<&Element> {
        let mut out = Vec::new();
        let mut stack = vec![self];
        while let Some(e) = stack.pop() {
            out.push(e);
            for c in e.children.iter().rev() {
                stack.push(c);
            }
        }
        out
    }

    /// Attribute value by expanded name.
    pub fn attr(&self, name: &QName) -> Option<&str> {
        self.attrs
            .iter()
            .find(|a| &a.name == name)
            .map(|a| a.value.as_str())
    }

    /// Value of an unqualified attribute.
    pub fn attr_local(&self, local: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|a| a.name.ns.is_empty() && a.name.local == local)
            .map(|a| a.value.as_str())
    }

    pub fn text_trimmed(&self) -> &str {
        self.text.trim()
    }

    /// Namespace URI bound to `prefix` at this element (parsed trees only).
    pub fn resolve_prefix(&self, prefix: &str) -> Option<String> {
        if prefix == "xml" {
            return Some(ns::XML.to_string());
        }
        self.scope.as_ref().and_then(|s| s.lookup(prefix))
    }

    /// Interpret the text as a QName using the namespaces in scope. A built element with
    /// `qtext` returns that.
    pub fn text_qname(&self) -> Option<QName> {
        if let Some(q) = &self.qtext {
            return Some(q.clone());
        }
        self.parse_qname(self.text.trim())
    }

    /// Interpret an attribute value as a QName using the namespaces in scope.
    pub fn attr_qname(&self, name: &QName) -> Option<QName> {
        let attr = self.attrs.iter().find(|a| &a.name == name)?;
        if let Some(q) = &attr.qvalue {
            return Some(q.clone());
        }
        self.parse_qname(attr.value.trim())
    }

    fn parse_qname(&self, raw: &str) -> Option<QName> {
        if raw.is_empty() {
            return None;
        }
        match raw.split_once(':') {
            Some((p, l)) => {
                if !is_ncname(p) || !is_ncname(l) {
                    return None;
                }
                Some(QName::new(&self.resolve_prefix(p)?, l))
            }
            None => {
                if !is_ncname(raw) {
                    return None;
                }
                Some(QName::new(
                    &self.resolve_prefix("").unwrap_or_default(),
                    raw,
                ))
            }
        }
    }

    /// Serialize without an XML declaration.
    pub fn to_xml(&self, prefixes: &PrefixMap) -> String {
        let mut out = String::new();
        write_tree(self, prefixes, &mut out);
        out
    }

    /// Serialize with `<?xml version="1.0" encoding="utf-8"?>`.
    pub fn to_document(&self, prefixes: &PrefixMap) -> String {
        let mut out = String::from("<?xml version=\"1.0\" encoding=\"utf-8\"?>");
        write_tree(self, prefixes, &mut out);
        out
    }
}

// ---------------------------------------------------------------------------------------------
// Name checks
// ---------------------------------------------------------------------------------------------

fn is_name_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_' || !c.is_ascii()
}

fn is_name_char(c: char) -> bool {
    is_name_start(c) || c.is_ascii_digit() || c == '-' || c == '.'
}

/// A non-colonized XML name.
pub fn is_ncname(s: &str) -> bool {
    let mut chars = s.chars();
    chars.next().is_some_and(is_name_start) && chars.all(is_name_char)
}

fn bad_xml_char(c: char) -> bool {
    matches!(c, '\u{0}'..='\u{8}' | '\u{b}' | '\u{c}' | '\u{e}'..='\u{1f}' | '\u{fffe}' | '\u{ffff}')
}

fn has_bad_char(s: &str) -> bool {
    s.chars().any(bad_xml_char)
}

// ---------------------------------------------------------------------------------------------
// Decoding
// ---------------------------------------------------------------------------------------------

/// Decode bytes to text. `charset` is a hint (Content-Type charset); a BOM wins over it.
/// Supported: UTF-8, UTF-16 (BOM, or explicit LE/BE), US-ASCII, ISO-8859-1.
pub fn decode_text<'a>(bytes: &'a [u8], charset: Option<&str>) -> Result<Cow<'a, str>, XmlError> {
    let enc_err = |m: &str| XmlError::new(XmlErrorKind::Encoding, m);
    if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return std::str::from_utf8(rest)
            .map(Cow::Borrowed)
            .map_err(|_| enc_err("invalid UTF-8"));
    }
    if bytes.starts_with(&[0xFF, 0xFE]) {
        return utf16(&bytes[2..], true).map(Cow::Owned);
    }
    if bytes.starts_with(&[0xFE, 0xFF]) {
        return utf16(&bytes[2..], false).map(Cow::Owned);
    }
    let hint = charset
        .map(|c| c.trim().trim_matches('"').to_ascii_lowercase())
        .filter(|c| !c.is_empty())
        .or_else(|| declared_encoding(bytes));
    match hint.as_deref() {
        None | Some("utf-8") | Some("utf8") => std::str::from_utf8(bytes)
            .map(Cow::Borrowed)
            .map_err(|_| enc_err("invalid UTF-8")),
        Some("us-ascii") | Some("ascii") => {
            if bytes.is_ascii() {
                std::str::from_utf8(bytes)
                    .map(Cow::Borrowed)
                    .map_err(|_| enc_err("invalid ASCII"))
            } else {
                Err(enc_err("non-ASCII byte in US-ASCII text"))
            }
        }
        Some("iso-8859-1") | Some("latin1") | Some("iso_8859-1") => {
            Ok(Cow::Owned(bytes.iter().map(|b| char::from(*b)).collect()))
        }
        Some("utf-16le") => utf16(bytes, true).map(Cow::Owned),
        Some("utf-16be") => utf16(bytes, false).map(Cow::Owned),
        Some("utf-16") => Err(enc_err("UTF-16 without a byte order mark")),
        Some(other) => Err(enc_err(&format!("unsupported charset {other:?}"))),
    }
}

fn utf16(bytes: &[u8], little: bool) -> Result<String, XmlError> {
    if !bytes.len().is_multiple_of(2) {
        return Err(XmlError::new(XmlErrorKind::Encoding, "odd UTF-16 length"));
    }
    let units = bytes.as_chunks::<2>().0.iter().map(|c| {
        if little {
            u16::from_le_bytes(*c)
        } else {
            u16::from_be_bytes(*c)
        }
    });
    char::decode_utf16(units)
        .collect::<Result<String, _>>()
        .map_err(|_| XmlError::new(XmlErrorKind::Encoding, "invalid UTF-16"))
}

/// `encoding="..."` of a leading XML declaration, lowercased.
fn declared_encoding(bytes: &[u8]) -> Option<String> {
    let head = &bytes[..bytes.len().min(200)];
    if !head.starts_with(b"<?xml") {
        return None;
    }
    let end = head.windows(2).position(|w| w == b"?>")?;
    let decl = std::str::from_utf8(&head[..end]).ok()?;
    let at = decl.find("encoding")?;
    let rest = decl[at + 8..].trim_start().strip_prefix('=')?.trim_start();
    let quote = rest.chars().next().filter(|c| *c == '"' || *c == '\'')?;
    let value = rest[1..].split(quote).next()?;
    Some(value.to_ascii_lowercase())
}

// ---------------------------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------------------------

/// Parse bytes (UTF-8, or per BOM/declaration) into the root element.
pub fn parse(bytes: &[u8], limits: &XmlLimits) -> Result<Element, XmlError> {
    if bytes.len() > limits.max_input {
        return Err(XmlError::new(
            XmlErrorKind::TooLarge,
            format!("input larger than {} bytes", limits.max_input),
        ));
    }
    let text = decode_text(bytes, None)?;
    parse_str(&text, limits)
}

/// Parse already-decoded text into the root element.
pub fn parse_str(text: &str, limits: &XmlLimits) -> Result<Element, XmlError> {
    if text.len() > limits.max_input.saturating_mul(4) {
        return Err(XmlError::new(XmlErrorKind::TooLarge, "input too large"));
    }
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    if has_bad_char(text) {
        return Err(XmlError::new(
            XmlErrorKind::Syntax,
            "forbidden control character",
        ));
    }
    let mut reader = Reader::from_str(text);
    {
        let cfg = reader.config_mut();
        cfg.trim_text(false);
        cfg.check_end_names = true;
        cfg.expand_empty_elements = false;
    }
    let mut stack: Vec<Element> = Vec::new();
    let mut root: Option<Element> = None;
    let mut nodes = 0usize;

    loop {
        let pos = reader.buffer_position();
        let event = reader
            .read_event()
            .map_err(|e| XmlError::new(XmlErrorKind::Syntax, e.to_string()).at(pos))?;
        match event {
            Event::Start(s) => {
                handle_start(&s, false, &mut stack, &mut root, &mut nodes, limits, pos)?;
            }
            Event::Empty(s) => {
                handle_start(&s, true, &mut stack, &mut root, &mut nodes, limits, pos)?;
            }
            Event::End(_) => {
                let node = stack.pop().ok_or_else(|| {
                    XmlError::new(XmlErrorKind::Syntax, "unbalanced end tag").at(pos)
                })?;
                attach(node, &mut stack, &mut root);
            }
            Event::Text(t) => {
                let value = t
                    .unescape()
                    .map_err(|e| XmlError::new(XmlErrorKind::Entity, e.to_string()).at(pos))?;
                add_text(&value, &mut stack, limits, pos)?;
            }
            Event::CData(c) => {
                let bytes: &[u8] = c.as_ref();
                let value = std::str::from_utf8(bytes)
                    .map_err(|_| XmlError::new(XmlErrorKind::Encoding, "invalid UTF-8").at(pos))?;
                add_text(value, &mut stack, limits, pos)?;
            }
            Event::DocType(_) => {
                return Err(XmlError::new(XmlErrorKind::Dtd, "DOCTYPE is not allowed").at(pos));
            }
            Event::Comment(_) | Event::PI(_) | Event::Decl(_) => {}
            Event::Eof => break,
        }
    }
    if !stack.is_empty() {
        return Err(XmlError::new(XmlErrorKind::Syntax, "unclosed element"));
    }
    root.ok_or_else(|| XmlError::new(XmlErrorKind::Empty, "no root element"))
}

fn add_text(
    value: &str,
    stack: &mut [Element],
    limits: &XmlLimits,
    pos: u64,
) -> Result<(), XmlError> {
    if has_bad_char(value) {
        return Err(XmlError::new(XmlErrorKind::Entity, "forbidden character in text").at(pos));
    }
    match stack.last_mut() {
        Some(top) => {
            if top.text.len().saturating_add(value.len()) > limits.max_text {
                return Err(XmlError::new(XmlErrorKind::TooLarge, "text too large").at(pos));
            }
            top.text.push_str(value);
            Ok(())
        }
        None => {
            if value.trim().is_empty() {
                Ok(())
            } else {
                Err(XmlError::new(XmlErrorKind::Trailing, "text outside the root element").at(pos))
            }
        }
    }
}

fn attach(mut node: Element, stack: &mut [Element], root: &mut Option<Element>) {
    if !node.children.is_empty() && node.text.trim().is_empty() {
        node.text.clear();
    }
    match stack.last_mut() {
        Some(parent) => parent.children.push(node),
        None => *root = Some(node),
    }
}

fn handle_start(
    s: &BytesStart<'_>,
    empty: bool,
    stack: &mut Vec<Element>,
    root: &mut Option<Element>,
    nodes: &mut usize,
    limits: &XmlLimits,
    pos: u64,
) -> Result<(), XmlError> {
    if root.is_some() {
        return Err(
            XmlError::new(XmlErrorKind::Trailing, "content after the root element").at(pos),
        );
    }
    if stack.len() + 1 > limits.max_depth {
        return Err(XmlError::new(XmlErrorKind::TooDeep, "nesting too deep").at(pos));
    }
    *nodes += 1;
    if *nodes > limits.max_nodes {
        return Err(XmlError::new(XmlErrorKind::TooMany, "too many elements").at(pos));
    }
    let parent_scope = stack.last().and_then(|e| e.scope.clone());
    let node = open_element(s, parent_scope, limits, pos)?;
    if empty {
        attach(node, stack, root);
    } else {
        stack.push(node);
    }
    Ok(())
}

fn open_element(
    s: &BytesStart<'_>,
    parent: Option<Arc<Scope>>,
    limits: &XmlLimits,
    pos: u64,
) -> Result<Element, XmlError> {
    let err = |k: XmlErrorKind, m: &str| XmlError::new(k, m).at(pos);
    let raw = std::str::from_utf8(s.name().as_ref())
        .map_err(|_| err(XmlErrorKind::Encoding, "invalid UTF-8 in name"))?
        .to_string();
    if raw.len() > limits.max_name {
        return Err(err(XmlErrorKind::TooLarge, "element name too long"));
    }
    let (eprefix, elocal) =
        split_qname(&raw).ok_or_else(|| err(XmlErrorKind::Syntax, "bad element name"))?;

    let mut decls: Vec<(String, String)> = Vec::new();
    let mut plain: Vec<(String, String)> = Vec::new();
    for attr in s.attributes() {
        let attr = attr.map_err(|e| err(XmlErrorKind::Syntax, &e.to_string()))?;
        let key = std::str::from_utf8(attr.key.as_ref())
            .map_err(|_| err(XmlErrorKind::Encoding, "invalid UTF-8 in attribute name"))?;
        if key.len() > limits.max_name {
            return Err(err(XmlErrorKind::TooLarge, "attribute name too long"));
        }
        let value = attr
            .unescape_value()
            .map_err(|e| err(XmlErrorKind::Entity, &e.to_string()))?;
        if value.len() > limits.max_attr_value {
            return Err(err(XmlErrorKind::TooLarge, "attribute value too long"));
        }
        if has_bad_char(&value) {
            return Err(err(
                XmlErrorKind::Entity,
                "forbidden character in attribute",
            ));
        }
        let value = value.into_owned();
        if key == "xmlns" {
            decls.push((String::new(), value));
        } else if let Some(prefix) = key.strip_prefix("xmlns:") {
            if !is_ncname(prefix) || prefix == "xmlns" {
                return Err(err(
                    XmlErrorKind::Namespace,
                    "bad namespace prefix declaration",
                ));
            }
            if prefix == "xml" {
                if value != ns::XML {
                    return Err(err(XmlErrorKind::Namespace, "xml prefix rebound"));
                }
                continue;
            }
            if value.is_empty() {
                return Err(err(XmlErrorKind::Namespace, "prefix cannot be un-declared"));
            }
            if value == ns::XML || value == ns::XMLNS {
                return Err(err(XmlErrorKind::Namespace, "reserved namespace URI"));
            }
            decls.push((prefix.to_string(), value));
        } else {
            plain.push((key.to_string(), value));
        }
        if plain.len() > limits.max_attrs {
            return Err(err(XmlErrorKind::TooMany, "too many attributes"));
        }
        if decls.len() > limits.max_ns_decls {
            return Err(err(
                XmlErrorKind::TooMany,
                "too many namespace declarations",
            ));
        }
    }

    let scope = if decls.is_empty() {
        parent
    } else {
        Some(Arc::new(Scope {
            parent,
            decls: decls.clone(),
        }))
    };
    let lookup = |prefix: &str| -> Option<String> {
        if prefix == "xml" {
            return Some(ns::XML.to_string());
        }
        scope.as_ref().and_then(|sc| sc.lookup(prefix))
    };

    let ens = match eprefix {
        Some(p) => {
            lookup(p).ok_or_else(|| err(XmlErrorKind::Namespace, "unbound element prefix"))?
        }
        None => lookup("").unwrap_or_default(),
    };

    let mut attrs: Vec<Attr> = Vec::with_capacity(plain.len());
    for (key, value) in plain {
        let (aprefix, alocal) =
            split_qname(&key).ok_or_else(|| err(XmlErrorKind::Syntax, "bad attribute name"))?;
        let ans = match aprefix {
            Some(p) => {
                lookup(p).ok_or_else(|| err(XmlErrorKind::Namespace, "unbound attribute prefix"))?
            }
            None => String::new(),
        };
        let name = QName {
            ns: ans,
            local: alocal.to_string(),
        };
        if attrs.iter().any(|a| a.name == name) {
            return Err(err(XmlErrorKind::Namespace, "duplicate attribute"));
        }
        attrs.push(Attr {
            name,
            value,
            qvalue: None,
        });
    }

    Ok(Element {
        name: QName {
            ns: ens,
            local: elocal.to_string(),
        },
        attrs,
        children: Vec::new(),
        text: String::new(),
        qtext: None,
        declared: decls,
        scope,
    })
}

fn split_qname(raw: &str) -> Option<(Option<&str>, &str)> {
    match raw.split_once(':') {
        Some((p, l)) if is_ncname(p) && is_ncname(l) => Some((Some(p), l)),
        Some(_) => None,
        None if is_ncname(raw) => Some((None, raw)),
        None => None,
    }
}

// ---------------------------------------------------------------------------------------------
// Serialization
// ---------------------------------------------------------------------------------------------

/// Preferred prefixes by namespace URI.
#[derive(Debug, Clone, Default)]
pub struct PrefixMap {
    entries: Vec<(String, String)>, // (ns, prefix)
}

impl PrefixMap {
    pub fn new() -> Self {
        Self::default()
    }

    /// Prefer `prefix` for `ns`.
    pub fn with(mut self, prefix: &str, ns: &str) -> Self {
        self.entries.push((ns.to_string(), prefix.to_string()));
        self
    }

    /// The prefixes the WSD stack conventionally uses.
    pub fn wsd() -> Self {
        Self::new()
            .with("soap", ns::SOAP12)
            .with("wsa", ns::WSA)
            .with("wse", ns::WSE)
            .with("wxf", ns::WXF)
            .with("wsx", ns::MEX)
            .with("wsd", ns::WSD)
            .with("wsdp", ns::WSDP)
            .with("wprt", ns::PRINT)
            .with("wprt11", ns::PRINT11)
            .with("wprt12", ns::PRINT12)
            .with("wprt20", ns::PRINT20)
            .with("xop", ns::XOP)
            .with("xsi", ns::XSI)
    }

    fn preferred(&self, uri: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|(n, _)| n == uri)
            .map(|(_, p)| p.as_str())
    }
}

/// Serializer recursion guard. Parsed trees are far shallower; this bounds built trees.
const MAX_WRITE_DEPTH: usize = 256;

fn write_tree(root: &Element, map: &PrefixMap, out: &mut String) {
    // 1. namespaces in first-use order (iterative walk)
    let mut used: Vec<String> = Vec::new();
    let mut note = |uri: &str| {
        if !uri.is_empty() && uri != ns::XML && !used.iter().any(|u| u == uri) {
            used.push(uri.to_string());
        }
    };
    let mut stack: Vec<(&Element, usize)> = vec![(root, 1)];
    while let Some((e, depth)) = stack.pop() {
        note(&e.name.ns);
        for a in &e.attrs {
            note(&a.name.ns);
            if let Some(q) = &a.qvalue {
                note(&q.ns);
            }
        }
        if let Some(q) = &e.qtext {
            note(&q.ns);
        }
        if depth < MAX_WRITE_DEPTH {
            for c in e.children.iter().rev() {
                stack.push((c, depth + 1));
            }
        }
    }
    // 2. assign prefixes
    let mut assigned: Vec<(String, String)> = Vec::new(); // (uri, prefix)
    let mut counter = 0usize;
    for uri in &used {
        let mut prefix = map.preferred(uri).map(str::to_string);
        if prefix
            .as_ref()
            .is_some_and(|p| assigned.iter().any(|(_, ap)| ap == p) || p == "xml" || p == "xmlns")
        {
            prefix = None;
        }
        let prefix = prefix.unwrap_or_else(|| loop {
            counter += 1;
            let candidate = format!("ns{counter}");
            if !assigned.iter().any(|(_, p)| p == &candidate)
                && !map.entries.iter().any(|(_, p)| p == &candidate)
            {
                break candidate;
            }
        });
        assigned.push((uri.clone(), prefix));
    }
    write_element(root, &assigned, true, 1, out);
}

fn prefixed(assigned: &[(String, String)], q: &QName) -> String {
    if q.ns.is_empty() {
        return q.local.clone();
    }
    if q.ns == ns::XML {
        return format!("xml:{}", q.local);
    }
    match assigned.iter().find(|(u, _)| u == &q.ns) {
        Some((_, p)) => format!("{p}:{}", q.local),
        None => q.local.clone(),
    }
}

fn write_element(
    e: &Element,
    assigned: &[(String, String)],
    is_root: bool,
    depth: usize,
    out: &mut String,
) {
    let name = prefixed(assigned, &e.name);
    out.push('<');
    out.push_str(&name);
    if is_root {
        for (uri, prefix) in assigned {
            out.push_str(" xmlns:");
            out.push_str(prefix);
            out.push_str("=\"");
            escape_attr(uri, out);
            out.push('"');
        }
        // extra declarations requested on the root (for QNames inside raw markup)
        for (prefix, uri) in &e.declared {
            if prefix.is_empty() || assigned.iter().any(|(_, p)| p == prefix) {
                continue;
            }
            out.push_str(" xmlns:");
            out.push_str(prefix);
            out.push_str("=\"");
            escape_attr(uri, out);
            out.push('"');
        }
    }
    for a in &e.attrs {
        out.push(' ');
        out.push_str(&prefixed(assigned, &a.name));
        out.push_str("=\"");
        match &a.qvalue {
            Some(q) => escape_attr(&prefixed(assigned, q), out),
            None => escape_attr(&a.value, out),
        }
        out.push('"');
    }
    let qtext = e.qtext.as_ref().map(|q| prefixed(assigned, q));
    let has_text = qtext.is_some() || !e.text.is_empty();
    if e.children.is_empty() && !has_text {
        out.push_str("/>");
        return;
    }
    out.push('>');
    match &qtext {
        Some(t) => escape_text(t, out),
        None => escape_text(&e.text, out),
    }
    if depth < MAX_WRITE_DEPTH {
        for c in &e.children {
            write_element(c, assigned, false, depth + 1, out);
        }
    }
    out.push_str("</");
    out.push_str(&name);
    out.push('>');
}

/// Escape text content (also usable for building fragments by hand).
pub fn escape_text(s: &str, out: &mut String) {
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\r' => out.push_str("&#13;"),
            c if bad_xml_char(c) => out.push('\u{fffd}'),
            c => out.push(c),
        }
    }
}

/// Escape an attribute value for double quotes.
pub fn escape_attr(s: &str, out: &mut String) {
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\t' => out.push_str("&#9;"),
            '\n' => out.push_str("&#10;"),
            '\r' => out.push_str("&#13;"),
            c if bad_xml_char(c) => out.push('\u{fffd}'),
            c => out.push(c),
        }
    }
}

/// Escape to a new string (text-content rules plus quotes, safe in either context).
pub fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            c if bad_xml_char(c) => out.push('\u{fffd}'),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Result<Element, XmlError> {
        parse_str(s, &XmlLimits::SMALL)
    }

    #[test]
    fn resolves_namespaces_and_finds_by_qname() {
        let el = p(r#"<a:r xmlns:a="urn:a" xmlns="urn:d"><b x="1"/><a:c a:y="2">t</a:c></a:r>"#)
            .unwrap();
        assert!(el.is("urn:a", "r"));
        assert!(el.child("urn:d", "b").is_some());
        let c = el.find(&QName::new("urn:a", "c")).unwrap();
        assert_eq!(c.text, "t");
        assert_eq!(c.attr(&QName::new("urn:a", "y")), Some("2"));
        assert_eq!(el.child("urn:d", "b").unwrap().attr_local("x"), Some("1"));
        assert!(el.descendant(&QName::new("urn:a", "c")).is_some());
    }

    #[test]
    fn rejects_unbound_prefix_and_duplicate_attrs() {
        assert_eq!(p("<a:r/>").unwrap_err().kind, XmlErrorKind::Namespace);
        assert_eq!(
            p(r#"<r x:a="1"/>"#).unwrap_err().kind,
            XmlErrorKind::Namespace
        );
        assert!(p(r#"<r a="1" a="2"/>"#).is_err());
        assert_eq!(
            p(r#"<r xmlns:a="u" xmlns:b="u" a:x="1" b:x="2"/>"#)
                .unwrap_err()
                .kind,
            XmlErrorKind::Namespace
        );
    }

    #[test]
    fn rejects_doctype_and_entities_billion_laughs() {
        let bomb = r#"<?xml version="1.0"?><!DOCTYPE lolz [<!ENTITY lol "lol"><!ENTITY lol2 "&lol;&lol;&lol;">]><lolz>&lol2;</lolz>"#;
        assert_eq!(p(bomb).unwrap_err().kind, XmlErrorKind::Dtd);
        assert_eq!(p("<a>&foo;</a>").unwrap_err().kind, XmlErrorKind::Entity);
        assert_eq!(p("<a>&#0;</a>").unwrap_err().kind, XmlErrorKind::Entity);
        assert_eq!(p("<a>&lt;&#65;&amp;</a>").unwrap().text, "<A&");
    }

    #[test]
    fn caps_depth_nodes_attrs_and_sizes() {
        let deep = format!("{}{}", "<a>".repeat(200), "</a>".repeat(200));
        assert_eq!(p(&deep).unwrap_err().kind, XmlErrorKind::TooDeep);
        let wide = format!("<r>{}</r>", "<a/>".repeat(30_000));
        assert_eq!(p(&wide).unwrap_err().kind, XmlErrorKind::TooMany);
        let attrs: String = (0..100).map(|i| format!(" a{i}=\"1\"")).collect();
        assert_eq!(
            p(&format!("<r{attrs}/>")).unwrap_err().kind,
            XmlErrorKind::TooMany
        );
        let big = format!("<r a=\"{}\"/>", "x".repeat(40_000));
        assert_eq!(p(&big).unwrap_err().kind, XmlErrorKind::TooLarge);
        let huge = vec![b' '; (1 << 20) + 1];
        assert_eq!(
            parse(&huge, &XmlLimits::SMALL).unwrap_err().kind,
            XmlErrorKind::TooLarge
        );
        let long_name = format!("<{}/>", "n".repeat(1000));
        assert_eq!(p(&long_name).unwrap_err().kind, XmlErrorKind::TooLarge);
    }

    #[test]
    fn rejects_malformed_and_trailing() {
        for bad in [
            "",
            "<a>",
            "<a></b>",
            "<a/><b/>",
            "x<a/>",
            "<a/>junk",
            "<a b></a>",
            "<a\u{1}/>",
            "<1a/>",
        ] {
            assert!(p(bad).is_err(), "{bad:?}");
        }
        assert!(p("<a/><!-- c --> ").is_ok());
        assert_eq!(p("").unwrap_err().kind, XmlErrorKind::Empty);
    }

    #[test]
    fn decodes_charsets() {
        let mut v = vec![0xFF, 0xFE];
        for u in "<a>é</a>".encode_utf16() {
            v.extend_from_slice(&u.to_le_bytes());
        }
        assert_eq!(parse(&v, &XmlLimits::SMALL).unwrap().text, "é");
        let latin = b"<?xml version=\"1.0\" encoding=\"iso-8859-1\"?><a>\xe9</a>";
        assert_eq!(parse(latin, &XmlLimits::SMALL).unwrap().text, "é");
        assert!(parse(b"<a>\xe9</a>", &XmlLimits::SMALL).is_err());
        assert!(decode_text(b"x", Some("koi8-r")).is_err());
        assert!(decode_text(b"\xc3\xa9", Some("us-ascii")).is_err());
        assert_eq!(decode_text(b"\xef\xbb\xbfhi", None).unwrap(), "hi");
    }

    #[test]
    fn serialize_round_trip_with_prefix_map() {
        let el = Element::new(ns::SOAP12, "Envelope")
            .with_child(
                Element::new(ns::PRINT, "A")
                    .with_attr("k", "a<b\"&\n")
                    .with_text("x & <y>"),
            )
            .with_child(Element::new("urn:other", "B"))
            .with_child(Element::new("", "Plain"));
        let xml = el.to_xml(&PrefixMap::wsd());
        assert!(xml.starts_with("<soap:Envelope xmlns:soap="), "{xml}");
        assert!(
            xml.contains("<wprt:A k=\"a&lt;b&quot;&amp;&#10;\">x &amp; &lt;y&gt;</wprt:A>"),
            "{xml}"
        );
        assert!(xml.contains("xmlns:ns1=\"urn:other\""), "{xml}");
        assert!(xml.contains("<Plain/>"), "{xml}");
        assert_eq!(p(&xml).unwrap(), el);
    }

    #[test]
    fn qname_values_round_trip() {
        let el = Element::new(ns::PRINT, "ElementData")
            .with_attr_qname("Name", QName::new(ns::PRINT, "PrinterStatus"))
            .with_child(
                Element::new("", "V").with_qtext(QName::new(ns::WSA, "ActionNotSupported")),
            );
        let xml = el.to_xml(&PrefixMap::wsd());
        assert!(xml.contains("Name=\"wprt:PrinterStatus\""), "{xml}");
        let back = p(&xml).unwrap();
        assert_eq!(
            back.attr_qname(&QName::unqualified("Name")),
            Some(QName::new(ns::PRINT, "PrinterStatus"))
        );
        assert_eq!(
            back.child("", "V").unwrap().text_qname(),
            Some(QName::new(ns::WSA, "ActionNotSupported"))
        );
        assert_eq!(p("<a>nope:x</a>").unwrap().text_qname(), None);
    }

    #[test]
    fn serializer_survives_extreme_depth() {
        let mut el = Element::new("", "leaf");
        for _ in 0..2000 {
            el = Element::new("", "n").with_child(el);
        }
        let xml = el.to_xml(&PrefixMap::new());
        assert!(xml.len() < 100_000);
    }

    #[test]
    fn cdata_and_mixed_whitespace() {
        let el = p("<a>\n <b/>\n</a>").unwrap();
        assert_eq!(el.text, "");
        assert_eq!(p("<a><![CDATA[<x>]]></a>").unwrap().text, "<x>");
    }
}
