//! fakeprint binary: control plane + WSD Print Service + discovery. Argument parsing around
//! [`fakeprint::server::serve`].

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;

use clap::Parser;
use fakeprint::lab::{LAB_DEVICE_UUID, LAB_IPP_UUID, LAB_NAME};
use fakeprint::server::{serve, DaemonOptions, ServerConfig};
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

    /// Validate every outgoing SOAP body against the WS-Print schema; violations are logged
    /// loudly and counted on /v1/status. Same as FAKEPRINT_VALIDATE_REPLIES=1.
    #[arg(long)]
    validate_replies: bool,

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

    let mut cfg = ServerConfig::daemon(args.profiles, args.inbox);
    cfg.listen = args.listen;
    cfg.advertise = args.advertise;
    cfg.profile_name = args.profile;
    cfg.device_uuid = args.device_uuid;
    cfg.ipp_uuid = Some(args.ipp_uuid);
    cfg.lab_name = args.lab_name;
    cfg.wsd = !args.no_wsd;
    cfg.ipp = args.ipp && !args.no_ipp;
    if args.validate_replies {
        cfg.validate_replies = Some(true);
    }
    cfg.daemon = DaemonOptions {
        discovery: true,
        probe_listen: args.probe_listen && !args.no_probe_listen,
        jetdirect_port: (!args.no_jetdirect && args.jetdirect_port != 0)
            .then_some(args.jetdirect_port),
        named_pipe: true,
        instance_lock: !args.no_lock,
    };

    let handle = match serve(cfg).await {
        Ok(h) => h,
        Err(e) => {
            eprintln!("{e:#}");
            std::process::exit(1);
        }
    };
    let _ = tokio::signal::ctrl_c().await;
    handle.shutdown().await;
    Ok(())
}
