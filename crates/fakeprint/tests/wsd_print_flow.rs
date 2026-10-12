//! Integration: GetPrinterElements + CreatePrintJob + SendDocument → inbox.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::{get, post};
use axum::Router;
use fakeprint::discovery::DiscoveryStats;
use fakeprint::profile::Profile;
use fakeprint::state::World;
use fakeprint::wsd;
use http_body_util::BodyExt;
use tokio::sync::RwLock;
use tower::ServiceExt;
use uuid::Uuid;

fn test_world(inbox: std::path::PathBuf) -> Arc<RwLock<World>> {
    let profiles = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../profiles");
    let profile = Profile::load(&profiles, "generic-wsd").expect("load generic-wsd");
    let mut w = World::new(
        profiles,
        inbox,
        profile,
        Arc::new(DiscoveryStats::default()),
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
    Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/WebServices/Device", post(wsd::print_service))
        .route("/WebServices/PrinterService", post(wsd::print_service))
        .with_state(world)
}

async fn post_soap(app: &Router, path: &str, body: &str) -> (StatusCode, String) {
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
    (status, String::from_utf8_lossy(&bytes).into_owned())
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
    let (st, xml) = post_soap(&app, "/WebServices/PrinterService", validate).await;
    assert_eq!(st, StatusCode::OK);
    assert!(xml.contains("ValidTicket>true"), "{xml}");

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
        </wprt:JobDescription>
        <wprt:DocumentDescription>
          <wprt:DocumentFormat>application/pdf</wprt:DocumentFormat>
        </wprt:DocumentDescription>
      </wprt:PrintTicket>
    </wprt:CreatePrintJobRequest>
  </soap:Body>
</soap:Envelope>"#;
    let (st, xml) = post_soap(&app, "/WebServices/PrinterService", create).await;
    assert_eq!(st, StatusCode::OK);
    assert!(xml.contains("<wprt:JobId>1</wprt:JobId>"), "{xml}");
    assert!(xml.contains("<wprt:JobToken>job-1</wprt:JobToken>"), "{xml}");

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
        <wprt:JobDescription><wprt:JobName>mtom</wprt:JobName></wprt:JobDescription>
        <wprt:DocumentDescription>
          <wprt:DocumentFormat>application/pdf</wprt:DocumentFormat>
        </wprt:DocumentDescription>
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

    let _ = std::fs::remove_dir_all(inbox);
}
