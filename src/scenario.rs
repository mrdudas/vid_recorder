//! Scenarios: named camera + recording settings, one JSON file each.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::frame::PixelFormat;

/// Which camera a scenario uses.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Device {
    /// First USB3 Vision camera (Allied Vision etc.).
    #[default]
    U3v,
    /// UVC camera (webcam, panel camera) by its system name.
    Uvc { name: String },
    /// Generated test pattern.
    Sim,
}

impl Device {
    /// The camera crops itself (UVC does not: the engine crops in software).
    pub fn hw_crop(&self) -> bool {
        !matches!(self, Device::Uvc { .. })
    }

    pub fn label(&self) -> String {
        match self {
            Device::U3v => "USB3 Vision (első kamera)".into(),
            Device::Uvc { name } => format!("UVC: {name}"),
            Device::Sim => "Szimulált kamera".into(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Scenario {
    pub name: String,
    #[serde(default)]
    pub device: Device,
    pub camera: CameraSettings,
    pub recording: RecordingSettings,
    /// `.cube` file in the S-Log LUT directory, applied to the live preview
    /// only. Recordings are always raw.
    #[serde(default)]
    pub preview_lut: Option<String>,
    /// Live preview look. Recordings are always raw.
    #[serde(default)]
    pub view: View,
    /// What the main page's Export button makes of a recording.
    #[serde(default)]
    pub export: crate::recordings::ExportSettings,
}

/// Preview-only adjustments (applied after the LUT, if any).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct View {
    /// Automatic (grey-world) white balance for colour sources.
    pub auto_white_balance: bool,
    /// Stretch the preview's levels to the image content (auto-levels).
    pub auto_levels: bool,
    /// Added to the 0..1 signal.
    pub brightness: f32,
    /// Multiplier around mid grey.
    pub contrast: f32,
    /// Average the preview frames (noise reduction; see `stack`).
    pub average: bool,
    /// Frames to average; 0: automatic (until the image stops changing).
    pub average_frames: u32,
    /// Align the frames before averaging (shift and rotation).
    pub align: bool,
    /// Subtract the scenario's dark frame (see `dark`).
    pub dark: bool,
}

impl Default for View {
    fn default() -> Self {
        View { auto_white_balance: true, auto_levels: false, brightness: 0.0, contrast: 1.0, average: false, average_frames: 0, align: false, dark: false }
    }
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
    /// Camera's own automatic exposure and gain (changes the recording).
    #[serde(default)]
    pub auto_exposure: bool,
    /// Range the auto exposure may use [µs]; clamped to the camera's range
    /// and to one frame period.
    #[serde(default = "auto_exposure_range")]
    pub auto_exposure_range: (f64, f64),
    /// Range the auto gain may use [dB]; clamped to the camera's range.
    #[serde(default = "auto_gain_range")]
    pub auto_gain_range: (f64, f64),
    /// Mean brightness the auto exposure aims at [% of full scale]
    /// (Alvium `IntensityControllerTarget`). The camera's own default (~50%)
    /// burns out the bright parts of a contrasty scene: 20% of the pixels
    /// saturated in a room with a window, 1% at 25% (2026-11-27).
    #[serde(default = "auto_target")]
    pub auto_target: f64,
    /// Size the camera delivers, for cameras that cannot crop (UVC): the
    /// image is then cut to width × height at the offsets in software.
    /// `None`: the camera delivers width × height itself (hardware crop).
    #[serde(default)]
    pub capture_size: Option<(u32, u32)>,
    /// Mirror the image on the camera (both = rotated by 180°).
    #[serde(default)]
    pub reverse_x: bool,
    #[serde(default)]
    pub reverse_y: bool,
    /// Any other GenICam feature, written as is (name -> number/string/bool).
    #[serde(default)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

fn auto_target() -> f64 {
    25.0
}

fn auto_exposure_range() -> (f64, f64) {
    (0.0, 1_000_000.0)
}

fn auto_gain_range() -> (f64, f64) {
    (0.0, 48.0)
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct RecordingSettings {
    /// Encoder profile id, or "auto" (first available hardware encoder).
    pub encoder: String,
    pub output_dir: PathBuf,
    /// Store 10/12-bit sensor samples as 8-bit codes on the noise-matched
    /// curve (`curve.rs`): full dynamic range in 8 bits.
    #[serde(default)]
    pub curve8: bool,
}

impl Default for Scenario {
    fn default() -> Self {
        Scenario {
            name: "default".into(),
            device: Device::U3v,
            camera: CameraSettings {
                pixel_format: PixelFormat::Mono8,
                width: 1920,
                height: 1080,
                offset_x: 0,
                offset_y: 0,
                frame_rate: 50.0,
                exposure_us: 10_000.0,
                gain_db: 0.0,
                auto_exposure: false,
                auto_exposure_range: auto_exposure_range(),
                auto_gain_range: auto_gain_range(),
                auto_target: auto_target(),
                capture_size: None,
                reverse_x: false,
                reverse_y: false,
                extra: BTreeMap::new(),
            },
            recording: RecordingSettings { encoder: "auto".into(), output_dir: "recordings".into(), curve8: false },
            preview_lut: None,
            view: View::default(),
            export: Default::default(),
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
