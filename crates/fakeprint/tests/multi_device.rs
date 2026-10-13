//! Multi-device contract, M0 slice: config resolution and the default device's `/v1/devices` row.
//! (The multi-device behaviour tests of docs/multi-device.md section 10 arrive with M1.)

mod common;

use common::*;
use fakeprint::server::{serve, DeviceConfig, ServerConfig};
use uuid::Uuid;

#[test]
fn empty_devices_synthesize_the_legacy_default() {
    let mut cfg = ServerConfig::embedded("inbox");
    cfg.profile_name = "mono-laser".into();
    let d = cfg.resolved_devices().unwrap();
    assert_eq!(d.len(), 1);
    assert_eq!(d[0].id, "default");
    assert!(d[0].is_default);
    assert_eq!(d[0].base_path, "");
    assert_eq!(d[0].profile, "mono-laser");
    assert_eq!(d[0].device_uuid, cfg.device_uuid);
    assert_eq!(d[0].inbox_dir, std::path::PathBuf::from("inbox"));
    assert!(d[0].friendly_name.is_none());
}

#[test]
fn explicit_devices_inherit_and_derive() {
    let mut cfg = ServerConfig::embedded("inbox").with_devices(vec![
        DeviceConfig::derived("mono", "mono-laser"),
        DeviceConfig::derived("color", "color-inkjet"),
    ]);
    cfg.timing.processing_base = std::time::Duration::from_millis(7);
    let d = cfg.resolved_devices().unwrap();
    assert_eq!(d[0].base_path, "");
    assert_eq!(d[1].base_path, "/d/color");
    assert_eq!(
        d[1].inbox_dir,
        std::path::PathBuf::from("inbox").join("color")
    );
    assert_eq!(d[1].friendly_name.as_deref(), Some("fakeprint color"));
    assert_eq!(d[1].timing.processing_base, cfg.timing.processing_base);
    assert_ne!(d[0].device_uuid, d[1].device_uuid);
    assert_ne!(d[0].ipp_uuid, d[0].device_uuid);
}

#[test]
fn validation_rejects_bad_sets() {
    let ok = |devs: Vec<DeviceConfig>| {
        ServerConfig::embedded("i")
            .with_devices(devs)
            .resolved_devices()
    };
    assert!(ok(vec![DeviceConfig::derived("bad id", "p")]).is_err());
    assert!(ok(vec![
        DeviceConfig::derived("a", "p"),
        DeviceConfig::derived("a", "p")
    ])
    .is_err());
    let u = Uuid::parse_str("00000000-0000-4000-8000-0000000000a1").unwrap();
    let v = Uuid::parse_str("00000000-0000-4000-8000-0000000000a2").unwrap(); // = ipp uuid of u
    assert!(ok(vec![
        DeviceConfig::new("a", "p", u),
        DeviceConfig::new("b", "p", u)
    ])
    .is_err());
    assert!(ok(vec![
        DeviceConfig::new("a", "p", u),
        DeviceConfig::new("b", "p", v)
    ])
    .is_err());
    let mut named = DeviceConfig::derived("a", "p");
    named.friendly_name = Some("Brother HL".into());
    assert!(ok(vec![named]).is_err());
    let mut named = DeviceConfig::derived("a", "p");
    named.friendly_name = Some("FakePrint Lab".into());
    assert!(ok(vec![named]).is_ok());
}

#[tokio::test]
async fn v1_devices_lists_the_default_device() {
    let tmp = TempDir::new("md-default");
    let mut cfg = ServerConfig::embedded(tmp.path().join("inbox"));
    cfg.profile =
        Some(fakeprint::profile::PrinterProfile::load(&profiles_dir(), "generic-wsd").unwrap());
    let srv = serve(cfg).await.unwrap();
    let rows = get_json(srv.local_addr(), "/v1/devices").await;
    let rows = rows.as_array().unwrap();
    assert_eq!(rows.len(), 1);
    let d = &rows[0];
    assert_eq!(d["id"], "default");
    assert_eq!(d["default"], true);
    assert_eq!(d["base_path"], "");
    assert_eq!(d["uuid"], srv.device_uuid().to_string());
    assert_eq!(d["ipp_uuid"], srv.ipp_uuid().to_string());
    assert!(d["device_endpoint"]
        .as_str()
        .unwrap()
        .ends_with("/WebServices/Device"));
    assert!(d["ipp_endpoint"].as_str().unwrap().starts_with("ipp://"));
    assert!(d["ipp_endpoint"].as_str().unwrap().ends_with("/ipp/print"));
    assert_eq!(d["install"]["state"], "new");
    assert_eq!(d["needs_readd"], false);
    assert_eq!(d["live"]["printer_state"], "Idle");
    assert_eq!(d["live"]["faults"]["offline"], false);
    assert_eq!(d["live"]["timing_preset"], "default");
    assert!(d["config"]["published_fingerprint"]
        .as_str()
        .unwrap()
        .starts_with("fnv64:"));
    assert_eq!(d["config"]["class"].as_str().unwrap().len(), 12);

    // the manager serves the same data and the stubs say NotYet
    let m = srv.manager();
    assert_eq!(m.list().await.len(), 1);
    assert!(m.get("nope").await.is_none());
    assert!(m.default_device().await.is_some());
    let err = m.remove("default").await.unwrap_err();
    assert_eq!(fakeprint::manager::error_status(&err), 400);
    assert!(m.mark_probed("default", "x").await.unwrap());
    assert_eq!(m.queue().await.total, 0);

    // a fault shows up in the live block
    post_json(
        srv.local_addr(),
        "/v1/faults",
        &serde_json::json!({"paper_out": true, "not_accepting_jobs": true}),
    )
    .await;
    let d = &srv.devices().await[0];
    assert!(d.live.faults.paper_out && d.live.faults.offline && !d.live.accepting_jobs);
    srv.shutdown().await;
}
