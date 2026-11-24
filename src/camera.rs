//! Camera interface and a simulated camera for work without hardware.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::frame::{BufferPool, Frame, PixelFormat};
use crate::scenario::{CameraSettings, Device};

/// Receives every frame on the camera's acquisition thread. Must be fast.
pub type FrameSink = Box<dyn FnMut(Frame) + Send>;

pub trait Camera: Send {
    fn name(&self) -> String;
    /// Apply settings. Only called while not streaming.
    fn apply(&mut self, s: &CameraSettings) -> Result<(), String>;
    fn start(&mut self, pool: BufferPool, sink: FrameSink) -> Result<(), String>;
    fn stop(&mut self);
    /// Frames the camera delivered broken or in an unusable format.
    fn errors(&self) -> u64 {
        0
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
        "Simulated camera".into()
    }

    fn apply(&mut self, s: &CameraSettings) -> Result<(), String> {
        if s.width == 0 || s.height == 0 || s.frame_rate <= 0.0 {
            return Err("width, height and frame rate must be positive".into());
        }
        if matches!(s.pixel_format, PixelFormat::Yuyv | PixelFormat::Nv12 | PixelFormat::Mjpeg) {
            return Err(format!("the simulated camera has no {} output", s.pixel_format.name()));
        }
        self.settings = Some(s.clone());
        Ok(())
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
