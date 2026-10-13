//! `fakeprint daemon install | uninstall | status`: a thin `schtasks` wrapper that makes the
//! manager start at logon (docs/multi-device.md 4.5).
//!
//! It creates a per-user, ONLOGON, LIMITED (non-elevated) scheduled task `fakeprint-daemon`
//! that runs `"<exe>" --manage --profiles "<abs>" [--listen L] [--advertise A] [--discovery]`.
//! It is not a Windows service (no elevation: the registry and the Windows Add are per user).
//! The command builders are pure and unit-tested; `schtasks.exe` only runs when a human invokes the
//! subcommand, never from the daemon, the tray, the UI or a test. Nothing here adds a printer.

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;

use anyhow::{bail, Result};

/// Name of the scheduled task.
pub const TASK_NAME: &str = "fakeprint-daemon";

/// What the task runs.
#[derive(Debug, Clone)]
pub struct TaskSpec {
    pub exe: PathBuf,
    pub profiles: PathBuf,
    pub listen: Option<SocketAddr>,
    pub advertise: Option<IpAddr>,
    /// `--discovery`: allow WS-Discovery multicast (off by default, rule 7).
    pub discovery: bool,
}

/// The `/TR` value: the command line the task runs. Paths containing `"` are refused.
pub fn task_command(spec: &TaskSpec) -> Result<String> {
    let exe = spec.exe.display().to_string();
    let profiles = spec.profiles.display().to_string();
    if exe.contains('"') || profiles.contains('"') {
        bail!("paths with a double quote cannot be put in a scheduled task");
    }
    let mut cmd = format!("\"{exe}\" --manage --profiles \"{profiles}\"");
    if let Some(l) = spec.listen {
        cmd.push_str(&format!(" --listen {l}"));
    }
    if let Some(a) = spec.advertise {
        cmd.push_str(&format!(" --advertise {a}"));
    }
    if spec.discovery {
        cmd.push_str(" --discovery");
    }
    Ok(cmd)
}

/// Arguments of `schtasks` that create (or replace) the task.
pub fn create_args(spec: &TaskSpec) -> Result<Vec<String>> {
    Ok([
        "/Create", "/TN", TASK_NAME, "/SC", "ONLOGON", "/RL", "LIMITED", "/F", "/TR",
    ]
    .iter()
    .map(|s| s.to_string())
    .chain(std::iter::once(task_command(spec)?))
    .collect())
}

/// Arguments of `schtasks` that delete the task.
pub fn delete_args() -> Vec<String> {
    ["/Delete", "/TN", TASK_NAME, "/F"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

/// Arguments of `schtasks` that describe the task.
pub fn query_args() -> Vec<String> {
    ["/Query", "/TN", TASK_NAME, "/FO", "LIST", "/V"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

/// What the subcommand does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Install,
    Uninstall,
    Status,
}

/// Run `schtasks` for `action` and return its output. Windows only.
#[cfg(windows)]
pub fn run(action: Action, spec: &TaskSpec) -> Result<String> {
    use std::os::windows::process::CommandExt;
    let args = match action {
        Action::Install => create_args(spec)?,
        Action::Uninstall => delete_args(),
        Action::Status => query_args(),
    };
    let out = std::process::Command::new("schtasks")
        .args(&args)
        .creation_flags(0x0800_0000)
        .output()?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    if !out.status.success() {
        bail!("schtasks failed ({}): {}", out.status, text.trim());
    }
    Ok(text)
}

/// Run `schtasks` for `action`. Not available off Windows.
#[cfg(not(windows))]
pub fn run(_action: Action, _spec: &TaskSpec) -> Result<String> {
    bail!("the scheduled-task helper is Windows only")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> TaskSpec {
        TaskSpec {
            exe: PathBuf::from(r"C:\Program Files\fakeprint\fakeprint.exe"),
            profiles: PathBuf::from(r"C:\lab\profiles"),
            listen: None,
            advertise: None,
            discovery: false,
        }
    }

    #[test]
    fn command_line_is_manage_with_quoted_paths() {
        assert_eq!(
            task_command(&spec()).unwrap(),
            r#""C:\Program Files\fakeprint\fakeprint.exe" --manage --profiles "C:\lab\profiles""#
        );
        let mut s = spec();
        s.listen = Some("192.168.1.5:3921".parse().unwrap());
        s.advertise = Some("192.168.1.5".parse().unwrap());
        s.discovery = true;
        let c = task_command(&s).unwrap();
        assert!(c.ends_with("--listen 192.168.1.5:3921 --advertise 192.168.1.5 --discovery"));
    }

    #[test]
    fn create_args_are_per_user_logon_limited() {
        let a = create_args(&spec()).unwrap();
        assert_eq!(a[0], "/Create");
        for (flag, value) in [("/TN", TASK_NAME), ("/SC", "ONLOGON"), ("/RL", "LIMITED")] {
            let i = a.iter().position(|x| x == flag).unwrap();
            assert_eq!(a[i + 1], value);
        }
        assert!(a.contains(&"/F".to_string()));
        assert!(!a
            .iter()
            .any(|x| x.eq_ignore_ascii_case("/RU") || x == "HIGHEST"));
        assert!(a.last().unwrap().contains("--manage"));
    }

    #[test]
    fn quotes_in_paths_are_refused() {
        let mut s = spec();
        s.profiles = PathBuf::from("a\"b");
        assert!(task_command(&s).is_err());
    }

    #[test]
    fn delete_and_query_name_the_task() {
        assert_eq!(delete_args(), ["/Delete", "/TN", TASK_NAME, "/F"]);
        assert_eq!(query_args()[..3], ["/Query", "/TN", TASK_NAME]);
    }
}
