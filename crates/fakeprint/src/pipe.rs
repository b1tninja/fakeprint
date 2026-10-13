//! Windows named-pipe control surface: `\.\pipe\fakeprint-<name>`, one JSON request per line,
//! the same commands as `POST /v1/ctl`. Daemon only; compiled out elsewhere.

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::windows::named_pipe::ServerOptions;
use tracing::{info, warn};

use crate::lab;
use crate::server::{dispatch_ctl, CtlRequest};
use crate::wsd::SharedWorld;

/// Serve the pipe until the task is aborted.
pub async fn serve_named_pipe(name: &str, world: SharedWorld) {
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

        let w = world.clone();
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

async fn handle_pipe_line(line: &str, world: &SharedWorld) -> String {
    match serde_json::from_str::<CtlRequest>(line) {
        Err(e) => serde_json::json!({"error": format!("parse: {e}")}).to_string(),
        Ok(req) => match dispatch_ctl(&req.cmd, &req.args, world).await {
            Ok(v) => v.to_string(),
            Err((_, msg)) => serde_json::json!({"error": msg}).to_string(),
        },
    }
}
