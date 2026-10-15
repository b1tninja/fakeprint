//! What the printer supports, as IPP values, read from the profile.
//!
//! The profile speaks WS-Print words (`OneSided`, `Portrait`, `Draft`); IPP has its own
//! (`one-sided`, enum 3, enum 3). [`Caps`] is that translation, computed once per request from
//! the live profile, and it is the single list every part of the IPP face consults: the
//! `*-supported` / `*-default` printer attributes, the validation of job-template attributes,
//! the `job-creation-attributes-supported` list.

use crate::profile::PrinterProfile;

/// Operations the printer advertises in `operations-supported`.
pub const OPERATIONS_SUPPORTED: [i32; 8] = [
    0x0002, 0x0004, 0x0005, 0x0006, 0x0008, 0x0009, 0x000a, 0x000b,
];

/// `orientation-requested` enum values.
pub const ORIENTATION_PORTRAIT: i32 = 3;
pub const ORIENTATION_LANDSCAPE: i32 = 4;
pub const ORIENTATION_REVERSE_LANDSCAPE: i32 = 5;
pub const ORIENTATION_REVERSE_PORTRAIT: i32 = 6;

/// `print-quality` enum values.
pub const QUALITY_DRAFT: i32 = 3;
pub const QUALITY_NORMAL: i32 = 4;
pub const QUALITY_HIGH: i32 = 5;

/// The printer's capabilities in IPP terms.
#[derive(Debug, Clone, PartialEq)]
pub struct Caps {
    /// `document-format-supported`: exactly the profile's list.
    pub formats: Vec<String>,
    pub default_format: String,
    /// `compression-supported` keywords (`none` always first).
    pub compressions: Vec<String>,
    pub copies: (i32, i32),
    pub sides: Vec<String>,
    pub orientations: Vec<i32>,
    pub qualities: Vec<i32>,
    pub resolutions: Vec<(i32, i32)>,
    pub media: Vec<String>,
    pub media_types: Vec<String>,
    pub media_colors: Vec<String>,
    pub sources: Vec<String>,
    pub outputs: Vec<String>,
    pub color_modes: Vec<String>,
    pub number_up: Vec<i32>,
    pub priority: (i32, i32),
    pub multiple_documents: bool,
    pub color: bool,
    pub ppm: i32,
    pub ppm_color: Option<i32>,
    pub default_copies: i32,
    pub default_sides: String,
    pub default_orientation: i32,
    pub default_quality: i32,
    pub default_resolution: (i32, i32),
    pub default_media: String,
    pub default_color_mode: String,
    pub default_number_up: i32,
    pub default_priority: i32,
    /// `print-scaling-supported`; empty: the printer does not advertise `print-scaling`.
    pub scaling: Vec<String>,
    /// `print-scaling-default` (meaningful only when `scaling` is not empty).
    pub default_scaling: String,
}

/// `OneSided` -> `one-sided`, `TwoSidedLongEdge` -> `two-sided-long-edge`.
pub fn sides_keyword(profile_word: &str) -> String {
    kebab(profile_word)
}

/// A CamelCase or snake_case word as a kebab-case keyword.
pub fn kebab(s: &str) -> String {
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if c.is_ascii_uppercase() {
            if i > 0 && !out.ends_with('-') {
                out.push('-');
            }
            out.push(c.to_ascii_lowercase());
        } else if c == '_' || c == ' ' {
            if !out.ends_with('-') {
                out.push('-');
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn orientation_enum(w: &str) -> Option<i32> {
    match w {
        "Portrait" => Some(ORIENTATION_PORTRAIT),
        "Landscape" => Some(ORIENTATION_LANDSCAPE),
        "ReverseLandscape" => Some(ORIENTATION_REVERSE_LANDSCAPE),
        "ReversePortrait" => Some(ORIENTATION_REVERSE_PORTRAIT),
        _ => None,
    }
}

fn quality_enum(w: &str) -> Option<i32> {
    match w {
        "Draft" => Some(QUALITY_DRAFT),
        "Normal" => Some(QUALITY_NORMAL),
        "High" | "Photo" => Some(QUALITY_HIGH),
        _ => None,
    }
}

fn push_unique<T: PartialEq>(v: &mut Vec<T>, x: T) {
    if !v.contains(&x) {
        v.push(x);
    }
}

/// The `media-source` keyword of an input bin (`Tray1` -> `tray-1`).
pub fn source_keyword(bin: &str) -> String {
    let lower = bin.to_ascii_lowercase();
    if let Some(n) = lower.strip_prefix("tray") {
        if !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()) {
            return format!("tray-{n}");
        }
    }
    match lower.as_str() {
        "bypass" | "manual" | "multipurpose" | "mp" => "manual".into(),
        "top" | "main" => lower,
        _ => kebab(bin),
    }
}

/// Width and height in hundredths of a millimetre of a PWG self-describing media name
/// (`na_letter_8.5x11in` -> 21590 x 27940, `iso_a4_210x297mm` -> 21000 x 29700).
pub fn media_dimensions(name: &str) -> Option<(i32, i32)> {
    let size = name.rsplit('_').next()?;
    let (dims, unit) = size
        .strip_suffix("mm")
        .map(|d| (d, 100.0))
        .or_else(|| size.strip_suffix("in").map(|d| (d, 2540.0)))?;
    let (w, h) = dims.split_once('x')?;
    let (w, h): (f64, f64) = (w.parse().ok()?, h.parse().ok()?);
    if w <= 0.0 || h <= 0.0 {
        return None;
    }
    // rounding to whole hundredths of a millimetre
    #[allow(clippy::cast_possible_truncation)]
    Some(((w * unit).round() as i32, (h * unit).round() as i32))
}

impl Caps {
    pub fn from_profile(p: &PrinterProfile) -> Caps {
        let formats = p.documents.formats.clone();
        let default_format = if formats.iter().any(|f| f == "application/octet-stream") {
            "application/octet-stream".to_string()
        } else {
            formats
                .first()
                .cloned()
                .unwrap_or_else(|| "application/octet-stream".to_string())
        };
        let mut compressions = vec!["none".to_string()];
        for c in &p.documents.compressions {
            match c.as_str() {
                "Gzip" => push_unique(&mut compressions, "gzip".to_string()),
                "Deflate" => push_unique(&mut compressions, "deflate".to_string()),
                "Compress" => push_unique(&mut compressions, "compress".to_string()),
                _ => {}
            }
        }
        let mut sides = Vec::new();
        for s in &p.print.sides {
            push_unique(&mut sides, sides_keyword(s));
        }
        if sides.is_empty() {
            sides.push("one-sided".into());
        }
        let mut orientations = Vec::new();
        for o in &p.print.orientations {
            if let Some(e) = orientation_enum(o) {
                push_unique(&mut orientations, e);
            }
        }
        let mut qualities = Vec::new();
        for q in &p.print.qualities {
            if let Some(e) = quality_enum(q) {
                push_unique(&mut qualities, e);
            }
        }
        let resolutions: Vec<(i32, i32)> = p
            .print
            .resolutions
            .iter()
            .map(|r| (r.width, r.height()))
            .collect();
        let color = p.color_supported();
        let mut color_modes = Vec::new();
        for m in &p.description.color_modes {
            push_unique(&mut color_modes, m.to_ascii_lowercase());
        }
        if color_modes.iter().any(|m| m == "color") && color_modes.iter().any(|m| m == "monochrome")
        {
            color_modes.insert(0, "auto".into());
        }
        let media = p.print.media_sizes.clone();
        let t = &p.default_ticket;
        let default_media = t
            .media_size
            .clone()
            .filter(|m| media.contains(m))
            .or_else(|| media.first().cloned())
            .unwrap_or_else(|| "na_letter_8.5x11in".into());
        let default_sides = t
            .sides
            .as_deref()
            .map(sides_keyword)
            .filter(|s| sides.contains(s))
            .unwrap_or_else(|| sides[0].clone());
        let default_orientation = t
            .orientation
            .as_deref()
            .and_then(orientation_enum)
            .filter(|o| orientations.contains(o))
            .or_else(|| orientations.first().copied())
            .unwrap_or(ORIENTATION_PORTRAIT);
        let default_quality = t
            .quality
            .as_deref()
            .and_then(quality_enum)
            .filter(|q| qualities.contains(q))
            .or_else(|| qualities.first().copied())
            .unwrap_or(QUALITY_NORMAL);
        let default_resolution = t
            .resolution
            .map(|r| (r.width, r.height()))
            .filter(|r| resolutions.contains(r))
            .or_else(|| resolutions.first().copied())
            .unwrap_or((600, 600));
        let default_color_mode = if color { "color" } else { "monochrome" }.to_string();
        let number_up: Vec<i32> = p.print.pages_per_sheet.clone();
        let default_number_up = t
            .pages_per_sheet
            .filter(|n| number_up.contains(n))
            .unwrap_or(1);
        let mut sources = Vec::new();
        for b in p.inputs() {
            push_unique(&mut sources, source_keyword(&b.name));
        }
        let mut outputs = Vec::new();
        for b in p.outputs() {
            push_unique(&mut outputs, kebab(&b.name).to_ascii_lowercase());
        }
        let scaling = p.print.scaling.clone();
        let default_scaling = t
            .scaling
            .clone()
            .filter(|k| scaling.contains(k))
            .or_else(|| scaling.iter().find(|k| *k == "auto").cloned())
            .or_else(|| scaling.first().cloned())
            .unwrap_or_default();
        Caps {
            formats,
            default_format,
            compressions,
            copies: (p.print.copies.min, p.print.copies.max),
            sides,
            orientations,
            qualities,
            resolutions,
            media,
            media_types: p.print.media_types.clone(),
            media_colors: p.print.media_colors.clone(),
            sources,
            outputs,
            color_modes,
            number_up,
            priority: (p.print.priority.min, p.print.priority.max),
            multiple_documents: p.description.multiple_document_jobs,
            color,
            ppm: i32::try_from(p.description.pages_per_minute).unwrap_or(i32::MAX),
            ppm_color: color.then(|| {
                i32::try_from(p.description.pages_per_minute_color.unwrap_or(12))
                    .unwrap_or(i32::MAX)
            }),
            default_copies: t.copies.clamp(p.print.copies.min, p.print.copies.max),
            default_sides,
            default_orientation,
            default_quality,
            default_resolution,
            default_media,
            default_color_mode,
            default_number_up,
            default_priority: t.priority.clamp(p.print.priority.min, p.print.priority.max),
            scaling,
            default_scaling,
        }
    }

    /// Whether a document format is listed (MIME types compare case-insensitively).
    pub fn format_ok(&self, f: &str) -> bool {
        self.formats.iter().any(|x| x.eq_ignore_ascii_case(f))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_names_become_dimensions() {
        assert_eq!(media_dimensions("na_letter_8.5x11in"), Some((21590, 27940)));
        assert_eq!(media_dimensions("iso_a4_210x297mm"), Some((21000, 29700)));
        assert_eq!(media_dimensions("custom_photo_4x6in"), Some((10160, 15240)));
        assert_eq!(media_dimensions("letter"), None);
        assert_eq!(media_dimensions("x_0x5mm"), None);
    }

    #[test]
    fn profile_words_become_ipp_words() {
        assert_eq!(sides_keyword("TwoSidedLongEdge"), "two-sided-long-edge");
        assert_eq!(source_keyword("Tray2"), "tray-2");
        assert_eq!(source_keyword("Multipurpose"), "manual");
        assert_eq!(source_keyword("Face-down"), "face-down");
        let mut p = PrinterProfile::generic();
        p.documents.formats = vec!["application/pdf".into(), "image/jpeg".into()];
        let c = Caps::from_profile(&p);
        assert_eq!(c.default_format, "application/pdf");
        assert!(c.format_ok("IMAGE/JPEG") && !c.format_ok("image/png"));
        assert_eq!(c.compressions, ["none"]);
        assert_eq!(c.orientations, [3, 4]);
        assert_eq!(c.qualities, [3, 4, 5]);
        assert_eq!(c.sides, ["one-sided"]);
        assert!(c.color_modes.contains(&"auto".to_string()));
    }
}
