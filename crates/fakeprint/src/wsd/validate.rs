//! The reply-validation switch.
//!
//! When it is on, every SOAP body the service sends (operation replies, event notifications,
//! the structure of faults) goes through the schema validator of `print_schema`. A violation is
//! never fatal: it is logged loudly (`tracing::error!`), counted, and the newest messages are
//! kept so a test or `/v1/status` can show them. The switch is on when
//! `FAKEPRINT_VALIDATE_REPLIES` is `1`/`true`/`yes`/`on`, when the crate is built for its own
//! unit tests, or when a server is configured with `validate_replies`.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use serde::Serialize;

use super::elements::schema::{self, Options, Target};
use super::envelope::EndpointRef;
use super::eventing::{Deliver, DeliverError};
use super::fault::FaultKind;
use super::soap::SoapReply;
use super::types::PrintOp;
use super::xml::{ns, Element};

/// Environment variable that switches validation on.
pub const ENV_VALIDATE: &str = "FAKEPRINT_VALIDATE_REPLIES";

/// Messages kept for inspection.
const KEEP: usize = 64;

/// Whether the environment (or a unit-test build) asks for validation.
pub fn requested_by_environment() -> bool {
    if cfg!(test) {
        return true;
    }
    std::env::var(ENV_VALIDATE)
        .map(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

/// Snapshot for `/v1/status`.
#[derive(Debug, Clone, Serialize)]
pub struct ValidationStats {
    pub enabled: bool,
    /// Bodies checked so far.
    pub checked: u64,
    /// Violations found so far.
    pub problems: u64,
    /// The newest problem messages (bounded).
    pub recent: Vec<String>,
}

/// Validates outgoing bodies and counts the violations.
pub struct ReplyValidator {
    enabled: AtomicBool,
    options: Mutex<Options>,
    checked: AtomicU64,
    problems: AtomicU64,
    recent: Mutex<VecDeque<String>>,
}

impl ReplyValidator {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled: AtomicBool::new(enabled),
            options: Mutex::new(Options::STRICT),
            checked: AtomicU64::new(0),
            problems: AtomicU64::new(0),
            recent: Mutex::new(VecDeque::new()),
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    pub fn set_enabled(&self, on: bool) {
        self.enabled.store(on, Ordering::Relaxed);
    }

    /// The validator options replies must pass (the profile's quirks relax them).
    pub fn set_options(&self, opts: Options) {
        *lock(&self.options) = opts;
    }

    /// Tolerate the vendor `JobToken` element (follows the service's `job_token` quirk).
    pub fn set_job_token_quirk(&self, on: bool) {
        lock(&self.options).quirk_job_token = on;
    }

    fn options(&self) -> Options {
        *lock(&self.options)
    }

    /// Total violations found.
    pub fn problem_count(&self) -> u64 {
        self.problems.load(Ordering::Relaxed)
    }

    /// Bodies checked.
    pub fn checked(&self) -> u64 {
        self.checked.load(Ordering::Relaxed)
    }

    /// The newest violation messages, oldest first.
    pub fn recent_problems(&self) -> Vec<String> {
        lock(&self.recent).iter().cloned().collect()
    }

    pub fn clear(&self) {
        self.checked.store(0, Ordering::Relaxed);
        self.problems.store(0, Ordering::Relaxed);
        lock(&self.recent).clear();
    }

    pub fn stats(&self) -> ValidationStats {
        ValidationStats {
            enabled: self.enabled(),
            checked: self.checked(),
            problems: self.problem_count(),
            recent: self.recent_problems(),
        }
    }

    fn record(&self, what: &str, problems: Vec<String>) {
        if problems.is_empty() {
            return;
        }
        self.problems
            .fetch_add(problems.len() as u64, Ordering::Relaxed);
        let mut q = lock(&self.recent);
        for p in problems {
            tracing::error!(target: "fakeprint::validate", message = %what, problem = %p, "outgoing message violates the WS-Print schema");
            if q.len() >= KEEP {
                q.pop_front();
            }
            q.push_back(format!("{what}: {p}"));
        }
    }

    /// Check one outgoing message. `op` is the operation it answers, when known (it pins the
    /// expected root element).
    pub fn check_reply(&self, op: Option<PrintOp>, reply: &SoapReply) {
        if !self.enabled() {
            return;
        }
        let Some(body) = &reply.body else {
            return;
        };
        self.checked.fetch_add(1, Ordering::Relaxed);
        let what = short(&reply.action);
        let problems = if body.is(ns::SOAP12, "Fault") {
            fault_problems(body)
        } else {
            let opts = self.options();
            let violations = match op.filter(|o| schema::root_for(*o, true).is_some()) {
                Some(op) => schema::validate_body_with(Target::Response(op), body, &opts),
                None => schema::validate_root(body, &opts),
            };
            violations.iter().map(ToString::to_string).collect()
        };
        self.record(&what, problems);
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    match m.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    }
}

fn short(action: &str) -> String {
    action.rsplit('/').next().unwrap_or(action).to_string()
}

/// Structure of a `soap:Fault`: a Code/Value and a Reason/Text, a print subcode from the 15
/// catalogued ones, and nothing but known detail elements in the print namespace.
fn fault_problems(fault: &Element) -> Vec<String> {
    let mut out = Vec::new();
    match fault.child(ns::SOAP12, "Code") {
        Some(code) => {
            if code.child(ns::SOAP12, "Value").is_none() {
                out.push("Fault/Code has no Value".to_string());
            }
            if let Some(sub) = code.child(ns::SOAP12, "Subcode") {
                match sub.child(ns::SOAP12, "Value").and_then(Element::text_qname) {
                    Some(q) if q.ns == ns::PRINT && FaultKind::from_subcode(&q).is_none() => {
                        out.push(format!("{} is not a WS-Print fault subcode", q.local));
                    }
                    Some(_) => {}
                    None => out.push("Fault/Code/Subcode/Value is not a QName".to_string()),
                }
            }
        }
        None => out.push("Fault has no Code".to_string()),
    }
    if fault
        .child(ns::SOAP12, "Reason")
        .and_then(|r| r.child(ns::SOAP12, "Text"))
        .is_none()
    {
        out.push("Fault has no Reason/Text".to_string());
    }
    if let Some(detail) = fault.child(ns::SOAP12, "Detail") {
        for d in &detail.children {
            if d.name.ns == ns::PRINT
                && !matches!(d.name.local.as_str(), "JobId" | "Format" | "Compression")
            {
                out.push(format!("unexpected print detail element {}", d.name.local));
            }
        }
    }
    out
}

/// A [`Deliver`] that validates every message before handing it to the real one.
pub struct ValidatingDeliver {
    pub inner: Arc<dyn Deliver>,
    pub validator: Arc<ReplyValidator>,
}

impl Deliver for ValidatingDeliver {
    fn deliver(&self, target: &EndpointRef, msg: &SoapReply) -> Result<(), DeliverError> {
        self.validator.check_reply(None, msg);
        self.inner.deliver(target, msg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wsd::fault::PrintFault;

    #[test]
    fn counts_and_keeps_violations() {
        let v = ReplyValidator::new(true);
        let bad = SoapReply::new(
            "http://schemas.microsoft.com/windows/2006/08/wdp/print/CancelJobResponse",
            Element::new(ns::PRINT, "CancelJobResponse")
                .with_child(Element::new(ns::PRINT, "Bogus")),
        );
        v.check_reply(Some(PrintOp::CancelJob), &bad);
        assert_eq!(v.checked(), 1);
        assert!(v.problem_count() >= 1);
        assert!(v.recent_problems()[0].starts_with("CancelJobResponse"));
        let ok = SoapReply::new("a", Element::new(ns::PRINT, "CancelJobResponse"));
        let before = v.problem_count();
        v.check_reply(Some(PrintOp::CancelJob), &ok);
        assert_eq!(v.problem_count(), before);
    }

    #[test]
    fn disabled_validator_checks_nothing() {
        let v = ReplyValidator::new(false);
        let bad = SoapReply::new(
            "a",
            Element::new(ns::PRINT, "CancelJobResponse").with_child(Element::new(ns::PRINT, "X")),
        );
        v.check_reply(None, &bad);
        assert_eq!(v.checked(), 0);
        assert_eq!(v.problem_count(), 0);
    }

    #[test]
    fn catalogued_faults_pass_and_foreign_subcodes_do_not() {
        let v = ReplyValidator::new(true);
        for k in FaultKind::ALL {
            v.check_reply(None, &PrintFault::new(k).to_reply(None));
        }
        v.check_reply(None, &PrintFault::job_id_not_found(7).to_reply(None));
        assert_eq!(v.problem_count(), 0, "{:?}", v.recent_problems());
        let odd = PrintFault::custom(
            crate::wsd::fault::FaultCode::Sender,
            Some(crate::wsd::xml::QName::new(ns::PRINT, "NoSuchSubcode")),
            "x",
        );
        v.check_reply(None, &odd.to_reply(None));
        assert_eq!(v.problem_count(), 1);
    }
}
