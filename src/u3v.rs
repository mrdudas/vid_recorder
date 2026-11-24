//! USB3 Vision camera via `cameleon` (libusb + GenICam).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::thread::JoinHandle;

use cameleon::Camera as CCamera;
use cameleon::genapi::{DefaultGenApiCtxt, ParamsCtxt};
use cameleon::payload::PixelFormat as UPixelFormat;
use cameleon::u3v::{self, ControlHandle, StreamHandle};

use crate::camera::{Camera, FrameSink};
use crate::frame::{BufferPool, Frame, PixelFormat};
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
}

/// "vendor model (serial)" for every connected USB3 Vision camera.
pub fn list() -> Result<Vec<String>, String> {
    let cams = u3v::enumerate_cameras().map_err(|e| e.to_string())?;
    Ok(cams.iter().map(|c| describe(c.info())).collect())
}

/// Linux limits USB transfer buffers to 16 MB by default, too little for
/// streaming without loss. `linux/99-vid-recorder.rules` raises it.
fn warn_usbfs_limit() {
    if cfg!(target_os = "linux") {
        let path = "/sys/module/usbcore/parameters/usbfs_memory_mb";
        if let Some(mb) = std::fs::read_to_string(path).ok().and_then(|s| s.trim().parse::<u32>().ok()) {
            if mb != 0 && mb < 1000 {
                eprintln!("warning: usbfs_memory_mb is {mb}, frames may be lost; install linux/99-vid-recorder.rules");
            }
        }
    }
}

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
        warn_usbfs_limit();
        let mut cam = cams.swap_remove(0);
        cam.open().map_err(|e| format!("open: {e}"))?;
        cam.load_context().map_err(|e| format!("GenICam XML: {e}"))?;
        Ok(U3vCamera {
            name: describe(cam.info()),
            cam,
            running: Arc::new(AtomicBool::new(false)),
            thread: None,
            stream_errors: Arc::new(AtomicU64::new(0)),
        })
    }
}

impl Camera for U3vCamera {
    fn name(&self) -> String {
        self.name.clone()
    }

    fn apply(&mut self, s: &CameraSettings) -> Result<(), String> {
        let mut p = self.cam.params_ctxt().map_err(|e| e.to_string())?;
        set_enum(&mut p, "PixelFormat", s.pixel_format.name())?;
        // Offsets first to 0 so any width/height fits, then the real offsets.
        set_int(&mut p, "OffsetX", 0)?;
        set_int(&mut p, "OffsetY", 0)?;
        set_int(&mut p, "Width", s.width as i64)?;
        set_int(&mut p, "Height", s.height as i64)?;
        set_int(&mut p, "OffsetX", s.offset_x as i64)?;
        set_int(&mut p, "OffsetY", s.offset_y as i64)?;
        // Manual exposure/gain; the auto features may not exist on every model.
        let _ = set_enum(&mut p, "ExposureAuto", "Off");
        let _ = set_enum(&mut p, "GainAuto", "Off");
        set_float(&mut p, "ExposureTime", s.exposure_us)?;
        set_float(&mut p, "Gain", s.gain_db)?;
        let _ = set_bool(&mut p, "AcquisitionFrameRateEnable", true);
        set_float(&mut p, "AcquisitionFrameRate", s.frame_rate)?;
        for (name, value) in &s.extra {
            set_json(&mut p, name, value)?;
        }
        Ok(())
    }

    fn start(&mut self, pool: BufferPool, mut sink: FrameSink) -> Result<(), String> {
        self.stop();
        let rx = self.cam.start_streaming(STREAM_QUEUE).map_err(|e| format!("start streaming: {e}"))?;
        self.running.store(true, Relaxed);
        let running = self.running.clone();
        let errors = self.stream_errors.clone();
        let thread = std::thread::Builder::new()
            .name("u3v-camera".into())
            .spawn(move || {
                while running.load(Relaxed) {
                    // Returns an error once streaming is stopped.
                    let payload = match rx.recv_blocking() {
                        Ok(p) => p,
                        Err(_) if !running.load(Relaxed) => break,
                        Err(e) => {
                            errors.fetch_add(1, Relaxed);
                            eprintln!("stream: {e}");
                            continue;
                        }
                    };
                    let frame = payload.image_info().zip(payload.image()).and_then(|(info, image)| {
                        let format = map_format(&info.pixel_format)?;
                        let mut data = pool.get(image.len());
                        data.copy_from_slice(image);
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

    fn stop(&mut self) {
        if !self.running.swap(false, Relaxed) {
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
        UPixelFormat::BayerRG12 => PixelFormat::BayerRG12,
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
