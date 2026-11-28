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

/// What the camera allows around its applied settings, read at every
/// `apply` (GenICam computes each feature's range from the others). The
/// settings window offers exactly this, so it cannot ask for what the
/// camera refuses.
#[derive(Clone, Debug, PartialEq)]
pub struct Ranges {
    /// Formats the camera has (and we can record).
    pub formats: Vec<PixelFormat>,
    /// Highest frame rate with the applied format and size, at the shortest
    /// exposure (USB link included).
    pub max_fps: f64,
    pub exposure_us: (f64, f64),
    pub gain_db: (f64, f64),
}

/// Longest exposure that keeps the frame rate: 95% of the frame period.
pub fn exposure_cap(r: &Ranges, fps: f64) -> f64 {
    r.exposure_us.1.min(1e6 / fps * 0.95).max(r.exposure_us.0)
}

/// Snap the crop to the camera's steps and limits; notes say what changed.
pub fn fit_crop(s: &mut CameraSettings, g: &Geometry) -> Vec<String> {
    let before = (s.width, s.height, s.offset_x, s.offset_y);
    let down = |v: u32, st: u32| v / st.max(1) * st.max(1);
    let (sw, sh) = (down(g.sensor.0, g.step[0]), down(g.sensor.1, g.step[1]));
    s.width = down(s.width, g.step[0]).clamp(g.min_size.0.min(sw), sw);
    s.height = down(s.height, g.step[1]).clamp(g.min_size.1.min(sh), sh);
    s.offset_x = down(s.offset_x, g.step[2]).min(down(g.sensor.0 - s.width, g.step[2]));
    s.offset_y = down(s.offset_y, g.step[3]).min(down(g.sensor.1 - s.height, g.step[3]));
    if (s.width, s.height, s.offset_x, s.offset_y) == before {
        return Vec::new();
    }
    vec![format!("kivágás a kamera lépésközeihez igazítva: {}×{} ({}, {})", s.width, s.height, s.offset_x, s.offset_y)]
}

/// Frame rate, exposure and gain into the ranges, in this order: the
/// frame rate first, the exposure fits into its frame period.
pub fn fit_light(s: &mut CameraSettings, r: &Ranges) -> Vec<String> {
    let mut notes = Vec::new();
    if s.frame_rate > r.max_fps + 1e-3 {
        // Rounding differences silently, real changes with a note.
        if s.frame_rate - r.max_fps >= 0.05 {
            notes.push(format!("{:.1} fps helyett {:.1}: ennyi a legtöbb ezzel a formátummal és kivágással", s.frame_rate, r.max_fps));
        }
        s.frame_rate = r.max_fps;
    }
    s.frame_rate = s.frame_rate.max(1.0);
    let cap = exposure_cap(r, s.frame_rate);
    let lo = r.exposure_us.0;
    if s.auto_exposure {
        let (a, b) = s.auto_exposure_range;
        s.auto_exposure_range = (a.clamp(lo, cap), b.clamp(lo, cap).max(a.clamp(lo, cap)));
        if b > cap + 0.5 && b < 999_999.0 {
            notes.push(format!("automatikus expozíció legfeljebb {cap:.0} µs (egy képkocka ideje)"));
        }
    } else if s.exposure_us > cap + 0.5 || s.exposure_us < lo {
        notes.push(format!("expozíció {:.0} µs helyett {:.0} µs (egy képkocka ideje)", s.exposure_us, s.exposure_us.clamp(lo, cap)));
        s.exposure_us = s.exposure_us.clamp(lo, cap);
    }
    let (g0, g1) = r.gain_db;
    if g1 > g0 {
        s.gain_db = s.gain_db.clamp(g0, g1);
        let (a, b) = s.auto_gain_range;
        s.auto_gain_range = (a.clamp(g0, g1), b.clamp(g0, g1).max(a.clamp(g0, g1)));
    }
    notes
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
    /// Ranges read at the last `apply`; `None`: the camera does not tell
    /// (UVC: its modes are the choices).
    fn ranges(&self) -> Option<Ranges> {
        None
    }
    /// Adjust `s` to what the camera allows (format, crop, frame rate,
    /// exposure, gain); notes say what changed. Only while not streaming.
    fn fit(&mut self, _s: &mut CameraSettings) -> Result<Vec<String>, String> {
        Ok(Vec::new())
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
    /// Change the auto exposure target [%] and/or the crop offset while the
    /// camera streams (and records). Only what the camera allows then.
    fn live(&mut self, _target: Option<f64>, _offset: Option<(u32, u32)>) -> Result<(), String> {
        Err("a kamera futás közben nem állítható".into())
    }
    /// Exposure [µs], gain [dB] and/or frame rate while the camera streams
    /// (also recording), within what it allows now. Returns what it runs
    /// now: (exposure, gain, frame rate).
    fn live_light(&mut self, _exposure_us: Option<f64>, _gain_db: Option<f64>, _fps: Option<f64>) -> Result<(f64, f64, f64), String> {
        Err("a kamera futás közben nem állítja az expozíciót".into())
    }
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

/// The simulated sensor, like the Alvium 1800 U-291c.
const SIM_GEOMETRY: Geometry = Geometry { sensor: (1936, 1216), min_size: (8, 8), step: [8, 2, 8, 8] };

/// Like the Alvium: ~155 fps at 1080 lines (8/10 bit), 120 at 12 bit,
/// faster with fewer lines.
fn sim_ranges(s: &CameraSettings) -> Ranges {
    let full = if s.pixel_format.bits() > 10 { 120.0 } else { 155.0 };
    Ranges {
        formats: PixelFormat::ALL
            .into_iter()
            .filter(|f| !matches!(f, PixelFormat::Yuyv | PixelFormat::Uyvy | PixelFormat::Nv12 | PixelFormat::Mjpeg))
            .collect(),
        max_fps: (full * 1080.0 / s.height.max(8) as f64).min(1000.0).floor(),
        exposure_us: (10.0, 10_000_000.0),
        gain_db: (0.0, 48.0),
    }
}

/// Generates a moving test pattern at the configured frame rate with
/// sequential ids and nanosecond timestamps, like a real camera.
pub struct SimCamera {
    settings: Option<CameraSettings>,
    running: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    /// Exposure × gain relative to 2 ms at 0 dB (f64 bits): the pattern's
    /// brightness, so the exposure control can be tried without a camera.
    light: Arc<std::sync::atomic::AtomicU64>,
}

impl SimCamera {
    pub fn new() -> Self {
        SimCamera { settings: None, running: Arc::new(AtomicBool::new(false)), thread: None, light: Arc::new(1f64.to_bits().into()) }
    }

    fn set_light(&self, s: &CameraSettings) {
        let l = if s.auto_exposure { 1.0 } else { s.exposure_us / 2000.0 * 10f64.powf(s.gain_db / 20.0) };
        self.light.store(l.to_bits(), Relaxed);
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

    fn ranges(&self) -> Option<Ranges> {
        let s = self.settings.as_ref()?;
        Some(sim_ranges(s))
    }

    fn geometry(&self) -> Option<Geometry> {
        Some(SIM_GEOMETRY)
    }

    fn fit(&mut self, s: &mut CameraSettings) -> Result<Vec<String>, String> {
        let mut notes = Vec::new();
        let r = sim_ranges(s);
        if !r.formats.contains(&s.pixel_format) {
            notes.push(format!("{} helyett BayerRG8: a kamera nem tudja", s.pixel_format.name()));
            s.pixel_format = PixelFormat::BayerRG8;
        }
        notes.extend(fit_crop(s, &SIM_GEOMETRY));
        notes.extend(fit_light(s, &sim_ranges(s)));
        Ok(notes)
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
        self.set_light(&s);
        self.running.store(true, Relaxed);
        let running = self.running.clone();
        let light = self.light.clone();
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
                    fill_pattern(&mut data, s.width as usize, s.pixel_format, id, f64::from_bits(light.load(Relaxed)));
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

    fn live_light(&mut self, exposure_us: Option<f64>, gain_db: Option<f64>, fps: Option<f64>) -> Result<(f64, f64, f64), String> {
        let s = self.settings.as_mut().ok_or("no settings applied")?;
        if fps.is_some_and(|f| (f - s.frame_rate).abs() > 1e-6) {
            return Err("a szimulált kamera fps-e futás közben nem változik".into());
        }
        let r = sim_ranges(s);
        if let Some(e) = exposure_us {
            s.exposure_us = e.clamp(r.exposure_us.0, exposure_cap(&r, s.frame_rate));
        }
        if let Some(g) = gain_db {
            s.gain_db = g.clamp(r.gain_db.0, r.gain_db.1);
        }
        s.auto_exposure = false;
        let s = s.clone();
        self.set_light(&s);
        Ok((s.exposure_us, s.gain_db, s.frame_rate))
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

/// Diagonal gradient moving with the frame id, scaled by `light` (clipped
/// at full scale like a sensor).
fn fill_pattern(data: &mut [u8], width: usize, format: PixelFormat, id: u64, light: f64) {
    let shift = id as usize * 4;
    let k = (light * 256.0) as u32;
    if format.bytes_per_pixel() == 1 {
        for (y, row) in data.chunks_exact_mut(width).enumerate() {
            for (x, px) in row.iter_mut().enumerate() {
                *px = ((((x + y + shift) as u32 & 255) * k) >> 8).min(255) as u8;
            }
        }
    } else {
        let max = (1u32 << format.bits()) - 1;
        for (y, row) in data.chunks_exact_mut(width * 2).enumerate() {
            for (x, px) in row.chunks_exact_mut(2).enumerate() {
                let v = (((((x + y + shift) as u32 * 16) & max) as u64 * k as u64) >> 8).min(max as u64) as u16;
                px.copy_from_slice(&v.to_le_bytes());
            }
        }
    }
}
