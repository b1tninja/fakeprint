//! Every operation the package defines an action for, and every fault the service can raise,
//! over a real loopback server with reply validation on: zero problems allowed.
//!
//! The five WS-Print 2.0 operations (CreatePrintJob2, GetPrintDeviceCapabilities,
//! GetPrintDeviceResources, PrepareToPrint, GetBidiSchemaExtensions) have a schema but no
//! action URI in the package, so a request cannot name them; the test asserts exactly that set
//! is what it leaves out.

mod common;

use std::collections::BTreeSet;
use std::time::Duration;

use common::*;
use fakeprint::discovery::{DIRECTED_PATH, DISCOVERY_TO};
use fakeprint::profile::PrinterProfile;
use fakeprint::server::{serve, ServerConfig, ServerHandle};
use fakeprint::wsd::fault::FaultKind;
use fakeprint::wsd::types::PrintOp;
use fakeprint::wsd::xml::{ns, QName};

const DEVICE: &str = "/WebServices/Device";
const SERVICE: &str = "/WebServices/PrinterService";
const PDF: &str = "<wprt:DocumentData>JVBERi0xLjQ=</wprt:DocumentData>";

struct Run {
    srv: ServerHandle,
    covered: BTreeSet<&'static str>,
    faults: BTreeSet<String>,
}

impl Run {
    async fn call(&mut self, op: PrintOp, path: &str, env: String) -> Resp {
        self.covered.insert(op.name());
        let r = soap(self.srv.local_addr(), path, env).await;
        if let Some(k) = r.fault_kind() {
            self.faults.insert(format!("{k:?}"));
        }
        r
    }

    /// A print-service call.
    async fn print(&mut self, op: PrintOp, body: &str) -> Resp {
        let env = envelope(op.action().expect("action"), "", body);
        self.call(op, SERVICE, env).await
    }

    async fn expect_fault(&mut self, r: Resp, kind: FaultKind) {
        assert_eq!(r.fault_kind(), Some(kind), "{}", r.text());
        self.faults.insert(format!("{kind:?}"));
    }

    async fn create(&mut self, name: &str) -> u32 {
        job_id_of(
            &self
                .print(PrintOp::CreatePrintJob, &ticket_body(name))
                .await,
        )
    }

    async fn send(&mut self, job: u32, last: bool) -> Resp {
        self.print(
            PrintOp::SendDocument,
            &send_body(job, 1, "application/pdf", "None", last, PDF),
        )
        .await
    }

    async fn faults(&self, body: serde_json::Value) {
        let r = post_json(self.srv.local_addr(), "/v1/faults", &body).await;
        assert_eq!(r.status, 200, "{}", r.text());
    }

    async fn wait_state(&self, job: u32, want: &str) {
        let end = std::time::Instant::now() + Duration::from_secs(15);
        loop {
            let ctx = self.srv.ctx().await;
            let state = ctx
                .jobs
                .get(job)
                .map(|j| j.state.token().to_string())
                .unwrap_or_default();
            if state == want {
                return;
            }
            assert!(
                std::time::Instant::now() < end,
                "job {job} is {state}, wanted {want}"
            );
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
    }
}

fn section_names(srv: &ServerHandle, sections: &[QName]) -> String {
    let _ = srv;
    sections
        .iter()
        .enumerate()
        .map(|(i, q)| {
            format!(
                "<wprt:Name xmlns:s{i}=\"{}\">s{i}:{}</wprt:Name>",
                q.ns, q.local
            )
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_operation_and_every_fault_validates() {
    let tmp = TempDir::new("allops");
    let mut profile = PrinterProfile::load(&profiles_dir(), "generic-wsd").expect("profile");
    profile.versions.v11 = true;
    profile.versions.v12 = true;
    profile.versions.v20 = true;
    profile.documents.add_document = true;
    assert_eq!(profile.problems(), Vec::<String>::new());
    let mut cfg = ServerConfig::embedded(tmp.path().join("inbox"));
    cfg.profile = Some(profile);
    cfg.directed_discovery = true;
    cfg.timing.processing_base = Duration::from_millis(1300);
    cfg.timing.processing_per_mib = Duration::ZERO;
    let srv = serve(cfg).await.expect("server");
    let addr = srv.local_addr();
    let mut run = Run {
        srv,
        covered: BTreeSet::new(),
        faults: BTreeSet::new(),
    };
    let rec = Recorder::start();

    // ---- eventing: Subscribe / GetStatus / Renew / Unsubscribe ----
    let sub = run
        .call(
            PrintOp::Subscribe,
            SERVICE,
            envelope(
                PrintOp::Subscribe.action().expect("a"),
                "",
                &format!("<wse:Subscribe><wse:Delivery><wse:NotifyTo><wsa:Address>{}</wsa:Address></wse:NotifyTo></wse:Delivery></wse:Subscribe>", rec.url()),
            ),
        )
        .await;
    let id = subscription_id(&sub);
    let hdr = identifier_header(&id);
    let r = run
        .call(
            PrintOp::GetStatus,
            SERVICE,
            envelope(
                PrintOp::GetStatus.action().expect("a"),
                &hdr,
                "<wse:GetStatus/>",
            ),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.text());
    let r = run
        .call(
            PrintOp::Renew,
            SERVICE,
            envelope(
                PrintOp::Renew.action().expect("a"),
                &hdr,
                "<wse:Renew><wse:Expires>PT2H</wse:Expires></wse:Renew>",
            ),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.text());

    // ---- GetPrinterElements: every section the model lists, plus an unknown one ----
    let ctx = run.srv.ctx().await;
    let mut names = ctx.model.sections();
    assert!(
        names.len() >= 9,
        "v1.1, v1.2 and v2.0 sections are listed: {names:?}"
    );
    names.push(QName::new("urn:vendor", "Secret"));
    let body = format!(
        "<wprt:GetPrinterElementsRequest><wprt:RequestedElements>{}</wprt:RequestedElements></wprt:GetPrinterElementsRequest>",
        section_names(&run.srv, &names)
    );
    let r = run.print(PrintOp::GetPrinterElements, &body).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let data = r
        .soap_body()
        .child(ns::PRINT, "PrinterElements")
        .expect("elements")
        .children
        .clone();
    assert_eq!(data.len(), names.len());
    assert!(data[..data.len() - 1]
        .iter()
        .all(|d| d.attr_local("Valid") == Some("true")));
    assert_eq!(data[data.len() - 1].attr_local("Valid"), Some("false"));

    // ---- jobs: create, send (inline), AddDocument, status, lists ----
    let job = run.create("one").await;
    let r = run.send(job, false).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let add = format!(
        "<wprt:AddDocumentRequest><wprt:JobId>{job}</wprt:JobId><wprt:DocumentDescription><wprt:DocumentId>2</wprt:DocumentId><wprt:Compression>None</wprt:Compression><wprt:Format>application/pdf</wprt:Format><wprt:DocumentUrl>http://127.0.0.1:9/doc.pdf</wprt:DocumentUrl></wprt:DocumentDescription><wprt:LastDocument>true</wprt:LastDocument></wprt:AddDocumentRequest>"
    );
    let r = run.print(PrintOp::AddDocument, &add).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let r = run
        .print(PrintOp::GetActiveJobs, "<wprt:GetActiveJobsRequest/>")
        .await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(
        r.soap_body()
            .child(ns::PRINT, "ActiveJobs")
            .expect("list")
            .children
            .len(),
        1
    );
    let elements = format!(
        "<wprt:GetJobElementsRequest><wprt:JobId>{job}</wprt:JobId><wprt:RequestedElements><wprt:Name>wprt:JobStatus</wprt:Name><wprt:Name>wprt:PrintTicket</wprt:Name><wprt:Name>wprt:Documents</wprt:Name><wprt:Name>wprt:Nope</wprt:Name></wprt:RequestedElements></wprt:GetJobElementsRequest>"
    );
    let r = run.print(PrintOp::GetJobElements, &elements).await;
    assert_eq!(r.status, 200, "{}", r.text());
    run.wait_state(job, "Completed").await;
    let r = run
        .print(PrintOp::GetJobHistory, "<wprt:GetJobHistoryRequest/>")
        .await;
    assert_eq!(
        r.soap_body()
            .child(ns::PRINT, "JobHistory")
            .expect("list")
            .children
            .len(),
        1
    );
    let r = run.print(PrintOp::SetEventRate, "<wprt:SetEventRateRequest><wprt:EventRate>5</wprt:EventRate></wprt:SetEventRateRequest>").await;
    assert_eq!(r.status, 200, "{}", r.text());
    let r = run
        .print(
            PrintOp::SetPrinterElements,
            "<wprt12:SetPrinterElementsRequest><wprt12:ElementsToSet><wprt12:ElementToSet><wprt12:ElementPath>wprt:PrinterDescription/wprt:PrinterName</wprt12:ElementPath><wprt12:ElementValue>x</wprt12:ElementValue></wprt12:ElementToSet><wprt12:ElementToSet><wprt12:ElementPath>vendor:Thing</wprt12:ElementPath></wprt12:ElementToSet></wprt12:ElementsToSet></wprt12:SetPrinterElementsRequest>",
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.text());

    // a job that runs long enough to show progress to a JobStatus subscriber
    let rec2 = Recorder::start();
    let status_sub = soap(
        addr,
        SERVICE,
        envelope(
            PrintOp::Subscribe.action().expect("a"),
            "",
            &format!("<wse:Subscribe><wse:Delivery><wse:NotifyTo><wsa:Address>{}</wsa:Address></wse:NotifyTo></wse:Delivery><wse:Filter Dialect=\"http://schemas.xmlsoap.org/ws/2006/02/devprof/Action\">{}</wse:Filter></wse:Subscribe>", rec2.url(), print_action("JobStatusEvent")),
        ),
    )
    .await;
    subscription_id(&status_sub);
    let job_b = run.create("progress").await;
    run.send(job_b, true).await;
    rec2.wait_for("a JobStatusEvent", Duration::from_secs(10), |ev| {
        ev.iter().any(|e| e.short() == "JobStatusEvent")
    })
    .await;
    run.wait_state(job_b, "Completed").await;

    // ---- metadata and discovery ----
    let r = run
        .call(
            PrintOp::TransferGet,
            DEVICE,
            envelope(PrintOp::TransferGet.action().expect("a"), "", ""),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.text());
    let r = run
        .call(
            PrintOp::TransferGet,
            SERVICE,
            envelope(PrintOp::TransferGet.action().expect("a"), "", ""),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.text());
    let r = run
        .call(
            PrintOp::GetMetadata,
            DEVICE,
            envelope(
                PrintOp::GetMetadata.action().expect("a"),
                "",
                "<wsx:GetMetadata/>",
            ),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.text());
    let probe = envelope_to(PrintOp::Probe, "<wsd:Probe/>");
    let r = run.call(PrintOp::Probe, DIRECTED_PATH, probe).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert!(r.text().contains(run.srv.device_endpoint()), "{}", r.text());
    let resolve = envelope_to(
        PrintOp::Resolve,
        &format!("<wsd:Resolve><wsa:EndpointReference><wsa:Address>urn:uuid:{}</wsa:Address></wsa:EndpointReference></wsd:Resolve>", run.srv.device_uuid()),
    );
    let r = run.call(PrintOp::Resolve, DIRECTED_PATH, resolve).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert!(r.text().contains("ResolveMatch"), "{}", r.text());

    // ---- fault paths ----
    // schema and header level
    let r = run
        .print(PrintOp::CreatePrintJob, "<wprt:CreatePrintJobRequest/>")
        .await;
    run.expect_fault(r, FaultKind::InvalidArgs).await;
    let r = soap(addr, SERVICE, envelope("urn:not-an-action", "", "<x/>")).await;
    run.expect_fault(r, FaultKind::ActionNotSupported).await;
    let r = soap(
        addr,
        SERVICE,
        envelope(
            &print_action("ValidatePrintTicket"),
            "",
            "<wprt:ValidatePrintTicketRequest/>",
        ),
    )
    .await;
    run.expect_fault(r, FaultKind::ActionNotSupported).await;
    let no_action =
        envelope("x", "", "<wprt:GetActiveJobsRequest/>").replace("<wsa:Action>x</wsa:Action>", "");
    let r = soap(addr, SERVICE, no_action).await;
    run.expect_fault(r, FaultKind::MessageInformationHeaderRequired)
        .await;
    let r = request(
        addr,
        "POST",
        SERVICE,
        vec![
            ("Content-Type".into(), "application/soap+xml".into()),
            ("SOAPAction".into(), "\"urn:something-else\"".into()),
        ],
        envelope(
            &print_action("GetActiveJobs"),
            "",
            "<wprt:GetActiveJobsRequest/>",
        )
        .into_bytes(),
    )
    .await;
    run.expect_fault(r, FaultKind::InvalidMessageInformationHeader)
        .await;
    let must = "<v:Block xmlns:v=\"urn:vendor\" soap:mustUnderstand=\"1\">x</v:Block>";
    let r = soap(
        addr,
        SERVICE,
        envelope(
            &print_action("GetActiveJobs"),
            must,
            "<wprt:GetActiveJobsRequest/>",
        ),
    )
    .await;
    run.expect_fault(r, FaultKind::MustUnderstand).await;
    let soap11 = envelope(
        &print_action("GetActiveJobs"),
        "",
        "<wprt:GetActiveJobsRequest/>",
    )
    .replace(ns::SOAP12, ns::SOAP11);
    let r = soap(addr, SERVICE, soap11).await;
    run.expect_fault(r, FaultKind::VersionMismatch).await;
    let r = soap(addr, SERVICE, "this is not xml".to_string()).await;
    run.expect_fault(r, FaultKind::Malformed).await;

    // job ids
    let r = run.send(9999, true).await;
    run.expect_fault(r, FaultKind::ClientErrorJobIdNotFound)
        .await;
    let r = run
        .print(
            PrintOp::CancelJob,
            "<wprt:CancelJobRequest><wprt:JobId>9999</wprt:JobId></wprt:CancelJobRequest>",
        )
        .await;
    run.expect_fault(r, FaultKind::ClientErrorJobIdNotFound)
        .await;
    let r = run
        .print(
            PrintOp::CancelJob,
            &format!(
                "<wprt:CancelJobRequest><wprt:JobId>{job}</wprt:JobId></wprt:CancelJobRequest>"
            ),
        )
        .await;
    run.expect_fault(r, FaultKind::OperationFailed).await;
    let r = run.send(job, true).await;
    run.expect_fault(r, FaultKind::ClientErrorLastDocumentAlreadySent)
        .await;

    // documents
    let j = run.create("formats").await;
    let r = run
        .print(
            PrintOp::SendDocument,
            &send_body(j, 1, "image/x-nope", "None", true, PDF),
        )
        .await;
    run.expect_fault(r, FaultKind::ClientErrorFormatNotSupported)
        .await;
    let r = run
        .print(
            PrintOp::SendDocument,
            &send_body(j, 1, "application/pdf", "Gzip", true, PDF),
        )
        .await;
    run.expect_fault(r, FaultKind::ClientErrorCompressionNotSupported)
        .await;
    let bad_url = |u: &str| {
        format!("<wprt:AddDocumentRequest><wprt:JobId>{j}</wprt:JobId><wprt:DocumentDescription><wprt:DocumentId>1</wprt:DocumentId><wprt:Compression>None</wprt:Compression><wprt:Format>application/pdf</wprt:Format><wprt:DocumentUrl>{u}</wprt:DocumentUrl></wprt:DocumentDescription><wprt:LastDocument>true</wprt:LastDocument></wprt:AddDocumentRequest>")
    };
    let r = run
        .print(PrintOp::AddDocument, &bad_url("ftp://127.0.0.1/doc.pdf"))
        .await;
    run.expect_fault(r, FaultKind::ClientErrorDocumentURLSchemeNotSupported)
        .await;
    let r = run
        .print(PrintOp::AddDocument, &bad_url("http:///doc.pdf"))
        .await;
    run.expect_fault(r, FaultKind::ClientErrorDocumentURLNotFound)
        .await;

    // optional operation and single-document printers
    run.srv
        .world()
        .write()
        .await
        .configure_print(|c| c.add_document = false);
    let r = run
        .print(PrintOp::AddDocument, &bad_url("http://127.0.0.1/doc.pdf"))
        .await;
    run.expect_fault(r, FaultKind::ServerErrorAddDocumentNotSupported)
        .await;
    run.srv.world().write().await.configure_print(|c| {
        c.add_document = true;
        c.multiple_document_jobs = false;
    });
    let r = run.send(j, false).await;
    run.expect_fault(r, FaultKind::ClientErrorMultipleDocumentsNotSupported)
        .await;
    run.srv
        .world()
        .write()
        .await
        .configure_print(|c| c.multiple_document_jobs = true);

    // ticket conflicts
    run.srv.world().write().await.configure_print(|c| {
        c.conflict_rules.push(fakeprint::wsd::ctx::ConflictRule {
            a: ("MediaSizeName".into(), "iso_a4_210x297mm".into()),
            b: ("Orientation".into(), "Landscape".into()),
        });
    });
    let both = "<wprt:CreatePrintJobRequest><wprt:PrintTicket><wprt:JobDescription><wprt:JobName>c</wprt:JobName><wprt:JobOriginatingUserName>u</wprt:JobOriginatingUserName></wprt:JobDescription><wprt:DocumentProcessing><wprt:MediaSizeName wprt:MustHonor=\"true\">iso_a4_210x297mm</wprt:MediaSizeName><wprt:Orientation wprt:MustHonor=\"true\">Landscape</wprt:Orientation></wprt:DocumentProcessing></wprt:PrintTicket></wprt:CreatePrintJobRequest>";
    let r = run.print(PrintOp::CreatePrintJob, both).await;
    run.expect_fault(r, FaultKind::ClientErrorConflictingRequiredParameters)
        .await;

    // cancelled job, then service-level faults from the control plane
    let jc = run.create("cancelled").await;
    run.print(
        PrintOp::CancelJob,
        &format!("<wprt:CancelJobRequest><wprt:JobId>{jc}</wprt:JobId></wprt:CancelJobRequest>"),
    )
    .await;
    let r = run.send(jc, true).await;
    run.expect_fault(r, FaultKind::ServerErrorJobCancelled)
        .await;
    run.faults(serde_json::json!({"document_active": true}))
        .await;
    let r = run.send(j, true).await;
    run.expect_fault(r, FaultKind::ServerErrorDocumentActive)
        .await;
    run.faults(serde_json::json!({"document_active": false, "not_accepting_jobs": true}))
        .await;
    let r = run.print(PrintOp::CreatePrintJob, &ticket_body("n")).await;
    run.expect_fault(r, FaultKind::ServerErrorNotAcceptingJobs)
        .await;
    run.faults(serde_json::json!({"not_accepting_jobs": false, "temporary_error": true}))
        .await;
    let r = run.print(PrintOp::CreatePrintJob, &ticket_body("t")).await;
    run.expect_fault(r, FaultKind::ServerErrorTemporaryError)
        .await;
    run.faults(serde_json::json!({"temporary_error": false, "internal_error": true}))
        .await;
    let r = run.print(PrintOp::CreatePrintJob, &ticket_body("i")).await;
    run.expect_fault(r, FaultKind::ServerErrorInternalError)
        .await;
    post_json(addr, "/v1/faults", &serde_json::json!({"reset": true})).await;

    // the last of the eventing operations
    let r = run
        .call(
            PrintOp::Unsubscribe,
            SERVICE,
            envelope(
                PrintOp::Unsubscribe.action().expect("a"),
                &hdr,
                "<wse:Unsubscribe/>",
            ),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.text());

    // ---- coverage ----
    let dispatchable: BTreeSet<&'static str> = PrintOp::ALL
        .iter()
        .filter(|o| o.action().is_some())
        .map(|o| o.name())
        .collect();
    assert_eq!(run.covered, dispatchable, "operations not exercised");
    let undispatchable: BTreeSet<&str> = PrintOp::ALL
        .iter()
        .filter(|o| o.action().is_none())
        .map(|o| o.name())
        .collect();
    assert_eq!(
        undispatchable,
        BTreeSet::from([
            "CreatePrintJob2",
            "GetPrintDeviceCapabilities",
            "GetPrintDeviceResources",
            "PrepareToPrint",
            "GetBidiSchemaExtensions"
        ])
    );
    // Internal is the handler-failed fault: nothing a client can provoke.
    let all: BTreeSet<String> = FaultKind::ALL
        .iter()
        .filter(|k| **k != FaultKind::Internal)
        .map(|k| format!("{k:?}"))
        .collect();
    assert_eq!(run.faults, all, "faults not exercised");

    assert_eq!(run.srv.validation_problems(), Vec::<String>::new());
    assert!(
        run.srv.validator().checked() > 40,
        "checked {}",
        run.srv.validator().checked()
    );
    run.srv.shutdown().await;
}

fn envelope_to(op: PrintOp, body: &str) -> String {
    envelope(
        op.action().expect("a"),
        &format!("<wsa:To>{DISCOVERY_TO}</wsa:To>"),
        body,
    )
}
