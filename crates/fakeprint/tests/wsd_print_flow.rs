//! Integration: GetPrinterElements + CreatePrintJob + SendDocument → inbox.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use fakeprint::discovery::DiscoveryStats;
use fakeprint::profile::PrinterProfile;
use fakeprint::server::{router, RouterOptions};
use fakeprint::state::{World, WorldOptions};
use http_body_util::BodyExt;
use tokio::sync::RwLock;
use tower::ServiceExt;
use uuid::Uuid;

fn test_world(inbox: std::path::PathBuf) -> Arc<RwLock<World>> {
    let profiles = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../profiles");
    let profile = PrinterProfile::load(&profiles, "generic-wsd").expect("load generic-wsd");
    let options = WorldOptions {
        validate_replies: Some(true),
        ..WorldOptions::default()
    };
    let mut w = World::with_options(
        profiles,
        inbox,
        profile,
        Arc::new(DiscoveryStats::default()),
        options,
    )
    .unwrap();
    w.set_identity(
        "http://127.0.0.1:3921/WebServices/Device".into(),
        "http://127.0.0.1:3921/WebServices/PrinterService".into(),
        Uuid::parse_str("00000000-0000-4000-8000-0000000000f3").unwrap(),
    );
    Arc::new(RwLock::new(w))
}

fn app(world: Arc<RwLock<World>>) -> Router {
    router(
        world,
        &RouterOptions {
            wsd: true,
            ipp: false,
            directed_discovery: None,
            trace: false,
        },
    )
}

/// Every reply the server sent passed its own schema validator.
async fn assert_valid(world: &Arc<RwLock<World>>) {
    let ctx = world.read().await.print_ctx();
    assert!(ctx.validator.checked() > 0, "the validator was never run");
    assert_eq!(ctx.validation_problems(), Vec::<String>::new());
}

async fn post_exchange(app: &Router, path: &str, body: &str) -> (StatusCode, String) {
    let req = Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/soap+xml")
        .extension(axum::extract::ConnectInfo(
            "127.0.0.1:9".parse::<SocketAddr>().unwrap(),
        ))
        .body(Body::from(body.to_string()))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let body = String::from_utf8_lossy(&bytes).into_owned();
    (status, body)
}

/// Envelope headers of a reply; the body is checked by the server's own reply validator
/// (switched on in `test_world`) and asserted at the end of each test.
fn assert_schema(body: &str) {
    let problems = fakeprint::wsd::soap::reply_problems(body);
    assert!(
        problems.is_empty(),
        "response envelope: {problems:?}\n{body}"
    );
}

async fn post_soap(app: &Router, path: &str, body: &str) -> (StatusCode, String) {
    let (status, body) = post_exchange(app, path, body).await;
    assert_schema(&body);
    (status, body)
}

async fn post_raw(
    app: &Router,
    path: &str,
    content_type: &str,
    body: Vec<u8>,
) -> (StatusCode, String) {
    let req = Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", content_type)
        .extension(axum::extract::ConnectInfo(
            "127.0.0.1:9".parse::<SocketAddr>().unwrap(),
        ))
        .body(Body::from(body))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

#[tokio::test]
async fn get_printer_elements_and_capture_job() {
    let inbox = std::env::temp_dir().join(format!("fp-itest-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&inbox).unwrap();
    let world = test_world(inbox.clone());
    let app = app(world.clone());

    let get_body = r#"<?xml version="1.0"?>
<soap:Envelope xmlns:soap="http://www.w3.org/2003/05/soap-envelope"
               xmlns:wsa="http://schemas.xmlsoap.org/ws/2004/08/addressing"
               xmlns:wprt="http://schemas.microsoft.com/windows/2006/08/wdp/print">
  <soap:Header>
    <wsa:Action>http://schemas.microsoft.com/windows/2006/08/wdp/print/GetPrinterElements</wsa:Action>
    <wsa:MessageID>urn:uuid:11111111-1111-1111-1111-111111111111</wsa:MessageID>
  </soap:Header>
  <soap:Body>
    <wprt:GetPrinterElementsRequest>
      <wprt:RequestedElements>
        <wprt:Name>wprt:PrinterDescription</wprt:Name>
        <wprt:Name>wprt:PrinterCapabilities</wprt:Name>
        <wprt:Name>wprt:DefaultPrintTicket</wprt:Name>
        <wprt:Name>wprt:PrinterStatus</wprt:Name>
      </wprt:RequestedElements>
    </wprt:GetPrinterElementsRequest>
  </soap:Body>
</soap:Envelope>"#;
    let (st, xml) = post_soap(&app, "/WebServices/PrinterService", get_body).await;
    assert_eq!(st, StatusCode::OK);
    assert!(xml.contains("GetPrinterElementsResponse"), "{xml}");
    assert!(xml.contains("Fake WSD printer"), "{xml}");
    assert!(xml.contains("JobValues"), "{xml}");
    assert!(xml.contains("DefaultPrintTicket"), "{xml}");
    assert!(xml.contains("PrinterCurrentTime"), "{xml}");

    let validate = r#"<?xml version="1.0"?>
<soap:Envelope xmlns:soap="http://www.w3.org/2003/05/soap-envelope"
               xmlns:wsa="http://schemas.xmlsoap.org/ws/2004/08/addressing"
               xmlns:wprt="http://schemas.microsoft.com/windows/2006/08/wdp/print">
  <soap:Header>
    <wsa:Action>http://schemas.microsoft.com/windows/2006/08/wdp/print/ValidatePrintTicket</wsa:Action>
    <wsa:MessageID>urn:uuid:aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa</wsa:MessageID>
  </soap:Header>
  <soap:Body>
    <wprt:ValidatePrintTicketRequest>
      <wprt:PrintTicket>
        <wprt:JobDescription>
          <wprt:JobName>check</wprt:JobName>
        </wprt:JobDescription>
      </wprt:PrintTicket>
    </wprt:ValidatePrintTicketRequest>
  </soap:Body>
</soap:Envelope>"#;
    // ValidatePrintTicket does not exist in any WS-Print version: ActionNotSupported.
    let (st, xml) = post_exchange(&app, "/WebServices/PrinterService", validate).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert!(xml.contains("ActionNotSupported"), "{xml}");
    let problems = fakeprint::wsd::soap::reply_problems(&xml);
    assert!(
        problems.is_empty(),
        "{problems:?}
{xml}"
    );

    let create = r#"<?xml version="1.0"?>
<soap:Envelope xmlns:soap="http://www.w3.org/2003/05/soap-envelope"
               xmlns:wsa="http://schemas.xmlsoap.org/ws/2004/08/addressing"
               xmlns:wprt="http://schemas.microsoft.com/windows/2006/08/wdp/print">
  <soap:Header>
    <wsa:Action>http://schemas.microsoft.com/windows/2006/08/wdp/print/CreatePrintJob</wsa:Action>
    <wsa:MessageID>urn:uuid:22222222-2222-2222-2222-222222222222</wsa:MessageID>
  </soap:Header>
  <soap:Body>
    <wprt:CreatePrintJobRequest>
      <wprt:PrintTicket>
        <wprt:JobDescription>
          <wprt:JobName>lab-page</wprt:JobName>
          <wprt:JobOriginatingUserName>lab</wprt:JobOriginatingUserName>
        </wprt:JobDescription>
      </wprt:PrintTicket>
    </wprt:CreatePrintJobRequest>
  </soap:Body>
</soap:Envelope>"#;
    let (st, xml) = post_soap(&app, "/WebServices/PrinterService", create).await;
    assert_eq!(st, StatusCode::OK);
    assert!(xml.contains("<wprt:JobId>1</wprt:JobId>"), "{xml}");
    // JobToken is a lab quirk and off by default: the print schema has no such element.
    assert!(!xml.contains("JobToken"), "{xml}");

    let job_el = r#"<?xml version="1.0"?>
<soap:Envelope xmlns:soap="http://www.w3.org/2003/05/soap-envelope"
               xmlns:wsa="http://schemas.xmlsoap.org/ws/2004/08/addressing"
               xmlns:wprt="http://schemas.microsoft.com/windows/2006/08/wdp/print">
  <soap:Header>
    <wsa:Action>http://schemas.microsoft.com/windows/2006/08/wdp/print/GetJobElements</wsa:Action>
    <wsa:MessageID>urn:uuid:bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb</wsa:MessageID>
  </soap:Header>
  <soap:Body>
    <wprt:GetJobElementsRequest>
      <wprt:JobId>1</wprt:JobId>
      <wprt:RequestedElements>
        <wprt:Name>wprt:JobStatus</wprt:Name>
      </wprt:RequestedElements>
    </wprt:GetJobElementsRequest>
  </soap:Body>
</soap:Envelope>"#;
    let (st, xml) = post_soap(&app, "/WebServices/PrinterService", job_el).await;
    assert_eq!(st, StatusCode::OK);
    assert!(xml.contains("JobState>Pending"), "{xml}");

    let send = r#"<?xml version="1.0"?>
<soap:Envelope xmlns:soap="http://www.w3.org/2003/05/soap-envelope"
               xmlns:wsa="http://schemas.xmlsoap.org/ws/2004/08/addressing"
               xmlns:wprt="http://schemas.microsoft.com/windows/2006/08/wdp/print">
  <soap:Header>
    <wsa:Action>http://schemas.microsoft.com/windows/2006/08/wdp/print/SendDocument</wsa:Action>
    <wsa:MessageID>urn:uuid:33333333-3333-3333-3333-333333333333</wsa:MessageID>
  </soap:Header>
  <soap:Body>
    <wprt:SendDocumentRequest>
      <wprt:JobId>1</wprt:JobId>
      <wprt:DocumentDescription>
        <wprt:DocumentId>1</wprt:DocumentId>
        <wprt:Compression>None</wprt:Compression>
        <wprt:Format>application/pdf</wprt:Format>
      </wprt:DocumentDescription>
      <wprt:LastDocument>true</wprt:LastDocument>
      <wprt:DocumentData>JVBERi0xLjQ=</wprt:DocumentData>
    </wprt:SendDocumentRequest>
  </soap:Body>
</soap:Envelope>"#;
    let (st, xml) = post_soap(&app, "/WebServices/PrinterService", send).await;
    assert_eq!(st, StatusCode::OK);
    assert!(xml.contains("SendDocumentResponse"), "{xml}");

    let captured = world.read().await.captured().to_vec();
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0].bytes, 8); // "%PDF-1.4"
    let data = std::fs::read(&captured[0].path).unwrap();
    assert_eq!(&data[..4], b"%PDF");

    assert_valid(&world).await;
    let _ = std::fs::remove_dir_all(inbox);
}

#[tokio::test]
async fn send_document_mtom_xop_include() {
    let inbox = std::env::temp_dir().join(format!("fp-mtom-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&inbox).unwrap();
    let world = test_world(inbox.clone());
    let app = app(world.clone());

    let create = r#"<?xml version="1.0"?>
<soap:Envelope xmlns:soap="http://www.w3.org/2003/05/soap-envelope"
               xmlns:wsa="http://schemas.xmlsoap.org/ws/2004/08/addressing"
               xmlns:wprt="http://schemas.microsoft.com/windows/2006/08/wdp/print">
  <soap:Header>
    <wsa:Action>http://schemas.microsoft.com/windows/2006/08/wdp/print/CreatePrintJob</wsa:Action>
    <wsa:MessageID>urn:uuid:44444444-4444-4444-4444-444444444444</wsa:MessageID>
  </soap:Header>
  <soap:Body>
    <wprt:CreatePrintJobRequest>
      <wprt:PrintTicket>
        <wprt:JobDescription><wprt:JobName>mtom</wprt:JobName><wprt:JobOriginatingUserName>lab</wprt:JobOriginatingUserName></wprt:JobDescription>
      </wprt:PrintTicket>
    </wprt:CreatePrintJobRequest>
  </soap:Body>
</soap:Envelope>"#;
    let (st, _) = post_soap(&app, "/WebServices/PrinterService", create).await;
    assert_eq!(st, StatusCode::OK);

    let pdf = b"%PDF-1.4 mtom-lab";
    let mut body = Vec::new();
    body.extend_from_slice(b"--uuid:bound\r\n");
    body.extend_from_slice(
        b"Content-Type: application/xop+xml; charset=utf-8; type=\"application/soap+xml\"\r\n\r\n",
    );
    body.extend_from_slice(
        br#"<?xml version="1.0"?>
<soap:Envelope xmlns:soap="http://www.w3.org/2003/05/soap-envelope"
               xmlns:wsa="http://schemas.xmlsoap.org/ws/2004/08/addressing"
               xmlns:wprt="http://schemas.microsoft.com/windows/2006/08/wdp/print"
               xmlns:xop="http://www.w3.org/2004/08/xop/include">
  <soap:Header>
    <wsa:Action>http://schemas.microsoft.com/windows/2006/08/wdp/print/SendDocument</wsa:Action>
    <wsa:MessageID>urn:uuid:55555555-5555-5555-5555-555555555555</wsa:MessageID>
  </soap:Header>
  <soap:Body>
    <wprt:SendDocumentRequest>
      <wprt:JobId>1</wprt:JobId>
      <wprt:DocumentDescription>
        <wprt:DocumentId>1</wprt:DocumentId>
        <wprt:Compression>None</wprt:Compression>
        <wprt:Format>application/pdf</wprt:Format>
      </wprt:DocumentDescription>
      <wprt:LastDocument>true</wprt:LastDocument>
      <wprt:DocumentData><xop:Include href="cid:doc@fakeprint"/></wprt:DocumentData>
    </wprt:SendDocumentRequest>
  </soap:Body>
</soap:Envelope>"#,
    );
    body.extend_from_slice(b"\r\n--uuid:bound\r\n");
    body.extend_from_slice(b"Content-Type: application/pdf\r\n");
    body.extend_from_slice(b"Content-ID: <doc@fakeprint>\r\n");
    body.extend_from_slice(b"Content-Transfer-Encoding: binary\r\n\r\n");
    body.extend_from_slice(pdf);
    body.extend_from_slice(b"\r\n--uuid:bound--\r\n");

    let ct = "multipart/related; boundary=\"uuid:bound\"; type=\"application/xop+xml\"; start-info=\"application/soap+xml\"";
    let (st, xml) = post_raw(&app, "/WebServices/PrinterService", ct, body).await;
    assert_eq!(st, StatusCode::OK);
    assert!(xml.contains("SendDocumentResponse"), "{xml}");

    let captured = world.read().await.captured().to_vec();
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0].bytes, pdf.len());
    assert_eq!(std::fs::read(&captured[0].path).unwrap(), pdf);

    assert_valid(&world).await;
    let _ = std::fs::remove_dir_all(inbox);
}

#[tokio::test]
async fn print_v1_accepts_the_2006_01_namespace() {
    let inbox = std::env::temp_dir().join(format!("fp-ns-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&inbox).unwrap();
    let world = test_world(inbox.clone());
    let app = app(world.clone());
    let get_body = r#"<?xml version="1.0"?>
<soap:Envelope xmlns:soap="http://www.w3.org/2003/05/soap-envelope"
               xmlns:wsa="http://schemas.xmlsoap.org/ws/2004/08/addressing"
               xmlns:wprt="http://schemas.microsoft.com/windows/2006/01/wdp/print">
  <soap:Header>
    <wsa:Action>http://schemas.microsoft.com/windows/2006/01/wdp/print/GetPrinterElements</wsa:Action>
    <wsa:MessageID>urn:uuid:01010101-0101-4101-8101-010101010101</wsa:MessageID>
  </soap:Header>
  <soap:Body>
    <wprt:GetPrinterElementsRequest>
      <wprt:RequestedElements>
        <wprt:Name>wprt:PrinterDescription</wprt:Name>
        <wprt:Name>wprt:PrinterCapabilities</wprt:Name>
      </wprt:RequestedElements>
    </wprt:GetPrinterElementsRequest>
  </soap:Body>
</soap:Envelope>"#;
    let (st, xml) = post_soap(&app, "/WebServices/PrinterService", get_body).await;
    assert_eq!(st, StatusCode::OK);
    assert!(xml.contains("GetPrinterElementsResponse"), "{xml}");
    assert!(xml.contains("ColorSupported"), "{xml}");

    let create = r#"<?xml version="1.0"?>
<soap:Envelope xmlns:soap="http://www.w3.org/2003/05/soap-envelope"
               xmlns:wsa="http://schemas.xmlsoap.org/ws/2004/08/addressing"
               xmlns:wprt="http://schemas.microsoft.com/windows/2006/01/wdp/print">
  <soap:Header>
    <wsa:Action>http://schemas.microsoft.com/windows/2006/01/wdp/print/CreatePrintJob</wsa:Action>
    <wsa:MessageID>urn:uuid:01010101-0101-4101-8101-010101010102</wsa:MessageID>
  </soap:Header>
  <soap:Body>
    <wprt:CreatePrintJobRequest>
      <wprt:PrintTicket>
        <wprt:JobDescription>
          <wprt:JobName>older-ns</wprt:JobName>
          <wprt:JobOriginatingUserName>lab</wprt:JobOriginatingUserName>
        </wprt:JobDescription>
      </wprt:PrintTicket>
    </wprt:CreatePrintJobRequest>
  </soap:Body>
</soap:Envelope>"#;
    let (st, xml) = post_soap(&app, "/WebServices/PrinterService", create).await;
    assert_eq!(st, StatusCode::OK);
    assert!(xml.contains("<wprt:JobId>1</wprt:JobId>"), "{xml}");
    assert_valid(&world).await;
    let _ = std::fs::remove_dir_all(inbox);
}

#[tokio::test]
async fn later_print_versions_are_not_advertised() {
    let inbox = std::env::temp_dir().join(format!("fp-ver-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&inbox).unwrap();
    let world = test_world(inbox.clone());
    let app = app(world.clone());
    for (action, phrase) in [
        (
            "http://schemas.microsoft.com/windows/2012/10/wdp/printV12/SetPrinterElements",
            "ActionNotSupported",
        ),
        (
            "http://schemas.microsoft.com/windows/2014/04/wdp/printV20/GetPrintDeviceCapabilities",
            "ActionNotSupported",
        ),
        (
            "http://schemas.microsoft.com/windows/2014/04/wdp/printV20/CreatePrintJob2",
            "ActionNotSupported",
        ),
    ] {
        let body = format!(
            r#"<?xml version="1.0"?>
<soap:Envelope xmlns:soap="http://www.w3.org/2003/05/soap-envelope"
               xmlns:wsa="http://schemas.xmlsoap.org/ws/2004/08/addressing">
  <soap:Header>
    <wsa:Action>{action}</wsa:Action>
    <wsa:MessageID>urn:uuid:02020202-0202-4202-8202-020202020202</wsa:MessageID>
  </soap:Header>
  <soap:Body></soap:Body>
</soap:Envelope>"#
        );
        let (st, xml) = post_exchange(&app, "/WebServices/PrinterService", &body).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{action} {xml}");
        assert!(xml.contains(phrase), "{action} {xml}");
        let problems = fakeprint::wsd::soap::reply_problems(&xml);
        assert!(problems.is_empty(), "{action} {problems:?}\n{xml}");
    }
    assert_valid(&world).await;
    let _ = std::fs::remove_dir_all(inbox);
}

fn soap_env(action_op: &str, body: &str) -> String {
    format!(
        r#"<?xml version="1.0"?>
<soap:Envelope xmlns:soap="http://www.w3.org/2003/05/soap-envelope"
               xmlns:wsa="http://schemas.xmlsoap.org/ws/2004/08/addressing"
               xmlns:wprt="http://schemas.microsoft.com/windows/2006/08/wdp/print">
  <soap:Header>
    <wsa:Action>http://schemas.microsoft.com/windows/2006/08/wdp/print/{action_op}</wsa:Action>
    <wsa:MessageID>urn:uuid:77777777-7777-4777-8777-777777777777</wsa:MessageID>
  </soap:Header>
  <soap:Body>{body}</soap:Body>
</soap:Envelope>"#
    )
}

const CREATE_BODY: &str = "<wprt:CreatePrintJobRequest><wprt:PrintTicket><wprt:JobDescription><wprt:JobName>big</wprt:JobName><wprt:JobOriginatingUserName>lab</wprt:JobOriginatingUserName></wprt:JobDescription></wprt:PrintTicket></wprt:CreatePrintJobRequest>";

fn send_body(job: u32, fmt: &str, last: bool, data: &str) -> String {
    format!("<wprt:SendDocumentRequest><wprt:JobId>{job}</wprt:JobId><wprt:DocumentDescription><wprt:DocumentId>1</wprt:DocumentId><wprt:Compression>None</wprt:Compression><wprt:Format>{fmt}</wprt:Format></wprt:DocumentDescription><wprt:LastDocument>{last}</wprt:LastDocument>{data}</wprt:SendDocumentRequest>")
}

#[tokio::test]
async fn large_mtom_document_is_not_cut_off_by_the_default_body_limit() {
    let inbox = std::env::temp_dir().join(format!("fp-big-{}", Uuid::new_v4()));
    let world = test_world(inbox.clone());
    let app = app(world.clone());
    let (st, _) = post_soap(
        &app,
        "/WebServices/PrinterService",
        &soap_env("CreatePrintJob", CREATE_BODY),
    )
    .await;
    assert_eq!(st, StatusCode::OK);

    let doc = vec![0x41u8; 5 << 20]; // 5 MiB, above axum's 2 MiB default
    let env = soap_env(
        "SendDocument",
        &send_body(1, "application/octet-stream", true, "<wprt:DocumentData><xop:Include xmlns:xop=\"http://www.w3.org/2004/08/xop/include\" href=\"cid:d@x\"/></wprt:DocumentData>"),
    );
    let mut body = Vec::new();
    body.extend_from_slice(b"--B\r\nContent-Type: application/xop+xml; charset=utf-8; type=\"application/soap+xml\"\r\n\r\n");
    body.extend_from_slice(env.as_bytes());
    body.extend_from_slice(b"\r\n--B\r\nContent-Type: application/octet-stream\r\nContent-ID: <d@x>\r\nContent-Transfer-Encoding: binary\r\n\r\n");
    body.extend_from_slice(&doc);
    body.extend_from_slice(b"\r\n--B--\r\n");
    let ct = "multipart/related; boundary=\"B\"; type=\"application/xop+xml\"; start-info=\"application/soap+xml\"";
    let (st, xml) = post_raw(&app, "/WebServices/PrinterService", ct, body).await;
    assert_eq!(st, StatusCode::OK, "{xml}");
    let caps = world.read().await.captured();
    assert_eq!(caps.len(), 1);
    assert_eq!(caps[0].bytes, doc.len());
    assert_valid(&world).await;
    let _ = std::fs::remove_dir_all(inbox);
}

#[tokio::test]
async fn multi_document_job_keeps_every_document_and_its_own_format() {
    let inbox = std::env::temp_dir().join(format!("fp-multi-{}", Uuid::new_v4()));
    let world = test_world(inbox.clone());
    let app = app(world.clone());
    post_soap(
        &app,
        "/WebServices/PrinterService",
        &soap_env("CreatePrintJob", CREATE_BODY),
    )
    .await;
    for (fmt, last, data) in [
        (
            "application/pdf",
            false,
            "<wprt:DocumentData>Zmlyc3Q=</wprt:DocumentData>",
        ),
        (
            "application/octet-stream",
            true,
            "<wprt:DocumentData>c2Vjb25k</wprt:DocumentData>",
        ),
    ] {
        let (st, xml) = post_soap(
            &app,
            "/WebServices/PrinterService",
            &soap_env("SendDocument", &send_body(1, fmt, last, data)),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{xml}");
    }
    let caps = world.read().await.captured();
    assert_eq!(caps.len(), 2);
    assert_eq!(std::fs::read(&caps[0].path).unwrap(), b"first");
    assert_eq!(std::fs::read(&caps[1].path).unwrap(), b"second");
    assert_eq!(caps[0].document_format, "application/pdf");
    assert_eq!(caps[1].document_format, "application/octet-stream");

    // fault statuses follow the SOAP 1.2 HTTP binding and carry the subcode
    let (st, xml) = post_exchange(
        &app,
        "/WebServices/PrinterService",
        &soap_env(
            "SendDocument",
            &send_body(
                1,
                "application/pdf",
                true,
                "<wprt:DocumentData>eA==</wprt:DocumentData>",
            ),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert!(xml.contains("ClientErrorLastDocumentAlreadySent"), "{xml}");
    let (st, xml) = post_exchange(
        &app,
        "/WebServices/PrinterService",
        &soap_env(
            "SendDocument",
            &send_body(
                42,
                "application/pdf",
                true,
                "<wprt:DocumentData>eA==</wprt:DocumentData>",
            ),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert!(xml.contains("ClientErrorJobIdNotFound"), "{xml}");
    assert_valid(&world).await;
    let _ = std::fs::remove_dir_all(inbox);
}
