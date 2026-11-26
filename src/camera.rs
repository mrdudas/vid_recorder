//! Camera interface and a simulated camera for work without hardware.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::frame::{BufferPool, Frame, PixelFormat};
use crate::scenario::{CameraSettings, Device};

/// One way the camera can run: pixel format, size and frame rate (the
/// highest the camera reports, or a candidate to try).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Mode {
    pub format: PixelFormat,
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    /// The camera reported this mode. `false`: a guess to try (the camera
    /// did not say what it can do).
    #[serde(default)]
    pub reported: bool,
    /// Every frame rate the camera lists for this format and size (UVC
    /// cameras take only these); empty: any up to `fps`.
    #[serde(default)]
    pub rates: Vec<f64>,
}

/// Image geometry the camera allows, read at every `apply`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Geometry {
    /// Full sensor (largest image) for the applied pixel format.
    pub sensor: (u32, u32),
    /// Smallest width and height.
    pub min_size: (u32, u32),
    /// Steps of Width, Height, OffsetX and OffsetY.
    pub step: [u32; 4],
}

/// What the camera allows, read from it in well under a second instead of
/// trying settings one by one.
#[derive(Clone, Debug)]
pub struct Limits {
    /// Largest image (full sensor) for the scenario's pixel format.
    pub sensor: (u32, u32),
    /// Smallest width and height.
    pub min_size: (u32, u32),
    /// Steps of Width, Height, OffsetX and OffsetY.
    pub step: [u32; 4],
    pub exposure_us: (f64, f64),
    pub gain_db: (f64, f64),
    /// Highest frame rate with exactly the scenario's settings.
    pub max_fps: Option<f64>,
    /// Heights of the `table` columns.
    pub heights: Vec<u32>,
    /// Highest frame rate per available format and height, at the
    /// scenario's width and (shortest) exposure.
    pub table: Vec<(PixelFormat, Vec<Option<f64>>)>,
}

/// Receives every frame on the camera's acquisition thread. Must be fast.
pub type FrameSink = Box<dyn FnMut(Frame) + Send>;

pub trait Camera: Send {
    fn name(&self) -> String;
    /// Apply settings. Only called while not streaming.
    fn apply(&mut self, s: &CameraSettings) -> Result<(), String>;
    fn start(&mut self, pool: BufferPool, sink: FrameSink) -> Result<(), String>;
    fn stop(&mut self);
    /// Modes the camera reports, with `s`'s exposure and extra features
    /// applied first (they limit the frame rate). Only called while not
    /// streaming.
    fn modes(&mut self, s: &CameraSettings) -> Result<Vec<Mode>, String>;
    /// Limits for `s`'s settings (see [`Limits`]). Only called while not
    /// streaming; the camera's state is undefined afterwards (apply again).
    fn limits(&mut self, _s: &CameraSettings) -> Result<Limits, String> {
        Err("ez a kamera nem adja meg a határait".into())
    }
    /// Highest frame rate the camera allows with the applied settings.
    fn max_fps(&self) -> Option<f64> {
        None
    }
    /// Geometry read at the last `apply`.
    fn geometry(&self) -> Option<Geometry> {
        None
    }
    /// The camera crops itself (ROI). `false`: it delivers its full image
    /// (`capture_size` or the sensor) and the engine crops in software.
    fn hw_crop(&self) -> bool {
        true
    }
    /// Frames the camera delivered broken or in an unusable format.
    fn errors(&self) -> u64 {
        0
    }
    /// Restart the device (it reconnects as if replugged). For a camera
    /// that is attached but sends nothing.
    fn reset(&mut self) {}
    /// The device went away (unplugged); it has to be opened again.
    fn disconnected(&self) -> bool {
        false
    }
}

/// Open the camera a scenario asks for.
pub fn open(device: &Device) -> Result<Box<dyn Camera>, String> {
    Ok(match device {
        Device::U3v => Box::new(crate::u3v::U3vCamera::open_first()?),
        Device::Uvc { name } => Box::new(crate::uvc::UvcCamera::new(name)),
        Device::Sim => Box::new(SimCamera::new()),
    })
}

/// Generates a moving test pattern at the configured frame rate with
/// sequential ids and nanosecond timestamps, like a real camera.
pub struct SimCamera {
    settings: Option<CameraSettings>,
    running: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl SimCamera {
    pub fn new() -> Self {
        SimCamera { settings: None, running: Arc::new(AtomicBool::new(false)), thread: None }
    }
}

impl Camera for SimCamera {
    fn name(&self) -> String {
        "Szimulált kamera".into()
    }

    fn apply(&mut self, s: &CameraSettings) -> Result<(), String> {
        if s.width == 0 || s.height == 0 || s.frame_rate <= 0.0 {
            return Err("width, height and frame rate must be positive".into());
        }
        if matches!(s.pixel_format, PixelFormat::Yuyv | PixelFormat::Uyvy | PixelFormat::Nv12 | PixelFormat::Mjpeg) {
            return Err(format!("the simulated camera has no {} output", s.pixel_format.name()));
        }
        self.settings = Some(s.clone());
        Ok(())
    }

    /// Like the Alvium 1800 U-291c the project uses.
    fn modes(&mut self, _: &CameraSettings) -> Result<Vec<Mode>, String> {
        let m = |format, fps| Mode { format, width: 1920, height: 1080, fps, reported: true, rates: Vec::new() };
        Ok(vec![
            m(PixelFormat::BayerRG8, 155.0),
            m(PixelFormat::BayerRG10p, 155.0),
            m(PixelFormat::BayerRG12p, 120.0),
        ])
    }

    fn start(&mut self, pool: BufferPool, mut sink: FrameSink) -> Result<(), String> {
        self.stop();
        let s = self.settings.clone().ok_or("no settings applied")?;
        self.running.store(true, Relaxed);
        let running = self.running.clone();
        let thread = std::thread::Builder::new()
            .name("sim-camera".into())
            .spawn(move || {
                let period = Duration::from_secs_f64(1.0 / s.frame_rate);
                let start = Instant::now();
                let len = Frame::byte_size(s.width, s.height, s.pixel_format);
                let mut id = 0u64;
                while running.load(Relaxed) {
                    let due = start + period * id as u32;
                    if let Some(wait) = due.checked_duration_since(Instant::now()) {
                        std::thread::sleep(wait);
                    }
                    let mut data = pool.get(len);
                    fill_pattern(&mut data, s.width as usize, s.pixel_format, id);
                    sink(Frame {
                        id,
                        timestamp_ns: start.elapsed().as_nanos() as u64,
                        width: s.width,
                        height: s.height,
                        format: s.pixel_format,
                        data,
                    });
                    id += 1;
                }
            })
            .map_err(|e| e.to_string())?;
        self.thread = Some(thread);
        Ok(())
    }

    fn stop(&mut self) {
        self.running.store(false, Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for SimCamera {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Diagonal gradient moving with the frame id.
fn fill_pattern(data: &mut [u8], width: usize, format: PixelFormat, id: u64) {
    let shift = id as usize * 4;
    if format.bytes_per_pixel() == 1 {
        for (y, row) in data.chunks_exact_mut(width).enumerate() {
            for (x, px) in row.iter_mut().enumerate() {
                *px = (x + y + shift) as u8;
            }
        }
    } else {
        let max = (1u32 << format.bits()) - 1;
        for (y, row) in data.chunks_exact_mut(width * 2).enumerate() {
            for (x, px) in row.chunks_exact_mut(2).enumerate() {
                let v = (((x + y + shift) as u32 * 16) & max) as u16;
                px.copy_from_slice(&v.to_le_bytes());
            }
        }
    }
}
