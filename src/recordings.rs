//! Recordings page backend: list the recordings in the scenarios' output
//! folders, export them in the background (our own `export` command as a
//! child process) and delete them to free disk space.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime};

use crate::encoder::{ffmpeg_path, ffprobe_path};
use crate::frame::{Frame, PixelFormat};

/// Exports go to this subfolder of the recording's folder.
pub const EXPORT_DIR: &str = "export";

pub struct Recording {
    pub video: PathBuf,
    /// In an `export/` folder (made by us) rather than a raw recording.
    pub export: bool,
    pub bytes: u64,
    pub modified: SystemTime,
    /// From the timestamp CSV: frame count and duration [s].
    pub frames: Option<u64>,
    pub seconds: Option<f64>,
}

impl Recording {
    pub fn name(&self) -> String {
        self.video.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
    }

}

/// Videos directly in `dirs` and in their `export/` subfolders, newest first.
pub fn list(dirs: &[PathBuf]) -> Vec<Recording> {
    let mut out = Vec::new();
    for (dir, export) in dirs.iter().flat_map(|d| [(d.clone(), false), (d.join(EXPORT_DIR), true)]) {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for e in entries.flatten() {
            let video = e.path();
            let is_video = video.extension().is_some_and(|x| ["mkv", "nut", "mp4", "mov"].iter().any(|v| x.eq_ignore_ascii_case(v)));
            let Ok(meta) = e.metadata() else { continue };
            if !is_video || !meta.is_file() {
                continue;
            }
            let (frames, seconds) = csv_info(&video.with_extension("csv"));
            out.push(Recording {
                export,
                bytes: meta.len(),
                modified: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                frames,
                seconds,
                video,
            });
        }
    }
    out.sort_by(|a, b| b.modified.cmp(&a.modified));
    out
}

/// Frame count and first-to-last timestamp span of a timestamp CSV.
fn csv_info(csv: &Path) -> (Option<u64>, Option<f64>) {
    let Ok(text) = std::fs::read_to_string(csv) else { return (None, None) };
    let ts: Vec<u64> = text.lines().skip(1).filter_map(|l| l.split(',').next()?.trim().parse().ok()).collect();
    // Frames times the median interval: pauses and camera dropouts do not
    // count as recorded time.
    let mut d: Vec<u64> = ts.windows(2).filter_map(|w| w[1].checked_sub(w[0])).filter(|&d| d > 0).collect();
    d.sort_unstable();
    let span = d.get(d.len() / 2).map(|&m| m as f64 / 1e9 * ts.len() as f64);
    (Some(ts.len() as u64), span)
}

/// Delete a recording and its timestamp CSV.
pub fn delete(video: &Path) -> std::io::Result<()> {
    std::fs::remove_file(video)?;
    let csv = video.with_extension("csv");
    if csv.exists() {
        std::fs::remove_file(csv)?;
    }
    Ok(())
}

/// Free space on the disk holding `dir` (or its nearest existing parent).
pub fn free_space(dir: &Path) -> Option<u64> {
    dir.ancestors().find(|d| d.exists()).and_then(|d| fs4::available_space(d).ok())
}

/// What to make of a recording.
#[derive(Clone, PartialEq)]
pub struct ExportSettings {
    /// `archive`, `native` or magnification methods (see `export::USAGE`).
    pub method: String,
    /// Output codec (not for `archive`).
    pub codec: String,
    /// Every frame, played at 25 fps.
    pub slow: bool,
    /// Processing width, 0 = full.
    pub width: u32,
    /// Part of the recording [s]; `dur` 0 = to the end.
    pub start: f64,
    pub dur: f64,
    /// Delete the recording once the export succeeded.
    pub delete_after: bool,
}

impl Default for ExportSettings {
    fn default() -> Self {
        ExportSettings {
            method: "archive".into(),
            codec: "h264".into(),
            slow: false,
            width: 0,
            start: 0.0,
            dur: 0.0,
            delete_after: false,
        }
    }
}

impl ExportSettings {
    fn output(&self, video: &Path) -> PathBuf {
        let ext = match (self.method.as_str(), self.codec.as_str()) {
            ("archive", _) | (_, "ffv1") => "mkv",
            (_, "prores") => "mov",
            _ => "mp4",
        };
        let stem = video.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let dir = video.parent().unwrap_or(Path::new(".")).join(EXPORT_DIR);
        dir.join(format!("{stem}_{}.{ext}", self.method.replace(',', "+")))
    }

    fn args(&self, video: &Path, output: &Path) -> Vec<String> {
        let mut a = vec!["export".into(), video.display().to_string(), output.display().to_string(), self.method.clone()];
        if self.method != "archive" {
            a.push(format!("codec={}", self.codec));
            if self.slow {
                a.push("slow".into());
            }
            if self.width > 0 {
                a.push(format!("width={}", self.width));
            }
            if self.start > 0.0 {
                a.push(format!("start={}", self.start));
            }
            if self.dur > 0.0 {
                a.push(format!("dur={}", self.dur));
            }
        }
        a
    }
}

/// One export running as a child process.
pub struct Job {
    pub video: PathBuf,
    pub output: PathBuf,
    pub settings: ExportSettings,
    pub started: Instant,
    child: Child,
    /// Last line the export printed (progress or error).
    pub last_line: Arc<Mutex<String>>,
}

pub enum JobState {
    Running,
    Done,
    Failed(String),
}

impl Job {
    pub fn start(video: &Path, settings: &ExportSettings) -> Result<Job, String> {
        let output = settings.output(video);
        if output.exists() {
            return Err(format!("{} már létezik", output.display()));
        }
        std::fs::create_dir_all(output.parent().unwrap()).map_err(|e| e.to_string())?;
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let mut child = crate::encoder::low_priority(&exe)
            .args(settings.args(video, &output))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("export: {e}"))?;
        let last_line = Arc::new(Mutex::new(String::new()));
        let (stderr, last) = (child.stderr.take().unwrap(), last_line.clone());
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                *last.lock().unwrap() = line;
            }
        });
        Ok(Job { video: video.to_path_buf(), output, settings: settings.clone(), started: Instant::now(), child, last_line })
    }

    /// Check the child; on success delete the recording if asked to.
    pub fn poll(&mut self) -> JobState {
        match self.child.try_wait() {
            Ok(None) => JobState::Running,
            Ok(Some(st)) if st.success() && self.output.exists() => {
                if self.settings.delete_after
                    && let Err(e) = delete(&self.video)
                {
                    return JobState::Failed(format!("az export kész, de a felvétel nem törölhető: {e}"));
                }
                JobState::Done
            }
            Ok(Some(_)) => {
                // Leave no half-written file behind.
                let _ = std::fs::remove_file(&self.output);
                JobState::Failed(self.last_line.lock().unwrap().clone())
            }
            Err(e) => JobState::Failed(e.to_string()),
        }
    }

    pub fn cancel(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.output);
        let _ = std::fs::remove_file(self.output.with_extension("csv"));
    }
}

/// What the viewer needs to know about a video file.
#[derive(Clone)]
pub struct MediaInfo {
    pub width: u32,
    pub height: u32,
    pub duration: f64,
    pub fps: f64,
    pub pix_fmt: String,
    /// Camera format from the recorder's PIXEL_FORMAT tag.
    pub format: Option<PixelFormat>,
}

pub fn media_info(path: &Path) -> Result<MediaInfo, String> {
    let out = Command::new(ffprobe_path())
        .args(["-v", "error", "-select_streams", "v:0"])
        .args(["-show_entries", "format=duration:stream=width,height,pix_fmt,avg_frame_rate:stream_tags=PIXEL_FORMAT", "-of", "default=nw=1"])
        .arg(path)
        .output()
        .map_err(|e| format!("ffprobe: {e}"))?;
    let text = String::from_utf8_lossy(&out.stdout);
    let field = |k: &str| text.lines().find_map(|l| l.split_once('=').filter(|(key, _)| key.eq_ignore_ascii_case(k)).map(|(_, v)| v.to_string()));
    let num = |k: &str| field(k).and_then(|v| v.parse::<f64>().ok());
    let fps = field("avg_frame_rate")
        .and_then(|r| {
            let (n, d) = r.split_once('/')?;
            Some(n.parse::<f64>().ok()? / d.parse::<f64>().ok()?)
        })
        .filter(|f| f.is_finite() && *f > 0.0)
        .unwrap_or(25.0);
    Ok(MediaInfo {
        width: num("width").ok_or_else(|| format!("{}: nem videó", path.display()))? as u32,
        height: num("height").unwrap_or(0.0) as u32,
        duration: num("duration").unwrap_or(0.0),
        fps,
        pix_fmt: field("pix_fmt").unwrap_or_default(),
        format: field("TAG:PIXEL_FORMAT").and_then(|t| PixelFormat::ALL.into_iter().find(|f| f.name() == t)),
    })
}

/// One decoded frame: raw camera samples (grey / Bayer recordings, shown
/// like the live preview) or RGB at most `max_width` wide (anything else).
pub enum Still {
    Raw(Frame),
    Rgb { width: usize, height: usize, data: Vec<u8> },
}

/// Decode the frame at `t` seconds.
pub fn decode_frame(path: &Path, info: &MediaInfo, t: f64, max_width: u32) -> Result<Still, String> {
    let grey = matches!(info.pix_fmt.as_str(), "gray" | "gray10le" | "gray12le" | "gray16le");
    let mut cmd = Command::new(ffmpeg_path());
    cmd.args(["-v", "error", "-nostdin", "-ss", &format!("{t:.4}")]).arg("-i").arg(path);
    cmd.args(["-frames:v", "1", "-an", "-f", "rawvideo"]);
    let (w, h) = if grey {
        // Same sample layout as stored: the values stay the camera's.
        cmd.args(["-pix_fmt", &info.pix_fmt]);
        (info.width, info.height)
    } else {
        let w = info.width.min(max_width) & !1;
        let h = ((w as f64 * info.height as f64 / info.width as f64).round() as u32).max(2) & !1;
        cmd.args(["-vf", &format!("scale={w}:{h}"), "-pix_fmt", "rgb24"]);
        (w, h)
    };
    let out = cmd.arg("-").output().map_err(|e| format!("ffmpeg: {e}"))?;
    if !out.status.success() || out.stdout.is_empty() {
        return Err(format!("nincs képkocka itt: {t:.2} s {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    if !grey {
        return Ok(Still::Rgb { width: w as usize, height: h as usize, data: out.stdout });
    }
    let wide = info.pix_fmt != "gray";
    let format = info.format.filter(|f| (f.bytes_per_pixel() == 2) == wide && f.bits() <= 16 && !matches!(f, PixelFormat::Yuyv | PixelFormat::Uyvy | PixelFormat::Nv12 | PixelFormat::Mjpeg));
    let format = format.unwrap_or(match info.pix_fmt.as_str() {
        "gray" => PixelFormat::Mono8,
        "gray10le" => PixelFormat::Mono10,
        "gray12le" => PixelFormat::Mono12,
        _ => PixelFormat::Mono16,
    });
    Ok(Still::Raw(Frame { id: 0, timestamp_ns: 0, width: w, height: h, format, data: out.stdout }))
}
