//! Raw JetDirect / AppSocket listener (TCP :9100).
//!
//! Windows `Add-PrinterPort -PrinterHostAddress` always creates TCPMON RAW :9100,
//! even when given an `http://…/ipp/print` URL. Jobs then stick at
//! Printing/Retained unless something accepts the raw stream. This listener
//! captures the stream to the inbox so the TCPMON lab path works.

use std::sync::Arc;

use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;
use tokio::sync::RwLock;
use tracing::{info, warn};

use crate::state::World;

/// Bind the RAW listener (separate from [`serve_listener`] so the caller can report a port
/// conflict synchronously). Port 0 picks a free port.
pub async fn bind(bind: std::net::SocketAddr) -> std::io::Result<TcpListener> {
    TcpListener::bind(bind).await
}

/// Accept raw print jobs on `bind` (JetDirect). A bind failure is logged and the task ends.
pub async fn serve(world: Arc<RwLock<World>>, bind: std::net::SocketAddr) {
    match TcpListener::bind(bind).await {
        Ok(l) => serve_listener(world, l).await,
        Err(e) => {
            warn!(%bind, error = %e, "JetDirect bind failed (TCPMON RAW jobs will stick)");
        }
    }
}

/// Accept raw print jobs on an already bound listener; runs until aborted.
pub async fn serve_listener(world: Arc<RwLock<World>>, listener: TcpListener) {
    if let Ok(addr) = listener.local_addr() {
        info!(%addr, "JetDirect RAW listener ready");
    }

    loop {
        let (mut sock, peer) = match listener.accept().await {
            Ok(x) => x,
            Err(e) => {
                warn!(error = %e, "JetDirect accept failed");
                continue;
            }
        };
        let world = Arc::clone(&world);
        tokio::spawn(async move {
            let mut buf = Vec::with_capacity(64 * 1024);
            let mut chunk = [0u8; 16 * 1024];
            loop {
                match sock.read(&mut chunk).await {
                    Ok(0) => break,
                    Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    Err(e) => {
                        warn!(%peer, error = %e, "JetDirect read failed");
                        break;
                    }
                }
                // Cap runaway jobs (64 MiB).
                if buf.len() > 64 * 1024 * 1024 {
                    warn!(%peer, "JetDirect job exceeded 64 MiB; truncating");
                    break;
                }
            }
            if buf.is_empty() {
                info!(%peer, "JetDirect empty connection");
                return;
            }
            let fmt = sniff_format(&buf);
            let job_id = {
                let mut w = world.write().await;
                w.note_ipp_action("JetDirect");
                w.create_job("jetdirect-job", fmt, "tcpmon")
            };
            {
                let mut w = world.write().await;
                match w.capture_document(job_id, &buf, fmt, "tcpmon") {
                    Ok(cap) => {
                        info!(
                            %peer,
                            job_id,
                            bytes = cap.bytes,
                            %fmt,
                            "JetDirect job captured to inbox"
                        );
                    }
                    Err(e) => warn!(%peer, job_id, error = %e, "JetDirect capture failed"),
                }
                w.end_job(job_id);
            }
        });
    }
}

fn sniff_format(buf: &[u8]) -> &'static str {
    if buf.starts_with(b"%PDF") {
        "application/pdf"
    } else if buf.starts_with(b"\x1b%-12345X") || buf.windows(5).any(|w| w == b"%!PS-") {
        "application/postscript"
    } else if buf.starts_with(b"\x1b") {
        "application/vnd.hp-PCL"
    } else {
        "application/octet-stream"
    }
}
