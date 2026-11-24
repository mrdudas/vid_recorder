//! UVC cameras (webcams, panel cameras) via `nokhwa`:
//! AVFoundation on macOS, Media Foundation on Windows, V4L2 on Linux.
//!
//! UVC gives no frame counter, so frame ids are assigned here in arrival
//! order and lost frames cannot be detected from ids. Timestamps are the
//! backend's capture time if it reports one, else the host arrival time.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use nokhwa::Camera as NCamera;
use nokhwa::pixel_format::RgbFormat;
use nokhwa::utils::{
    ApiBackend, CameraFormat, CameraIndex, FrameFormat, RequestedFormat, RequestedFormatType, Resolution,
};

use crate::camera::{Camera, FrameSink, Mode};
use crate::frame::{BufferPool, Frame, PixelFormat};
use crate::scenario::CameraSettings;

/// Names of the UVC cameras the system reports. On Linux one camera shows
/// up as several /dev/video nodes with the same name (capture + metadata),
/// so names are listed once.
pub fn list() -> Vec<String> {
    if !ensure_permission() {
        return Vec::new();
    }
    let mut names: Vec<String> = Vec::new();
    for c in nokhwa::query(ApiBackend::Auto).unwrap_or_default() {
        if !names.contains(&c.human_name()) {
            names.push(c.human_name());
        }
    }
    names
}

/// macOS asks the user for camera access once; elsewhere this is a no-op.
fn ensure_permission() -> bool {
    if nokhwa::nokhwa_check() {
        return true;
    }
    let (tx, rx) = mpsc::channel();
    nokhwa::nokhwa_initialize(move |granted| {
        let _ = tx.send(granted);
    });
    rx.recv_timeout(Duration::from_secs(60)).unwrap_or(false)
}

/// All device indices with this name; the first one that opens and starts
/// streaming is used (on Linux the metadata node has the same name but
/// fails to open).
fn find_indices(name: &str) -> Result<Vec<CameraIndex>, String> {
    if !ensure_permission() {
        return Err("camera access denied".into());
    }
    let cams = nokhwa::query(ApiBackend::Auto).map_err(|e| e.to_string())?;
    let found: Vec<CameraIndex> = cams.iter().filter(|c| c.human_name() == name).map(|c| c.index().clone()).collect();
    if found.is_empty() {
        return Err(format!("UVC camera \"{name}\" not found"));
    }
    Ok(found)
}

fn to_fourcc(f: PixelFormat) -> Result<FrameFormat, String> {
    Ok(match f {
        PixelFormat::Mjpeg => FrameFormat::MJPEG,
        PixelFormat::Yuyv => FrameFormat::YUYV,
        PixelFormat::Nv12 => FrameFormat::NV12,
        PixelFormat::Mono8 => FrameFormat::GRAY,
        other => return Err(format!("{} is not a UVC format (use MJPEG, YUYV, NV12 or Mono8)", other.name())),
    })
}

pub struct UvcCamera {
    name: String,
    settings: Option<CameraSettings>,
    running: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl UvcCamera {
    pub fn new(name: &str) -> Self {
        UvcCamera { name: name.into(), settings: None, running: Arc::new(AtomicBool::new(false)), thread: None }
    }
}

impl Camera for UvcCamera {
    fn name(&self) -> String {
        format!("UVC: {}", self.name)
    }

    fn apply(&mut self, s: &CameraSettings) -> Result<(), String> {
        to_fourcc(s.pixel_format)?;
        self.settings = Some(s.clone());
        Ok(())
    }

    /// Formats the backend lists, highest frame rate per format and size.
    /// Sizes below 640 px wide are skipped.
    fn modes(&mut self, _: &CameraSettings) -> Result<Vec<Mode>, String> {
        let mut last = String::new();
        for index in find_indices(&self.name)? {
            let requested = RequestedFormat::new::<RgbFormat>(RequestedFormatType::None);
            // nokhwa's AVFoundation backend lists the fourccs but no sizes or
            // frame rates; then try common sizes at 30 fps and let the
            // recording test show which work.
            let list = NCamera::new(index, requested).and_then(|mut c| {
                let mut all = c.compatible_camera_formats().unwrap_or_default();
                if all.is_empty() {
                    for fourcc in c.compatible_fourcc()? {
                        for (w, h) in [(1920, 1080), (1280, 720), (640, 480)] {
                            all.push(CameraFormat::new(Resolution::new(w, h), fourcc, 30));
                        }
                    }
                }
                Ok(all)
            });
            let list = match list {
                Ok(l) if !l.is_empty() => l,
                Ok(_) => continue,
                Err(e) => {
                    last = e.to_string();
                    continue;
                }
            };
            let mut modes: Vec<Mode> = Vec::new();
            for f in list {
                let Some(format) = from_fourcc(f.format()) else { continue };
                let (width, height, fps) = (f.width(), f.height(), f.frame_rate() as f64);
                if width < 640 {
                    continue;
                }
                match modes.iter_mut().find(|m| m.format == format && m.width == width && m.height == height) {
                    Some(m) => m.fps = m.fps.max(fps),
                    None => modes.push(Mode { format, width, height, fps }),
                }
            }
            modes.sort_by_key(|m| (m.format.name(), std::cmp::Reverse(m.width * m.height)));
            return Ok(modes);
        }
        Err(format!("no format list from \"{}\": {last}", self.name))
    }

    fn start(&mut self, pool: BufferPool, mut sink: FrameSink) -> Result<(), String> {
        self.stop();
        let s = self.settings.clone().ok_or("no settings applied")?;
        let indices = find_indices(&self.name)?;
        let fourcc = to_fourcc(s.pixel_format)?;
        self.running.store(true, Relaxed);
        let running = self.running.clone();
        // The camera is opened on the capture thread (backends are not Send);
        // the open result is reported back so `start` can fail properly.
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
        let thread = std::thread::Builder::new()
            .name("uvc-camera".into())
            .spawn(move || {
                let format = CameraFormat::new(Resolution::new(s.width, s.height), fourcc, s.frame_rate.round() as u32);
                let requested = RequestedFormat::new::<RgbFormat>(RequestedFormatType::Exact(format));
                let mut opened = Err(String::new());
                for index in indices {
                    opened = NCamera::new(index, requested)
                        .and_then(|mut c| c.open_stream().map(|_| c))
                        .map_err(|e| e.to_string());
                    if opened.is_ok() {
                        break;
                    }
                }
                let mut cam = match opened {
                    Ok(c) => c,
                    Err(e) => {
                        let _ = ready_tx.send(Err(format!("open {fourcc} {}x{} @ {}: {e}", s.width, s.height, s.frame_rate)));
                        return;
                    }
                };
                let _ = ready_tx.send(Ok(()));
                let start = Instant::now();
                let mut id = 0u64;
                while running.load(Relaxed) {
                    let buf = match cam.frame() {
                        Ok(b) => b,
                        Err(e) => {
                            eprintln!("uvc frame: {e}");
                            continue;
                        }
                    };
                    let res = buf.resolution();
                    let Some(format) = from_fourcc(buf.source_frame_format()) else { continue };
                    let bytes = buf.buffer();
                    let mut data = pool.get(bytes.len());
                    data.copy_from_slice(bytes);
                    let ts = buf.capture_timestamp().unwrap_or_else(|| start.elapsed());
                    sink(Frame {
                        id,
                        timestamp_ns: ts.as_nanos() as u64,
                        width: res.width(),
                        height: res.height(),
                        format,
                        data,
                    });
                    id += 1;
                }
                let _ = cam.stop_stream();
            })
            .map_err(|e| e.to_string())?;
        self.thread = Some(thread);
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => {
                self.stop();
                Err(e)
            }
            Err(_) => {
                self.stop();
                Err("UVC capture thread failed".into())
            }
        }
    }

    fn stop(&mut self) {
        self.running.store(false, Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for UvcCamera {
    fn drop(&mut self) {
        self.stop();
    }
}

fn from_fourcc(f: FrameFormat) -> Option<PixelFormat> {
    Some(match f {
        FrameFormat::MJPEG => PixelFormat::Mjpeg,
        FrameFormat::YUYV => PixelFormat::Yuyv,
        FrameFormat::NV12 => PixelFormat::Nv12,
        FrameFormat::GRAY => PixelFormat::Mono8,
        _ => return None,
    })
}
