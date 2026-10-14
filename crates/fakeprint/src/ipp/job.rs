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

    /// The control-plane rendering (`GET /v1/jobs` row key `ipp`).
    pub fn to_json(&self) -> Json {
        json!({
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
