//! Windows named-pipe control surface: `\.\pipe\fakeprint-<name>`, one JSON request per line,
//! the same commands as `POST /v1/ctl`. Daemon only; compiled out elsewhere.

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::windows::named_pipe::ServerOptions;
use tracing::{info, warn};

use crate::lab;
use crate::manager::DeviceManager;
use crate::server::{dispatch_ctl, CtlRequest};
use std::sync::Arc;

/// Serve the pipe until the task is aborted.
pub async fn serve_named_pipe(name: &str, manager: Arc<DeviceManager>) {
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

        let m = Arc::clone(&manager);
        tokio::spawn(async move {
            let (reader, mut writer) = tokio::io::split(server);
            let mut lines = BufReader::new(reader).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let line = line.trim().to_string();
                if line.is_empty() {
                    continue;
                }
                let resp = handle_pipe_line(&line, &m).await;
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

/// One JSON request line to one JSON reply line. The pipe is local to the machine, so manager
/// commands need no further peer check; `args.device` selects the printer.
pub(crate) async fn handle_pipe_line(line: &str, manager: &Arc<DeviceManager>) -> String {
    let err = |msg: String| serde_json::json!({"error": msg}).to_string();
    let req = match serde_json::from_str::<CtlRequest>(line) {
        Ok(r) => r,
        Err(e) => return err(format!("parse: {e}")),
    };
    let result = if crate::manager_http::MANAGER_CMDS.contains(&req.cmd.as_str()) {
        crate::manager_http::manager_cmd(manager, &req.cmd, &req.args, None).await
    } else {
        let world = match req.args.get("device").and_then(|v| v.as_str()) {
            Some(id) => manager.world_of(id),
            None => manager.default_managed().map(|d| d.world),
        };
        match world {
            Some(w) => dispatch_ctl(&req.cmd, &req.args, &w).await,
            None => Err((axum::http::StatusCode::NOT_FOUND, "unknown device".into())),
        }
    };
    match result {
        Ok(v) => v.to_string(),
        Err((_, msg)) => err(msg),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::{serve, DeviceConfig, ServerConfig};

    #[tokio::test]
    async fn pipe_lines_run_manager_and_device_commands() {
        let dir = std::env::temp_dir().join(format!("fakeprint-pipe-{}", std::process::id()));
        let cfg = ServerConfig::embedded(dir.join("inbox")).with_devices(vec![
            DeviceConfig::derived("a", "generic-wsd"),
            DeviceConfig::derived("b", "generic-wsd"),
        ]);
        let srv = serve(cfg).await.unwrap();
        let m = srv.manager();
        let run = |line: &str| {
            let (m, line) = (Arc::clone(&m), line.to_string());
            async move {
                serde_json::from_str::<serde_json::Value>(&handle_pipe_line(&line, &m).await)
                    .unwrap()
            }
        };
        assert_eq!(
            run(r#"{"cmd":"devices"}"#).await.as_array().unwrap().len(),
            2
        );
        assert_eq!(run(r#"{"cmd":"status"}"#).await["device_id"], "a");
        assert_eq!(
            run(r#"{"cmd":"status","args":{"device":"b"}}"#).await["device_id"],
            "b"
        );
        assert!(run(r#"{"cmd":"status","args":{"device":"zz"}}"#).await["error"].is_string());
        assert!(run(r#"{"cmd":"shutdown"}"#).await["error"].is_string());
        assert!(run("not json").await["error"].is_string());
        assert_eq!(run(r#"{"cmd":"queue"}"#).await["total"], 0);
        srv.shutdown().await;
        let _ = std::fs::remove_dir_all(dir);
    }
}
