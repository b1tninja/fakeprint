//! Validator for WS-Print message bodies, driven by compact tables written for this project.
//!
//! The tables below restate, in our own words and notation, what the published WS-Print
//! contract requires of every body the emulator sends or receives: which children an element
//! has, in which order, how often; which attributes it carries; which value domain every text
//! node belongs to. Nothing here reads the Microsoft package, so the validator works from a
//! bare checkout. A test-only cross-check (see the tests) loads the locally generated
//! `docs/reference/schema/schema-model.json` when it exists and proves the tables agree with it.
//!
//! Notation. A complex type is a [`Ct`]: a name (the schema's type name, without prefix), the
//! namespace family it lives in, a [`Shape`] (ordered sequence, one-of choice, or text) and its
//! attributes. Children of a type are always in the type's own namespace. `Item::Any` is the
//! schema's "other namespace" extension slot: elements from a namespace other than the type's
//! own. Those of the known extension families (v1.1, v1.2, v2.0) are validated; elements from
//! unknown vendor namespaces are tolerated, as a consumer must.
//!
//! Entry points: [`validate_body`] (an operation message, an event or a section, by [`Target`])
//! and [`validate_root`] (the element's own name selects the type).

use std::fmt;

use super::types::PrintOp;
use super::xml::{self, ns, Element};

const UNB: u32 = u32::MAX;
const MAX_DEPTH: usize = 40;

// ---------------------------------------------------------------------------------------------
// Public result types
// ---------------------------------------------------------------------------------------------

/// What kind of rule was broken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViolationKind {
    /// A required child (or a required choice) is absent.
    Missing,
    /// A child the type does not allow at all.
    Unexpected,
    /// A child that exists in the type but at the wrong place or too often.
    OutOfOrder,
    /// A text value outside its domain.
    BadValue,
    /// An attribute the type does not declare, or one with a bad value.
    BadAttribute,
    /// A required attribute is absent.
    MissingAttribute,
    /// The root element is not the expected one.
    WrongRoot,
    /// The table itself is inconsistent (reported instead of panicking).
    Internal,
}

/// One broken rule, located by a slash path from the checked element.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    pub path: String,
    pub kind: ViolationKind,
    pub message: String,
}

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path, self.message)
    }
}

/// Switches. [`Options::STRICT`] is the default and the one replies must pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Options {
    /// Also check that capability `AllowedValue` lists draw from the value's own domain (the
    /// schema types the lists as plain token lists; a printer advertising an orientation the
    /// ticket could never carry is still wrong).
    pub strict_domains: bool,
    /// Tolerate the vendor `JobToken` element after `JobId` in a CreatePrintJob reply
    /// (opt-in profile quirk `job_token`).
    pub quirk_job_token: bool,
    /// Tolerate the non-schema `PrinterLocalTime` after `QueuedJobCount` (quirk
    /// `printer_local_time`).
    pub quirk_printer_local_time: bool,
}

impl Options {
    pub const STRICT: Options = Options {
        strict_domains: true,
        quirk_job_token: false,
        quirk_printer_local_time: false,
    };
    /// Structure only, with both lab quirks tolerated (used for traffic from older code).
    pub const LENIENT: Options = Options {
        strict_domains: false,
        quirk_job_token: true,
        quirk_printer_local_time: true,
    };
}

impl Default for Options {
    fn default() -> Self {
        Options::STRICT
    }
}

/// What a body is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target<'a> {
    /// The request body of an operation.
    Request(PrintOp),
    /// The response body of an operation.
    Response(PrintOp),
    /// An event body, by event element name (`JobEndStateEvent`, ...).
    Event(&'a str),
    /// A section as found inside `ElementData` (`PrinterStatus`, `PrinterCapabilities`,
    /// `JobStatus`, `PrintTicket`, `DefaultPrintTicket`, ...), by local name.
    Section(&'a str),
}

// ---------------------------------------------------------------------------------------------
// Table vocabulary
// ---------------------------------------------------------------------------------------------

/// Namespace family of a type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ns {
    Print,
    Print11,
    Print12,
    Print20,
}

impl Ns {
    pub const fn uri(self) -> &'static str {
        match self {
            Ns::Print => ns::PRINT,
            Ns::Print11 => ns::PRINT11,
            Ns::Print12 => ns::PRINT12,
            Ns::Print20 => ns::PRINT20,
        }
    }
    fn of_uri(uri: &str) -> Option<Ns> {
        [Ns::Print, Ns::Print11, Ns::Print12, Ns::Print20]
            .into_iter()
            .find(|n| n.uri() == uri)
    }
}

/// One slot of a content model.
#[derive(Debug, Clone, Copy)]
pub enum Item {
    /// A child element of the given local name and type, `min..max` times.
    El {
        name: &'static str,
        min: u32,
        max: u32,
        ty: &'static str,
    },
    /// Elements from other namespaces, `min..max` times.
    Any { min: u32, max: u32 },
    /// Exactly one of these `(name, type)` elements (a choice nested in a sequence).
    OneOf(&'static [(&'static str, &'static str)]),
}

const fn el(name: &'static str, min: u32, max: u32, ty: &'static str) -> Item {
    Item::El { name, min, max, ty }
}
/// Exactly one.
const fn r(name: &'static str, ty: &'static str) -> Item {
    el(name, 1, 1, ty)
}
/// Zero or one.
const fn o(name: &'static str, ty: &'static str) -> Item {
    el(name, 0, 1, ty)
}
/// Zero or more.
const fn m(name: &'static str, ty: &'static str) -> Item {
    el(name, 0, UNB, ty)
}
/// One or more.
const fn s(name: &'static str, ty: &'static str) -> Item {
    el(name, 1, UNB, ty)
}
const ANY: Item = Item::Any { min: 0, max: UNB };
const ANY1: Item = Item::Any { min: 0, max: 1 };

/// How the content of a complex type is organised.
#[derive(Debug, Clone, Copy)]
pub enum Shape {
    /// Children in this order.
    Seq(&'static [Item]),
    /// One child of the listed names (or foreign elements, if `Any` is listed).
    Choice(&'static [Item]),
    /// Text only, drawn from the named simple type.
    Text(&'static str),
}

/// An attribute a type declares.
#[derive(Debug, Clone, Copy)]
pub struct At {
    pub name: &'static str,
    /// True when written with the print namespace (`wprt:MustHonor`); false for local ones.
    pub qualified: bool,
    pub required: bool,
    pub ty: &'static str,
}

/// A named complex type.
#[derive(Debug, Clone, Copy)]
pub struct Ct {
    pub name: &'static str,
    pub ns: Ns,
    pub shape: Shape,
    pub attrs: &'static [At],
}

const NOA: &[At] = &[];
const fn seq(name: &'static str, items: &'static [Item]) -> Ct {
    Ct {
        name,
        ns: Ns::Print,
        shape: Shape::Seq(items),
        attrs: NOA,
    }
}
const fn seq_a(name: &'static str, items: &'static [Item], attrs: &'static [At]) -> Ct {
    Ct {
        name,
        ns: Ns::Print,
        shape: Shape::Seq(items),
        attrs,
    }
}
const fn seq_n(ns: Ns, name: &'static str, items: &'static [Item]) -> Ct {
    Ct {
        name,
        ns,
        shape: Shape::Seq(items),
        attrs: NOA,
    }
}
const fn choice(name: &'static str, ns: Ns, items: &'static [Item], attrs: &'static [At]) -> Ct {
    Ct {
        name,
        ns,
        shape: Shape::Choice(items),
        attrs,
    }
}
const fn txt(name: &'static str, simple: &'static str, attrs: &'static [At]) -> Ct {
    Ct {
        name,
        ns: Ns::Print,
        shape: Shape::Text(simple),
        attrs,
    }
}

const MUST: &[At] = &[At {
    name: "MustHonor",
    qualified: true,
    required: false,
    ty: "boolean",
}];
const REMOVED_AT: At = At {
    name: "Removed",
    qualified: true,
    required: false,
    ty: "boolean",
};
const fn name_attr(ty: &'static str) -> At {
    At {
        name: "Name",
        qualified: false,
        required: true,
        ty,
    }
}
const NAME255_REMOVED: &[At] = &[name_attr("String255BaseType"), REMOVED_AT];
const INBIN_ATTRS: &[At] = &[name_attr("InputBinNameType"), REMOVED_AT];
const OUTBIN_ATTRS: &[At] = &[name_attr("OutputBinNameType"), REMOVED_AT];
const COND_ATTRS: &[At] = &[At {
    name: "Id",
    qualified: false,
    required: true,
    ty: "ConditionIdType",
}];
const ELDATA_ATTRS: &[At] = &[
    At {
        name: "Name",
        qualified: false,
        required: true,
        ty: "QName",
    },
    At {
        name: "Valid",
        qualified: false,
        required: true,
        ty: "boolean",
    },
];

// ---------------------------------------------------------------------------------------------
// Simple types
// ---------------------------------------------------------------------------------------------

/// How an enumerated domain may be extended beyond its listed values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ext {
    /// The list is closed.
    Closed,
    /// `namespace:keyword` vendor tokens.
    Keyword,
    /// MIME-looking strings.
    Mime,
    /// `<prefix>_<name>_<w>x<h>(in|mm)` media size names.
    MediaSize,
    /// `custom-media-color-<name>`.
    MediaColor,
    /// `custom-media-type-<name>`.
    MediaType,
    /// `Tray<n>`.
    InputTray,
    /// `Mailbox<n>`, `Stacker<n>`, `Bin<n>`.
    OutputBin,
}

/// A text value domain.
#[derive(Debug, Clone, Copy)]
pub enum Simple {
    Bool,
    /// Integer within inclusive bounds.
    Int(i64, i64),
    /// String of at most this many characters.
    Str(usize),
    /// String of exactly this many characters.
    Len(usize),
    /// A name token of at most this many characters.
    Token(usize),
    /// One or more name tokens separated by white space.
    Tokens,
    DateTime,
    Uri,
    QName,
    Base64,
    Double,
    /// Listed values, optionally extensible; `xsd` names the schema type it restates.
    Enum {
        xsd: &'static str,
        vals: &'static [&'static str],
        ext: Ext,
    },
}

const I32MAX: i64 = i32::MAX as i64;
const I32MIN: i64 = i32::MIN as i64;

const PRINTER_STATES: &[&str] = &["Idle", "Processing", "Stopped"];
const PRINTER_REASONS: &[&str] = &[
    "AttentionRequired",
    "DoorOpen",
    "MarkerSupplyEmpty",
    "MarkerSupplyLow",
    "MediaEmpty",
    "MediaJam",
    "MediaLow",
    "MediaNeeded",
    "None",
    "Paused",
    "OutputAreaAlmostFull",
    "OutputAreaFull",
];
const JOB_STATES: &[&str] = &[
    "Aborted",
    "Canceled",
    "Completed",
    "Pending",
    "Pending-Held",
    "Processing",
    "ProcessingStopped",
    "Started",
    "Terminating",
];
const JOB_REASONS: &[&str] = &[
    "CompressionError",
    "DocumentAccessError",
    "DocumentFormatError",
    "DocumentTimeoutError",
    "DocumentTransferError",
    "JobCanceledAtDevice",
    "JobCanceledByUser",
    "JobCompletedSuccessfully",
    "JobCompletedWithErrors",
    "JobCompletedWithWarnings",
    "JobIncoming",
    "JobPasswordWait",
    "JobPrinting",
    "None",
    "PrinterStopped",
    "UnsupportedCompression",
    "UnsupportedDocumentFormat",
];
const COMPRESSIONS: &[&str] = &["None", "Gzip", "Compress", "Deflate"];
const FORMATS: &[&str] = &[
    "application/octet-stream",
    "application/postscript",
    "application/vnd.hp-PCL",
    "application/vnd.ms-xpsdocument",
    "application/vnd.pwg-xhtml-print+xml",
    "image/g3fax",
    "image/gif",
    "image/jpeg",
    "image/png",
    "image/tiff",
    "image/tiff-fx",
    "text/html",
    "text/plain",
    "text/plain;charset=utf-8",
    "unknown",
];
const MEDIA_SIZES: &[&str] = &[
    "iso_a4_210x297mm",
    "iso_c5_162x229mm",
    "iso_dl_110x220mm",
    "jis_b4_257x364mm",
    "na_legal_8.5x14in",
    "na_letter_8.5x11in",
    "pwg_letter-or-a4_choice",
];
const MEDIA_TYPES: &[&str] = &[
    "cardstock",
    "envelope",
    "labels",
    "photographic",
    "photographic-glossy",
    "photographic-matte",
    "stationery",
    "stationery-inkjet",
    "transparency",
    "other",
    "unknown",
];
const MEDIA_COLORS: &[&str] = &[
    "unknown",
    "white",
    "pink",
    "yellow",
    "buff",
    "goldenrod",
    "blue",
    "green",
    "red",
    "gray",
    "ivory",
    "orange",
    "other",
    "no-color",
];
const ORIENTATIONS: &[&str] = &[
    "Landscape",
    "Portrait",
    "ReverseLandscape",
    "ReversePortrait",
];
const QUALITIES: &[&str] = &["Draft", "High", "Normal", "Photo"];
const SIDES: &[&str] = &["OneSided", "TwoSidedLongEdge", "TwoSidedShortEdge"];
const NUP_DIRECTIONS: &[&str] = &["RightDown", "DownRight", "LeftDown", "DownLeft"];
const STAPLE_LOCATIONS: &[&str] = &[
    "None",
    "StapleBottomLeft",
    "StapleBottomRight",
    "StapleTopLeft",
    "StapleTopRight",
    "StapleDualBottom",
    "StapleDualLeft",
    "StapleDualRight",
    "StapleDualTop",
    "SaddleStitch",
    "other",
    "unknown",
];
const STAPLE_ANGLES: &[&str] = &[
    "unknown",
    "NotApplicable",
    "Any",
    "Horizontal",
    "Slanted",
    "Vertical",
];
const PUNCH_EDGES: &[&str] = &["None", "Top", "Bottom", "Left", "Right"];
const PUNCH_PATTERNS: &[&str] = &[
    "unknown",
    "NotApplicable",
    "TwoHoleUSTop",
    "ThreeHoleUS",
    "TwoHoleDIN",
    "FourHoleDIN",
    "TwentyTwoHoleUS",
    "NineteenHoleUS",
    "TwoHoleMetric",
    "Swedish4Hole",
    "TwoHoleUSSide",
    "FiveHoleUS",
    "SevenHoleUS",
    "Mixed7H4S",
    "Norweg6Hole",
    "Metric26Hole",
    "Metric30Hole",
];
const CONDITION_NAMES: &[&str] = &[
    "CoverOpen",
    "InterlockOpen",
    "Jam",
    "DoorOpen",
    "InputTrayMissing",
    "InputTrayMediaSizeChange",
    "InputTrayMediaTypeChange",
    "InputTraySupplyLow",
    "InputTraySupplyEmpty",
    "OutputTrayMissing",
    "OutputTrayAlmostFull",
    "OutputTrayFull",
    "FuserUnderTemperature",
    "FuserOverTemperature",
    "ConsumableLow",
    "ConsumableEmpty",
    "WasteReceptacleAlmostFull",
    "WasteReceptacleFull",
    "Unknown",
];
const SEVERITIES: &[&str] = &["Informational", "Warning", "Critical"];
const COMPONENT_GROUPS: &[&str] = &[
    "PrintService",
    "InputBin",
    "MediaPath",
    "OutputBin",
    "Consumable",
    "CoverDoor",
];
const CONSUMABLE_TYPES: &[&str] = &[
    "Toner",
    "WasteToner",
    "Ink",
    "WasteInk",
    "Opc",
    "Developer",
    "FuserOil",
    "Wax",
    "WasteWax",
    "Staples",
    "unknown",
];
const CONSUMABLE_COLORS: &[&str] = &[
    "Black",
    "Blue",
    "Color",
    "Cyan",
    "Gray",
    "Green",
    "Magenta",
    "PhotoBlack",
    "PhotoColor",
    "PhotoCyan",
    "PhotoMagenta",
    "PhotoYellow",
    "Red",
    "White",
    "Yellow",
];
const STORAGE_TYPES: &[&str] = &[
    "RAM",
    "PSMemory",
    "NonRemovableFlash",
    "HardDisk",
    "SmartMedia",
    "CompactFlash",
    "MemoryStick",
    "SecureDigital",
    "unknown",
];
const INPUT_BINS: &[&str] = &[
    "Bottom",
    "Bypass",
    "Envelope",
    "EnvelopeManual",
    "LargeCapacity",
    "Manual",
    "Middle",
    "Multipurpose",
    "Top",
];
const OUTPUT_BINS: &[&str] = &[
    "Bottom",
    "Center",
    "Face-down",
    "Face-up",
    "Finisher",
    "Large-capacity",
    "Left",
    "Middle",
    "My-mailbox",
    "Rear",
    "Right",
    "Side",
    "Top",
];
const FEED_DIRECTIONS: &[&str] = &["LongEdgeFirst", "ShortEdgeFirst"];
const SET_RESULTS: &[&str] = &[
    "Succeeded",
    "ElementReadOnly",
    "IncompatibleType",
    "InvalidElementPath",
    "UnknownElementPath",
    "UnknownFailure",
];

const fn en(xsd: &'static str, vals: &'static [&'static str], ext: Ext) -> Simple {
    Simple::Enum { xsd, vals, ext }
}

/// Simple types by table name.
static SIMPLES: &[(&str, Simple)] = &[
    ("boolean", Simple::Bool),
    ("QName", Simple::QName),
    ("string", Simple::Str(usize::MAX)),
    ("base64Binary", Simple::Base64),
    ("double", Simple::Double),
    ("anyURI", Simple::Uri),
    ("Tokens", Simple::Tokens),
    ("IntExtType", Simple::Int(I32MIN, I32MAX)),
    ("IntOneExtType", Simple::Int(1, I32MAX)),
    ("IntZeroExtType", Simple::Int(0, I32MAX)),
    ("DateTimeExtType", Simple::DateTime),
    ("String255BaseType", Simple::Str(255)),
    ("LocalizedStringType", Simple::Str(127)),
    ("DeviceIdType", Simple::Str(1023)),
    ("JobIdType", Simple::Int(0, I32MAX)),
    ("JobNameType", Simple::Str(255)),
    ("JobCopiesRestrictionType", Simple::Int(1, I32MAX)),
    ("JobPriorityRestrictionType", Simple::Int(1, 100)),
    ("NUpPagesPerSheetRestrictionType", Simple::Int(1, I32MAX)),
    ("EventRateBaseType", Simple::Int(1, 600)),
    ("LevelFreeRestrictType", Simple::Int(-1, 100)),
    ("CapacityRestrictType", Simple::Int(-1, I32MAX)),
    ("ConditionIdType", Simple::Int(1, I32MAX)),
    ("DocumentIdType", Simple::Int(1, I32MAX)),
    ("ComponentNameBaseType", Simple::Token(255)),
    ("ColorRepType", Simple::Len(6)),
    ("DeviceModelIdGuidType", Simple::Len(36)),
    (
        "PrinterStateBaseType",
        en("PrinterStateWKVType", PRINTER_STATES, Ext::Keyword),
    ),
    (
        "PrinterStateReasonBaseType",
        en("PrinterStateReasonsWKVType", PRINTER_REASONS, Ext::Keyword),
    ),
    (
        "JobStateExtType",
        en("JobStateWKVType", JOB_STATES, Ext::Keyword),
    ),
    (
        "JobStateReasonExtType",
        en("JobStateReasonsWKVType", JOB_REASONS, Ext::Keyword),
    ),
    (
        "CompressionExtType",
        en("CompressionWKVType", COMPRESSIONS, Ext::Keyword),
    ),
    (
        "DocumentFormatExtType",
        en("DocumentFormatWKVType", FORMATS, Ext::Mime),
    ),
    (
        "MediaSizeNameExtType",
        en(
            "MediaSizeSelfDescribingNameWKVType",
            MEDIA_SIZES,
            Ext::MediaSize,
        ),
    ),
    (
        "MediaTypeExtType",
        en("MediaTypeWKVType", MEDIA_TYPES, Ext::MediaType),
    ),
    (
        "MediaColorExtType",
        en("MediaColorWKVType", MEDIA_COLORS, Ext::MediaColor),
    ),
    (
        "OrientationBaseType",
        en("OrientationWKVType", ORIENTATIONS, Ext::Keyword),
    ),
    (
        "PrintQualityBaseType",
        en("PrintQualityWKVType", QUALITIES, Ext::Keyword),
    ),
    ("SidesBaseType", en("SidesWKVType", SIDES, Ext::Keyword)),
    (
        "NUpDirectionRestrictionType",
        en("NUpDirectionRestrictionType", NUP_DIRECTIONS, Ext::Closed),
    ),
    (
        "StapleLocationExtType",
        en("StapleLocationWKVType", STAPLE_LOCATIONS, Ext::Keyword),
    ),
    (
        "StapleAngleExtType",
        en("StapleAngleWKVType", STAPLE_ANGLES, Ext::Keyword),
    ),
    (
        "HolePunchEdgeExtType",
        en("HolePunchEdgeWKVType", PUNCH_EDGES, Ext::Keyword),
    ),
    (
        "HolePunchPatternExtType",
        en("HolePunchPatternWKVType", PUNCH_PATTERNS, Ext::Keyword),
    ),
    (
        "ConditionNameType",
        en("ConditionNameBaseType", CONDITION_NAMES, Ext::Closed),
    ),
    (
        "ConditionSeverityType",
        en("ConditionSeverityBaseType", SEVERITIES, Ext::Closed),
    ),
    (
        "ComponentGroupType",
        en("ComponentGroupBaseType", COMPONENT_GROUPS, Ext::Closed),
    ),
    (
        "ConsumableTypeBaseType",
        en("ConsumableTypeWKVType", CONSUMABLE_TYPES, Ext::Keyword),
    ),
    (
        "ConsumableColorType",
        en("ConsumableColorWKVType", CONSUMABLE_COLORS, Ext::Keyword),
    ),
    (
        "StorageTypeBaseType",
        en("StorageTypeWKVType", STORAGE_TYPES, Ext::Keyword),
    ),
    (
        "InputBinNameType",
        en("InputBinWKVType", INPUT_BINS, Ext::InputTray),
    ),
    (
        "OutputBinNameType",
        en("OutputBinWKVType", OUTPUT_BINS, Ext::OutputBin),
    ),
    (
        "InputBinFeedDirectionBaseType",
        en(
            "InputBinFeedDirectionBaseType",
            FEED_DIRECTIONS,
            Ext::Closed,
        ),
    ),
    (
        "InputBinMediaSizeBaseType",
        en(
            "MediaSizeSelfDescribingNameWKVType",
            MEDIA_SIZES,
            Ext::Closed,
        ),
    ),
    (
        "InputBinMediaTypeExtType",
        en("MediaTypeWKVType", MEDIA_TYPES, Ext::Closed),
    ),
    (
        "InputBinMediaColorBaseType",
        en("MediaColorWKVType", MEDIA_COLORS, Ext::Closed),
    ),
    (
        "ElementSetResultValuesType",
        en("ElementSetResultValuesType", SET_RESULTS, Ext::Closed),
    ),
];

/// The simple type of this table name.
pub fn simple_type(name: &str) -> Option<&'static Simple> {
    SIMPLES.iter().find(|(n, _)| *n == name).map(|(_, t)| t)
}

/// Check `text` against the simple type `name` (a table name such as `MediaSizeNameExtType`).
/// For use by the profile loader so profiles and the wire share one definition of a legal value.
pub fn check_value(name: &str, text: &str) -> Result<(), String> {
    match simple_type(name) {
        Some(t) => check_simple(t, text),
        None => Err(format!("unknown value type {name}")),
    }
}

/// The well-known values of an enumerated simple type, for lists and defaults.
pub fn well_known(name: &str) -> &'static [&'static str] {
    match simple_type(name) {
        Some(Simple::Enum { vals, .. }) => vals,
        _ => &[],
    }
}

// ---------------------------------------------------------------------------------------------
// Complex types
// ---------------------------------------------------------------------------------------------

static TYPES: &[Ct] = &[
    // ---- Printer ---------------------------------------------------------------------------
    seq(
        "PrinterType",
        &[
            r("PrinterDescription", "PrinterDescriptionType"),
            r("PrinterConfiguration", "PrinterConfigurationType"),
            r("PrinterStatus", "PrinterStatusType"),
            r("DefaultPrintTicket", "PrintTicketType"),
            r("PrinterCapabilities", "PrinterCapabilitiesType"),
            ANY,
        ],
    ),
    seq(
        "PrinterDescriptionType",
        &[
            r("ColorSupported", "boolean"),
            r("DeviceId", "DeviceIdType"),
            o("MultipleDocumentJobsSupported", "boolean"),
            r("PagesPerMinute", "IntOneExtType"),
            o("PagesPerMinuteColor", "IntOneExtType"),
            s("PrinterName", "LocalizedStringType"),
            m("PrinterInfo", "LocalizedStringType"),
            m("PrinterLocation", "LocalizedStringType"),
            ANY,
        ],
    ),
    seq(
        "PrinterConfigurationType",
        &[
            o("PrinterEventRate", "EventRateBaseType"),
            o("Storage", "StorageBaseType"),
            o("Consumables", "ConsumablesType"),
            o("InputBins", "InputBinsType"),
            o("Finishings", "FinishingsType"),
            o("OutputBins", "OutputBinsType"),
            ANY,
        ],
    ),
    seq("StorageBaseType", &[s("StorageEntry", "StorageEntryType")]),
    seq_a(
        "StorageEntryType",
        &[
            r("Type", "StorageTypeBaseType"),
            r("Size", "IntOneExtType"),
            r("Free", "LevelFreeRestrictType"),
            ANY,
        ],
        NAME255_REMOVED,
    ),
    seq(
        "ConsumablesType",
        &[s("ConsumableEntry", "ConsumableEntryType")],
    ),
    seq_a(
        "ConsumableEntryType",
        &[
            r("Type", "ConsumableTypeBaseType"),
            o("Color", "ConsumableColorType"),
            r("Level", "LevelFreeRestrictType"),
            o("Model", "String255BaseType"),
            ANY,
        ],
        NAME255_REMOVED,
    ),
    seq("InputBinsType", &[s("InputBinEntry", "InputBinEntryType")]),
    seq_a(
        "InputBinEntryType",
        &[
            r("FeedDirection", "InputBinFeedDirectionBaseType"),
            r("MediaSize", "InputBinMediaSizeBaseType"),
            o("MediaType", "InputBinMediaTypeExtType"),
            o("MediaColor", "InputBinMediaColorBaseType"),
            r("Capacity", "CapacityRestrictType"),
            r("Level", "LevelFreeRestrictType"),
            ANY,
        ],
        INBIN_ATTRS,
    ),
    seq(
        "OutputBinsType",
        &[s("OutputBinEntry", "OutputBinEntryType")],
    ),
    seq_a(
        "OutputBinEntryType",
        &[
            r("Capacity", "CapacityRestrictType"),
            r("Level", "LevelFreeRestrictType"),
            ANY,
        ],
        OUTBIN_ATTRS,
    ),
    seq(
        "FinishingsType",
        &[
            r("CollationSupported", "boolean"),
            r("JogOffsetSupported", "boolean"),
            r("DuplexerInstalled", "boolean"),
            r("StaplerInstalled", "boolean"),
            r("HolePunchInstalled", "boolean"),
            ANY,
        ],
    ),
    seq(
        "PrinterStatusType",
        &[
            r("PrinterCurrentTime", "DateTimeExtType"),
            r("PrinterState", "PrinterStateBaseType"),
            o("PrinterPrimaryStateReason", "PrinterStateReasonBaseType"),
            o("PrinterStateReasons", "PrinterStateReasonsType"),
            r("QueuedJobCount", "IntZeroExtType"),
            o("ActiveCondition", "ActiveConditionTableType"),
            o("ConditionHistory", "ConditionHistoryTableType"),
            ANY,
        ],
    ),
    seq(
        "PrinterStateReasonsType",
        &[s("PrinterStateReason", "PrinterStateReasonBaseType")],
    ),
    seq(
        "ActiveConditionTableType",
        &[s("DeviceCondition", "DeviceConditionType")],
    ),
    seq(
        "ConditionHistoryTableType",
        &[s("ConditionHistoryEntry", "ConditionHistoryEntryType")],
    ),
    seq_a(
        "DeviceConditionType",
        &[
            r("Time", "DateTimeExtType"),
            r("Name", "ConditionNameType"),
            r("Component", "ConditionComponentType"),
            r("Severity", "ConditionSeverityType"),
            ANY,
        ],
        COND_ATTRS,
    ),
    // extends DeviceConditionType: the base sequence (minus its extension slot), then ClearTime
    seq_a(
        "ConditionHistoryEntryType",
        &[
            r("Time", "DateTimeExtType"),
            r("Name", "ConditionNameType"),
            r("Component", "ConditionComponentType"),
            r("Severity", "ConditionSeverityType"),
            r("ClearTime", "DateTimeExtType"),
        ],
        COND_ATTRS,
    ),
    seq(
        "ConditionComponentType",
        &[
            r("Group", "ComponentGroupType"),
            r("Name", "ComponentNameBaseType"),
        ],
    ),
    seq(
        "StatusSummaryType",
        &[
            r("PrinterState", "PrinterStateBaseType"),
            o("PrinterPrimaryStateReason", "PrinterStateReasonBaseType"),
            o("PrinterStateReasons", "PrinterStateReasonsType"),
            ANY,
        ],
    ),
    seq(
        "DeviceConditionClearedType",
        &[
            r("ConditionId", "ConditionIdType"),
            r("ConditionClearTime", "DateTimeExtType"),
            ANY,
        ],
    ),
    // ---- Capabilities -----------------------------------------------------------------------
    seq(
        "PrinterCapabilitiesType",
        &[
            o("JobValues", "JobValuesType"),
            o("DocumentValues", "DocumentValuesType"),
            ANY1,
        ],
    ),
    seq(
        "JobValuesType",
        &[
            o("JobProcessing", "JobValuesJobProcessing"),
            o("DocumentProcessing", "JobValuesDocumentProcessing"),
            ANY,
        ],
    ),
    seq(
        "JobValuesJobProcessing",
        &[
            o("Copies", "ValueIntRangeType"),
            o("JobFinishings", "JobValuesFinishings"),
            o("Priority", "ValueIntRangeType"),
            ANY1,
        ],
    ),
    seq(
        "JobValuesFinishings",
        &[
            o("Staple", "CapStaple"),
            o("HolePunch", "CapHolePunch"),
            ANY1,
        ],
    ),
    seq(
        "CapStaple",
        &[
            r("Location", "ValueTokenListType"),
            r("Angle", "ValueTokenListType"),
            ANY1,
        ],
    ),
    seq(
        "CapHolePunch",
        &[
            r("Edge", "ValueTokenListType"),
            r("Pattern", "ValueTokenListType"),
            ANY1,
        ],
    ),
    seq(
        "JobValuesDocumentProcessing",
        &[
            o("MediaSizeName", "ValueTokenListType"),
            o("MediaType", "ValueTokenListType"),
            o("MediaColor", "ValueTokenListType"),
            o("NumberUp", "CapNumberUp"),
            o("Orientation", "ValueTokenListType"),
            o("Resolution", "CapResolution"),
            o("PrintQuality", "ValueTokenListType"),
            o("Sides", "ValueTokenListType"),
            ANY1,
        ],
    ),
    seq(
        "CapNumberUp",
        &[
            r("PagesPerSheet", "ValueIntListType"),
            r("Direction", "ValueTokenListType"),
            ANY1,
        ],
    ),
    seq("CapResolution", &[s("AllowedValue", "ResolutionEntryType")]),
    seq(
        "DocumentValuesType",
        &[o("DocumentDescription", "CapDocumentDescription"), ANY],
    ),
    seq(
        "CapDocumentDescription",
        &[
            o("Compression", "ValueTokenListType"),
            o("Format", "ValueStringListType"),
            ANY1,
        ],
    ),
    seq(
        "ValueIntRangeType",
        &[o("MinValue", "IntExtType"), r("MaxValue", "IntExtType")],
    ),
    seq("ValueIntListType", &[s("AllowedValue", "IntExtType")]),
    seq("ValueStringListType", &[s("AllowedValue", "anyURI")]),
    seq("ValueTokenListType", &[s("AllowedValue", "Tokens")]),
    // ---- Tickets ----------------------------------------------------------------------------
    seq(
        "PrintTicketType",
        &[
            o("JobDescription", "JobDescriptionType"),
            o("JobProcessing", "JobProcessingType"),
            o("DocumentProcessing", "DocumentProcessingType"),
            ANY,
        ],
    ),
    seq(
        "JobDescriptionType",
        &[
            r("JobName", "JobNameType"),
            r("JobOriginatingUserName", "JobNameType"),
            ANY,
        ],
    ),
    seq(
        "JobProcessingType",
        &[
            o("Copies", "JobCopiesType"),
            o("JobFinishings", "JobFinishingsType"),
            o("Priority", "JobPriorityType"),
            ANY,
        ],
    ),
    txt("JobCopiesType", "JobCopiesRestrictionType", MUST),
    txt("JobPriorityType", "JobPriorityRestrictionType", MUST),
    seq(
        "JobFinishingsType",
        &[
            o("Collate", "BooleanExtType"),
            o("JogOffset", "BooleanExtType"),
            o("Staple", "StapleDetailsType"),
            o("HolePunch", "HolePunchDetailsType"),
            ANY,
        ],
    ),
    txt("BooleanExtType", "boolean", MUST),
    seq(
        "StapleDetailsType",
        &[
            r("Location", "StapleLocationType"),
            o("Angle", "StapleAngleType"),
            ANY,
        ],
    ),
    txt("StapleLocationType", "StapleLocationExtType", MUST),
    txt("StapleAngleType", "StapleAngleExtType", MUST),
    seq_a(
        "HolePunchDetailsType",
        &[
            r("Edge", "HolePunchEdgeExtType"),
            r("Pattern", "HolePunchPatternExtType"),
            ANY,
        ],
        MUST,
    ),
    seq(
        "DocumentProcessingType",
        &[
            o("MediaSizeName", "MediaSizeNameType"),
            o("MediaType", "MediaTypeBaseType"),
            o("MediaColor", "MediaColorType"),
            o("NumberUp", "NumberUpType"),
            o("Orientation", "OrientationType"),
            o("Resolution", "ResolutionType"),
            o("PrintQuality", "PrintQualityType"),
            o("Sides", "SidesType"),
            ANY,
        ],
    ),
    txt("MediaSizeNameType", "MediaSizeNameExtType", MUST),
    txt("MediaTypeBaseType", "MediaTypeExtType", MUST),
    txt("MediaColorType", "MediaColorExtType", MUST),
    txt("OrientationType", "OrientationBaseType", MUST),
    txt("PrintQualityType", "PrintQualityBaseType", MUST),
    txt("SidesType", "SidesBaseType", MUST),
    seq(
        "NumberUpType",
        &[
            r("PagesPerSheet", "NUpPagesPerSheetType"),
            r("Direction", "NUpDirectionType"),
            ANY,
        ],
    ),
    txt(
        "NUpPagesPerSheetType",
        "NUpPagesPerSheetRestrictionType",
        MUST,
    ),
    txt("NUpDirectionType", "NUpDirectionRestrictionType", MUST),
    seq_a(
        "ResolutionType",
        &[r("Width", "IntExtType"), o("Height", "IntExtType")],
        MUST,
    ),
    seq(
        "ResolutionEntryType",
        &[r("Width", "IntExtType"), o("Height", "IntExtType")],
    ),
    // ---- Documents and jobs -----------------------------------------------------------------
    txt("CompressionType", "CompressionExtType", MUST),
    txt("DocumentFormatType", "DocumentFormatExtType", MUST),
    seq(
        "DocumentDescriptionType",
        &[
            r("DocumentId", "DocumentIdType"),
            r("Compression", "CompressionType"),
            r("Format", "DocumentFormatType"),
            o("DocumentName", "String255BaseType"),
            o("DocumentUrl", "anyURI"),
            ANY,
        ],
    ),
    seq(
        "DocumentType",
        &[
            r("DocumentDescription", "DocumentDescriptionType"),
            o("DocumentProcessing", "DocumentProcessingType"),
            ANY,
        ],
    ),
    seq("DocumentsType", &[s("Document", "DocumentType"), ANY]),
    seq(
        "JobStatusType",
        &[
            r("JobId", "JobIdType"),
            r("JobState", "JobStateExtType"),
            o("JobStateReasons", "JobStateReasonsType"),
            r("KOctetsProcessed", "IntZeroExtType"),
            r("MediaSheetsCompleted", "IntZeroExtType"),
            r("NumberOfDocuments", "IntZeroExtType"),
            ANY,
        ],
    ),
    seq(
        "JobSummaryType",
        &[
            r("JobId", "JobIdType"),
            r("JobState", "JobStateExtType"),
            o("JobStateReasons", "JobStateReasonsType"),
            r("JobName", "JobNameType"),
            r("JobOriginatingUserName", "JobNameType"),
            r("KOctetsProcessed", "IntZeroExtType"),
            r("MediaSheetsCompleted", "IntZeroExtType"),
            r("NumberOfDocuments", "IntZeroExtType"),
            ANY,
        ],
    ),
    seq(
        "JobEndStateType",
        &[
            r("JobId", "JobIdType"),
            r("JobCompletedState", "JobStateExtType"),
            r("JobCompletedStateReasons", "JobStateReasonsType"),
            r("JobName", "JobNameType"),
            r("JobOriginatingUserName", "JobNameType"),
            r("KOctetsProcessed", "IntZeroExtType"),
            r("MediaSheetsCompleted", "IntZeroExtType"),
            r("NumberOfDocuments", "IntZeroExtType"),
            ANY,
        ],
    ),
    seq(
        "JobStateReasonsType",
        &[s("JobStateReason", "JobStateReasonExtType")],
    ),
    seq(
        "JobType",
        &[
            r("JobStatus", "JobStatusType"),
            r("PrintTicket", "PrintTicketType"),
            o("Documents", "DocumentsType"),
            ANY,
        ],
    ),
    seq("ListOfJobsType", &[m("Job", "JobType"), ANY]),
    seq("ListOfSummarysType", &[m("JobSummary", "JobSummaryType")]),
    seq(
        "JobTableType",
        &[
            r("ActiveJobs", "ListOfJobsType"),
            o("JobHistory", "ListOfJobsType"),
            ANY,
        ],
    ),
    // ---- Element exchange -------------------------------------------------------------------
    seq("RequestedPrinterElementsType", &[s("Name", "QName")]),
    seq("RequestedJobElementsType", &[s("Name", "QName")]),
    seq(
        "PrinterElementsType",
        &[s("ElementData", "PrinterElementDataType")],
    ),
    choice(
        "PrinterElementDataType",
        Ns::Print,
        &[
            r("PrinterDescription", "PrinterDescriptionType"),
            r("PrinterConfiguration", "PrinterConfigurationType"),
            r("PrinterStatus", "PrinterStatusType"),
            r("DefaultPrintTicket", "PrintTicketType"),
            r("PrinterCapabilities", "PrinterCapabilitiesType"),
            ANY,
        ],
        ELDATA_ATTRS,
    ),
    seq("JobElementsType", &[s("ElementData", "JobElementDataType")]),
    choice(
        "JobElementDataType",
        Ns::Print,
        &[
            r("JobStatus", "JobStatusType"),
            r("PrintTicket", "PrintTicketType"),
            r("Documents", "DocumentsType"),
            ANY,
        ],
        ELDATA_ATTRS,
    ),
    seq(
        "ElementChangesType",
        &[
            o("PrinterDescription", "PrinterDescriptionType"),
            o("PrinterConfiguration", "PrinterConfigurationType"),
            o("DefaultPrintTicket", "PrintTicketType"),
            o("PrinterCapabilities", "PrinterCapabilitiesType"),
            ANY,
        ],
    ),
    // ---- v1.0 operation messages ------------------------------------------------------------
    seq(
        "CreatePrintJobRequestType",
        &[r("PrintTicket", "PrintTicketType")],
    ),
    seq("CreatePrintJobResponseType", &[r("JobId", "JobIdType")]),
    seq(
        "SendDocumentRequestType",
        &[
            r("JobId", "JobIdType"),
            r("DocumentDescription", "DocumentDescriptionType"),
            o("DocumentProcessing", "DocumentProcessingType"),
            r("LastDocument", "boolean"),
            r("DocumentData", "base64Binary"),
        ],
    ),
    seq("SendDocumentResponseType", &[ANY]),
    seq(
        "AddDocumentRequestType",
        &[
            r("JobId", "JobIdType"),
            r("DocumentDescription", "DocumentDescriptionType"),
            o("DocumentProcessing", "DocumentProcessingType"),
            r("LastDocument", "boolean"),
            r("DocumentUrl", "anyURI"),
        ],
    ),
    seq("AddDocumentResponseType", &[ANY]),
    seq("CancelJobRequestType", &[r("JobId", "JobIdType")]),
    seq("CancelJobResponseType", &[ANY]),
    seq(
        "GetPrinterElementsRequestType",
        &[r("RequestedElements", "RequestedPrinterElementsType")],
    ),
    seq(
        "GetPrinterElementsResponseType",
        &[r("PrinterElements", "PrinterElementsType")],
    ),
    seq(
        "GetJobElementsRequestType",
        &[
            r("JobId", "JobIdType"),
            r("RequestedElements", "RequestedJobElementsType"),
        ],
    ),
    seq(
        "GetJobElementsResponseType",
        &[r("JobElements", "JobElementsType")],
    ),
    seq("GetActiveJobsRequestType", &[ANY]),
    seq(
        "GetActiveJobsResponseType",
        &[r("ActiveJobs", "ListOfSummarysType")],
    ),
    seq("GetJobHistoryRequestType", &[ANY]),
    seq(
        "GetJobHistoryResponseType",
        &[r("JobHistory", "ListOfSummarysType")],
    ),
    seq(
        "SetEventRateRequestType",
        &[r("EventRate", "EventRateBaseType")],
    ),
    seq("SetEventRateResponseType", &[ANY]),
    // ---- events -----------------------------------------------------------------------------
    seq(
        "PrinterElementsChangeEventType",
        &[r("ElementChanges", "ElementChangesType"), ANY],
    ),
    seq(
        "PrinterStatusSummaryEventType",
        &[r("StatusSummary", "StatusSummaryType"), ANY],
    ),
    seq(
        "PrinterStatusConditionEventType",
        &[r("DeviceCondition", "DeviceConditionType"), ANY],
    ),
    seq(
        "PrinterStatusConditionClearedEventType",
        &[
            r("DeviceConditionCleared", "DeviceConditionClearedType"),
            ANY,
        ],
    ),
    seq(
        "JobStatusEventType",
        &[r("JobStatus", "JobStatusType"), ANY],
    ),
    seq(
        "JobEndStateEventType",
        &[r("JobEndState", "JobEndStateType"), ANY],
    ),
    // ---- v1.1 -------------------------------------------------------------------------------
    seq_n(
        Ns::Print11,
        "DriverConfigurationType",
        &[
            o("GPDConfigFile", "string"),
            o("PPDConfigFile", "string"),
            ANY,
        ],
    ),
    // ---- v1.2 -------------------------------------------------------------------------------
    seq_n(
        Ns::Print12,
        "SetPrinterElementsRequestType",
        &[r("ElementsToSet", "PrinterElementsToSetType")],
    ),
    seq_n(
        Ns::Print12,
        "SetPrinterElementsResponseType",
        &[r("ElementsToSetResults", "PrinterElementsToSetResultsType")],
    ),
    seq_n(
        Ns::Print12,
        "PrinterElementsToSetType",
        &[s("ElementToSet", "ElementToSetType")],
    ),
    seq_n(
        Ns::Print12,
        "ElementToSetType",
        &[
            r("ElementPath", "string"),
            Item::OneOf(&[
                ("Boolean", "boolean"),
                ("Integer", "IntExtType"),
                ("String", "string"),
                ("Float", "double"),
                ("Binary", "base64Binary"),
            ]),
            ANY,
        ],
    ),
    seq_n(
        Ns::Print12,
        "PrinterElementsToSetResultsType",
        &[s("ElementToSetResult", "ElementToSetResultType")],
    ),
    seq_n(
        Ns::Print12,
        "ElementToSetResultType",
        &[
            r("ElementPath", "string"),
            r("Result", "ElementSetResultValuesType"),
        ],
    ),
    // ---- v2.0 -------------------------------------------------------------------------------
    seq_n(
        Ns::Print20,
        "CreatePrintJob2RequestType",
        &[
            r("JobDescription2", "JobDescription2Type"),
            o("PrintJobTicketContainer", "PrintJobTicketContainerType"),
            ANY,
        ],
    ),
    seq_n(
        Ns::Print20,
        "JobDescription2Type",
        &[
            r("JobName", "JobNameType"),
            r("JobOriginatingUserName", "JobNameType"),
            o(
                "JobOriginatingUserIdentityEntries",
                "UserIdentityEntriesType",
            ),
            ANY,
        ],
    ),
    seq_n(
        Ns::Print20,
        "UserIdentityEntriesType",
        &[s("UserIdentityEntry", "UserIdentityEntryType"), ANY],
    ),
    seq_n(
        Ns::Print20,
        "UserIdentityEntryType",
        &[
            r("UserIdentityProvider", "string"),
            r("UserIdentity", "string"),
            ANY,
        ],
    ),
    seq_n(
        Ns::Print20,
        "PrintJobTicketContainerType",
        &[
            r("PrintJobTicketFormat", "string"),
            r("PrintJobTicketContent", "base64Binary"),
        ],
    ),
    seq_n(Ns::Print20, "GetBidiSchemaExtensionsRequestType", &[ANY]),
    seq_n(
        Ns::Print20,
        "GetBidiSchemaExtensionsResponseType",
        &[r("BidiSchemaExtensionsData", "base64Binary"), ANY],
    ),
    seq_n(
        Ns::Print20,
        "GetPrintDeviceCapabilitiesRequestType",
        &[
            r("PrintDeviceCapabilitiesFormat", "string"),
            r("PrintDeviceCapabilitiesChangeID", "string"),
            ANY,
        ],
    ),
    seq_n(
        Ns::Print20,
        "GetPrintDeviceCapabilitiesResponseType",
        &[r("PrintDeviceCapabilities", "PrintDeviceCapabilitiesType")],
    ),
    seq_n(
        Ns::Print20,
        "PrintDeviceCapabilitiesType",
        &[r("DeviceCapabilitiesData", "base64Binary")],
    ),
    seq_n(
        Ns::Print20,
        "GetPrintDeviceResourcesRequestType",
        &[
            r("ResourceLanguage", "string"),
            r("ResourceFormat", "string"),
        ],
    ),
    seq_n(
        Ns::Print20,
        "GetPrintDeviceResourcesResponseType",
        &[r("PrintDeviceResources", "PrintDeviceResourcesType")],
    ),
    seq_n(
        Ns::Print20,
        "PrintDeviceResourcesType",
        &[
            r("ResourceLanguage", "string"),
            r("Resources", "PrintDeviceResourcesDataType"),
        ],
    ),
    choice(
        "PrintDeviceResourcesDataType",
        Ns::Print20,
        &[
            r("ResourceURL", "string"),
            r("ResourceData", "base64Binary"),
        ],
        NOA,
    ),
    seq_n(Ns::Print20, "PrepareToPrintRequestType", &[ANY]),
    seq_n(
        Ns::Print20,
        "PrinterFormatsType",
        &[
            r("PrintDeviceCapabilitiesFormats", "ValueTokenListType"),
            r("PrintJobTicketFormats", "ValueTokenListType"),
            r("PrintDeviceResourceFormats", "ValueTokenListType"),
            ANY,
        ],
    ),
];

/// A global element: namespace, local name, type name.
pub type Global = (Ns, &'static str, &'static str);

static GLOBALS: &[Global] = &[
    (Ns::Print, "Printer", "PrinterType"),
    (
        Ns::Print,
        "CreatePrintJobRequest",
        "CreatePrintJobRequestType",
    ),
    (
        Ns::Print,
        "CreatePrintJobResponse",
        "CreatePrintJobResponseType",
    ),
    (Ns::Print, "SendDocumentRequest", "SendDocumentRequestType"),
    (
        Ns::Print,
        "SendDocumentResponse",
        "SendDocumentResponseType",
    ),
    (Ns::Print, "AddDocumentRequest", "AddDocumentRequestType"),
    (Ns::Print, "AddDocumentResponse", "AddDocumentResponseType"),
    (Ns::Print, "CancelJobRequest", "CancelJobRequestType"),
    (Ns::Print, "CancelJobResponse", "CancelJobResponseType"),
    (
        Ns::Print,
        "GetPrinterElementsRequest",
        "GetPrinterElementsRequestType",
    ),
    (
        Ns::Print,
        "GetPrinterElementsResponse",
        "GetPrinterElementsResponseType",
    ),
    (
        Ns::Print,
        "GetJobElementsRequest",
        "GetJobElementsRequestType",
    ),
    (
        Ns::Print,
        "GetJobElementsResponse",
        "GetJobElementsResponseType",
    ),
    (
        Ns::Print,
        "GetActiveJobsRequest",
        "GetActiveJobsRequestType",
    ),
    (
        Ns::Print,
        "GetActiveJobsResponse",
        "GetActiveJobsResponseType",
    ),
    (
        Ns::Print,
        "GetJobHistoryRequest",
        "GetJobHistoryRequestType",
    ),
    (
        Ns::Print,
        "GetJobHistoryResponse",
        "GetJobHistoryResponseType",
    ),
    (Ns::Print, "SetEventRateRequest", "SetEventRateRequestType"),
    (
        Ns::Print,
        "SetEventRateResponse",
        "SetEventRateResponseType",
    ),
    (
        Ns::Print,
        "PrinterElementsChangeEvent",
        "PrinterElementsChangeEventType",
    ),
    (
        Ns::Print,
        "PrinterStatusSummaryEvent",
        "PrinterStatusSummaryEventType",
    ),
    (
        Ns::Print,
        "PrinterStatusConditionEvent",
        "PrinterStatusConditionEventType",
    ),
    (
        Ns::Print,
        "PrinterStatusConditionClearedEvent",
        "PrinterStatusConditionClearedEventType",
    ),
    (Ns::Print, "JobStatusEvent", "JobStatusEventType"),
    (Ns::Print, "JobEndStateEvent", "JobEndStateEventType"),
    (Ns::Print11, "SupportsWSPrintv11", "boolean"),
    (
        Ns::Print11,
        "DriverConfiguration",
        "DriverConfigurationType",
    ),
    (Ns::Print11, "DeviceModelId", "DeviceModelIdGuidType"),
    (Ns::Print11, "ColorRepresentation", "ColorRepType"),
    (Ns::Print12, "SupportsWSPrintV12", "boolean"),
    (
        Ns::Print12,
        "SetPrinterElementsRequest",
        "SetPrinterElementsRequestType",
    ),
    (
        Ns::Print12,
        "SetPrinterElementsResponse",
        "SetPrinterElementsResponseType",
    ),
    (Ns::Print20, "SupportsWSPrintV20", "boolean"),
    (
        Ns::Print20,
        "CreatePrintJob2Request",
        "CreatePrintJob2RequestType",
    ),
    (
        Ns::Print20,
        "CreatePrintJob2Response",
        "CreatePrintJobResponseType",
    ),
    (
        Ns::Print20,
        "GetBidiSchemaExtensionsRequest",
        "GetBidiSchemaExtensionsRequestType",
    ),
    (
        Ns::Print20,
        "GetBidiSchemaExtensionsResponse",
        "GetBidiSchemaExtensionsResponseType",
    ),
    (
        Ns::Print20,
        "GetPrintDeviceCapabilitiesRequest",
        "GetPrintDeviceCapabilitiesRequestType",
    ),
    (
        Ns::Print20,
        "GetPrintDeviceCapabilitiesResponse",
        "GetPrintDeviceCapabilitiesResponseType",
    ),
    (
        Ns::Print20,
        "GetPrintDeviceResourcesRequest",
        "GetPrintDeviceResourcesRequestType",
    ),
    (
        Ns::Print20,
        "GetPrintDeviceResourcesResponse",
        "GetPrintDeviceResourcesResponseType",
    ),
    (
        Ns::Print20,
        "PrepareToPrintRequest",
        "PrepareToPrintRequestType",
    ),
    (Ns::Print20, "PrintDeviceCapabilitiesChangeID", "string"),
    (Ns::Print20, "PrinterFormats", "PrinterFormatsType"),
];

/// Local names of the sections of an `ElementData` and the types they have.
static SECTIONS: &[(&str, &str)] = &[
    ("PrinterDescription", "PrinterDescriptionType"),
    ("PrinterConfiguration", "PrinterConfigurationType"),
    ("PrinterStatus", "PrinterStatusType"),
    ("DefaultPrintTicket", "PrintTicketType"),
    ("PrinterCapabilities", "PrinterCapabilitiesType"),
    ("JobStatus", "JobStatusType"),
    ("JobSummary", "JobSummaryType"),
    ("PrintTicket", "PrintTicketType"),
    ("Documents", "DocumentsType"),
    ("JobEndState", "JobEndStateType"),
    ("DeviceCondition", "DeviceConditionType"),
    ("StatusSummary", "StatusSummaryType"),
    ("InputBins", "InputBinsType"),
    ("OutputBins", "OutputBinsType"),
    ("Consumables", "ConsumablesType"),
];

/// Which domain the `AllowedValue` entries under a capability list element must draw from.
static LIST_DOMAINS: &[(&str, &str)] = &[
    ("MediaSizeName", "MediaSizeNameExtType"),
    ("MediaType", "MediaTypeExtType"),
    ("MediaColor", "MediaColorExtType"),
    ("Orientation", "OrientationBaseType"),
    ("PrintQuality", "PrintQualityBaseType"),
    ("Sides", "SidesBaseType"),
    ("Direction", "NUpDirectionRestrictionType"),
    ("Location", "StapleLocationExtType"),
    ("Angle", "StapleAngleExtType"),
    ("Edge", "HolePunchEdgeExtType"),
    ("Pattern", "HolePunchPatternExtType"),
    ("Compression", "CompressionExtType"),
    ("Format", "DocumentFormatExtType"),
];

/// The complex type table, for the cross-check and tooling.
pub fn complex_types() -> &'static [Ct] {
    TYPES
}

/// The global element table, for the cross-check and tooling.
pub fn globals() -> &'static [Global] {
    GLOBALS
}

fn complex(name: &str) -> Option<&'static Ct> {
    TYPES.iter().find(|c| c.name == name)
}

fn global(ns: Ns, local: &str) -> Option<&'static Global> {
    GLOBALS.iter().find(|g| g.0 == ns && g.1 == local)
}

/// Root element `(namespace, name)` of an operation's request or response, when it has one.
pub fn root_for(op: PrintOp, response: bool) -> Option<(Ns, String)> {
    use PrintOp::*;
    let ns = match op {
        CreatePrintJob | SendDocument | AddDocument | CancelJob | GetPrinterElements
        | GetJobElements | GetActiveJobs | GetJobHistory | SetEventRate => Ns::Print,
        SetPrinterElements => Ns::Print12,
        CreatePrintJob2
        | GetPrintDeviceCapabilities
        | GetPrintDeviceResources
        | GetBidiSchemaExtensions
        | PrepareToPrint => Ns::Print20,
        _ => return None,
    };
    let name = format!(
        "{}{}",
        op.name(),
        if response { "Response" } else { "Request" }
    );
    global(ns, &name).map(|_| (ns, name))
}

// ---------------------------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------------------------

/// Validate a body with the strict options.
pub fn validate_body(target: Target<'_>, el: &Element) -> Vec<Violation> {
    validate_body_with(target, el, &Options::STRICT)
}

/// Validate a body.
///
/// Operations that carry no print body (WS-Eventing, metadata, discovery) and operations
/// without a response message yield no violations.
pub fn validate_body_with(target: Target<'_>, el: &Element, opts: &Options) -> Vec<Violation> {
    let mut cx = Cx::new(opts);
    let local = el.name.local.clone();
    match target {
        Target::Request(op) | Target::Response(op) => {
            let response = matches!(target, Target::Response(_));
            let Some((ns, name)) = root_for(op, response) else {
                return Vec::new();
            };
            if el.name.ns != ns.uri() || el.name.local != name {
                cx.push(
                    &local,
                    ViolationKind::WrongRoot,
                    format!("expected {{{}}}{name}, found {}", ns.uri(), el.name),
                );
                return cx.out;
            }
            match global(ns, &name) {
                Some((_, _, ty)) => cx.check_element(el, ty, &local, 0),
                None => cx.push(
                    &local,
                    ViolationKind::Internal,
                    "root has no global entry".into(),
                ),
            }
        }
        Target::Event(name) => {
            if el.name.ns != ns::PRINT || el.name.local != name {
                cx.push(
                    &local,
                    ViolationKind::WrongRoot,
                    format!("expected event {name}, found {}", el.name),
                );
                return cx.out;
            }
            match global(Ns::Print, name) {
                Some((_, _, ty)) => cx.check_element(el, ty, &local, 0),
                None => cx.push(
                    &local,
                    ViolationKind::WrongRoot,
                    format!("{name} is not an event"),
                ),
            }
        }
        Target::Section(name) => {
            if el.name.ns != ns::PRINT || el.name.local != name {
                cx.push(
                    &local,
                    ViolationKind::WrongRoot,
                    format!("expected section {name}, found {}", el.name),
                );
                return cx.out;
            }
            match SECTIONS.iter().find(|(n, _)| *n == name) {
                Some((_, ty)) => cx.check_element(el, ty, &local, 0),
                None => cx.push(
                    &local,
                    ViolationKind::WrongRoot,
                    format!("{name} is not a section"),
                ),
            }
        }
    }
    cx.out
}

/// Validate an element by its own name: a known global element, or a section name in the
/// print namespace. Elements outside the print families yield no violations.
pub fn validate_root(el: &Element, opts: &Options) -> Vec<Violation> {
    let mut cx = Cx::new(opts);
    let local = el.name.local.clone();
    if let Some(ns) = Ns::of_uri(&el.name.ns) {
        match global(ns, &el.name.local) {
            Some((_, _, ty)) => cx.check_element(el, ty, &local, 0),
            None => match (ns, SECTIONS.iter().find(|(n, _)| *n == el.name.local)) {
                (Ns::Print, Some((_, ty))) => cx.check_element(el, ty, &local, 0),
                _ => cx.push(
                    &local,
                    ViolationKind::Unexpected,
                    format!("{} is not a print schema element", el.name),
                ),
            },
        }
    }
    cx.out
}

// ---------------------------------------------------------------------------------------------
// The checker
// ---------------------------------------------------------------------------------------------

struct Cx<'a> {
    opts: &'a Options,
    out: Vec<Violation>,
}

impl<'a> Cx<'a> {
    fn new(opts: &'a Options) -> Self {
        Self {
            opts,
            out: Vec::new(),
        }
    }

    fn push(&mut self, path: &str, kind: ViolationKind, message: String) {
        if self.out.len() < 64 {
            self.out.push(Violation {
                path: path.to_string(),
                kind,
                message,
            });
        }
    }

    /// Check element `el` against the type named `ty`.
    fn check_element(&mut self, el: &Element, ty: &str, path: &str, depth: usize) {
        if depth > MAX_DEPTH {
            self.push(path, ViolationKind::Internal, "nesting too deep".into());
            return;
        }
        if let Some(ct) = complex(ty) {
            self.check_attrs(el, ct.attrs, path);
            match ct.shape {
                Shape::Text(simple) => {
                    if !el.children.is_empty() {
                        self.push(
                            path,
                            ViolationKind::Unexpected,
                            "element content in a text element".into(),
                        );
                    }
                    self.check_text(el, simple, path);
                }
                Shape::Seq(items) => {
                    self.no_text(el, path);
                    self.check_seq(el, ct, items, path, depth);
                }
                Shape::Choice(items) => {
                    self.no_text(el, path);
                    self.check_choice(el, ct, items, path, depth);
                }
            }
            return;
        }
        if simple_type(ty).is_some() {
            self.check_attrs(el, NOA, path);
            if !el.children.is_empty() {
                self.push(
                    path,
                    ViolationKind::Unexpected,
                    "element content in a text element".into(),
                );
            }
            self.check_text(el, ty, path);
            return;
        }
        self.push(
            path,
            ViolationKind::Internal,
            format!("table has no type {ty}"),
        );
    }

    fn no_text(&mut self, el: &Element, path: &str) {
        if !el.text.trim().is_empty() {
            self.push(
                path,
                ViolationKind::BadValue,
                "text inside an element-only element".into(),
            );
        }
    }

    fn check_text(&mut self, el: &Element, simple: &str, path: &str) {
        let Some(t) = simple_type(simple) else {
            self.push(
                path,
                ViolationKind::Internal,
                format!("table has no value type {simple}"),
            );
            return;
        };
        let res = match (&el.qtext, t) {
            (Some(_), Simple::QName) => Ok(()),
            (Some(_), _) => Err("a QName where a plain value is required".to_string()),
            (None, Simple::Str(_)) => check_simple(t, &el.text),
            (None, _) => check_simple(t, el.text.trim()),
        };
        if let Err(e) = res {
            self.push(path, ViolationKind::BadValue, e);
        }
    }

    fn check_attrs(&mut self, el: &Element, declared: &[At], path: &str) {
        for a in &el.attrs {
            let own = Ns::of_uri(&a.name.ns);
            if a.name.ns.is_empty() || own == Some(Ns::Print) {
                let qualified = !a.name.ns.is_empty();
                match declared
                    .iter()
                    .find(|d| d.name == a.name.local && d.qualified == qualified)
                {
                    Some(d) => {
                        let res = match (&a.qvalue, simple_type(d.ty)) {
                            (Some(_), Some(Simple::QName)) => Ok(()),
                            (Some(_), _) => Err("QName value".to_string()),
                            (None, Some(t)) => check_simple(t, a.value.trim()),
                            (None, None) => Err(format!("table has no type {}", d.ty)),
                        };
                        if let Err(e) = res {
                            self.push(
                                &format!("{path}/@{}", a.name.local),
                                ViolationKind::BadAttribute,
                                e,
                            );
                        }
                    }
                    None => self.push(
                        &format!("{path}/@{}", a.name.local),
                        ViolationKind::BadAttribute,
                        "attribute not declared by this element".into(),
                    ),
                }
            } else if own == Some(Ns::Print20) && a.name.local == "Volatile" {
                if let Err(e) = check_simple(&Simple::Bool, a.value.trim()) {
                    self.push(&format!("{path}/@Volatile"), ViolationKind::BadAttribute, e);
                }
            }
            // other namespaces: the extension attribute slot, tolerated
        }
        for d in declared.iter().filter(|d| d.required) {
            let found = el
                .attrs
                .iter()
                .any(|a| a.name.local == d.name && (a.name.ns.is_empty() != d.qualified));
            if !found {
                self.push(
                    &format!("{path}/@{}", d.name),
                    ViolationKind::MissingAttribute,
                    "required attribute is absent".into(),
                );
            }
        }
    }

    fn check_seq(&mut self, el: &Element, ct: &Ct, items: &[Item], path: &str, depth: usize) {
        let own = ct.ns.uri();
        let kids = &el.children;
        let mut i = 0usize;
        for item in items {
            match *item {
                Item::El { name, min, max, ty } => {
                    let mut count = 0u32;
                    while i < kids.len() && kids[i].name.ns == own && kids[i].name.local == name {
                        if count < max {
                            let p = child_path(path, name, count, max);
                            self.check_child(&kids[i], ty, &p, depth);
                        } else if count == max {
                            self.push(
                                &format!("{path}/{name}"),
                                ViolationKind::OutOfOrder,
                                format!("{name} occurs more than {max} time(s)"),
                            );
                        }
                        count = count.saturating_add(1);
                        i += 1;
                    }
                    if count < min {
                        self.push(
                            &format!("{path}/{name}"),
                            ViolationKind::Missing,
                            format!("required child {name} is missing (or out of order)"),
                        );
                    }
                }
                Item::Any { min, max } => {
                    let mut count = 0u32;
                    while i < kids.len() && kids[i].name.ns != own {
                        if count < max {
                            self.check_foreign(&kids[i], path, depth);
                        } else if count == max {
                            self.push(
                                path,
                                ViolationKind::OutOfOrder,
                                format!("more than {max} extension element(s)"),
                            );
                        }
                        count = count.saturating_add(1);
                        i += 1;
                    }
                    if count < min {
                        self.push(
                            path,
                            ViolationKind::Missing,
                            "extension element missing".into(),
                        );
                    }
                }
                Item::OneOf(alts) => {
                    let hit = kids.get(i).and_then(|k| {
                        alts.iter()
                            .find(|(n, _)| k.name.ns == own && k.name.local == *n)
                    });
                    match (hit, kids.get(i)) {
                        (Some((n, ty)), Some(k)) => {
                            let p = format!("{path}/{n}");
                            self.check_child(k, ty, &p, depth);
                            i += 1;
                        }
                        _ => {
                            let names: Vec<&str> = alts.iter().map(|(n, _)| *n).collect();
                            self.push(
                                path,
                                ViolationKind::Missing,
                                format!("one of {} is required here", names.join(", ")),
                            );
                        }
                    }
                }
            }
        }
        self.leftovers(kids, i, ct, items, path);
    }

    fn check_choice(&mut self, el: &Element, ct: &Ct, items: &[Item], path: &str, depth: usize) {
        let own = ct.ns.uri();
        let kids = &el.children;
        let has_any = items.iter().any(|it| matches!(it, Item::Any { .. }));
        let mut i = 0usize;
        if let Some(first) = kids.first() {
            let hit = items.iter().find_map(|it| match *it {
                Item::El { name, ty, .. } if first.name.ns == own && first.name.local == name => {
                    Some((name, ty))
                }
                _ => None,
            });
            if let Some((name, ty)) = hit {
                self.check_child(first, ty, &format!("{path}/{name}"), depth);
                i = 1;
            } else if has_any && first.name.ns != own {
                while i < kids.len() && kids[i].name.ns != own {
                    self.check_foreign(&kids[i], path, depth);
                    i += 1;
                }
            }
        } else if !has_any {
            self.push(
                path,
                ViolationKind::Missing,
                "one child is required here".into(),
            );
        }
        self.leftovers(kids, i, ct, items, path);
    }

    fn leftovers(&mut self, kids: &[Element], from: usize, ct: &Ct, items: &[Item], path: &str) {
        let own = ct.ns.uri();
        for k in kids.iter().skip(from) {
            let tolerated = k.name.ns == own
                && ((self.opts.quirk_job_token
                    && k.name.local == "JobToken"
                    && matches!(ct.name, "CreatePrintJobResponseType" | "JobElementDataType"))
                    || (self.opts.quirk_printer_local_time
                        && k.name.local == "PrinterLocalTime"
                        && ct.name == "PrinterStatusType"));
            if tolerated {
                continue;
            }
            let known = k.name.ns == own
                && items.iter().any(|it| match it {
                    Item::El { name, .. } => *name == k.name.local,
                    Item::OneOf(alts) => alts.iter().any(|(n, _)| *n == k.name.local),
                    Item::Any { .. } => false,
                });
            let (kind, msg) = if known {
                (
                    ViolationKind::OutOfOrder,
                    format!("{} is out of order or repeated too often", k.name.local),
                )
            } else {
                (
                    ViolationKind::Unexpected,
                    format!("{} is not allowed in {}", k.name, ct.name),
                )
            };
            self.push(&format!("{path}/{}", k.name.local), kind, msg);
        }
    }

    /// A child with a declared type, with the strict list-domain rule on top.
    fn check_child(&mut self, kid: &Element, ty: &str, path: &str, depth: usize) {
        self.check_element(kid, ty, path, depth + 1);
        if self.opts.strict_domains && (ty == "ValueTokenListType" || ty == "ValueStringListType") {
            if let Some((_, domain)) = LIST_DOMAINS.iter().find(|(n, _)| *n == kid.name.local) {
                for av in kid
                    .children
                    .iter()
                    .filter(|c| c.name.local == "AllowedValue")
                {
                    for tok in av.text.split_whitespace() {
                        if let Err(e) = check_value(domain, tok) {
                            self.push(
                                &format!("{path}/AllowedValue"),
                                ViolationKind::BadValue,
                                format!("{tok:?} is outside the domain of {}: {e}", kid.name.local),
                            );
                        }
                    }
                }
            }
        }
    }

    fn check_foreign(&mut self, kid: &Element, path: &str, depth: usize) {
        let local = &kid.name.local;
        if let Some(ns) = Ns::of_uri(&kid.name.ns) {
            let p = format!("{path}/{local}");
            match global(ns, local) {
                Some((_, _, ty)) => self.check_element(kid, ty, &p, depth + 1),
                None => self.push(
                    &p,
                    ViolationKind::Unexpected,
                    format!("{} is not a known extension element", kid.name),
                ),
            }
        }
        // vendor namespaces: tolerated
    }
}

fn child_path(path: &str, name: &str, index: u32, max: u32) -> String {
    if max > 1 {
        format!("{path}/{name}[{}]", index + 1)
    } else {
        format!("{path}/{name}")
    }
}

// ---------------------------------------------------------------------------------------------
// Value domains
// ---------------------------------------------------------------------------------------------

/// Check a text against a value domain.
pub fn check_simple(t: &Simple, text: &str) -> Result<(), String> {
    match *t {
        Simple::Bool => match text {
            "true" | "false" | "1" | "0" => Ok(()),
            _ => Err(format!("{text:?} is not a boolean")),
        },
        Simple::Int(lo, hi) => {
            let n = parse_int(text)?;
            if n < lo || n > hi {
                Err(format!("{n} is outside {lo}..{hi}"))
            } else {
                Ok(())
            }
        }
        Simple::Str(max) => {
            if text.chars().count() > max {
                Err(format!("longer than {max} characters"))
            } else {
                Ok(())
            }
        }
        Simple::Len(n) => {
            if text.chars().count() == n {
                Ok(())
            } else {
                Err(format!("must be exactly {n} characters"))
            }
        }
        Simple::Token(max) => token(text, max),
        Simple::Tokens => {
            let mut any = false;
            for tok in text.split_whitespace() {
                token_with(tok, 255, true)?;
                any = true;
            }
            if any {
                Ok(())
            } else {
                Err("an empty token list".into())
            }
        }
        Simple::DateTime => date_time(text),
        Simple::Uri => {
            if text
                .chars()
                .any(|c| c.is_control() || c.is_whitespace() || "<>\"{}|\\^`".contains(c))
            {
                Err(format!("{text:?} is not a URI"))
            } else {
                Ok(())
            }
        }
        Simple::QName => {
            let ok = match text.split_once(':') {
                Some((p, l)) => xml::is_ncname(p) && xml::is_ncname(l),
                None => xml::is_ncname(text),
            };
            if ok {
                Ok(())
            } else {
                Err(format!("{text:?} is not a QName"))
            }
        }
        Simple::Base64 => base64_ok(text),
        Simple::Double => {
            let tt = text.trim();
            if matches!(tt, "INF" | "-INF" | "NaN") || tt.parse::<f64>().is_ok() {
                Ok(())
            } else {
                Err(format!("{text:?} is not a number"))
            }
        }
        Simple::Enum { vals, ext, .. } => {
            if vals.contains(&text) {
                return Ok(());
            }
            let ok = match ext {
                Ext::Closed => false,
                Ext::Keyword => keyword_ns(text),
                Ext::Mime => mime_ext(text),
                Ext::MediaSize => media_size_ext(text),
                Ext::MediaColor => custom_name(text, "custom-media-color-"),
                Ext::MediaType => custom_name(text, "custom-media-type-"),
                Ext::InputTray => digits_after(text, "Tray"),
                Ext::OutputBin => {
                    digits_after(text, "Mailbox")
                        || digits_after(text, "Stacker")
                        || digits_after(text, "Bin")
                }
            };
            if ok {
                Ok(())
            } else {
                Err(format!("{text:?} is not an allowed value"))
            }
        }
    }
}

fn parse_int(text: &str) -> Result<i64, String> {
    let t = text.trim();
    let digits = t.strip_prefix(['+', '-']).unwrap_or(t);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("{text:?} is not an integer"));
    }
    t.parse::<i64>()
        .map_err(|_| format!("{text:?} is out of range"))
}

fn is_token_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '.' | '-' | '_' | ':')
}

/// Name token; `mime_chars` also admits `/` and `+` (the package's own prose uses MIME strings
/// in lists its schema types as plain token lists, so those lists must accept them).
fn token_with(text: &str, max: usize, mime_chars: bool) -> Result<(), String> {
    let ok = |c: char| is_token_char(c) || (mime_chars && matches!(c, '/' | '+'));
    if text.is_empty() || !text.chars().all(ok) {
        return Err(format!("{text:?} is not a name token"));
    }
    if text.chars().count() > max {
        return Err(format!("{text:?} is longer than {max} characters"));
    }
    Ok(())
}

fn token(text: &str, max: usize) -> Result<(), String> {
    token_with(text, max, false)
}

fn keyword_ns(text: &str) -> bool {
    match text.split_once(':') {
        Some((p, k)) => {
            !p.is_empty()
                && p.chars().all(|c| c.is_alphanumeric() || c == '_')
                && !k.is_empty()
                && k.chars()
                    .all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.'))
        }
        None => false,
    }
}

fn mime_ext(text: &str) -> bool {
    let mut cs = text.chars();
    match cs.next() {
        Some(c) if c.is_alphanumeric() || c == '_' => {}
        _ => return false,
    }
    cs.all(|c| c.is_alphanumeric() || matches!(c, '_' | ':' | '/' | ';' | '-' | '\\' | '+' | '.'))
}

fn custom_name(text: &str, prefix: &str) -> bool {
    match text.strip_prefix(prefix) {
        Some(rest) => {
            let mut cs = rest.chars();
            matches!(cs.next(), Some(c) if c.is_ascii_lowercase())
                && cs.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        }
        None => false,
    }
}

fn digits_after(text: &str, prefix: &str) -> bool {
    match text.strip_prefix(prefix) {
        Some(rest) => !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()),
        None => false,
    }
}

/// `<prefix>_<name>_<w>x<h><unit>`: prefix `[a-z0-9][a-z0-9.]*`, name `[a-z0-9][a-z0-9-]*`,
/// dimensions positive decimals without redundant zeros, unit `in` or `mm`.
fn media_size_ext(text: &str) -> bool {
    let parts: Vec<&str> = text.split('_').collect();
    let [prefix, name, dims] = parts.as_slice() else {
        return false;
    };
    let alnum = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit();
    let head_ok = |s: &str, extra: char| {
        let mut cs = s.chars();
        matches!(cs.next(), Some(c) if alnum(c)) && cs.all(|c| alnum(c) || c == extra)
    };
    if !head_ok(prefix, '.') || !head_ok(name, '-') {
        return false;
    }
    let Some(body) = dims.strip_suffix("in").or_else(|| dims.strip_suffix("mm")) else {
        return false;
    };
    match body.split_once('x') {
        Some((w, h)) => dim_ok(w) && dim_ok(h),
        None => false,
    }
}

fn dim_ok(s: &str) -> bool {
    let (int, frac) = match s.split_once('.') {
        Some((i, f)) => (i, Some(f)),
        None => (s, None),
    };
    if int.is_empty() || !int.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    if let Some(f) = frac {
        if f.is_empty() || !f.bytes().all(|b| b.is_ascii_digit()) || f.ends_with('0') {
            return false;
        }
    }
    if int == "0" {
        frac.is_some()
    } else {
        !int.starts_with('0')
    }
}

fn base64_ok(text: &str) -> Result<(), String> {
    let compact: Vec<u8> = text.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    if !compact.len().is_multiple_of(4) {
        return Err("base64 length is not a multiple of 4".into());
    }
    let pad = compact.iter().rev().take_while(|b| **b == b'=').count();
    if pad > 2 {
        return Err("too much base64 padding".into());
    }
    let body = &compact[..compact.len() - pad];
    if body
        .iter()
        .all(|b| b.is_ascii_alphanumeric() || *b == b'+' || *b == b'/')
    {
        Ok(())
    } else {
        Err("not base64".into())
    }
}

fn date_time(text: &str) -> Result<(), String> {
    let bad = || Err(format!("{text:?} is not an xs:dateTime"));
    let t = text.trim();
    let t = t.strip_prefix('-').unwrap_or(t);
    let Some((date, time)) = t.split_once('T') else {
        return bad();
    };
    let dp: Vec<&str> = date.split('-').collect();
    let [y, mo, d] = dp.as_slice() else {
        return bad();
    };
    let num = |s: &str, len: usize| -> Option<u32> {
        if s.len() == len && s.bytes().all(|b| b.is_ascii_digit()) {
            s.parse().ok()
        } else {
            None
        }
    };
    if y.len() < 4 || !y.bytes().all(|b| b.is_ascii_digit()) {
        return bad();
    }
    let year: i64 = y.parse().unwrap_or(0);
    let (Some(month), Some(day)) = (num(mo, 2), num(d, 2)) else {
        return bad();
    };
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let dim = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return bad(),
    };
    if day == 0 || day > dim {
        return bad();
    }
    let (clock, zone) = match time.find(['Z', '+', '-']) {
        Some(at) => time.split_at(at),
        None => (time, ""),
    };
    let cp: Vec<&str> = clock.split(':').collect();
    let [hh, mm, ss] = cp.as_slice() else {
        return bad();
    };
    let (sec_int, sec_frac) = match ss.split_once('.') {
        Some((a, b)) => (a, Some(b)),
        None => (*ss, None),
    };
    if let Some(f) = sec_frac {
        if f.is_empty() || !f.bytes().all(|b| b.is_ascii_digit()) {
            return bad();
        }
    }
    let (Some(h), Some(mi), Some(se)) = (num(hh, 2), num(mm, 2), num(sec_int, 2)) else {
        return bad();
    };
    let frac_zero = sec_frac.is_none_or(|f| f.bytes().all(|b| b == b'0'));
    if h > 24 || mi > 59 || se > 59 || (h == 24 && (mi != 0 || se != 0 || !frac_zero)) {
        return bad();
    }
    match zone {
        "" | "Z" => Ok(()),
        z => {
            let rest = z.get(1..).unwrap_or("");
            match rest.split_once(':') {
                Some((zh, zm))
                    if matches!(num(zh, 2), Some(h) if h <= 14)
                        && matches!(num(zm, 2), Some(m) if m <= 59) =>
                {
                    Ok(())
                }
                _ => bad(),
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wsd::xml::{Element, QName};

    fn w(name: &str) -> Element {
        Element::new(ns::PRINT, name)
    }
    fn wt(name: &str, text: &str) -> Element {
        Element::text_el(ns::PRINT, name, text)
    }

    fn status() -> Element {
        w("PrinterStatus")
            .with_child(wt("PrinterCurrentTime", "2026-10-13T12:00:00Z"))
            .with_child(wt("PrinterState", "Idle"))
            .with_child(w("PrinterStateReasons").with_child(wt("PrinterStateReason", "None")))
            .with_child(wt("QueuedJobCount", "0"))
    }

    fn kinds(v: &[Violation]) -> Vec<ViolationKind> {
        v.iter().map(|x| x.kind).collect()
    }

    #[test]
    fn minimal_status_is_valid() {
        let v = validate_body(Target::Section("PrinterStatus"), &status());
        assert!(v.is_empty(), "{v:?}");
    }

    #[test]
    fn missing_required_and_order_are_reported() {
        let mut s = status();
        s.children.remove(3);
        let v = validate_body(Target::Section("PrinterStatus"), &s);
        assert!(kinds(&v).contains(&ViolationKind::Missing), "{v:?}");

        let mut s = status();
        s.children.swap(0, 1);
        let v = validate_body(Target::Section("PrinterStatus"), &s);
        assert!(!v.is_empty(), "swapped children must not validate");
    }

    #[test]
    fn bad_values_are_reported() {
        let mut s = status();
        s.children[1] = wt("PrinterState", "Busy");
        s.children[0] = wt("PrinterCurrentTime", "2026-13-01T00:00:00Z");
        let v = validate_body(Target::Section("PrinterStatus"), &s);
        assert_eq!(
            v.iter()
                .filter(|x| x.kind == ViolationKind::BadValue)
                .count(),
            2,
            "{v:?}"
        );
        let mut s = status();
        s.children[1] = wt("PrinterState", "acme:Warming");
        assert!(validate_body(Target::Section("PrinterStatus"), &s).is_empty());
    }

    #[test]
    fn unexpected_children_and_attributes() {
        let s = status().with_child(wt("PrinterLocalTime", "2026-10-13T12:00:00Z"));
        let v = validate_body(Target::Section("PrinterStatus"), &s);
        assert_eq!(kinds(&v), vec![ViolationKind::Unexpected], "{v:?}");
        let mut opts = Options::STRICT;
        opts.quirk_printer_local_time = true;
        assert!(validate_body_with(Target::Section("PrinterStatus"), &s, &opts).is_empty());
        let s = status().with_attr("Bogus", "1");
        let v = validate_body(Target::Section("PrinterStatus"), &s);
        assert_eq!(kinds(&v), vec![ViolationKind::BadAttribute], "{v:?}");
    }

    #[test]
    fn vendor_extensions_are_tolerated_known_ones_checked() {
        let vendor = Element::text_el("urn:acme", "Thermal", "hot");
        let s = status().with_child(vendor);
        assert!(validate_body(Target::Section("PrinterStatus"), &s).is_empty());
        let bad = status().with_child(Element::text_el(ns::PRINT20, "SupportsWSPrintV20", "maybe"));
        assert!(!validate_body(Target::Section("PrinterStatus"), &bad).is_empty());
        let good = status().with_attr_ns(QName::new(ns::PRINT20, "Volatile"), "true");
        assert!(validate_body(Target::Section("PrinterStatus"), &good).is_empty());
    }

    #[test]
    fn element_data_choice_and_attributes() {
        let ok = w("GetPrinterElementsResponse").with_child(
            w("PrinterElements")
                .with_child(
                    w("ElementData")
                        .with_attr_qname("Name", QName::new(ns::PRINT, "PrinterStatus"))
                        .with_attr("Valid", "true")
                        .with_child(status()),
                )
                .with_child(
                    w("ElementData")
                        .with_attr_qname("Name", QName::new(ns::PRINT, "Nope"))
                        .with_attr("Valid", "false"),
                ),
        );
        let v = validate_body(Target::Response(PrintOp::GetPrinterElements), &ok);
        assert!(v.is_empty(), "{v:?}");

        let no_valid = w("GetPrinterElementsResponse").with_child(
            w("PrinterElements")
                .with_child(w("ElementData").with_attr("Name", "wprt:PrinterStatus")),
        );
        let v = validate_body(Target::Response(PrintOp::GetPrinterElements), &no_valid);
        assert!(
            kinds(&v).contains(&ViolationKind::MissingAttribute),
            "{v:?}"
        );

        let two = w("GetPrinterElementsResponse").with_child(
            w("PrinterElements").with_child(
                w("ElementData")
                    .with_attr("Name", "wprt:PrinterStatus")
                    .with_attr("Valid", "true")
                    .with_child(status())
                    .with_child(w("PrinterDescription")),
            ),
        );
        assert!(!validate_body(Target::Response(PrintOp::GetPrinterElements), &two).is_empty());
    }

    #[test]
    fn wrong_root_and_non_print_ops() {
        let v = validate_body(Target::Response(PrintOp::CancelJob), &w("CancelJobRequest"));
        assert_eq!(kinds(&v), vec![ViolationKind::WrongRoot]);
        assert!(validate_body(Target::Response(PrintOp::Subscribe), &w("Whatever")).is_empty());
        assert!(
            validate_body(Target::Response(PrintOp::PrepareToPrint), &w("Whatever")).is_empty()
        );
        assert!(validate_body(
            Target::Response(PrintOp::CancelJob),
            &w("CancelJobResponse")
        )
        .is_empty());
    }

    #[test]
    fn ticket_values_and_must_honor() {
        let ticket = w("PrintTicket")
            .with_child(
                w("JobDescription")
                    .with_child(wt("JobName", "x"))
                    .with_child(wt("JobOriginatingUserName", "u")),
            )
            .with_child(w("JobProcessing").with_child(
                wt("Copies", "2").with_attr_ns(QName::new(ns::PRINT, "MustHonor"), "true"),
            ))
            .with_child(
                w("DocumentProcessing")
                    .with_child(wt("MediaSizeName", "iso_a4_210x297mm"))
                    .with_child(wt("Orientation", "Portrait"))
                    .with_child(wt("Sides", "OneSided")),
            );
        let req = w("CreatePrintJobRequest").with_child(ticket.clone());
        assert!(validate_body(Target::Request(PrintOp::CreatePrintJob), &req).is_empty());
        let mut bad = ticket;
        bad.children[2].children[0] = wt("MediaSizeName", "A4");
        let req = w("CreatePrintJobRequest").with_child(bad);
        assert_eq!(
            kinds(&validate_body(
                Target::Request(PrintOp::CreatePrintJob),
                &req
            )),
            vec![ViolationKind::BadValue]
        );
    }

    #[test]
    fn capability_domains_are_strict_only_when_asked() {
        let caps = w("PrinterCapabilities").with_child(
            w("JobValues").with_child(
                w("DocumentProcessing")
                    .with_child(w("Orientation").with_child(wt("AllowedValue", "Sideways"))),
            ),
        );
        assert!(!validate_body(Target::Section("PrinterCapabilities"), &caps).is_empty());
        let v = validate_body_with(
            Target::Section("PrinterCapabilities"),
            &caps,
            &Options::LENIENT,
        );
        assert!(v.is_empty(), "{v:?}");
    }

    #[test]
    fn job_token_quirk_is_opt_in() {
        let resp = w("CreatePrintJobResponse")
            .with_child(wt("JobId", "7"))
            .with_child(wt("JobToken", "abc"));
        assert!(!validate_body(Target::Response(PrintOp::CreatePrintJob), &resp).is_empty());
        let mut opts = Options::STRICT;
        opts.quirk_job_token = true;
        assert!(
            validate_body_with(Target::Response(PrintOp::CreatePrintJob), &resp, &opts).is_empty()
        );
    }

    #[test]
    fn value_domains() {
        for (ty, good, bad) in [
            ("MediaSizeNameExtType", "iso_a4_210x297mm", "A4"),
            (
                "MediaSizeNameExtType",
                "custom_label_4x6in",
                "custom_label_04x6in",
            ),
            (
                "MediaSizeNameExtType",
                "om_small-photo_100x150mm",
                "om_small-photo_100.0x150mm",
            ),
            ("DocumentFormatExtType", "application/pdf", "/pdf"),
            ("DocumentFormatExtType", "image/pwg-raster", ""),
            ("Tokens", "application/vnd.ms-PrintSchemaTicket+xml", "a b!"),
            ("CompressionExtType", "Gzip", "zip"),
            ("CompressionExtType", "acme:fast", "acme:"),
            ("InputBinNameType", "Tray12", "Tray"),
            ("OutputBinNameType", "Bin1", "OutputBin1"),
            ("LevelFreeRestrictType", "-1", "101"),
            ("EventRateBaseType", "600", "0"),
            (
                "DateTimeExtType",
                "2026-10-13T00:00:00.5+02:00",
                "2026-10-13 00:00:00",
            ),
            (
                "DateTimeExtType",
                "2024-02-29T23:59:59Z",
                "2023-02-29T00:00:00Z",
            ),
            ("ColorRepType", "FF00FF", "FF00F"),
        ] {
            assert!(check_value(ty, good).is_ok(), "{ty} {good}");
            assert!(check_value(ty, bad).is_err(), "{ty} {bad}");
        }
        assert!(check_value("nope", "x").is_err());
    }

    #[test]
    fn set_printer_elements_choice() {
        let ok = Element::new(ns::PRINT12, "SetPrinterElementsRequest").with_child(
            Element::new(ns::PRINT12, "ElementsToSet").with_child(
                Element::new(ns::PRINT12, "ElementToSet")
                    .with_child(Element::text_el(ns::PRINT12, "ElementPath", "a/b"))
                    .with_child(Element::text_el(ns::PRINT12, "Integer", "5")),
            ),
        );
        assert!(validate_body(Target::Request(PrintOp::SetPrinterElements), &ok).is_empty());
        let none = Element::new(ns::PRINT12, "SetPrinterElementsRequest").with_child(
            Element::new(ns::PRINT12, "ElementsToSet").with_child(
                Element::new(ns::PRINT12, "ElementToSet").with_child(Element::text_el(
                    ns::PRINT12,
                    "ElementPath",
                    "a/b",
                )),
            ),
        );
        assert!(!validate_body(Target::Request(PrintOp::SetPrinterElements), &none).is_empty());
    }

    #[test]
    fn every_table_reference_resolves() {
        for ct in TYPES {
            let items: &[Item] = match ct.shape {
                Shape::Seq(i) | Shape::Choice(i) => i,
                Shape::Text(t) => {
                    assert!(simple_type(t).is_some(), "{}: value type {t}", ct.name);
                    &[]
                }
            };
            for it in items {
                let tys: Vec<&str> = match it {
                    Item::El { ty, .. } => vec![ty],
                    Item::OneOf(alts) => alts.iter().map(|(_, t)| *t).collect(),
                    Item::Any { .. } => vec![],
                };
                for ty in tys {
                    assert!(
                        complex(ty).is_some() || simple_type(ty).is_some(),
                        "{}: unresolved type {ty}",
                        ct.name
                    );
                }
            }
            for a in ct.attrs {
                assert!(
                    simple_type(a.ty).is_some(),
                    "{}: attribute type {}",
                    ct.name,
                    a.ty
                );
            }
        }
        for (_, name, ty) in GLOBALS {
            assert!(
                complex(ty).is_some() || simple_type(ty).is_some(),
                "{name}: {ty}"
            );
        }
        for (_, ty) in SECTIONS {
            assert!(complex(ty).is_some(), "{ty}");
        }
        for (_, d) in LIST_DOMAINS {
            assert!(simple_type(d).is_some(), "{d}");
        }
        let mut names: Vec<&str> = TYPES.iter().map(|c| c.name).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(before, names.len(), "duplicate type names");
    }

    // ------------------------------------------------------------------------------------
    // Cross-check against the generated schema model (skipped when it is absent)
    // ------------------------------------------------------------------------------------

    use serde_json::Value;
    use std::collections::BTreeSet;

    type Row = (String, u64, u64, Option<Value>);

    fn load_model() -> Option<Value> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../docs/reference/schema/schema-model.json");
        let raw = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&raw).ok()
    }

    fn strip(n: &str) -> &str {
        n.rsplit_once(':').map_or(n, |(_, l)| l)
    }

    fn bound(v: &Value) -> u64 {
        match v {
            Value::String(s) if s == "unbounded" => u64::from(UNB),
            other => other.as_u64().unwrap_or(0),
        }
    }

    fn walk_particle(p: &Value, out: &mut Vec<Row>) {
        match p["kind"].as_str() {
            Some("element") => out.push((
                strip(p["name"].as_str().unwrap_or("")).to_string(),
                bound(&p["min"]),
                bound(&p["max"]),
                Some(p.clone()),
            )),
            Some("any") => out.push(("*".into(), bound(&p["min"]), bound(&p["max"]), None)),
            _ => {
                if let Some(kids) = p["children"].as_array() {
                    for k in kids {
                        walk_particle(k, out);
                    }
                }
            }
        }
    }

    /// Content rows of a model type (base first); returns whether the top particle is a choice.
    fn model_rows(model: &Value, ty: &str, out: &mut Vec<Row>) -> bool {
        let Some(t) = model["complex_types"].get(ty) else {
            return false;
        };
        let mut is_choice = false;
        if let Some(base) = t["base"].as_str() {
            if t["derivation"] == "extension" && model["complex_types"].get(base).is_some() {
                is_choice |= model_rows(model, base, out);
                if matches!(out.last(), Some(row) if row.0 == "*") {
                    out.pop();
                }
            }
        }
        if let Some(p) = t.get("particle").filter(|p| p.is_object()) {
            is_choice |= p["kind"] == "choice";
            walk_particle(p, out);
        }
        is_choice
    }

    fn compare_rows(
        model: &Value,
        mine: &Ct,
        rows: &[Row],
        is_choice: bool,
        problems: &mut Vec<String>,
    ) {
        let (Shape::Seq(items) | Shape::Choice(items)) = mine.shape else {
            return;
        };
        let nested_choice = items.iter().any(|i| matches!(i, Item::OneOf(_)));
        let mut mine_rows: Vec<(String, u64, u64, Option<&'static str>)> = Vec::new();
        for it in items {
            match *it {
                Item::El { name, min, max, ty } => {
                    let (mn, mx) = if is_choice {
                        (1, 1)
                    } else {
                        (u64::from(min), u64::from(max))
                    };
                    mine_rows.push((name.into(), mn, mx, Some(ty)));
                }
                Item::Any { min, max } => {
                    mine_rows.push(("*".into(), u64::from(min), u64::from(max), None));
                }
                Item::OneOf(alts) => {
                    for (n, t) in alts {
                        mine_rows.push(((*n).into(), 1, 1, Some(*t)));
                    }
                }
            }
        }
        let model_names: Vec<&str> = rows.iter().map(|r| r.0.as_str()).collect();
        let my_names: Vec<&str> = mine_rows.iter().map(|r| r.0.as_str()).collect();
        if model_names != my_names {
            problems.push(format!(
                "{}: children {my_names:?} vs model {model_names:?}",
                mine.name
            ));
            return;
        }
        for (mr, row) in mine_rows.iter().zip(rows) {
            if !nested_choice && (mr.1 != row.1 || mr.2 != row.2) {
                problems.push(format!(
                    "{}: {} occurs {}..{} vs model {}..{}",
                    mine.name, mr.0, mr.1, mr.2, row.1, row.2
                ));
            }
            if let (Some(my_ty), Some(p)) = (mr.3, &row.3) {
                if let Some(mt) = p["type"].as_str() {
                    if model["complex_types"].get(mt).is_some() {
                        if let Some(c) = complex(my_ty) {
                            if c.name != strip(mt) {
                                problems.push(format!(
                                    "{}: {} has type {my_ty}, model {mt}",
                                    mine.name, mr.0
                                ));
                            }
                        }
                    }
                } else if let Some(anon) = p.get("anon_type").filter(|a| a.is_object()) {
                    if let Some(sub) = complex(my_ty) {
                        let mut r2 = Vec::new();
                        let ch = anon["particle"]["kind"] == "choice";
                        if anon["particle"].is_object() {
                            walk_particle(&anon["particle"], &mut r2);
                        }
                        compare_rows(model, sub, &r2, ch, problems);
                    }
                }
            }
        }
    }

    fn flatten_enum(
        model: &Value,
        ty: &str,
        vals: &mut BTreeSet<String>,
        patterns: &mut usize,
        depth: usize,
    ) {
        if depth > 12 {
            return;
        }
        let Some(t) = model["simple_types"].get(ty) else {
            if let Some(c) = model["complex_types"].get(ty) {
                if let Some(base) = c["base"].as_str() {
                    flatten_enum(model, base, vals, patterns, depth + 1);
                }
            }
            return;
        };
        for e in t["facets"]["enumerations"].as_array().into_iter().flatten() {
            if let Some(v) = e["value"].as_str() {
                vals.insert(v.to_string());
            }
        }
        *patterns += t["facets"]["patterns"].as_array().map_or(0, Vec::len);
        if let Some(b) = t["base"].as_str() {
            flatten_enum(model, b, vals, patterns, depth + 1);
        }
        for mm in t["members"].as_array().into_iter().flatten() {
            if let Some(n) = mm.as_str() {
                flatten_enum(model, n, vals, patterns, depth + 1);
            }
        }
    }

    fn prefix_of(n: Ns) -> &'static str {
        match n {
            Ns::Print => "wprt",
            Ns::Print11 => "wprt11",
            Ns::Print12 => "wprt12",
            Ns::Print20 => "wprt20",
        }
    }

    #[test]
    fn tables_agree_with_the_generated_schema_model() {
        let Some(model) = load_model() else {
            eprintln!("schema-model.json not present: cross-check skipped");
            return;
        };
        let mut problems: Vec<String> = Vec::new();

        // 1. complex types: same content model and attributes
        let mut compared = 0usize;
        let model_keys: Vec<String> = model["complex_types"]
            .as_object()
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default();
        for mine in TYPES {
            let Some(key) = model_keys.iter().find(|k| strip(k) == mine.name).cloned() else {
                let anonymous = mine.name.starts_with("Cap")
                    || mine.name.starts_with("JobValues")
                    || mine.name == "ConditionHistoryEntryType";
                if !anonymous && !matches!(mine.shape, Shape::Text(_)) {
                    problems.push(format!("{}: not in the model", mine.name));
                }
                continue;
            };
            compared += 1;
            if !matches!(mine.shape, Shape::Text(_)) {
                let mut rows = Vec::new();
                let is_choice = model_rows(&model, &key, &mut rows);
                if matches!(mine.shape, Shape::Choice(_)) != is_choice {
                    problems.push(format!("{}: choice/sequence mismatch", mine.name));
                }
                compare_rows(&model, mine, &rows, is_choice, &mut problems);
            }
            let mut attrs: Vec<(String, bool)> = Vec::new();
            let mut cur = Some(key.clone());
            while let Some(k) = cur {
                match model["complex_types"].get(&k) {
                    Some(t) => {
                        for a in t["attributes"].as_array().into_iter().flatten() {
                            attrs.push((
                                strip(a["name"].as_str().unwrap_or("")).to_string(),
                                a["use"] == "required",
                            ));
                        }
                        cur = t["base"]
                            .as_str()
                            .filter(|b| model["complex_types"].get(*b).is_some())
                            .map(str::to_string);
                    }
                    None => cur = None,
                }
            }
            let mut mine_attrs: Vec<(String, bool)> = mine
                .attrs
                .iter()
                .map(|a| (a.name.to_string(), a.required))
                .collect();
            attrs.sort();
            mine_attrs.sort();
            if attrs != mine_attrs {
                problems.push(format!(
                    "{}: attributes {mine_attrs:?} vs model {attrs:?}",
                    mine.name
                ));
            }
        }
        if compared < 70 {
            problems.push(format!("only {compared} types compared"));
        }

        // 2. global elements both ways
        let elements = model["elements"].as_object().cloned().unwrap_or_default();
        for qn in elements.keys() {
            let local = strip(qn);
            let found = GLOBALS
                .iter()
                .any(|g| g.1 == local && qn.starts_with(&format!("{}:", prefix_of(g.0))));
            if !found {
                problems.push(format!("global {qn}: missing from our table"));
            }
        }
        for (ns, name, _) in GLOBALS {
            let key = format!("{}:{name}", prefix_of(*ns));
            if !elements.contains_key(&key) {
                problems.push(format!("global {key} is not in the model"));
            }
        }

        // 3. enumerations
        for (name, t) in SIMPLES {
            let Simple::Enum { xsd, vals, ext } = t else {
                continue;
            };
            let _ = xsd;
            let mut set = BTreeSet::new();
            let mut pats = 0usize;
            let mut found = false;
            for pre in ["wprt:", "wprt12:"] {
                flatten_enum(&model, &format!("{pre}{name}"), &mut set, &mut pats, 0);
                found |= !set.is_empty();
            }
            let mine: BTreeSet<String> = vals.iter().map(|v| (*v).to_string()).collect();
            if !found {
                problems.push(format!("{name}: model type not found"));
            } else if set != mine {
                problems.push(format!(
                    "{name}: values differ; ours-only {:?}, model-only {:?}",
                    mine.difference(&set).collect::<Vec<_>>(),
                    set.difference(&mine).collect::<Vec<_>>()
                ));
            }
            if *ext != Ext::Closed && pats == 0 {
                problems.push(format!(
                    "{name}: we allow extensions, the model has no pattern"
                ));
            }
            if *ext == Ext::Closed && pats > 0 {
                problems.push(format!(
                    "{name}: closed here, the model has {pats} extension pattern(s)"
                ));
            }
        }

        let faults = model["faults"][0]["values"].as_array().map_or(0, Vec::len);
        if faults != 15 {
            problems.push(format!(
                "expected 15 fault codes in the model, found {faults}"
            ));
        }
        assert!(
            problems.is_empty(),
            "tables disagree with the model:\n{}",
            problems.join("\n")
        );
    }
}
