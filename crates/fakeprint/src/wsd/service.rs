//! Routing of one WSD request to the part of the stack that answers it.
//!
//! [`WsdService`] is an owned snapshot of what a request needs (print context, event manager,
//! model, device metadata). `handle` looks at the `wsa:Action` and the endpoint kind:
//!
//! * print operations (and anything unknown) -> `ops::handle`;
//! * WS-Eventing management on the print service -> the [`EventManager`];
//! * WS-Transfer `Get` / WS-MetadataExchange `GetMetadata` -> [`MetadataService`];
//! * an operation on an endpoint that does not serve it -> `wsa:ActionNotSupported`.

use std::sync::Arc;

use super::ctx::PrintCtx;
use super::envelope::EndpointRef;
use super::eventing::EventManager;
use super::metadata::{DeviceInfo, MetadataService};
use super::ops;
use super::printer_model::ProfilePrinterModel;
use super::soap::{SoapReply, SoapRequest};
use super::types::{wprt, EndpointKind, PrintEvent, PrintOp};
use super::xml::{ns, Element};

/// Everything a WSD request is answered from.
#[derive(Clone)]
pub struct WsdService {
    pub ctx: Arc<PrintCtx>,
    pub events: Arc<EventManager>,
    pub model: Arc<ProfilePrinterModel>,
    pub info: DeviceInfo,
    /// Push the current state to a new subscriber right after its Subscribe.
    pub seed_subscribers: bool,
}

impl WsdService {
    /// Answer one request that arrived on `kind`. Always returns a reply (a fault on error).
    pub fn handle(&self, kind: EndpointKind, req: &SoapRequest) -> SoapReply {
        self.ctx.tick();
        let op = PrintOp::from_action_lenient(&req.action);
        match op {
            Some(
                op @ (PrintOp::Subscribe
                | PrintOp::Renew
                | PrintOp::GetStatus
                | PrintOp::Unsubscribe),
            ) if op.allowed_on(kind) => {
                let reply = self.events.handle(req);
                if op == PrintOp::Subscribe && self.seed_subscribers {
                    self.seed(&reply);
                }
                self.ctx.validator.check_reply(Some(op), &reply);
                reply
            }
            Some(op @ (PrintOp::TransferGet | PrintOp::GetMetadata)) if op.allowed_on(kind) => {
                let reply = MetadataService.handle_on(req, &self.info, kind);
                self.ctx.validator.check_reply(Some(op), &reply);
                reply
            }
            _ => {
                let reply = ops::handle(&self.ctx, kind, req);
                if op == Some(PrintOp::SetEventRate) && reply.status == 200 {
                    self.event_rate_changed();
                }
                reply
            }
        }
    }

    /// `SetEventRate` succeeded: the event manager and `PrinterConfiguration` follow.
    fn event_rate_changed(&self) {
        let secs = self.ctx.event_rate_secs();
        if let Err(fault) = self.events.set_event_rate(secs) {
            tracing::warn!(%fault, "event manager refused the event rate");
        }
        self.model.set_event_rate(secs);
    }

    /// Queue the current printer status (and configuration) for the subscription a successful
    /// `SubscribeResponse` created; the subscription's filter decides what it receives.
    fn seed(&self, reply: &SoapReply) {
        let Some(id) = subscription_id(reply) else {
            return;
        };
        let s = self.ctx.status();
        self.events.queue_for(
            &id,
            PrintEvent::PrinterStatusSummary {
                state: s.state,
                primary_reason: s.primary_reason,
                reasons: s.reasons,
            },
        );
        if let Some(config) = self.ctx.model.element(&wprt("PrinterConfiguration")) {
            self.events.queue_for(
                &id,
                PrintEvent::PrinterElementsChange {
                    changes: vec![config],
                },
            );
        }
    }
}

/// The `wse:Identifier` of the `SubscriptionManager` in a `SubscribeResponse`.
fn subscription_id(reply: &SoapReply) -> Option<String> {
    let body: &Element = reply.body.as_ref()?;
    if !body.is(ns::WSE, "SubscribeResponse") {
        return None;
    }
    let manager = body.child(ns::WSE, "SubscriptionManager")?;
    let epr = EndpointRef::from_element(manager).ok()?;
    epr.reference_parameters
        .iter()
        .find(|p| p.is(ns::WSE, "Identifier"))
        .map(|p| p.text_trimmed().to_string())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::discovery::DiscoveryStats;
    use crate::profile::PrinterProfile;
    use crate::state::World;
    use crate::wsd::eventing::Filter;

    struct Rig {
        svc: WsdService,
        dir: std::path::PathBuf,
    }

    impl Drop for Rig {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn rig(seed: bool) -> Rig {
        let dir = std::env::temp_dir().join(format!("fakeprint-svc-{}", uuid::Uuid::new_v4()));
        let mut world = World::with_options(
            dir.clone(),
            dir.join("inbox"),
            PrinterProfile::generic(),
            Arc::new(DiscoveryStats::default()),
            crate::state::WorldOptions {
                initial_events: seed,
                ..Default::default()
            },
        )
        .unwrap();
        world.set_identity(
            "http://127.0.0.1:3921/WebServices/Device".into(),
            "http://127.0.0.1:3921/WebServices/PrinterService".into(),
            uuid::Uuid::nil(),
        );
        Rig {
            svc: world.wsd_service(),
            dir,
        }
    }

    fn request(action: &str, header: &str, body: &str) -> SoapRequest {
        let xml = format!(
            r#"<s:Envelope xmlns:s="{}" xmlns:a="{}" xmlns:e="{}" xmlns:p="{}"><s:Header><a:Action>{action}</a:Action><a:MessageID>urn:uuid:11111111-1111-4111-8111-111111111111</a:MessageID>{header}</s:Header><s:Body>{body}</s:Body></s:Envelope>"#,
            ns::SOAP12,
            ns::WSA,
            ns::WSE,
            ns::PRINT
        );
        SoapRequest::parse(Some("application/soap+xml"), None, xml.as_bytes()).unwrap()
    }

    fn subscribe(r: &Rig, kind: EndpointKind) -> SoapReply {
        let action =
            "http://schemas.microsoft.com/windows/2006/08/wdp/print/PrinterStatusSummaryEvent";
        let body = format!(
            "<e:Subscribe><e:Delivery><e:NotifyTo><a:Address>http://127.0.0.1:9/n</a:Address></e:NotifyTo></e:Delivery><e:Filter Dialect=\"http://schemas.xmlsoap.org/ws/2006/02/devprof/Action\">{action}</e:Filter></e:Subscribe>"
        );
        r.svc.handle(
            kind,
            &request(PrintOp::Subscribe.action().unwrap(), "", &body),
        )
    }

    #[test]
    fn a_new_subscription_is_seeded_with_the_current_status_when_asked() {
        let r = rig(true);
        let reply = subscribe(&r, EndpointKind::PrintService);
        assert_eq!(reply.status, 200);
        let subs = r.svc.events.subscriptions();
        assert_eq!(subs.len(), 1);
        assert!(matches!(subs[0].filter, Filter::Actions(_)));
        // the summary matches the filter; the configuration change does not
        assert_eq!(subs[0].queued, 1);
        let quiet = rig(false);
        subscribe(&quiet, EndpointKind::PrintService);
        assert_eq!(quiet.svc.events.subscriptions()[0].queued, 0);
    }

    #[test]
    fn eventing_belongs_to_the_print_service_and_metadata_to_both() {
        let r = rig(false);
        let reply = subscribe(&r, EndpointKind::Device);
        assert_eq!(reply.status, 400);
        let fault = reply.body.unwrap();
        let sub = fault
            .child(ns::SOAP12, "Code")
            .unwrap()
            .child(ns::SOAP12, "Subcode")
            .unwrap();
        assert_eq!(
            sub.child(ns::SOAP12, "Value")
                .unwrap()
                .text_qname()
                .unwrap()
                .local,
            "ActionNotSupported"
        );
        assert!(r.svc.events.subscriptions().is_empty());
        for kind in [EndpointKind::Device, EndpointKind::PrintService] {
            let get = request(PrintOp::TransferGet.action().unwrap(), "", "");
            assert_eq!(
                r.svc.handle(kind, &get).body.unwrap().name.local,
                "Metadata"
            );
        }
    }

    #[test]
    fn set_event_rate_reaches_the_event_manager_and_the_configuration() {
        let r = rig(false);
        let body = "<p:SetEventRateRequest><p:EventRate>7</p:EventRate></p:SetEventRateRequest>";
        let reply = r.svc.handle(
            EndpointKind::PrintService,
            &request(PrintOp::SetEventRate.action().unwrap(), "", body),
        );
        assert_eq!(reply.status, 200);
        assert_eq!(r.svc.ctx.event_rate_secs(), 7);
        assert_eq!(r.svc.events.event_rate(), Duration::from_secs(7));
        let config = r
            .svc
            .ctx
            .model
            .element(&crate::wsd::types::wprt("PrinterConfiguration"))
            .unwrap();
        assert_eq!(
            config
                .child(ns::PRINT, "PrinterEventRate")
                .unwrap()
                .text_trimmed(),
            "7"
        );
        assert_eq!(r.svc.ctx.validation_problems(), Vec::<String>::new());
    }
}
