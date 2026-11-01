"""Check a SOAP body against the Print Service XSD in `docs/reference/schema`.

Stdlib-only (xml.etree + re) port of `crates/fakeprint/src/wsd/print_schema.rs`.
The schema's target namespace is `2006/08`; messages in `2006/01` use the same
local names, so either print namespace is accepted. SOAP faults and WS-Eventing
wrappers are not print elements; a print element nested in one (for example
`GetPrinterElementsResponse` / `PrinterElements`) is still checked. v1.1, v1.2,
and v2.0 extension XSDs are merged when present; the service does not advertise
those versions.

Lab allowances (stock XSD does not model these):
- **CreatePrintJobResponse:** optional same-NS `JobToken` after `JobId`
- **PrinterStatus:** optional same-NS `PrinterLocalTime` after `QueuedJobCount`
- **JobElementDataType:** `Name=…JobToken` may be empty or carry a `JobToken` child

Print `JobStatus` counters are schema (`KOctetsProcessed` / `MediaSheetsCompleted` /
`NumberOfDocuments`) — not scan `ImagesCompleted` / `ImagesToTransfer`.
"""

from __future__ import annotations

import re
from dataclasses import dataclass, field
from pathlib import Path
from xml.etree import ElementTree as ET

__all__ = [
    "AttrUse",
    "Model",
    "Particle",
    "Schema",
    "SimpleKind",
    "TypeBody",
    "body_problems",
    "load_schema",
]

MAX_PROBLEMS = 12
PRINT_NS_SUFFIX = "/wdp/print"

_KEYWORD_EXT = re.compile(r"^[A-Za-z0-9_]+:[A-Za-z0-9_.\-]+$")
_MIME_EXT = re.compile(r"^[A-Za-z0-9_][A-Za-z0-9_:/;\\+.\-]*$")
_BASE64 = re.compile(r"^[A-Za-z0-9+/=\s]*$")
_UTC_DATE = re.compile(
    r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d+)?(Z|[+-]\d{2}:\d{2})?$"
)
_IRE = re.compile(r"^[A-Za-z][A-Za-z0-9+.\-]*:[^\s]*$")


def _local_name(tag: str) -> str:
    return tag.rsplit("}", 1)[-1] if tag.startswith("{") else tag


def _ns(tag: str) -> str:
    return tag[1 : tag.index("}")] if tag.startswith("{") else ""


def _in_print(tag: str) -> bool:
    return PRINT_NS_SUFFIX in _ns(tag)


# --------------------------------------------------------------------------
# schema model
# --------------------------------------------------------------------------


@dataclass(frozen=True)
class SimpleKind:
    """Validated simple content kind.

    kind is one of: string, boolean, int, numeric, dateTime, anyURI, QName,
    token, NMTOKENS, base64Binary, pattern, union.
    """

    kind: str
    max: int | None = None
    exact: int | None = None
    min: int | float | None = None
    values: tuple[str, ...] = ()
    pattern: str | None = None  # "keywordExt" | "mimeExt"
    members: tuple[str, ...] = ()

    def label(self) -> str:
        if self.kind == "string":
            if self.exact is not None:
                return f"string[{self.exact}]"
            return f"string({self.max})" if self.max is not None else "string"
        if self.kind == "boolean":
            return "boolean"
        if self.kind in ("int", "numeric"):
            floor = self.kind + ">=" + str(self.min) if self.min is not None else self.kind
            return floor
        if self.kind in ("dateTime", "anyURI", "QName", "NMTOKENS", "base64Binary"):
            return self.kind
        if self.kind == "token":
            return "|".join(self.values) if self.values else "NMTOKEN"
        if self.kind == "pattern":
            return "namespace keyword" if self.pattern == "keywordExt" else "mime token"
        if self.kind == "union":
            return " or ".join(self.members)
        return self.kind


@dataclass(frozen=True)
class Particle:
    """Content particle.

    kind is 'element', 'any', or 'choice' (choice options live in `options`).
    `max` of None means unbounded.
    """

    kind: str
    name: str = ""
    type_name: str = ""
    min: int = 1
    max: int | None = 1
    options: tuple[Particle, ...] = ()


@dataclass(frozen=True)
class AttrUse:
    name: str
    in_schema_ns: bool
    required: bool
    simple: SimpleKind


@dataclass(frozen=True)
class TypeBody:
    kind: str  # 'simple' | 'complex'
    simple: SimpleKind | None = None
    base: str | None = None
    mode: str = "sequence"  # sequence | simpleContent | complexContent
    particles: tuple[Particle, ...] = ()
    attrs: tuple[AttrUse, ...] = ()


@dataclass(frozen=True)
class Model:
    particles: tuple[Particle, ...] = ()
    simple: SimpleKind | None = None
    attrs: tuple[AttrUse, ...] = ()
    any_attr: bool = False


@dataclass
class Schema:
    elements: dict[str, str] = field(default_factory=dict)
    types: dict[str, TypeBody] = field(default_factory=dict)
    attributes: dict[str, SimpleKind] = field(default_factory=dict)


class _Node:
    """Element view with resolved local name and print-NS flag."""

    __slots__ = ("local", "in_print", "attrs", "children", "text")

    def __init__(self, el: ET.Element) -> None:
        self.local = _local_name(el.tag)
        self.in_print = _in_print(el.tag)
        self.attrs: list[tuple[str, bool, str]] = []
        for key, value in el.attrib.items():
            if key == "xmlns" or key.startswith("xmlns:"):
                continue
            if key.startswith("{http://www.w3.org/2000/xmlns/}"):
                continue
            name = _local_name(key)
            self.attrs.append((name, _in_print(key), value))
        self.children = [_Node(child) for child in el]
        self.text = el.text or ""

    def attr(self, name: str) -> str | None:
        for item in self.attrs:
            if item[0] == name:
                return item[2]
        return None

    def child(self, name: str) -> _Node | None:
        return next((c for c in self.children if c.local == name), None)

    def child_attr(self, child: str, name: str) -> str | None:
        node = self.child(child)
        return node.attr(name) if node is not None else None


def _parse_tree(text: str) -> _Node | None:
    root = ET.fromstring(text)
    return _Node(root) if root is not None else None



def _value_ok(schema: Schema, simple: SimpleKind, text: str) -> bool:
    text = text.strip()
    if not text:
        return False
    if simple.kind == "string":
        count = len(text)
        if simple.exact is not None and count != simple.exact:
            return False
        return simple.max is None or count <= simple.max
    if simple.kind == "boolean":
        return text in ("true", "false", "1", "0")
    if simple.kind == "int":
        try:
            value = int(text)
        except ValueError:
            return False
        return simple.min is None or value >= simple.min
    if simple.kind == "numeric":
        try:
            value = float(text)
        except ValueError:
            return False
        return simple.min is None or value >= simple.min
    if simple.kind == "dateTime":
        return _utc_date(text)
    if simple.kind == "anyURI":
        return _ire(text)
    if simple.kind == "QName":
        return not any(ch.isspace() for ch in text)
    if simple.kind == "token":
        if simple.max is not None and len(text) > simple.max:
            return False
        if simple.values:
            return text in simple.values
        return True
    if simple.kind == "NMTOKENS":
        return all(tok for tok in text.split())
    if simple.kind == "base64Binary":
        return bool(_BASE64(text))
    if simple.kind == "pattern":
        if simple.pattern == "keywordExt":
            return bool(_KEYWORD_EXT.match(text))
        return bool(_MIME_EXT.match(text))
    if simple.kind == "union":
        for member in simple.members:
            builtin = _builtin(member)
            if builtin is not None:
                if _value_ok(schema, builtin, text):
                    return True
                continue
            body = schema.types.get(member)
            if body is not None and body.kind == "simple" and body.simple is not None:
                if _value_ok(schema, body.simple, text):
                    return True
        return False
    return True


def _utc_date(text: str) -> bool:
    if len(text) < 20 or text[10] != "T":
        return False
    return _UTC_DATE.match(text) is not None


def _ire(text: str) -> bool:
    if any(ch.isspace() for ch in text):
        return False
    return bool(_IRE(text))


def _builtin(name: str) -> SimpleKind | None:
    if name == "xs:boolean":
        return SimpleKind("boolean")
    if name == "xs:int":
        return SimpleKind("int")
    if name == "xs:string":
        return SimpleKind("string")
    if name == "xs:dateTime":
        return SimpleKind("dateTime")
    if name == "xs:anyURI":
        return SimpleKind("anyURI")
    if name == "xs:QName":
        return SimpleKind("QName")
    if name == "xs:NMTOKEN":
        return SimpleKind("token")
    if name == "xs:NMTOKENS":
        return SimpleKind("NMTOKENS")
    if name == "xs:base64Binary":
        return SimpleKind("base64Binary")
    return None



def _type_key(value: str) -> str:
    if value.startswith("xs:"):
        return value
    return value.rsplit(":", 1)[-1]


def _opt_int(raw: str | None) -> int | None:
    if raw is None:
        return None
    try:
        return int(raw)
    except ValueError:
        return None


def _opt_number(raw: str | None) -> int | float | None:
    if raw is None:
        return None
    try:
        return int(raw)
    except ValueError:
        pass
    try:
        return float(raw)
    except ValueError:
        return None


def _min_occurs(node: _Node) -> int:
    value = _opt_int(node.attr("minOccurs"))
    return 1 if value is None else value


def _max_occurs(node: _Node) -> int | None:
    raw = node.attr("maxOccurs")
    if raw is None:
        return 1
    if raw == "unbounded":
        return None
    value = _opt_int(raw)
    return 1 if value is None else value


def _simple_from_name(schema: Schema, type_name: str) -> SimpleKind:
    key = _type_key(type_name)
    builtin = _builtin(key)
    if builtin is not None:
        return builtin
    body = schema.types.get(key)
    if body is not None and body.kind == "simple" and body.simple is not None:
        return body.simple
    return SimpleKind("string")


def _simple_kind(node: _Node) -> SimpleKind:
    union = node.child("union")
    if union is not None:
        members = tuple(_type_key(part) for part in (union.attr("memberTypes") or "").split())
        return SimpleKind("union", members=members)
    restriction = node.child("restriction")
    if restriction is None:
        return SimpleKind("string")
    base = _type_key(restriction.attr("base") or "xs:string")
    enums = tuple(
        child.attr("value")
        for child in restriction.children
        if child.local == "enumeration" and child.attr("value") is not None
    )
    max_len = _opt_int(restriction.child_attr("maxLength", "value"))
    exact_len = _opt_int(restriction.child_attr("length", "value"))
    min_val = _opt_number(restriction.child_attr("minInclusive", "value"))
    if restriction.child_attr("pattern", "value") is not None:
        pattern = restriction.child_attr("pattern", "value")
        kind = "keywordExt" if ":" in (pattern or "") else "mimeExt"
        return SimpleKind("pattern", pattern=kind)
    if base in ("xs:int", "xs:integer", "xs:long") and min_val is not None:
        return SimpleKind("int" if isinstance(min_val, int) else "numeric", min=min_val)
    if base in ("xs:double", "xs:decimal", "xs:float") and min_val is not None:
        return SimpleKind("numeric", min=min_val)
    if enums or base == "xs:NMTOKEN":
        return SimpleKind("token", max=max_len, values=enums)
    if base == "xs:boolean":
        return SimpleKind("boolean")
    return SimpleKind("string", max=max_len, exact=exact_len)


def _particles_of(node: _Node, schema: Schema, anon: list[int]) -> tuple[Particle, ...]:
    out: list[Particle] = []
    for child in node.children:
        if child.local == "element":
            out.append(
                Particle(
                    kind="element",
                    name=child.attr("name") or "",
                    type_name=_declare_element(child, schema, anon, False),
                    min=_min_occurs(child),
                    max=_max_occurs(child),
                )
            )
        elif child.local == "any":
            out.append(Particle(kind="any", min=_min_occurs(child), max=_max_occurs(child)))
        elif child.local == "choice":
            out.append(Particle(kind="choice", options=_particles_of(child, schema, anon)))
    return tuple(out)


def _declare_element(node: _Node, schema: Schema, anon: list[int], global_el: bool) -> str:
    type_name = node.attr("type") or "xs:string"
    complex_type = node.child("complexType")
    if complex_type is None:
        return type_name
    if node.child("complexContent") is None:
        anon[0] += 1
        label = f"_element_{anon[0]}"
        _define_complex(complex_type, label, schema, anon, global_el)
        return label
    return type_name


def _attrs_of(node: _Node, schema: Schema) -> tuple[AttrUse, ...]:
    out: list[AttrUse] = []
    for child in node.children:
        if child.local != "attribute":
            continue
        if child.attr("ref") is not None:
            name = (child.attr("ref") or "").rsplit(":", 1)[-1]
            simple = schema.attributes.get(name, SimpleKind("string"))
        else:
            name = child.attr("name")
            if name is None:
                continue
            type_name = child.attr("type")
            if type_name:
                simple = _simple_from_name(schema, type_name)
            elif child.child("simpleType") is not None:
                simple = _simple_kind(child.child("simpleType"))
            else:
                simple = SimpleKind("string")
        out.append(
            AttrUse(
                name=name,
                in_schema_ns=child.attr("ref") is not None,
                required=child.attr("use") == "required",
                simple=simple,
            )
        )
    return tuple(out)



def _define_complex(node: _Node, name: str, schema: Schema, anon: list[int], _global_type: bool) -> None:
    simple_content = node.child("simpleContent")
    if simple_content is not None:
        ext = simple_content.child("extension")
        if ext is not None:
            base = _type_key(ext.attr("base") or "xs:string")
            schema.types[name] = TypeBody(kind="complex", base=base, mode="simpleContent", attrs=_attrs_of(ext, schema))
        else:
            schema.types[name] = TypeBody(kind="complex", base="xs:string", mode="simpleContent")
        return
    complex_content = node.child("complexContent")
    if complex_content is not None:
        ext = complex_content.child("extension")
        if ext is not None:
            base = _type_key(ext.attr("base") or "xs:anyType")
            schema.types[name] = TypeBody(
                kind="complex",
                base=base,
                mode="complexContent",
                particles=_particles_of(ext, schema, anon),
                attrs=_attrs_of(ext, schema),
            )
        return
    sequence = node.child("sequence")
    choice = node.child("choice")
    if choice is not None and sequence is None:
        particles = (Particle(kind="choice", options=_particles_of(choice, schema, anon)),)
    elif sequence is not None:
        particles = _particles_of(sequence, schema, anon)
    else:
        particles = ()
    schema.types[name] = TypeBody(kind="complex", mode="sequence", particles=particles, attrs=_attrs_of(node, schema))


def _ingest_roots(schema: Schema, roots: list[_Node], anon: list[int]) -> None:
    for root in roots:
        for child in root.children:
            if child.local == "simpleType" and child.attr("name"):
                if child.child("union") is not None or child.child("restriction") is not None:
                    schema.types[child.attr("name")] = TypeBody(kind="simple", simple=_simple_kind(child))
            elif child.local == "attribute" and child.attr("name"):
                if child.attr("type"):
                    schema.attributes[child.attr("name")] = _simple_from_name(schema, child.attr("type"))
                elif child.child("simpleType") is not None:
                    schema.attributes[child.attr("name")] = _simple_kind(child.child("simpleType"))
                else:
                    schema.attributes[child.attr("name")] = SimpleKind("string")
            elif child.local == "complexType" and child.attr("name"):
                _define_complex(child, child.attr("name"), schema, anon, True)
            elif child.local == "element" and child.attr("name"):
                schema.elements[child.attr("name")] = _declare_element(child, schema, anon, True)


def _load_schema(schema_dir: Path) -> Schema:
    schema = Schema()
    anon = [0]
    roots: list[_Node] = []
    if schema_dir.is_dir():
        for path in sorted(schema_dir.glob("*.xsd")):
            try:
                text = path.read_text(encoding="utf-8")
            except OSError as exc:
                raise RuntimeError(f"print schema {path}: {exc}") from exc
            try:
                node = _parse_tree(text)
            except ET.ParseError as exc:
                raise RuntimeError(f"print schema {path}: {exc}") from exc
            if node is not None:
                roots.append(node)
    v20 = schema_dir.parent / "upstream" / "print-v2" / "WDPPrintV20.xsd"
    if v20.is_file():
        try:
            text = v20.read_text(encoding="utf-8")
        except OSError as exc:
            raise RuntimeError(f"print schema {v20}: {exc}") from exc
        try:
            node = _parse_tree(text)
        except ET.ParseError as exc:
            raise RuntimeError(f"print schema {v20}: {exc}") from exc
        if node is not None:
            roots.append(node)
    _ingest_roots(schema, roots, anon)
    return schema


def parse_schema(text: str | Path) -> Schema:
    """Parse a single XSD document (str or file path) into a Schema."""
    source = text.read_text(encoding="utf-8") if isinstance(text, Path) else text
    schema = Schema()
    anon = [0]
    node = _parse_tree(source)
    if node is not None:
        _ingest_roots(schema, [node], anon)
    return schema


_SCHEMA_CACHE: dict[Path, Schema] = {}


def _default_schema_dir() -> Path:
    here = Path(__file__).resolve().parents[2]
    return here / "docs" / "reference" / "schema"


def load_schema(schema_dir: Path | str | None = None) -> Schema:
    """Load the XSD directory (default: `docs/reference/schema` at the repo root)."""
    key = Path(schema_dir).resolve() if schema_dir is not None else _default_schema_dir()
    if key not in _SCHEMA_CACHE:
        _SCHEMA_CACHE[key] = _load_schema(key)
    return _SCHEMA_CACHE[key]


def _merge_attrs(parent: tuple[AttrUse, ...], own: tuple[AttrUse, ...]) -> tuple[AttrUse, ...]:
    merged = list(parent)
    for attr in own:
        slot = next(
            (
                index
                for index, item in enumerate(merged)
                if item.name == attr.name and item.in_schema_ns == attr.in_schema_ns
            ),
            None,
        )
        if slot is not None:
            merged[slot] = attr
        else:
            merged.append(attr)
    return tuple(merged)


def _find_body(node: _Node) -> _Node | None:
    if node.local == "Body":
        return node
    for child in node.children:
        found = _find_body(child)
        if found is not None:
            return found
    return None


def _check_tree(node: _Node, schema: Schema, problems: list[str]) -> None:
    if node.local == "Body":
        for child in node.children:
            _check_body_child(child, schema, problems)
        return
    if node.local == "Envelope":
        for child in node.children:
            _check_tree(child, schema, problems)


def _element_data_is_unrecognized(node: _Node) -> bool:
    return (
        node.local == "ElementData"
        and not node.children
        and node.attr("Valid") in ("false", "0")
    )


def _element_data_is_job_token(node: _Node) -> bool:
    if node.local != "ElementData":
        return False
    name = node.attr("Name") or ""
    return name == "JobToken" or name.rsplit(":", 1)[-1] == "JobToken"


def _check_job_token_element_data(node: _Node, schema: Schema, path: str, problems: list[str]) -> None:
    children = node.children
    if not children:
        return
    if len(children) == 1 and children[0].local == "JobToken" and children[0].in_print:
        model = _model_of(schema, "String255ExtType", [])
        _check_model(schema, model, children[0], path + "/JobToken", problems)
        return
    if len(problems) < MAX_PROBLEMS:
        names = ", ".join(child.local for child in children)
        problems.append(f"{path} JobToken ElementData expected empty or JobToken, found {names}")


def _check_attrs(
    node: _Node,
    model: Model,
    schema: Schema,
    path: str,
    problems: list[str],
) -> None:
    for attr in model.attrs:
        found = next(
            (item for item in node.attrs if item[0] == attr.name and item[1] == attr.in_schema_ns),
            None,
        )
        if attr.required and found is None and len(problems) < MAX_PROBLEMS:
            problems.append(f"{path} is missing attribute {attr.name}")
        if found is not None:
            value = found[2]
            if not _value_ok(schema, attr.simple, value) and len(problems) < MAX_PROBLEMS:
                problems.append(
                    f"{path}/@{attr.name} value '{value}' does not match {attr.simple.label()}"
                )
    for name, in_schema_ns, value in node.attrs:
        if in_schema_ns:
            known = any(item.name == name and item.in_schema_ns for item in model.attrs)
            if not known and len(problems) < MAX_PROBLEMS:
                problems.append(f"{path} has unexpected attribute {name}")
        else:
            known = any(item.name == name and not item.in_schema_ns for item in model.attrs)
            allowed = known or model.any_attr or name == "lang"
            if not allowed and len(problems) < MAX_PROBLEMS:
                problems.append(f"{path} has unexpected attribute {name}")


def _particle_min(particle: Particle) -> int:
    if particle.kind in ("element", "any"):
        return particle.min
    if particle.kind == "choice":
        return min((_particle_min(option) for option in particle.options), default=0)
    return 0


def _particle_names(particle: Particle) -> list[str]:
    if particle.kind == "element" and particle.name:
        return [particle.name]
    if particle.kind == "choice":
        names: list[str] = []
        for option in particle.options:
            names.extend(_particle_names(option))
        return names
    return []


def _lab_named(node: _Node, local: str) -> bool:
    return node.in_print and node.local == local


def _walk_particles(
    particles: tuple[Particle, ...],
    children: list[_Node],
    schema: Schema,
    path: str,
    problems: list[str],
) -> None:
    index = 0
    for particle in particles:
        if len(problems) >= MAX_PROBLEMS:
            return
        index = _consume(particle, children, index, schema, path, problems)
    if path.endswith("CreatePrintJobResponse"):
        while index < len(children) and _lab_named(children[index], "JobToken"):
            index += 1
    if path.endswith("PrinterStatus"):
        while index < len(children) and _lab_named(children[index], "PrinterLocalTime"):
            index += 1
    if index < len(children) and len(problems) < MAX_PROBLEMS:
        extra = ", ".join(child.local for child in children[index:])
        problems.append(f"{path} has unexpected child {extra}")


def _consume(
    particle: Particle,
    children: list[_Node],
    index: int,
    schema: Schema,
    path: str,
    problems: list[str],
) -> int:
    if particle.kind == "element":
        count = 0
        while (
            index < len(children)
            and children[index].local == particle.name
            and children[index].in_print
        ):
            if particle.max is not None and count >= particle.max:
                break
            child_path = f"{path}/{particle.name}"
            model = _model_of(schema, particle.type_name, [])
            _check_model(schema, model, children[index], child_path, problems)
            count += 1
            index += 1
        if count < particle.min and len(problems) < MAX_PROBLEMS:
            problems.append(f"{path} is missing {particle.name}")
        return index
    if particle.kind == "any":
        count = 0
        while index < len(children) and not children[index].in_print:
            if particle.max is not None and count >= particle.max:
                break
            count += 1
            index += 1
        if count < particle.min and len(problems) < MAX_PROBLEMS:
            problems.append(f"{path} is missing an extension element")
        return index
    if particle.kind == "choice":
        if index >= len(children):
            if all(_particle_min(option) > 0 for option in particle.options) and len(problems) < MAX_PROBLEMS:
                problems.append(f"{path} is missing a choice")
            return index
        next_child = children[index]
        for option in particle.options:
            if next_child.local in _particle_names(option):
                return _consume(option, children, index, schema, path, problems)
        if any(option.kind == "any" for option in particle.options) and not next_child.in_print:
            return index + 1
        names = "|".join(
            name for option in particle.options for name in _particle_names(option)
        )
        if len(problems) < MAX_PROBLEMS:
            problems.append(f"{path} choice expected {names}, found {next_child.local}")
        return index + 1
    return index


def _check_model(
    schema: Schema,
    model: Model,
    node: _Node,
    path: str,
    problems: list[str],
) -> None:
    if len(problems) >= MAX_PROBLEMS:
        return
    _check_attrs(node, model, schema, path, problems)
    if model.simple is not None:
        foreign = next((child for child in node.children if not child.in_print), None)
        if foreign is not None and any(child.in_print for child in node.children):
            if len(problems) < MAX_PROBLEMS:
                problems.append(f"{path} mixes print children into a simple value")
        elif not _value_ok(schema, model.simple, node.text.strip()) and len(problems) < MAX_PROBLEMS:
            problems.append(
                f"{path} value '{node.text.strip()}' does not match {model.simple.label()}"
            )
    if _element_data_is_unrecognized(node):
        return
    if _element_data_is_job_token(node):
        _check_job_token_element_data(node, schema, path, problems)
        return
    if model.particles or model.simple is None:
        _walk_particles(model.particles, node.children, schema, path, problems)


def _validate_element(node: _Node, schema: Schema, problems: list[str]) -> None:
    type_name = schema.elements.get(node.local)
    if type_name is None:
        return
    model = _model_of(schema, type_name, [])
    _check_model(schema, model, node, node.local, problems)


def _check_known_descendants(node: _Node, schema: Schema, problems: list[str]) -> None:
    for child in node.children:
        if child.in_print and child.local in schema.elements:
            _validate_element(child, schema, problems)
        else:
            _check_known_descendants(child, schema, problems)


def _check_body_child(node: _Node, schema: Schema, problems: list[str]) -> None:
    if len(problems) >= MAX_PROBLEMS:
        return
    if node.in_print:
        if node.local in schema.elements:
            _validate_element(node, schema, problems)
        else:
            if len(problems) < MAX_PROBLEMS:
                problems.append(f"{node.local} is not a print schema element")
            _check_known_descendants(node, schema, problems)
        return
    for child in node.children:
        if child.in_print and child.local in schema.elements:
            _validate_element(child, schema, problems)


def body_problems(soap_xml: str) -> list[str]:
    """Validate a SOAP body against the WSD print schema.

    Returns human-readable problems; an empty list means the body is valid.
    Only print-namespaced elements are checked, so non-print envelopes and
    faults pass through untouched.
    """
    try:
        root = _parse_tree(soap_xml)
    except ET.ParseError:
        return ["body is not XML"]
    if root is None:
        return ["body is not XML"]
    schema = load_schema()
    problems: list[str] = []
    _check_tree(root, schema, problems)
    return problems


def _model_of(schema: Schema, type_name: str, stack: list[str]) -> Model:
    key = _type_key(type_name)
    if key in stack:
        return Model(any_attr=True)
    stack.append(key)
    try:
        builtin = _builtin(key)
        if builtin is not None:
            return Model(simple=builtin, any_attr=True)
        body = schema.types.get(key)
        if body is None:
            return Model(any_attr=True)
        if body.kind == "simple" and body.simple is not None:
            return Model(simple=body.simple, any_attr=True)
        if body.kind != "complex":
            return Model(any_attr=True)
        if body.mode == "simpleContent":
            base_model = _model_of(schema, body.base or "xs:string", stack) if body.base else Model(any_attr=True)
            return Model(
                simple=base_model.simple,
                attrs=_merge_attrs(base_model.attrs, body.attrs),
                any_attr=True,
            )
        if body.mode == "complexContent":
            base_model = _model_of(schema, body.base or "xs:anyType", stack) if body.base else Model(any_attr=True)
            return Model(
                particles=base_model.particles + body.particles,
                simple=base_model.simple,
                attrs=_merge_attrs(base_model.attrs, body.attrs),
                any_attr=True,
            )
        return Model(particles=body.particles, attrs=body.attrs, any_attr=True)
    finally:
        stack.pop()

