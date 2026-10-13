//! End to end over loopback: `/ui` and every JSON endpoint the page and the tray call.

mod common;

use common::*;
use fakeprint::server::{serve, DeviceConfig, ServerConfig};
use serde_json::json;
use tower::ServiceExt;

#[tokio::test]
async fn the_page_and_every_endpoint_it_uses_exist() {
    let tmp = TempDir::new("ui-e2e");
    let mut cfg = ServerConfig::embedded(tmp.path().join("inbox")).with_devices(vec![
        DeviceConfig::derived("a", "generic-wsd"),
        DeviceConfig::derived("b", "generic-wsd"),
    ]);
    cfg.profile_dir = Some(profiles_dir());
    let srv = serve(cfg).await.unwrap();
    let addr = srv.local_addr();

    for path in ["/ui", "/ui/"] {
        let r = request(addr, "GET", path, vec![], vec![]).await;
        assert_eq!(r.status, 200, "{path}");
        let t = r.text();
        assert!(t.contains("<html") || t.contains("<!doctype") || t.contains("<!DOCTYPE"));
        assert!(
            !t.contains("http://") && !t.contains("https://"),
            "self-contained page"
        );
    }
    let lan: std::net::SocketAddr = "192.168.1.9:5555".parse().unwrap();
    let mut req = axum::http::Request::builder()
        .uri("/ui")
        .body(axum::body::Body::empty())
        .unwrap();
    req.extensions_mut().insert(axum::extract::ConnectInfo(lan));
    assert_eq!(srv.router().oneshot(req).await.unwrap().status(), 403);

    assert_eq!(
        get_json(addr, "/v1/devices")
            .await
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let q = get_json(addr, "/v1/queue").await;
    assert!(q["remaining"].is_array() && q["total"].is_number());
    let profiles = get_json(addr, "/v1/profiles").await;
    assert!(profiles
        .as_array()
        .unwrap()
        .iter()
        .any(|p| p["id"] == "generic-wsd"));
    assert!(get_json(addr, "/d/a/v1/jobs").await["print_jobs"].is_array());
    for (path, body) in [
        ("/d/a/v1/faults", json!({"paper_out": true})),
        ("/d/a/v1/timing", json!({"preset": "instant"})),
        ("/d/a/v1/levels", json!({})),
        ("/d/a/v1/reset", json!({})),
        (
            "/v1/ctl",
            json!({"cmd": "open_settings", "args": {"page": "printers"}}),
        ),
        ("/v1/queue/advance", json!({})),
        ("/v1/queue/skip", json!({})),
    ] {
        let r = post_json(addr, path, &body).await;
        assert_eq!(r.status, 200, "{path}: {}", r.text());
    }
    let r = post_json(
        addr,
        "/v1/devices",
        &json!({"id": "new", "profile": "generic-wsd"}),
    )
    .await;
    assert_eq!(r.status, 201);
    let r = post_json(
        addr,
        "/v1/devices/new/reconfigure",
        &json!({"profile": "generic-wsd"}),
    )
    .await;
    assert_eq!(r.status, 200);
    let r = request(
        addr,
        "DELETE",
        "/v1/devices/new?forget=true",
        vec![],
        vec![],
    )
    .await;
    assert_eq!(r.status, 200);
    srv.shutdown().await;
}
