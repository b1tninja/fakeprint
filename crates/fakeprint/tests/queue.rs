//! The serial install queue and the announce rule, driven through the discovery hub on a fake
//! socket (no multicast, no network). docs/multi-device.md section 3.3.

mod common;

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::*;
use fakeprint::discovery::{DiscoveryHub, DiscoverySocket};
use fakeprint::registry::{InstallState, WindowsPrinter};
use fakeprint::server::{serve, DeviceConfig, ServerConfig, ServerHandle};
use serde_json::json;

type Sent = Arc<Mutex<Vec<(SocketAddr, Vec<u8>)>>>;

struct FakeSocket {
    rx: tokio::sync::mpsc::UnboundedReceiver<(Vec<u8>, SocketAddr)>,
    sent: Sent,
}

impl DiscoverySocket for FakeSocket {
    async fn recv(&mut self) -> std::io::Result<(Vec<u8>, SocketAddr)> {
        match self.rx.recv().await {
            Some(x) => Ok(x),
            None => std::future::pending().await,
        }
    }
    async fn send_to(&mut self, data: &[u8], dest: SocketAddr) -> std::io::Result<usize> {
        self.sent.lock().unwrap().push((dest, data.to_vec()));
        Ok(data.len())
    }
    fn join_group(&mut self) -> std::io::Result<()> {
        Ok(())
    }
    fn leave_group(&mut self) -> std::io::Result<()> {
        Ok(())
    }
    fn can_receive(&self) -> bool {
        true
    }
}

struct Lab {
    srv: ServerHandle,
    sent: Sent,
    tx: tokio::sync::mpsc::UnboundedSender<(Vec<u8>, SocketAddr)>,
}

fn kinds(sent: &Sent) -> Vec<&'static str> {
    sent.lock()
        .unwrap()
        .iter()
        .map(|(_, d)| {
            let t = String::from_utf8_lossy(d);
            if t.contains("discovery/Hello") {
                "Hello"
            } else if t.contains("discovery/Bye") {
                "Bye"
            } else if t.contains("discovery/ProbeMatches") {
                "ProbeMatches"
            } else {
                "?"
            }
        })
        .collect()
}

fn count(sent: &Sent, kind: &str) -> usize {
    kinds(sent).iter().filter(|k| **k == kind).count()
}

async fn settle_io() {
    tokio::time::sleep(Duration::from_millis(150)).await;
}

/// Five un-added printers a..e, all with discovery on, behind a hub on a fake socket.
async fn lab(tmp: &TempDir, settle_ms: u64, timeout_ms: u64) -> Lab {
    let devices: Vec<DeviceConfig> = ["a", "b", "c", "d", "e"]
        .iter()
        .map(|id| {
            let mut d = DeviceConfig::derived(*id, "generic-wsd");
            d.discovery = Some(true);
            d
        })
        .collect();
    let mut cfg = ServerConfig::embedded(tmp.path().join("inbox")).with_devices(devices);
    cfg.profile_dir = Some(profiles_dir());
    cfg.directed_discovery = true;
    cfg.discovery_timings = Some(fakeprint::discovery::DiscoveryTimings::immediate());
    let srv = serve(cfg).await.unwrap();
    let m = srv.manager();
    m.set_queue_timers(
        Duration::from_millis(settle_ms),
        Duration::from_millis(timeout_ms),
    );
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let sent: Sent = Arc::default();
    m.attach_hub(DiscoveryHub::spawn(
        FakeSocket {
            rx,
            sent: sent.clone(),
        },
        None,
    ));
    Lab { srv, sent, tx }
}

async fn head(l: &Lab) -> Option<String> {
    l.srv.manager().queue().await.head
}

async fn wait_head(l: &Lab, want: &str) {
    for _ in 0..100 {
        if head(l).await.as_deref() == Some(want) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    panic!(
        "head never became {want}: {:?}",
        l.srv.manager().queue().await
    );
}

fn win(l: &Lab, uuid: uuid::Uuid) -> WindowsPrinter {
    let _ = l;
    WindowsPrinter {
        name: "fakeprint".into(),
        port: format!("WSD-{uuid}"),
        ..WindowsPrinter::default()
    }
}

fn probe(id: &str) -> Vec<u8> {
    format!(
        r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope" xmlns:a="http://schemas.xmlsoap.org/ws/2004/08/addressing" xmlns:d="http://schemas.xmlsoap.org/ws/2005/04/discovery" xmlns:dp="http://schemas.xmlsoap.org/ws/2006/02/devprof"><s:Header><a:To>urn:schemas-xmlsoap-org:ws:2005:04:discovery</a:To><a:Action>http://schemas.xmlsoap.org/ws/2005/04/discovery/Probe</a:Action><a:MessageID>urn:uuid:{id}</a:MessageID></s:Header><s:Body><d:Probe><d:Types>dp:Device</d:Types></d:Probe></s:Body></s:Envelope>"#
    )
    .into_bytes()
}

#[tokio::test]
async fn five_uninstalled_devices_exactly_one_announces() {
    let tmp = TempDir::new("q-one");
    let l = lab(&tmp, 100, 60_000).await;
    settle_io().await;
    assert_eq!(kinds(&l.sent), ["Hello"], "only the head says Hello");
    let q = l.srv.manager().queue().await;
    assert_eq!(q.head.as_deref(), Some("a"));
    assert_eq!(q.remaining, ["b", "c", "d", "e"]);
    assert_eq!((q.position, q.total), (Some(1), 5));
    assert!(q.head_since.is_some());
    let infos = l.srv.devices().await;
    assert_eq!(infos.iter().filter(|d| d.announcing).count(), 1);
    assert!(infos[0].announcing);
    assert_eq!(
        infos.iter().map(|d| d.queue_position).collect::<Vec<_>>(),
        [Some(1), Some(2), Some(3), Some(4), Some(5)]
    );
    l.srv.shutdown().await;
}

#[tokio::test]
async fn install_then_settle_then_next_hello_and_installed_keep_announcing() {
    let tmp = TempDir::new("q-advance");
    let l = lab(&tmp, 400, 60_000).await;
    settle_io().await;
    let m = l.srv.manager();
    let a = l.srv.device("a").await.unwrap().info.uuid;
    m.mark_installed("a", win(&l, a)).await.unwrap();
    // during the settle delay nobody new announces
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        count(&l.sent, "Hello"),
        1,
        "settle delay: b is still silent"
    );
    wait_head(&l, "b").await;
    settle_io().await;
    assert_eq!(count(&l.sent, "Hello"), 2);
    assert_eq!(
        count(&l.sent, "Bye"),
        0,
        "the installed device keeps announcing"
    );
    let infos = l.srv.devices().await;
    assert!(infos[0].announcing && infos[1].announcing && !infos[2].announcing);
    assert_eq!(infos[0].queue_position, None);
    assert_eq!(infos[1].queue_position, Some(1));
    // position counts the installed ones: "2 of 5"
    let q = m.queue().await;
    assert_eq!((q.position, q.total), (Some(2), 5));
    l.srv.shutdown().await;
}

#[tokio::test]
async fn skip_advance_reorder_and_head_timeout_rotate() {
    let tmp = TempDir::new("q-rotate");
    let l = lab(&tmp, 50, 700).await;
    settle_io().await;
    let m = l.srv.manager();
    let q = m.queue_skip().await;
    assert_eq!(q.head.as_deref(), Some("b"));
    assert_eq!(q.remaining, ["c", "d", "e", "a"]);
    settle_io().await;
    // a (the old head) said Bye, b said Hello; Bye only for devices that were announcing
    assert_eq!(kinds(&l.sent), ["Hello", "Bye", "Hello"]);
    let q = m.queue_advance().await;
    assert_eq!(q.head.as_deref(), Some("c"));

    let q = m.queue_reorder(&["e".into(), "d".into()]).await.unwrap();
    assert_eq!(q.head.as_deref(), Some("e"));
    assert_eq!(q.remaining[0], "d");
    let e = m.queue_reorder(&["nope".into()]).await.unwrap_err();
    assert_eq!(fakeprint::manager::error_status(&e), 400);

    // the head is not installed within the timeout: rotate to the back
    wait_head(&l, "d").await;
    let q = m.queue().await;
    assert_eq!(q.remaining.last().map(String::as_str), Some("e"));
    l.srv.shutdown().await;
}

#[tokio::test]
async fn probes_are_answered_only_by_announcing_members_and_dormant_ones_answer_directed() {
    let tmp = TempDir::new("q-probe");
    let l = lab(&tmp, 50, 60_000).await;
    settle_io().await;
    let peer: SocketAddr = "127.0.0.1:40000".parse().unwrap();
    l.tx.send((probe("p1"), peer)).unwrap();
    settle_io().await;
    assert_eq!(count(&l.sent, "ProbeMatches"), 1, "only the head answers");
    // the head's contact is recorded as peer contact
    let a = l.srv.device("a").await.unwrap().info;
    assert_eq!(a.install.state, InstallState::Probed);

    // install a and wait for b: now two announcing members
    let m = l.srv.manager();
    m.mark_installed("a", win(&l, a.uuid)).await.unwrap();
    wait_head(&l, "b").await;
    settle_io().await;
    l.tx.send((probe("p2"), peer)).unwrap();
    settle_io().await;
    assert_eq!(count(&l.sent, "ProbeMatches"), 3);

    // a dormant device answers the directed (HTTP) discovery endpoint
    let r = request(
        l.srv.local_addr(),
        "POST",
        "/d/e/StableWSDiscoveryEndpoint/schemas-xmlsoap-org_ws_2005_04_discovery",
        vec![("Content-Type".into(), "application/soap+xml".into())],
        probe("p3"),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert!(r.text().contains("ProbeMatches"), "{}", r.text());
    assert!(!l.srv.device("e").await.unwrap().info.announcing);

    // shutdown: Bye for the announcing members only (a and b), none for the dormant three
    let before = count(&l.sent, "Bye");
    let sent = l.sent.clone();
    l.srv.shutdown().await;
    assert_eq!(count(&sent, "Bye") - before, 2);
}

#[tokio::test]
async fn offline_devices_are_not_queued_and_a_removed_head_passes_the_baton() {
    let tmp = TempDir::new("q-offline");
    let l = lab(&tmp, 50, 60_000).await;
    settle_io().await;
    let m = l.srv.manager();
    m.mark_installed("b", win(&l, l.srv.device("b").await.unwrap().info.uuid))
        .await
        .unwrap();
    m.mark_stale("b", "not listed by Windows").await.unwrap();
    let q = m.queue().await;
    assert!(
        !q.remaining.contains(&"b".to_string()),
        "offline is not an Add request"
    );
    assert!(
        l.srv.device("b").await.unwrap().info.announcing,
        "but it keeps announcing"
    );
    // remove the head device: the next one is promoted (the default device cannot go, so use c)
    m.queue_reorder(&["c".into()]).await.unwrap();
    wait_head(&l, "c").await;
    m.remove("c").await.unwrap();
    wait_head(&l, "a").await;
    let reply = post_json(l.srv.local_addr(), "/v1/queue/skip", &json!({})).await;
    assert_eq!(reply.status, 200);
    l.srv.shutdown().await;
}

#[tokio::test]
async fn embedded_never_announces_and_has_no_queue() {
    let tmp = TempDir::new("q-embedded");
    let mut cfg = ServerConfig::embedded(tmp.path().join("inbox")).with_devices(vec![
        DeviceConfig::derived("a", "generic-wsd"),
        DeviceConfig::derived("b", "generic-wsd"),
    ]);
    cfg.profile_dir = Some(profiles_dir());
    let srv = serve(cfg).await.unwrap();
    let q = srv.manager().queue().await;
    assert!(q.head.is_none() && q.remaining.is_empty() && q.total == 0);
    assert!(srv
        .devices()
        .await
        .iter()
        .all(|d| !d.announcing && d.queue_position.is_none()));
    srv.shutdown().await;
}
