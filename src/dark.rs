//! Dark calibration: the per-pixel level the sensor gives with no light
//! (offset, fixed pattern, hot pixels), averaged over many frames with the
//! lens covered. Subtracted from the raw pixels of the preview, the
//! averaged image and its saved PNG. The recording stays raw.
//!
//! Only for raw formats (Mono, Bayer): a dark frame is valid for the same
//! format, size, crop and mirroring; exposure, gain and temperature change
//! it, so a mismatch is reported.
//!
//! File: `darks/<scenario>.dark`: one JSON line (the settings it was taken
//! with), then the levels as little-endian f32 (0..1 of the full scale).

use std::io::{BufRead, Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::frame::{Frame, PixelFormat};
use crate::scenario::CameraSettings;

pub const DIR: &str = "darks";

/// What the dark frame was taken with.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Meta {
    pub format: PixelFormat,
    pub width: u32,
    pub height: u32,
    pub offset_x: u32,
    pub offset_y: u32,
    pub reverse_x: bool,
    pub reverse_y: bool,
    pub exposure_us: f64,
    pub gain_db: f64,
    pub auto_exposure: bool,
    pub frames: u32,
    pub taken: String,
    /// Mean level and hot pixels (above mean + 6 σ), for information.
    pub mean: f32,
    pub hot: u32,
}

pub struct Dark {
    pub meta: Meta,
    pub data: Vec<f32>,
}

/// The format can be dark-corrected (raw sensor values).
pub fn supported(f: PixelFormat) -> bool {
    f.is_bayer() || matches!(f, PixelFormat::Mono8 | PixelFormat::Mono10 | PixelFormat::Mono12 | PixelFormat::Mono16)
}

pub fn path(scenario: &str) -> PathBuf {
    Path::new(DIR).join(format!("{scenario}.dark"))
}

impl Dark {
    /// Why it cannot be used for these frames (`Err`), or warnings about
    /// settings that differ (`Ok`, possibly empty).
    pub fn check(&self, c: &CameraSettings, f: Option<(PixelFormat, u32, u32)>) -> Result<Vec<String>, String> {
        let m = &self.meta;
        if let Some((format, w, h)) = f
            && (format, w, h) != (m.format, m.width, m.height)
        {
            return Err(format!("a sötétkép {} {}×{}, a kamera most {} {w}×{h}", m.format.name(), m.width, m.height, format.name()));
        }
        if (c.offset_x, c.offset_y, c.reverse_x, c.reverse_y) != (m.offset_x, m.offset_y, m.reverse_x, m.reverse_y) {
            return Err("a sötétkép más kivágással vagy tükrözéssel készült".into());
        }
        let mut warn = Vec::new();
        if c.auto_exposure || m.auto_exposure {
            warn.push("automatikus expozíciónál a sötétszint változik".into());
        } else {
            if (c.exposure_us - m.exposure_us).abs() > m.exposure_us * 0.02 {
                warn.push(format!("az expozíció most {} µs, a sötétképé {} µs", c.exposure_us, m.exposure_us));
            }
            if (c.gain_db - m.gain_db).abs() > 0.05 {
                warn.push(format!("az erősítés most {} dB, a sötétképé {} dB", c.gain_db, m.gain_db));
            }
        }
        Ok(warn)
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        std::fs::create_dir_all(path.parent().unwrap_or(Path::new("."))).map_err(|e| e.to_string())?;
        let mut w = std::io::BufWriter::new(std::fs::File::create(path).map_err(|e| e.to_string())?);
        let head = serde_json::to_string(&self.meta).map_err(|e| e.to_string())?;
        writeln!(w, "{head}").map_err(|e| e.to_string())?;
        let bytes: Vec<u8> = self.data.iter().flat_map(|v| v.to_le_bytes()).collect();
        w.write_all(&bytes).map_err(|e| e.to_string())?;
        w.flush().map_err(|e| e.to_string())
    }

    pub fn load(path: &Path) -> Result<Dark, String> {
        let mut r = std::io::BufReader::new(std::fs::File::open(path).map_err(|e| e.to_string())?);
        let mut head = String::new();
        r.read_line(&mut head).map_err(|e| e.to_string())?;
        let meta: Meta = serde_json::from_str(&head).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut bytes = Vec::new();
        r.read_to_end(&mut bytes).map_err(|e| e.to_string())?;
        let n = meta.width as usize * meta.height as usize;
        if bytes.len() != n * 4 {
            return Err(format!("{}: sérült fájl", path.display()));
        }
        let data = bytes.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect();
        Ok(Dark { meta, data })
    }
}

/// Normalized raw value of pixel `i` (0..1 of the format's full scale).
pub fn raw(f: &Frame, i: usize) -> f32 {
    let max = ((1u32 << f.format.bits()) - 1) as f32;
    if f.format.bytes_per_pixel() == 1 {
        f.data[i] as f32 / max
    } else {
        u16::from_le_bytes([f.data[2 * i], f.data[2 * i + 1]]) as f32 / max
    }
}

/// Averages dark frames.
pub struct Capture {
    pub target: u32,
    pub n: u32,
    sum: Vec<f64>,
    shape: Option<(PixelFormat, u32, u32)>,
}

impl Capture {
    pub fn new(target: u32) -> Self {
        Capture { target, n: 0, sum: Vec::new(), shape: None }
    }

    /// Add a frame; `Err` if the format or size changed (or is not raw).
    pub fn add(&mut self, f: &Frame) -> Result<(), String> {
        if !supported(f.format) {
            return Err(format!("{} kép nem kalibrálható (csak Mono és Bayer)", f.format.name()));
        }
        let shape = (f.format, f.width, f.height);
        match self.shape {
            None => {
                self.shape = Some(shape);
                self.sum = vec![0.0; f.width as usize * f.height as usize];
            }
            Some(s) if s != shape => return Err("a kép formátuma vagy mérete közben megváltozott".into()),
            _ => {}
        }
        for (i, s) in self.sum.iter_mut().enumerate() {
            *s += raw(f, i) as f64;
        }
        self.n += 1;
        Ok(())
    }

    pub fn done(&self) -> bool {
        self.n >= self.target
    }

    /// The averaged dark frame, with the settings it was taken with.
    pub fn finish(self, c: &CameraSettings) -> Option<Dark> {
        let (format, width, height) = self.shape?;
        let data: Vec<f32> = self.sum.iter().map(|s| (s / self.n as f64) as f32).collect();
        let n = data.len() as f64;
        let mean = data.iter().map(|&v| v as f64).sum::<f64>() / n;
        let sd = (data.iter().map(|&v| (v as f64 - mean).powi(2)).sum::<f64>() / n).sqrt();
        let hot = data.iter().filter(|&&v| v as f64 > mean + 6.0 * sd.max(1e-6)).count() as u32;
        let meta = Meta {
            format,
            width,
            height,
            offset_x: c.offset_x,
            offset_y: c.offset_y,
            reverse_x: c.reverse_x,
            reverse_y: c.reverse_y,
            exposure_us: c.exposure_us,
            gain_db: c.gain_db,
            auto_exposure: c.auto_exposure,
            frames: self.n,
            taken: chrono::Local::now().format("%Y-%m-%d %H:%M").to_string(),
            mean: mean as f32,
            hot,
        };
        Some(Dark { meta, data })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_save_load() {
        let mut c = Capture::new(4);
        for k in 0..4u16 {
            let data: Vec<u8> = (0..16u16).flat_map(|i| (100 + i + k).to_le_bytes()).collect();
            let f = Frame { id: 0, timestamp_ns: 0, width: 4, height: 4, format: PixelFormat::Mono12, data };
            c.add(&f).unwrap();
        }
        let d = c.finish(&crate::scenario::Scenario::default().camera).unwrap();
        assert!((d.data[0] - 101.5 / 4095.0).abs() < 1e-6);
        let p = std::env::temp_dir().join("vid_recorder_test.dark");
        d.save(&p).unwrap();
        let e = Dark::load(&p).unwrap();
        assert_eq!(e.data, d.data);
        assert_eq!(e.meta, d.meta);
        let _ = std::fs::remove_file(p);
    }
}

#[cfg(test)]
mod subtract {
    use super::*;

    #[test]
    fn dark_frame_cancels_fixed_pattern() {
        // Bayer frame = per-pixel pattern + uniform light; minus the pattern
        // leaves the light everywhere.
        let (w, h) = (8u32, 6u32);
        let pattern: Vec<u8> = (0..w * h).map(|i| (i * 13 % 40) as u8).collect();
        let mut cap = Capture::new(1);
        cap.add(&Frame { id: 0, timestamp_ns: 0, width: w, height: h, format: PixelFormat::BayerRG8, data: pattern.clone() }).unwrap();
        let d = cap.finish(&crate::scenario::Scenario::default().camera).unwrap();
        let lit = Frame { id: 1, timestamp_ns: 0, width: w, height: h, format: PixelFormat::BayerRG8, data: pattern.iter().map(|p| p + 100).collect() };
        let img = crate::stack::to_image(&lit, Some(&d)).unwrap();
        assert!(img.data.iter().all(|&v| (v - 100.0 / 255.0).abs() < 1e-5), "{:?}", &img.data[..6]);
    }
}
