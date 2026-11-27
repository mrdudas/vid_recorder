//! USB3 Vision camera via `cameleon` (libusb + GenICam).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::thread::JoinHandle;

use cameleon::Camera as CCamera;
use cameleon::StreamError;
use cameleon::genapi::{DefaultGenApiCtxt, ParamsCtxt};
use cameleon::payload::PixelFormat as UPixelFormat;
use cameleon::u3v::{self, ControlHandle, StreamHandle};

use crate::camera::{Camera, FrameSink, Geometry, Limits, Mode};
use crate::frame::{self, BufferPool, Frame, PixelFormat};
use crate::scenario::CameraSettings;

type Cam = CCamera<ControlHandle, StreamHandle, DefaultGenApiCtxt>;
type Params<'a> = ParamsCtxt<&'a mut ControlHandle, &'a mut DefaultGenApiCtxt>;

/// Payloads cameleon may hold for us before it has to drop one.
const STREAM_QUEUE: usize = 64;

pub struct U3vCamera {
    cam: Cam,
    name: String,
    running: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    /// Payloads that arrived broken or in an unsupported format.
    stream_errors: Arc<AtomicU64>,
    /// Set by the stream thread when the camera is unplugged.
    lost: Arc<AtomicBool>,
    /// Highest frame rate the camera allowed at the last `apply`.
    max_fps: Option<f64>,
    geometry: Option<Geometry>,
    /// Width, Height and the offsets are writable (hardware crop).
    roi: bool,
}

/// "vendor model (serial)" for every connected USB3 Vision camera.
pub fn list() -> Result<Vec<String>, String> {
    let cams = u3v::enumerate_cameras().map_err(|e| e.to_string())?;
    Ok(cams.iter().map(|c| describe(c.info())).collect())
}

/// GenICam XML of the first camera: every feature name for `extra`.
pub fn genicam_xml() -> Result<String, String> {
    use cameleon::DeviceControl;
    let mut cam = u3v::enumerate_cameras().map_err(|e| e.to_string())?.into_iter().next().ok_or("no USB3 Vision camera found")?;
    cam.open().map_err(|e| format!("open: {e}"))?;
    let xml = cam.ctrl.genapi().map_err(|e| format!("GenICam XML: {e}"));
    let _ = cam.close();
    xml
}

/// Linux limits USB transfer buffers to 16 MB by default, too little for
/// streaming without loss. Raise it to `USBFS_MB` (0 means unlimited):
/// directly if we may write sysfs, else through `pkexec` (asks for the
/// admin password). `linux/70-vid-recorder.rules` does the same at plug-in.
#[cfg(target_os = "linux")]
fn ensure_usbfs_limit() {
    const PATH: &str = "/sys/module/usbcore/parameters/usbfs_memory_mb";
    const USBFS_MB: u32 = 1000;
    let read = || std::fs::read_to_string(PATH).ok().and_then(|s| s.trim().parse::<u32>().ok());
    let Some(mb) = read() else { return };
    if mb == 0 || mb >= USBFS_MB {
        return;
    }
    if std::fs::write(PATH, USBFS_MB.to_string()).is_err() {
        let _ = std::process::Command::new("pkexec")
            .args(["sh", "-c", &format!("echo {USBFS_MB} > {PATH}")])
            .status();
    }
    match read() {
        Some(now) if now == 0 || now >= USBFS_MB => eprintln!("usbfs_memory_mb raised from {mb} to {now}"),
        _ => eprintln!("warning: usbfs_memory_mb is {mb}, frames may be lost; install linux/70-vid-recorder.rules"),
    }
}

#[cfg(not(target_os = "linux"))]
fn ensure_usbfs_limit() {}

/// Why an attached Allied Vision device (USB vendor 1ab2) is not usable:
/// cameleon skips devices it cannot open without saying why.
fn why_not_found() -> String {
    let Ok(list) = rusb::DeviceList::new() else { return String::new() };
    let mut out = String::new();
    for d in list.iter() {
        if d.device_descriptor().is_ok_and(|desc| desc.vendor_id() == 0x1ab2) {
            let state = match d.open() {
                Ok(h) => match h.active_configuration() {
                    Ok(c) => format!("opens, configuration {c}"),
                    Err(e) => format!("configuration: {e}"),
                },
                Err(e) => format!("cannot open: {e}"),
            };
            out += &format!(" (Allied Vision device on bus {} address {}: {state})", d.bus_number(), d.address());
        }
    }
    out
}

fn describe(i: &cameleon::CameraInfo) -> String {
    format!("{} {} ({})", i.vendor_name, i.model_name, i.serial_number)
}

impl U3vCamera {
    /// Open the first USB3 Vision camera and load its GenICam description.
    pub fn open_first() -> Result<U3vCamera, String> {
        let mut cams = u3v::enumerate_cameras().map_err(|e| format!("USB enumeration: {e}"))?;
        if cams.is_empty() {
            return Err(format!("no USB3 Vision camera found{}", why_not_found()));
        }
        ensure_usbfs_limit();
        let mut cam = cams.swap_remove(0);
        cam.open().map_err(|e| format!("open: {e}"))?;
        // Slow writes (e.g. PixelFormat) answer "pending" more than the
        // default 3 times.
        cam.ctrl.set_retry_count(100);
        cam.load_context().map_err(|e| format!("GenICam XML: {e}"))?;
        // A previous process that exited while streaming leaves acquisition
        // running and the parameters locked (writes fail with WriteProtect).
        if let Ok(mut p) = cam.params_ctxt() {
            if let Some(c) = p.node("AcquisitionStop").and_then(|n| n.as_command(&p)) {
                let _ = c.execute(&mut p);
            }
            let _ = set_int(&mut p, "TLParamsLocked", 0);
        }
        let roi = cam.params_ctxt().is_ok_and(|mut p| {
            let p = &mut p;
            ["Width", "Height", "OffsetX", "OffsetY"].iter().all(|name| {
                p.node(name).and_then(|n| n.as_integer(p)).is_some_and(|n| n.is_writable(p).unwrap_or(false))
            })
        });
        if !roi {
            eprintln!("the camera cannot crop (ROI), cropping in software");
        }
        Ok(U3vCamera {
            roi,
            name: describe(cam.info()),
            cam,
            running: Arc::new(AtomicBool::new(false)),
            thread: None,
            stream_errors: Arc::new(AtomicU64::new(0)),
            lost: Arc::new(AtomicBool::new(false)),
            max_fps: None,
            geometry: None,
        })
    }
}

impl Camera for U3vCamera {
    fn name(&self) -> String {
        self.name.clone()
    }

    fn apply(&mut self, s: &CameraSettings) -> Result<(), String> {
        self.max_fps = None;
        let mut p = self.cam.params_ctxt().map_err(|e| e.to_string())?;
        // Chunk data may be left on (e.g. by Vimba); we don't use it, and
        // cameleon fails to parse the Alvium's chunk payloads.
        let _ = set_bool(&mut p, "ChunkModeActive", false);
        set_enum(&mut p, "PixelFormat", s.pixel_format.name())?;
        set_bool(&mut p, "ReverseX", s.reverse_x)?;
        set_bool(&mut p, "ReverseY", s.reverse_y)?;
        let (width, height) = if self.roi {
            // Offsets first to 0 so any width/height fits, then the real offsets.
            set_int(&mut p, "OffsetX", 0)?;
            set_int(&mut p, "OffsetY", 0)?;
            self.geometry = geometry(&mut p).ok();
            set_int(&mut p, "Width", s.width as i64)?;
            set_int(&mut p, "Height", s.height as i64)?;
            set_int(&mut p, "OffsetX", s.offset_x as i64)?;
            set_int(&mut p, "OffsetY", s.offset_y as i64)?;
            (s.width, s.height)
        } else {
            // Full image; the engine crops in software.
            let size = |p: &mut Params, name| p.node(name).and_then(|n| n.as_integer(p)).and_then(|n| n.value(p).ok()).unwrap_or(0) as u32;
            let full = (size(&mut p, "Width"), size(&mut p, "Height"));
            self.geometry = Some(Geometry { sensor: full, min_size: (16, 16), step: [2; 4] });
            full
        };
        // Extra features before the frame rate checks: they may change the
        // limits (binning, DeviceLinkThroughputLimit...).
        for (name, value) in &s.extra {
            set_json(&mut p, name, value)?;
        }
        // The camera accepts any frame rate but silently runs slower (or
        // drops frames) when the sensor or the USB link cannot keep up:
        // refuse those settings.
        if let Some((max_fps, link)) = link_fps(&mut p, s.pixel_format, width, height)
            && s.frame_rate > max_fps
        {
            return Err(format!(
                "{:.0} fps needs {:.0} MB/s, the USB link carries {:.0} MB/s: at most {:.1} fps with {}x{} {}",
                s.frame_rate,
                s.frame_rate * link / max_fps / 1e6,
                link / 1e6,
                max_fps,
                width,
                height,
                s.pixel_format.name()
            ));
        }
        // Exposure first, with the frame rate unconstrained (the exposure
        // range depends on the current frame rate and vice versa). With auto
        // exposure the frame rate must hold at the shortest exposure it may
        // use; the longest is capped to the frame period below.
        set_exposure(&mut p, s)?;
        // The maximum now reflects format, size and exposure.
        let _ = set_bool(&mut p, "AcquisitionFrameRateEnable", true);
        let max = float_max(&mut p, "AcquisitionFrameRate").ok();
        self.max_fps = max;
        if let Some(max) = max.filter(|&m| s.frame_rate > m + 1e-3) {
            return Err(format!(
                "the camera does at most {max:.1} fps with {}x{} {} and {}",
                s.width,
                s.height,
                s.pixel_format.name(),
                if s.auto_exposure {
                    format!("auto exposure from {} µs", s.auto_exposure_range.0)
                } else {
                    format!("{} µs exposure", s.exposure_us)
                }
            ));
        }
        set_float(&mut p, "AcquisitionFrameRate", s.frame_rate)?;
        if s.auto_exposure {
            // Keep the exposure within one frame so the frame rate holds.
            let period = (1e6 / s.frame_rate * 0.95).max(1.0);
            let (lo, hi) = s.auto_exposure_range;
            set_range(&mut p, "ExposureAutoMin", "ExposureAutoMax", lo.min(period), hi.min(period));
            set_range(&mut p, "GainAutoMin", "GainAutoMax", s.auto_gain_range.0, s.auto_gain_range.1);
            // Only if the camera has it: reading a missing feature locks the
            // Alvium. Its auto aims at a mean brightness; "FitRange" (no
            // burn-out) is not available on the U-291c (firmware, 2026-11).
            // Longest exposure first, gain only when that is not enough.
            if p.node("IntensityAutoPrecedence").is_some() {
                if let Err(e) = set_enum(&mut p, "IntensityAutoPrecedence", "MinimizeNoise") {
                    eprintln!("warning: {e}");
                }
            }
            if p.node("IntensityControllerTarget").is_some() {
                if let Err(e) = set_float_clamped(&mut p, "IntensityControllerTarget", s.auto_target) {
                    eprintln!("warning: {e}");
                }
            }
            set_enum(&mut p, "ExposureAuto", "Continuous")?;
            let _ = set_enum(&mut p, "GainAuto", "Continuous");
        }
        Ok(())
    }

    fn max_fps(&self) -> Option<f64> {
        self.max_fps
    }

    fn geometry(&self) -> Option<Geometry> {
        self.geometry
    }

    /// Only register reads and cheap writes, no streaming: the whole table
    /// takes about a second.
    fn limits(&mut self, s: &CameraSettings) -> Result<Limits, String> {
        let mut p = self.cam.params_ctxt().map_err(|e| e.to_string())?;
        let p = &mut p;
        let _ = set_bool(p, "ChunkModeActive", false);
        let _ = set_bool(p, "ReverseX", s.reverse_x);
        let _ = set_bool(p, "ReverseY", s.reverse_y);
        for (name, value) in &s.extra {
            let _ = set_json(p, name, value);
        }
        set_exposure(p, s)?;
        let float_range = |p: &mut Params, name| -> (f64, f64) {
            p.node(name)
                .and_then(|n| n.as_float(p))
                .and_then(|n| Some((n.min(p).ok()?, n.max(p).ok()?)))
                .unwrap_or((0.0, 0.0))
        };
        let exposure_us = float_range(p, "ExposureTime");
        let gain_db = float_range(p, "Gain");
        let _ = set_bool(p, "AcquisitionFrameRateEnable", true);

        let node = p.node("PixelFormat").and_then(|n| n.as_enumeration(p)).ok_or_else(|| missing("PixelFormat", "enumeration"))?;
        let mut formats = Vec::new();
        for e in node.entries(p) {
            if e.is_available(p).unwrap_or(false)
                && let Some(f) = PixelFormat::ALL.into_iter().find(|f| f.name() == e.symbolic(p))
            {
                formats.push(f);
            }
        }
        // Switching between 12-bit and other formats takes the Alvium
        // ~1.5 s (sensor mode change), the rest a few ms: scenario's group first.
        let group = |f: PixelFormat| (f.bits() == 12) != (s.pixel_format.bits() == 12);
        formats.sort_by_key(|&f| group(f));
        // Geometry of the scenario's format (others are the same on the
        // Alvium, but the table re-reads the maximum per format).
        set_enum(p, "PixelFormat", s.pixel_format.name())?;
        // Mirroring takes a column / row pair off the usable area.
        set_bool(p, "ReverseX", s.reverse_x)?;
        set_bool(p, "ReverseY", s.reverse_y)?;
        set_int(p, "OffsetX", 0)?;
        set_int(p, "OffsetY", 0)?;
        let Geometry { sensor, min_size, step } = geometry(p)?;

        let fps_at = |p: &mut Params, f: PixelFormat, w: u32, h: u32| -> Option<f64> {
            set_int(p, "Width", w as i64).ok()?;
            set_int(p, "Height", h as i64).ok()?;
            let cam = float_max(p, "AcquisitionFrameRate").ok()?;
            let link = link_fps(p, f, w, h).map_or(f64::MAX, |l| l.0);
            Some((cam.min(link) * 10.0).floor() / 10.0)
        };
        let max_fps = (|| {
            set_int(p, "Width", s.width as i64).ok()?;
            set_int(p, "Height", s.height as i64).ok()?;
            set_int(p, "OffsetX", s.offset_x as i64).ok()?;
            set_int(p, "OffsetY", s.offset_y as i64).ok()?;
            fps_at(p, s.pixel_format, s.width, s.height)
        })();

        let snap = |v: u32, step: u32, min: u32, max: u32| (v / step * step).clamp(min, max);
        let mut heights: Vec<u32> = [s.height, sensor.1, 1080, 720, 480, 240, 120]
            .into_iter()
            .filter(|&h| h <= sensor.1)
            .map(|h| snap(h, step[1], min_size.1, sensor.1))
            .collect();
        heights.sort_unstable_by(|a, b| b.cmp(a));
        heights.dedup();
        let mut table = Vec::new();
        for f in formats {
            if set_enum(p, "PixelFormat", f.name()).is_err() {
                continue;
            }
            let _ = set_int(p, "OffsetX", 0);
            let _ = set_int(p, "OffsetY", 0);
            let Ok(wmax) = int_max(p, "Width") else { continue };
            let w = snap(s.width, step[0], min_size.0, wmax as u32);
            table.push((f, heights.iter().map(|&h| fps_at(p, f, w, h)).collect()));
        }
        Ok(Limits { sensor, min_size, step, exposure_us, gain_db, max_fps, heights, table })
    }

    /// Every available PixelFormat we can record, at full sensor size and
    /// at 1920x1080 if smaller, each with the camera's maximum frame rate.
    fn modes(&mut self, s: &CameraSettings) -> Result<Vec<Mode>, String> {
        let mut p = self.cam.params_ctxt().map_err(|e| e.to_string())?;
        let p = &mut p;
        // Exposure and e.g. DeviceLinkThroughputLimit bound the frame rate.
        let _ = set_enum(p, "ExposureAuto", "Off");
        let _ = set_float(p, "ExposureTime", s.exposure_us);
        for (name, value) in &s.extra {
            let _ = set_json(p, name, value);
        }
        let node = p.node("PixelFormat").and_then(|n| n.as_enumeration(p)).ok_or_else(|| missing("PixelFormat", "enumeration"))?;
        let mut names = Vec::new();
        for e in node.entries(p) {
            if e.is_available(p).unwrap_or(false) {
                names.push(e.symbolic(p).to_string());
            }
        }
        let mut modes = Vec::new();
        for name in names {
            let Some(format) = PixelFormat::ALL.into_iter().find(|f| f.name() == name) else { continue };
            if set_enum(p, "PixelFormat", &name).is_err() {
                continue;
            }
            let _ = set_int(p, "OffsetX", 0);
            let _ = set_int(p, "OffsetY", 0);
            let (Ok(w), Ok(h)) = (int_max(p, "Width"), int_max(p, "Height")) else { continue };
            let mut sizes = vec![(w, h)];
            if self.roi && w >= 1920 && h >= 1080 && (w, h) != (1920, 1080) {
                sizes.push((1920, 1080));
            }
            for (w, h) in sizes {
                if self.roi && (set_int(p, "Width", w).is_err() || set_int(p, "Height", h).is_err()) {
                    continue;
                }
                let _ = set_bool(p, "AcquisitionFrameRateEnable", true);
                let Ok(fps) = float_max(p, "AcquisitionFrameRate") else { continue };
                // Rounded down: the exact maximum may not be settable.
                modes.push(Mode {
                    format,
                    width: w as u32,
                    height: h as u32,
                    fps: (fps * 100.0).floor() / 100.0,
                    reported: true,
                    rates: Vec::new(),
                });
            }
        }
        Ok(modes)
    }

    fn start(&mut self, pool: BufferPool, mut sink: FrameSink) -> Result<(), String> {
        self.stop();
        let rx = self.cam.start_streaming(STREAM_QUEUE).map_err(|e| format!("start streaming: {e}"))?;
        self.running.store(true, Relaxed);
        let running = self.running.clone();
        let errors = self.stream_errors.clone();
        let lost = self.lost.clone();
        let thread = std::thread::Builder::new()
            .name("u3v-camera".into())
            .spawn(move || {
                // The Alvium often discards the payload that is in flight
                // when streaming starts (DataDiscarded): nothing has been
                // delivered yet, so no frame of the stream is missing.
                let mut started = false;
                while running.load(Relaxed) {
                    // Returns an error once streaming is stopped.
                    let payload = match rx.recv_blocking() {
                        Ok(p) => p,
                        Err(_) if !running.load(Relaxed) => break,
                        Err(StreamError::Disconnected | StreamError::Io(_)) => {
                            eprintln!("stream: camera disconnected");
                            lost.store(true, Relaxed);
                            break;
                        }
                        Err(e) if !started => {
                            eprintln!("stream: before the first frame, ignored: {e}");
                            continue;
                        }
                        Err(e) => {
                            errors.fetch_add(1, Relaxed);
                            eprintln!("stream: {e}");
                            continue;
                        }
                    };
                    // Stopped (or given up on) while waiting: pass nothing on.
                    if !running.load(Relaxed) {
                        break;
                    }
                    let frame = payload.image_info().zip(payload.image()).and_then(|(info, image)| {
                        let format = map_format(&info.pixel_format)?;
                        let data = if format.is_packed() {
                            // Unpack to 2 bytes per pixel (bit-exact) right here,
                            // so everything downstream sees plain 16-bit data.
                            let mut data = pool.get(info.width * info.height * 2);
                            frame::unpack(format, image, &mut data);
                            data
                        } else {
                            let mut data = pool.get(image.len());
                            data.copy_from_slice(image);
                            data
                        };
                        Some(Frame {
                            id: payload.id(),
                            timestamp_ns: payload.timestamp().as_nanos() as u64,
                            width: info.width as u32,
                            height: info.height as u32,
                            format,
                            data,
                        })
                    });
                    rx.send_back(payload);
                    match frame {
                        Some(f) => {
                            started = true;
                            sink(f)
                        }
                        None => {
                            errors.fetch_add(1, Relaxed);
                        }
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        self.thread = Some(thread);
        Ok(())
    }

    fn hw_crop(&self) -> bool {
        self.roi
    }

    fn errors(&self) -> u64 {
        self.stream_errors.load(Relaxed)
    }

    fn disconnected(&self) -> bool {
        self.lost.load(Relaxed)
    }

    fn reset(&mut self) {
        self.stop();
        let Ok(mut p) = self.cam.params_ctxt() else { return };
        match p.node("DeviceReset").and_then(|n| n.as_command(&p)) {
            Some(c) => match c.execute(&mut p) {
                Ok(()) => eprintln!("camera reset sent"),
                Err(e) => eprintln!("camera reset: {e}"),
            },
            None => eprintln!("camera has no DeviceReset"),
        }
    }

    fn stop(&mut self) {
        if !self.running.swap(false, Relaxed) {
            return;
        }
        // Nothing to tell a device that is gone.
        if !self.disconnected()
            && let Err(e) = self.cam.stop_streaming()
        {
            eprintln!("stop streaming: {e}");
        }
        // A thread stuck on a hung device must not hang us: wait at most
        // 2 s, then leave it (it passes nothing on once it wakes up).
        if let Some(t) = self.thread.take() {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while !t.is_finished() && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            if t.is_finished() {
                let _ = t.join();
            } else {
                eprintln!("warning: camera stream thread did not stop, left behind");
            }
        }
    }
}

impl Drop for U3vCamera {
    fn drop(&mut self) {
        self.stop();
        let _ = self.cam.close();
    }
}

fn map_format(f: &UPixelFormat) -> Option<PixelFormat> {
    Some(match f {
        UPixelFormat::Mono8 => PixelFormat::Mono8,
        UPixelFormat::Mono10 => PixelFormat::Mono10,
        UPixelFormat::Mono12 => PixelFormat::Mono12,
        UPixelFormat::Mono16 => PixelFormat::Mono16,
        UPixelFormat::BayerRG8 => PixelFormat::BayerRG8,
        UPixelFormat::BayerRG10 => PixelFormat::BayerRG10,
        UPixelFormat::BayerRG12 => PixelFormat::BayerRG12,
        UPixelFormat::BayerRG10p => PixelFormat::BayerRG10p,
        UPixelFormat::BayerRG12p => PixelFormat::BayerRG12p,
        UPixelFormat::YCbCr422_8_CbYCrY => PixelFormat::Uyvy,
        _ => return None,
    })
}

/// Exposure and gain for `apply` and `limits`, with the frame rate
/// unconstrained. With auto exposure: the shortest exposure the auto may
/// use (the frame rate must hold there); the auto is switched on later.
fn set_exposure(p: &mut Params, s: &CameraSettings) -> Result<(), String> {
    let _ = set_bool(p, "AcquisitionFrameRateEnable", false);
    // The auto features may not exist on every model.
    let _ = set_enum(p, "ExposureAuto", "Off");
    let _ = set_enum(p, "GainAuto", "Off");
    if s.auto_exposure {
        set_float_clamped(p, "ExposureTime", s.auto_exposure_range.0)?;
    } else {
        set_float(p, "ExposureTime", s.exposure_us)?;
        set_float(p, "Gain", s.gain_db)?;
    }
    Ok(())
}

/// Frame rate the USB link limit allows, and the limit [bytes/s]; `None`
/// if the camera does not limit it.
fn link_fps(p: &mut Params, f: PixelFormat, width: u32, height: u32) -> Option<(f64, f64)> {
    let mode = p.node("DeviceLinkThroughputLimitMode").and_then(|n| n.as_enumeration(p));
    if mode.and_then(|n| n.current_entry(p).ok()).is_some_and(|e| e.symbolic(p) == "Off") {
        return None;
    }
    let limit = p.node("DeviceLinkThroughputLimit").and_then(|n| n.as_integer(p)).and_then(|n| n.value(p).ok())? as f64;
    let wire_bits = if f.is_packed() { f.bits() as f64 } else { 8.0 * f.bytes_per_pixel() as f64 };
    Some((limit / (width as f64 * height as f64 * wire_bits / 8.0), limit))
}

/// Set a Min/Max feature pair, clamped to what the camera allows. Min goes
/// to its lowest first so any new max fits. Failures are only reported.
fn set_range(p: &mut Params, min_name: &str, max_name: &str, lo: f64, hi: f64) {
    let lowest = p.node(min_name).and_then(|n| n.as_float(p)).and_then(|n| n.min(p).ok());
    if let Some(l) = lowest {
        let _ = set_float(p, min_name, l);
    }
    for (name, v) in [(max_name, hi.max(lo)), (min_name, lo)] {
        if let Err(e) = set_float_clamped(p, name, v) {
            eprintln!("warning: {e}");
        }
    }
}

fn set_float_clamped(p: &mut Params, name: &str, v: f64) -> Result<(), String> {
    let node = p.node(name).and_then(|n| n.as_float(p)).ok_or_else(|| missing(name, "float"))?;
    let (lo, hi) = (node.min(p).unwrap_or(f64::MIN), node.max(p).unwrap_or(f64::MAX));
    let v = v.clamp(lo, hi.max(lo));
    node.set_value(p, v).map_err(|e| format!("{name} = {v}: {e}"))
}

fn missing(name: &str, kind: &str) -> String {
    format!("camera feature {name} not found (or not a {kind})")
}

fn set_int(p: &mut Params, name: &str, v: i64) -> Result<(), String> {
    let node = p.node(name).and_then(|n| n.as_integer(p)).ok_or_else(|| missing(name, "integer"))?;
    // Check the range first: after a write it rejects, the Alvium refuses
    // every further write until it is reopened.
    if let (Ok(min), Ok(max)) = (node.min(p), node.max(p)) {
        let inc = node.inc(p).ok().flatten().unwrap_or(1).max(1);
        if v < min || v > max || (v - min) % inc != 0 {
            return Err(format!("{name} = {v}: a kamera {min}..{max} értéket fogad el, {inc} lépésközzel"));
        }
    }
    node.set_value(p, v).map_err(|e| format!("{name} = {v}: {e}"))
}

/// Sensor size, smallest size and steps; the offsets must be 0.
fn geometry(p: &mut Params) -> Result<Geometry, String> {
    let inc = |p: &mut Params, name| {
        p.node(name).and_then(|n| n.as_integer(p)).and_then(|n| n.inc(p).ok().flatten()).unwrap_or(1).max(1) as u32
    };
    let int_min = |p: &mut Params, name| p.node(name).and_then(|n| n.as_integer(p)).and_then(|n| n.min(p).ok()).unwrap_or(1) as u32;
    Ok(Geometry {
        sensor: (int_max(p, "Width")? as u32, int_max(p, "Height")? as u32),
        min_size: (int_min(p, "Width"), int_min(p, "Height")),
        step: [inc(p, "Width"), inc(p, "Height"), inc(p, "OffsetX"), inc(p, "OffsetY")],
    })
}

fn int_max(p: &mut Params, name: &str) -> Result<i64, String> {
    let node = p.node(name).and_then(|n| n.as_integer(p)).ok_or_else(|| missing(name, "integer"))?;
    node.max(p).map_err(|e| format!("{name} max: {e}"))
}

fn float_max(p: &mut Params, name: &str) -> Result<f64, String> {
    let node = p.node(name).and_then(|n| n.as_float(p)).ok_or_else(|| missing(name, "float"))?;
    node.max(p).map_err(|e| format!("{name} max: {e}"))
}

fn set_float(p: &mut Params, name: &str, v: f64) -> Result<(), String> {
    let node = p.node(name).and_then(|n| n.as_float(p)).ok_or_else(|| missing(name, "float"))?;
    node.set_value(p, v).map_err(|e| format!("{name} = {v}: {e}"))
}

fn set_bool(p: &mut Params, name: &str, v: bool) -> Result<(), String> {
    let node = p.node(name).and_then(|n| n.as_boolean(p)).ok_or_else(|| missing(name, "boolean"))?;
    node.set_value(p, v).map_err(|e| format!("{name} = {v}: {e}"))
}

fn set_enum(p: &mut Params, name: &str, v: &str) -> Result<(), String> {
    let node = p.node(name).and_then(|n| n.as_enumeration(p)).ok_or_else(|| missing(name, "enumeration"))?;
    node.set_entry_by_symbolic(p, v).map_err(|e| format!("{name} = {v}: {e}"))
}

/// Set any feature from a scenario's `extra` map by the JSON value's type.
fn set_json(p: &mut Params, name: &str, v: &serde_json::Value) -> Result<(), String> {
    use serde_json::Value;
    let node = p.node(name).ok_or_else(|| format!("camera feature {name} not found"))?;
    match v {
        Value::Bool(b) => set_bool(p, name, *b),
        Value::String(s) => {
            if node.as_enumeration(p).is_some() {
                set_enum(p, name, s)
            } else if let Some(n) = node.as_string(p) {
                n.set_value(p, s.clone()).map_err(|e| format!("{name} = {s}: {e}"))
            } else {
                Err(missing(name, "enumeration or string"))
            }
        }
        Value::Number(n) => {
            if node.as_integer(p).is_some() {
                let i = n.as_i64().ok_or_else(|| format!("{name}: integer expected"))?;
                set_int(p, name, i)
            } else {
                set_float(p, name, n.as_f64().unwrap_or_default())
            }
        }
        _ => Err(format!("{name}: unsupported value {v}")),
    }
}
