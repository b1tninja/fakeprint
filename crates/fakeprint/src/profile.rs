//! Capability profiles loaded from JSON.

use std::fs;
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    pub id: String,
    pub label: String,
    #[serde(default = "default_manufacturer")]
    pub manufacturer: String,
    #[serde(default = "default_model")]
    pub model: String,
    #[serde(default = "default_device_id")]
    pub device_id: String,
    #[serde(default = "default_true")]
    pub color_supported: bool,
    #[serde(default = "default_ppm")]
    pub pages_per_minute: u32,
    #[serde(default = "default_ppm_color")]
    pub pages_per_minute_color: u32,
    #[serde(default = "default_formats")]
    pub document_formats: Vec<String>,
    #[serde(default = "default_true")]
    pub multiple_document_jobs: bool,
}

fn default_manufacturer() -> String {
    "fakeprint".into()
}
fn default_model() -> String {
    "Fake WSD Print".into()
}
fn default_device_id() -> String {
    "MFG:Fake;CMD:PJL,PCL,POSTSCRIPT;MDL:WSD Print;CLS:PRINTER;DES:Fake WSD printer;CID:MS_IPP;"
        .into()
}
fn default_true() -> bool {
    true
}
fn default_ppm() -> u32 {
    20
}
fn default_ppm_color() -> u32 {
    12
}
fn default_formats() -> Vec<String> {
    vec![
        "application/octet-stream".into(),
        "application/pdf".into(),
        "image/pwg-raster".into(),
    ]
}

impl Profile {
    pub fn load(dir: &Path, id: &str) -> Result<Self> {
        let path = dir.join(format!("{id}.json"));
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("read profile {}", path.display()))?;
        let p: Self =
            serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
        if p.id != id {
            bail!("profile id mismatch: file says {:?}, expected {id}", p.id);
        }
        Ok(p)
    }

    pub fn list_ids(dir: &Path) -> Result<Vec<String>> {
        let mut ids = Vec::new();
        if !dir.is_dir() {
            return Ok(ids);
        }
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("json") {
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    ids.push(stem.to_string());
                }
            }
        }
        ids.sort();
        Ok(ids)
    }
}
