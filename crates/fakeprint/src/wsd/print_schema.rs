//! Check a SOAP body against the Print Service XSD in `docs/reference/schema`.
//!
//! The schema's target namespace is `2006/08`. Messages in `2006/01` use the same
//! local names, so either `/wdp/print` namespace is accepted. SOAP faults and
//! WS-Eventing wrappers are not print elements; a print element nested in one is
//! still checked. v1.1, v1.2, and v2.0 extension schemas are loaded too. Their
//! elements live in later namespaces and do not replace the v1.0 local names.
//! The service does not advertise those versions.
//!
//! Lab allowances (stock XSD does not model these):
//! - **CreatePrintJobResponse:** optional same-NS `JobToken` after `JobId`
//! - **PrinterStatus:** optional same-NS `PrinterLocalTime` after `QueuedJobCount`
//! - **JobElementDataType:** `Name=…JobToken` may be empty or carry a `JobToken` child
//!
//! Print `JobStatus` counters are schema (`KOctetsProcessed` / `MediaSheetsCompleted` /
//! `NumberOfDocuments`) — not scan `ImagesCompleted` / `ImagesToTransfer`.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::OnceLock;

use quick_xml::events::Event;
use quick_xml::reader::Reader;

const SCAN_NS_SUFFIX: &str = "/wdp/print";

pub fn body_problems(soap_xml: &str) -> Vec<String> {
    let schema = schema();
    let root = match parse_tree(xml_document(soap_xml)) {
        Some(root) => root,
        None => return vec!["body is not XML".into()],
    };
    let mut problems = Vec::new();
    check_tree(&root, schema, &mut problems);
    problems
}

fn check_tree(node: &Node, schema: &Schema, problems: &mut Vec<String>) {
    if node.local == "Body" {
        for child in &node.children {
            check_body_child(child, schema, problems);
        }
        return;
    }
    if node.local == "Envelope" {
        for child in &node.children {
            check_tree(child, schema, problems);
        }
    }
}

fn check_body_child(node: &Node, schema: &Schema, problems: &mut Vec<String>) {
    if node.in_scan {
        if schema.elements.contains_key(&node.local) {
            validate_element(node, schema, problems);
        } else {
            if problems.len() < 12 {
                problems.push(format!("{} is not a print schema element", node.local));
            }
            // The wrapper is outside v1.2 (ValidatePrintTicket). Children that are
            // schema elements, such as PrintTicket, are still checked.
            check_known_descendants(node, schema, problems);
        }
        return;
    }
    for child in &node.children {
        if child.in_scan && schema.elements.contains_key(&child.local) {
            validate_element(child, schema, problems);
        }
    }
}

fn check_known_descendants(node: &Node, schema: &Schema, problems: &mut Vec<String>) {
    for child in &node.children {
        if child.in_scan && schema.elements.contains_key(&child.local) {
            validate_element(child, schema, problems);
        } else {
            check_known_descendants(child, schema, problems);
        }
    }
}

fn validate_element(node: &Node, schema: &Schema, problems: &mut Vec<String>) {
    let Some(type_name) = schema.elements.get(&node.local) else {
        return;
    };
    let mut stack = Vec::new();
    let model = model_of(schema, type_name, &mut stack);
    check_model(node, &model, schema, &node.local, Ctx::default(), problems);
}

#[derive(Clone, Copy, Default)]
struct Ctx {
    /// Reserved for ancestor-aware lab allowances (print has few today).
    _reserved: bool,
}

fn check_model(
    node: &Node,
    model: &Model,
    schema: &Schema,
    path: &str,
    ctx: Ctx,
    problems: &mut Vec<String>,
) {
    if problems.len() >= 12 {
        return;
    }
    let ctx = validation_ctx(node, ctx);
    check_attrs(node, model, schema, path, problems);
    if let Some(simple) = &model.simple {
        if let Some(foreign) = node.children.iter().find(|child| !child.in_scan) {
            if node.children.iter().any(|child| child.in_scan) && problems.len() < 12 {
                problems.push(format!("{path} mixes print children into a simple value"));
            }
            let _ = foreign;
        } else if !value_ok(schema, simple, node.text.trim()) && problems.len() < 12 {
            problems.push(format!(
                "{path} value '{}' does not match {}",
                node.text.trim(),
                simple.label()
            ));
        }
    }
    if element_data_is_unrecognized(node) {
        return;
    }
    if element_data_is_job_token(node) {
        check_job_token_element_data(node, schema, path, problems);
        return;
    }
    if !model.particles.is_empty() || model.simple.is_none() {
        walk_particles(&model.particles, &node.children, schema, path, ctx, problems);
    }
}

fn validation_ctx(_node: &Node, ctx: Ctx) -> Ctx {
    ctx
}

fn element_data_is_unrecognized(node: &Node) -> bool {
    node.local == "ElementData"
        && node.children.is_empty()
        && matches!(node.attr("Valid"), Some("false" | "0"))
}

fn element_data_is_job_token(node: &Node) -> bool {
    node.local == "ElementData"
        && node.attrs.iter().any(|attr| {
            attr.name == "Name"
                && (attr.value == "JobToken" || attr.value.rsplit(':').next() == Some("JobToken"))
        })
}

fn check_job_token_element_data(
    node: &Node,
    schema: &Schema,
    path: &str,
    problems: &mut Vec<String>,
) {
    // JobElementDataType choice is JobStatus | PrintTicket | Documents | ##other.
    // Lab: Name=JobToken with a JobToken child, or Valid=false empty.
    match node.children.as_slice() {
        [] => {}
        [child] if child.local == "JobToken" && child.in_scan => {
            let mut stack = Vec::new();
            let model = model_of(schema, "String255ExtType", &mut stack);
            check_model(
                child,
                &model,
                schema,
                &format!("{path}/JobToken"),
                Ctx::default(),
                problems,
            );
        }
        _ if problems.len() < 12 => {
            let names: Vec<_> = node
                .children
                .iter()
                .map(|child| child.local.as_str())
                .collect();
            problems.push(format!(
                "{path} JobToken ElementData expected empty or JobToken, found {}",
                names.join(", ")
            ));
        }
        _ => {}
    }
}

fn check_attrs(
    node: &Node,
    model: &Model,
    schema: &Schema,
    path: &str,
    problems: &mut Vec<String>,
) {
    for attr in &model.attrs {
        let found = node
            .attrs
            .iter()
            .find(|item| item.name == attr.name && item.in_scan == attr.in_schema_ns);
        if attr.required && found.is_none() && problems.len() < 12 {
            problems.push(format!("{path} is missing attribute {}", attr.name));
        }
        if let Some(found) = found {
            if !value_ok(schema, &attr.simple, &found.value) && problems.len() < 12 {
                problems.push(format!(
                    "{path}/@{} value '{}' does not match {}",
                    attr.name,
                    found.value,
                    attr.simple.label()
                ));
            }
        }
    }
    for attr in &node.attrs {
        if attr.in_scan {
            let known = model
                .attrs
                .iter()
                .any(|item| item.name == attr.name && item.in_schema_ns);
            if !known && problems.len() < 12 {
                problems.push(format!("{path} has unexpected attribute {}", attr.name));
            }
        } else {
            let known = model
                .attrs
                .iter()
                .any(|item| item.name == attr.name && !item.in_schema_ns);
            let allowed = known || model.any_attr || attr.name == "lang";
            if !allowed && problems.len() < 12 {
                problems.push(format!("{path} has unexpected attribute {}", attr.name));
            }
        }
    }
}

fn walk_particles(
    particles: &[Particle],
    children: &[Node],
    schema: &Schema,
    path: &str,
    ctx: Ctx,
    problems: &mut Vec<String>,
) {
    let mut index = 0;
    for particle in particles {
        if problems.len() >= 12 {
            return;
        }
        index = consume(particle, children, index, schema, path, ctx, problems);
    }
    // Print JobStatusType already includes KOctets/MediaSheets/NumberOfDocuments
    // in the sequence (not scan Images*). Only ##other after that.
    // v1.0 CreatePrintJobResponseType is JobId only. The lab keeps JobToken
    // after JobId as a correlator.
    if path.ends_with("CreatePrintJobResponse") {
        while index < children.len() && lab_named(&children[index], "JobToken") {
            index += 1;
        }
    }
    // PrinterStatusType ends at QueuedJobCount (then xs:any ##other).
    // Microsoft samples and this lab still send PrinterLocalTime after that.
    if path.ends_with("PrinterStatus") {
        while index < children.len() && lab_named(&children[index], "PrinterLocalTime") {
            index += 1;
        }
    }
    if index < children.len() && problems.len() < 12 {
        let extra: Vec<_> = children[index..]
            .iter()
            .map(|child| child.local.as_str())
            .collect();
        problems.push(format!("{path} has unexpected child {}", extra.join(", ")));
    }
}

fn lab_named(node: &Node, local: &str) -> bool {
    node.in_scan && node.local == local
}

fn consume(
    particle: &Particle,
    children: &[Node],
    index: usize,
    schema: &Schema,
    path: &str,
    ctx: Ctx,
    problems: &mut Vec<String>,
) -> usize {
    match particle {
        Particle::Element {
            name,
            type_name,
            min,
            max,
        } => {
            let mut count = 0;
            let mut index = index;
            while index < children.len()
                && &children[index].local == name
                && children[index].in_scan
            {
                if max.is_some_and(|limit| count >= limit) {
                    break;
                }
                let mut stack = Vec::new();
                let model = model_of(schema, type_name, &mut stack);
                let child_path = format!("{path}/{name}");
                check_model(&children[index], &model, schema, &child_path, ctx, problems);
                count += 1;
                index += 1;
            }
            if count < *min && problems.len() < 12 {
                problems.push(format!("{path} is missing {name}"));
            }
            index
        }
        Particle::Any { min, max } => {
            let mut count = 0;
            let mut index = index;
            while index < children.len() && !children[index].in_scan {
                if max.is_some_and(|limit| count >= limit) {
                    break;
                }
                count += 1;
                index += 1;
            }
            if count < *min && problems.len() < 12 {
                problems.push(format!("{path} is missing an extension element"));
            }
            index
        }
        Particle::Choice(options) => {
            let Some(next) = children.get(index) else {
                // An `xs:any` with minOccurs 0 can be the chosen branch, so an empty choice is valid.
                if options.iter().all(|option| particle_min(option) > 0) && problems.len() < 12 {
                    problems.push(format!("{path} is missing a choice"));
                }
                return index;
            };
            if let Some(option) = options
                .iter()
                .find(|option| particle_names(option).contains(&next.local.as_str()))
            {
                return consume(option, children, index, schema, path, ctx, problems);
            }
            if options
                .iter()
                .any(|option| matches!(option, Particle::Any { .. }))
                && !next.in_scan
            {
                return index + 1;
            }
            let names: Vec<_> = options.iter().flat_map(particle_names).collect();
            if problems.len() < 12 {
                problems.push(format!(
                    "{path} choice expected {}, found {}",
                    names.join("|"),
                    next.local
                ));
            }
            index + 1
        }
    }
}

fn particle_min(particle: &Particle) -> u32 {
    match particle {
        Particle::Element { min, .. } | Particle::Any { min, .. } => *min,
        Particle::Choice(options) => options.iter().map(particle_min).min().unwrap_or(0),
    }
}

fn particle_names(particle: &Particle) -> Vec<&str> {
    match particle {
        Particle::Element { name, .. } => vec![name.as_str()],
        Particle::Any { .. } => Vec::new(),
        Particle::Choice(options) => options.iter().flat_map(particle_names).collect(),
    }
}

fn value_ok(schema: &Schema, simple: &SimpleKind, text: &str) -> bool {
    match simple {
        SimpleKind::String { max, exact } => {
            let count = text.chars().count();
            exact.is_none_or(|limit| count == limit) && max.is_none_or(|limit| count <= limit)
        }
        SimpleKind::Boolean => matches!(text, "true" | "false" | "1" | "0"),
        SimpleKind::Int { min } => text
            .parse::<i64>()
            .ok()
            .is_some_and(|value| min.is_none_or(|floor| value >= floor)),
        SimpleKind::DateTime => {
            let bytes = text.as_bytes();
            bytes.len() >= 20
                && bytes.get(10) == Some(&b'T')
                && text
                    .chars()
                    .all(|c| c.is_ascii_digit() || matches!(c, '-' | ':' | 'T' | 'Z' | '+' | '.'))
        }
        // xs:anyURI is a URI reference. Print format values are MIME types (`application/pdf`).
        SimpleKind::AnyUri => !text.is_empty() && !text.chars().any(char::is_whitespace),
        SimpleKind::QName => !text.is_empty() && !text.chars().any(char::is_whitespace),
        SimpleKind::Token { max, values } => {
            max.is_none_or(|limit| text.chars().count() <= limit)
                && (values.is_empty() || values.iter().any(|value| value == text))
        }
        SimpleKind::Pattern(kind) => match kind {
            Pattern::KeywordExt => keyword_ext(text),
            Pattern::MimeExt => mime_ext(text),
        },
        SimpleKind::Union(members) => members.iter().any(|member| {
            let mut stack = Vec::new();
            model_of(schema, member, &mut stack)
                .simple
                .is_some_and(|simple| value_ok(schema, &simple, text))
        }),
        SimpleKind::NmTokens => !text.split_whitespace().any(|token| token.is_empty()),
        SimpleKind::Base64 => text.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=' | '\n' | '\r' | ' ' | '\t')
        }),
    }
}

fn keyword_ext(text: &str) -> bool {
    let Some((left, right)) = text.split_once(':') else {
        return false;
    };
    !left.is_empty()
        && left.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !right.is_empty()
        && right
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

fn mime_ext(text: &str) -> bool {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) if first.is_ascii_alphanumeric() || first == '_' => {}
        _ => return false,
    }
    chars.all(|c| {
        c.is_ascii_alphanumeric() || matches!(c, '_' | ':' | '/' | ';' | '-' | '\\' | '+' | '.')
    })
}

fn model_of(schema: &Schema, type_name: &str, stack: &mut Vec<String>) -> Model {
    if stack.iter().any(|name| name == type_name) {
        return Model::default();
    }
    if let Some(builtin) = builtin(type_name) {
        return Model {
            simple: Some(builtin),
            ..Model::default()
        };
    }
    let Some(body) = schema.types.get(type_name) else {
        return Model::default();
    };
    stack.push(type_name.to_string());
    let model = match body {
        TypeBody::Simple(simple) => Model {
            simple: Some(simple.clone()),
            ..Model::default()
        },
        TypeBody::Complex {
            base,
            mode,
            particles,
            attrs,
        } => {
            let parent = base
                .as_deref()
                .map(|name| model_of(schema, name, stack))
                .unwrap_or_default();
            match mode {
                Mode::Sequence => Model {
                    particles: particles.clone(),
                    attrs: attrs.clone(),
                    any_attr: true,
                    ..Model::default()
                },
                Mode::SimpleContent => Model {
                    simple: parent.simple.clone(),
                    attrs: merge_attrs(&parent.attrs, attrs),
                    any_attr: true,
                    ..Model::default()
                },
                Mode::ComplexContent => {
                    let mut combined = parent.particles.clone();
                    combined.extend(particles.iter().cloned());
                    Model {
                        particles: combined,
                        simple: parent.simple.clone(),
                        attrs: merge_attrs(&parent.attrs, attrs),
                        any_attr: true,
                    }
                }
            }
        }
    };
    stack.pop();
    model
}

fn merge_attrs(parent: &[AttrUse], own: &[AttrUse]) -> Vec<AttrUse> {
    let mut attrs = parent.to_vec();
    for attr in own {
        if let Some(slot) = attrs
            .iter_mut()
            .find(|item| item.name == attr.name && item.in_schema_ns == attr.in_schema_ns)
        {
            *slot = attr.clone();
        } else {
            attrs.push(attr.clone());
        }
    }
    attrs
}

fn builtin(type_name: &str) -> Option<SimpleKind> {
    Some(match type_name {
        "xs:boolean" => SimpleKind::Boolean,
        "xs:int" => SimpleKind::Int { min: None },
        "xs:string" => SimpleKind::String { max: None, exact: None },
        "xs:dateTime" => SimpleKind::DateTime,
        "xs:anyURI" => SimpleKind::AnyUri,
        "xs:QName" => SimpleKind::QName,
        "xs:NMTOKEN" => SimpleKind::Token {
            max: None,
            values: Vec::new(),
        },
        "xs:NMTOKENS" => SimpleKind::NmTokens,
        "xs:base64Binary" => SimpleKind::Base64,
        _ => return None,
    })
}

#[derive(Clone, Default)]
struct Model {
    particles: Vec<Particle>,
    simple: Option<SimpleKind>,
    attrs: Vec<AttrUse>,
    any_attr: bool,
}

#[derive(Clone)]
enum Particle {
    Element {
        name: String,
        type_name: String,
        min: u32,
        max: Option<u32>,
    },
    Any {
        min: u32,
        max: Option<u32>,
    },
    Choice(Vec<Particle>),
}

#[derive(Clone)]
struct AttrUse {
    name: String,
    in_schema_ns: bool,
    required: bool,
    simple: SimpleKind,
}

#[derive(Clone)]
enum SimpleKind {
    String {
        max: Option<usize>,
        exact: Option<usize>,
    },
    Boolean,
    Int {
        min: Option<i64>,
    },
    DateTime,
    AnyUri,
    QName,
    Token {
        max: Option<usize>,
        values: Vec<String>,
    },
    Pattern(Pattern),
    Union(Vec<String>),
    NmTokens,
    Base64,
}

impl SimpleKind {
    fn label(&self) -> String {
        match self {
            SimpleKind::String { max, exact } => match (exact, max) {
                (Some(limit), _) => format!("string[{limit}]"),
                (None, Some(limit)) => format!("string({limit})"),
                (None, None) => "string".into(),
            },
            SimpleKind::Boolean => "boolean".into(),
            SimpleKind::Int { min } => match min {
                Some(floor) => format!("int>={floor}"),
                None => "int".into(),
            },
            SimpleKind::DateTime => "dateTime".into(),
            SimpleKind::AnyUri => "anyURI".into(),
            SimpleKind::QName => "QName".into(),
            SimpleKind::Token { values, .. } if !values.is_empty() => values.join("|"),
            SimpleKind::Token { .. } => "NMTOKEN".into(),
            SimpleKind::Pattern(Pattern::KeywordExt) => "namespace keyword".into(),
            SimpleKind::Pattern(Pattern::MimeExt) => "mime token".into(),
            SimpleKind::Union(members) => members.join(" or "),
            SimpleKind::NmTokens => "NMTOKENS".into(),
            SimpleKind::Base64 => "base64".into(),
        }
    }
}

#[derive(Clone, Copy)]
enum Pattern {
    KeywordExt,
    MimeExt,
}

enum TypeBody {
    Simple(SimpleKind),
    Complex {
        base: Option<String>,
        mode: Mode,
        particles: Vec<Particle>,
        attrs: Vec<AttrUse>,
    },
}

#[derive(Clone, Copy)]
enum Mode {
    Sequence,
    SimpleContent,
    ComplexContent,
}

struct Schema {
    elements: HashMap<String, String>,
    types: HashMap<String, TypeBody>,
    attributes: HashMap<String, SimpleKind>,
}

fn schema() -> &'static Schema {
    static SCHEMA: OnceLock<Schema> = OnceLock::new();
    SCHEMA.get_or_init(load_schema)
}

fn load_schema() -> Schema {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/reference/schema");
    let mut roots = Vec::new();
    let entries =
        fs::read_dir(&dir).unwrap_or_else(|error| panic!("print schema {}: {error}", dir.display()));
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("xsd") {
            continue;
        }
        let text =
            fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        if let Some(root) = parse_tree(&text) {
            roots.push(root);
        }
    }
    let v20 = dir
        .join("..")
        .join("upstream")
        .join("print-v2")
        .join("WDPPrintV20.xsd");
    if v20.is_file() {
        let text = fs::read_to_string(&v20)
            .unwrap_or_else(|error| panic!("{}: {error}", v20.display()));
        if let Some(root) = parse_tree(&text) {
            roots.push(root);
        }
    }
    let mut schema = Schema {
        elements: HashMap::new(),
        types: HashMap::new(),
        attributes: HashMap::new(),
    };
    for root in &roots {
        for child in &root.children {
            match child.local.as_str() {
                "simpleType" => {
                    if let Some(name) = child.attr("name") {
                        schema
                            .types
                            .insert(name.to_string(), TypeBody::Simple(simple_kind(child)));
                    }
                }
                "attribute" => {
                    if let Some(name) = child.attr("name") {
                        let simple = child
                            .attr("type")
                            .map(simple_from_type)
                            .unwrap_or(SimpleKind::String { max: None, exact: None });
                        schema.attributes.insert(name.to_string(), simple);
                    }
                }
                _ => {}
            }
        }
    }
    let mut anon = 0u32;
    for root in &roots {
        for child in &root.children {
            match child.local.as_str() {
                "complexType" => {
                    if let Some(name) = child.attr("name") {
                        let body = complex_body(child, &mut schema, &mut anon);
                        schema.types.insert(name.to_string(), body);
                    }
                }
                "element" => {
                    declare_element(child, &mut schema, &mut anon, true);
                }
                _ => {}
            }
        }
    }
    schema
}

fn declare_element(node: &Node, schema: &mut Schema, anon: &mut u32, global: bool) -> String {
    let name = node.attr("name").unwrap_or("").to_string();
    let type_name = if let Some(explicit) = node.attr("type") {
        type_key(explicit)
    } else if let Some(inline) = node.child("complexType") {
        *anon += 1;
        let type_name = format!("anon{anon}");
        let body = complex_body(inline, schema, anon);
        schema.types.insert(type_name.clone(), body);
        type_name
    } else {
        "xs:string".into()
    };
    if global && !name.is_empty() {
        schema.elements.insert(name, type_name.clone());
    }
    type_name
}

fn complex_body(node: &Node, schema: &mut Schema, anon: &mut u32) -> TypeBody {
    if let Some(simple) = node
        .child("simpleContent")
        .and_then(|content| content.child("extension"))
    {
        return TypeBody::Complex {
            base: simple.attr("base").map(type_key),
            mode: Mode::SimpleContent,
            particles: Vec::new(),
            attrs: attrs_of(simple, schema),
        };
    }
    if let Some(complex) = node
        .child("complexContent")
        .and_then(|content| content.child("extension"))
    {
        let particles = complex
            .child("sequence")
            .map(|seq| particles_of(seq, schema, anon))
            .unwrap_or_default();
        return TypeBody::Complex {
            base: complex.attr("base").map(type_key),
            mode: Mode::ComplexContent,
            particles,
            attrs: attrs_of(complex, schema),
        };
    }
    let particles = node
        .child("sequence")
        .or_else(|| node.child("choice"))
        .map(|content| particles_of(content, schema, anon))
        .unwrap_or_default();
    let mut attrs = attrs_of(node, schema);
    if node.child("choice").is_some() && node.child("sequence").is_none() {
        return TypeBody::Complex {
            base: None,
            mode: Mode::Sequence,
            particles: vec![Particle::Choice(particles)],
            attrs,
        };
    }
    let _ = &mut attrs;
    TypeBody::Complex {
        base: None,
        mode: Mode::Sequence,
        particles,
        attrs,
    }
}

fn particles_of(node: &Node, schema: &mut Schema, anon: &mut u32) -> Vec<Particle> {
    let mut particles = Vec::new();
    for child in &node.children {
        match child.local.as_str() {
            "element" => {
                let name = child.attr("name").unwrap_or("").to_string();
                let type_name = declare_element(child, schema, anon, false);
                particles.push(Particle::Element {
                    name,
                    type_name,
                    min: occurs(child, "minOccurs", 1),
                    max: max_occurs(child),
                });
            }
            "any" => particles.push(Particle::Any {
                min: occurs(child, "minOccurs", 1),
                max: max_occurs(child),
            }),
            "choice" => particles.push(Particle::Choice(particles_of(child, schema, anon))),
            _ => {}
        }
    }
    particles
}

fn attrs_of(node: &Node, schema: &Schema) -> Vec<AttrUse> {
    let mut attrs = Vec::new();
    for child in &node.children {
        if child.local != "attribute" {
            continue;
        }
        if let Some(reference) = child.attr("ref") {
            let name = reference
                .rsplit(':')
                .next()
                .unwrap_or(reference)
                .to_string();
            let simple = schema
                .attributes
                .get(&name)
                .cloned()
                .unwrap_or(SimpleKind::String { max: None, exact: None });
            attrs.push(AttrUse {
                name,
                in_schema_ns: true,
                required: child.attr("use") == Some("required"),
                simple,
            });
        } else if let Some(name) = child.attr("name") {
            let simple = child
                .attr("type")
                .map(|value| simple_from_name(schema, value))
                .unwrap_or(SimpleKind::String { max: None, exact: None });
            attrs.push(AttrUse {
                name: name.to_string(),
                in_schema_ns: false,
                required: child.attr("use") == Some("required"),
                simple,
            });
        }
    }
    attrs
}

fn simple_from_name(schema: &Schema, type_name: &str) -> SimpleKind {
    let key = type_key(type_name);
    if let Some(builtin) = builtin(&key) {
        return builtin;
    }
    if let Some(TypeBody::Simple(simple)) = schema.types.get(&key) {
        return simple.clone();
    }
    SimpleKind::String { max: None, exact: None }
}

fn simple_from_type(type_name: &str) -> SimpleKind {
    builtin(&type_key(type_name)).unwrap_or(SimpleKind::String { max: None, exact: None })
}

fn simple_kind(node: &Node) -> SimpleKind {
    if let Some(union_node) = node.child("union") {
        let members = union_node
            .attr("memberTypes")
            .unwrap_or("")
            .split_whitespace()
            .map(type_key)
            .collect();
        return SimpleKind::Union(members);
    }
    let Some(restriction) = node.child("restriction") else {
        return SimpleKind::String { max: None, exact: None };
    };
    let base = restriction
        .attr("base")
        .map(type_key)
        .unwrap_or_else(|| "xs:string".into());
    let enums: Vec<String> = restriction
        .children
        .iter()
        .filter(|child| child.local == "enumeration")
        .filter_map(|child| child.attr("value").map(str::to_string))
        .collect();
    let max = restriction
        .child_attr("maxLength", "value")
        .and_then(|value| value.parse().ok());
    let min = restriction
        .child_attr("minInclusive", "value")
        .and_then(|value| value.parse().ok());
    if let Some(pattern) = restriction.child_attr("pattern", "value") {
        if pattern.contains(':') {
            return SimpleKind::Pattern(Pattern::KeywordExt);
        }
        return SimpleKind::Pattern(Pattern::MimeExt);
    }
    if !enums.is_empty() || base == "xs:NMTOKEN" {
        return SimpleKind::Token { max, values: enums };
    }
    if base == "xs:int" || min.is_some() {
        return SimpleKind::Int { min };
    }
    if base == "xs:boolean" {
        return SimpleKind::Boolean;
    }
    let exact = restriction
        .child_attr("length", "value")
        .and_then(|value| value.parse().ok());
    SimpleKind::String { max, exact }
}

fn occurs(node: &Node, name: &str, default: u32) -> u32 {
    node.attr(name)
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn max_occurs(node: &Node) -> Option<u32> {
    match node.attr("maxOccurs") {
        None => Some(1),
        Some("unbounded") => None,
        Some(value) => value.parse().ok(),
    }
}

fn type_key(value: &str) -> String {
    if let Some(rest) = value.strip_prefix("xs:") {
        format!("xs:{rest}")
    } else {
        value.rsplit(':').next().unwrap_or(value).to_string()
    }
}

struct Node {
    local: String,
    in_scan: bool,
    attrs: Vec<Attr>,
    children: Vec<Node>,
    text: String,
}

struct Attr {
    name: String,
    in_scan: bool,
    value: String,
}

impl Node {
    fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|attr| attr.name == name)
            .map(|attr| attr.value.as_str())
    }

    fn child(&self, name: &str) -> Option<&Node> {
        self.children.iter().find(|child| child.local == name)
    }

    fn child_attr(&self, child: &str, name: &str) -> Option<&str> {
        self.child(child).and_then(|node| node.attr(name))
    }
}

fn xml_document(input: &str) -> &str {
    if let Some(end) = input.find("</soap:Envelope>") {
        &input[..end + "</soap:Envelope>".len()]
    } else if let Some(end) = input.find("</Envelope>") {
        &input[..end + "</Envelope>".len()]
    } else {
        input
    }
}

fn parse_tree(input: &str) -> Option<Node> {
    let mut reader = Reader::from_str(input);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut stack: Vec<Node> = Vec::new();
    let mut prefixes: Vec<HashMap<String, String>> = vec![HashMap::new()];
    while let Ok(event) = reader.read_event_into(&mut buf) {
        match event {
            Event::Start(el) => {
                let mut map = prefixes.last().cloned().unwrap_or_default();
                take_xmlns(&el, &mut map);
                let node = start_node(&el, &map);
                prefixes.push(map);
                stack.push(node);
            }
            Event::Empty(el) => {
                let mut map = prefixes.last().cloned().unwrap_or_default();
                take_xmlns(&el, &mut map);
                push_child(&mut stack, start_node(&el, &map));
            }
            Event::Text(text) => {
                if let Some(parent) = stack.last_mut() {
                    if let Ok(value) = text.unescape() {
                        parent.text.push_str(&value);
                    }
                }
            }
            Event::End(_) => {
                prefixes.pop();
                let node = stack.pop()?;
                if stack.is_empty() {
                    return Some(node);
                }
                push_child(&mut stack, node);
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    stack.pop()
}

fn start_node(el: &quick_xml::events::BytesStart<'_>, prefixes: &HashMap<String, String>) -> Node {
    let raw = String::from_utf8_lossy(el.name().as_ref()).into_owned();
    let (prefix, local) = split_name(&raw);
    let ns = prefix
        .and_then(|name| prefixes.get(name))
        .map(String::as_str);
    let in_scan = ns.is_some_and(|uri| uri.contains(SCAN_NS_SUFFIX));
    let mut attrs = Vec::new();
    for attr in el.attributes().flatten() {
        let key = String::from_utf8_lossy(attr.key.as_ref()).into_owned();
        if key == "xmlns" || key.starts_with("xmlns:") {
            continue;
        }
        let (attr_prefix, name) = split_name(&key);
        let attr_ns = attr_prefix
            .and_then(|item| prefixes.get(item))
            .map(String::as_str);
        let Ok(value) = attr.unescape_value() else {
            continue;
        };
        attrs.push(Attr {
            name: name.to_string(),
            in_scan: attr_ns.is_some_and(|uri| uri.contains(SCAN_NS_SUFFIX)),
            value: value.into_owned(),
        });
    }
    Node {
        local: local.to_string(),
        in_scan,
        attrs,
        children: Vec::new(),
        text: String::new(),
    }
}

fn take_xmlns(el: &quick_xml::events::BytesStart<'_>, map: &mut HashMap<String, String>) {
    for attr in el.attributes().flatten() {
        let key = String::from_utf8_lossy(attr.key.as_ref()).into_owned();
        let Ok(value) = attr.unescape_value() else {
            continue;
        };
        if key == "xmlns" {
            map.insert(String::new(), value.into_owned());
        } else if let Some(prefix) = key.strip_prefix("xmlns:") {
            map.insert(prefix.to_string(), value.into_owned());
        }
    }
}

fn push_child(stack: &mut [Node], node: Node) {
    if let Some(parent) = stack.last_mut() {
        parent.children.push(node);
    }
}

fn split_name(name: &str) -> (Option<&str>, &str) {
    match name.split_once(':') {
        Some((prefix, local)) => (Some(prefix), local),
        None => (None, name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_lists_the_print_operations() {
        let schema = schema();
        for name in [
            "CreatePrintJobRequest",
            "CreatePrintJobResponse",
            "GetPrinterElementsResponse",
            "SendDocumentResponse",
        ] {
            assert!(schema.elements.contains_key(name), "{name}");
        }
    }

    fn status_envelope(state: &str) -> String {
        format!(
            r#"<soap:Envelope xmlns:soap="http://www.w3.org/2003/05/soap-envelope" xmlns:wprt="http://schemas.microsoft.com/windows/2006/08/wdp/print">
          <soap:Body><wprt:GetPrinterElementsResponse><wprt:PrinterElements>
            <wprt:ElementData Name="wprt:PrinterStatus" Valid="true">
              <wprt:PrinterStatus>
                <wprt:PrinterCurrentTime>2026-10-12T15:20:11Z</wprt:PrinterCurrentTime>
                <wprt:PrinterState>{state}</wprt:PrinterState>
                <wprt:QueuedJobCount>0</wprt:QueuedJobCount>
              </wprt:PrinterStatus>
            </wprt:ElementData>
          </wprt:PrinterElements></wprt:GetPrinterElementsResponse></soap:Body>
        </soap:Envelope>"#
        )
    }

    #[test]
    fn rejects_a_printer_state_outside_the_schema() {
        let problems = body_problems(&status_envelope("Busy"));
        assert!(problems.iter().any(|problem| problem.contains("Busy")), "{problems:?}");
    }

    #[test]
    fn accepts_idle_status() {
        let problems = body_problems(&status_envelope("Idle"));
        assert!(problems.is_empty(), "{problems:?}");
    }

    #[test]
    fn accepts_idle_status_in_the_2006_01_namespace() {
        let xml = status_envelope("Idle").replace(
            "http://schemas.microsoft.com/windows/2006/08/wdp/print",
            "http://schemas.microsoft.com/windows/2006/01/wdp/print",
        );
        let problems = body_problems(&xml);
        assert!(problems.is_empty(), "{problems:?}");
    }

    #[test]
    fn create_print_job_response_may_carry_lab_job_token() {
        let xml = r#"<soap:Envelope xmlns:soap="http://www.w3.org/2003/05/soap-envelope" xmlns:wprt="http://schemas.microsoft.com/windows/2006/08/wdp/print">
          <soap:Body><wprt:CreatePrintJobResponse>
            <wprt:JobId>1</wprt:JobId>
            <wprt:JobToken>job-1</wprt:JobToken>
          </wprt:CreatePrintJobResponse></soap:Body>
        </soap:Envelope>"#;
        assert!(body_problems(xml).is_empty(), "{:?}", body_problems(xml));
    }

    #[test]
    fn printer_status_may_carry_lab_printer_local_time() {
        let xml = r#"<soap:Envelope xmlns:soap="http://www.w3.org/2003/05/soap-envelope" xmlns:wprt="http://schemas.microsoft.com/windows/2006/08/wdp/print">
          <soap:Body><wprt:GetPrinterElementsResponse><wprt:PrinterElements>
            <wprt:ElementData Name="wprt:PrinterStatus" Valid="true">
              <wprt:PrinterStatus>
                <wprt:PrinterCurrentTime>2026-10-12T15:20:11Z</wprt:PrinterCurrentTime>
                <wprt:PrinterState>Idle</wprt:PrinterState>
                <wprt:QueuedJobCount>0</wprt:QueuedJobCount>
                <wprt:PrinterLocalTime>2026-01-01T00:00:00Z</wprt:PrinterLocalTime>
              </wprt:PrinterStatus>
            </wprt:ElementData>
          </wprt:PrinterElements></wprt:GetPrinterElementsResponse></soap:Body>
        </soap:Envelope>"#;
        assert!(body_problems(xml).is_empty(), "{:?}", body_problems(xml));
    }

    #[test]
    fn job_status_schema_counters_are_not_scan_images() {
        let xml = r#"<soap:Envelope xmlns:soap="http://www.w3.org/2003/05/soap-envelope" xmlns:wprt="http://schemas.microsoft.com/windows/2006/08/wdp/print">
          <soap:Body><wprt:GetJobElementsResponse><wprt:JobElements>
            <wprt:ElementData Name="wprt:JobStatus" Valid="true">
              <wprt:JobStatus>
                <wprt:JobId>1</wprt:JobId>
                <wprt:JobState>Pending</wprt:JobState>
                <wprt:JobStateReasons><wprt:JobStateReason>None</wprt:JobStateReason></wprt:JobStateReasons>
                <wprt:KOctetsProcessed>0</wprt:KOctetsProcessed>
                <wprt:MediaSheetsCompleted>0</wprt:MediaSheetsCompleted>
                <wprt:NumberOfDocuments>0</wprt:NumberOfDocuments>
              </wprt:JobStatus>
            </wprt:ElementData>
          </wprt:JobElements></wprt:GetJobElementsResponse></soap:Body>
        </soap:Envelope>"#;
        assert!(body_problems(xml).is_empty(), "{:?}", body_problems(xml));
        let bad = xml.replace(
            "<wprt:NumberOfDocuments>0</wprt:NumberOfDocuments>",
            "<wprt:NumberOfDocuments>0</wprt:NumberOfDocuments><wprt:ImagesCompleted>1</wprt:ImagesCompleted>",
        );
        assert!(
            body_problems(&bad).iter().any(|p| p.contains("ImagesCompleted")),
            "{:?}",
            body_problems(&bad),
        );
    }

    #[test]
    fn schema_lists_v11_and_v12_extensions() {
        let schema = schema();
        for name in [
            "SupportsWSPrintv11",
            "DriverConfiguration",
            "ColorRepresentation",
            "SupportsWSPrintV12",
            "SetPrinterElementsRequest",
            "SetPrinterElementsResponse",
        ] {
            assert!(schema.elements.contains_key(name), "{name}");
        }
    }

    #[test]
    fn schema_lists_v20_when_the_package_was_fetched() {
        let v20 = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../docs/reference/upstream/print-v2/WDPPrintV20.xsd");
        if !v20.is_file() {
            return;
        }
        let schema = schema();
        for name in [
            "SupportsWSPrintV20",
            "GetPrintDeviceCapabilitiesRequest",
            "GetPrintDeviceCapabilitiesResponse",
            "CreatePrintJob2Request",
        ] {
            assert!(schema.elements.contains_key(name), "{name}");
        }
    }

    fn extension_envelope(xmlns: &str, prefix: &str, inner: &str) -> String {
        format!(
            r#"<soap:Envelope xmlns:soap="http://www.w3.org/2003/05/soap-envelope" xmlns:{prefix}="{xmlns}">
              <soap:Body>{inner}</soap:Body>
            </soap:Envelope>"#
        )
    }

    #[test]
    fn v11_driver_configuration_and_color_are_checked() {
        let ok = extension_envelope(
            "http://schemas.microsoft.com/windows/2010/06/wdp/printv11",
            "wprt11",
            "<wprt11:DriverConfiguration><wprt11:GPDConfigFile>printer.gpd</wprt11:GPDConfigFile></wprt11:DriverConfiguration>",
        );
        assert!(body_problems(&ok).is_empty(), "{:?}", body_problems(&ok));
        let color = extension_envelope(
            "http://schemas.microsoft.com/windows/2010/06/wdp/printv11",
            "wprt11",
            "<wprt11:ColorRepresentation>000000</wprt11:ColorRepresentation>",
        );
        assert!(body_problems(&color).is_empty(), "{:?}", body_problems(&color));
        let short = color.replace("000000", "00000");
        assert!(
            body_problems(&short).iter().any(|problem| problem.contains("ColorRepresentation")),
            "{:?}",
            body_problems(&short)
        );
    }

    #[test]
    fn v12_set_printer_elements_request_is_checked() {
        let ok = extension_envelope(
            "http://schemas.microsoft.com/windows/2012/10/wdp/printV12",
            "wprt12",
            r#"<wprt12:SetPrinterElementsRequest><wprt12:ElementsToSet>
              <wprt12:ElementToSet>
                <wprt12:ElementPath>wprt:PrinterLocation</wprt12:ElementPath>
                <wprt12:String>lab</wprt12:String>
              </wprt12:ElementToSet>
            </wprt12:ElementsToSet></wprt12:SetPrinterElementsRequest>"#,
        );
        assert!(body_problems(&ok).is_empty(), "{:?}", body_problems(&ok));
    }

    #[test]
    fn v20_capabilities_request_is_checked_when_fetched() {
        let v20 = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../docs/reference/upstream/print-v2/WDPPrintV20.xsd");
        if !v20.is_file() {
            return;
        }
        let ok = extension_envelope(
            "http://schemas.microsoft.com/windows/2014/04/wdp/printV20",
            "wprt20",
            r#"<wprt20:GetPrintDeviceCapabilitiesRequest>
              <wprt20:PrintDeviceCapabilitiesFormat>application/vnd.ms-printer.pdc+xml</wprt20:PrintDeviceCapabilitiesFormat>
              <wprt20:PrintDeviceCapabilitiesChangeID>1</wprt20:PrintDeviceCapabilitiesChangeID>
            </wprt20:GetPrintDeviceCapabilitiesRequest>"#,
        );
        assert!(body_problems(&ok).is_empty(), "{:?}", body_problems(&ok));
    }

    #[test]
    fn capability_profiles_change_the_lists_and_still_validate() {
        use crate::profile::Profile;
        use crate::wsd::elements::get_printer_elements_body;

        fn profile(color: bool, formats: &[&str]) -> Profile {
            Profile {
                id: "t".into(),
                label: "Lab printer".into(),
                manufacturer: "fakeprint".into(),
                model: "lab".into(),
                device_id: "MFG:Fake;CLS:PRINTER;".into(),
                color_supported: color,
                pages_per_minute: 10,
                pages_per_minute_color: if color { 8 } else { 0 },
                document_formats: formats.iter().map(|value| (*value).to_string()).collect(),
                multiple_document_jobs: color,
            }
        }
        let ask = r#"<wprt:RequestedElements>
          <wprt:Name>wprt:PrinterDescription</wprt:Name>
          <wprt:Name>wprt:PrinterCapabilities</wprt:Name>
          <wprt:Name>wprt11:DriverConfiguration</wprt:Name>
          <wprt:Name>wprt11:SupportsWSPrintv11</wprt:Name>
          <wprt:Name>wprt12:SupportsWSPrintV12</wprt:Name>
          <wprt:Name>wprt20:SupportsWSPrintV20</wprt:Name>
        </wprt:RequestedElements>"#;
        let color = get_printer_elements_body(&profile(true, &["application/pdf", "image/pwg-raster"]), ask, 0);
        let mono = get_printer_elements_body(&profile(false, &["application/octet-stream"]), ask, 0);
        for xml in [&color, &mono] {
            let wrapped = extension_envelope(
                "http://schemas.microsoft.com/windows/2006/08/wdp/print",
                "wprt",
                xml,
            );
            assert!(body_problems(&wrapped).is_empty(), "{:?}\n{xml}", body_problems(&wrapped));
            assert!(!xml.contains("SupportsWSPrintv11>true"), "{xml}");
            assert!(!xml.contains("SupportsWSPrintV12>true"), "{xml}");
            assert!(!xml.contains("SupportsWSPrintV20>true"), "{xml}");
            assert!(xml.contains(r#"Name="wprt11:DriverConfiguration" Valid="false""#), "{xml}");
        }
        assert!(color.contains("<wprt:ColorSupported>true</wprt:ColorSupported>"), "{color}");
        assert!(color.contains("application/pdf") && color.contains("image/pwg-raster"), "{color}");
        assert!(mono.contains("<wprt:ColorSupported>false</wprt:ColorSupported>"), "{mono}");
        assert!(mono.contains("application/octet-stream"), "{mono}");
        assert!(!mono.contains("application/pdf"), "{mono}");
    }
}

