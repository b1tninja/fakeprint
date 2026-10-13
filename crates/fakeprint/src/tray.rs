//! Windows tray shell (`tray-icon` + `native-windows-gui` on its own thread).
//!
//! Compiled only with `--features tray` on Windows. The shell is deliberately dumb: every decision
//! (summary, menu tree, action ids, HTTP mapping, notifications, icon pixels) is in
//! [`crate::tray_model`] and unit-tested there. This file only
//!
//! * polls the daemon's own loopback `GET /v1/devices`, `GET /v1/queue` (every ~2 s) and
//!   `GET /v1/profiles` (every ~30 s),
//! * draws the [`MenuModel`] with `tray-icon`,
//! * runs [`TrayAction::request`] over loopback HTTP, opens the web UI / the reported inbox folder,
//! * raises the detector's notifications through a hidden, detached PowerShell balloon.
//!
//! It never writes the registry, never touches autostart, and never adds a Windows printer (the
//! only Windows action is opening `ms-settings:printers`). The tray thread has its own message
//! loop; an init failure or panic is logged and the daemon carries on headless. The workspace
//! forbids `unsafe`.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use native_windows_gui as nwg;
use tray_icon::menu::{
    CheckMenuItem, IsMenuItem, Menu, MenuEvent, MenuItem as TrayMenuItem, PredefinedMenuItem,
    Submenu,
};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

use crate::tray_model::{
    encode_command, icon_rgba, notify_script, win_label, HttpMethod, HttpRequest, ItemKind, Level,
    MenuId, MenuItem, MenuModel, Notification, ProfileChoice, Summary, TransitionDetector,
    TrayAction, ICON_SIZE,
};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const POLL_EVERY: Duration = Duration::from_secs(2);
const PROFILES_EVERY: Duration = Duration::from_secs(30);

/// What the tray needs from the daemon.
pub struct TrayConfig {
    /// The daemon's actual listen port (loopback is used to reach it).
    pub port: u16,
    /// Called once when the user confirms Quit; must trigger the daemon's graceful shutdown.
    pub quit: Arc<dyn Fn() + Send + Sync>,
}

/// Handle to a running tray thread.
pub struct TrayHandle {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl TrayHandle {
    /// Remove the icon and join the tray thread (bounded wait). Safe to call more than once.
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let deadline = Instant::now() + Duration::from_secs(3);
            while !t.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(50));
            }
            if t.is_finished() {
                let _ = t.join();
            }
        }
    }
}

impl Drop for TrayHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Start the tray on its own thread. Returns `None` (after logging) if the thread cannot be
/// spawned; later failures are logged from the thread and leave the daemon running headless.
pub fn spawn(cfg: TrayConfig) -> Option<TrayHandle> {
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = Arc::clone(&stop);
    let thread = std::thread::Builder::new()
        .name("fakeprint-tray".into())
        .spawn(move || {
            tracing::info!("tray thread started");
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(cfg, stop2)));
            match r {
                Ok(Ok(())) => tracing::info!("tray thread ended"),
                Ok(Err(e)) => tracing::warn!(error = %e, "tray unavailable; continuing headless"),
                Err(_) => tracing::warn!("tray thread panicked; continuing headless"),
            }
        });
    match thread {
        Ok(t) => Some(TrayHandle {
            stop,
            thread: Some(t),
        }),
        Err(e) => {
            tracing::warn!(error = %e, "could not start tray thread; continuing headless");
            None
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Processes
// ---------------------------------------------------------------------------------------------

fn hidden(cmd: &mut Command) -> &mut Command {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
}

fn reap(mut child: std::process::Child) {
    std::thread::spawn(move || {
        let _ = child.wait();
    });
}

fn spawn_notification(n: &Notification) {
    let enc = encode_command(&notify_script(n, 120));
    let mut cmd = Command::new("powershell.exe");
    hidden(&mut cmd).args([
        "-NoProfile",
        "-STA",
        "-ExecutionPolicy",
        "Bypass",
        "-EncodedCommand",
        &enc,
    ]);
    match cmd.spawn() {
        Ok(child) => reap(child),
        Err(e) => tracing::warn!(error = %e, "tray notification helper failed to start"),
    }
}

/// Open the daemon's own web page (`target` is built by us only).
fn open_ui(port: u16) {
    let url = format!("http://127.0.0.1:{port}/ui");
    let mut cmd = Command::new("cmd");
    hidden(&mut cmd).args(["/C", "start", "", &url]);
    match cmd.spawn() {
        Ok(child) => reap(child),
        Err(e) => tracing::warn!(error = %e, "tray: could not open the device manager page"),
    }
}

/// Open a folder in Explorer. The path was reported by the daemon and validated by the model;
/// it is passed as one argument, never through a shell.
fn open_folder(path: &str) {
    let mut cmd = Command::new("explorer.exe");
    hidden(&mut cmd).arg(path);
    match cmd.spawn() {
        Ok(child) => reap(child),
        Err(e) => tracing::warn!(error = %e, "tray: could not open the inbox folder"),
    }
}

// ---------------------------------------------------------------------------------------------
// Loopback HTTP
// ---------------------------------------------------------------------------------------------

fn http(port: u16, raw: &[u8]) -> std::io::Result<Vec<u8>> {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(1))?;
    s.set_read_timeout(Some(Duration::from_secs(3)))?;
    s.set_write_timeout(Some(Duration::from_secs(3)))?;
    s.write_all(raw)?;
    let mut buf = Vec::new();
    s.read_to_end(&mut buf)?;
    Ok(buf)
}

fn get(port: u16, path: &str) -> std::io::Result<Vec<u8>> {
    http(
        port,
        &HttpRequest {
            method: HttpMethod::Get,
            path: path.to_string(),
            body: None,
        }
        .to_bytes(),
    )
}

fn fetch_summary(port: u16) -> Summary {
    match get(port, "/v1/devices") {
        Ok(devices) => {
            let queue = get(port, "/v1/queue").ok();
            Summary::from_http_responses(&devices, queue.as_deref())
        }
        Err(e) => Summary::error(e.to_string()),
    }
}

fn fetch_profiles(port: u16) -> Option<Vec<ProfileChoice>> {
    let raw = get(port, "/v1/profiles").ok()?;
    let v = crate::tray_model::http_body(&raw).ok()?;
    Some(ProfileChoice::list_from_json(&v))
}

/// Run the HTTP side of an action on a worker thread so the menu never blocks.
fn run_request(port: u16, req: HttpRequest) {
    let spawned = std::thread::Builder::new()
        .name("fakeprint-tray-action".into())
        .spawn(move || match http(port, &req.to_bytes()) {
            Ok(resp) => {
                let status = String::from_utf8_lossy(&resp)
                    .lines()
                    .next()
                    .unwrap_or("")
                    .to_string();
                tracing::info!(method = req.method.as_str(), path = %req.path, %status, "tray action");
            }
            Err(e) => tracing::warn!(path = %req.path, error = %e, "tray action failed"),
        });
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "tray: could not start action thread");
    }
}

// ---------------------------------------------------------------------------------------------
// Poller
// ---------------------------------------------------------------------------------------------

#[derive(Clone, PartialEq)]
struct View {
    summary: Summary,
    profiles: Vec<ProfileChoice>,
}

type Latest = Arc<Mutex<Option<View>>>;

fn poller(
    port: u16,
    stop: Arc<AtomicBool>,
    latest: Latest,
    changed: Arc<AtomicBool>,
    wake: nwg::NoticeSender,
) {
    let t0 = Instant::now();
    let mut detector = TransitionDetector::new();
    let mut profiles: Vec<ProfileChoice> = Vec::new();
    let mut profiles_at: Option<Instant> = None;
    while !stop.load(Ordering::SeqCst) {
        if profiles_at.is_none_or(|t| t.elapsed() >= PROFILES_EVERY) {
            if let Some(p) = fetch_profiles(port) {
                profiles = p;
            }
            profiles_at = Some(Instant::now());
        }
        let summary = fetch_summary(port);
        for n in detector.observe(&summary, t0.elapsed()) {
            tracing::info!(kind = ?n.kind, devices = ?n.devices, "tray: notifying");
            spawn_notification(&n);
        }
        let view = View {
            summary,
            profiles: profiles.clone(),
        };
        if let Ok(mut g) = latest.lock() {
            if g.as_ref() != Some(&view) {
                *g = Some(view);
                changed.store(true, Ordering::SeqCst);
            }
        }
        // Always wake the pump so it can see `stop`; cheap.
        wake.notice();
        let until = Instant::now() + POLL_EVERY;
        while Instant::now() < until && !stop.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    wake.notice();
}

// ---------------------------------------------------------------------------------------------
// Drawing the model
// ---------------------------------------------------------------------------------------------

fn make_icon(level: Level) -> Result<Icon, tray_icon::BadIcon> {
    Icon::from_rgba(icon_rgba(level), ICON_SIZE, ICON_SIZE)
}

fn append_items(
    menu: &dyn Fn(&dyn IsMenuItem) -> Result<(), tray_icon::menu::Error>,
    items: &[MenuItem],
) -> Result<(), tray_icon::menu::Error> {
    for i in items {
        if i.kind == ItemKind::Separator {
            menu(&PredefinedMenuItem::separator())?;
        } else if !i.children.is_empty() {
            let sub = Submenu::with_id(i.id.0.as_str(), win_label(&i.label), i.enabled);
            append_items(&|c| sub.append(c), &i.children)?;
            menu(&sub)?;
        } else if let Some(on) = i.checked {
            menu(&CheckMenuItem::with_id(
                i.id.0.as_str(),
                win_label(&i.label),
                i.enabled,
                on,
                None,
            ))?;
        } else {
            menu(&TrayMenuItem::with_id(
                i.id.0.as_str(),
                win_label(&i.label),
                i.enabled,
                None,
            ))?;
        }
    }
    Ok(())
}

fn build_menu(model: &MenuModel) -> Result<Menu, tray_icon::menu::Error> {
    let menu = Menu::new();
    append_items(&|c| menu.append(c), &model.items)?;
    Ok(menu)
}

fn apply(tray: &TrayIcon, v: &View) {
    match make_icon(v.summary.icon_level()) {
        Ok(i) => {
            let _ = tray.set_icon(Some(i));
        }
        Err(e) => tracing::warn!(error = %e, "tray icon"),
    }
    let _ = tray.set_tooltip(Some(v.summary.tooltip()));
    match build_menu(&v.summary.menu(&v.profiles)) {
        Ok(m) => tray.set_menu(Some(Box::new(m))),
        Err(e) => tracing::warn!(error = %e, "tray menu"),
    }
}

// ---------------------------------------------------------------------------------------------
// Tray thread body
// ---------------------------------------------------------------------------------------------

fn handle_click(id: &str, port: u16, latest: &Latest, quit: &Arc<dyn Fn() + Send + Sync>) {
    let Some(action) = MenuId::parse(id) else {
        return;
    };
    match &action {
        TrayAction::OpenUi => open_ui(port),
        TrayAction::OpenInbox { device } => {
            let path = latest.lock().ok().and_then(|g| {
                g.as_ref()
                    .and_then(|v| v.summary.inbox_for(device).map(String::from))
            });
            match path {
                Some(p) => open_folder(&p),
                None => tracing::warn!(%device, "tray: no usable inbox path reported"),
            }
        }
        TrayAction::Quit => {
            tracing::info!("tray: quit confirmed");
            quit();
            nwg::stop_thread_dispatch();
        }
        other => {
            if let Some(req) = other.request() {
                run_request(port, req);
            }
        }
    }
}

fn run(cfg: TrayConfig, stop: Arc<AtomicBool>) -> anyhow::Result<()> {
    nwg::init().map_err(|e| anyhow::anyhow!("nwg init: {e:?}"))?;
    let mut window = nwg::MessageWindow::default();
    nwg::MessageWindow::builder()
        .build(&mut window)
        .map_err(|e| anyhow::anyhow!("message window: {e:?}"))?;
    let mut notice = nwg::Notice::default();
    nwg::Notice::builder()
        .parent(&window)
        .build(&mut notice)
        .map_err(|e| anyhow::anyhow!("notice: {e:?}"))?;

    let initial = View {
        summary: Summary::error("starting"),
        profiles: Vec::new(),
    };
    let tray = TrayIconBuilder::new()
        .with_icon(make_icon(initial.summary.icon_level())?)
        .with_tooltip("fakeprint: starting")
        .with_menu(Box::new(build_menu(&initial.summary.menu(&[]))?))
        .build()?;

    let port = cfg.port;
    let latest: Latest = Arc::new(Mutex::new(None));
    let changed = Arc::new(AtomicBool::new(false));

    {
        let latest = Arc::clone(&latest);
        let quit = Arc::clone(&cfg.quit);
        MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
            handle_click(e.id.0.as_str(), port, &latest, &quit);
        }));
    }

    let poll = {
        let (stop, latest, changed, wake) = (
            Arc::clone(&stop),
            Arc::clone(&latest),
            Arc::clone(&changed),
            notice.sender(),
        );
        std::thread::Builder::new()
            .name("fakeprint-tray-poll".into())
            .spawn(move || poller(port, stop, latest, changed, wake))?
    };

    let notice_handle = notice.handle;
    let stop2 = Arc::clone(&stop);
    let handler = nwg::full_bind_event_handler(&window.handle, move |evt, _data, handle| {
        if evt == nwg::Event::OnNotice && handle == notice_handle {
            if stop2.load(Ordering::SeqCst) {
                nwg::stop_thread_dispatch();
                return;
            }
            if changed.swap(false, Ordering::SeqCst) {
                let view = latest.lock().ok().and_then(|g| g.clone());
                if let Some(v) = view {
                    apply(&tray, &v);
                }
            }
        }
    });
    nwg::dispatch_thread_events();
    nwg::unbind_event_handler(&handler);
    stop.store(true, Ordering::SeqCst);
    let _ = poll.join();
    Ok(())
}
