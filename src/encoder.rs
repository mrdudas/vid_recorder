//! ffmpeg discovery and encoder profiles.
//!
//! Encoding runs in a separate ffmpeg process fed with raw frames on stdin.
//! Which encoders work depends on the machine (GPU, drivers, ffmpeg build),
//! so every profile is probed by encoding a few test frames.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::OnceLock;

use crate::frame::PixelFormat;

pub struct Profile {
    pub id: &'static str,
    pub label: &'static str,
    pub hardware: bool,
    /// Highest bit depth verified to round-trip bit-exactly (0 = lossy).
    /// "auto" only picks a profile that is lossless for the frame format.
    pub lossless_bits: u32,
    /// Output codec arguments (everything after the input).
    pub args: &'static [&'static str],
}

/// Recordings must be lossless (raw sensor data, no LUT), so "auto" only
/// picks a lossless profile. The lossy hardware profiles stay selectable.
/// MJPEG from UVC cameras is stored as received (no decode, no re-encode).
pub const MJPEG_COPY: Profile = Profile {
    id: "copy",
    label: "MJPEG (eredeti, újrakódolás nélkül)",
    hardware: false,
    lossless_bits: 8,
    args: &["-c:v", "copy"],
};

pub const PROFILES: &[Profile] = &[
    Profile {
        id: "hevc_nvenc",
        // Not verified: NVENC takes no grey input, the YUV conversion may alter
        // values and it is limited to 10 bit.
        label: "NVIDIA NVENC HEVC (lossless tune, nem ellenőrzött)",
        hardware: true,
        lossless_bits: 0,
        args: &["-c:v", "hevc_nvenc", "-preset", "p7", "-tune", "lossless"],
    },
    Profile {
        id: "hevc_qsv",
        label: "Intel QSV HEVC (veszteséges)",
        hardware: true,
        lossless_bits: 0,
        args: &["-c:v", "hevc_qsv", "-preset", "veryslow", "-global_quality", "12"],
    },
    Profile {
        id: "hevc_amf",
        label: "AMD AMF HEVC (veszteséges)",
        hardware: true,
        lossless_bits: 0,
        args: &["-c:v", "hevc_amf", "-quality", "quality", "-rc", "cqp", "-qp_i", "10", "-qp_p", "10"],
    },
    Profile {
        id: "hevc_videotoolbox",
        label: "VideoToolbox HEVC (macOS) (veszteséges)",
        hardware: true,
        lossless_bits: 0,
        args: &["-c:v", "hevc_videotoolbox", "-b:v", "200M", "-allow_sw", "0"],
    },
    // Fast, but bit-exact only for 8 bit (higher depths are converted).
    Profile {
        id: "ffvhuff",
        label: "FFVHuff (veszteségmentes 8 bitig, szoftveres, gyors)",
        hardware: false,
        lossless_bits: 8,
        args: &["-c:v", "ffvhuff", "-pred", "median"],
    },
    Profile {
        id: "ffv1",
        label: "FFV1 (veszteségmentes, szoftveres)",
        hardware: false,
        lossless_bits: 16,
        args: &["-c:v", "ffv1", "-level", "3", "-g", "1", "-slices", "16", "-slicecrc", "1"],
    },
];

/// Path of the ffmpeg binary: `VID_RECORDER_FFMPEG`, then PATH, then common
/// install locations (apps started from a GUI often have a minimal PATH).
pub fn ffmpeg_path() -> PathBuf {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| {
        if let Ok(p) = std::env::var("VID_RECORDER_FFMPEG") {
            return p.into();
        }
        let exe = if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" };
        if let Some(paths) = std::env::var_os("PATH") {
            for dir in std::env::split_paths(&paths) {
                let p = dir.join(exe);
                if p.is_file() {
                    return p;
                }
            }
        }
        for p in ["/opt/local/bin/ffmpeg", "/opt/homebrew/bin/ffmpeg", "/usr/local/bin/ffmpeg", "/usr/bin/ffmpeg"] {
            if std::path::Path::new(p).is_file() {
                return p.into();
            }
        }
        exe.into()
    })
    .clone()
}

/// ffprobe next to the ffmpeg binary.
pub fn ffprobe_path() -> PathBuf {
    ffmpeg_path().with_file_name(if cfg!(windows) { "ffprobe.exe" } else { "ffprobe" })
}

fn probe(p: &Profile) -> bool {
    Command::new(ffmpeg_path())
        .args(["-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i", "testsrc2=s=640x480:r=25", "-frames:v", "5", "-pix_fmt", "yuv420p"])
        .args(p.args)
        .args(["-f", "null", "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Profiles that work on this machine (probed once).
pub fn available() -> &'static [&'static Profile] {
    static AVAIL: OnceLock<Vec<&'static Profile>> = OnceLock::new();
    AVAIL.get_or_init(|| {
        let handles: Vec<_> = PROFILES.iter().map(|p| std::thread::spawn(move || (p, probe(p)))).collect();
        handles.into_iter().filter_map(|h| h.join().ok()).filter(|(_, ok)| *ok).map(|(p, _)| p).collect()
    })
}

/// Resolve "auto" or a profile id to a usable profile for `format`.
/// MJPEG is always copied unchanged.
pub fn resolve(id: &str, format: PixelFormat) -> Result<&'static Profile, String> {
    if format == PixelFormat::Mjpeg {
        return Ok(&MJPEG_COPY);
    }
    let bits = format.bits();
    let avail = available();
    if id == "auto" {
        return avail
            .iter()
            .find(|p| p.lossless_bits >= bits)
            .copied()
            .ok_or_else(|| format!("no working lossless {bits}-bit encoder (ffmpeg: {})", ffmpeg_path().display()));
    }
    avail
        .iter()
        .find(|p| p.id == id)
        .copied()
        .ok_or_else(|| format!("encoder {id} is not available on this machine"))
}
