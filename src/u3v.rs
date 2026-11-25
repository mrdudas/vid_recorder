//! USB3 Vision camera via `cameleon` (libusb + GenICam).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::thread::JoinHandle;

use cameleon::Camera as CCamera;
use cameleon::StreamError;
use cameleon::genapi::{DefaultGenApiCtxt, ParamsCtxt};
use cameleon::payload::PixelFormat as UPixelFormat;
use cameleon::u3v::{self, ControlHandle, StreamHandle};

use crate::camera::{Camera, FrameSink, Mode};
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

fn describe(i: &cameleon::CameraInfo) -> String {
    format!("{} {} ({})", i.vendor_name, i.model_name, i.serial_number)
}

impl U3vCamera {
    /// Open the first USB3 Vision camera and load its GenICam description.
    pub fn open_first() -> Result<U3vCamera, String> {
        let mut cams = u3v::enumerate_cameras().map_err(|e| format!("USB enumeration: {e}"))?;
        if cams.is_empty() {
            return Err("no USB3 Vision camera found".into());
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
        Ok(U3vCamera {
            name: describe(cam.info()),
            cam,
            running: Arc::new(AtomicBool::new(false)),
            thread: None,
            stream_errors: Arc::new(AtomicU64::new(0)),
            lost: Arc::new(AtomicBool::new(false)),
        })
    }
}

impl Camera for U3vCamera {
    fn name(&self) -> String {
        self.name.clone()
    }

    fn apply(&mut self, s: &CameraSettings) -> Result<(), String> {
        let mut p = self.cam.params_ctxt().map_err(|e| e.to_string())?;
        // Chunk data may be left on (e.g. by Vimba); we don't use it, and
        // cameleon fails to parse the Alvium's chunk payloads.
        let _ = set_bool(&mut p, "ChunkModeActive", false);
        set_enum(&mut p, "PixelFormat", s.pixel_format.name())?;
        set_bool(&mut p, "ReverseX", s.reverse_x)?;
        set_bool(&mut p, "ReverseY", s.reverse_y)?;
        // Offsets first to 0 so any width/height fits, then the real offsets.
        set_int(&mut p, "OffsetX", 0)?;
        set_int(&mut p, "OffsetY", 0)?;
        set_int(&mut p, "Width", s.width as i64)?;
        set_int(&mut p, "Height", s.height as i64)?;
        set_int(&mut p, "OffsetX", s.offset_x as i64)?;
        set_int(&mut p, "OffsetY", s.offset_y as i64)?;
        // Extra features before the frame rate checks: they may change the
        // limits (binning, DeviceLinkThroughputLimit...).
        for (name, value) in &s.extra {
            set_json(&mut p, name, value)?;
        }
        // The camera accepts any frame rate but silently runs slower (or
        // drops frames) when the sensor or the USB link cannot keep up:
        // refuse those settings.
        let limit_on = p
            .node("DeviceLinkThroughputLimitMode")
            .and_then(|n| n.as_enumeration(&p))
            .and_then(|n| n.current_entry(&mut p).ok())
            .is_none_or(|e| e.symbolic(&p) != "Off");
        let limit = limit_on
            .then(|| p.node("DeviceLinkThroughputLimit").and_then(|n| n.as_integer(&p)).and_then(|n| n.value(&mut p).ok()))
            .flatten();
        if let Some(limit) = limit {
            let wire_bits = if s.pixel_format.is_packed() { s.pixel_format.bits() as f64 } else { 8.0 * s.pixel_format.bytes_per_pixel() as f64 };
            let frame_bytes = s.width as f64 * s.height as f64 * wire_bits / 8.0;
            let usable = limit as f64;
            let max_fps = usable / frame_bytes;
            if s.frame_rate > max_fps {
                return Err(format!(
                    "{:.0} fps needs {:.0} MB/s, the USB link carries {:.0} MB/s: at most {:.1} fps with {}x{} {}",
                    s.frame_rate,
                    s.frame_rate * frame_bytes / 1e6,
                    usable / 1e6,
                    max_fps,
                    s.width,
                    s.height,
                    s.pixel_format.name()
                ));
            }
        }
        // Exposure first, with the frame rate unconstrained (the exposure
        // range depends on the current frame rate and vice versa).
        let _ = set_bool(&mut p, "AcquisitionFrameRateEnable", false);
        if s.auto_exposure {
            // Keep the exposure within one frame so the frame rate holds.
            if let Err(e) = set_float(&mut p, "ExposureAutoMax", (1e6 / s.frame_rate * 0.95).max(100.0)) {
                eprintln!("warning: {e}");
            }
            set_enum(&mut p, "ExposureAuto", "Continuous")?;
            let _ = set_enum(&mut p, "GainAuto", "Continuous");
        } else {
            // Manual exposure/gain; the auto features may not exist on every model.
            let _ = set_enum(&mut p, "ExposureAuto", "Off");
            let _ = set_enum(&mut p, "GainAuto", "Off");
            set_float(&mut p, "ExposureTime", s.exposure_us)?;
            set_float(&mut p, "Gain", s.gain_db)?;
        }
        // The maximum now reflects format, size and exposure.
        let _ = set_bool(&mut p, "AcquisitionFrameRateEnable", true);
        let max = p.node("AcquisitionFrameRate").and_then(|n| n.as_float(&p)).and_then(|n| n.max(&mut p).ok());
        if let Some(max) = max.filter(|&m| s.frame_rate > m + 1e-3) {
            return Err(format!(
                "the camera does at most {max:.1} fps with {}x{} {} and {}",
                s.width,
                s.height,
                s.pixel_format.name(),
                if s.auto_exposure { "auto exposure".to_string() } else { format!("{} µs exposure", s.exposure_us) }
            ));
        }
        set_float(&mut p, "AcquisitionFrameRate", s.frame_rate)?;
        Ok(())
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
            if w >= 1920 && h >= 1080 && (w, h) != (1920, 1080) {
                sizes.push((1920, 1080));
            }
            for (w, h) in sizes {
                if set_int(p, "Width", w).is_err() || set_int(p, "Height", h).is_err() {
                    continue;
                }
                let _ = set_bool(p, "AcquisitionFrameRateEnable", true);
                let Ok(fps) = float_max(p, "AcquisitionFrameRate") else { continue };
                // Rounded down: the exact maximum may not be settable.
                modes.push(Mode { format, width: w as u32, height: h as u32, fps: (fps * 100.0).floor() / 100.0 });
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
                        Err(e) => {
                            errors.fetch_add(1, Relaxed);
                            eprintln!("stream: {e}");
                            continue;
                        }
                    };
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
                        Some(f) => sink(f),
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

    fn errors(&self) -> u64 {
        self.stream_errors.load(Relaxed)
    }

    fn disconnected(&self) -> bool {
        self.lost.load(Relaxed)
    }

    fn stop(&mut self) {
        if !self.running.swap(false, Relaxed) {
            return;
        }
        if self.disconnected() {
            // Nothing to tell a device that is gone; just end the thread.
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
            return;
        }
        if let Err(e) = self.cam.stop_streaming() {
            eprintln!("stop streaming: {e}");
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
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

fn missing(name: &str, kind: &str) -> String {
    format!("camera feature {name} not found (or not a {kind})")
}

fn set_int(p: &mut Params, name: &str, v: i64) -> Result<(), String> {
    let node = p.node(name).and_then(|n| n.as_integer(p)).ok_or_else(|| missing(name, "integer"))?;
    node.set_value(p, v).map_err(|e| format!("{name} = {v}: {e}"))
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
