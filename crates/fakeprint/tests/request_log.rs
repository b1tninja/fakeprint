//! The raw request log over a real loopback server: what a protocol-level test reads back of
//! the exact requests (headers as written, bodies, MTOM vs inline, IPP attributes), the
//! injected faults, the ticket substitutions and the bounds of the ring.

mod common;

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use common::*;
use fakeprint::profile::PrinterProfile;
use fakeprint::reqlog::{sha256_hex, RequestLogConfig};
use fakeprint::server::{serve, ServerConfig, ServerHandle};
use fakeprint::wsd::xml::ns;
use serde_json::{json, Value};

const SERVICE: &str = "/WebServices/PrinterService";

async fn start_with(
    profile: &str,
    tweak: impl FnOnce(&mut ServerConfig),
) -> (ServerHandle, TempDir) {
    let tmp = TempDir::new("reqlog");
    let mut cfg = ServerConfig::embedded(tmp.path().join("inbox"));
    cfg.profile = Some(PrinterProfile::load(&profiles_dir(), profile).expect("shipped profile"));
    cfg.tick = Duration::from_millis(25);
    tweak(&mut cfg);
    (serve(cfg).await.expect("server starts"), tmp)
}

async fn start(tweak: impl FnOnce(&mut ServerConfig)) -> (ServerHandle, TempDir) {
    start_with("generic-wsd", tweak).await
}

async fn log(addr: SocketAddr, query: &str) -> Value {
    get_json(addr, &format!("/v1/requests{query}")).await
}

fn records(v: &Value) -> Vec<Value> {
    v["records"].as_array().cloned().unwrap_or_default()
}

fn header<'a>(rec: &'a Value, name: &str) -> Option<&'a str> {
    rec["request"]["headers"]
        .as_array()?
        .iter()
        .find(|h| h[0].as_str().is_some_and(|k| k.eq_ignore_ascii_case(name)))
        .and_then(|h| h[1].as_str())
}

/// Write `bytes` to a fresh connection and read until the peer closes.
fn raw_exchange(addr: SocketAddr, bytes: &[u8]) -> Vec<u8> {
    let mut s = TcpStream::connect(addr).expect("connect");
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    s.write_all(bytes).expect("write");
    let mut out = Vec::new();
    let _ = s.read_to_end(&mut out);
    out
}

async fn raw(addr: SocketAddr, bytes: Vec<u8>) -> Vec<u8> {
    tokio::task::spawn_blocking(move || raw_exchange(addr, &bytes))
        .await
        .expect("raw task")
}

fn inline_send(job: u32, last: bool) -> String {
    envelope(
        &print_action("SendDocument"),
        "",
        &send_body(
            job,
            1,
            "application/pdf",
            "None",
            last,
            "<wprt:DocumentData>JVBERi0xLjQ=</wprt:DocumentData>",
        ),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wsd_create_and_inline_send_document_are_captured_with_summary_and_links() {
    let (srv, _tmp) = start(|_| {}).await;
    let addr = srv.local_addr();
    let created = soap(
        addr,
        SERVICE,
        envelope(&print_action("CreatePrintJob"), "", &ticket_body("rawlog")),
    )
    .await;
    let job = job_id_of(&created);
    let sent = soap(addr, SERVICE, inline_send(job, true)).await;
    assert_eq!(sent.status, 200, "{}", sent.text());

    let l = log(addr, "").await;
    assert_eq!(l["config"]["enabled"], json!(true));
    let recs = records(&l);
    assert_eq!(recs.len(), 2, "{l}");
    let (c, s) = (&recs[0], &recs[1]);
    assert_eq!(c["direction"], "in");
    assert_eq!(c["protocol"], "wsd");
    assert_eq!(c["action"], "CreatePrintJob");
    assert_eq!(c["action_uri"], print_action("CreatePrintJob"));
    assert!(c["message_id"].as_str().unwrap().starts_with("urn:uuid:"));
    assert_eq!(c["job_id"], json!(job));
    assert_eq!(c["result"], "ok");
    assert_eq!(c["path"], SERVICE);
    assert_eq!(c["method"], "POST");
    assert_eq!(c["version"], "HTTP/1.1");
    assert_eq!(c["response"]["status"], 200);
    assert!(c["latency_us"].as_u64().is_some());
    assert!(c["t_wall"].as_str().unwrap().ends_with('Z'));
    assert!(c["seq"].as_u64().unwrap() < s["seq"].as_u64().unwrap());
    // the reply body is stored as text too
    let reply = c["response"]["body"]["text"].as_str().unwrap();
    assert!(reply.contains("CreatePrintJobResponse"), "{reply}");
    // SendDocument is a document-carrying request: stored as a head slice, not truncated here
    assert_eq!(s["action"], "SendDocument");
    assert_eq!(s["request"]["body"]["document"], json!(true));
    assert_eq!(s["job_id"], json!(job));
    let text = s["request"]["body"]["text"].as_str().unwrap();
    assert!(
        text.contains("JVBERi0xLjQ="),
        "inline base64 visible: {text}"
    );

    // job row links back to the records
    let jobs = get_json(addr, "/v1/jobs").await;
    let row = jobs["print_jobs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == json!(job))
        .unwrap();
    assert_eq!(
        row["request_seqs"],
        json!([c["seq"].clone(), s["seq"].clone()])
    );
    let raw_ticket = row["ticket_received"].as_str().unwrap();
    assert!(raw_ticket.starts_with("<wprt:PrintTicket>"), "{raw_ticket}");
    assert!(raw_ticket.contains("<wprt:JobName>rawlog</wprt:JobName>"));
    assert_eq!(row["ticket_substitutions"], json!([]));

    // filters
    let only = records(&log(addr, "?action=senddocument").await);
    assert_eq!(only.len(), 1);
    let by_job = records(&log(addr, &format!("?job={job}")).await);
    assert_eq!(by_job.len(), 2);
    let since = records(&log(addr, &format!("?since={}", c["seq"])).await);
    assert_eq!(since.len(), 1);
    let lim = records(&log(addr, "?limit=1").await);
    assert_eq!(lim.len(), 1);
    let none = log(addr, "?bodies=0").await;
    assert!(records(&none)[0]["request"]["body"].get("base64").is_none());
    assert_eq!(srv.validation_problems(), Vec::<String>::new());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mtom_send_document_is_a_head_slice_with_hash_and_length_of_the_whole() {
    let (srv, _tmp) = start(|cfg| {
        cfg.request_log.doc_head = 1800;
    })
    .await;
    let addr = srv.local_addr();
    let job = job_id_of(
        &soap(
            addr,
            SERVICE,
            envelope(&print_action("CreatePrintJob"), "", &ticket_body("mtom")),
        )
        .await,
    );
    let doc: Vec<u8> = (0..40_000u32).map(|i| b'a' + (i % 26) as u8).collect();
    let env = envelope(
        &print_action("SendDocument"),
        "",
        &send_body(
            job,
            1,
            "application/pdf",
            "None",
            true,
            "<wprt:DocumentData><xop:Include xmlns:xop=\"http://www.w3.org/2004/08/xop/include\" href=\"cid:doc1\"/></wprt:DocumentData>",
        ),
    );
    let (ct, body) = mtom(&env, "doc1", &doc);
    let whole = sha256_hex(&body);
    let total = body.len();
    let r = soap_mtom(addr, SERVICE, ct.clone(), body).await;
    assert_eq!(r.status, 200, "{}", r.text());

    let recs = records(&log(addr, "?action=SendDocument").await);
    let s = &recs[0];
    let h = header(s, "content-type").unwrap();
    assert!(h.starts_with("multipart/related"), "{h}");
    let b = &s["request"]["body"];
    assert_eq!(b["length"], json!(total));
    assert_eq!(b["stored"], json!(1800));
    assert_eq!(b["truncated"], json!(true));
    assert_eq!(b["document"], json!(true));
    assert_eq!(b["sha256"], json!(whole));
    // the head still shows the envelope part
    assert!(b["text"]
        .as_str()
        .unwrap_or("")
        .contains("SendDocumentRequest"));
    // raw endpoints
    let seq = s["seq"].as_u64().unwrap();
    let rq = request(
        addr,
        "GET",
        &format!("/v1/requests/{seq}/raw"),
        vec![],
        vec![],
    )
    .await;
    assert_eq!(rq.status, 200);
    let text = String::from_utf8_lossy(&rq.body).into_owned();
    assert!(
        text.starts_with(&format!("POST {SERVICE} HTTP/1.1\r\n")),
        "{text}"
    );
    let only_body = request(
        addr,
        "GET",
        &format!("/v1/requests/{seq}/raw?what=body"),
        vec![],
        vec![],
    )
    .await;
    assert_eq!(only_body.body.len(), 1800);
    let resp = request(
        addr,
        "GET",
        &format!("/v1/requests/{seq}/raw?part=response"),
        vec![],
        vec![],
    )
    .await;
    assert!(String::from_utf8_lossy(&resp.body).starts_with("HTTP/1.1 200"));
    let b64 = get_json(
        addr,
        &format!("/v1/requests/{seq}/raw?what=body&encoding=base64"),
    )
    .await;
    assert_eq!(b64["size"], 1800);
    assert_eq!(b64["truncated"], true);
    assert_eq!(b64["sha256"], json!(whole));
    let missing = request(addr, "GET", "/v1/requests/99999/raw", vec![], vec![]).await;
    assert_eq!(missing.status, 404);
    // the document itself still arrived whole
    assert_eq!(srv.validation_problems(), Vec::<String>::new());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn headers_keep_case_and_order_and_chunking_is_visible() {
    let (srv, _tmp) = start(|_| {}).await;
    let addr = srv.local_addr();
    let env = envelope(
        &print_action("GetPrinterElements"),
        "",
        "<wprt:GetPrinterElementsRequest><wprt:RequestedElements><wprt:Name>wprt:PrinterDescription</wprt:Name></wprt:RequestedElements></wprt:GetPrinterElementsRequest>",
    );
    let head = format!(
        "POST {SERVICE} HTTP/1.1\r\nuser-AGENT: Probe/1.0\r\nHost: x\r\nX-Zed: 1\r\nx-alpha: 2\r\nX-Dup: a\r\nX-Dup: b\r\nContent-Type: application/soap+xml; charset=utf-8\r\nExpect: 100-continue\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        env.len()
    );
    let mut wire = head.into_bytes();
    wire.extend_from_slice(env.as_bytes());
    let reply = raw(addr, wire).await;
    assert!(
        String::from_utf8_lossy(&reply).contains("200 OK"),
        "{}",
        String::from_utf8_lossy(&reply)
    );
    let recs = records(&log(addr, "").await);
    assert_eq!(recs.len(), 1);
    let r = &recs[0];
    assert_eq!(r["request"]["headers_raw"], json!(true));
    let names: Vec<&str> = r["request"]["headers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h[0].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "user-AGENT",
            "Host",
            "X-Zed",
            "x-alpha",
            "X-Dup",
            "X-Dup",
            "Content-Type",
            "Expect",
            "Content-Length",
            "Connection"
        ]
    );
    assert_eq!(header(r, "x-dup"), Some("a"));
    assert_eq!(header(r, "expect"), Some("100-continue"));
    // the raw bytes of the request start with the wire head
    let seq = r["seq"].as_u64().unwrap();
    let rq = request(
        addr,
        "GET",
        &format!("/v1/requests/{seq}/raw"),
        vec![],
        vec![],
    )
    .await;
    assert!(String::from_utf8_lossy(&rq.body).contains("user-AGENT: Probe/1.0\r\n"));

    // a chunked request on a keep-alive connection, then a second request on it
    let chunk_head = format!(
        "POST {SERVICE} HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\nContent-Type: application/soap+xml\r\n\r\n"
    );
    let mut wire = chunk_head.into_bytes();
    for part in env.as_bytes().chunks(200) {
        wire.extend_from_slice(format!("{:x}\r\n", part.len()).as_bytes());
        wire.extend_from_slice(part);
        wire.extend_from_slice(b"\r\n");
    }
    wire.extend_from_slice(b"0\r\n\r\n");
    wire.extend_from_slice(
        format!(
            "POST {SERVICE} HTTP/1.1\r\nUser-Agent: second\r\nHost: x\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{env}",
            env.len()
        )
        .as_bytes(),
    );
    let reply = raw(addr, wire).await;
    assert_eq!(
        String::from_utf8_lossy(&reply).matches("200 OK").count(),
        2,
        "{}",
        String::from_utf8_lossy(&reply)
    );
    let recs = records(&log(addr, &format!("?since={seq}")).await);
    assert_eq!(recs.len(), 2);
    assert_eq!(header(&recs[0], "transfer-encoding"), Some("chunked"));
    assert_eq!(recs[0]["action"], "GetPrinterElements");
    assert_eq!(recs[0]["request"]["body"]["length"], json!(env.len()));
    assert_eq!(header(&recs[1], "user-agent"), Some("second"));
    assert_eq!(recs[1]["request"]["headers_raw"], json!(true));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ticket_substitutions_distinguish_asked_for_from_not_asked() {
    let (srv, _tmp) = start_with("mono-laser", |_| {}).await;
    let addr = srv.local_addr();
    let asked = "<wprt:CreatePrintJobRequest><wprt:PrintTicket><wprt:JobDescription><wprt:JobName>asks</wprt:JobName><wprt:JobOriginatingUserName>lab</wprt:JobOriginatingUserName></wprt:JobDescription><wprt:DocumentProcessing><wprt:MediaSizeName>iso_a0_841x1189mm</wprt:MediaSizeName><wprt:Sides>TwoSidedLongEdge</wprt:Sides></wprt:DocumentProcessing></wprt:PrintTicket></wprt:CreatePrintJobRequest>";
    let ja = job_id_of(
        &soap(
            addr,
            SERVICE,
            envelope(&print_action("CreatePrintJob"), "", asked),
        )
        .await,
    );
    let jn = job_id_of(
        &soap(
            addr,
            SERVICE,
            envelope(&print_action("CreatePrintJob"), "", &ticket_body("silent")),
        )
        .await,
    );
    let recs = records(&log(addr, "?action=CreatePrintJob").await);
    assert_eq!(recs.len(), 2);
    let a = &recs[0]["ticket"];
    assert!(a["received"]
        .as_str()
        .unwrap()
        .contains("iso_a0_841x1189mm"));
    let subs = a["substitutions"].as_array().unwrap();
    let by: std::collections::BTreeMap<_, _> = subs
        .iter()
        .map(|s| (s["field"].as_str().unwrap().to_string(), s.clone()))
        .collect();
    assert!(by.contains_key("MediaSizeName"), "{subs:?}");
    assert_eq!(by["MediaSizeName"]["requested"], "iso_a0_841x1189mm");
    assert!(by["MediaSizeName"]["reason"].as_str().unwrap().len() > 3);
    assert!(by.contains_key("Sides"), "{subs:?}");
    // the silent ticket: nothing received for DocumentProcessing, no substitutions
    let n = &recs[1]["ticket"];
    assert!(!n["received"].as_str().unwrap().contains("MediaSizeName"));
    assert_eq!(n["substitutions"], json!([]));
    // the effective ticket looks the same size for both; the log tells them apart
    let jobs = get_json(addr, "/v1/jobs").await;
    let rows = jobs["print_jobs"].as_array().unwrap();
    let row = |id: u32| rows.iter().find(|r| r["id"] == json!(id)).unwrap().clone();
    assert!(!row(ja)["ticket_substitutions"]
        .as_array()
        .unwrap()
        .is_empty());
    assert!(row(jn)["ticket_substitutions"]
        .as_array()
        .unwrap()
        .is_empty());
    assert_eq!(
        row(ja)["ticket"]["document"]["media_size_name"],
        row(jn)["ticket"]["document"]["media_size_name"]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fault_injected_and_dropped_requests_are_recorded_with_their_rule() {
    let (srv, _tmp) = start(|_| {}).await;
    let addr = srv.local_addr();
    let armed = post_json(
        addr,
        "/v1/faults",
        &json!({"inject": [
            {"op": "GetPrinterElements", "effect": {"type": "fault", "subcode": "wprt:ServerErrorTemporaryError"}},
            {"op": "GetJobElements", "effect": {"type": "close_connection"}}
        ]}),
    )
    .await;
    assert_eq!(armed.status, 200, "{}", armed.text());
    let r = soap(
        addr,
        SERVICE,
        envelope(
            &print_action("GetPrinterElements"),
            "",
            "<wprt:GetPrinterElementsRequest><wprt:RequestedElements><wprt:Name>wprt:PrinterDescription</wprt:Name></wprt:RequestedElements></wprt:GetPrinterElementsRequest>",
        ),
    )
    .await;
    assert_ne!(r.status, 200);
    let dropped = raw(
        addr,
        {
            let env = envelope(
                &print_action("GetJobElements"),
                "",
                "<wprt:GetJobElementsRequest><wprt:JobId>1</wprt:JobId></wprt:GetJobElementsRequest>",
            );
            format!(
                "POST {SERVICE} HTTP/1.1\r\nHost: x\r\nContent-Type: application/soap+xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{env}",
                env.len()
            )
            .into_bytes()
        },
    )
    .await;
    assert!(!String::from_utf8_lossy(&dropped).contains("HTTP/1.1 200"));
    let ok = soap(
        addr,
        SERVICE,
        envelope(&print_action("CreatePrintJob"), "", &ticket_body("after")),
    )
    .await;
    assert_eq!(ok.status, 200);
    let recs = records(&log(addr, "").await);
    assert_eq!(recs.len(), 3, "{recs:?}");
    assert_eq!(recs[0]["result"], "faulted");
    assert!(recs[0]["fault_rule"]
        .as_str()
        .unwrap()
        .starts_with("GetPrinterElements[0]:fault"));
    assert_eq!(recs[0]["response"]["status"], json!(r.status));
    assert_eq!(recs[1]["result"], "dropped");
    assert!(recs[1]["fault_rule"]
        .as_str()
        .unwrap()
        .contains("close_connection"));
    assert_eq!(recs[1]["response"]["status"], Value::Null);
    assert_eq!(recs[2]["result"], "ok");
    assert_eq!(recs[2]["fault_rule"], Value::Null);
}

// ---- IPP -------------------------------------------------------------------------------------

fn ipp_attr(tag: u8, name: &str, value: &[u8]) -> Vec<u8> {
    let mut v = vec![tag];
    v.extend_from_slice(&(name.len() as u16).to_be_bytes());
    v.extend_from_slice(name.as_bytes());
    v.extend_from_slice(&(value.len() as u16).to_be_bytes());
    v.extend_from_slice(value);
    v
}

fn print_job(uri: &str, request_id: u32, job_attrs: &[Vec<u8>], data: &[u8]) -> Vec<u8> {
    let mut b = vec![2, 0, 0, 2];
    b.extend_from_slice(&request_id.to_be_bytes());
    b.push(1);
    b.extend(ipp_attr(0x47, "attributes-charset", b"utf-8"));
    b.extend(ipp_attr(0x48, "attributes-natural-language", b"en"));
    b.extend(ipp_attr(0x45, "printer-uri", uri.as_bytes()));
    b.extend(ipp_attr(0x42, "requesting-user-name", b"tester"));
    b.extend(ipp_attr(0x42, "job-name", b"ipp-log"));
    if !job_attrs.is_empty() {
        b.push(2);
        for a in job_attrs {
            b.extend_from_slice(a);
        }
    }
    b.push(3);
    b.extend_from_slice(data);
    b
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ipp_print_job_is_decoded_with_attributes_and_unsupported_ones() {
    let (srv, _tmp) = start(|_| {}).await;
    let addr = srv.local_addr();
    let uri = format!("ipp://127.0.0.1:{}/ipp/print", addr.port());
    let body = print_job(
        &uri,
        77,
        &[
            ipp_attr(0x21, "copies", &2i32.to_be_bytes()),
            ipp_attr(0x44, "sides", b"two-sided-long-edge-not-real"),
        ],
        b"%PDF-1.4 fake document",
    );
    let r = request(
        addr,
        "POST",
        "/ipp/print",
        vec![
            ("Content-Type".into(), "application/ipp".into()),
            ("SOAPAction".into(), "\"\"".into()),
        ],
        body,
    )
    .await;
    assert_eq!(r.status, 200);
    let recs = records(&log(addr, "?action=print-job").await);
    assert_eq!(recs.len(), 1, "{}", log(addr, "").await);
    let p = &recs[0];
    assert_eq!(p["protocol"], "ipp");
    assert_eq!(p["ipp_operation"], "Print-Job");
    assert_eq!(p["ipp_operation_id"], 2);
    assert_eq!(p["request_id"], 77);
    assert_eq!(p["request"]["body"]["document"], json!(true));
    assert_eq!(
        header(p, "soapaction"),
        Some("\"\""),
        "craft's SOAPAction on IPP is visible"
    );
    let job = p["job_id"].as_u64().expect("job id from the reply");
    assert!(job >= 1);
    let names: Vec<&str> = p["ipp"]["request_attrs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["name"].as_str().unwrap())
        .collect();
    for want in [
        "attributes-charset",
        "printer-uri",
        "job-name",
        "copies",
        "sides",
    ] {
        assert!(names.contains(&want), "{want} in {names:?}");
    }
    assert!(
        p["ipp_status"].as_str().unwrap().contains("successful"),
        "{}",
        p["ipp_status"]
    );
    let subs = p["ticket"]["substitutions"].as_array().unwrap();
    assert!(subs.iter().any(|s| s["field"] == "sides"), "{subs:?}");
    // an unrelated GET shows nothing; the job row links to the request
    let jobs = get_json(addr, "/v1/jobs").await;
    let row = jobs["print_jobs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == json!(job))
        .unwrap();
    assert!(row["request_seqs"].as_array().unwrap().contains(&p["seq"]));
    // the ipp document is not an XML ticket
    assert!(row["ticket_received"].is_null());
}

// ---- bounds ----------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_ring_drops_the_oldest_and_seq_stays_monotonic() {
    let (srv, _tmp) = start(|cfg| cfg.request_log.capacity = 4).await;
    let addr = srv.local_addr();
    for i in 0..10 {
        let r = soap(
            addr,
            SERVICE,
            envelope(
                &print_action("CreatePrintJob"),
                "",
                &ticket_body(&format!("j{i}")),
            ),
        )
        .await;
        assert_eq!(r.status, 200);
    }
    let l = log(addr, "").await;
    let seqs: Vec<u64> = records(&l)
        .iter()
        .map(|r| r["seq"].as_u64().unwrap())
        .collect();
    assert_eq!(seqs, [7, 8, 9, 10]);
    assert_eq!(l["dropped"], 6);
    assert_eq!(l["next_seq"], 11);
    // clear keeps counting
    let cleared = request(addr, "DELETE", "/v1/requests", vec![], vec![]).await;
    assert_eq!(cleared.status, 200);
    assert_eq!(cleared.json()["cleared"], 4);
    assert!(records(&log(addr, "").await).is_empty());
    soap(
        addr,
        SERVICE,
        envelope(&print_action("CreatePrintJob"), "", &ticket_body("n")),
    )
    .await;
    let seqs: Vec<u64> = records(&log(addr, "").await)
        .iter()
        .map(|r| r["seq"].as_u64().unwrap())
        .collect();
    assert_eq!(seqs, [11]);
    // a job row keeps its ticket after the ring forgot the records, without dead links
    let jobs = get_json(addr, "/v1/jobs").await;
    let first = jobs["print_jobs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == json!(1))
        .unwrap();
    assert!(first["ticket_received"].as_str().unwrap().contains("j0"));
    assert_eq!(first["request_seqs"], json!([]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn body_caps_set_the_truncated_flags_for_request_and_reply() {
    let (srv, _tmp) = start(|cfg| cfg.request_log.body_cap = 100).await;
    let addr = srv.local_addr();
    let env = envelope(&print_action("CreatePrintJob"), "", &ticket_body("capped"));
    let r = soap(addr, SERVICE, env.clone()).await;
    assert_eq!(r.status, 200);
    let rec = &records(&log(addr, "").await)[0];
    let (q, a) = (&rec["request"]["body"], &rec["response"]["body"]);
    assert_eq!(q["stored"], 100);
    assert_eq!(q["length"], json!(env.len()));
    assert_eq!(q["truncated"], true);
    assert_eq!(q["sha256"], json!(sha256_hex(env.as_bytes())));
    assert_eq!(a["stored"], 100);
    assert_eq!(a["length"], json!(r.body.len()));
    assert_eq!(a["truncated"], true);
    // below the cap: nothing truncated
    let post = post_json(addr, "/v1/requests/config", &json!({"body_cap": 1_000_000})).await;
    assert_eq!(post.json()["body_cap"], 1_000_000);
    soap(addr, SERVICE, env.clone()).await;
    let recs = records(&log(addr, "").await);
    assert_eq!(recs.last().unwrap()["request"]["body"]["truncated"], false);
    // the ticket is cut from the whole body, not from the stored head
    assert!(recs[0]["ticket"]["received"]
        .as_str()
        .unwrap()
        .ends_with("</wprt:PrintTicket>"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_clients_get_unique_ordered_seqs_and_nothing_is_lost() {
    let (srv, _tmp) = start(|cfg| cfg.request_log.capacity = 1000).await;
    let addr = srv.local_addr();
    let mut tasks = Vec::new();
    for _ in 0..8 {
        tasks.push(tokio::spawn(async move {
            for _ in 0..10 {
                let r = soap(
                    addr,
                    SERVICE,
                    envelope(
                        &print_action("GetPrinterElements"),
                        "",
                        "<wprt:GetPrinterElementsRequest><wprt:RequestedElements><wprt:Name>wprt:PrinterDescription</wprt:Name></wprt:RequestedElements></wprt:GetPrinterElementsRequest>",
                    ),
                )
                .await;
                assert_eq!(r.status, 200);
            }
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    let l = log(addr, "?limit=1000").await;
    let seqs: Vec<u64> = records(&l)
        .iter()
        .map(|r| r["seq"].as_u64().unwrap())
        .collect();
    assert_eq!(seqs.len(), 80);
    assert!(seqs.windows(2).all(|w| w[0] < w[1]), "ordered and unique");
    assert_eq!(l["in_flight"], 0);
    let mut ids: Vec<String> = records(&l)
        .iter()
        .map(|r| r["message_id"].as_str().unwrap().to_string())
        .collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), 80, "every request is its own record");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn disabled_log_records_nothing_and_can_be_switched_on_at_run_time() {
    let (srv, _tmp) = start(|cfg| cfg.request_log = RequestLogConfig::off()).await;
    let addr = srv.local_addr();
    let r = soap(
        addr,
        SERVICE,
        envelope(&print_action("CreatePrintJob"), "", &ticket_body("x")),
    )
    .await;
    assert_eq!(r.status, 200);
    let l = log(addr, "").await;
    assert_eq!(l["config"]["enabled"], false);
    assert!(records(&l).is_empty());
    let jobs = get_json(addr, "/v1/jobs").await;
    assert!(jobs["print_jobs"][0]["ticket_received"].is_null());
    let on = post_json(
        addr,
        "/v1/requests/config",
        &json!({"enabled": true, "capacity": 8}),
    )
    .await;
    assert_eq!(on.json()["enabled"], true);
    soap(
        addr,
        SERVICE,
        envelope(&print_action("CreatePrintJob"), "", &ticket_body("y")),
    )
    .await;
    assert_eq!(records(&log(addr, "").await).len(), 1);
    // capacity 0 means off
    let off = post_json(addr, "/v1/requests/config", &json!({"capacity": 0})).await;
    assert_eq!(off.json()["enabled"], false);
    let bad = post_json(addr, "/v1/requests/config", &json!({"nonsense": 1})).await;
    assert_eq!(bad.status, 400);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn redaction_replaces_the_peer_and_credential_headers() {
    let (srv, _tmp) = start(|cfg| cfg.request_log.redact = true).await;
    let addr = srv.local_addr();
    let r = request(
        addr,
        "POST",
        SERVICE,
        vec![
            ("Content-Type".into(), "application/soap+xml".into()),
            ("Authorization".into(), "Basic c2VjcmV0".into()),
        ],
        envelope(&print_action("CreatePrintJob"), "", &ticket_body("r")).into_bytes(),
    )
    .await;
    assert_eq!(r.status, 200);
    let rec = &records(&log(addr, "").await)[0];
    assert_eq!(rec["peer"], "redacted");
    assert_eq!(header(rec, "authorization"), Some("redacted"));
    let seq = rec["seq"].as_u64().unwrap();
    let raw = request(
        addr,
        "GET",
        &format!("/v1/requests/{seq}/raw"),
        vec![],
        vec![],
    )
    .await;
    assert!(!String::from_utf8_lossy(&raw.body).contains("c2VjcmV0"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn peers_are_loopback_and_unredacted_by_default() {
    let (srv, _tmp) = start(|_| {}).await;
    let addr = srv.local_addr();
    soap(
        addr,
        SERVICE,
        envelope(&print_action("CreatePrintJob"), "", &ticket_body("p")),
    )
    .await;
    let rec = &records(&log(addr, "").await)[0];
    assert!(rec["peer"].as_str().unwrap().starts_with("127.0.0.1:"));
    assert!(rec["t_mono_ms"].as_f64().unwrap() >= 0.0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn control_requests_are_not_logged_and_a_reset_clears_the_log() {
    let (srv, _tmp) = start(|_| {}).await;
    let addr = srv.local_addr();
    get_json(addr, "/v1/status").await;
    get_json(addr, "/v1/jobs").await;
    assert!(records(&log(addr, "").await).is_empty());
    soap(
        addr,
        SERVICE,
        envelope(&print_action("CreatePrintJob"), "", &ticket_body("z")),
    )
    .await;
    assert_eq!(records(&log(addr, "").await).len(), 1);
    let r = request(addr, "POST", "/v1/reset", vec![], vec![]).await;
    assert_eq!(r.status, 200);
    assert!(records(&log(addr, "").await).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn each_device_has_its_own_log_and_the_original_path_is_kept() {
    let (srv, _tmp) = start(|cfg| {
        cfg.profile_dir = Some(profiles_dir());
        cfg.devices = vec![
            fakeprint::server::DeviceConfig::derived("one", "generic-wsd"),
            fakeprint::server::DeviceConfig::derived("two", "mono-laser"),
        ];
    })
    .await;
    let addr = srv.local_addr();
    let path_two = "/d/two/WebServices/PrinterService";
    let r = soap(
        addr,
        path_two,
        envelope(&print_action("CreatePrintJob"), "", &ticket_body("two")),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.text());
    let two = records(&get_json(addr, "/d/two/v1/requests").await);
    assert_eq!(two.len(), 1);
    assert_eq!(two[0]["path"], path_two);
    assert!(records(&get_json(addr, "/d/one/v1/requests").await).is_empty());
    assert!(
        records(&log(addr, "").await).is_empty(),
        "the default device is 'one'"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn event_deliveries_are_logged_as_outbound_records() {
    let (srv, _tmp) = start(|_| {}).await;
    let addr = srv.local_addr();
    let rec = Recorder::start();
    let sub = soap(
        addr,
        SERVICE,
        envelope(
            "http://schemas.xmlsoap.org/ws/2004/08/eventing/Subscribe",
            "",
            &format!(
                "<wse:Subscribe><wse:Delivery><wse:NotifyTo><wsa:Address>{}</wsa:Address></wse:NotifyTo></wse:Delivery></wse:Subscribe>",
                rec.url()
            ),
        ),
    )
    .await;
    subscription_id(&sub);
    let job = job_id_of(
        &soap(
            addr,
            SERVICE,
            envelope(&print_action("CreatePrintJob"), "", &ticket_body("ev")),
        )
        .await,
    );
    soap(addr, SERVICE, inline_send(job, true)).await;
    rec.wait_for("a job event", Duration::from_secs(10), |got| {
        !got.is_empty()
    })
    .await;
    let out = records(&log(addr, "?direction=out").await);
    assert!(!out.is_empty(), "{}", log(addr, "").await);
    let o = &out[0];
    assert_eq!(o["direction"], "out");
    assert_eq!(o["protocol"], "event");
    assert_eq!(o["method"], "POST");
    assert_eq!(o["path"], "/notify");
    assert_eq!(o["response"]["status"], 200);
    assert_eq!(o["result"], "ok");
    assert!(o["request"]["body"]["text"]
        .as_str()
        .unwrap()
        .contains(ns::PRINT));
    assert!(header(o, "content-type").unwrap().contains("action="));
    assert_eq!(srv.validation_problems(), Vec::<String>::new());
}
