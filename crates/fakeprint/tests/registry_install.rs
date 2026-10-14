//! Install state machine, reconcile, HTTP install surface and registry persistence
//! (docs/multi-device.md sections 3.4, 4). Loopback only; Windows is never touched: the
//! "Windows printers" are plain values handed to the manager.

mod common;

use common::*;
use fakeprint::manager::error_status;
use fakeprint::registry::{InstallState, Registry, WindowsPrinter};
use fakeprint::server::{serve, DeviceConfig, ServerConfig, ServerHandle};
use serde_json::json;

async fn start(tmp: &TempDir) -> ServerHandle {
    let mut cfg = ServerConfig::embedded(tmp.path().join("inbox")).with_devices(vec![
        DeviceConfig::derived("a", "generic-wsd"),
        DeviceConfig::derived("b", "generic-wsd"),
    ]);
    cfg.profile_dir = Some(profiles_dir());
    serve(cfg).await.unwrap()
}

fn wsd(name: &str, uuid: impl std::fmt::Display) -> WindowsPrinter {
    WindowsPrinter {
        name: name.into(),
        port: format!("WSD-{uuid}"),
        driver: "Microsoft IPP Class Driver".into(),
        ..WindowsPrinter::default()
    }
}

async fn state(srv: &ServerHandle, id: &str) -> InstallState {
    srv.device(id).await.unwrap().info.install.state
}

#[tokio::test]
async fn new_to_probed_installed_and_back() {
    let tmp = TempDir::new("ri-states");
    let srv = start(&tmp).await;
    let m = srv.manager();
    let b = srv.device("b").await.unwrap().info.uuid;
    assert_eq!(state(&srv, "b").await, InstallState::New);

    // peer contact on the WSD endpoint flips new -> probed (the daemon records it itself)
    let r = soap(
        srv.local_addr(),
        "/d/b/WebServices/Device",
        envelope("http://schemas.xmlsoap.org/ws/2004/09/transfer/Get", "", ""),
    )
    .await;
    assert_eq!(r.status, 200);
    for _ in 0..50 {
        if state(&srv, "b").await == InstallState::Probed {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let info = srv.device("b").await.unwrap().info;
    assert_eq!(info.install.state, InstallState::Probed);
    assert!(info.install.first_probed_at.is_some() && info.install.last_peer.is_some());
    assert_eq!(
        state(&srv, "a").await,
        InstallState::New,
        "other devices are untouched"
    );

    // refused: another device's UUID, a TCPMON port, a name that merely looks right
    let a = srv.device("a").await.unwrap().info.uuid;
    for bad in [
        wsd("fakeprint b", a),
        WindowsPrinter {
            name: "fakeprint b".into(),
            port: "192.168.1.5:9100".into(),
            ..WindowsPrinter::default()
        },
        wsd("x", format!("{b}-{a}")),
    ] {
        let e = m.mark_installed("b", bad).await.unwrap_err();
        assert_eq!(error_status(&e), 400, "{e:#}");
    }
    assert_eq!(state(&srv, "b").await, InstallState::Probed);

    // accepted: its own WSD port
    m.mark_installed("b", wsd("fakeprint b", b)).await.unwrap();
    let info = srv.device("b").await.unwrap().info;
    assert_eq!(info.install.state, InstallState::Installed);
    assert_eq!(
        info.install.kind,
        Some(fakeprint::registry::InstallKind::Wsd)
    );
    assert_eq!(info.install.windows.name, "fakeprint b");
    assert_eq!(
        info.install.published_fingerprint_at_install.as_deref(),
        Some(info.config.published_fingerprint.as_str())
    );
    assert!(!info.needs_readd);

    // the IPP URL of the device matches too
    let ipp = srv.device("a").await.unwrap().info.ipp_endpoint.unwrap();
    let url = ipp.replacen("ipp://", "http://", 1);
    m.mark_installed(
        "a",
        WindowsPrinter {
            name: "fakeprint a".into(),
            port: url,
            ..WindowsPrinter::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(
        srv.device("a").await.unwrap().info.install.kind,
        Some(fakeprint::registry::InstallKind::Ipp)
    );

    // forget keeps the device, drops the install info
    m.forget("b").await.unwrap();
    assert_eq!(state(&srv, "b").await, InstallState::New);
    srv.shutdown().await;
}

#[tokio::test]
async fn profile_change_makes_stale_only_when_published_caps_change_and_heals() {
    let tmp = TempDir::new("ri-stale");
    let srv = start(&tmp).await;
    let m = srv.manager();
    let uuid = srv.device("b").await.unwrap().info.uuid;
    m.mark_installed("b", wsd("fakeprint b", uuid))
        .await
        .unwrap();
    let addr = srv.local_addr();

    // runtime state never makes a device stale
    post_json(
        addr,
        "/d/b/v1/faults",
        &json!({"paper_out": true, "toner_low": true}),
    )
    .await;
    post_json(addr, "/d/b/v1/timing", &json!({"preset": "realistic"})).await;
    assert_eq!(state(&srv, "b").await, InstallState::Installed);

    let ids: Vec<String> = get_json(addr, "/v1/profiles")
        .await
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["id"].as_str().unwrap().to_string())
        .collect();
    let other = ids.iter().find(|i| *i != "generic-wsd").unwrap().clone();
    let r = post_json(addr, "/d/b/v1/profile", &json!({"id": other})).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let info = srv.device("b").await.unwrap().info;
    assert_eq!(info.install.state, InstallState::Stale);
    assert!(info.needs_readd, "profile change asks for a re-Add");
    assert!(info
        .needs_readd_reason
        .unwrap()
        .starts_with("profile changed"));
    assert!(info.install.stale_from.as_deref() == Some("installed"));

    // back to the old profile: healed without any Windows round trip
    let info = m.reconfigure("b", "generic-wsd").await.unwrap();
    assert_eq!(info.install.state, InstallState::Installed);
    assert!(!info.needs_readd);
    srv.shutdown().await;
}

#[tokio::test]
async fn reconcile_marks_offline_separately_and_reports_foreign() {
    let tmp = TempDir::new("ri-reconcile");
    let srv = start(&tmp).await;
    let m = srv.manager();
    let a = srv.device("a").await.unwrap().info.uuid;
    let b = srv.device("b").await.unwrap().info.uuid;
    let foreign = WindowsPrinter {
        name: "Brother HL-L2350DW".into(),
        port: "WSD-11111111-2222-4333-8444-555555555555".into(),
        ..WindowsPrinter::default()
    };
    let r = m
        .reconcile_with_windows(&[
            wsd("fakeprint a", a),
            wsd("fakeprint b", b),
            foreign.clone(),
        ])
        .unwrap();
    assert_eq!(r.installed, ["a", "b"]);
    assert_eq!(r.foreign, std::slice::from_ref(&foreign));
    assert!(r.stale.is_empty());

    // b disappears from Windows: stale, but offline - not a request to Add
    let r = m.reconcile_with_windows(&[wsd("fakeprint a", a)]).unwrap();
    assert_eq!(r.stale, ["b"]);
    let info = srv.device("b").await.unwrap().info;
    assert_eq!(info.install.state, InstallState::Stale);
    assert!(!info.needs_readd);
    assert_eq!(
        info.install.reason.as_deref(),
        Some("not listed by Windows")
    );

    // it heals only by a reconcile that sees it again
    let r = m
        .reconcile_with_windows(&[wsd("fakeprint a", a), wsd("fakeprint b", b)])
        .unwrap();
    assert!(r.installed.contains(&"b".to_string()));
    assert_eq!(state(&srv, "b").await, InstallState::Installed);
    srv.shutdown().await;
}

#[tokio::test]
async fn http_install_surface_and_loopback_guard() {
    use tower::ServiceExt;
    let tmp = TempDir::new("ri-http");
    let srv = start(&tmp).await;
    let addr = srv.local_addr();
    let b = srv.device("b").await.unwrap().info.uuid;
    let body = json!({"windows": {"name": "fakeprint b", "port": format!("WSD-{b}")}});
    let r = post_json(addr, "/v1/devices/b/installed", &body).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.json()["install"]["state"], "installed");
    let bad = json!({"windows": {"name": "fakeprint b", "port": "10.0.0.1:9100"}});
    assert_eq!(
        post_json(addr, "/v1/devices/b/installed", &bad)
            .await
            .status,
        400
    );
    assert_eq!(
        post_json(addr, "/v1/devices/zz/installed", &body)
            .await
            .status,
        404
    );

    let r = post_json(addr, "/v1/reconcile", &json!({"printers": []})).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["stale"][0], "b");
    let reg = get_json(addr, "/v1/registry").await;
    assert_eq!(reg["devices"].as_array().unwrap().len(), 2);
    let r = post_json(addr, "/v1/devices/b/forget", &json!({})).await;
    assert_eq!(r.json()["install"]["state"], "new");

    // ctl: the same operations by name; shutdown needs confirm
    let r = post_json(
        addr,
        "/v1/ctl",
        &json!({"cmd": "mark_added", "args": {"id": "a"}}),
    )
    .await;
    assert_eq!(r.json()["install"]["state"], "installed");
    let r = post_json(
        addr,
        "/v1/ctl",
        &json!({"cmd": "open_settings", "args": {"page": "wifi"}}),
    )
    .await;
    assert_eq!(r.status, 400);
    let r = post_json(addr, "/v1/ctl", &json!({"cmd": "open_settings"})).await;
    assert_eq!(
        r.json()["opened"],
        false,
        "no launcher installed: nothing opens"
    );
    let r = post_json(addr, "/v1/ctl", &json!({"cmd": "shutdown"})).await;
    assert_eq!(r.status, 400);
    let mut exit = srv.exit_signal();
    assert!(!*exit.borrow());
    let r = post_json(
        addr,
        "/v1/ctl",
        &json!({"cmd": "shutdown", "args": {"confirm": true}}),
    )
    .await;
    assert_eq!(r.status, 200);
    exit.changed().await.unwrap();
    assert!(*exit.borrow());
    let r = post_json(addr, "/v1/ctl", &json!({"cmd": "devices"})).await;
    assert_eq!(r.json().as_array().unwrap().len(), 2);

    // non-loopback peers: 403 on install / registry / ctl mutations, reads stay open
    let lan: std::net::SocketAddr = "192.168.1.9:5555".parse().unwrap();
    let send = |method: &str, uri: &str, b: String| {
        let mut req = axum::http::Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json")
            .body(axum::body::Body::from(b))
            .unwrap();
        req.extensions_mut().insert(axum::extract::ConnectInfo(lan));
        let router = srv.router();
        async move { router.oneshot(req).await.unwrap().status() }
    };
    assert_eq!(send("GET", "/v1/registry", String::new()).await, 403);
    assert_eq!(send("POST", "/v1/reconcile", "{}".into()).await, 403);
    assert_eq!(
        send("POST", "/v1/devices/b/installed", body.to_string()).await,
        403
    );
    assert_eq!(send("POST", "/v1/queue/advance", "{}".into()).await, 403);
    assert_eq!(
        send(
            "POST",
            "/v1/ctl",
            json!({"cmd": "shutdown", "args": {"confirm": true}}).to_string()
        )
        .await,
        403
    );
    assert_eq!(send("GET", "/v1/devices", String::new()).await, 200);
    assert_eq!(send("GET", "/v1/queue", String::new()).await, 200);
    srv.shutdown().await;
}

#[tokio::test]
async fn a_managed_daemon_persists_and_reloads_its_registry() {
    let tmp = TempDir::new("ri-managed");
    let reg_path = tmp.path().join("devices.json");
    let make = || {
        let mut cfg = ServerConfig::managed(profiles_dir(), tmp.path().join("inbox"), &reg_path);
        cfg.listen = "127.0.0.1:0".parse().unwrap();
        cfg.daemon.instance_lock = false;
        cfg.daemon.jetdirect_port = None;
        cfg.daemon.named_pipe = false;
        cfg
    };
    // first run: the legacy default device gets a row
    let srv = serve(make()).await.unwrap();
    let m = srv.manager();
    let rows = srv.devices().await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, "default");
    assert_eq!(rows[0].uuid.to_string(), fakeprint::lab::LAB_DEVICE_UUID);
    let reg = Registry::parse(&std::fs::read_to_string(&reg_path).unwrap()).unwrap();
    assert_eq!(reg.devices.len(), 1);
    assert_eq!(reg.devices[0].name, "default");

    // add a device, install it, switch its profile; everything lands in the file
    let info = m
        .add(DeviceConfig::derived("mono", "generic-wsd"))
        .await
        .unwrap();
    m.mark_installed("mono", wsd("fakeprint mono", info.uuid))
        .await
        .unwrap();
    let q = m.queue().await;
    assert_eq!(
        q.head.as_deref(),
        Some("default"),
        "the daemon has an install queue"
    );
    srv.shutdown().await;

    let reg = Registry::parse(&std::fs::read_to_string(&reg_path).unwrap()).unwrap();
    assert_eq!(reg.devices.len(), 2);
    let row = reg.device("mono").unwrap();
    assert_eq!(row.install.state, InstallState::Installed);
    assert!(row.added.pnp);
    assert_eq!(row.device_uuid, Some(info.uuid));
    assert!(
        row.discovery.is_none(),
        "an inherited setting is never stored"
    );
    assert!(row.config.published_fingerprint.starts_with("fnv64:"));

    // restart: same devices, same UUIDs, install state kept
    let srv = serve(make()).await.unwrap();
    let rows = srv.devices().await;
    assert_eq!(rows.len(), 2);
    let mono = rows.iter().find(|r| r.id == "mono").unwrap();
    assert_eq!(mono.uuid, info.uuid);
    assert_eq!(mono.install.state, InstallState::Installed);
    srv.shutdown().await;

    // the row's profile is edited while the daemon is down: the published caps change, so the
    // installed printer becomes stale ("profile changed") at the next start
    let other = std::fs::read_dir(profiles_dir())
        .unwrap()
        .filter_map(Result::ok)
        .filter_map(|e| {
            e.path()
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
        })
        .find(|n| n != "generic-wsd" && n != "README")
        .unwrap();
    let mut reg = Registry::parse(&std::fs::read_to_string(&reg_path).unwrap()).unwrap();
    reg.devices
        .iter_mut()
        .find(|r| r.name == "mono")
        .unwrap()
        .profile = other;
    std::fs::write(&reg_path, reg.to_json()).unwrap();
    let srv = serve(make()).await.unwrap();
    let mono = srv.device("mono").await.unwrap().info;
    assert_eq!(mono.install.state, InstallState::Stale);
    assert!(mono.needs_readd);
    // removing with forget drops the row; keeping it does not
    srv.manager().remove_row("mono", true).await.unwrap();
    srv.shutdown().await;
    let reg = Registry::parse(&std::fs::read_to_string(&reg_path).unwrap()).unwrap();
    assert!(reg.device("mono").is_none() && reg.device("default").is_some());
}

/// Rows belong to the *port*: restarting with another address (127.0.0.1 -> 127.0.0.2, as
/// 127.0.0.1 -> 0.0.0.0 on a real machine) must still find every printer added at run time.
#[tokio::test]
async fn runtime_added_devices_survive_a_restart_on_another_address_of_the_same_port() {
    let tmp = TempDir::new("ri-restart");
    let reg_path = tmp.path().join("devices.json");
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let make = |ip: &str| {
        let mut cfg = ServerConfig::managed(profiles_dir(), tmp.path().join("inbox"), &reg_path);
        cfg.listen = format!("{ip}:{port}").parse().unwrap();
        cfg.daemon.instance_lock = false;
        cfg.daemon.jetdirect_port = None;
        cfg.daemon.named_pipe = false;
        cfg
    };
    let srv = serve(make("127.0.0.1")).await.unwrap();
    let m = srv.manager();
    let mono = m
        .add(DeviceConfig::derived("mono1", "generic-wsd"))
        .await
        .unwrap();
    let ink = m
        .add(DeviceConfig::derived("inkjet1", "generic-wsd"))
        .await
        .unwrap();
    m.queue_reorder(&["inkjet1".into(), "mono1".into()])
        .await
        .unwrap();
    srv.shutdown().await;

    let srv = serve(make("127.0.0.2")).await.unwrap();
    let rows = srv.devices().await;
    let ids: Vec<&str> = rows.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(ids, ["default", "mono1", "inkjet1"]);
    assert_eq!(rows[1].uuid, mono.uuid);
    assert_eq!(rows[2].uuid, ink.uuid);
    assert_eq!(rows[1].friendly_name, "fakeprint mono1");
    assert_eq!(
        srv.manager().registry().await.queue_order,
        ["inkjet1", "mono1"]
    );
    srv.shutdown().await;
    // one row per device, now carrying the new address
    let reg = Registry::parse(&std::fs::read_to_string(&reg_path).unwrap()).unwrap();
    assert_eq!(
        reg.devices.len(),
        3,
        "{:?}",
        reg.devices.iter().map(|d| &d.name).collect::<Vec<_>>()
    );
    assert!(reg
        .devices
        .iter()
        .all(|d| d.listen.starts_with("127.0.0.2:")));
}

#[test]
fn duplicate_rows_of_one_device_collapse_to_the_best_one() {
    let mk = |name: &str, listen: &str, seen: Option<&str>| fakeprint::registry::RegistryDevice {
        name: name.into(),
        listen: listen.into(),
        install: fakeprint::registry::Install {
            last_seen_at: seen.map(str::to_string),
            ..Default::default()
        },
        ..Default::default()
    };
    let reg = Registry {
        devices: vec![
            mk("default", "0.0.0.0:3921", Some("2026-10-13T23:21:19Z")),
            mk("mono1", "127.0.0.1:3921", None),
            mk("default", "127.0.0.1:3921", Some("2026-10-13T22:00:00Z")),
            mk("other", "0.0.0.0:9999", None),
        ],
        ..Default::default()
    };
    let mut cfg = ServerConfig::embedded("i");
    cfg.listen = "127.0.0.1:3921".parse().unwrap();
    let (devs, rows) = fakeprint::manager::devices_from_registry(&cfg, &reg);
    let names: Vec<&str> = devs.iter().map(|d| d.id.as_str()).collect();
    assert_eq!(names, ["default", "mono1"]);
    assert_eq!(
        rows[0].1.listen, "127.0.0.1:3921",
        "the exact-address row wins"
    );
}

#[tokio::test]
async fn a_real_windows_wsd_printer_has_a_random_port_and_a_recorded_uuid() {
    let tmp = TempDir::new("ri-winport");
    let srv = start(&tmp).await;
    let addr = srv.local_addr();
    let a = srv.device("a").await.unwrap().info;
    let port = "WSD-3f2b9c1e-7d44-4a55-9e0b-1c2d3e4f5a6b";
    // wrong UUID: refused; right UUID: installed; the payload fields are additive
    let wrong = json!({"windows": {"name": "Fake WSD printer", "port": port,
        "wsd_uuid": "11111111-2222-4333-8444-555555555555"}});
    assert_eq!(
        post_json(addr, "/v1/devices/a/installed", &wrong)
            .await
            .status,
        400
    );
    let ok = json!({"windows": {"name": "Fake WSD printer", "port": port,
        "wsd_uuid": a.uuid.to_string(), "location": a.device_endpoint.clone().unwrap()}});
    let r = post_json(addr, "/v1/devices/a/installed", &ok).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.json()["install"]["state"], "installed");
    assert_eq!(
        r.json()["install"]["windows"]["wsd_uuid"],
        a.uuid.to_string()
    );

    // reconcile by Location alone finds the other device
    let b = srv.device("b").await.unwrap().info;
    let r = post_json(
        addr,
        "/v1/reconcile",
        &json!({"printers": [
            {"name": "Fake WSD printer", "port": port, "wsd_uuid": a.uuid.to_string()},
            {"name": "other", "port": "WSD-0d0d0d0d-7d44-4a55-9e0b-1c2d3e4f5a6b",
             "location": b.device_endpoint.clone().unwrap()},
            {"name": "Brother", "port": "WSD-1d0d0d0d-7d44-4a55-9e0b-1c2d3e4f5a6b"}
        ]}),
    )
    .await;
    assert_eq!(r.json()["installed"], json!(["a", "b"]));
    assert_eq!(r.json()["foreign"][0]["name"], "Brother");
    srv.shutdown().await;
}
