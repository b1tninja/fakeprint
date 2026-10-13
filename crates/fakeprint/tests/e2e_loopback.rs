//! End to end over a real loopback server: metadata, subscription, a job with inline and MTOM
//! documents, events delivered to a recorder, cancel, a paper-out fault plan, an expired
//! subscription. Reply validation is on, so every message the server sends also passes the
//! schema validator; each test ends by asserting zero problems.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use fakeprint::profile::PrinterProfile;
use fakeprint::server::{serve, ServerConfig, ServerHandle};
use fakeprint::wsd::ctx::ManualClock;
use fakeprint::wsd::xml::{ns, QName};

const DEVICE: &str = "/WebServices/Device";
const SERVICE: &str = "/WebServices/PrinterService";

fn generic() -> PrinterProfile {
    PrinterProfile::load(&profiles_dir(), "generic-wsd").expect("shipped profile")
}

async fn start(tmp: &TempDir, tweak: impl FnOnce(&mut ServerConfig)) -> ServerHandle {
    let mut cfg = ServerConfig::embedded(tmp.path().join("inbox"));
    cfg.profile = Some(generic());
    cfg.timing.processing_base = Duration::from_millis(150);
    cfg.timing.processing_per_mib = Duration::ZERO;
    tweak(&mut cfg);
    serve(cfg).await.expect("server starts")
}

async fn job_state(srv: &ServerHandle, job: u32) -> (String, Vec<String>) {
    let body = format!("<wprt:GetJobElementsRequest><wprt:JobId>{job}</wprt:JobId><wprt:RequestedElements><wprt:Name>wprt:JobStatus</wprt:Name></wprt:RequestedElements></wprt:GetJobElementsRequest>");
    let r = soap(
        srv.local_addr(),
        SERVICE,
        envelope(&print_action("GetJobElements"), "", &body),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.text());
    let b = r.soap_body();
    let state = b
        .descendant(&QName::new(ns::PRINT, "JobState"))
        .expect("JobState")
        .text_trimmed()
        .to_string();
    let reasons = b
        .descendants()
        .into_iter()
        .filter(|e| e.is(ns::PRINT, "JobStateReason"))
        .map(|e| e.text_trimmed().to_string())
        .collect();
    (state, reasons)
}

async fn wait_job(srv: &ServerHandle, job: u32, want: &str) {
    let end = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        let (state, _) = job_state(srv, job).await;
        if state == want {
            return;
        }
        assert!(
            std::time::Instant::now() < end,
            "job {job} stuck in {state}, wanted {want}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn subscribe_body(notify: &str, expires: &str, actions: &[&str]) -> String {
    let filter = actions
        .iter()
        .map(|a| print_action(a))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "<wse:Subscribe><wse:Delivery><wse:NotifyTo><wsa:Address>{notify}</wsa:Address></wse:NotifyTo></wse:Delivery><wse:Expires>{expires}</wse:Expires><wse:Filter Dialect=\"http://schemas.xmlsoap.org/ws/2006/02/devprof/Action\">{filter}</wse:Filter></wse:Subscribe>"
    )
}

const SUBSCRIBE: &str = "http://schemas.xmlsoap.org/ws/2004/08/eventing/Subscribe";
const GET_STATUS: &str = "http://schemas.xmlsoap.org/ws/2004/08/eventing/GetStatus";
const RENEW: &str = "http://schemas.xmlsoap.org/ws/2004/08/eventing/Renew";
const TRANSFER_GET: &str = "http://schemas.xmlsoap.org/ws/2004/09/transfer/Get";

const ALL_EVENTS: [&str; 5] = [
    "PrinterStatusSummaryEvent",
    "PrinterStatusConditionEvent",
    "PrinterStatusConditionClearedEvent",
    "JobStatusEvent",
    "JobEndStateEvent",
];

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn print_flow_with_events_cancel_and_paper_out() {
    let tmp = TempDir::new("e2e");
    let srv = start(&tmp, |_| {}).await;
    let addr = srv.local_addr();

    // --- the server is what it says it is ---
    assert!(addr.ip().is_loopback());
    assert_eq!(srv.base_url(), format!("http://127.0.0.1:{}", addr.port()));
    assert_ne!(srv.device_uuid(), srv.ipp_uuid());
    assert_eq!(
        request(addr, "GET", "/health", vec![], vec![]).await.text(),
        "ok"
    );

    // --- metadata on the device endpoint (WS-Transfer Get and WS-MetadataExchange) ---
    let r = soap(addr, DEVICE, envelope(TRANSFER_GET, "", "")).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let text = r.text();
    assert!(text.contains("PrintFax.Printer"), "{text}");
    assert!(
        text.contains(srv.service_endpoint()),
        "hosted endpoint is the print service: {text}"
    );
    assert!(
        text.contains(&format!("urn:uuid:{}", srv.device_uuid())),
        "{text}"
    );
    assert_eq!(
        r.action(),
        "http://schemas.xmlsoap.org/ws/2004/09/transfer/GetResponse"
    );
    let r = soap(
        addr,
        DEVICE,
        envelope(
            "http://schemas.xmlsoap.org/ws/2004/09/mex/GetMetadata/Request",
            "",
            "<wsx:GetMetadata/>",
        ),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.soap_body().children.len(), 3);
    // eventing belongs to the print service, not to the device endpoint
    let rec = Recorder::start();
    let r = soap(
        addr,
        DEVICE,
        envelope(
            SUBSCRIBE,
            "",
            &subscribe_body(&rec.url(), "PT1H", &["JobEndStateEvent"]),
        ),
    )
    .await;
    assert_eq!(
        r.fault_kind(),
        Some(fakeprint::wsd::fault::FaultKind::ActionNotSupported),
        "{}",
        r.text()
    );
    // and print operations do not belong to the device endpoint
    let r = soap(
        addr,
        DEVICE,
        envelope(
            &print_action("GetActiveJobs"),
            "",
            "<wprt:GetActiveJobsRequest/>",
        ),
    )
    .await;
    assert_eq!(
        r.fault_kind(),
        Some(fakeprint::wsd::fault::FaultKind::ActionNotSupported)
    );

    // --- subscribe to everything, speed events up ---
    let sub = soap(
        addr,
        SERVICE,
        envelope(
            SUBSCRIBE,
            "",
            &subscribe_body(&rec.url(), "PT1H", &ALL_EVENTS),
        ),
    )
    .await;
    let sub_id = subscription_id(&sub);
    let r = soap(addr, SERVICE, envelope(&print_action("SetEventRate"), "", "<wprt:SetEventRateRequest><wprt:EventRate>1</wprt:EventRate></wprt:SetEventRateRequest>")).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let st = get_json(addr, "/v1/status").await;
    assert_eq!(st["subscriptions"], 1);

    // --- job 1: ticket, an inline document, then an MTOM document as the last one ---
    let job = job_id_of(
        &soap(
            addr,
            SERVICE,
            envelope(
                &print_action("CreatePrintJob"),
                "",
                &ticket_body("two documents"),
            ),
        )
        .await,
    );
    assert_eq!(job, 1);
    let first = envelope(
        &print_action("SendDocument"),
        "",
        &send_body(
            job,
            1,
            "application/pdf",
            "None",
            false,
            "<wprt:DocumentData>JVBERi0xLjQ=</wprt:DocumentData>",
        ),
    );
    let r = soap(addr, SERVICE, first).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let pdf = b"%PDF-1.4 second document, as an MTOM attachment";
    let env = envelope(
        &print_action("SendDocument"),
        "",
        &send_body(job, 2, "application/pdf", "None", true, "<wprt:DocumentData><xop:Include xmlns:xop=\"http://www.w3.org/2004/08/xop/include\" href=\"cid:doc2@fakeprint\"/></wprt:DocumentData>"),
    );
    let (ct, body) = mtom(&env, "doc2@fakeprint", pdf);
    let r = soap_mtom(addr, SERVICE, ct, body).await;
    assert_eq!(r.status, 200, "{}", r.text());

    wait_job(&srv, job, "Completed").await;
    let (_, reasons) = job_state(&srv, job).await;
    assert_eq!(reasons, ["JobCompletedSuccessfully"]);
    // both documents are in the inbox, in order, byte for byte
    let jobs = get_json(addr, "/v1/jobs").await;
    let docs = jobs["jobs"].as_array().expect("captured list");
    assert_eq!(docs.len(), 2, "{jobs}");
    assert_eq!(
        std::fs::read(docs[0]["path"].as_str().expect("path")).expect("doc1"),
        b"%PDF-1.4"
    );
    assert_eq!(
        std::fs::read(docs[1]["path"].as_str().expect("path")).expect("doc2"),
        pdf
    );
    assert_eq!(jobs["print_jobs"][0]["state"], "Completed");

    // --- the events arrived, in a sensible order ---
    let got = rec
        .wait_for(
            "job end and the idle summary",
            Duration::from_secs(15),
            |ev| {
                ev.iter().any(|e| e.short() == "JobEndStateEvent")
                    && ev
                        .iter()
                        .rposition(|e| e.short() == "PrinterStatusSummaryEvent")
                        .is_some_and(|i| ev[i].text("PrinterState").as_deref() == Some("Idle"))
            },
        )
        .await;
    let pos = |pred: &dyn Fn(&Received) -> bool| got.iter().position(pred);
    let processing = pos(&|e| {
        e.short() == "PrinterStatusSummaryEvent"
            && e.text("PrinterState").as_deref() == Some("Processing")
    })
    .expect("a Processing summary");
    let end = pos(&|e| e.short() == "JobEndStateEvent").expect("JobEndState");
    let idle = got
        .iter()
        .rposition(|e| {
            e.short() == "PrinterStatusSummaryEvent"
                && e.text("PrinterState").as_deref() == Some("Idle")
        })
        .expect("Idle summary");
    assert!(
        processing < end,
        "the printer was Processing before the job ended: {:?}",
        rec.shorts()
    );
    assert!(processing < idle);
    assert_eq!(got[end].text("JobId").as_deref(), Some("1"));
    assert_eq!(
        got[end].text("JobCompletedState").as_deref(),
        Some("Completed")
    );
    assert!(got.iter().all(|e| e.path == "/notify"));

    // --- job 2: cancelled ---
    rec.clear();
    let job2 = job_id_of(
        &soap(
            addr,
            SERVICE,
            envelope(
                &print_action("CreatePrintJob"),
                "",
                &ticket_body("to cancel"),
            ),
        )
        .await,
    );
    assert_eq!(job2, 2);
    let r = soap(
        addr,
        SERVICE,
        envelope(
            &print_action("CancelJob"),
            "",
            &format!(
                "<wprt:CancelJobRequest><wprt:JobId>{job2}</wprt:JobId></wprt:CancelJobRequest>"
            ),
        ),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.text());
    wait_job(&srv, job2, "Canceled").await;
    assert_eq!(job_state(&srv, job2).await.1, ["JobCanceledByUser"]);
    let got = rec
        .wait_for(
            "the cancelled job's end event",
            Duration::from_secs(10),
            |ev| ev.iter().any(|e| e.short() == "JobEndStateEvent"),
        )
        .await;
    let end = got
        .iter()
        .find(|e| e.short() == "JobEndStateEvent")
        .expect("end");
    assert_eq!(end.text("JobCompletedState").as_deref(), Some("Canceled"));
    assert_eq!(end.text("JobId").as_deref(), Some("2"));

    // --- job 3: paper out stops the printer and the job; clearing resumes both ---
    rec.clear();
    let r = post_json(addr, "/v1/faults", &serde_json::json!({"paper_out": true})).await;
    assert_eq!(r.status, 200);
    let faults = get_json(addr, "/v1/faults").await;
    assert_eq!(faults["active"]["printer_state"], "Stopped");
    assert_eq!(
        faults["active"]["conditions"][0]["name"],
        "InputTraySupplyEmpty"
    );
    let job3 = job_id_of(
        &soap(
            addr,
            SERVICE,
            envelope(
                &print_action("CreatePrintJob"),
                "",
                &ticket_body("no paper"),
            ),
        )
        .await,
    );
    let r = soap(
        addr,
        SERVICE,
        envelope(
            &print_action("SendDocument"),
            "",
            &send_body(
                job3,
                1,
                "application/pdf",
                "None",
                true,
                "<wprt:DocumentData>JVBERi0xLjQ=</wprt:DocumentData>",
            ),
        ),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.text());
    let (state, reasons) = job_state(&srv, job3).await;
    assert_eq!(state, "ProcessingStopped");
    assert_eq!(reasons, ["PrinterStopped"]);
    let got = rec
        .wait_for("the condition event", Duration::from_secs(10), |ev| {
            ev.iter()
                .any(|e| e.short() == "PrinterStatusConditionEvent")
        })
        .await;
    let cond = got
        .iter()
        .find(|e| e.short() == "PrinterStatusConditionEvent")
        .expect("cond");
    assert!(
        cond.body
            .descendants()
            .into_iter()
            .any(|e| e.text_trimmed() == "InputTraySupplyEmpty"),
        "{:?}",
        cond.body
    );
    // the printer status reflects it
    let r = soap(addr, SERVICE, envelope(&print_action("GetPrinterElements"), "", "<wprt:GetPrinterElementsRequest><wprt:RequestedElements><wprt:Name>wprt:PrinterStatus</wprt:Name></wprt:RequestedElements></wprt:GetPrinterElementsRequest>")).await;
    let text = r.text();
    assert!(
        text.contains("<wprt:PrinterState>Stopped</wprt:PrinterState>"),
        "{text}"
    );
    assert!(text.contains("MediaEmpty"), "{text}");
    let r = post_json(addr, "/v1/faults", &serde_json::json!({"paper_out": false})).await;
    assert_eq!(r.status, 200);
    wait_job(&srv, job3, "Completed").await;
    rec.wait_for("the cleared event", Duration::from_secs(10), |ev| {
        ev.iter()
            .any(|e| e.short() == "PrinterStatusConditionClearedEvent")
    })
    .await;

    // --- a by-name condition from the control plane ---
    let r = post_json(
        addr,
        "/v1/faults",
        &serde_json::json!({"raise": [{"name": "CoverOpen", "component": "CoverDoor/Cover1"}]}),
    )
    .await;
    assert_eq!(r.json()["active"]["conditions"][0]["name"], "CoverOpen");
    let r = post_json(
        addr,
        "/v1/faults",
        &serde_json::json!({"clear": ["CoverOpen"]}),
    )
    .await;
    assert!(r.json()["active"]["conditions"]
        .as_array()
        .expect("list")
        .is_empty());

    // --- the subscription is manageable, then goes away ---
    let hdr = identifier_header(&sub_id);
    let r = soap(
        addr,
        SERVICE,
        envelope(GET_STATUS, &hdr, "<wse:GetStatus/>"),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.text());
    let r = soap(
        addr,
        SERVICE,
        envelope(
            "http://schemas.xmlsoap.org/ws/2004/08/eventing/Unsubscribe",
            &hdr,
            "<wse:Unsubscribe/>",
        ),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(get_json(addr, "/v1/status").await["subscriptions"], 0);

    assert_eq!(srv.validation_problems(), Vec::<String>::new());
    assert!(srv.validator().checked() > 20);
    let inbox = srv.inbox_dir().to_path_buf();
    srv.shutdown().await;
    assert!(inbox.starts_with(tmp.path()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_expired_subscription_gets_no_events_and_cannot_be_renewed() {
    let tmp = TempDir::new("expire");
    let clock = Arc::new(ManualClock::new());
    let srv = start(&tmp, |c| c.clock = Some(clock.clone())).await;
    let addr = srv.local_addr();
    let rec = Recorder::start();

    let sub = soap(
        addr,
        SERVICE,
        envelope(
            SUBSCRIBE,
            "",
            &subscribe_body(&rec.url(), "PT30S", &ALL_EVENTS),
        ),
    )
    .await;
    let id = subscription_id(&sub);
    let hdr = identifier_header(&id);
    let r = soap(
        addr,
        SERVICE,
        envelope(GET_STATUS, &hdr, "<wse:GetStatus/>"),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert!(
        r.text().contains("PT30S") || r.text().contains("PT29S"),
        "{}",
        r.text()
    );

    // a live subscription receives an event ...
    post_json(addr, "/v1/faults", &serde_json::json!({"cover_open": true})).await;
    rec.wait_for(
        "the condition and the summary while subscribed",
        Duration::from_secs(10),
        |ev| {
            ev.iter()
                .any(|e| e.short() == "PrinterStatusConditionEvent")
                && ev.iter().any(|e| e.short() == "PrinterStatusSummaryEvent")
        },
    )
    .await;

    // ... an expired one does not, and is gone from every operation
    clock.advance(Duration::from_secs(31));
    rec.clear();
    post_json(
        addr,
        "/v1/faults",
        &serde_json::json!({"cover_open": false}),
    )
    .await;
    let r = soap(
        addr,
        SERVICE,
        envelope(GET_STATUS, &hdr, "<wse:GetStatus/>"),
    )
    .await;
    assert_eq!(r.status, 400, "{}", r.text());
    assert!(r.text().contains("InvalidMessage"), "{}", r.text());
    let r = soap(
        addr,
        SERVICE,
        envelope(
            RENEW,
            &hdr,
            "<wse:Renew><wse:Expires>PT1H</wse:Expires></wse:Renew>",
        ),
    )
    .await;
    assert!(r.text().contains("UnableToRenew"), "{}", r.text());
    assert_eq!(get_json(addr, "/v1/status").await["subscriptions"], 0);
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(
        rec.received().is_empty(),
        "no event after expiry: {:?}",
        rec.shorts()
    );

    assert_eq!(srv.validation_problems(), Vec::<String>::new());
    srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_embedded_server_is_loopback_only_and_stays_in_its_inbox() {
    let tmp = TempDir::new("embedded");
    // refuses a non-loopback listen address unless told otherwise
    let mut cfg = ServerConfig::embedded(tmp.path().join("inbox"));
    cfg.listen = "0.0.0.0:0".parse().expect("addr");
    let err = serve(cfg).await.err().expect("refused");
    assert!(err.to_string().contains("not loopback"), "{err}");

    let srv = start(&tmp, |_| {}).await;
    let job = job_id_of(
        &soap(
            srv.local_addr(),
            SERVICE,
            envelope(&print_action("CreatePrintJob"), "", &ticket_body("x")),
        )
        .await,
    );
    soap(
        srv.local_addr(),
        SERVICE,
        envelope(
            &print_action("SendDocument"),
            "",
            &send_body(
                job,
                1,
                "application/pdf",
                "None",
                true,
                "<wprt:DocumentData>JVBERi0xLjQ=</wprt:DocumentData>",
            ),
        ),
    )
    .await;
    // everything the server wrote is under the temp root: the inbox only
    let entries: Vec<_> = std::fs::read_dir(tmp.path())
        .expect("dir")
        .flatten()
        .map(|e| e.file_name())
        .collect();
    assert_eq!(entries, ["inbox"]);
    // no named-pipe/lock/JetDirect/discovery in an embedded server: status shows no Hello
    let st = get_json(srv.local_addr(), "/v1/status").await;
    assert_eq!(st["wsd_hellos_sent"], 0);
    srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_ends_every_subscription_with_a_subscription_end() {
    let tmp = TempDir::new("shutdown");
    let srv = start(&tmp, |_| {}).await;
    let addr = srv.local_addr();
    let rec = Recorder::start();
    let body = format!(
        "<wse:Subscribe><wse:EndTo><wsa:Address>{u}</wsa:Address></wse:EndTo><wse:Delivery><wse:NotifyTo><wsa:Address>{u}</wsa:Address></wse:NotifyTo></wse:Delivery></wse:Subscribe>",
        u = rec.url()
    );
    let sub = soap(addr, SERVICE, envelope(SUBSCRIBE, "", &body)).await;
    subscription_id(&sub);
    assert_eq!(srv.events().subscription_count(), 1);
    assert_eq!(srv.validation_problems(), Vec::<String>::new());
    srv.shutdown().await;
    let got = rec.received();
    let end = got
        .iter()
        .find(|e| e.short() == "SubscriptionEnd")
        .expect("a SubscriptionEnd was delivered");
    let status = end
        .body
        .child(ns::WSE, "Status")
        .expect("Status")
        .text_trimmed();
    assert!(status.ends_with("SourceShuttingDown"), "{status}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn control_plane_and_sidecar_expose_name_user_ticket_and_document() {
    let tmp = TempDir::new("jobmeta");
    let srv = start(&tmp, |cfg| {
        cfg.profile = Some(
            PrinterProfile::load(&profiles_dir(), "color-inkjet-duplex").expect("shipped profile"),
        );
    })
    .await;
    let addr = srv.local_addr();
    let ticket = "<wprt:CreatePrintJobRequest><wprt:PrintTicket>\
<wprt:JobDescription><wprt:JobName>quarterly</wprt:JobName><wprt:JobOriginatingUserName>alice</wprt:JobOriginatingUserName></wprt:JobDescription>\
<wprt:JobProcessing><wprt:Copies>3</wprt:Copies><wprt:Priority>7</wprt:Priority></wprt:JobProcessing>\
<wprt:DocumentProcessing><wprt:MediaSizeName>iso_a4_210x297mm</wprt:MediaSizeName>\
<wprt:NumberUp><wprt:PagesPerSheet>2</wprt:PagesPerSheet><wprt:Direction>RightDown</wprt:Direction></wprt:NumberUp>\
<wprt:Orientation>Landscape</wprt:Orientation>\
<wprt:Resolution><wprt:Width>600</wprt:Width><wprt:Height>600</wprt:Height></wprt:Resolution>\
<wprt:PrintQuality>High</wprt:PrintQuality>\
<wprt:Sides wprt:MustHonor=\"true\">TwoSidedLongEdge</wprt:Sides></wprt:DocumentProcessing>\
</wprt:PrintTicket></wprt:CreatePrintJobRequest>";
    let r = soap(
        addr,
        SERVICE,
        envelope(&print_action("CreatePrintJob"), "", ticket),
    )
    .await;
    let job = job_id_of(&r);
    let r = soap(
        addr,
        SERVICE,
        envelope(
            &print_action("SendDocument"),
            "",
            &send_body(
                job,
                1,
                "application/pdf",
                "None",
                true,
                "<wprt:DocumentData>JVBERi0xLjQ=</wprt:DocumentData>",
            ),
        ),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.text());

    let row = get_json(addr, &format!("/v1/jobs/{job}")).await;
    assert_eq!(row["name"], "quarterly");
    assert_eq!(row["user"], "alice");
    let t = &row["ticket"];
    assert_eq!(t["job_name"], "quarterly");
    assert_eq!(t["user"], "alice");
    assert_eq!(t["copies"]["value"], 3);
    assert_eq!(t["copies"]["must_honor"], false);
    assert_eq!(t["priority"]["value"], 7);
    assert_eq!(t["sides"]["value"], "TwoSidedLongEdge");
    assert_eq!(t["sides"]["must_honor"], true);
    assert_eq!(t["orientation"]["value"], "Landscape");
    assert_eq!(t["media_size"]["value"], "iso_a4_210x297mm");
    assert_eq!(t["quality"]["value"], "High");
    assert_eq!(t["resolution"]["width"], 600);
    assert_eq!(t["resolution"]["height"], 600);
    assert_eq!(t["number_up"]["pages_per_sheet"], 2);
    assert_eq!(t["must_honor"], serde_json::json!(["sides"]));
    assert_eq!(t["document_format_requested"], "application/pdf");
    let d = &row["documents"][0];
    assert_eq!(d["seq"], 1);
    assert_eq!(d["format"], "application/pdf");
    assert_eq!(d["compression"], "None");
    assert_eq!(d["bytes"], 8);
    let file = d["file"].as_str().expect("file name").to_string();
    assert!(file.ends_with(".bin"), "{file}");
    assert!(row["created"].as_str().expect("created").ends_with('Z'));
    assert!(row["state"].is_string());

    // the list carries the same row
    let list = get_json(addr, "/v1/jobs").await;
    let listed = list["print_jobs"]
        .as_array()
        .expect("print_jobs")
        .iter()
        .find(|j| j["id"] == job)
        .expect("job listed");
    assert_eq!(listed["ticket"], row["ticket"]);

    // the sidecar next to the capture
    let sidecar = std::path::Path::new(&file).with_extension("json");
    let raw = std::fs::read(tmp.path().join("inbox").join(sidecar)).expect("sidecar");
    let side: serde_json::Value = serde_json::from_slice(&raw).expect("sidecar json");
    assert_eq!(side["job_id"], job);
    assert_eq!(side["document_format"], "application/pdf");
    assert_eq!(side["peer"], "wsd");
    assert_eq!(side["name"], "quarterly");
    assert_eq!(side["user"], "alice");
    assert_eq!(side["seq"], 1);
    assert_eq!(side["format"], "application/pdf");
    assert_eq!(side["compression"], "None");
    assert_eq!(side["ticket"]["sides"]["value"], "TwoSidedLongEdge");
    assert_eq!(side["ticket"]["sides"]["must_honor"], true);
    assert_eq!(side["ticket"]["copies"]["value"], 3);
    assert_eq!(
        side["ticket"]["document_format_requested"],
        "application/pdf"
    );
    assert_eq!(srv.validation_problems(), Vec::<String>::new());
    srv.shutdown().await;
}
