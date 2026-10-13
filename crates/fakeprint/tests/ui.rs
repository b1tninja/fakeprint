//! The operator page: asset hygiene, the router-facing helpers, and an XSS/behaviour run of the
//! real embedded script against a fake DOM (needs `node`; skipped with a message when it is not
//! on PATH).

#![cfg(feature = "ui")]

use std::path::PathBuf;

use fakeprint::ui::{check_open_settings, is_loopback_peer, profile_summaries, INDEX_HTML};
use serde_json::{json, Value};

fn manifest() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// The page is an operator console served to a browser, so it must stay inert:
///  - no external scripts, styles, images or fonts, no absolute `http(s)` URL anywhere;
///  - no `innerHTML`/`outerHTML`/`insertAdjacentHTML`/`document.write`/`eval`, so server-provided
///    strings can only reach the DOM as text.
#[test]
fn embedded_asset_hygiene() {
    let t = INDEX_HTML;
    assert!(
        !t.contains("http://") && !t.contains("https://"),
        "absolute URL"
    );
    assert!(
        !t.contains("//cdn") && !t.contains("src=\"//"),
        "protocol-relative URL"
    );
    assert!(!t.contains("<script src") && !t.contains("<link") && !t.contains("<iframe"));
    for needle in ["@import", "url(", "<img", "<object", "<embed", "<base"] {
        assert!(!t.contains(needle), "external reference: {needle}");
    }
    for needle in [
        "innerHTML",
        "outerHTML",
        "insertAdjacentHTML",
        "document.write",
        "eval(",
        "new Function",
        "setTimeout(\"",
        "javascript:",
    ] {
        assert!(!t.contains(needle), "unsafe sink: {needle}");
    }
    for needle in [
        "XMLHttpRequest",
        "WebSocket(",
        "EventSource(",
        "sendBeacon",
        "importScripts",
    ] {
        assert!(!t.contains(needle), "unexpected network use: {needle}");
    }
    // Every fetch goes through api() with a relative path.
    assert_eq!(t.matches("fetch(").count(), 1);
    assert!(t.contains("fetch(path, opt)"));
    assert!(t.contains("<title>fakeprint device manager</title>"));
    assert!(t.contains("name=\"viewport\""), "narrow screens");
    assert!(t.contains("max-width: 560px"), "narrow layout rules");
    assert!(t.contains(":focus-visible"), "keyboard focus ring");
}

#[test]
fn page_has_the_documented_controls() {
    for needle in [
        "Add printer",
        "Open Printers &amp; scanners",
        "/v1/devices",
        "/v1/queue",
        "/v1/profiles",
        "/v1/jobs",
        "/v1/levels",
        "/v1/faults",
        "/v1/timing",
        "/v1/reset",
        "/reconfigure",
        "forget=true",
        "open_settings",
        "not_accepting_jobs",
        "Needs re-Add",
        "Add instructions for Windows",
    ] {
        assert!(INDEX_HTML.contains(needle), "page lacks {needle}");
    }
}

#[test]
fn helpers_match_the_router_contract() {
    assert!(is_loopback_peer("127.0.0.1".parse().unwrap()));
    assert!(!is_loopback_peer("10.1.2.3".parse().unwrap()));
    let lo = Some("127.0.0.1".parse().unwrap());
    assert!(check_open_settings(lo, "printers").is_ok());
    assert!(check_open_settings(lo, "bluetooth").is_err());
    let list = profile_summaries(&manifest().join("../../profiles"));
    assert!(list.iter().any(|p| p.id == "generic-wsd"));
}

#[test]
fn embedded_script_parses_with_node() {
    if !node_available() {
        return;
    }
    let start = INDEX_HTML.find("<script>").expect("script open") + "<script>".len();
    let end = INDEX_HTML.rfind("</script>").expect("script close");
    let path = std::env::temp_dir().join(format!("fakeprint-ui-script-{}.js", std::process::id()));
    std::fs::write(&path, &INDEX_HTML[start..end]).unwrap();
    let out = std::process::Command::new("node")
        .arg("--check")
        .arg(&path)
        .output()
        .unwrap();
    let _ = std::fs::remove_file(&path);
    assert!(
        out.status.success(),
        "node --check failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn node_available() -> bool {
    match std::process::Command::new("node").arg("--version").output() {
        Ok(o) if o.status.success() => true,
        _ => {
            eprintln!("node not on PATH; skipping the script checks");
            false
        }
    }
}

const XSS: &str = "<img src=x onerror=alert(1)>";
const XSS_SCRIPT: &str = "<script>alert(2)</script>";
const XSS_JOB: &str = "\"><svg onload=alert(3)>";
const XSS_USER: &str = "' onmouseover='alert(4)";

fn device(id: &str, friendly: &str, state: &str) -> Value {
    json!({
        "id": id, "default": false,
        "uuid": "00000000-0000-4000-8000-0000000000a1",
        "ipp_uuid": "00000000-0000-4000-8000-0000000000a2",
        "profile": "mono-laser", "friendly_name": friendly, "base_path": format!("/d/{id}"),
        "device_endpoint": "http://127.0.0.1:3921/d/x/WebServices/Device",
        "service_endpoint": null, "ipp_endpoint": "ipp://127.0.0.1:3921/d/x/ipp/print",
        "jetdirect_port": null, "inbox_dir": "C:\\inbox\\<b>", "announcing": true,
        "queue_position": 1, "needs_readd": true, "needs_readd_reason": "<u>why</u>",
        "install": {"state": state, "reason": XSS, "last_peer": XSS_USER},
        "config": {"fingerprint": "", "published_fingerprint": "", "class": XSS,
                   "summary": {"formats": ["application/pdf"], "color_modes": ["monochrome"],
                               "sides": ["one-sided"], "media_sizes": ["A4"], "input_bins": ["Tray1"],
                               "versions": {"v11": false, "v12": false, "v20": true}}},
        "live": {
            "printer_state": "Idle", "state_reasons": [XSS], "accepting_jobs": true,
            "active_jobs": 0, "captured_jobs": 1, "active_job": null, "last_job": null,
            "faults": {"paper_out": false, "toner_low": false, "cover_open": false,
                       "paper_jam": false, "offline": false},
            "timing_preset": "default",
            "consumables": [{"name": XSS_SCRIPT, "level": 50}],
            "inputs": [{"name": "Tray1", "level": 100}]
        }
    })
}

#[test]
fn xss_and_behaviour_through_the_real_script() {
    if !node_available() {
        return;
    }
    let devices = json!([
        device("mono", XSS, "\"><script>x</script>"),
        device(XSS_SCRIPT, "hostile id", "installed"),
        device("ok-2", "Fine", "installed"),
    ]);
    let jobs = json!({"jobs": [], "print_jobs": [{
        "id": 7, "name": XSS_JOB, "user": XSS_USER, "state": XSS,
        "documents": [{"seq": 1, "id": 1, "format": "application/pdf<b>", "bytes": 12, "file": XSS}]
    }]});
    let profiles = json!([
        {"id": "generic-wsd", "label": XSS, "description": XSS, "formats": [XSS],
         "color_modes": ["color"], "sides": ["one-sided"], "media_sizes": ["A4"],
         "input_bins": ["Tray1"], "versions": {"v11": false, "v12": false, "v20": true}},
        {"id": "bad id <x>", "label": "dropped"}
    ]);
    let scenario = json!({
        "hostile": [XSS, XSS_SCRIPT, XSS_JOB, XSS_USER, "onerror", "onload", "alert("],
        "routes": {
            "/v1/devices": {"status": 200, "body": devices},
            "/v1/queue": {"status": 200, "body": {"head": "mono", "remaining": [], "position": 1, "total": 2}},
            "/v1/profiles": {"status": 200, "body": profiles},
            "/d/mono/v1/jobs": {"status": 200, "body": jobs},
            "/d/ok-2/v1/jobs": {"status": 200, "body": {"jobs": [], "print_jobs": []}},
            "/d/mono/v1/faults": {"status": 200, "body": {}},
            "/d/mono/v1/timing": {"status": 200, "body": {}},
            "/d/mono/v1/levels": {"status": 200, "body": {}},
            "/d/mono/v1/reset": {"status": 200, "body": {}},
            "/v1/devices/mono?forget=true": {"status": 200, "body": {}},
            "/v1/devices": {"status": 200, "body": devices},
            "/v1/queue/skip": {"status": 200, "body": {}},
            "/v1/queue/advance": {"status": 200, "body": {}},
            "/v1/ctl": {"status": 200, "body": {"opened": true}}
        }
    });
    let dir = std::env::temp_dir().join(format!("fakeprint-ui-xss-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let html = dir.join("index.html");
    let scn = dir.join("scenario.json");
    std::fs::write(&html, INDEX_HTML).unwrap();
    std::fs::write(&scn, scenario.to_string()).unwrap();
    let out = std::process::Command::new("node")
        .arg(manifest().join("tests/ui_harness/fake_dom.js"))
        .arg(&html)
        .arg(&scn)
        .output()
        .expect("run node");
    let _ = std::fs::remove_dir_all(&dir);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "harness failed: {stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let r: Value = serde_json::from_str(stdout.trim()).unwrap_or_else(|e| panic!("{e}: {stdout}"));
    assert!(r.get("crash").is_none(), "{r}");

    // Nothing hostile became markup or an attribute value.
    assert_eq!(r["violations"], json!([]), "{r}");
    // The hostile strings are shown, as plain text.
    let text = r["devicesText"].as_str().unwrap();
    for t in [XSS, XSS_SCRIPT, XSS_JOB, XSS_USER] {
        assert!(text.contains(t), "{t} should be visible as text");
    }
    // The printer with a hostile id is dropped (2 cards) and never requested.
    assert_eq!(r["cards"], 2);
    for p in r["allPaths"].as_array().unwrap() {
        let p = p.as_str().unwrap();
        assert!(p.starts_with('/'), "relative paths only: {p}");
        assert!(!p.contains('<') && !p.contains(' '), "{p}");
    }
    assert!(r["errorBanner"].as_str().unwrap().contains("unusable id"));
    // Queue guidance is the contract text with the head's friendly name.
    let q = r["queueText"].as_str().unwrap();
    assert!(
        q.starts_with(&format!(
            "Add now: {XSS} (1 of 2). Open Settings > Bluetooth & devices"
        )),
        "{q}"
    );
    assert!(q.ends_with("only after this one is installed."));
    assert_eq!(r["queueHidden"], false);
    // Profiles with unsafe ids are dropped from the table.
    assert_eq!(r["profilesRows"], 1);

    // Interactions go through the documented endpoints with the documented bodies.
    let i = &r["interactions"];
    let one = |k: &str| -> (String, Value) {
        let a = i[k].as_array().unwrap_or_else(|| panic!("{k}: {r}"));
        assert_eq!(a.len(), 1, "{k}: {a:?}");
        let body = a[0]["body"]
            .as_str()
            .map(|s| serde_json::from_str(s).unwrap())
            .unwrap_or(Value::Null);
        (a[0]["path"].as_str().unwrap().to_string(), body)
    };
    assert_eq!(
        one("fault"),
        ("/d/mono/v1/faults".into(), json!({"toner_low": true}))
    );
    assert_eq!(
        one("timing"),
        ("/d/mono/v1/timing".into(), json!({"preset": "instant"}))
    );
    assert_eq!(
        one("levels"),
        (
            "/d/mono/v1/levels".into(),
            json!({"consumables": {XSS_SCRIPT: 15}, "inputs": {}})
        )
    );
    assert_eq!(one("reset").0, "/d/mono/v1/reset");
    assert_eq!(
        i["addBad"],
        json!([]),
        "a bad id is refused before any request"
    );
    assert!(i["addBadError"]
        .as_str()
        .unwrap()
        .contains("letters, digits"));
    let (path, body) = one("addGood");
    assert_eq!(path, "/v1/devices");
    assert_eq!(body["profile"], "generic-wsd");
    assert_eq!(body["id"], "generic-wsd", "suggested id from the profile");
    assert!(i["removeText"].as_str().unwrap().contains("mono"));
    let rm = i["remove"].as_array().unwrap();
    assert_eq!(rm.len(), 1);
    assert_eq!(rm[0]["path"], "/v1/devices/mono?forget=true");
    let btn: Vec<(&str, Value)> = i["buttons"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            (
                c["path"].as_str().unwrap(),
                c["body"]
                    .as_str()
                    .map(|s| serde_json::from_str(s).unwrap())
                    .unwrap_or(Value::Null),
            )
        })
        .collect();
    assert_eq!(btn[0].0, "/v1/queue/skip");
    assert_eq!(btn[1].0, "/v1/queue/advance");
    assert_eq!(btn[2].0, "/v1/ctl");
    assert_eq!(
        btn[2].1,
        json!({"cmd": "open_settings", "args": {"page": "printers"}})
    );
}
