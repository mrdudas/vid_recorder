//! Scenarios: named camera + recording settings, one JSON file each.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::frame::PixelFormat;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Scenario {
    pub name: String,
    pub camera: CameraSettings,
    pub recording: RecordingSettings,
    /// `.cube` file in the S-Log LUT directory, applied to the live preview
    /// only. Recordings are always raw.
    #[serde(default)]
    pub preview_lut: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct CameraSettings {
    pub pixel_format: PixelFormat,
    pub width: u32,
    pub height: u32,
    #[serde(default)]
    pub offset_x: u32,
    #[serde(default)]
    pub offset_y: u32,
    pub frame_rate: f64,
    pub exposure_us: f64,
    pub gain_db: f64,
    /// Any other GenICam feature, written as is (name -> number/string/bool).
    #[serde(default)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct RecordingSettings {
    /// Encoder profile id, or "auto" (first available hardware encoder).
    pub encoder: String,
    pub output_dir: PathBuf,
}

impl Default for Scenario {
    fn default() -> Self {
        Scenario {
            name: "default".into(),
            camera: CameraSettings {
                pixel_format: PixelFormat::Mono8,
                width: 1920,
                height: 1080,
                offset_x: 0,
                offset_y: 0,
                frame_rate: 50.0,
                exposure_us: 10_000.0,
                gain_db: 0.0,
                extra: BTreeMap::new(),
            },
            recording: RecordingSettings { encoder: "auto".into(), output_dir: "recordings".into() },
            preview_lut: None,
        }
    }
}

/// File name for a scenario: name with anything unsafe replaced by '_'.
fn file_name(name: &str) -> String {
    let safe: String = name
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    format!("{safe}.json")
}

pub fn load_all(dir: &Path) -> Vec<Scenario> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else { return out };
    for e in entries.flatten() {
        let path = e.path();
        if path.extension().is_some_and(|x| x == "json") {
            match fs::read_to_string(&path).map_err(|e| e.to_string()).and_then(|s| {
                serde_json::from_str::<Scenario>(&s).map_err(|e| e.to_string())
            }) {
                Ok(s) => out.push(s),
                Err(err) => eprintln!("skipping scenario {}: {err}", path.display()),
            }
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

pub fn save(dir: &Path, s: &Scenario) -> std::io::Result<PathBuf> {
    fs::create_dir_all(dir)?;
    let path = dir.join(file_name(&s.name));
    let json = serde_json::to_string_pretty(s).expect("scenario serializes");
    fs::write(&path, json + "\n")?;
    Ok(path)
}

pub fn delete(dir: &Path, name: &str) -> std::io::Result<()> {
    fs::remove_file(dir.join(file_name(name)))
}
