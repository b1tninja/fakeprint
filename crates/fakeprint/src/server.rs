//! The fakeprint server as a library: control-plane router, WSD Print Service, IPP, and the
//! background tasks that keep the printer alive (timers, event delivery, WS-Discovery).
//!
//! `main.rs` is argument parsing around [`serve`]. An embedder (the Python binding, a test)
//! calls the same function with [`ServerConfig::embedded`] and gets the exact server the lab
//! runs, in process, on an ephemeral loopback port, with
//!
//! * no multicast, no WS-Discovery, no JetDirect listener, no named pipe, no instance lock,
//!   no Ctrl-C handler;
//! * no file written anywhere except the configured inbox directory;
//! * a graceful [`ServerHandle::shutdown`] that ends every subscription and stops every task.
//!
//! Control plane (JSON): `GET /health`, `GET /v1/status`, `GET /v1/jobs`, `GET /v1/jobs/{id}`,
//! `POST /v1/reset`, `POST /v1/profile`, `GET|POST /v1/ctl`, `GET|POST|DELETE /v1/faults`,
//! `GET|POST /v1/timing`.
//!
//! `POST /v1/faults` also takes request-injection rules (`"inject": [...]`, `"inject_clear"`),
//! see `wsd::inject`; they apply to the WSD (and, per rule, IPP) routes and never to `/v1/*`.
//! `POST /v1/timing` switches job processing times at run time (`{"preset":"instant"}`).
//!
//! To mount the server inside another axum application use [`ServerHandle::router`] (the same
//! routes, sharing the running server's state) or build one yourself with [`router`] and
//! [`RouterOptions`].

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use axum::body::Bytes;
use axum::extract::{ConnectInfo, Extension, Path as UrlPath, State};
use axum::http::{header, HeaderMap, Request, StatusCode};
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tokio::sync::{oneshot, RwLock};
use tokio::task::{AbortHandle, JoinHandle};
use tower_http::trace::TraceLayer;
use tracing::{info, info_span, warn};
use uuid::Uuid;

use crate::discovery::{self, DiscoveryConfig, DiscoveryService, DiscoveryStats};
use crate::lab;
use crate::profile::PrinterProfile;
use crate::state::{Status, World, WorldOptions};
use crate::wsd::ctx::{
    spawn_ticker, Clock, FaultBody, PrintCtx, PrintLimits, PrintTiming, TimingPatch,
};
use crate::wsd::eventing::{AddressPolicy, EventManager, HttpDeliver};
use crate::wsd::inject::{InjectQuery, InjectRule, Injector, PendingRule, RuleEndpoint};
use crate::wsd::soap::SoapRequest;
use crate::wsd::validate::{ReplyValidator, ValidatingDeliver};
use crate::wsd::{self, SharedWorld, WSD_PATHS};
use crate::{ipp, jetdirect};

/// Tick of the job timers and of the event pump.
pub const DEFAULT_TICK: Duration = Duration::from_millis(250);

/// What only the daemon (the `fakeprint` binary) turns on. Every field is ignored when
/// [`ServerConfig::embedded`] is set.
#[derive(Debug, Clone)]
pub struct DaemonOptions {
    /// Announce on WS-Discovery (multicast Hello/Bye, Probe answers).
    pub discovery: bool,
    /// Bind UDP :3702 to answer Probes.
    pub probe_listen: bool,
    /// JetDirect RAW listener port (`None` = off).
    pub jetdirect_port: Option<u16>,
    /// Windows named-pipe control surface.
    pub named_pipe: bool,
    /// Per-port single-instance lock (skipped for ephemeral ports).
    pub instance_lock: bool,
}

impl Default for DaemonOptions {
    fn default() -> Self {
        Self {
            discovery: true,
            probe_listen: !cfg!(windows),
            jetdirect_port: Some(9100),
            named_pipe: true,
            instance_lock: true,
        }
    }
}

/// Everything [`serve`] needs, already resolved (CLI parsing is the caller's job).
#[derive(Clone)]
pub struct ServerConfig {
    /// Listen address; port 0 picks a free port (see [`ServerHandle::port`]).
    pub listen: SocketAddr,
    /// Allow a listen address that is not loopback. Without it [`serve`] refuses.
    pub allow_non_loopback: bool,
    /// Host advertised in the endpoint URLs and WS-Discovery; default: the listen IP (a LAN
    /// IPv4 when listening on all interfaces).
    pub advertise: Option<IpAddr>,
    /// Library use: switches off everything that touches the network beyond the listener or
    /// the machine beyond the inbox (see the module documentation) and ignores `daemon`.
    pub embedded: bool,
    /// Directory of `<id>.json` profiles (for `/v1/profile` and loading by name).
    pub profile_dir: Option<PathBuf>,
    /// Profile id to load from `profile_dir`.
    pub profile_name: String,
    /// A ready profile; wins over `profile_dir`/`profile_name`. With neither a directory nor
    /// a profile the stock [`PrinterProfile::generic`] printer is used (embedded only).
    pub profile: Option<PrinterProfile>,
    /// Where captured documents go (created if missing).
    pub inbox_dir: PathBuf,
    pub device_uuid: Uuid,
    /// IPP `printer-uuid`; default: derived from `device_uuid` (never equal to it).
    pub ipp_uuid: Option<Uuid>,
    /// Lab instance name (registry row, named pipe).
    pub lab_name: String,
    /// Serve the WSD endpoints.
    pub wsd: bool,
    /// Serve `/ipp/print`.
    pub ipp: bool,
    /// Serve the directed WS-Discovery Probe/Resolve HTTP endpoint (pure HTTP, no multicast).
    pub directed_discovery: bool,
    pub limits: PrintLimits,
    pub timing: PrintTiming,
    /// Force reply validation on/off; `None` follows `FAKEPRINT_VALIDATE_REPLIES`.
    pub validate_replies: Option<bool>,
    /// Which `NotifyTo` addresses subscriptions may use.
    pub address_policy: AddressPolicy,
    /// Push the current state to a new subscriber (WSDMON lab experiment).
    pub initial_events: bool,
    /// Initial event rate in seconds; `None`: the profile's `PrinterEventRate`.
    pub event_rate_secs: Option<u32>,
    /// Time source (tests inject a manual clock).
    pub clock: Option<Arc<dyn Clock>>,
    /// Period of the job timers and the event pump.
    pub tick: Duration,
    pub daemon: DaemonOptions,
}

impl ServerConfig {
    /// A server for a test or a binding: loopback, ephemeral port, nothing but the inbox
    /// touched, validation on, no initial event push.
    pub fn embedded(inbox_dir: impl Into<PathBuf>) -> Self {
        Self {
            listen: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            allow_non_loopback: false,
            advertise: None,
            embedded: true,
            profile_dir: None,
            profile_name: "generic-wsd".to_string(),
            profile: None,
            inbox_dir: inbox_dir.into(),
            device_uuid: Uuid::new_v4(),
            ipp_uuid: None,
            lab_name: lab::LAB_NAME.to_string(),
            wsd: true,
            ipp: true,
            directed_discovery: false,
            limits: PrintLimits::default(),
            timing: PrintTiming::default(),
            validate_replies: Some(true),
            address_policy: AddressPolicy::PrivateOnly,
            initial_events: false,
            event_rate_secs: None,
            clock: None,
            tick: DEFAULT_TICK,
            daemon: DaemonOptions::default(),
        }
    }

    /// The lab daemon: `0.0.0.0:3921`, the stable lab UUID, everything on.
    pub fn daemon(profile_dir: impl Into<PathBuf>, inbox_dir: impl Into<PathBuf>) -> Self {
        Self {
            listen: lab::default_listen(),
            allow_non_loopback: true,
            embedded: false,
            profile_dir: Some(profile_dir.into()),
            device_uuid: lab::lab_device_uuid(),
            ipp_uuid: Some(Uuid::parse_str(lab::LAB_IPP_UUID).unwrap_or_else(|_| Uuid::nil())),
            directed_discovery: true,
            validate_replies: None,
            initial_events: true,
            ..Self::embedded(inbox_dir)
        }
    }
}

/// Which routes [`router`] mounts. `Default` mounts only the control plane (`/`, `/health`,
/// `/v1/*`); switch the rest on field by field.
///
/// ```ignore
/// let app = fakeprint::server::router(
///     world,
///     &RouterOptions { wsd: true, ipp: true, ..RouterOptions::default() },
/// );
/// let outer = axum::Router::new().nest("/printer", app);
/// ```
#[derive(Clone, Default)]
pub struct RouterOptions {
    /// Mount the WSD endpoints (`/WebServices/Device`, `/WebServices/PrinterService`,
    /// `/PrintService`). Request injection applies to them.
    pub wsd: bool,
    /// Mount `/ipp/print` (and `/ipp/print/`). Injection applies to rules with
    /// `endpoint: "ipp"` only.
    pub ipp: bool,
    /// The service behind the directed discovery endpoint (`None`: not mounted).
    pub directed_discovery: Option<Arc<DiscoveryService>>,
    /// Log one span per request (a `tracing` span with method, URI and peer).
    pub trace: bool,
}

/// A running server. Dropping it asks every task to stop; [`ServerHandle::shutdown`] also
/// waits for them (and for the final delivery of `SubscriptionEnd` messages).
pub struct ServerHandle {
    local_addr: SocketAddr,
    base_url: String,
    device_endpoint: String,
    service_endpoint: String,
    device_uuid: Uuid,
    ipp_uuid: Uuid,
    inbox_dir: PathBuf,
    world: SharedWorld,
    validator: Arc<ReplyValidator>,
    injector: Arc<Injector>,
    router: Router,
    events: Arc<EventManager>,
    stop_http: Option<oneshot::Sender<()>>,
    stop_pump: Option<oneshot::Sender<()>>,
    stop_discovery: Option<oneshot::Sender<()>>,
    joins: Vec<JoinHandle<()>>,
    aborts: Vec<AbortHandle>,
    _lock: Option<lab::InstanceLock>,
}

impl ServerHandle {
    /// The bound address.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub fn port(&self) -> u16 {
        self.local_addr.port()
    }

    /// `http://host:port` (no trailing slash).
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// `http://host:port/WebServices/Device`
    pub fn device_endpoint(&self) -> &str {
        &self.device_endpoint
    }

    /// `http://host:port/WebServices/PrinterService`
    pub fn service_endpoint(&self) -> &str {
        &self.service_endpoint
    }

    pub fn device_uuid(&self) -> Uuid {
        self.device_uuid
    }

    pub fn ipp_uuid(&self) -> Uuid {
        self.ipp_uuid
    }

    /// The capture directory.
    pub fn inbox_dir(&self) -> &std::path::Path {
        &self.inbox_dir
    }

    /// The shared state (jobs, profile, status).
    pub fn world(&self) -> SharedWorld {
        Arc::clone(&self.world)
    }

    /// The current print context (jobs, engine, model, clock).
    pub async fn ctx(&self) -> Arc<PrintCtx> {
        self.world.read().await.print_ctx()
    }

    /// The subscription manager.
    pub fn events(&self) -> Arc<EventManager> {
        Arc::clone(&self.events)
    }

    /// Schema violations found in outgoing messages so far (newest 64).
    pub fn validation_problems(&self) -> Vec<String> {
        self.validator.recent_problems()
    }

    /// Number of schema violations found in outgoing messages so far.
    pub fn validation_problem_count(&self) -> u64 {
        self.validator.problem_count()
    }

    /// The reply validator itself (counters, switch).
    pub fn validator(&self) -> Arc<ReplyValidator> {
        Arc::clone(&self.validator)
    }

    /// The routes this server serves, sharing its state, for mounting in another axum
    /// application (`Router::nest` / `merge`). Cheap to call; every clone talks to the same
    /// printer. The server's own listener keeps running.
    pub fn router(&self) -> Router {
        self.router.clone()
    }

    /// The request-injection table (also reachable through `POST /v1/faults`).
    pub fn injector(&self) -> Arc<Injector> {
        Arc::clone(&self.injector)
    }

    /// Arm a request-injection rule. Refused (`Err`) when the rule is malformed.
    pub fn inject(&self, rule: InjectRule) -> Result<(), String> {
        self.injector.add(rule)
    }

    /// Drop every pending injection rule.
    pub fn clear_inject(&self) {
        self.injector.clear();
    }

    /// The pending injection rules with their remaining counts.
    pub fn pending_inject(&self) -> Vec<PendingRule> {
        self.injector.pending()
    }

    /// Effects injected so far.
    pub fn inject_hits(&self) -> u64 {
        self.injector.hits()
    }

    fn signal_stop(&mut self) {
        for tx in [
            self.stop_http.take(),
            self.stop_pump.take(),
            self.stop_discovery.take(),
        ]
        .into_iter()
        .flatten()
        {
            let _ = tx.send(());
        }
    }

    /// Stop gracefully: close the listener, end every subscription (`SourceShuttingDown` is
    /// delivered best effort), say Bye on WS-Discovery, and wait for the tasks.
    pub async fn shutdown(mut self) {
        self.signal_stop();
        for h in self.aborts.drain(..) {
            h.abort();
        }
        for j in self.joins.drain(..) {
            let _ = j.await;
        }
    }

    /// Wait until the server task ends on its own (a `Drop`/`shutdown` stops it).
    pub async fn wait(&mut self) {
        if let Some(first) = self.joins.first_mut() {
            let _ = first.await;
        }
    }
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        self.signal_stop();
        for h in self.aborts.drain(..) {
            h.abort();
        }
    }
}

/// Start the server. Returns once the listener is bound and the routes are live.
pub async fn serve(cfg: ServerConfig) -> Result<ServerHandle> {
    if !cfg.allow_non_loopback && !cfg.listen.ip().is_loopback() {
        bail!(
            "refusing to listen on {}: the address is not loopback (set allow_non_loopback to serve the network)",
            cfg.listen
        );
    }
    let daemon = if cfg.embedded {
        DaemonOptions {
            discovery: false,
            probe_listen: false,
            jetdirect_port: None,
            named_pipe: false,
            instance_lock: false,
        }
    } else {
        cfg.daemon.clone()
    };

    let (profile, profiles_dir) = match (&cfg.profile, &cfg.profile_dir) {
        (Some(p), dir) => (p.clone(), dir.clone().unwrap_or_default()),
        (None, Some(dir)) => (
            PrinterProfile::load(dir, &cfg.profile_name)
                .with_context(|| format!("profile {:?} in {}", cfg.profile_name, dir.display()))?,
            dir.clone(),
        ),
        (None, None) if cfg.embedded => (PrinterProfile::generic(), PathBuf::new()),
        (None, None) => bail!("a profile or a profile directory is required"),
    };
    info!(id = %profile.id, "loaded profile");

    let lock = if daemon.instance_lock && cfg.listen.port() != 0 {
        Some(lab::acquire_instance_lock(cfg.listen.port())?)
    } else {
        None
    };
    let listener = lab::bind_listener(cfg.listen).await?;
    let local_addr = listener.local_addr()?;
    let adv_ip = match cfg.advertise.unwrap_or_else(|| local_addr.ip()) {
        ip if ip.is_unspecified() => lab::first_lan_ipv4()
            .map(IpAddr::V4)
            .unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST)),
        ip => ip,
    };
    let host = match adv_ip {
        IpAddr::V6(v6) => format!("[{v6}]"),
        v4 => v4.to_string(),
    };
    let base_url = format!("http://{host}:{}", local_addr.port());
    let device_endpoint = format!("{base_url}/WebServices/Device");
    let service_endpoint = format!("{base_url}/WebServices/PrinterService");

    let discovery_stats = Arc::new(DiscoveryStats::default());
    let mut world = World::with_options(
        profiles_dir,
        cfg.inbox_dir.clone(),
        profile,
        Arc::clone(&discovery_stats),
        WorldOptions {
            clock: cfg
                .clock
                .clone()
                .unwrap_or_else(|| Arc::new(crate::wsd::ctx::SystemClock)),
            limits: cfg.limits.clone(),
            timing: cfg.timing.clone(),
            validate_replies: cfg.validate_replies,
            address_policy: cfg.address_policy,
            initial_events: cfg.initial_events,
            event_rate_secs: cfg.event_rate_secs,
        },
    )?;
    world.set_lab_name(&cfg.lab_name);
    // The IPP UUID must differ from the WSD UUID (Windows DAFWSDProvider collision).
    let ipp_uuid = match cfg.ipp_uuid {
        Some(u) if u != cfg.device_uuid => u,
        _ => lab::derive_ipp_uuid(cfg.device_uuid),
    };
    world.set_identity(
        device_endpoint.clone(),
        service_endpoint.clone(),
        cfg.device_uuid,
    );
    world.set_ipp_uuid(ipp_uuid);
    let events = world.events();
    let ctx = world.print_ctx();
    let validator = Arc::clone(&ctx.validator);
    let injector = Arc::clone(&ctx.inject);
    let inbox_dir = world.inbox_dir().to_path_buf();
    let world: SharedWorld = Arc::new(RwLock::new(world));

    // WS-Discovery: the service is shared by the multicast loop and the directed endpoint.
    let discovery_cfg = {
        let multicast_iface = match adv_ip {
            IpAddr::V4(v4) if !v4.is_unspecified() => v4,
            _ => Ipv4Addr::UNSPECIFIED,
        };
        DiscoveryConfig {
            multicast_iface,
            listen_probes: daemon.probe_listen,
            stats: discovery_stats,
            ..DiscoveryConfig::new(device_endpoint.clone(), cfg.device_uuid)
        }
    };
    let discovery_service = (cfg.wsd && (daemon.discovery || cfg.directed_discovery))
        .then(|| Arc::new(DiscoveryService::new(&discovery_cfg)));

    let app = router(
        Arc::clone(&world),
        &RouterOptions {
            wsd: cfg.wsd,
            ipp: cfg.ipp,
            directed_discovery: discovery_service.clone().filter(|_| cfg.directed_discovery),
            trace: !cfg.embedded,
        },
    );

    let handle_router = app.clone();
    let mut joins: Vec<JoinHandle<()>> = Vec::new();
    let mut aborts: Vec<AbortHandle> = Vec::new();

    // Timers and event delivery.
    aborts.push(spawn_ticker(Arc::clone(&ctx), cfg.tick).abort_handle());
    let (stop_pump, pump_rx) = oneshot::channel();
    let deliver = Arc::new(ValidatingDeliver {
        inner: Arc::new(HttpDeliver {
            policy: cfg.address_policy,
            ..HttpDeliver::default()
        }),
        validator: Arc::clone(&validator),
    });
    let pump_events = Arc::clone(&events);
    let tick = cfg.tick;

    // The listener task goes first so `wait` follows the HTTP server.
    let (stop_http, http_rx) = oneshot::channel::<()>();
    joins.push(tokio::spawn(async move {
        let served = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async move {
            let _ = http_rx.await;
        })
        .await;
        if let Err(e) = served {
            warn!(error = %e, "HTTP server stopped with an error");
        }
    }));
    joins.push(tokio::spawn(wsd::eventing::run_pump(
        pump_events,
        deliver,
        tick,
        pump_rx,
    )));

    let mut stop_discovery = None;
    if daemon.discovery && cfg.wsd {
        if let Some(service) = discovery_service.clone() {
            info!(
                endpoint = %discovery_cfg.endpoint,
                multicast_iface = %discovery_cfg.multicast_iface,
                listen_probes = daemon.probe_listen,
                "WSD Print Service + WS-Discovery enabled"
            );
            let (tx, rx) = oneshot::channel();
            stop_discovery = Some(tx);
            joins.push(tokio::spawn(async move {
                if let Err(e) = discovery::run_with_service(discovery_cfg, service, rx).await {
                    tracing::error!(error = %e, "WS-Discovery stopped");
                }
            }));
        }
    }
    if let Some(port) = daemon.jetdirect_port.filter(|p| *p != 0) {
        let jd_world = Arc::clone(&world);
        let bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port);
        aborts.push(
            tokio::spawn(async move { jetdirect::serve(jd_world, bind).await }).abort_handle(),
        );
    }
    #[cfg(windows)]
    if daemon.named_pipe {
        let pipe_world = Arc::clone(&world);
        let pipe_name = cfg.lab_name.clone();
        aborts.push(
            tokio::spawn(
                async move { crate::pipe::serve_named_pipe(&pipe_name, pipe_world).await },
            )
            .abort_handle(),
        );
    }

    info!(listen = %local_addr, name = %cfg.lab_name, "listening");
    Ok(ServerHandle {
        local_addr,
        base_url,
        device_endpoint,
        service_endpoint,
        device_uuid: cfg.device_uuid,
        ipp_uuid,
        inbox_dir,
        world,
        validator,
        injector,
        router: handle_router,
        events,
        stop_http: Some(stop_http),
        stop_pump: Some(stop_pump),
        stop_discovery,
        joins,
        aborts,
        _lock: lock,
    })
}

// ---------------------------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------------------------

/// The routes the binary serves: control plane, optionally IPP, the WSD endpoints and the
/// directed discovery endpoint.
pub fn router(world: SharedWorld, opts: &RouterOptions) -> Router {
    let mut app = Router::new()
        .route("/", get(presentation))
        .route("/health", get(health))
        .route("/v1/status", get(status))
        .route("/v1/reset", post(reset))
        .route("/v1/jobs", get(list_jobs))
        .route("/v1/jobs/{id}", get(get_job))
        .route("/v1/profile", post(set_profile))
        .route("/v1/ctl", get(ctl_get).post(ctl_post))
        .route(
            "/v1/faults",
            get(faults_get).post(faults_post).delete(faults_delete),
        )
        .route("/v1/timing", get(timing_get).post(timing_post));
    if opts.ipp {
        let ipp_routes = Router::new()
            .route(
                "/ipp/print",
                get(ipp::print_handler).post(ipp::print_handler),
            )
            .route(
                "/ipp/print/",
                get(ipp::print_handler).post(ipp::print_handler),
            )
            .layer(axum::middleware::from_fn_with_state(
                Arc::clone(&world),
                ipp_inject,
            ));
        app = app.merge(ipp_routes);
    }
    if opts.wsd {
        for path in WSD_PATHS {
            app = app.route(path, post(wsd::print_service));
        }
    }
    if let Some(service) = &opts.directed_discovery {
        app = app
            .route(discovery::DIRECTED_PATH, post(directed_discovery))
            .layer(Extension(Arc::clone(service)));
    }
    if opts.trace {
        app = app.layer(
            TraceLayer::new_for_http().make_span_with(|req: &Request<_>| {
                let peer = req
                    .extensions()
                    .get::<ConnectInfo<SocketAddr>>()
                    .map(|ConnectInfo(a)| a.to_string())
                    .unwrap_or_else(|| "-".into());
                info_span!("http", method = %req.method(), uri = %req.uri(), peer = %peer)
            }),
        );
    }
    app.with_state(world)
}

/// Request injection on the IPP route: only rules with `endpoint: "ipp"` match.
async fn ipp_inject(
    State(world): State<SharedWorld>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let injector = Arc::clone(&world.read().await.print_ctx().inject);
    let Some(inj) = injector.take(&InjectQuery {
        endpoint: RuleEndpoint::Ipp,
        action: None,
    }) else {
        return next.run(req).await;
    };
    inj.sleep(false).await;
    if inj.closes() {
        return wsd::aborted_response();
    }
    if let Some(h) = inj.http_reply() {
        inj.sleep(true).await;
        return wsd::http_response(h);
    }
    let out = next.run(req).await;
    inj.sleep(true).await;
    out
}

/// Directed (unicast) WS-Discovery Probe / Resolve over HTTP.
async fn directed_discovery(
    Extension(service): Extension<Arc<DiscoveryService>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok());
    let reply = match SoapRequest::parse(content_type, None, &body) {
        Ok(req) => service.handle_directed(&req),
        Err(fault) => fault.to_reply(None),
    };
    wsd::http_response(reply.into_http())
}

async fn health() -> &'static str {
    "ok"
}

/// Minimal page for ThisModel PresentationUrl (Settings "Acquiring data").
async fn presentation(State(world): State<SharedWorld>) -> axum::response::Html<String> {
    let label = world.read().await.profile().label.clone();
    let label = crate::wsd::xml::esc(&label);
    axum::response::Html(format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>{label}</title></head>\
         <body><h1>{label}</h1><p>fakeprint lab presentation</p></body></html>"
    ))
}

async fn status(State(world): State<SharedWorld>) -> Json<Status> {
    Json(world.read().await.status())
}

async fn reset(State(world): State<SharedWorld>) -> Json<Status> {
    let mut w = world.write().await;
    w.reset();
    Json(w.status())
}

/// One print job as the control plane lists it: `id`, `name`, `user`, `ticket` (the effective
/// stored ticket as typed JSON, `null` for IPP/raw jobs), `state`, `reasons`, `created` /
/// `ended` (ISO 8601 UTC, plus `*_unix_ms`), counters and
/// `documents: [{seq, id, format, compression, bytes, file, path}]`.
fn job_row(j: &crate::wsd::types::PrintJob) -> serde_json::Value {
    let mut row = crate::wsd::jobs::job_meta_json(j, None);
    let docs: Vec<serde_json::Value> = j
        .documents
        .iter()
        .map(|d| {
            serde_json::json!({
                "seq": d.id,
                "id": d.id,
                "format": d.format,
                "compression": d.compression,
                "bytes": d.size,
                "file": d.capture_path.as_ref().and_then(|p| p.file_name()).and_then(|n| n.to_str()),
                "path": d.capture_path.as_ref().map(|p| p.display().to_string()),
            })
        })
        .collect();
    if let Some(o) = row.as_object_mut() {
        o.insert("id".into(), j.id.into());
        o.insert("documents".into(), docs.into());
        o.insert("k_octets_processed".into(), j.k_octets_processed().into());
        o.insert(
            "media_sheets_completed".into(),
            j.media_sheets_completed.into(),
        );
        o.insert(
            "last_document_received".into(),
            j.last_document_received.into(),
        );
    }
    row
}

/// Captured documents (`jobs`, as before) and the print jobs, history included (`print_jobs`,
/// ascending by id).
fn jobs_json(w: &World) -> serde_json::Value {
    let ctx = w.print_ctx();
    let mut all = ctx.jobs.history();
    all.extend(ctx.jobs.active());
    all.sort_by_key(|j| j.id);
    let rows: Vec<serde_json::Value> = all.iter().map(job_row).collect();
    serde_json::json!({ "jobs": w.captured(), "print_jobs": rows })
}

async fn list_jobs(State(world): State<SharedWorld>) -> Json<serde_json::Value> {
    Json(jobs_json(&*world.read().await))
}

async fn get_job(
    State(world): State<SharedWorld>,
    UrlPath(id): UrlPath<u32>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let ctx = world.read().await.print_ctx();
    ctx.jobs
        .get(id)
        .map(|j| Json(job_row(&j)))
        .ok_or(StatusCode::NOT_FOUND)
}

// ---- profile -------------------------------------------------------------------------------

/// `POST /v1/profile`: `{"id": "..."}` loads `<profiles>/<id>.json`; `{"profile": {...}}`
/// installs a schema-2 profile document directly.
#[derive(Debug, Deserialize)]
struct ProfileBody {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    profile: Option<serde_json::Value>,
}

type ApiError = (StatusCode, String);

fn swap_profile(w: &mut World, body: &ProfileBody) -> Result<(), ApiError> {
    let bad = |e: anyhow::Error| (StatusCode::BAD_REQUEST, format!("{e:#}"));
    match (&body.id, &body.profile) {
        (Some(id), None) => w.hot_swap_profile(id).map_err(bad),
        (None, Some(doc)) => {
            let p = PrinterProfile::from_json(&doc.to_string()).map_err(bad)?;
            w.swap_profile(p);
            Ok(())
        }
        _ => Err((
            StatusCode::BAD_REQUEST,
            "give exactly one of \"id\" or \"profile\"".into(),
        )),
    }
}

async fn set_profile(
    State(world): State<SharedWorld>,
    Json(body): Json<ProfileBody>,
) -> Result<Json<Status>, ApiError> {
    let mut w = world.write().await;
    swap_profile(&mut w, &body)?;
    Ok(Json(w.status()))
}

// ---- faults --------------------------------------------------------------------------------

/// Armed one-shot faults (`pending`) and what is switched on (`active`).
fn faults_json(ctx: &PrintCtx) -> serde_json::Value {
    let plan = ctx.engine.faults();
    let st = ctx.status();
    let conditions: Vec<serde_json::Value> = st
        .active_conditions
        .iter()
        .map(|c| {
            serde_json::json!({
                "id": c.id,
                "name": c.name,
                "component": c.component,
                "severity": c.severity.token(),
                "time": c.time,
            })
        })
        .collect();
    serde_json::json!({
        "pending": {
            "fail_next_document": plan.fail_next_document.map(|f| format!("{f:?}")),
        },
        "active": {
            "not_accepting_jobs": plan.not_accepting_jobs,
            "temporary_error": plan.temporary_error,
            "internal_error": plan.internal_error,
            "document_active": plan.document_active,
            "conditions": conditions,
            "printer_state": st.state.token(),
            "state_reasons": st.reasons,
        },
        "inject": crate::wsd::inject::inject_json(&ctx.inject),
    })
}

async fn faults_get(State(world): State<SharedWorld>) -> Json<serde_json::Value> {
    let ctx = world.read().await.print_ctx();
    Json(faults_json(&ctx))
}

/// `POST /v1/faults` with a [`FaultBody`]: raise or clear conditions and switch service faults.
async fn faults_post(
    State(world): State<SharedWorld>,
    raw: Bytes,
) -> Result<Json<serde_json::Value>, ApiError> {
    let body: FaultBody = parse_body(&raw)?;
    let ctx = world.read().await.print_ctx();
    ctx.apply_faults_checked(&body)
        .map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    Ok(Json(faults_json(&ctx)))
}

// ---- timing --------------------------------------------------------------------------------

async fn timing_get(State(world): State<SharedWorld>) -> Json<serde_json::Value> {
    Json(world.read().await.print_ctx().timing().to_json())
}

/// `POST /v1/timing`: `{"preset":"instant"|"realistic"|"default", "processing_base_ms":..,
/// "processing_per_mib_ms":.., "document_timeout_ms":.., "terminating_ms":..,
/// "document_busy_ms":.., "bytes_per_sheet":..}`; every key optional. Applies to documents that
/// arrive after the call.
async fn timing_post(
    State(world): State<SharedWorld>,
    raw: Bytes,
) -> Result<Json<serde_json::Value>, ApiError> {
    let patch: TimingPatch = parse_body(&raw)?;
    apply_timing(&world, &patch).await.map(Json)
}

/// JSON body of a control call; a malformed or unknown field is a 400 with the reason.
fn parse_body<T: serde::de::DeserializeOwned>(raw: &[u8]) -> Result<T, ApiError> {
    let raw = if raw.iter().all(u8::is_ascii_whitespace) {
        &b"{}"[..]
    } else {
        raw
    };
    serde_json::from_slice(raw).map_err(|e| (StatusCode::BAD_REQUEST, format!("body: {e}")))
}

async fn apply_timing(
    world: &SharedWorld,
    patch: &TimingPatch,
) -> Result<serde_json::Value, ApiError> {
    let ctx = world.read().await.print_ctx();
    let mut t = ctx.timing();
    t.apply(patch).map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    ctx.set_timing(t);
    Ok(ctx.timing().to_json())
}

/// `DELETE /v1/faults`: clear every condition and flag.
async fn faults_delete(State(world): State<SharedWorld>) -> Json<serde_json::Value> {
    let ctx = world.read().await.print_ctx();
    ctx.apply_faults(&FaultBody {
        reset: true,
        ..FaultBody::default()
    });
    Json(faults_json(&ctx))
}

// ---- ctl -----------------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct CtlInfo {
    name: String,
    port: u16,
    uuid: String,
    base_url: String,
    pipe: String,
    cmds: Vec<&'static str>,
}

async fn ctl_get(State(world): State<SharedWorld>) -> Json<CtlInfo> {
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
        cmds: vec![
            "status", "health", "jobs", "reset", "profile", "faults", "timing", "save",
        ],
    })
}

#[derive(Debug, Deserialize)]
pub(crate) struct CtlRequest {
    pub cmd: String,
    #[serde(default)]
    pub args: serde_json::Value,
}

async fn ctl_post(
    State(world): State<SharedWorld>,
    Json(req): Json<CtlRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    dispatch_ctl(&req.cmd, &req.args, &world).await.map(Json)
}

/// Shared dispatcher of `POST /v1/ctl` and the named pipe.
pub(crate) async fn dispatch_ctl(
    cmd: &str,
    args: &serde_json::Value,
    world: &SharedWorld,
) -> Result<serde_json::Value, ApiError> {
    let to_value = |s: Status| serde_json::to_value(s).unwrap_or(serde_json::Value::Null);
    match cmd {
        "status" => Ok(to_value(world.read().await.status())),
        "health" => Ok(serde_json::json!({"ok": true})),
        "jobs" => Ok(jobs_json(&*world.read().await)),
        "reset" => {
            let mut w = world.write().await;
            w.reset();
            Ok(to_value(w.status()))
        }
        "profile" => {
            let body = ProfileBody {
                id: args.get("id").and_then(|v| v.as_str()).map(str::to_string),
                profile: args.get("profile").cloned(),
            };
            if body.id.as_deref().is_some_and(str::is_empty) {
                return Err((StatusCode::BAD_REQUEST, "args.id required".into()));
            }
            let mut w = world.write().await;
            swap_profile(&mut w, &body)?;
            Ok(to_value(w.status()))
        }
        "faults" => {
            let ctx = world.read().await.print_ctx();
            if !args.is_null() && args.as_object().is_some_and(|o| !o.is_empty()) {
                let body: FaultBody = serde_json::from_value(args.clone())
                    .map_err(|e| (StatusCode::BAD_REQUEST, format!("args: {e}")))?;
                ctx.apply_faults_checked(&body)
                    .map_err(|e| (StatusCode::BAD_REQUEST, e))?;
            }
            Ok(faults_json(&ctx))
        }
        "timing" => {
            if args.as_object().is_some_and(|o| !o.is_empty()) {
                let patch: TimingPatch = serde_json::from_value(args.clone())
                    .map_err(|e| (StatusCode::BAD_REQUEST, format!("args: {e}")))?;
                apply_timing(world, &patch).await
            } else {
                Ok(world.read().await.print_ctx().timing().to_json())
            }
        }
        "save" => {
            // Registry write is a PowerShell/Python CLI responsibility.
            // The process exposes its current config via /v1/status for the script to read.
            Ok(serde_json::json!({
                "ok": true,
                "note": "use: .\\scripts\\fakeprint.ps1 save  (writes current status to registry)"
            }))
        }
        other => Err((
            StatusCode::BAD_REQUEST,
            format!(
                "unknown cmd {other:?}; known: status health jobs reset profile faults timing save"
            ),
        )),
    }
}
