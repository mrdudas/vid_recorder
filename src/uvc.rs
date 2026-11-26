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

use crate::camera::{Camera, FrameSink, Geometry, Mode};
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

/// Sizes and frame rates tried when the backend does not list the modes.
const TRY_SIZES: [(u32, u32); 7] = [(3840, 2160), (2560, 1440), (1920, 1080), (1280, 720), (1024, 768), (800, 600), (640, 480)];
const TRY_RATES: [f64; 3] = [60.0, 30.0, 15.0];

/// Open the camera with this format and size at 30 fps and wait (up to
/// 3 s) for one frame: what it really delivers (format, width, height).
fn try_open(index: CameraIndex, fourcc: FrameFormat, w: u32, h: u32) -> Option<(PixelFormat, u32, u32)> {
    let (tx, rx) = mpsc::channel();
    // On its own thread: a hung backend must not hang the caller.
    std::thread::spawn(move || {
        let format = CameraFormat::new(Resolution::new(w, h), fourcc, 30);
        let got = NCamera::new(index, RequestedFormat::new::<RgbFormat>(RequestedFormatType::Exact(format))).ok().and_then(|mut c| {
            c.open_stream().ok()?;
            let f = c.frame().ok();
            let _ = c.stop_stream();
            let f = f?;
            Some((from_fourcc(f.source_frame_format())?, f.resolution().width(), f.resolution().height()))
        });
        let _ = tx.send(got);
    });
    rx.recv_timeout(Duration::from_secs(3)).ok().flatten()
}

/// Size the camera is opened with (the engine crops from it).
fn capture(s: &CameraSettings) -> (u32, u32) {
    s.capture_size.unwrap_or((s.width, s.height))
}

pub struct UvcCamera {
    name: String,
    settings: Option<CameraSettings>,
    running: Arc<AtomicBool>,
    /// Not found, or frames fail continuously (unplugged).
    lost: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl UvcCamera {
    pub fn new(name: &str) -> Self {
        UvcCamera { name: name.into(), settings: None, running: Arc::new(AtomicBool::new(false)), lost: Arc::new(AtomicBool::new(false)), thread: None }
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

    /// The modes the backend lists: per format and size the highest frame
    /// rate, with all listed rates. If it lists none (nokhwa on macOS gives
    /// only the formats), every format × common size × common rate is
    /// returned as a guess to try (`reported: false`).
    fn modes(&mut self, _: &CameraSettings) -> Result<Vec<Mode>, String> {
        let mut last = String::new();
        for index in find_indices(&self.name)? {
            let requested = RequestedFormat::new::<RgbFormat>(RequestedFormatType::None);
            let listed = NCamera::new(index.clone(), requested).and_then(|mut c| {
                let all = c.compatible_camera_formats().unwrap_or_default();
                let fourccs = c.compatible_fourcc().unwrap_or_default();
                Ok((all, fourccs))
            });
            let (list, fourccs) = match listed {
                Ok(l) => l,
                Err(e) => {
                    last = e.to_string();
                    continue;
                }
            };
            let mut modes: Vec<Mode> = Vec::new();
            for f in list {
                let Some(format) = from_fourcc(f.format()) else { continue };
                let (width, height, fps) = (f.width(), f.height(), f.frame_rate() as f64);
                match modes.iter_mut().find(|m| m.format == format && m.width == width && m.height == height) {
                    Some(m) => {
                        m.fps = m.fps.max(fps);
                        m.rates.push(fps);
                    }
                    None => modes.push(Mode { format, width, height, fps, reported: true, rates: vec![fps] }),
                }
            }
            if modes.is_empty() {
                // Feel out what it accepts: open every format × size and
                // keep what really arrives (format and size of a frame).
                let mut formats: Vec<FrameFormat> = fourccs.into_iter().filter(|f| from_fourcc(*f).is_some()).collect();
                if formats.is_empty() {
                    formats = vec![FrameFormat::MJPEG, FrameFormat::YUYV, FrameFormat::NV12, FrameFormat::GRAY];
                }
                for fourcc in formats {
                    for (w, h) in TRY_SIZES {
                        let Some((format, width, height)) = try_open(index.clone(), fourcc, w, h) else { continue };
                        if (width, height) != (w, h) || modes.iter().any(|m| (m.format, m.width, m.height) == (format, width, height)) {
                            continue;
                        }
                        // 30 fps worked when opened; the discovery tries the others.
                        modes.push(Mode { format, width, height, fps: 30.0, reported: false, rates: TRY_RATES.to_vec() });
                    }
                }
            }
            for m in &mut modes {
                m.rates.sort_by(|a, b| b.total_cmp(a));
                m.rates.dedup();
            }
            modes.sort_by_key(|m| (m.format.name(), std::cmp::Reverse(m.width * m.height), std::cmp::Reverse((m.fps * 100.0) as u64)));
            return Ok(modes);
        }
        Err(format!("\"{}\" nem kérdezhető le: {last}", self.name))
    }

    /// The image is cut in software from the capture size.
    fn hw_crop(&self) -> bool {
        false
    }

    fn geometry(&self) -> Option<Geometry> {
        let s = self.settings.as_ref()?;
        Some(Geometry { sensor: capture(s), min_size: (16, 16), step: [2; 4] })
    }

    fn start(&mut self, pool: BufferPool, mut sink: FrameSink) -> Result<(), String> {
        self.stop();
        let s = self.settings.clone().ok_or("no settings applied")?;
        let indices = find_indices(&self.name).inspect_err(|_| self.lost.store(true, Relaxed))?;
        self.lost.store(false, Relaxed);
        let lost = self.lost.clone();
        let fourcc = to_fourcc(s.pixel_format)?;
        self.running.store(true, Relaxed);
        let running = self.running.clone();
        // The camera is opened on the capture thread (backends are not Send);
        // the open result is reported back so `start` can fail properly.
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
        let thread = std::thread::Builder::new()
            .name("uvc-camera".into())
            .spawn(move || {
                let (cw, ch) = capture(&s);
                let format = CameraFormat::new(Resolution::new(cw, ch), fourcc, s.frame_rate.round() as u32);
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
                        let _ = ready_tx.send(Err(format!("open {fourcc} {cw}x{ch} @ {}: {e}", s.frame_rate)));
                        return;
                    }
                };
                let _ = ready_tx.send(Ok(()));
                let start = Instant::now();
                let mut id = 0u64;
                let mut failing: Option<Instant> = None;
                while running.load(Relaxed) {
                    let buf = match cam.frame() {
                        Ok(b) => b,
                        Err(e) => {
                            // Failing for a second: unplugged.
                            let since = *failing.get_or_insert_with(Instant::now);
                            if since.elapsed().as_secs_f64() > 1.0 {
                                eprintln!("uvc: camera lost ({e})");
                                lost.store(true, Relaxed);
                                break;
                            }
                            std::thread::sleep(std::time::Duration::from_millis(10));
                            continue;
                        }
                    };
                    failing = None;
                    if !running.load(Relaxed) {
                        break;
                    }
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
        // Wait at most 2 s for a thread stuck on a gone device.
        if let Some(t) = self.thread.take() {
            let deadline = Instant::now() + std::time::Duration::from_secs(2);
            while !t.is_finished() && Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            if t.is_finished() {
                let _ = t.join();
            } else {
                eprintln!("warning: uvc capture thread did not stop, left behind");
            }
        }
    }

    fn disconnected(&self) -> bool {
        self.lost.load(Relaxed)
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
