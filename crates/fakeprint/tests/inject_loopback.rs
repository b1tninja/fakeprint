//! Server-side request injection and runtime timing over a real loopback server. Reply
//! validation is on, so every fault the server builds also passes the schema validator; each
//! test that sends faults ends by asserting zero validation problems.

mod common;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::*;
use fakeprint::profile::PrinterProfile;
use fakeprint::server::{serve, ServerConfig, ServerHandle};
use fakeprint::wsd::fault::FaultKind;
use fakeprint::wsd::inject::{InjectRule, RuleEndpoint};
use fakeprint::wsd::types::PrintOp;
use fakeprint::wsd::xml::{ns, parse_str, XmlLimits};
use serde_json::json;

const SERVICE: &str = "/WebServices/PrinterService";

async fn start(tmp: &TempDir) -> ServerHandle {
    let mut cfg = ServerConfig::embedded(tmp.path().join("inbox"));
    cfg.profile =
        Some(PrinterProfile::load(&profiles_dir(), "generic-wsd").expect("shipped profile"));
    cfg.tick = Duration::from_millis(25);
    serve(cfg).await.expect("server starts")
}

async fn arm(srv: &ServerHandle, rules: serde_json::Value) -> Resp {
    post_json(srv.local_addr(), "/v1/faults", &json!({ "inject": rules })).await
}

async fn call(srv: &ServerHandle, op: &str, body: &str) -> Resp {
    soap(
        srv.local_addr(),
        SERVICE,
        envelope(&print_action(op), "", body),
    )
    .await
}

fn elements_body() -> &'static str {
    "<wprt:GetPrinterElementsRequest><wprt:RequestedElements><wprt:Name>wprt:PrinterDescription</wprt:Name></wprt:RequestedElements></wprt:GetPrinterElementsRequest>"
}

async fn create(srv: &ServerHandle) -> Resp {
    call(srv, "CreatePrintJob", &ticket_body("inj")).await
}

async fn send(srv: &ServerHandle, job: u32, last: bool) -> Resp {
    call(
        srv,
        "SendDocument",
        &send_body(
            job,
            1,
            "application/pdf",
            "None",
            last,
            "<wprt:DocumentData>JVBERi0xLjQ=</wprt:DocumentData>",
        ),
    )
    .await
}

fn json_of(r: &Resp) -> serde_json::Value {
    assert_eq!(r.status, 200, "{}", r.text());
    r.json()
}

fn pending(srv: &ServerHandle) -> usize {
    srv.pending_inject().len()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_soap_fault_effect_is_well_formed_and_the_job_engine_never_sees_the_request() {
    let tmp = TempDir::new("inj-fault");
    let srv = start(&tmp).await;
    let addr = srv.local_addr();

    let job = job_id_of(&create(&srv).await);
    let armed = arm(
        &srv,
        json!([{ "op": "SendDocument",
                 "effect": { "type": "fault", "code": "receiver",
                             "subcode": "wprt:ServerErrorTemporaryError",
                             "reason": "paper elves on strike", "detail": "injected" } }]),
    )
    .await;
    let armed = json_of(&armed);
    assert_eq!(armed["inject"]["pending"][0]["remaining"], 1);
    assert_eq!(armed["inject"]["pending"][0]["op"], "SendDocument");
    assert_eq!(armed["inject"]["hits"], 0);

    let r = send(&srv, job, true).await;
    assert_eq!(r.status, 500, "{}", r.text());
    assert_eq!(r.fault_kind(), Some(FaultKind::ServerErrorTemporaryError));
    let root = parse_str(&r.text(), &XmlLimits::SMALL).expect("well-formed");
    let relates = root
        .child(ns::SOAP12, "Header")
        .and_then(|h| h.child(ns::WSA, "RelatesTo"))
        .map(|e| e.text_trimmed().to_string())
        .expect("RelatesTo");
    assert!(relates.starts_with("urn:uuid:"), "{relates}");
    let text = r.text();
    assert!(text.contains("paper elves on strike") && text.contains("injected"));

    // the job was not touched; the next SendDocument (rule consumed) succeeds
    let jobs = get_json(addr, "/v1/jobs").await;
    assert_eq!(
        jobs["print_jobs"][0]["documents"].as_array().map(Vec::len),
        Some(0)
    );
    assert_eq!(send(&srv, job, true).await.status, 200);
    let after = get_json(addr, "/v1/faults").await;
    assert_eq!(after["inject"]["hits"], 1);
    assert_eq!(after["inject"]["pending"].as_array().map(Vec::len), Some(0));
    assert_eq!(srv.validation_problems(), Vec::<String>::new());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fault_defaults_follow_the_catalogue_and_every_kind_validates() {
    let tmp = TempDir::new("inj-kinds");
    let srv = start(&tmp).await;
    for kind in FaultKind::SCHEMA_ENUMERATION {
        srv.inject(InjectRule::fault(kind).for_print_op(PrintOp::GetActiveJobs))
            .unwrap();
        let r = call(&srv, "GetActiveJobs", "<wprt:GetActiveJobsRequest/>").await;
        assert_eq!(r.fault_kind(), Some(kind), "{}", r.text());
        assert_eq!(
            r.status,
            if kind.code() == fakeprint::wsd::fault::FaultCode::Sender {
                400
            } else {
                500
            },
            "{kind:?}"
        );
    }
    // a bare token, Sender default code, custom HTTP status
    let r = arm(
        &srv,
        json!([{ "effect": { "type": "fault", "subcode": "InvalidArgs", "http_status": 422 } }]),
    )
    .await;
    assert_eq!(r.status, 200);
    let r = call(&srv, "GetActiveJobs", "<wprt:GetActiveJobsRequest/>").await;
    assert_eq!(r.status, 422);
    assert_eq!(r.fault_kind(), Some(FaultKind::InvalidArgs));
    // a vendor subcode is passed through untouched and does not count as a schema problem
    srv.inject(InjectRule::new(
        fakeprint::wsd::inject::InjectEffect::Fault {
            code: None,
            subcode: Some("{urn:vendor}Weird".into()),
            reason: None,
            detail: None,
            http_status: None,
        },
    ))
    .unwrap();
    let r = call(&srv, "GetActiveJobs", "<wprt:GetActiveJobsRequest/>").await;
    assert_eq!(r.status, 500);
    assert!(r.text().contains("urn:vendor"), "{}", r.text());
    assert_eq!(srv.validation_problems(), Vec::<String>::new());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn skip_and_count_are_consumed_in_order_by_short_name_or_action_uri() {
    let tmp = TempDir::new("inj-skip");
    let srv = start(&tmp).await;
    // skip 1, hit 2 (short name), then a second rule by full action URI for CreatePrintJob
    let r = arm(
        &srv,
        json!([
            { "op": "GetPrinterElements", "skip": 1, "count": 2,
              "effect": { "type": "http", "status": 503, "body": "busy" } },
            { "op": print_action("CreatePrintJob"),
              "effect": { "type": "fault", "subcode": "wprt:ServerErrorNotAcceptingJobs" } }
        ]),
    )
    .await;
    let listed = json_of(&r);
    assert_eq!(listed["inject"]["pending"][0]["skip_remaining"], 1);
    assert_eq!(listed["inject"]["pending"][1]["index"], 1);

    async fn get(srv: &ServerHandle) -> u16 {
        call(srv, "GetPrinterElements", elements_body())
            .await
            .status
    }
    assert_eq!(get(&srv).await, 200, "first match is let through");
    assert_eq!(get(&srv).await, 503);
    // other operations do not consume the first rule
    assert_eq!(
        call(&srv, "GetActiveJobs", "<wprt:GetActiveJobsRequest/>")
            .await
            .status,
        200
    );
    assert_eq!(get(&srv).await, 503);
    assert_eq!(get(&srv).await, 200, "exhausted");
    assert_eq!(pending(&srv), 1);
    let c = create(&srv).await;
    assert_eq!(c.fault_kind(), Some(FaultKind::ServerErrorNotAcceptingJobs));
    assert_eq!(create(&srv).await.status, 200, "count defaults to 1");
    assert_eq!(pending(&srv), 0);
    assert_eq!(srv.inject_hits(), 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn count_null_applies_forever_until_cleared() {
    let tmp = TempDir::new("inj-forever");
    let srv = start(&tmp).await;
    let addr = srv.local_addr();
    arm(
        &srv,
        json!([{ "count": null, "endpoint": "print_service",
                 "effect": { "type": "http", "status": 502, "body": "bad gateway" } }]),
    )
    .await;
    for _ in 0..5 {
        let r = call(&srv, "GetActiveJobs", "<wprt:GetActiveJobsRequest/>").await;
        assert_eq!(r.status, 502);
        assert_eq!(r.text(), "bad gateway");
    }
    // the device endpoint is another endpoint; control routes are never injected
    let dev = soap(
        addr,
        "/WebServices/Device",
        envelope("http://schemas.xmlsoap.org/ws/2004/09/transfer/Get", "", ""),
    )
    .await;
    assert_eq!(dev.status, 200);
    assert_eq!(
        request(addr, "GET", "/health", vec![], vec![]).await.text(),
        "ok"
    );
    let state = get_json(addr, "/v1/faults").await;
    assert_eq!(
        state["inject"]["pending"][0]["remaining"],
        serde_json::Value::Null
    );
    assert_eq!(state["inject"]["hits"], 5);
    // DELETE clears everything
    let del = request(addr, "DELETE", "/v1/faults", vec![], vec![]).await;
    assert_eq!(del.status, 200);
    assert_eq!(pending(&srv), 0);
    assert_eq!(
        call(&srv, "GetActiveJobs", "<wprt:GetActiveJobsRequest/>")
            .await
            .status,
        200
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn http_effect_sends_a_non_soap_body_with_its_content_type() {
    let tmp = TempDir::new("inj-http");
    let srv = start(&tmp).await;
    srv.inject(InjectRule::http(
        503,
        "<html><body>Service Unavailable</body></html>",
    ))
    .unwrap();
    let r = call(&srv, "GetActiveJobs", "<wprt:GetActiveJobsRequest/>").await;
    assert_eq!(r.status, 503);
    assert!(r.text().starts_with("<html>"));
    arm(
        &srv,
        json!([{ "effect": { "type": "http", "status": 429, "body": "{}",
                             "content_type": "application/json" } }]),
    )
    .await;
    // injection needs the action: a body that is not SOAP is answered by the parser first
    let raw = raw_post(srv.local_addr(), SERVICE, "x").await;
    assert!(raw.contains("400"), "{raw}");
    assert_eq!(pending(&srv), 1);
    let r = call(&srv, "GetActiveJobs", "<wprt:GetActiveJobsRequest/>").await;
    assert_eq!(r.status, 429);
}

/// Send a body with a plain content type and return the raw response text.
async fn raw_post(addr: std::net::SocketAddr, path: &str, body: &str) -> String {
    let body = body.to_string();
    let path = path.to_string();
    tokio::task::spawn_blocking(move || {
        let mut s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let req = format!(
            "POST {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        s.write_all(req.as_bytes()).unwrap();
        let mut out = Vec::new();
        let _ = s.read_to_end(&mut out);
        String::from_utf8_lossy(&out).into_owned()
    })
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn close_connection_drops_the_socket_without_an_answer() {
    let tmp = TempDir::new("inj-close");
    let srv = start(&tmp).await;
    let addr = srv.local_addr();
    srv.inject(InjectRule::close_connection().for_op("GetActiveJobs"))
        .unwrap();
    let env = envelope(
        &print_action("GetActiveJobs"),
        "",
        "<wprt:GetActiveJobsRequest/>",
    );
    let raw = tokio::task::spawn_blocking(move || {
        let mut s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let req = format!(
            "POST {SERVICE} HTTP/1.1\r\nHost: x\r\nConnection: close\r\nContent-Type: application/soap+xml\r\nContent-Length: {}\r\n\r\n{env}",
            env.len()
        );
        s.write_all(req.as_bytes()).unwrap();
        let mut out = Vec::new();
        let r = s.read_to_end(&mut out);
        (out, r.is_ok())
    })
    .await
    .unwrap();
    assert!(
        !String::from_utf8_lossy(&raw.0).contains("200 OK"),
        "got {:?}",
        String::from_utf8_lossy(&raw.0)
    );
    assert!(
        raw.0.is_empty(),
        "no status line expected, got {:?}",
        String::from_utf8_lossy(&raw.0)
    );
    // the server is fine afterwards and the rule is spent
    assert_eq!(
        call(&srv, "GetActiveJobs", "<wprt:GetActiveJobsRequest/>")
            .await
            .status,
        200
    );
    assert_eq!(srv.inject_hits(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn delays_slow_the_request_before_or_after_processing() {
    let tmp = TempDir::new("inj-delay");
    let srv = start(&tmp).await;
    let tolerance = Duration::from_millis(60);

    // delay alone (Pass): a slow printer that still answers correctly
    srv.inject(InjectRule::delay(400).for_op("GetActiveJobs"))
        .unwrap();
    let t = Instant::now();
    let r = call(&srv, "GetActiveJobs", "<wprt:GetActiveJobsRequest/>").await;
    let took = t.elapsed();
    assert_eq!(r.status, 200);
    assert!(took >= Duration::from_millis(400) - tolerance, "{took:?}");
    assert!(took < Duration::from_millis(3000), "{took:?}");

    // delay_after on a real job request: the effect (the job exists) happens at once, the
    // reply comes late
    srv.inject(
        InjectRule::default()
            .for_op("CreatePrintJob")
            .delayed_after(500),
    )
    .unwrap();
    let t = Instant::now();
    let c = tokio::spawn({
        let addr = srv.local_addr();
        async move {
            soap(
                addr,
                SERVICE,
                envelope(&print_action("CreatePrintJob"), "", &ticket_body("slow")),
            )
            .await
        }
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    let listed = get_json(srv.local_addr(), "/v1/jobs").await;
    assert_eq!(
        listed["print_jobs"].as_array().map(Vec::len),
        Some(1),
        "processed before the delay ends"
    );
    let r = c.await.unwrap();
    assert_eq!(r.status, 200);
    assert!(t.elapsed() >= Duration::from_millis(500) - tolerance);

    // delay with a fault: the fault arrives after the delay
    arm(
        &srv,
        json!([{ "delay_ms": 300, "effect": { "type": "fault", "subcode": "wprt:OperationFailed" } }]),
    )
    .await;
    let t = Instant::now();
    let r = call(&srv, "GetActiveJobs", "<wprt:GetActiveJobsRequest/>").await;
    assert_eq!(r.fault_kind(), Some(FaultKind::OperationFailed));
    assert!(t.elapsed() >= Duration::from_millis(300) - tolerance);
    assert_eq!(srv.validation_problems(), Vec::<String>::new());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_requests_each_consume_exactly_one_slot() {
    let tmp = TempDir::new("inj-conc");
    let srv = Arc::new(start(&tmp).await);
    srv.inject(
        InjectRule::fault(FaultKind::ServerErrorTemporaryError)
            .for_op("GetActiveJobs")
            .skipping(3)
            .times(7),
    )
    .unwrap();
    let mut tasks = Vec::new();
    for _ in 0..30 {
        let srv = Arc::clone(&srv);
        tasks.push(tokio::spawn(async move {
            call(&srv, "GetActiveJobs", "<wprt:GetActiveJobsRequest/>")
                .await
                .status
        }));
    }
    let mut faulted = 0;
    for t in tasks {
        if t.await.unwrap() == 500 {
            faulted += 1;
        }
    }
    assert_eq!(faulted, 7);
    assert_eq!(srv.inject_hits(), 7);
    assert_eq!(pending(&srv), 0);
    assert_eq!(srv.validation_problems(), Vec::<String>::new());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bad_requests_are_refused_with_400_and_change_nothing() {
    let tmp = TempDir::new("inj-bad");
    let srv = start(&tmp).await;
    let addr = srv.local_addr();

    let r = post_json(
        addr,
        "/v1/faults",
        &json!({ "fail_next_document": "Banana" }),
    )
    .await;
    assert_eq!(r.status, 400);
    assert!(
        r.text().contains("fail_next_document") && r.text().contains("Banana"),
        "{}",
        r.text()
    );
    for ok in ["Format", "compression", "TRANSFER", "None"] {
        let r = post_json(addr, "/v1/faults", &json!({ "fail_next_document": ok })).await;
        assert_eq!(r.status, 200, "{ok}: {}", r.text());
    }

    for bad in [
        json!([{ "effect": { "type": "http", "status": 99 } }]),
        json!([{ "effect": { "type": "explode" } }]),
        json!([{ "count": 0 }]),
        json!([{ "delay_ms": 99999999 }]),
        json!([{ "effect": { "type": "fault", "subcode": "nope:Thing" } }]),
        json!([{ "unknown_key": 1 }]),
    ] {
        let r = arm(&srv, bad.clone()).await;
        assert_eq!(r.status, 400, "{bad}: {}", r.text());
    }
    // all or nothing
    let r = arm(
        &srv,
        json!([{ "op": "GetActiveJobs" }, { "effect": { "type": "http", "status": 7 } }]),
    )
    .await;
    assert_eq!(r.status, 400);
    assert!(r.text().contains("inject[1]"), "{}", r.text());
    assert_eq!(pending(&srv), 0);

    // inject_clear drops what is there before adding
    arm(&srv, json!([{ "op": "A" }, { "op": "B" }])).await;
    assert_eq!(pending(&srv), 2);
    let r = post_json(
        addr,
        "/v1/faults",
        &json!({ "inject_clear": true, "inject": [{ "op": "C" }] }),
    )
    .await;
    assert_eq!(r.status, 200);
    assert_eq!(srv.pending_inject()[0].rule.op.as_deref(), Some("C"));
    srv.clear_inject();
    assert_eq!(pending(&srv), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ipp_rules_need_endpoint_ipp_and_wsd_rules_do_not_leak_into_ipp() {
    let tmp = TempDir::new("inj-ipp");
    let srv = start(&tmp).await;
    let addr = srv.local_addr();
    srv.inject(InjectRule::http(503, "no ipp today")).unwrap();
    let r = request(addr, "GET", "/ipp/print", vec![], vec![]).await;
    assert_eq!(r.status, 200, "an any-endpoint rule is for WSD only");
    assert_eq!(pending(&srv), 1);
    srv.inject(InjectRule::http(503, "no ipp today").on(RuleEndpoint::Ipp))
        .unwrap();
    let r = request(addr, "GET", "/ipp/print", vec![], vec![]).await;
    assert_eq!(r.status, 503);
    assert_eq!(r.text(), "no ipp today");
    assert_eq!(
        request(addr, "GET", "/ipp/print", vec![], vec![])
            .await
            .status,
        200
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_job_in_processing_can_be_cancelled_and_leaves_no_capture() {
    let tmp = TempDir::new("inj-cancel");
    let srv = start(&tmp).await;
    let addr = srv.local_addr();

    let t = json_of(
        &post_json(
            addr,
            "/v1/timing",
            &json!({ "preset": "realistic", "processing_base_ms": 5000 }),
        )
        .await,
    );
    assert_eq!(t["processing_base_ms"], 5000);
    assert_eq!(t["terminating_ms"], 500);
    assert_eq!(
        get_json(addr, "/v1/timing").await["processing_base_ms"],
        5000
    );

    let job = job_id_of(&create(&srv).await);
    assert_eq!(send(&srv, job, true).await.status, 200);
    let row = get_json(addr, &format!("/v1/jobs/{job}")).await;
    assert_eq!(row["state"], "Processing");
    assert_eq!(
        get_json(addr, "/v1/jobs").await["jobs"]
            .as_array()
            .map(Vec::len),
        Some(1)
    );

    let r = call(
        &srv,
        "CancelJob",
        &format!("<wprt:CancelJobRequest><wprt:JobId>{job}</wprt:JobId></wprt:CancelJobRequest>"),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.text());
    let end = Instant::now() + Duration::from_secs(10);
    loop {
        let row = get_json(addr, &format!("/v1/jobs/{job}")).await;
        if row["state"] == "Canceled" {
            break;
        }
        assert!(Instant::now() < end, "stuck in {}", row["state"]);
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let jobs = get_json(addr, "/v1/jobs").await;
    assert_eq!(
        jobs["jobs"].as_array().map(Vec::len),
        Some(0),
        "no capture left: {jobs}"
    );
    assert_eq!(srv.world().read().await.status().captured_jobs, 0);
    assert_eq!(
        std::fs::read_dir(srv.inbox_dir()).unwrap().count(),
        0,
        "files removed too"
    );

    // switch to instant jobs on the running server: the next job completes at once
    let t = json_of(&post_json(addr, "/v1/timing", &json!({ "preset": "instant" })).await);
    assert_eq!(t["processing_base_ms"], 0);
    let job2 = job_id_of(&create(&srv).await);
    assert_eq!(send(&srv, job2, true).await.status, 200);
    let end = Instant::now() + Duration::from_secs(5);
    loop {
        let row = get_json(addr, &format!("/v1/jobs/{job2}")).await;
        if row["state"] == "Completed" {
            break;
        }
        assert!(Instant::now() < end, "stuck in {}", row["state"]);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        get_json(addr, "/v1/jobs").await["jobs"]
            .as_array()
            .map(Vec::len),
        Some(1)
    );

    // bad timing input
    for bad in [
        json!({ "preset": "warp" }),
        json!({ "bytes_per_sheet": 0 }),
        json!({ "processing_base_ms": 999999999999u64 }),
        json!({ "document_timeout_ms": 0 }),
    ] {
        assert_eq!(
            post_json(addr, "/v1/timing", &bad).await.status,
            400,
            "{bad}"
        );
    }
    assert_eq!(srv.validation_problems(), Vec::<String>::new());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn timing_is_also_reachable_through_ctl_and_the_router_can_be_mounted() {
    use tower::ServiceExt;

    let tmp = TempDir::new("inj-router");
    let srv = start(&tmp).await;
    let addr = srv.local_addr();
    let r = post_json(
        addr,
        "/v1/ctl",
        &json!({ "cmd": "timing", "args": { "preset": "instant" } }),
    )
    .await;
    assert_eq!(json_of(&r)["processing_per_mib_ms"], 0);
    let r = post_json(
        addr,
        "/v1/ctl",
        &json!({ "cmd": "faults", "args": { "inject": [{ "op": "X" }] } }),
    )
    .await;
    assert_eq!(json_of(&r)["inject"]["pending"][0]["op"], "X");
    let r = post_json(
        addr,
        "/v1/ctl",
        &json!({ "cmd": "faults", "args": { "fail_next_document": "nope" } }),
    )
    .await;
    assert_eq!(r.status, 400);

    // mount the handle's router in an outer application and call it without a socket
    let outer = axum::Router::new().nest("/printer", srv.router());
    let resp = outer
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .uri("/printer/health")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let resp = outer
        .oneshot(
            axum::http::Request::builder()
                .uri("/printer/v1/timing")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}
