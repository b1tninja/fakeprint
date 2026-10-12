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
use fakeprint::profile::Profile;
use fakeprint::state::{Status, World};
use fakeprint::{discovery, ipp, wsd};
use tokio::sync::RwLock;
use tower_http::trace::TraceLayer;
use tracing::{info, info_span};
use uuid::Uuid;

#[derive(Debug, Parser)]
#[command(name = "fakeprint", about = "Fake WSD / IPP printer for Windows testing")]
struct Args {
    /// Listen address for the control API and Print Service.
    #[arg(long, default_value = "127.0.0.1:3921")]
    listen: SocketAddr,

    /// Advertise this host in WS-Discovery XAddrs (defaults to listen IP).
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

    /// Stable device UUID for discovery (random if omitted).
    #[arg(long)]
    device_uuid: Option<Uuid>,

    /// Bind UDP :3702 to answer WS-Discovery Probes.
    #[arg(long, default_value_t = !cfg!(windows))]
    probe_listen: bool,

    /// Force Hello-only mode (never bind :3702).
    #[arg(long)]
    no_probe_listen: bool,

    /// Enable IPP stub route (`/ipp/print` → 501). Phase 2 will implement IPP Everywhere.
    #[arg(long)]
    ipp: bool,

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

    let wsd_on = !args.no_wsd;
    let profile = Profile::load(&args.profiles, &args.profile)?;
    info!(id = %profile.id, "loaded profile");

    let device_uuid = args.device_uuid.unwrap_or_else(Uuid::new_v4);
    let discovery_stats = Arc::new(discovery::DiscoveryStats::default());
    let mut world_inner = World::new(
        args.profiles.clone(),
        args.inbox.clone(),
        profile,
        Arc::clone(&discovery_stats),
    )?;
    let mut endpoint = None;
    if wsd_on {
        let adv_ip = args.advertise.unwrap_or_else(|| args.listen.ip());
        let adv_ip = if adv_ip.is_unspecified() {
            IpAddr::V4(Ipv4Addr::LOCALHOST)
        } else {
            adv_ip
        };
        let device_ep = format!("http://{}:{}/WebServices/Device", adv_ip, args.listen.port());
        // Brother advertises /WebServices/PrinterService + ServiceId …/PrinterService.
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
        .route("/v1/jobs", get(list_jobs));

    if args.ipp {
        app = app
            .route("/ipp/print", get(ipp::print_stub).post(ipp::print_stub))
            .route("/ipp/print/", get(ipp::print_stub).post(ipp::print_stub));
        info!("IPP stub enabled at /ipp/print (501 Not Implemented)");
    }

    if wsd_on {
        app = app
            .route("/WebServices/Device", post(wsd::print_service))
            .route("/WebServices/Device/", post(wsd::print_service))
            // Canonical Brother-shaped hosted path + legacy aliases.
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
        .with_state(world);

    let mut discovery_task = None;
    let mut discovery_stop = None;
    if let Some(endpoint) = endpoint {
        let multicast_iface = match args.advertise.unwrap_or_else(|| args.listen.ip()) {
            IpAddr::V4(v4) if !v4.is_unspecified() => v4,
            _ => Ipv4Addr::UNSPECIFIED,
        };
        let cfg = DiscoveryConfig {
            endpoint: endpoint.clone(),
            device_uuid,
            friendly_name: "fakeprint".into(),
            multicast_iface,
            listen_probes,
            stats: discovery_stats,
            instance_id: 0,
            metadata_version: 0,
        };
        info!(
            %endpoint,
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

    info!(%args.listen, verbose = args.verbose, "listening");
    let listener = tokio::net::TcpListener::bind(args.listen).await?;
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
