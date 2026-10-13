//! Printer profiles: what a fake printer is and what it can do.
//!
//! A profile is a JSON file `profiles/<id>.json`. Two layouts load:
//!
//! * **schema 2** (`"schema_version": 2`, the current one): nested sections for identity,
//!   protocol versions, description, documents, print options, default ticket, bins,
//!   consumables, preloaded conditions and vendor quirks. Unknown keys are rejected so a typo
//!   never silently changes the published capabilities; every value that ends up on the wire
//!   is checked against the same value domains the reply validator uses.
//! * **schema 1** (no `schema_version`, the original flat file): upgraded on load with the
//!   defaults that file always implied, so existing profiles keep their published values.
//!
//! [`PrinterProfile`] is the model; both layouts load into it.
//!
//! All problems of a file are reported together, each with the path of the offending field.

use std::fs;
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::wsd::elements::schema;

/// Version written by this crate and the highest one it reads.
pub const SCHEMA_VERSION: u32 = 2;

// ---------------------------------------------------------------------------------------------
// The flat (schema 1) layout
// ---------------------------------------------------------------------------------------------

fn default_manufacturer() -> String {
    "fakeprint".into()
}
fn default_model() -> String {
    "Fake WSD Print".into()
}
fn default_device_id() -> String {
    "MFG:Fake;CMD:PJL,PCL,POSTSCRIPT;MDL:WSD Print;CLS:PRINTER;DES:Fake WSD printer;CID:MS_IPP;"
        .into()
}
fn default_true() -> bool {
    true
}
fn default_ppm() -> u32 {
    20
}
fn default_ppm_color() -> u32 {
    12
}
fn default_formats() -> Vec<String> {
    vec![
        "application/octet-stream".into(),
        "application/pdf".into(),
        "image/pwg-raster".into(),
    ]
}

/// The original nine-field flat file (schema 1); unknown keys are errors.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FlatFile {
    id: String,
    label: String,
    #[serde(default = "default_manufacturer")]
    manufacturer: String,
    #[serde(default = "default_model")]
    model: String,
    #[serde(default = "default_device_id")]
    device_id: String,
    #[serde(default = "default_true")]
    color_supported: bool,
    #[serde(default = "default_ppm")]
    pages_per_minute: u32,
    #[serde(default = "default_ppm_color")]
    pages_per_minute_color: u32,
    #[serde(default = "default_formats")]
    document_formats: Vec<String>,
    #[serde(default = "default_true")]
    multiple_document_jobs: bool,
}

impl FlatFile {
    /// Upgrade to the current model with the defaults the flat file always implied.
    fn upgrade(self) -> PrinterProfile {
        let mut p = PrinterProfile::new(&self.id, &self.label);
        p.identity.manufacturer = self.manufacturer;
        p.identity.model = self.model;
        p.identity.device_id = Some(self.device_id);
        p.description.color_modes = if self.color_supported {
            default_color_modes()
        } else {
            vec!["monochrome".into()]
        };
        p.description.pages_per_minute = self.pages_per_minute;
        p.description.pages_per_minute_color = (self.color_supported
            && self.pages_per_minute_color > 0)
            .then_some(self.pages_per_minute_color);
        p.description.multiple_document_jobs = self.multiple_document_jobs;
        p.documents.formats = self.document_formats;
        p
    }
}

// ---------------------------------------------------------------------------------------------
// The rich model (schema 2)
// ---------------------------------------------------------------------------------------------

/// A complete printer description. Construct through [`PrinterProfile::from_json`] /
/// [`PrinterProfile::load`] (which validate) or [`PrinterProfile::new`] (defaults).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrinterProfile {
    #[serde(default = "current_version")]
    pub schema_version: u32,
    /// File stem; also the id the lab selects the profile by.
    pub id: String,
    /// Human label; the default `PrinterName` and friendly name.
    pub label: String,
    #[serde(default)]
    pub identity: Identity,
    #[serde(default)]
    pub versions: Versions,
    #[serde(default)]
    pub description: Description,
    #[serde(default)]
    pub documents: Documents,
    #[serde(default)]
    pub print: PrintOptions,
    #[serde(default)]
    pub default_ticket: DefaultTicket,
    /// Input bins. Absent: one `Tray1` with Letter. `[]`: none published.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inputs: Option<Vec<InputBin>>,
    /// Output bins. Absent: one `Bin1`. `[]`: none published.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outputs: Option<Vec<OutputBin>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub storage: Vec<StorageEntry>,
    /// Finishings. Absent: published only when the printer duplexes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finishings: Option<Finishings>,
    /// Consumables (toner, ink). Absent: none published.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consumables: Option<Vec<Consumable>>,
    /// Conditions that are already active when the printer starts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<ConditionSpec>,
    #[serde(default)]
    pub extensions: Extensions,
    #[serde(default)]
    pub quirks: ProfileQuirks,
}

fn current_version() -> u32 {
    SCHEMA_VERSION
}

/// Who the printer says it is.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Identity {
    #[serde(default = "default_manufacturer")]
    pub manufacturer: String,
    #[serde(default = "default_model")]
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub serial_number: Option<String>,
    #[serde(default = "default_firmware")]
    pub firmware_version: String,
    /// `PrinterName`; absent: the profile label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub friendly_name: Option<String>,
    #[serde(default = "default_location")]
    pub location: String,
    /// `PrinterInfo` text.
    #[serde(default = "default_info")]
    pub info: String,
    /// Language tag of the localized strings.
    #[serde(default = "default_lang")]
    pub language: String,
    /// Fixed device UUID; absent: the lab generates one per instance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uuid: Option<String>,
    /// IEEE 1284 device id; absent: derived from manufacturer, model and formats.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
}

fn default_firmware() -> String {
    "1.0.0".into()
}
fn default_location() -> String {
    "lab".into()
}
fn default_info() -> String {
    "fakeprint WSD Phase 1".into()
}
fn default_lang() -> String {
    "en-US".into()
}

impl Default for Identity {
    fn default() -> Self {
        Identity {
            manufacturer: default_manufacturer(),
            model: default_model(),
            serial_number: None,
            firmware_version: default_firmware(),
            friendly_name: None,
            location: default_location(),
            info: default_info(),
            language: default_lang(),
            uuid: None,
            device_id: Some(default_device_id()),
        }
    }
}

/// Which WS-Print versions the printer offers. 1.0 is always on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Versions {
    /// Driver configuration and consumable colours.
    #[serde(default)]
    pub v11: bool,
    /// `SetPrinterElements`.
    #[serde(default)]
    pub v12: bool,
    /// Driverless (mobile) extensions.
    #[serde(default)]
    pub v20: bool,
}

/// `PrinterDescription` facts and speed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Description {
    /// `monochrome` and/or `color`.
    #[serde(default = "default_color_modes")]
    pub color_modes: Vec<String>,
    #[serde(default = "default_ppm")]
    pub pages_per_minute: u32,
    /// Colour speed. Absent (or null): none for a monochrome-only printer, and
    /// 12 pages for one that prints in colour (filled in on load).
    #[serde(default)]
    pub pages_per_minute_color: Option<u32>,
    #[serde(default = "default_true")]
    pub multiple_document_jobs: bool,
    /// `PrinterEventRate` seconds (1..600).
    #[serde(default = "default_event_rate")]
    pub event_rate: u32,
}

fn default_color_modes() -> Vec<String> {
    vec!["monochrome".into(), "color".into()]
}
fn default_ppm_color_opt() -> Option<u32> {
    Some(default_ppm_color())
}
fn default_event_rate() -> u32 {
    30
}

impl Default for Description {
    fn default() -> Self {
        Description {
            color_modes: default_color_modes(),
            pages_per_minute: default_ppm(),
            pages_per_minute_color: default_ppm_color_opt(),
            multiple_document_jobs: true,
            event_rate: default_event_rate(),
        }
    }
}

/// What the printer accepts as documents.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Documents {
    /// MIME strings. The schema's well-known tokens are `application/octet-stream`,
    /// `application/postscript`, `application/vnd.hp-PCL`, `application/vnd.ms-xpsdocument`,
    /// `application/vnd.pwg-xhtml-print+xml`, `image/g3fax|gif|jpeg|png|tiff|tiff-fx`,
    /// `text/html|plain`, `unknown`; any other MIME-looking string (`application/pdf`,
    /// `image/pwg-raster`, `image/urf`, ...) is a legal extension.
    #[serde(default = "default_formats")]
    pub formats: Vec<String>,
    /// `None`, `Gzip`, `Compress`, `Deflate` or `ns:keyword`.
    #[serde(default = "default_compressions")]
    pub compressions: Vec<String>,
    /// Offer the optional `AddDocument` operation (the printer fetches the document by URL).
    #[serde(default, skip_serializing_if = "is_false")]
    pub add_document: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

fn default_compressions() -> Vec<String> {
    vec!["None".into()]
}

impl Default for Documents {
    fn default() -> Self {
        Documents {
            formats: default_formats(),
            compressions: default_compressions(),
            add_document: false,
        }
    }
}

/// An inclusive integer range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Range {
    pub min: i32,
    pub max: i32,
}

/// A print resolution in dots per inch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Resolution {
    pub width: i32,
    /// Absent: square.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<i32>,
}

impl Resolution {
    pub fn square(dpi: i32) -> Self {
        Resolution {
            width: dpi,
            height: Some(dpi),
        }
    }

    /// Height, defaulting to the width.
    pub fn height(&self) -> i32 {
        self.height.unwrap_or(self.width)
    }
}

/// The values a ticket may use (the `PrinterCapabilities` lists).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrintOptions {
    #[serde(default = "default_media_sizes")]
    pub media_sizes: Vec<String>,
    #[serde(default = "default_media_types")]
    pub media_types: Vec<String>,
    /// Empty: not advertised.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub media_colors: Vec<String>,
    #[serde(default = "default_orientations")]
    pub orientations: Vec<String>,
    #[serde(default = "default_qualities")]
    pub qualities: Vec<String>,
    /// `OneSided`, `TwoSidedLongEdge`, `TwoSidedShortEdge`.
    #[serde(default = "default_sides")]
    pub sides: Vec<String>,
    #[serde(default = "default_resolutions")]
    pub resolutions: Vec<Resolution>,
    #[serde(default = "default_pages_per_sheet")]
    pub pages_per_sheet: Vec<i32>,
    #[serde(default = "default_nup_directions")]
    pub number_up_directions: Vec<String>,
    #[serde(default = "default_copies")]
    pub copies: Range,
    #[serde(default = "default_priority")]
    pub priority: Range,
}

fn default_media_sizes() -> Vec<String> {
    vec![
        "na_letter_8.5x11in".into(),
        "na_legal_8.5x14in".into(),
        "iso_a4_210x297mm".into(),
    ]
}
fn default_media_types() -> Vec<String> {
    vec!["stationery".into(), "photographic".into(), "other".into()]
}
fn default_orientations() -> Vec<String> {
    vec!["Portrait".into(), "Landscape".into()]
}
fn default_qualities() -> Vec<String> {
    vec!["Draft".into(), "Normal".into(), "High".into()]
}
fn default_sides() -> Vec<String> {
    vec!["OneSided".into()]
}
fn default_resolutions() -> Vec<Resolution> {
    vec![Resolution::square(300), Resolution::square(600)]
}
fn default_pages_per_sheet() -> Vec<i32> {
    vec![1, 2, 4]
}
fn default_nup_directions() -> Vec<String> {
    vec!["RightDown".into()]
}
fn default_copies() -> Range {
    Range { min: 1, max: 99 }
}
fn default_priority() -> Range {
    Range { min: 1, max: 100 }
}

impl Default for PrintOptions {
    fn default() -> Self {
        PrintOptions {
            media_sizes: default_media_sizes(),
            media_types: default_media_types(),
            media_colors: Vec::new(),
            orientations: default_orientations(),
            qualities: default_qualities(),
            sides: default_sides(),
            resolutions: default_resolutions(),
            pages_per_sheet: default_pages_per_sheet(),
            number_up_directions: default_nup_directions(),
            copies: default_copies(),
            priority: default_priority(),
        }
    }
}

/// `DefaultPrintTicket`. Unset fields take the preferred default when the lists allow it,
/// else the first listed value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DefaultTicket {
    #[serde(default = "default_job_name")]
    pub job_name: String,
    #[serde(default = "default_user_name")]
    pub user_name: String,
    #[serde(default = "default_one")]
    pub copies: i32,
    #[serde(default = "default_prio")]
    pub priority: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_size: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_color: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub orientation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution: Option<Resolution>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quality: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sides: Option<String>,
    /// Present: a `NumberUp` element is published with this many pages and the first direction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pages_per_sheet: Option<i32>,
}

fn default_job_name() -> String {
    "DefaultJob".into()
}
fn default_user_name() -> String {
    "DefaultUser".into()
}
fn default_one() -> i32 {
    1
}
fn default_prio() -> i32 {
    50
}

impl Default for DefaultTicket {
    fn default() -> Self {
        DefaultTicket {
            job_name: default_job_name(),
            user_name: default_user_name(),
            copies: 1,
            priority: 50,
            media_size: None,
            media_type: None,
            media_color: None,
            orientation: None,
            resolution: None,
            quality: None,
            sides: None,
            pages_per_sheet: None,
        }
    }
}

/// A paper tray and what is loaded in it (the "ready media").
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputBin {
    /// A well-known bin (`Top`, `Bypass`, `Manual`, ...) or `Tray<n>`.
    pub name: String,
    /// One of the seven well-known size names (no custom sizes in a bin).
    #[serde(default = "default_letter")]
    pub media_size: String,
    #[serde(default = "default_stationery")]
    pub media_type: Option<String>,
    #[serde(default = "default_white")]
    pub media_color: Option<String>,
    /// `LongEdgeFirst` or `ShortEdgeFirst`.
    #[serde(default = "default_feed")]
    pub feed_direction: String,
    /// Sheets the bin holds; -1 unknown.
    #[serde(default = "default_capacity")]
    pub capacity: i32,
    /// Percent full 0..100; -1 unknown.
    #[serde(default = "default_level")]
    pub level: i32,
}

fn default_letter() -> String {
    "na_letter_8.5x11in".into()
}
fn default_stationery() -> Option<String> {
    Some("stationery".into())
}
fn default_white() -> Option<String> {
    Some("white".into())
}
fn default_feed() -> String {
    "LongEdgeFirst".into()
}
fn default_capacity() -> i32 {
    250
}
fn default_level() -> i32 {
    80
}

impl InputBin {
    fn standard() -> Self {
        InputBin {
            name: "Tray1".into(),
            media_size: default_letter(),
            media_type: default_stationery(),
            media_color: default_white(),
            feed_direction: default_feed(),
            capacity: default_capacity(),
            level: default_level(),
        }
    }
}

/// An output tray.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputBin {
    /// A well-known bin (`Face-down`, `Top`, ...) or `Bin<n>`, `Stacker<n>`, `Mailbox<n>`.
    pub name: String,
    #[serde(default = "default_out_capacity")]
    pub capacity: i32,
    #[serde(default)]
    pub level: i32,
}

fn default_out_capacity() -> i32 {
    150
}

impl OutputBin {
    fn standard() -> Self {
        OutputBin {
            name: "Bin1".into(),
            capacity: default_out_capacity(),
            level: 0,
        }
    }
}

/// Internal storage.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StorageEntry {
    pub name: String,
    /// `RAM`, `HardDisk`, `NonRemovableFlash`, ...
    #[serde(default = "default_storage_kind")]
    pub kind: String,
    /// Size in megabytes.
    pub size_mb: i32,
    /// Free percent 0..100; -1 unknown.
    #[serde(default = "default_free")]
    pub free: i32,
}

fn default_storage_kind() -> String {
    "RAM".into()
}
fn default_free() -> i32 {
    100
}

/// Which finishing devices exist.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Finishings {
    #[serde(default)]
    pub collation: bool,
    #[serde(default)]
    pub jog_offset: bool,
    /// Absent: true exactly when `print.sides` lists a two-sided value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duplexer: Option<bool>,
    #[serde(default)]
    pub stapler: bool,
    #[serde(default)]
    pub hole_punch: bool,
}

/// A toner or ink supply.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Consumable {
    pub name: String,
    /// `Toner`, `Ink`, `WasteToner`, `Opc`, ... (see the schema's list).
    #[serde(default = "default_consumable_kind")]
    pub kind: String,
    /// `Black`, `Cyan`, `Magenta`, `Yellow`, `Color`, ...
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// Percent remaining 0..100; -1 unknown.
    pub level: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Six hex digits (RGB); published with version 1.1 only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rgb: Option<String>,
}

fn default_consumable_kind() -> String {
    "Toner".into()
}

/// A condition active at start (paper out, toner low, cover open, ...).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConditionSpec {
    /// A condition name token: `InputTraySupplyEmpty`, `ConsumableLow`, `CoverOpen`, `Jam`, ...
    pub name: String,
    /// `Informational`, `Warning` or `Critical`; absent: the usual severity of the condition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub severity: Option<String>,
    /// Component group (`InputBin`, `Consumable`, ...); absent: derived from the name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// Component name (`Tray1`, `Black`, ...); absent: the first matching bin or consumable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub component: Option<String>,
}

/// Settings of the protocol extensions. A block is allowed only with its version switched on.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Extensions {
    /// v1.1: GUID of the print device model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    /// v1.1: driver configuration files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub driver_config: Option<DriverConfig>,
    /// v2.0: formats and change id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub v20: Option<V20Settings>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DriverConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpd_config_file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ppd_config_file: Option<String>,
}

/// v2.0 settings. The package does not name the format strings; these are lab values
/// (unverified against Windows) the profile may override.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct V20Settings {
    #[serde(default = "default_caps_formats")]
    pub capabilities_formats: Vec<String>,
    #[serde(default = "default_ticket_formats")]
    pub ticket_formats: Vec<String>,
    #[serde(default = "default_resource_formats")]
    pub resource_formats: Vec<String>,
    /// Bump to tell clients the cached device capabilities are stale.
    #[serde(default = "default_change_id")]
    pub capabilities_change_id: String,
}

fn default_caps_formats() -> Vec<String> {
    vec!["application/vnd.ms-PrintDeviceCapabilities+xml".into()]
}
fn default_ticket_formats() -> Vec<String> {
    vec!["application/vnd.ms-PrintSchemaTicket+xml".into()]
}
fn default_resource_formats() -> Vec<String> {
    vec!["application/vnd.ms-PrintDeviceResources+xml".into()]
}
fn default_change_id() -> String {
    "1".into()
}

impl Default for V20Settings {
    fn default() -> Self {
        V20Settings {
            capabilities_formats: default_caps_formats(),
            ticket_formats: default_ticket_formats(),
            resource_formats: default_resource_formats(),
            capabilities_change_id: default_change_id(),
        }
    }
}

/// Vendor behaviour outside the schema. Everything defaults to off.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileQuirks {
    /// `JobToken` after `JobId` in CreatePrintJob replies.
    #[serde(default)]
    pub job_token: bool,
    /// `PrinterLocalTime` next to `PrinterCurrentTime`.
    #[serde(default)]
    pub printer_local_time: bool,
    /// Accept tickets without the required job description parts.
    #[serde(default)]
    pub lenient_job_description: bool,
    /// Accept SendDocument without a DocumentDescription.
    #[serde(default)]
    pub lenient_document_description: bool,
}

// ---------------------------------------------------------------------------------------------
// Loading and validation
// ---------------------------------------------------------------------------------------------

fn safe_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && !id.starts_with('.')
}

impl PrinterProfile {
    /// The stock lab printer, the same as `profiles/generic-wsd.json` (for servers that have
    /// no profile directory).
    pub fn generic() -> Self {
        let mut p = PrinterProfile::new("generic-wsd", "Fake WSD printer");
        p.documents.formats = vec![
            "application/octet-stream".into(),
            "application/pdf".into(),
            "image/pwg-raster".into(),
            "application/postscript".into(),
            "application/vnd.hp-PCL".into(),
        ];
        p
    }

    /// A profile with every default (the stable "generic" printer) under this id.
    pub fn new(id: &str, label: &str) -> Self {
        let mut p = PrinterProfile {
            schema_version: SCHEMA_VERSION,
            id: id.to_string(),
            label: label.to_string(),
            identity: Identity::default(),
            versions: Versions::default(),
            description: Description::default(),
            documents: Documents::default(),
            print: PrintOptions::default(),
            default_ticket: DefaultTicket::default(),
            inputs: None,
            outputs: None,
            storage: Vec::new(),
            finishings: None,
            consumables: None,
            conditions: Vec::new(),
            extensions: Extensions::default(),
            quirks: ProfileQuirks::default(),
        };
        p.fill_defaults();
        p
    }

    /// Parse and validate JSON text of either layout.
    pub fn from_json(raw: &str) -> Result<Self> {
        let value: serde_json::Value = serde_json::from_str(raw).context("not valid JSON")?;
        let version = match value.get("schema_version") {
            None => 1,
            Some(v) => v
                .as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .context("schema_version must be a small positive integer")?,
        };
        let mut p = match version {
            1 => {
                let flat: FlatFile = serde_json::from_value(value).map_err(|e| {
                    anyhow::anyhow!(
                        "{e} (schema 1 profile; add \"schema_version\": 2 to use the full format)"
                    )
                })?;
                flat.upgrade()
            }
            SCHEMA_VERSION => serde_json::from_value::<PrinterProfile>(value)
                .map_err(|e| anyhow::anyhow!("{e}"))?,
            other => bail!(
                "schema_version {other} is not supported (this build reads 1 and {SCHEMA_VERSION})"
            ),
        };
        p.fill_defaults();
        let problems = p.problems();
        if !problems.is_empty() {
            bail!(
                "{} problem(s):\n  - {}",
                problems.len(),
                problems.join("\n  - ")
            );
        }
        Ok(p)
    }

    /// Load `<dir>/<id>.json` (either layout); the file's id must equal `id`.
    pub fn load(dir: &Path, id: &str) -> Result<Self> {
        if !safe_id(id) {
            bail!("profile id {id:?} may only contain letters, digits, '-', '_' and '.'");
        }
        let path = dir.join(format!("{id}.json"));
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("read profile {}", path.display()))?;
        let p = Self::from_json(&raw).with_context(|| format!("profile {}", path.display()))?;
        if p.id != id {
            bail!("profile id mismatch: file says {:?}, expected {id}", p.id);
        }
        Ok(p)
    }

    /// Ids of the `*.json` files in `dir`, sorted. A missing directory is an empty list.
    pub fn list_ids(dir: &Path) -> Result<Vec<String>> {
        let mut ids = Vec::new();
        if !dir.is_dir() {
            return Ok(ids);
        }
        for entry in fs::read_dir(dir)? {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) == Some("json") {
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    ids.push(stem.to_string());
                }
            }
        }
        ids.sort();
        Ok(ids)
    }

    /// Serialize to schema 2 JSON.
    pub fn to_json(&self) -> Result<String> {
        serde_json::to_string_pretty(self).context("serialize profile")
    }

    // ---- derived facts ------------------------------------------------------------------

    pub fn color_supported(&self) -> bool {
        self.description.color_modes.iter().any(|m| m == "color")
    }

    /// `PrinterName`.
    pub fn friendly_name(&self) -> &str {
        self.identity
            .friendly_name
            .as_deref()
            .unwrap_or(&self.label)
    }

    /// Whether any two-sided value is advertised.
    pub fn duplex(&self) -> bool {
        self.print.sides.iter().any(|s| s != "OneSided")
    }

    pub fn inputs(&self) -> &[InputBin] {
        self.inputs.as_deref().unwrap_or(&[])
    }

    pub fn outputs(&self) -> &[OutputBin] {
        self.outputs.as_deref().unwrap_or(&[])
    }

    pub fn consumables(&self) -> &[Consumable] {
        self.consumables.as_deref().unwrap_or(&[])
    }

    /// The IEEE 1284 device id: the explicit one, else derived.
    pub fn device_id(&self) -> String {
        if let Some(d) = &self.identity.device_id {
            return d.clone();
        }
        let clean = |s: &str| s.replace([';', ':', ','], " ");
        let mut cmds: Vec<&str> = Vec::new();
        for f in &self.documents.formats {
            let c = match f.as_str() {
                "application/vnd.hp-PCL" => "PCL",
                "application/postscript" => "POSTSCRIPT",
                "application/pdf" => "PDF",
                "image/pwg-raster" => "PWGRaster",
                "image/urf" => "URF",
                "application/vnd.ms-xpsdocument" => "XPS",
                "image/jpeg" => "JPEG",
                _ => continue,
            };
            if !cmds.contains(&c) {
                cmds.push(c);
            }
        }
        format!(
            "MFG:{};CMD:{};MDL:{};CLS:PRINTER;DES:{};CID:MS_IPP;",
            clean(&self.identity.manufacturer),
            if cmds.is_empty() {
                "PJL".to_string()
            } else {
                cmds.join(",")
            },
            clean(&self.identity.model),
            clean(&self.label),
        )
    }

    // ---- defaults and checks ------------------------------------------------------------

    /// Resolve every "absent means derived" field so readers see concrete values.
    fn fill_defaults(&mut self) {
        if self.description.pages_per_minute_color.is_none() && self.color_supported() {
            self.description.pages_per_minute_color = Some(default_ppm_color());
        }
        if self.inputs.is_none() {
            self.inputs = Some(vec![InputBin::standard()]);
        }
        if self.outputs.is_none() {
            self.outputs = Some(vec![OutputBin::standard()]);
        }
        let pick = |preferred: &str, list: &[String]| -> Option<String> {
            if list.iter().any(|v| v == preferred) {
                Some(preferred.to_string())
            } else {
                list.first().cloned()
            }
        };
        let pr = &self.print;
        let dt = &mut self.default_ticket;
        if dt.media_size.is_none() {
            dt.media_size = pick("na_letter_8.5x11in", &pr.media_sizes);
        }
        if dt.media_type.is_none() {
            dt.media_type = pick("stationery", &pr.media_types);
        }
        if dt.orientation.is_none() {
            dt.orientation = pick("Portrait", &pr.orientations);
        }
        if dt.quality.is_none() {
            dt.quality = pick("Normal", &pr.qualities);
        }
        if dt.sides.is_none() {
            dt.sides = pick("OneSided", &pr.sides);
        }
        if dt.resolution.is_none() {
            let want = Resolution::square(600);
            dt.resolution = if pr.resolutions.contains(&want) {
                Some(want)
            } else {
                pr.resolutions.first().copied()
            };
        }
        // a default outside the advertised range is clamped, not an error
        if pr.copies.min <= pr.copies.max {
            dt.copies = dt.copies.clamp(pr.copies.min, pr.copies.max);
        }
        if pr.priority.min <= pr.priority.max {
            dt.priority = dt.priority.clamp(pr.priority.min, pr.priority.max);
        }
    }

    /// Every problem with the profile, each prefixed with the path of the field.
    pub fn problems(&self) -> Vec<String> {
        let mut e: Vec<String> = Vec::new();
        macro_rules! add {
            ($path:expr, $msg:expr) => {
                e.push(format!("{}: {}", $path, $msg))
            };
        }

        if self.schema_version != SCHEMA_VERSION {
            add!("schema_version", format!("must be {SCHEMA_VERSION}"));
        }
        if !safe_id(&self.id) {
            add!("id", "letters, digits, '-', '_' and '.' only");
        }
        if self.label.trim().is_empty() || self.label.chars().count() > 127 {
            add!("label", "1 to 127 characters");
        }

        // identity
        let id = &self.identity;
        for (name, v, max) in [
            ("identity.manufacturer", &id.manufacturer, 127usize),
            ("identity.model", &id.model, 127),
            ("identity.firmware_version", &id.firmware_version, 63),
            ("identity.location", &id.location, 127),
            ("identity.info", &id.info, 127),
        ] {
            if v.trim().is_empty() || v.chars().count() > max {
                add!(name, format!("1 to {max} characters"));
            }
        }
        if let Some(n) = &id.friendly_name {
            if n.trim().is_empty() || n.chars().count() > 127 {
                add!("identity.friendly_name", "1 to 127 characters");
            }
        }
        if let Some(sn) = &id.serial_number {
            if sn.trim().is_empty() || sn.chars().count() > 63 {
                add!("identity.serial_number", "1 to 63 characters");
            }
        }
        if id.language.is_empty()
            || !id
                .language
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-')
        {
            add!("identity.language", "a language tag such as en-US");
        }
        if let Some(u) = &id.uuid {
            if uuid::Uuid::parse_str(u).is_err() {
                add!("identity.uuid", format!("{u:?} is not a UUID"));
            }
        }
        if let Some(d) = &id.device_id {
            if d.is_empty() || d.chars().count() > 1023 {
                add!("identity.device_id", "1 to 1023 characters");
            } else if !d.contains("MFG:") && !d.contains("MANUFACTURER:") {
                add!("identity.device_id", "an IEEE 1284 id contains MFG:...;");
            }
        }

        // description
        let d = &self.description;
        if d.color_modes.is_empty() {
            add!(
                "description.color_modes",
                "at least one of monochrome, color"
            );
        }
        for (i, m) in d.color_modes.iter().enumerate() {
            if m != "monochrome" && m != "color" {
                add!(
                    format!("description.color_modes[{i}]"),
                    format!("{m:?} is not monochrome or color")
                );
            }
        }
        if d.pages_per_minute == 0 || d.pages_per_minute > 100_000 {
            add!("description.pages_per_minute", "1 or more");
        }
        match (d.pages_per_minute_color, self.color_supported()) {
            (Some(0), _) => add!(
                "description.pages_per_minute_color",
                "1 or more (omit it for none)"
            ),
            (Some(_), false) => add!(
                "description.pages_per_minute_color",
                "a monochrome-only printer has no colour speed"
            ),
            _ => {}
        }
        if !(1..=600).contains(&d.event_rate) {
            add!("description.event_rate", "1 to 600 seconds");
        }

        // documents
        if self.documents.formats.is_empty() {
            add!("documents.formats", "at least one format");
        }
        list_check(
            &mut e,
            "documents.formats",
            &self.documents.formats,
            "DocumentFormatExtType",
        );
        if self.documents.compressions.is_empty() {
            add!(
                "documents.compressions",
                "at least one compression (normally None)"
            );
        }
        list_check(
            &mut e,
            "documents.compressions",
            &self.documents.compressions,
            "CompressionExtType",
        );

        // print options
        let pr = &self.print;
        list_check_nonempty(
            &mut e,
            "print.media_sizes",
            &pr.media_sizes,
            "MediaSizeNameExtType",
        );
        list_check_nonempty(
            &mut e,
            "print.media_types",
            &pr.media_types,
            "MediaTypeExtType",
        );
        list_check(
            &mut e,
            "print.media_colors",
            &pr.media_colors,
            "MediaColorExtType",
        );
        list_check_nonempty(
            &mut e,
            "print.orientations",
            &pr.orientations,
            "OrientationBaseType",
        );
        list_check_nonempty(
            &mut e,
            "print.qualities",
            &pr.qualities,
            "PrintQualityBaseType",
        );
        list_check_nonempty(&mut e, "print.sides", &pr.sides, "SidesBaseType");
        list_check_nonempty(
            &mut e,
            "print.number_up_directions",
            &pr.number_up_directions,
            "NUpDirectionRestrictionType",
        );
        if pr.resolutions.is_empty() {
            add!("print.resolutions", "at least one resolution");
        }
        for (i, r) in pr.resolutions.iter().enumerate() {
            if r.width < 1 || r.height.is_some_and(|h| h < 1) {
                add!(
                    format!("print.resolutions[{i}]"),
                    "dots per inch must be 1 or more"
                );
            }
        }
        if pr.pages_per_sheet.is_empty() {
            add!(
                "print.pages_per_sheet",
                "at least one value (1 means no n-up)"
            );
        }
        for (i, n) in pr.pages_per_sheet.iter().enumerate() {
            if *n < 1 {
                add!(format!("print.pages_per_sheet[{i}]"), "1 or more");
            }
        }
        if pr.copies.min < 1 || pr.copies.max < pr.copies.min {
            add!("print.copies", "1 <= min <= max");
        }
        if pr.priority.min < 1 || pr.priority.max > 100 || pr.priority.max < pr.priority.min {
            add!("print.priority", "1 <= min <= max <= 100");
        }

        // default ticket against the lists
        let dt = &self.default_ticket;
        if dt.job_name.chars().count() > 255 {
            add!("default_ticket.job_name", "at most 255 characters");
        }
        if dt.user_name.chars().count() > 255 {
            add!("default_ticket.user_name", "at most 255 characters");
        }
        for (path, v, list, what) in [
            (
                "default_ticket.media_size",
                &dt.media_size,
                &pr.media_sizes,
                "print.media_sizes",
            ),
            (
                "default_ticket.media_type",
                &dt.media_type,
                &pr.media_types,
                "print.media_types",
            ),
            (
                "default_ticket.media_color",
                &dt.media_color,
                &pr.media_colors,
                "print.media_colors",
            ),
            (
                "default_ticket.orientation",
                &dt.orientation,
                &pr.orientations,
                "print.orientations",
            ),
            (
                "default_ticket.quality",
                &dt.quality,
                &pr.qualities,
                "print.qualities",
            ),
            ("default_ticket.sides", &dt.sides, &pr.sides, "print.sides"),
        ] {
            if let Some(v) = v {
                if !list.contains(v) {
                    add!(path, format!("{v:?} is not in {what}"));
                }
            }
        }
        if let Some(r) = &dt.resolution {
            if !pr.resolutions.contains(r) {
                add!("default_ticket.resolution", "is not in print.resolutions");
            }
        }
        if let Some(n) = dt.pages_per_sheet {
            if !pr.pages_per_sheet.contains(&n) {
                add!(
                    "default_ticket.pages_per_sheet",
                    "is not in print.pages_per_sheet"
                );
            }
        }
        if dt.copies < pr.copies.min || dt.copies > pr.copies.max {
            add!("default_ticket.copies", "outside print.copies");
        }
        if dt.priority < pr.priority.min || dt.priority > pr.priority.max {
            add!("default_ticket.priority", "outside print.priority");
        }

        // bins
        let mut names: Vec<&str> = Vec::new();
        for (i, b) in self.inputs().iter().enumerate() {
            let p = format!("inputs[{i}]");
            if let Err(m) = schema::check_value("InputBinNameType", &b.name) {
                add!(format!("{p}.name"), m);
            }
            if names.contains(&b.name.as_str()) {
                add!(format!("{p}.name"), format!("{:?} used twice", b.name));
            }
            names.push(&b.name);
            if let Err(m) = schema::check_value("InputBinMediaSizeBaseType", &b.media_size) {
                add!(
                    format!("{p}.media_size"),
                    format!("{m} (bins hold one of the well-known sizes)")
                );
            } else if !pr.media_sizes.contains(&b.media_size) {
                add!(
                    format!("{p}.media_size"),
                    format!("{:?} is not in print.media_sizes", b.media_size)
                );
            }
            if let Some(t) = &b.media_type {
                if let Err(m) = schema::check_value("InputBinMediaTypeExtType", t) {
                    add!(format!("{p}.media_type"), m);
                } else if !pr.media_types.contains(t) {
                    add!(
                        format!("{p}.media_type"),
                        format!("{t:?} is not in print.media_types")
                    );
                }
            }
            if let Some(c) = &b.media_color {
                if let Err(m) = schema::check_value("InputBinMediaColorBaseType", c) {
                    add!(format!("{p}.media_color"), m);
                }
            }
            if let Err(m) = schema::check_value("InputBinFeedDirectionBaseType", &b.feed_direction)
            {
                add!(format!("{p}.feed_direction"), m);
            }
            if b.capacity < -1 {
                add!(format!("{p}.capacity"), "-1 (unknown) or more");
            }
            if !(-1..=100).contains(&b.level) {
                add!(format!("{p}.level"), "-1 (unknown) to 100");
            }
        }
        let mut onames: Vec<&str> = Vec::new();
        for (i, b) in self.outputs().iter().enumerate() {
            let p = format!("outputs[{i}]");
            if let Err(m) = schema::check_value("OutputBinNameType", &b.name) {
                add!(format!("{p}.name"), m);
            }
            if onames.contains(&b.name.as_str()) {
                add!(format!("{p}.name"), format!("{:?} used twice", b.name));
            }
            onames.push(&b.name);
            if b.capacity < -1 {
                add!(format!("{p}.capacity"), "-1 (unknown) or more");
            }
            if !(-1..=100).contains(&b.level) {
                add!(format!("{p}.level"), "-1 (unknown) to 100");
            }
        }
        for (i, s) in self.storage.iter().enumerate() {
            let p = format!("storage[{i}]");
            if s.name.is_empty() || s.name.chars().count() > 255 {
                add!(format!("{p}.name"), "1 to 255 characters");
            }
            if let Err(m) = schema::check_value("StorageTypeBaseType", &s.kind) {
                add!(format!("{p}.kind"), m);
            }
            if s.size_mb < 1 {
                add!(format!("{p}.size_mb"), "1 or more");
            }
            if !(-1..=100).contains(&s.free) {
                add!(format!("{p}.free"), "-1 (unknown) to 100");
            }
        }
        if let Some(f) = &self.finishings {
            if f.duplexer == Some(false) && self.duplex() {
                add!(
                    "finishings.duplexer",
                    "false, but print.sides lists a two-sided value"
                );
            }
            if f.duplexer == Some(true) && !self.duplex() {
                add!(
                    "finishings.duplexer",
                    "true, but print.sides has no two-sided value"
                );
            }
        }

        // consumables
        let mut cnames: Vec<&str> = Vec::new();
        for (i, c) in self.consumables().iter().enumerate() {
            let p = format!("consumables[{i}]");
            if c.name.is_empty() || c.name.chars().count() > 255 {
                add!(format!("{p}.name"), "1 to 255 characters");
            }
            if cnames.contains(&c.name.as_str()) {
                add!(format!("{p}.name"), format!("{:?} used twice", c.name));
            }
            cnames.push(&c.name);
            if let Err(m) = schema::check_value("ConsumableTypeBaseType", &c.kind) {
                add!(format!("{p}.kind"), m);
            }
            if let Some(col) = &c.color {
                if let Err(m) = schema::check_value("ConsumableColorType", col) {
                    add!(format!("{p}.color"), m);
                }
            }
            if !(-1..=100).contains(&c.level) {
                add!(format!("{p}.level"), "-1 (unknown) to 100");
            }
            if c.model.as_ref().is_some_and(|m| m.chars().count() > 255) {
                add!(format!("{p}.model"), "at most 255 characters");
            }
            if let Some(rgb) = &c.rgb {
                if rgb.len() != 6 || !rgb.bytes().all(|b| b.is_ascii_hexdigit()) {
                    add!(format!("{p}.rgb"), "six hex digits");
                }
                if !self.versions.v11 {
                    add!(format!("{p}.rgb"), "needs versions.v11 = true");
                }
            }
        }

        // preloaded conditions
        for (i, c) in self.conditions.iter().enumerate() {
            let p = format!("conditions[{i}]");
            if let Err(m) = schema::check_value("ConditionNameType", &c.name) {
                add!(format!("{p}.name"), m);
            }
            if let Some(s) = &c.severity {
                if let Err(m) = schema::check_value("ConditionSeverityType", s) {
                    add!(format!("{p}.severity"), m);
                }
            }
            if let Some(g) = &c.group {
                if let Err(m) = schema::check_value("ComponentGroupType", g) {
                    add!(format!("{p}.group"), m);
                }
            }
            if let Some(n) = &c.component {
                if let Err(m) = schema::check_value("ComponentNameBaseType", n) {
                    add!(format!("{p}.component"), m);
                }
            }
        }

        // extensions
        let x = &self.extensions;
        if (x.model_id.is_some() || x.driver_config.is_some()) && !self.versions.v11 {
            add!(
                "extensions",
                "model_id / driver_config need versions.v11 = true"
            );
        }
        if x.v20.is_some() && !self.versions.v20 {
            add!("extensions.v20", "needs versions.v20 = true");
        }
        if let Some(m) = &x.model_id {
            if schema::check_value("DeviceModelIdGuidType", m).is_err()
                || uuid::Uuid::parse_str(m).is_err()
            {
                add!(
                    "extensions.model_id",
                    "a GUID such as 00000000-0000-0000-0000-000000000000"
                );
            }
        }
        if let Some(v) = &x.v20 {
            for (n, l) in [
                ("capabilities_formats", &v.capabilities_formats),
                ("ticket_formats", &v.ticket_formats),
                ("resource_formats", &v.resource_formats),
            ] {
                if l.is_empty() {
                    add!(format!("extensions.v20.{n}"), "at least one format");
                }
                for (i, f) in l.iter().enumerate() {
                    if schema::check_value("Tokens", f).is_err() || f.contains(char::is_whitespace)
                    {
                        add!(
                            format!("extensions.v20.{n}[{i}]"),
                            format!("{f:?} is not a token")
                        );
                    }
                }
            }
            if v.capabilities_change_id.is_empty() {
                add!("extensions.v20.capabilities_change_id", "must not be empty");
            }
        }
        e
    }
}

fn list_check(e: &mut Vec<String>, path: &str, list: &[String], ty: &str) {
    let mut seen: Vec<&str> = Vec::new();
    for (i, v) in list.iter().enumerate() {
        if let Err(m) = schema::check_value(ty, v) {
            e.push(format!("{path}[{i}]: {m}"));
        } else if ty == "DocumentFormatExtType" && !v.contains('/') && v != "unknown" {
            e.push(format!(
                "{path}[{i}]: {v:?} is not a MIME type such as application/pdf"
            ));
        } else if seen.contains(&v.as_str()) {
            e.push(format!("{path}[{i}]: {v:?} listed twice"));
        }
        seen.push(v);
    }
}

fn list_check_nonempty(e: &mut Vec<String>, path: &str, list: &[String], ty: &str) {
    if list.is_empty() {
        e.push(format!("{path}: at least one value"));
    }
    list_check(e, path, list, ty);
}

/// Load the profile called `name` from `dir` (either layout), validated. This is the entry
/// the lab and the Python binding use (`Server(profile=..., profiles=...)`).
pub fn load_by_name(dir: &Path, name: &str) -> Result<PrinterProfile> {
    PrinterProfile::load(dir, name)
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn profiles_dir() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../profiles")
    }

    fn v2(extra: &str) -> String {
        format!(r#"{{"schema_version":2,"id":"t","label":"Test"{extra}}}"#)
    }

    fn err_of(raw: &str) -> String {
        match PrinterProfile::from_json(raw) {
            Ok(_) => String::new(),
            Err(e) => format!("{e:#}"),
        }
    }

    /// The smallest document there is (also what docs/profiles.md shows).
    const SMALLEST: &str = r#"{"schema_version":2,"id":"smallest","label":"Smallest"}"#;
    const SMALLEST_MONO: &str = r#"{"schema_version":2,"id":"mono","label":"Mono","description":{"color_modes":["monochrome"],"pages_per_minute":30}}"#;

    #[test]
    fn the_smallest_documents_load() {
        let p = PrinterProfile::from_json(SMALLEST).unwrap();
        assert!(p.color_supported());
        assert_eq!(p.description.pages_per_minute_color, Some(12));
        let m = PrinterProfile::from_json(SMALLEST_MONO).unwrap();
        assert!(!m.color_supported());
        assert_eq!(m.description.pages_per_minute, 30);
        assert_eq!(m.description.pages_per_minute_color, None);
        // an explicit colour speed on a monochrome printer is still an error
        let bad = SMALLEST_MONO.replace(
            "\"pages_per_minute\":30",
            "\"pages_per_minute\":30,\"pages_per_minute_color\":9",
        );
        assert!(err_of(&bad).contains("no colour speed"));
        // a colour printer that gives no colour speed gets the default, one that gives it keeps it
        let c = PrinterProfile::from_json(&v2(
            r#","description":{"color_modes":["color"],"pages_per_minute":8}"#,
        ))
        .unwrap();
        assert_eq!(c.description.pages_per_minute_color, Some(12));
        let c = PrinterProfile::from_json(&v2(
            r#","description":{"color_modes":["monochrome","color"],"pages_per_minute_color":null}"#,
        ))
        .unwrap();
        assert_eq!(c.description.pages_per_minute_color, Some(12));
    }

    #[test]
    fn every_smallest_combination_loads_and_publishes_valid_sections() {
        use crate::wsd::elements::schema::{validate_root, Options};
        use crate::wsd::printer_model::ProfilePrinterModel;
        use crate::wsd::types::PrinterModel;

        let colors = [
            r#"["monochrome"]"#,
            r#"["color"]"#,
            r#"["monochrome","color"]"#,
            r#"["color","monochrome"]"#,
        ];
        let sides = [
            None,
            Some(r#"["OneSided","TwoSidedLongEdge"]"#),
            Some(r#"["OneSided","TwoSidedLongEdge","TwoSidedShortEdge"]"#),
            Some(r#"["TwoSidedLongEdge"]"#),
        ];
        let mut n = 0;
        for c in colors {
            for sd in sides {
                for bits in 0..8u8 {
                    let (v11, v12, v20) = (bits & 1 != 0, bits & 2 != 0, bits & 4 != 0);
                    let mut doc = format!(
                        r#"{{"schema_version":2,"id":"t","label":"T","description":{{"color_modes":{c},"pages_per_minute":10}},"versions":{{"v11":{v11},"v12":{v12},"v20":{v20}}}"#
                    );
                    if let Some(sd) = sd {
                        doc.push_str(&format!(r#","print":{{"sides":{sd}}}"#));
                    }
                    doc.push('}');
                    let p = PrinterProfile::from_json(&doc).unwrap_or_else(|e| {
                        panic!(
                            "{doc}
{e:#}"
                        )
                    });
                    assert_eq!(
                        p.description.pages_per_minute_color.is_some(),
                        p.color_supported(),
                        "{doc}"
                    );
                    let m = ProfilePrinterModel::new(p);
                    for q in m.sections() {
                        let e = m
                            .element(&q)
                            .unwrap_or_else(|| panic!("{doc}: {q} advertised, not answered"));
                        let v = validate_root(&e, &Options::STRICT);
                        assert!(v.is_empty(), "{doc}: {q}: {v:?}");
                    }
                    n += 1;
                }
            }
        }
        assert_eq!(n, 4 * 4 * 8);
    }

    #[test]
    fn minimal_v2_gets_sane_defaults() {
        let p = PrinterProfile::from_json(&v2("")).unwrap();
        assert_eq!(p.inputs().len(), 1);
        assert_eq!(p.outputs()[0].name, "Bin1");
        assert_eq!(
            p.default_ticket.media_size.as_deref(),
            Some("na_letter_8.5x11in")
        );
        assert_eq!(p.default_ticket.resolution, Some(Resolution::square(600)));
        assert!(p.color_supported() && !p.duplex());
        assert!(p.device_id().contains("MFG:"));
    }

    #[test]
    fn flat_v1_files_upgrade_with_their_old_values() {
        let raw = r#"{"id":"old","label":"Old","pages_per_minute":10,"pages_per_minute_color":0,
            "color_supported":false,"document_formats":["application/pdf"]}"#;
        let p = PrinterProfile::from_json(raw).unwrap();
        assert!(!p.color_supported());
        assert_eq!(p.description.pages_per_minute_color, None);
        assert_eq!(p.description.pages_per_minute, 10);
        assert_eq!(p.documents.formats, vec!["application/pdf"]);
        assert_eq!(p.device_id(), default_device_id());
        assert!(err_of(r#"{"id":"o","label":"O","bogus":1}"#).contains("bogus"));
    }

    #[test]
    fn malformed_files_are_refused_with_helpful_errors() {
        let cases: Vec<(String, &str)> = vec![
            ("{".into(), "not valid JSON"),
            (
                r#"{"schema_version":3,"id":"t","label":"x"}"#.into(),
                "not supported",
            ),
            (
                r#"{"schema_version":"two","id":"t","label":"x"}"#.into(),
                "schema_version",
            ),
            (v2(r#","colour":1"#), "colour"),
            (v2(r#","identity":{"manufacturr":"x"}"#), "manufacturr"),
            (v2(r#","documents":{"formats":[]}"#), "documents.formats"),
            (
                v2(r#","documents":{"formats":["pdf"]}"#),
                "documents.formats[0]",
            ),
            (
                v2(r#","documents":{"compressions":["zip"]}"#),
                "documents.compressions[0]",
            ),
            (
                v2(r#","print":{"media_sizes":["A4"]}"#),
                "print.media_sizes[0]",
            ),
            (v2(r#","print":{"sides":["TwoSided"]}"#), "print.sides[0]"),
            (
                v2(r#","print":{"copies":{"min":5,"max":2}}"#),
                "print.copies",
            ),
            (
                v2(r#","description":{"event_rate":0}"#),
                "description.event_rate",
            ),
            (
                v2(r#","description":{"color_modes":["monochrome"],"pages_per_minute_color":5}"#),
                "pages_per_minute_color",
            ),
            (
                v2(
                    r#","default_ticket":{"media_size":"iso_a4_210x297mm"},"print":{"media_sizes":["na_letter_8.5x11in"]}"#,
                ),
                "default_ticket.media_size",
            ),
            (
                v2(r#","inputs":[{"name":"Tray1"},{"name":"Tray1"}]"#),
                "used twice",
            ),
            (v2(r#","inputs":[{"name":"Drawer"}]"#), "inputs[0].name"),
            (
                v2(r#","inputs":[{"name":"Tray1","media_size":"custom_x_1x1in"}]"#),
                "well-known sizes",
            ),
            (
                v2(r#","outputs":[{"name":"OutputBin1"}]"#),
                "outputs[0].name",
            ),
            (
                v2(r#","consumables":[{"name":"K","level":101}]"#),
                "consumables[0].level",
            ),
            (
                v2(r#","consumables":[{"name":"K","level":5,"rgb":"000000"}]"#),
                "versions.v11",
            ),
            (
                v2(r#","conditions":[{"name":"PaperOut"}]"#),
                "conditions[0].name",
            ),
            (v2(r#","extensions":{"v20":{}}"#), "versions.v20"),
            (v2(r#","identity":{"uuid":"nope"}"#), "identity.uuid"),
            (
                v2(r#","finishings":{"duplexer":true}"#),
                "finishings.duplexer",
            ),
        ];
        for (raw, needle) in &cases {
            let msg = err_of(raw);
            assert!(
                msg.contains(needle),
                "expected {needle:?} for {raw}\n got: {msg}"
            );
        }
        let both = err_of(&v2(
            r#","documents":{"formats":["pdf"]},"print":{"sides":["x y"]}"#,
        ));
        assert!(
            both.contains("documents.formats[0]") && both.contains("print.sides[0]"),
            "{both}"
        );
    }

    #[test]
    fn good_variations_load() {
        let raw = v2(
            r#","identity":{"manufacturer":"Acme","model":"X1","serial_number":"SN1",
                 "uuid":"11111111-2222-4333-8444-555555555555"},
               "versions":{"v11":true,"v12":true,"v20":true},
               "description":{"color_modes":["monochrome"],"pages_per_minute":30,"pages_per_minute_color":null},
               "documents":{"formats":["application/pdf","image/pwg-raster","image/urf","application/vnd.ms-xpsdocument"],"compressions":["None","Gzip"]},
               "print":{"sides":["OneSided","TwoSidedLongEdge"],"media_sizes":["iso_a4_210x297mm","custom_label_4x6in"],"media_colors":["white"]},
               "inputs":[{"name":"Tray1","media_size":"iso_a4_210x297mm"},{"name":"Bypass","media_size":"iso_a4_210x297mm"}],
               "outputs":[{"name":"Face-down"}],
               "consumables":[{"name":"K","level":50,"color":"Black","rgb":"000000"}],
               "conditions":[{"name":"ConsumableLow","component":"K"}],
               "extensions":{"model_id":"11111111-2222-4333-8444-555555555555","v20":{}}"#,
        );
        let p = PrinterProfile::from_json(&raw).unwrap();
        assert!(p.duplex() && p.versions.v20);
        assert_eq!(
            p.default_ticket.media_size.as_deref(),
            Some("iso_a4_210x297mm")
        );
        let back = PrinterProfile::from_json(&p.to_json().unwrap()).unwrap();
        assert_eq!(p, back);
    }

    #[test]
    fn id_must_match_file_name_and_be_safe() {
        let dir = std::env::temp_dir().join(format!("fp-prof-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.json"), v2("").replace("\"t\"", "\"b\"")).unwrap();
        let e = format!("{:#}", PrinterProfile::load(&dir, "a").unwrap_err());
        assert!(e.contains("mismatch"), "{e}");
        assert!(PrinterProfile::load(&dir, "../a").is_err());
        assert!(PrinterProfile::load(&dir, "missing").is_err());
        assert_eq!(PrinterProfile::list_ids(&dir).unwrap(), vec!["a"]);
        assert!(PrinterProfile::list_ids(&dir.join("none"))
            .unwrap()
            .is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn shipped_profiles_all_load_and_keep_the_default_stable() {
        let dir = profiles_dir();
        let ids = PrinterProfile::list_ids(&dir).unwrap();
        assert!(ids.len() >= 5, "{ids:?}");
        for id in &ids {
            let p = PrinterProfile::load(&dir, id).unwrap_or_else(|e| panic!("{id}: {e:#}"));
            assert_eq!(
                p.schema_version, SCHEMA_VERSION,
                "{id} should use the current layout"
            );
        }
        let g = PrinterProfile::load(&dir, "generic-wsd").unwrap();
        assert_eq!(g.label, "Fake WSD printer");
        assert_eq!(g.device_id(), default_device_id());
        assert_eq!(g.description.pages_per_minute_color, Some(12));
        assert_eq!(PrinterProfile::generic(), g);
        assert_eq!(
            g.documents.formats,
            vec![
                "application/octet-stream",
                "application/pdf",
                "image/pwg-raster",
                "application/postscript",
                "application/vnd.hp-PCL"
            ]
        );
    }
}
