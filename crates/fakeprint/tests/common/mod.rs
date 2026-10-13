//! Shared helpers of the loopback tests: a tiny blocking HTTP client, an event recorder
//! (a loopback HTTP server that plays the part of a subscriber), SOAP envelope builders and
//! reply readers. Loopback only; nothing here touches the network beyond 127.0.0.1.

#![allow(dead_code)]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use fakeprint::wsd::fault::FaultKind;
use fakeprint::wsd::soap::SoapRequest;
use fakeprint::wsd::xml::{ns, parse_str, Element, QName, XmlLimits};

pub const PRINT: &str = ns::PRINT;

/// A scratch directory that is removed on drop.
pub struct TempDir(pub PathBuf);

impl TempDir {
    pub fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!("fakeprint-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&p).expect("create temp dir");
        Self(p)
    }

    pub fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The shipped profiles directory.
pub fn profiles_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../profiles")
}

// ---------------------------------------------------------------------------------------------
// HTTP client
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Resp {
    pub status: u16,
    pub body: Vec<u8>,
}

impl Resp {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap_or(serde_json::Value::Null)
    }

    /// The envelope's `soap:Body` first child.
    pub fn soap_body(&self) -> Element {
        let root = parse_str(&self.text(), &XmlLimits::SMALL).expect("reply is XML");
        let body = root.child(ns::SOAP12, "Body").expect("soap:Body");
        body.children.first().cloned().expect("a body element")
    }

    /// The `wsa:Action` of the reply.
    pub fn action(&self) -> String {
        let root = parse_str(&self.text(), &XmlLimits::SMALL).expect("reply is XML");
        root.child(ns::SOAP12, "Header")
            .and_then(|h| h.child(ns::WSA, "Action"))
            .map(|a| a.text_trimmed().to_string())
            .unwrap_or_default()
    }

    /// Which fault this reply is, if it is one.
    pub fn fault_kind(&self) -> Option<FaultKind> {
        let root = parse_str(&self.text(), &XmlLimits::SMALL).ok()?;
        let fault = root.child(ns::SOAP12, "Body")?.child(ns::SOAP12, "Fault")?;
        let code = fault.child(ns::SOAP12, "Code")?;
        let value = code.child(ns::SOAP12, "Value")?.text_qname()?;
        if let Some(sub) = code.child(ns::SOAP12, "Subcode") {
            let q = sub.child(ns::SOAP12, "Value")?.text_qname()?;
            return FaultKind::from_subcode(&q).or(Some(FaultKind::Malformed));
        }
        Some(match value.local.as_str() {
            "Receiver" => FaultKind::Internal,
            "MustUnderstand" => FaultKind::MustUnderstand,
            "VersionMismatch" => FaultKind::VersionMismatch,
            _ => FaultKind::Malformed,
        })
    }
}

/// One blocking HTTP/1.1 request (`Connection: close`).
pub fn http(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> Resp {
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(5)).expect("connect");
    s.set_read_timeout(Some(Duration::from_secs(20)))
        .expect("timeout");
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    );
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    s.write_all(head.as_bytes()).expect("write head");
    s.write_all(body).expect("write body");
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).expect("read reply");
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("header end");
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    let status = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse().ok())
        .expect("status line");
    let mut body = raw[split + 4..].to_vec();
    if head
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
        body = dechunk(&body);
    }
    Resp { status, body }
}

fn dechunk(mut b: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(nl) = b.windows(2).position(|w| w == b"\r\n") {
        let size = usize::from_str_radix(String::from_utf8_lossy(&b[..nl]).trim(), 16).unwrap_or(0);
        b = &b[nl + 2..];
        if size == 0 || b.len() < size {
            break;
        }
        out.extend_from_slice(&b[..size]);
        b = b.get(size + 2..).unwrap_or(&[]);
    }
    out
}

/// Async wrapper: run the blocking request on the blocking pool.
pub async fn request(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
) -> Resp {
    let (m, p) = (method.to_string(), path.to_string());
    tokio::task::spawn_blocking(move || {
        let h: Vec<(&str, &str)> = headers
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        http(addr, &m, &p, &h, &body)
    })
    .await
    .expect("request task")
}

pub async fn get_json(addr: SocketAddr, path: &str) -> serde_json::Value {
    request(addr, "GET", path, vec![], vec![]).await.json()
}

pub async fn post_json(addr: SocketAddr, path: &str, body: &serde_json::Value) -> Resp {
    request(
        addr,
        "POST",
        path,
        vec![("Content-Type".into(), "application/json".into())],
        body.to_string().into_bytes(),
    )
    .await
}

/// POST a SOAP envelope (`application/soap+xml`).
pub async fn soap(addr: SocketAddr, path: &str, envelope: String) -> Resp {
    request(
        addr,
        "POST",
        path,
        vec![(
            "Content-Type".into(),
            "application/soap+xml; charset=utf-8".into(),
        )],
        envelope.into_bytes(),
    )
    .await
}

// ---------------------------------------------------------------------------------------------
// Envelopes
// ---------------------------------------------------------------------------------------------

static COUNTER: AtomicU32 = AtomicU32::new(1);

pub fn message_id() -> String {
    format!("urn:uuid:{}", uuid::Uuid::new_v4())
}

/// A SOAP 1.2 envelope with the namespaces the tests use declared on the root.
pub fn envelope(action: &str, header_extra: &str, body: &str) -> String {
    let _ = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?><soap:Envelope xmlns:soap="{}" xmlns:wsa="{}" xmlns:wprt="{PRINT}" xmlns:wprt12="{}" xmlns:wse="{}" xmlns:wxf="{}" xmlns:wsx="{}" xmlns:wsd="{}"><soap:Header><wsa:Action>{action}</wsa:Action><wsa:MessageID>{}</wsa:MessageID>{header_extra}</soap:Header><soap:Body>{body}</soap:Body></soap:Envelope>"#,
        ns::SOAP12,
        ns::WSA,
        ns::PRINT12,
        ns::WSE,
        ns::WXF,
        ns::MEX,
        ns::WSD,
        message_id(),
    )
}

pub fn print_action(op: &str) -> String {
    format!("{PRINT}/{op}")
}

pub fn ticket_body(name: &str) -> String {
    format!(
        "<wprt:CreatePrintJobRequest><wprt:PrintTicket><wprt:JobDescription><wprt:JobName>{name}</wprt:JobName><wprt:JobOriginatingUserName>lab</wprt:JobOriginatingUserName></wprt:JobDescription></wprt:PrintTicket></wprt:CreatePrintJobRequest>"
    )
}

pub fn send_body(
    job: u32,
    doc_id: u32,
    fmt: &str,
    compression: &str,
    last: bool,
    data: &str,
) -> String {
    format!(
        "<wprt:SendDocumentRequest><wprt:JobId>{job}</wprt:JobId><wprt:DocumentDescription><wprt:DocumentId>{doc_id}</wprt:DocumentId><wprt:Compression>{compression}</wprt:Compression><wprt:Format>{fmt}</wprt:Format></wprt:DocumentDescription><wprt:LastDocument>{last}</wprt:LastDocument>{data}</wprt:SendDocumentRequest>"
    )
}

/// A `multipart/related` MTOM package carrying `doc` for a SendDocument envelope whose
/// `DocumentData` is an `xop:Include` of `cid`.
pub fn mtom(envelope_xml: &str, cid: &str, doc: &[u8]) -> (String, Vec<u8>) {
    let mut body = Vec::new();
    body.extend_from_slice(b"--B\r\nContent-Type: application/xop+xml; charset=utf-8; type=\"application/soap+xml\"\r\n\r\n");
    body.extend_from_slice(envelope_xml.as_bytes());
    body.extend_from_slice(format!("\r\n--B\r\nContent-Type: application/octet-stream\r\nContent-ID: <{cid}>\r\nContent-Transfer-Encoding: binary\r\n\r\n").as_bytes());
    body.extend_from_slice(doc);
    body.extend_from_slice(b"\r\n--B--\r\n");
    (
        "multipart/related; boundary=\"B\"; type=\"application/xop+xml\"; start-info=\"application/soap+xml\"".to_string(),
        body,
    )
}

pub async fn soap_mtom(addr: SocketAddr, path: &str, content_type: String, body: Vec<u8>) -> Resp {
    request(
        addr,
        "POST",
        path,
        vec![("Content-Type".into(), content_type)],
        body,
    )
    .await
}

/// `JobId` of a `CreatePrintJobResponse`.
pub fn job_id_of(resp: &Resp) -> u32 {
    assert_eq!(resp.status, 200, "{}", resp.text());
    resp.soap_body()
        .child(ns::PRINT, "JobId")
        .expect("JobId")
        .text_trimmed()
        .parse()
        .expect("numeric job id")
}

/// `wse:Identifier` of a `SubscribeResponse`.
pub fn subscription_id(resp: &Resp) -> String {
    assert_eq!(resp.status, 200, "{}", resp.text());
    resp.soap_body()
        .descendant(&QName::new(ns::WSE, "Identifier"))
        .expect("Identifier")
        .text_trimmed()
        .to_string()
}

pub fn identifier_header(id: &str) -> String {
    format!("<wse:Identifier>{id}</wse:Identifier>")
}

// ---------------------------------------------------------------------------------------------
// The event recorder
// ---------------------------------------------------------------------------------------------

/// One notification the recorder received.
#[derive(Debug, Clone)]
pub struct Received {
    pub path: String,
    pub action: String,
    pub body: Element,
}

impl Received {
    pub fn short(&self) -> &str {
        self.action.rsplit('/').next().unwrap_or(&self.action)
    }

    /// Text of the first descendant with this print-namespace name.
    pub fn text(&self, local: &str) -> Option<String> {
        self.body
            .descendant(&QName::new(ns::PRINT, local))
            .map(|e| e.text_trimmed().to_string())
    }
}

/// A loopback HTTP server that records every POST and answers 200.
pub struct Recorder {
    pub addr: SocketAddr,
    got: Arc<Mutex<Vec<Received>>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Recorder {
    pub fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind recorder");
        listener.set_nonblocking(true).expect("nonblocking");
        let addr = listener.local_addr().expect("addr");
        let got: Arc<Mutex<Vec<Received>>> = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (g, s) = (Arc::clone(&got), Arc::clone(&stop));
        let thread = std::thread::spawn(move || {
            while !s.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((conn, _)) => serve_one(conn, &g),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            addr,
            got,
            stop,
            thread: Some(thread),
        }
    }

    /// `http://127.0.0.1:port/notify`
    pub fn url(&self) -> String {
        format!("http://{}/notify", self.addr)
    }

    pub fn received(&self) -> Vec<Received> {
        self.got.lock().expect("lock").clone()
    }

    /// Short action names, in arrival order.
    pub fn shorts(&self) -> Vec<String> {
        self.received()
            .iter()
            .map(|r| r.short().to_string())
            .collect()
    }

    pub fn clear(&self) {
        self.got.lock().expect("lock").clear();
    }

    /// Wait until `pred` holds for the received events, or panic after `timeout`.
    pub async fn wait_for(
        &self,
        what: &str,
        timeout: Duration,
        pred: impl Fn(&[Received]) -> bool,
    ) -> Vec<Received> {
        let end = Instant::now() + timeout;
        loop {
            let now = self.received();
            if pred(&now) {
                return now;
            }
            if Instant::now() > end {
                panic!(
                    "timed out waiting for {what}; received: {:?}",
                    self.shorts()
                );
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn serve_one(mut conn: TcpStream, got: &Arc<Mutex<Vec<Received>>>) {
    let _ = conn.set_nonblocking(false);
    let _ = conn.set_read_timeout(Some(Duration::from_secs(3)));
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let (head_end, len) = loop {
        match conn.read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..i]).to_ascii_lowercase();
            let len = head
                .lines()
                .find_map(|l| {
                    l.strip_prefix("content-length:")
                        .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                })
                .unwrap_or(0);
            break (i + 4, len);
        }
    };
    while buf.len() < head_end + len {
        match conn.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let path = head.split_whitespace().nth(1).unwrap_or("/").to_string();
    let payload = &buf[head_end..buf.len().min(head_end + len)];
    if let Ok(req) = SoapRequest::parse(Some("application/soap+xml"), None, payload) {
        if let Some(body) = req.body() {
            got.lock().expect("lock").push(Received {
                path,
                action: req.action.clone(),
                body: body.clone(),
            });
        }
    }
    let _ = conn.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
}
