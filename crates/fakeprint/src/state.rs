//! Shared process state: identity, jobs, inbox captures.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::Serialize;
use uuid::Uuid;

use crate::discovery::DiscoveryStats;
use crate::profile::Profile;

#[derive(Debug, Clone, Serialize)]
pub struct CapturedJob {
    pub job_id: u32,
    pub path: String,
    pub meta_path: String,
    pub bytes: usize,
    pub document_format: String,
    pub peer: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    Pending,
    Processing,
    Completed,
    Canceled,
    Aborted,
}

#[derive(Debug, Clone)]
pub struct ActiveJob {
    pub job_id: u32,
    pub document_format: String,
    pub name: String,
    pub user: String,
    pub token: String,
    pub state: JobState,
    pub documents: u32,
    pub bytes_received: usize,
}

#[derive(Debug)]
pub struct World {
    profiles_dir: PathBuf,
    inbox_dir: PathBuf,
    profile: Profile,
    /// Lab instance name (`--lab-name` / `FAKEPRINT_NAME`); matches registry row.
    lab_name: String,
    device_endpoint: String,
    service_endpoint: String,
    device_uuid: Uuid,
    /// UUID advertised in IPP Get-Printer-Attributes (`printer-uuid`).
    ///
    /// Must differ from `device_uuid` (the WSD UUID) so that
    /// `Add-Printer -IppURL` does not hit "printer already exists" because
    /// Windows finds an existing `SWD\DAFWSDProvider` entry for the same UUID.
    ipp_uuid: Uuid,
    discovery: Arc<DiscoveryStats>,
    next_job_id: u32,
    active: HashMap<u32, ActiveJob>,
    /// Finished jobs kept briefly so GetJobElements still answers.
    history: HashMap<u32, ActiveJob>,
    captured: Vec<CapturedJob>,
    /// SOAP short-action hit counts (lab: prove Windows Transfer Get / Subscribe).
    wsd_actions: HashMap<String, u64>,
    /// IPP operation hit counts (lab: prove WSDMON probes /ipp/print).
    ipp_actions: HashMap<String, u64>,
    /// WS-Eventing NotifyTo sinks, keyed by WSDMON Subscribe Filter (short name).
    event_sinks: Vec<PrintEventSink>,
}

/// One WSDMON Subscribe: NotifyTo + Filter so later JobEndStateEvent can match.
#[derive(Debug, Clone)]
pub struct PrintEventSink {
    pub filter: String,
    pub notify_to: String,
    pub sub_id: Uuid,
    pub service_ep: String,
}

#[derive(Debug, Serialize)]
pub struct Status {
    pub name: String,
    pub profile_id: String,
    pub label: String,
    pub device_endpoint: String,
    pub service_endpoint: String,
    pub device_uuid: Uuid,
    /// IPP printer-uuid (distinct from WSD device_uuid; used by `Add-Printer -IppURL`).
    pub ipp_uuid: Uuid,
    pub wsd_hellos_sent: u64,
    pub wsd_probes_answered: u64,
    pub wsd_actions: HashMap<String, u64>,
    pub ipp_actions: HashMap<String, u64>,
    pub active_jobs: usize,
    pub captured_jobs: usize,
    pub inbox_dir: String,
    pub available_profiles: Vec<String>,
    pub document_formats: Vec<String>,
    pub note: String,
}

impl World {
    pub fn new(
        profiles_dir: PathBuf,
        inbox_dir: PathBuf,
        profile: Profile,
        discovery: Arc<DiscoveryStats>,
    ) -> Result<Self> {
        std::fs::create_dir_all(&inbox_dir)
            .with_context(|| format!("create inbox {}", inbox_dir.display()))?;
        Ok(Self {
            profiles_dir,
            inbox_dir,
            profile,
            lab_name: crate::lab::LAB_NAME.to_string(),
            device_endpoint: String::new(),
            service_endpoint: String::new(),
            device_uuid: Uuid::nil(),
            ipp_uuid: Uuid::nil(),
            discovery,
            next_job_id: 1,
            active: HashMap::new(),
            history: HashMap::new(),
            captured: Vec::new(),
            wsd_actions: HashMap::new(),
            ipp_actions: HashMap::new(),
            event_sinks: Vec::new(),
        })
    }

    pub fn set_lab_name(&mut self, name: impl Into<String>) {
        self.lab_name = name.into();
    }

    pub fn lab_name(&self) -> &str {
        &self.lab_name
    }

    /// Hot-swap the active profile without restarting; WSD identity unchanged.
    pub fn hot_swap_profile(&mut self, id: &str) -> Result<()> {
        let p = Profile::load(&self.profiles_dir, id)?;
        self.profile = p;
        Ok(())
    }

    pub fn set_identity(&mut self, device_ep: String, service_ep: String, uuid: Uuid) {
        self.device_endpoint = device_ep;
        self.service_endpoint = service_ep;
        self.device_uuid = uuid;
    }

    /// Override the UUID returned in IPP Get-Printer-Attributes (`printer-uuid`).
    /// Call after `set_identity`; defaults to `derive_ipp_uuid(device_uuid)`.
    pub fn set_ipp_uuid(&mut self, uuid: Uuid) {
        self.ipp_uuid = uuid;
    }

    /// UUID for the IPP endpoint (`printer-uuid` in Get-Printer-Attributes).
    ///
    /// Differs from `device_uuid` (WSD) so that `Add-Printer -IppURL` can
    /// create an Internet Port even when the WSD PnP device is already present.
    /// Falls back to a derive of `device_uuid` if never explicitly set.
    pub fn ipp_uuid(&self) -> Uuid {
        if self.ipp_uuid.is_nil() {
            crate::lab::derive_ipp_uuid(self.device_uuid)
        } else {
            self.ipp_uuid
        }
    }

    pub fn note_wsd_action(&mut self, short: &str) {
        *self.wsd_actions.entry(short.to_string()).or_insert(0) += 1;
    }

    pub fn note_ipp_action(&mut self, op: &str) {
        *self.ipp_actions.entry(op.to_string()).or_insert(0) += 1;
    }

    pub fn profile(&self) -> &Profile {
        &self.profile
    }

    pub fn device_endpoint(&self) -> &str {
        &self.device_endpoint
    }

    pub fn service_endpoint(&self) -> &str {
        &self.service_endpoint
    }

    pub fn device_uuid(&self) -> Uuid {
        self.device_uuid
    }

    /// `ipp://host:port/ipp/print` derived from the WSD device endpoint.
    /// WSDMON with CID:MS_IPP probes this URL to create the spooler queue.
    pub fn ipp_uri(&self) -> String {
        // device_endpoint = "http://192.168.1.70:3921/WebServices/Device"
        if let Some(rest) = self.device_endpoint.strip_prefix("http://") {
            if let Some(slash) = rest.find('/') {
                let host_port = &rest[..slash];
                return format!("ipp://{host_port}/ipp/print");
            }
        }
        "ipp://127.0.0.1:3921/ipp/print".into()
    }

    pub fn inbox_dir(&self) -> &Path {
        &self.inbox_dir
    }

    pub fn status(&self) -> Status {
        let (hellos, probes) = self.discovery.snapshot();
        let available = Profile::list_ids(&self.profiles_dir).unwrap_or_default();
        Status {
            name: self.lab_name.clone(),
            profile_id: self.profile.id.clone(),
            label: self.profile.label.clone(),
            device_endpoint: self.device_endpoint.clone(),
            service_endpoint: self.service_endpoint.clone(),
            device_uuid: self.device_uuid,
            ipp_uuid: self.ipp_uuid(),
            wsd_hellos_sent: hellos,
            wsd_probes_answered: probes,
            wsd_actions: self.wsd_actions.clone(),
            ipp_actions: self.ipp_actions.clone(),
            active_jobs: self.active.len(),
            captured_jobs: self.captured.len(),
            inbox_dir: self.inbox_dir.display().to_string(),
            available_profiles: available,
            document_formats: self.profile.document_formats.clone(),
            note: "WS-Print GetPrinterElements / CreatePrintJob / SendDocument / GetJobElements"
                .into(),
        }
    }

    pub fn reset(&mut self) {
        self.active.clear();
        self.history.clear();
        self.captured.clear();
        self.wsd_actions.clear();
        self.ipp_actions.clear();
        self.event_sinks.clear();
        self.next_job_id = 1;
    }

    pub fn register_event_sink(&mut self, sink: PrintEventSink) {
        self.event_sinks
            .retain(|s| !(s.filter == sink.filter && s.notify_to == sink.notify_to));
        self.event_sinks.push(sink);
        if self.event_sinks.len() > 16 {
            self.event_sinks.remove(0);
        }
    }

    pub fn event_sinks_matching(&self, filter: &str) -> Vec<PrintEventSink> {
        self.event_sinks
            .iter()
            .filter(|s| s.filter == filter)
            .cloned()
            .collect()
    }

    pub fn create_job(&mut self, name: &str, document_format: &str, user: &str) -> u32 {
        let job_id = self.next_job_id;
        self.next_job_id = self.next_job_id.saturating_add(1);
        let token = format!("job-{job_id}");
        self.active.insert(
            job_id,
            ActiveJob {
                job_id,
                document_format: document_format.to_string(),
                name: name.to_string(),
                user: user.to_string(),
                token,
                state: JobState::Pending,
                documents: 0,
                bytes_received: 0,
            },
        );
        job_id
    }

    pub fn job_token(&self, job_id: u32) -> Option<String> {
        self.active
            .get(&job_id)
            .or_else(|| self.history.get(&job_id))
            .map(|j| j.token.clone())
    }

    pub fn get_job(&self, job_id: u32) -> Option<&ActiveJob> {
        self.active
            .get(&job_id)
            .or_else(|| self.history.get(&job_id))
    }

    pub fn get_job_mut(&mut self, job_id: u32) -> Option<&mut ActiveJob> {
        if self.active.contains_key(&job_id) {
            self.active.get_mut(&job_id)
        } else {
            self.history.get_mut(&job_id)
        }
    }

    pub fn active_jobs(&self) -> impl Iterator<Item = &ActiveJob> {
        self.active.values()
    }

    pub fn end_job(&mut self, job_id: u32) {
        if let Some(mut job) = self.active.remove(&job_id) {
            job.state = JobState::Completed;
            self.history.insert(job_id, job);
        }
    }

    pub fn cancel_job(&mut self, job_id: u32) -> bool {
        if let Some(mut job) = self.active.remove(&job_id) {
            job.state = JobState::Canceled;
            self.history.insert(job_id, job);
            true
        } else {
            false
        }
    }

    pub fn capture_document(
        &mut self,
        job_id: u32,
        payload: &[u8],
        document_format: &str,
        peer: &str,
    ) -> Result<CapturedJob> {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let stem = format!("{ts}-{job_id}");
        let bin_path = self.inbox_dir.join(format!("{stem}.bin"));
        let meta_path = self.inbox_dir.join(format!("{stem}.json"));
        std::fs::write(&bin_path, payload)
            .with_context(|| format!("write {}", bin_path.display()))?;
        let meta = serde_json::json!({
            "job_id": job_id,
            "bytes": payload.len(),
            "document_format": document_format,
            "peer": peer,
            "path": bin_path.file_name().and_then(|s| s.to_str()).unwrap_or(""),
        });
        std::fs::write(&meta_path, serde_json::to_vec_pretty(&meta)?)
            .with_context(|| format!("write {}", meta_path.display()))?;
        if let Some(job) = self.active.get_mut(&job_id) {
            job.state = JobState::Processing;
            job.documents = job.documents.saturating_add(1);
            job.bytes_received = job.bytes_received.saturating_add(payload.len());
        }
        let captured = CapturedJob {
            job_id,
            path: bin_path.display().to_string(),
            meta_path: meta_path.display().to_string(),
            bytes: payload.len(),
            document_format: document_format.to_string(),
            peer: peer.to_string(),
        };
        self.captured.push(captured.clone());
        Ok(captured)
    }

    pub fn captured(&self) -> &[CapturedJob] {
        &self.captured
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile_dir::TempDir;

    // Minimal temp dir without extra crate.
    mod tempfile_dir {
        use std::path::PathBuf;
        pub struct TempDir(PathBuf);
        impl TempDir {
            pub fn new() -> Self {
                let p =
                    std::env::temp_dir().join(format!("fakeprint-test-{}", uuid::Uuid::new_v4()));
                std::fs::create_dir_all(&p).unwrap();
                Self(p)
            }
            pub fn path(&self) -> &std::path::Path {
                &self.0
            }
        }
        impl Drop for TempDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    #[test]
    fn capture_writes_bin_and_json() {
        let tmp = TempDir::new();
        let inbox = tmp.path().join("inbox");
        let profile = Profile {
            id: "generic-wsd".into(),
            label: "Fake WSD printer".into(),
            manufacturer: "fakeprint".into(),
            model: "Fake WSD Print".into(),
            device_id: "MFG:Fake;CMD:PJL,PCL,POSTSCRIPT;MDL:WSD Print;CLS:PRINTER;DES:Fake WSD printer;CID:MS_IPP;".into(),
            color_supported: true,
            pages_per_minute: 20,
            pages_per_minute_color: 12,
            document_formats: vec!["application/pdf".into()],
            multiple_document_jobs: true,
        };
        let mut w = World::new(
            tmp.path().to_path_buf(),
            inbox,
            profile,
            Arc::new(DiscoveryStats::default()),
        )
        .unwrap();
        let id = w.create_job("test", "application/pdf", "lab");
        let cap = w
            .capture_document(id, b"%PDF-1.4 fake", "application/pdf", "127.0.0.1:1")
            .unwrap();
        assert_eq!(cap.bytes, 13);
        assert!(Path::new(&cap.path).is_file());
        assert!(Path::new(&cap.meta_path).is_file());
        assert_eq!(w.get_job(id).unwrap().state, JobState::Processing);
        w.end_job(id);
        assert_eq!(w.get_job(id).unwrap().state, JobState::Completed);
    }
}
