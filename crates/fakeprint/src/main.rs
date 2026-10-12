//! fakeprint binary: control plane + WSD Print Service + discovery.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{ConnectInfo, State};
use axum::http::Request;
use axum::routing::{get, post};
use axum::{Json, Router};
use clap::Parser;
use fakeprint::discovery::DiscoveryConfig;
use fakeprint::lab::{self, LAB_DEVICE_UUID, LAB_IPP_UUID, LAB_NAME};
use fakeprint::profile::Profile;
use fakeprint::state::{Status, World};
use fakeprint::{discovery, ipp, wsd};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tower_http::trace::TraceLayer;
use tracing::{info, info_span, warn};
use uuid::Uuid;

#[derive(Debug, Parser)]
#[command(
    name = "fakeprint",
    about = "Fake WSD / IPP printer for Windows testing"
)]
struct Args {
    /// Listen address for the control API and Print Service.
    /// Default 0.0.0.0:3921 (fakeprint lab port). Use a LAN IP for multi-host.
    #[arg(long, default_value = "0.0.0.0:3921")]
    listen: SocketAddr,

    /// Advertise this host in WS-Discovery XAddrs (defaults to first LAN IPv4
    /// when listen is 0.0.0.0, otherwise the listen IP).
    #[arg(long)]
    advertise: Option<IpAddr>,

    /// Disable WS-Discovery / Print Service (control API only).
    #[arg(long)]
    no_wsd: bool,

    /// Directory of capability profiles (JSON).
    #[arg(long, default_value = "profiles")]
    profiles: PathBuf,

    /// Profile id to load at start.
    #[arg(long, default_value = "generic-wsd")]
    profile: String,

    /// Directory where captured print jobs are written.
    #[arg(long, default_value = "inbox")]
    inbox: PathBuf,

    /// Stable device UUID for discovery.
    /// Defaults to the canonical lab UUID (…000f3). Random with --device-uuid $(uuidgen).
    #[arg(long, default_value = LAB_DEVICE_UUID)]
    device_uuid: Uuid,

    /// UUID returned in IPP Get-Printer-Attributes (`printer-uuid`).
    ///
    /// Must differ from `--device-uuid` (the WSD UUID). When Windows
    /// `Add-Printer -IppURL` gets a uuid that already appears in
    /// `SWD\DAFWSDProvider` (the WSD device tree) it returns "printer already
    /// exists" and refuses to create an Internet Port — even with a different
    /// printer name. A distinct IPP UUID sidesteps this check.
    ///
    /// Default: last byte of `--device-uuid` incremented by 1
    /// (…000f3 → …000f4 = `LAB_IPP_UUID`).
    #[arg(long, default_value = LAB_IPP_UUID)]
    ipp_uuid: Uuid,

    /// Bind UDP :3702 to answer WS-Discovery Probes.
    #[arg(long, default_value_t = !cfg!(windows))]
    probe_listen: bool,

    /// Force Hello-only mode (never bind :3702).
    #[arg(long)]
    no_probe_listen: bool,

    /// Enable IPP on `/ipp/print` (Get-Printer-Attributes). Default on.
    #[arg(long, default_value_t = true)]
    ipp: bool,

    /// Disable IPP routes (WSD-only).
    #[arg(long)]
    no_ipp: bool,

    /// Listen for Windows TCPMON RAW jobs (JetDirect AppSocket). Default :9100.
    /// Needed because Add-PrinterPort -PrinterHostAddress always creates RAW :9100.
    #[arg(long, default_value_t = 9100)]
    jetdirect_port: u16,

    /// Disable the JetDirect RAW listener.
    #[arg(long)]
    no_jetdirect: bool,

    /// Lab instance name (registry row key; sets named-pipe suffix).
    /// Override via FAKEPRINT_NAME env var.
    #[arg(long, env = "FAKEPRINT_NAME", default_value = LAB_NAME)]
    lab_name: String,

    /// Skip the single-instance lock (advanced; may confuse WSDMON).
    #[arg(long)]
    no_lock: bool,

    /// Verbose logs.
    #[arg(short = 'v', long)]
    verbose: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        if args.verbose {
            "fakeprint=debug,tower_http=debug".into()
        } else {
            "fakeprint=info,tower_http=info".into()
        }
    });
    tracing_subscriber::fmt().with_env_filter(filter).init();

    // --- Single-instance lock ---
    let _lock = if args.no_lock {
        None
    } else {
        match lab::acquire_instance_lock(args.listen.port()) {
            Ok(l) => Some(l),
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(1);
            }
        }
    };

    let wsd_on = !args.no_wsd;
    let profile = Profile::load(&args.profiles, &args.profile)?;
    info!(id = %profile.id, "loaded profile");

    let device_uuid = args.device_uuid;
    // Resolve the IPP UUID: use the CLI value unless it matches the WSD UUID
    // (which would re-introduce the DAFWSDProvider collision).  If they are
    // equal fall back to the automatic +1 derivation so the default is safe
    // even when someone passes `--ipp-uuid` equal to `--device-uuid`.
    let ipp_uuid = if args.ipp_uuid == device_uuid {
        lab::derive_ipp_uuid(device_uuid)
    } else {
        args.ipp_uuid
    };

    let discovery_stats = Arc::new(discovery::DiscoveryStats::default());
    let mut world_inner = World::new(
        args.profiles.clone(),
        args.inbox.clone(),
        profile,
        Arc::clone(&discovery_stats),
    )?;
    world_inner.set_lab_name(&args.lab_name);
    world_inner.set_ipp_uuid(ipp_uuid);

    let mut endpoint = None;
    if wsd_on {
        let adv_ip = args
            .advertise
            .unwrap_or_else(|| args.listen.ip())
            // If listening on all interfaces, pick first LAN IPv4.
            .pipe_if_unspecified();
        let device_ep = format!(
            "http://{}:{}/WebServices/Device",
            adv_ip,
            args.listen.port()
        );
        let print_ep = format!(
            "http://{}:{}/WebServices/PrinterService",
            adv_ip,
            args.listen.port()
        );
        world_inner.set_identity(device_ep.clone(), print_ep, device_uuid);
        endpoint = Some(device_ep);
    }
    let world = Arc::new(RwLock::new(world_inner));
    let listen_probes = args.probe_listen && !args.no_probe_listen;

    let mut app = Router::new()
        .route("/", get(presentation))
        .route("/health", get(health))
        .route("/v1/status", get(status))
        .route("/v1/reset", post(reset))
        .route("/v1/jobs", get(list_jobs))
        .route("/v1/profile", post(set_profile))
        .route("/v1/ctl", get(ctl_get).post(ctl_post));

    if args.ipp && !args.no_ipp {
        app = app
            .route(
                "/ipp/print",
                get(ipp::print_handler).post(ipp::print_handler),
            )
            .route(
                "/ipp/print/",
                get(ipp::print_handler).post(ipp::print_handler),
            );
        info!("IPP enabled at /ipp/print (Get-Printer-Attributes)");
    }

    if wsd_on {
        app = app
            .route("/WebServices/Device", post(wsd::print_service))
            .route("/WebServices/Device/", post(wsd::print_service))
            .route("/WebServices/PrinterService", post(wsd::print_service))
            .route("/WebServices/PrinterService/", post(wsd::print_service))
            .route("/PrinterService", post(wsd::print_service))
            .route("/PrinterService/", post(wsd::print_service))
            .route("/PrintService", post(wsd::print_service))
            .route("/PrintService/", post(wsd::print_service))
            .route("/WebServices/PrintService", post(wsd::print_service))
            .route("/WebServices/PrintService/", post(wsd::print_service));
    }

    let app = app
        .layer(
            TraceLayer::new_for_http().make_span_with(|req: &Request<_>| {
                let peer = req
                    .extensions()
                    .get::<ConnectInfo<SocketAddr>>()
                    .map(|ConnectInfo(a)| a.to_string())
                    .unwrap_or_else(|| "-".into());
                info_span!(
                    "http",
                    method = %req.method(),
                    uri = %req.uri(),
                    peer = %peer,
                )
            }),
        )
        .with_state(Arc::clone(&world));

    // --- Named pipe (Windows only) ---
    #[cfg(windows)]
    {
        let pipe_world = Arc::clone(&world);
        let pipe_name = args.lab_name.clone();
        tokio::spawn(async move {
            serve_named_pipe(&pipe_name, pipe_world).await;
        });
    }

    let mut discovery_task = None;
    let mut discovery_stop = None;
    if let Some(ep) = endpoint {
        let multicast_iface = match args.advertise.unwrap_or_else(|| args.listen.ip()) {
            IpAddr::V4(v4) if !v4.is_unspecified() => v4,
            _ => Ipv4Addr::UNSPECIFIED,
        };
        let cfg = DiscoveryConfig {
            endpoint: ep.clone(),
            device_uuid,
            friendly_name: "fakeprint".into(),
            multicast_iface,
            listen_probes,
            stats: discovery_stats,
            instance_id: 0,
            metadata_version: 0,
        };
        info!(
            endpoint = %ep,
            %multicast_iface,
            listen_probes,
            "WSD Print Service + WS-Discovery enabled"
        );
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
        discovery_stop = Some(stop_tx);
        discovery_task = Some(tokio::spawn(async move {
            if let Err(e) = discovery::run(cfg, stop_rx).await {
                tracing::error!(error = %e, "WS-Discovery stopped");
            }
        }));
    }

    if !args.no_jetdirect && args.jetdirect_port != 0 {
        let jd_world = Arc::clone(&world);
        let jd_bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), args.jetdirect_port);
        tokio::spawn(async move {
            fakeprint::jetdirect::serve(jd_world, jd_bind).await;
        });
    }

    info!(listen = %args.listen, name = %args.lab_name, verbose = args.verbose, "listening");
    let listener = lab::bind_listener(args.listen).await?;
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        shutdown_signal().await;
        if let Some(tx) = discovery_stop {
            let _ = tx.send(());
        }
    })
    .await?;

    if let Some(task) = discovery_task {
        let _ = task.await;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Extension trait: resolve unspecified IP to a LAN address.
// ---------------------------------------------------------------------------

trait PipeIfUnspecified {
    fn pipe_if_unspecified(self) -> IpAddr;
}

impl PipeIfUnspecified for IpAddr {
    fn pipe_if_unspecified(self) -> IpAddr {
        if self.is_unspecified() {
            lab::first_lan_ipv4()
                .map(IpAddr::V4)
                .unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST))
        } else {
            self
        }
    }
}

// ---------------------------------------------------------------------------
// HTTP handlers
// ---------------------------------------------------------------------------

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

async fn health() -> &'static str {
    "ok"
}

/// Minimal page for ThisModel PresentationUrl (Settings "Acquiring data").
async fn presentation(State(world): State<Arc<RwLock<World>>>) -> axum::response::Html<String> {
    let label = world.read().await.profile().label.clone();
    axum::response::Html(format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>{label}</title></head>\
         <body><h1>{label}</h1><p>fakeprint lab presentation</p></body></html>"
    ))
}

async fn status(State(world): State<Arc<RwLock<World>>>) -> Json<Status> {
    Json(world.read().await.status())
}

async fn reset(State(world): State<Arc<RwLock<World>>>) -> Json<Status> {
    let mut w = world.write().await;
    w.reset();
    Json(w.status())
}

async fn list_jobs(State(world): State<Arc<RwLock<World>>>) -> Json<serde_json::Value> {
    let w = world.read().await;
    Json(serde_json::json!({ "jobs": w.captured() }))
}

// ---------------------------------------------------------------------------
// POST /v1/profile  (hot-swap profile)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ProfileBody {
    id: String,
}

async fn set_profile(
    State(world): State<Arc<RwLock<World>>>,
    Json(body): Json<ProfileBody>,
) -> Result<Json<Status>, (axum::http::StatusCode, String)> {
    let mut w = world.write().await;
    w.hot_swap_profile(&body.id)
        .map_err(|e| (axum::http::StatusCode::BAD_REQUEST, e.to_string()))?;
    Ok(Json(w.status()))
}

// ---------------------------------------------------------------------------
// GET /v1/ctl  — identity + command list
// POST /v1/ctl { "cmd": "…", "args": {…} }
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct CtlInfo {
    name: String,
    port: u16,
    uuid: String,
    base_url: String,
    pipe: String,
    cmds: Vec<&'static str>,
}

async fn ctl_get(State(world): State<Arc<RwLock<World>>>) -> Json<CtlInfo> {
    let w = world.read().await;
    let st = w.status();
    // Extract port from device_endpoint or fall back to lab default.
    let port = st
        .device_endpoint
        .split(':')
        .next_back()
        .and_then(|s| s.split('/').next())
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(lab::LAB_PORT);
    let name = st.name.clone();
    let pipe = lab::pipe_path(&name);
    Json(CtlInfo {
        name,
        port,
        uuid: st.device_uuid.to_string(),
        base_url: format!("http://127.0.0.1:{port}"),
        pipe,
        cmds: vec!["status", "health", "jobs", "reset", "profile", "save"],
    })
}

#[derive(Debug, Deserialize)]
struct CtlRequest {
    cmd: String,
    #[serde(default)]
    args: serde_json::Value,
}

async fn ctl_post(
    State(world): State<Arc<RwLock<World>>>,
    Json(req): Json<CtlRequest>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    dispatch_ctl(&req.cmd, &req.args, &world).await
}

/// Shared dispatcher used by both HTTP and named-pipe handlers.
async fn dispatch_ctl(
    cmd: &str,
    args: &serde_json::Value,
    world: &Arc<RwLock<World>>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    match cmd {
        "status" => {
            let w = world.read().await;
            Ok(Json(serde_json::to_value(w.status()).unwrap()))
        }
        "health" => Ok(Json(serde_json::json!({"ok": true}))),
        "jobs" => {
            let w = world.read().await;
            Ok(Json(serde_json::json!({"jobs": w.captured()})))
        }
        "reset" => {
            let mut w = world.write().await;
            w.reset();
            Ok(Json(serde_json::to_value(w.status()).unwrap()))
        }
        "profile" => {
            let id = args
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if id.is_empty() {
                return Err((
                    axum::http::StatusCode::BAD_REQUEST,
                    "args.id required".into(),
                ));
            }
            let mut w = world.write().await;
            w.hot_swap_profile(&id)
                .map_err(|e| (axum::http::StatusCode::BAD_REQUEST, e.to_string()))?;
            Ok(Json(serde_json::to_value(w.status()).unwrap()))
        }
        "save" => {
            // Registry write is a PowerShell/Python CLI responsibility.
            // The process exposes its current config via /v1/status for the script to read.
            Ok(Json(serde_json::json!({
                "ok": true,
                "note": "use: .\\scripts\\fakeprint.ps1 save  (writes current status to registry)"
            })))
        }
        other => Err((
            axum::http::StatusCode::BAD_REQUEST,
            format!("unknown cmd {other:?}; known: status health jobs reset profile save"),
        )),
    }
}

// ---------------------------------------------------------------------------
// Named pipe (Windows only)
// ---------------------------------------------------------------------------

/// Serve `\\.\pipe\fakeprint-<name>` — one JSONL ctl request per line.
/// Non-Windows: compiled out.
#[cfg(windows)]
async fn serve_named_pipe(name: &str, world: Arc<RwLock<World>>) {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::windows::named_pipe::ServerOptions;

    let path = lab::pipe_path(name);
    info!(pipe = %path, "named pipe listening");
    let mut first = true;
    loop {
        let server = {
            let mut opts = ServerOptions::new();
            if first {
                opts.first_pipe_instance(true);
                first = false;
            }
            match opts.create(&path) {
                Ok(s) => s,
                Err(e) => {
                    warn!(error = %e, pipe = %path, "named pipe create failed; retrying in 2s");
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    first = true; // Try first_pipe_instance again after error
                    continue;
                }
            }
        };

        // Wait for a client to connect.
        if server.connect().await.is_err() {
            continue;
        }

        let w = Arc::clone(&world);
        tokio::spawn(async move {
            let (reader, mut writer) = tokio::io::split(server);
            let mut lines = BufReader::new(reader).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let line = line.trim().to_string();
                if line.is_empty() {
                    continue;
                }
                let resp = handle_pipe_line(&line, &w).await;
                if writer.write_all(resp.as_bytes()).await.is_err() {
                    break;
                }
                if writer.write_all(b"\n").await.is_err() {
                    break;
                }
            }
        });
    }
}

#[cfg(windows)]
async fn handle_pipe_line(line: &str, world: &Arc<RwLock<World>>) -> String {
    match serde_json::from_str::<CtlRequest>(line) {
        Err(e) => serde_json::json!({"error": format!("parse: {e}")}).to_string(),
        Ok(req) => match dispatch_ctl(&req.cmd, &req.args, world).await {
            Ok(Json(v)) => v.to_string(),
            Err((_, msg)) => serde_json::json!({"error": msg}).to_string(),
        },
    }
}
