//! Builders for the six WS-Print event bodies.
//!
//! Each builder turns a [`PrintEvent`] into a namespaced [`Element`] tree whose child order
//! follows the schema sequences (checked against the generated reference pages, not copied
//! from them). No string concatenation: serialization is the tree writer's job.

use super::envelope::EndpointRef;
use super::soap::SoapReply;
use super::types::{
    wprt, DeviceCondition, JobState, JobStateReason, PrintEvent, PrintJob, PrinterState,
};
use super::xml::{ns, Element};

/// Order of the optional children of `ElementChanges` (schema sequence).
const ELEMENT_CHANGES_ORDER: [&str; 4] = [
    "PrinterDescription",
    "PrinterConfiguration",
    "DefaultPrintTicket",
    "PrinterCapabilities",
];

fn text(local: &str, value: impl Into<String>) -> Element {
    Element::text_el(ns::PRINT, local, value)
}

/// `int` on the wire: clamp to the XSD `int` range.
fn int_text(v: u64) -> String {
    v.min(i32::MAX as u64).to_string()
}

fn reasons_element(container: &str, reasons: &[JobStateReason]) -> Element {
    reasons
        .iter()
        .fold(Element::new(ns::PRINT, container), |el, r| {
            el.with_child(text("JobStateReason", r.token()))
        })
}

/// A reason is mandatory in `JobEndState`; derive a sensible one from the final state when
/// the job carries none.
fn end_reasons(job: &PrintJob) -> Vec<JobStateReason> {
    if !job.reasons.is_empty() {
        return job.reasons.clone();
    }
    vec![match job.state {
        JobState::Completed => JobStateReason::JobCompletedSuccessfully,
        JobState::Canceled => JobStateReason::JobCanceledByUser,
        _ => JobStateReason::None,
    }]
}

/// `wprt:PrinterElementsChangeEvent`.
pub fn elements_change(changes: &[Element]) -> Element {
    let rank = |e: &Element| -> usize {
        if e.name.ns == ns::PRINT {
            ELEMENT_CHANGES_ORDER
                .iter()
                .position(|n| *n == e.name.local)
                .unwrap_or(ELEMENT_CHANGES_ORDER.len())
        } else {
            // vendor extension elements (xs:any ##other) come last
            ELEMENT_CHANGES_ORDER.len() + 1
        }
    };
    let mut sorted: Vec<&Element> = changes.iter().collect();
    sorted.sort_by_key(|e| rank(e));
    let inner = sorted
        .into_iter()
        .fold(Element::new(ns::PRINT, "ElementChanges"), |el, c| {
            el.with_child(c.clone())
        });
    Element::new(ns::PRINT, "PrinterElementsChangeEvent").with_child(inner)
}

/// `wprt:PrinterStatusSummaryEvent`.
pub fn status_summary(
    state: PrinterState,
    primary_reason: Option<&str>,
    reasons: &[String],
) -> Element {
    let mut summary =
        Element::new(ns::PRINT, "StatusSummary").with_child(text("PrinterState", state.token()));
    if let Some(p) = primary_reason {
        summary.children.push(text("PrinterPrimaryStateReason", p));
    }
    if !reasons.is_empty() {
        summary.children.push(
            reasons
                .iter()
                .fold(Element::new(ns::PRINT, "PrinterStateReasons"), |el, r| {
                    el.with_child(text("PrinterStateReason", r.as_str()))
                }),
        );
    }
    Element::new(ns::PRINT, "PrinterStatusSummaryEvent").with_child(summary)
}

/// Split `Group/Name`; a bare name belongs to the `PrintService` group.
fn component_parts(component: &str) -> (&str, &str) {
    match component.split_once('/') {
        Some((g, n)) if !g.is_empty() && !n.is_empty() => (g, n),
        _ => ("PrintService", component),
    }
}

/// `wprt:PrinterStatusConditionEvent`.
pub fn status_condition(c: &DeviceCondition) -> Element {
    let (group, name) = component_parts(&c.component);
    let cond = Element::new(ns::PRINT, "DeviceCondition")
        .with_attr("Id", c.id.to_string())
        .with_child(text("Time", c.time.as_str()))
        .with_child(text("Name", c.name.as_str()))
        .with_child(
            Element::new(ns::PRINT, "Component")
                .with_child(text("Group", group))
                .with_child(text("Name", name)),
        )
        .with_child(text("Severity", c.severity.token()));
    Element::new(ns::PRINT, "PrinterStatusConditionEvent").with_child(cond)
}

/// `wprt:PrinterStatusConditionClearedEvent`.
pub fn status_condition_cleared(condition_id: u32, cleared: &str) -> Element {
    Element::new(ns::PRINT, "PrinterStatusConditionClearedEvent").with_child(
        Element::new(ns::PRINT, "DeviceConditionCleared")
            .with_child(text("ConditionId", condition_id.to_string()))
            .with_child(text("ConditionClearTime", cleared)),
    )
}

/// `wprt:JobStatusEvent`.
pub fn job_status(job: &PrintJob) -> Element {
    let mut st = Element::new(ns::PRINT, "JobStatus")
        .with_child(text("JobId", job.id.to_string()))
        .with_child(text("JobState", job.state.token()));
    if !job.reasons.is_empty() {
        st.children
            .push(reasons_element("JobStateReasons", &job.reasons));
    }
    st.children
        .push(text("KOctetsProcessed", int_text(job.k_octets_processed())));
    st.children.push(text(
        "MediaSheetsCompleted",
        int_text(job.media_sheets_completed as u64),
    ));
    st.children.push(text(
        "NumberOfDocuments",
        int_text(job.number_of_documents() as u64),
    ));
    Element::new(ns::PRINT, "JobStatusEvent").with_child(st)
}

/// `wprt:JobEndStateEvent`.
pub fn job_end_state(job: &PrintJob) -> Element {
    let st = Element::new(ns::PRINT, "JobEndState")
        .with_child(text("JobId", job.id.to_string()))
        .with_child(text("JobCompletedState", job.state.token()))
        .with_child(reasons_element(
            "JobCompletedStateReasons",
            &end_reasons(job),
        ))
        .with_child(text("JobName", job.name.as_str()))
        .with_child(text("JobOriginatingUserName", job.user.as_str()))
        .with_child(text("KOctetsProcessed", int_text(job.k_octets_processed())))
        .with_child(text(
            "MediaSheetsCompleted",
            int_text(job.media_sheets_completed as u64),
        ))
        .with_child(text(
            "NumberOfDocuments",
            int_text(job.number_of_documents() as u64),
        ));
    Element::new(ns::PRINT, "JobEndStateEvent").with_child(st)
}

/// Body element of any event.
pub fn event_element(event: &PrintEvent) -> Element {
    match event {
        PrintEvent::PrinterElementsChange { changes } => elements_change(changes),
        PrintEvent::PrinterStatusSummary {
            state,
            primary_reason,
            reasons,
        } => status_summary(*state, primary_reason.as_deref(), reasons),
        PrintEvent::PrinterStatusCondition { condition } => status_condition(condition),
        PrintEvent::PrinterStatusConditionCleared {
            condition_id,
            cleared,
        } => status_condition_cleared(*condition_id, cleared),
        PrintEvent::JobStatus { job } => job_status(job),
        PrintEvent::JobEndState { job } => job_end_state(job),
    }
}

/// The complete notification message for `target` (a `NotifyTo`).
pub fn notification(target: &EndpointRef, event: &PrintEvent) -> SoapReply {
    SoapReply::notification(target, event.action(), event_element(event))
}

/// Qualified name of the body element an event serializes to.
pub fn body_name(event: &PrintEvent) -> super::xml::QName {
    wprt(event.name())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wsd::types::Severity;
    use crate::wsd::xml::{parse_str, PrefixMap, XmlLimits};

    fn names(el: &Element) -> Vec<&str> {
        el.children.iter().map(|c| c.name.local.as_str()).collect()
    }

    fn job(state: JobState, reasons: Vec<JobStateReason>) -> PrintJob {
        let mut j = PrintJob::new(7, "doc", "alice");
        j.state = state;
        j.reasons = reasons;
        j.octets_processed = 2049;
        j.media_sheets_completed = 3;
        j
    }

    #[test]
    fn body_names_match_event_names() {
        let j = job(JobState::Completed, vec![]);
        let evs = [
            PrintEvent::PrinterElementsChange { changes: vec![] },
            PrintEvent::PrinterStatusSummary {
                state: PrinterState::Idle,
                primary_reason: None,
                reasons: vec![],
            },
            PrintEvent::PrinterStatusCondition {
                condition: DeviceCondition {
                    id: 1,
                    time: "2026-01-01T00:00:00Z".into(),
                    name: "TonerLow".into(),
                    component: "Consumable/Black".into(),
                    severity: Severity::Warning,
                },
            },
            PrintEvent::PrinterStatusConditionCleared {
                condition_id: 1,
                cleared: "2026-01-01T00:00:01Z".into(),
            },
            PrintEvent::JobStatus { job: j.clone() },
            PrintEvent::JobEndState { job: j },
        ];
        for e in &evs {
            let el = event_element(e);
            assert_eq!(el.name, body_name(e));
            assert_eq!(el.name.local, e.name());
        }
    }

    #[test]
    fn elements_change_children_sorted_by_schema_order() {
        let changes = vec![
            Element::new(ns::PRINT, "PrinterCapabilities"),
            Element::new("urn:vendor", "Extra"),
            Element::new(ns::PRINT, "PrinterDescription"),
            Element::new(ns::PRINT, "DefaultPrintTicket"),
            Element::new(ns::PRINT, "PrinterConfiguration"),
        ];
        let el = elements_change(&changes);
        let inner = &el.children[0];
        assert_eq!(inner.name.local, "ElementChanges");
        assert_eq!(
            names(inner),
            [
                "PrinterDescription",
                "PrinterConfiguration",
                "DefaultPrintTicket",
                "PrinterCapabilities",
                "Extra"
            ]
        );
    }

    #[test]
    fn status_summary_order_and_optional_parts() {
        let el = status_summary(
            PrinterState::Stopped,
            Some("MediaEmpty"),
            &["MediaEmpty".into(), "Other".into()],
        );
        let s = &el.children[0];
        assert_eq!(
            names(s),
            [
                "PrinterState",
                "PrinterPrimaryStateReason",
                "PrinterStateReasons"
            ]
        );
        assert_eq!(s.children[2].children.len(), 2);
        let bare = status_summary(PrinterState::Idle, None, &[]);
        assert_eq!(names(&bare.children[0]), ["PrinterState"]);
    }

    #[test]
    fn condition_shape() {
        let c = DeviceCondition {
            id: 5,
            time: "t".into(),
            name: "n".into(),
            component: "InputBin/Tray1".into(),
            severity: Severity::Critical,
        };
        let el = status_condition(&c);
        let dc = &el.children[0];
        assert_eq!(dc.attr_local("Id"), Some("5"));
        assert_eq!(names(dc), ["Time", "Name", "Component", "Severity"]);
        assert_eq!(names(&dc.children[2]), ["Group", "Name"]);
        assert_eq!(dc.children[2].children[0].text, "InputBin");
        assert_eq!(dc.children[2].children[1].text, "Tray1");
        let bare = status_condition(&DeviceCondition {
            component: "Spooler".into(),
            ..c
        });
        assert_eq!(
            bare.children[0].children[2].children[0].text,
            "PrintService"
        );
    }

    #[test]
    fn cleared_shape() {
        let el = status_condition_cleared(5, "ts");
        assert_eq!(
            names(&el.children[0]),
            ["ConditionId", "ConditionClearTime"]
        );
    }

    #[test]
    fn job_status_order_and_counters() {
        let el = job_status(&job(
            JobState::Processing,
            vec![JobStateReason::JobPrinting],
        ));
        let s = &el.children[0];
        assert_eq!(
            names(s),
            [
                "JobId",
                "JobState",
                "JobStateReasons",
                "KOctetsProcessed",
                "MediaSheetsCompleted",
                "NumberOfDocuments"
            ]
        );
        assert_eq!(s.children[3].text, "3"); // 2049 bytes round up to 3 KiB
        let noreason = job_status(&job(JobState::Pending, vec![]));
        assert!(!names(&noreason.children[0]).contains(&"JobStateReasons"));
    }

    #[test]
    fn job_end_state_order_and_mandatory_reason() {
        let el = job_end_state(&job(JobState::Canceled, vec![]));
        let s = &el.children[0];
        assert_eq!(
            names(s),
            [
                "JobId",
                "JobCompletedState",
                "JobCompletedStateReasons",
                "JobName",
                "JobOriginatingUserName",
                "KOctetsProcessed",
                "MediaSheetsCompleted",
                "NumberOfDocuments"
            ]
        );
        assert_eq!(s.children[2].children[0].name.local, "JobStateReason");
        assert_eq!(s.children[2].children[0].text, "JobCanceledByUser");
    }

    #[test]
    fn notification_serializes_and_reparses() {
        let target = EndpointRef::new("http://127.0.0.1:5357/x");
        let ev = PrintEvent::JobEndState {
            job: job(
                JobState::Completed,
                vec![JobStateReason::JobCompletedSuccessfully],
            ),
        };
        let msg = notification(&target, &ev);
        assert_eq!(msg.action, ev.action());
        assert_eq!(msg.to, "http://127.0.0.1:5357/x");
        let xml = msg.to_xml();
        let root = parse_str(&xml, &XmlLimits::SMALL).expect("reparse");
        let _ = PrefixMap::wsd();
        let body = root.child(ns::SOAP12, "Body").expect("body");
        assert!(body.child(ns::PRINT, "JobEndStateEvent").is_some());
    }
}
