//! What an IPP submission leaves on a job in the shared job engine.
//!
//! The engine's `PrintJob` carries the state machine, the documents and the counters for every
//! protocol. A job created over IPP additionally keeps the job-template attributes the printer
//! accepted ([`IppJobData`]), and a WS-Print `PrintTicket` equivalent of them (stored as the
//! job's ticket), so `GET /v1/jobs` shows the same typed ticket whichever protocol made the job.

use serde_json::{json, Value as Json};

use super::codec::Attr;

/// The accepted submission of an IPP job.
#[derive(Debug, Clone, PartialEq)]
pub struct IppJobData {
    /// IPP version of the creating request.
    pub version: (u8, u8),
    /// Job-template attributes the printer accepted (unsupported ones were dropped, or the job
    /// was refused when `ipp-attribute-fidelity` was true).
    pub attrs: Vec<Attr>,
    /// `ipp-attribute-fidelity` of the request.
    pub fidelity: bool,
    /// `job-name` as sent, if any.
    pub job_name: Option<String>,
    /// `requesting-user-name` as sent, if any.
    pub requesting_user: Option<String>,
    /// The PWG media name the job prints on: its `media`/`media-col`, else the printer's default.
    pub media: String,
    /// `print-scaling-default` when the printer advertises `print-scaling`, else `None`.
    pub scaling_default: Option<String>,
}

/// The first `/MediaBox [x0 y0 x1 y1]` of a PDF, as (width, height) in points. The fake stores
/// documents as received, so this is a plain byte scan (no object parsing, no compressed object
/// streams): enough for the flat PDFs a test client sends, `None` for anything else.
pub fn pdf_media_box(data: &[u8]) -> Option<(f64, f64)> {
    if !data.starts_with(b"%PDF") {
        return None;
    }
    let key = b"/MediaBox";
    let at = data.windows(key.len()).position(|w| w == key)? + key.len();
    let rest = data.get(at..at + 200.min(data.len() - at))?;
    let open = rest.iter().position(|b| *b == b'[')?;
    let close = rest.iter().position(|b| *b == b']')?;
    let text = std::str::from_utf8(rest.get(open + 1..close)?).ok()?;
    let n: Vec<f64> = text
        .split_ascii_whitespace()
        .filter_map(|t| t.parse().ok())
        .collect();
    let [x0, y0, x1, y1] = n[..] else { return None };
    let (w, h) = ((x1 - x0).abs(), (y1 - y0).abs());
    (w > 0.0 && h > 0.0).then_some((w, h))
}

/// The scale a printer applies to a page of `page` points to put it on `media` (points), by
/// `print-scaling` keyword (PWG 5100.13): `none` 1; `fit` the largest uniform scale that fits;
/// `auto-fit` the same but never above 1; `fill` the smallest uniform scale that covers; `auto`
/// is the printer's own choice, which this fake does not model (1). A page whose orientation
/// differs from the media is compared with the media turned.
pub fn scale_for(keyword: &str, page: (f64, f64), media: (f64, f64)) -> f64 {
    let media = if (page.0 > page.1) == (media.0 > media.1) {
        media
    } else {
        (media.1, media.0)
    };
    let (sx, sy) = (media.0 / page.0, media.1 / page.1);
    match keyword {
        "fit" => sx.min(sy),
        "auto-fit" => sx.min(sy).min(1.0),
        "fill" => sx.max(sy),
        _ => 1.0,
    }
}

impl IppJobData {
    /// The value of an accepted attribute.
    pub fn attr(&self, name: &str) -> Option<&Attr> {
        self.attrs.iter().find(|a| a.name == name)
    }

    /// Whether the job asked for two-sided output.
    pub fn two_sided(&self) -> bool {
        self.attr("sides")
            .and_then(|a| a.first())
            .and_then(|v| v.as_str())
            .is_some_and(|s| s.starts_with("two-sided"))
    }

    /// The `copies` requested (1 when absent).
    pub fn copies(&self) -> u32 {
        self.attr("copies")
            .and_then(|a| a.first())
            .and_then(|v| v.as_int())
            .and_then(|c| u32::try_from(c).ok())
            .unwrap_or(1)
            .max(1)
    }

    /// The `print-scaling` the job gets: its own, else the printer's default.
    pub fn scaling(&self) -> Option<String> {
        self.attr("print-scaling")
            .and_then(|a| a.first())
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .or_else(|| self.scaling_default.clone())
    }

    /// The scale the printer would apply to the first page of `document` (PDF bytes), computed
    /// from the page size and the job's media; `None` when the page size is unknown (not a flat
    /// PDF, or an unreadable media name).
    pub fn effective_scale(&self, document: &[u8]) -> Option<f64> {
        let page = pdf_media_box(document)?;
        let (w, h) = crate::ipp::caps::media_dimensions(&self.media)?;
        // hundredths of a millimetre to points
        let media = (f64::from(w) * 72.0 / 2540.0, f64::from(h) * 72.0 / 2540.0);
        Some(scale_for(&self.scaling().unwrap_or_default(), page, media))
    }

    /// The control-plane rendering (`GET /v1/jobs` row key `ipp`).
    pub fn to_json(&self) -> Json {
        self.to_json_with(None)
    }

    /// [`Self::to_json`] with the first document's bytes, so the row also carries `scaling` (the
    /// keyword in effect: the job's, else the printer's default), `media` and `effective_scale`.
    pub fn to_json_with(&self, document: Option<&[u8]>) -> Json {
        let effective = document.and_then(|d| self.effective_scale(d));
        json!({
            "scaling": self.scaling(),
            "media": self.media,
            "effective_scale": effective,
            "version": format!("{}.{}", self.version.0, self.version.1),
            "fidelity": self.fidelity,
            "job_name": self.job_name,
            "requesting_user_name": self.requesting_user,
            "attributes": Json::Object(
                self.attrs.iter().map(|a| (a.name.clone(), a.to_json())).collect()
            ),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_media_box_is_read_from_a_flat_pdf() {
        let pdf = b"%PDF-1.4\n<< /MediaBox [0 0 595.28 841.89] >>";
        assert_eq!(pdf_media_box(pdf), Some((595.28, 841.89)));
        assert_eq!(pdf_media_box(b"%PDF-1.4 fake"), None);
        assert_eq!(pdf_media_box(b"<< /MediaBox [0 0 1 1] >>"), None);
        assert_eq!(pdf_media_box(b"%PDF /MediaBox [0 0 0 9]"), None);
    }

    #[test]
    fn keywords_scale_by_pwg_5100_13() {
        let a4 = (595.28, 841.89);
        let letter = (612.0, 792.0);
        let s = |k| scale_for(k, a4, letter);
        assert!((s("fit") - 792.0 / 841.89).abs() < 1e-9);
        assert_eq!(s("auto-fit"), s("fit"), "oversized: both shrink");
        assert!((s("fill") - 612.0 / 595.28).abs() < 1e-9);
        assert_eq!((s("none"), s("auto")), (1.0, 1.0));
        // a page that already fits is not enlarged by auto-fit, but is by fit
        assert_eq!(scale_for("auto-fit", (300.0, 400.0), letter), 1.0);
        assert!(scale_for("fit", (300.0, 400.0), letter) > 1.0);
        // landscape page, portrait media: the media is turned
        assert!((scale_for("fit", (841.89, 595.28), letter) - 792.0 / 841.89).abs() < 1e-9);
    }
}
