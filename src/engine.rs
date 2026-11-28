//! Ties camera, preview and recorder together.
//!
//! On the acquisition thread every frame is: checked for id gaps, copied to
//! the preview slot if 40 ms passed since the last copy (max 25 fps), then
//! moved to the recorder if recording. Nothing there waits on the UI, the
//! encoder or the disk.
//!
//! Fault tolerance ([`Engine::poll`]): a camera that is unplugged, stops
//! sending frames or is missing when a scenario starts is reopened every
//! second. A running recording is not stopped: it continues in the same
//! file once the camera is back, with frame ids and timestamps continued
//! across the gap (the missing frames show as an id jump and count as lost).

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::camera::{self, Camera, Geometry, Limits, Mode, Ranges};
use crate::frame::{self, BufferPool, Frame, PixelFormat};
use crate::recorder::{RecStats, Recorder};
use crate::gpu_view::{Look, Source};
use crate::lut::Lut;
use crate::control::{Control, Measure};
use crate::scenario::{Device, Scenario};

pub const PREVIEW_INTERVAL: Duration = Duration::from_millis(40);

#[derive(Default)]
pub struct Stats {
    pub received: AtomicU64,
    /// Frames missing according to the camera's frame id sequence.
    pub lost_at_camera: AtomicU64,
}

/// What the camera really delivers, measured on the incoming frames
/// (before any software crop). May differ from what was asked for: e.g. a
/// UVC camera that sends YUYV when asked for Mono8.
#[derive(Clone, Copy, Debug, Default)]
pub struct Actual {
    pub format: Option<PixelFormat>,
    pub width: u32,
    pub height: u32,
    /// Frames per second over the last second.
    pub fps: Option<f64>,
    /// Start of the current measuring window and frames since.
    window: Option<(Instant, u32)>,
}

impl Actual {
    fn note(&mut self, f: &Frame) {
        (self.format, self.width, self.height) = (Some(f.format), f.width, f.height);
        match &mut self.window {
            None => self.window = Some((Instant::now(), 0)),
            Some((t, n)) => {
                *n += 1;
                let dt = t.elapsed().as_secs_f64();
                if dt >= 1.0 {
                    self.fps = Some(*n as f64 / dt);
                    (*t, *n) = (Instant::now(), 0);
                }
            }
        }
    }
}

/// Last frame passed on, so ids and timestamps continue after a reconnect.
#[derive(Default)]
struct Continuity {
    last: Option<(u64, u64, Instant)>,
    /// Set by a reconnect: shift the next frames to follow `last`.
    rebase: bool,
    /// Added (wrapping) to the camera's ids and timestamps.
    id_offset: u64,
    ts_offset: u64,
}

struct Shared {
    pool: BufferPool,
    stats: Stats,
    continuity: Mutex<Continuity>,
    actual: Mutex<Actual>,
    preview: Mutex<Option<Frame>>,
    recorder: Mutex<Option<Recorder>>,
}

pub struct Engine {
    /// Open camera and the device it was opened for.
    camera: Option<(Device, Box<dyn Camera>)>,
    shared: Arc<Shared>,
    pub scenario: Option<Scenario>,
    /// Recordings stopped but still being finalized.
    finishing: Vec<Recorder>,
    /// Scenario to get (back) to while the camera is missing, and the last try.
    lost: Option<(Scenario, Instant)>,
    /// Frame count and when it last changed (stall detection).
    last_received: (u64, Instant),
    /// Last reconnect error, logged only when it changes.
    last_error: String,
    /// What the picture looks like: the live view and a corrected
    /// recording use the same tables.
    pub look: Arc<Mutex<Look>>,
    /// The program's exposure control loop (`CameraSettings::program`).
    pub control: Control,
}

pub struct RecordingInfo {
    pub stats: Arc<RecStats>,
    pub video: String,
    pub csv: String,
}

impl Engine {
    pub fn new() -> Self {
        Engine {
            look: Arc::new(Mutex::new(Look::default())),
            control: Control::default(),
            camera: None,
            shared: Arc::new(Shared {
                pool: BufferPool::default(),
                stats: Stats::default(),
                continuity: Mutex::new(Continuity::default()),
                actual: Mutex::new(Actual::default()),
                preview: Mutex::new(None),
                recorder: Mutex::new(None),
            }),
            scenario: None,
            finishing: Vec::new(),
            lost: None,
            last_received: (0, Instant::now()),
            last_error: String::new(),
        }
    }

    pub fn camera_name(&self) -> String {
        self.camera.as_ref().map_or("nincs kamera".into(), |(_, c)| c.name())
    }

    pub fn camera_errors(&self) -> u64 {
        self.camera.as_ref().map_or(0, |(_, c)| c.errors())
    }

    /// Not opened, or the open device went away.
    fn camera_gone(&self) -> bool {
        self.camera.as_ref().is_none_or(|(_, c)| c.disconnected())
    }

    /// Waiting for the camera to come back (see [`Engine::poll`]).
    pub fn waiting_for_camera(&self) -> bool {
        self.lost.is_some()
    }

    /// Call regularly (UI frame, CLI loop). Notices a camera that is gone
    /// or sends no frames, reopens it once a second, and returns a message
    /// when something changed. A recording keeps running meanwhile.
    pub fn poll(&mut self) -> Option<String> {
        if self.lost.is_none() {
            let s = self.scenario.as_ref()?;
            let n = self.shared.stats.received.load(Relaxed);
            if n != self.last_received.0 {
                self.last_received = (n, Instant::now());
            }
            let unplugged = self.camera.as_ref().is_some_and(|(_, c)| c.disconnected());
            // No frame for 3 s (or 3 frame times): hung camera or USB.
            // Not with a hardware trigger, where frames may pause.
            let c = &s.camera;
            let frame_time = 1.0 / c.frame_rate + if c.auto_exposure { 0.0 } else { c.exposure_us / 1e6 };
            let triggered = c.extra.get("TriggerMode").is_some_and(|v| v == "On");
            let stalled = !triggered && self.last_received.1.elapsed().as_secs_f64() > (3.0 * frame_time).max(3.0);
            if !unplugged && !stalled {
                return None;
            }
            let s = s.clone();
            if stalled && !unplugged {
                // Attached but silent: restart it; it comes back as if replugged.
                if let Some((_, cam)) = self.camera.as_mut() {
                    cam.reset();
                }
                self.camera = None;
            }
            self.lost = Some((s, Instant::now() - Duration::from_secs(1)));
            let why = if unplugged { "levált" } else { "nem küld képet" };
            let rec = if self.is_recording() { " A felvétel folytatódik, amint visszajön." } else { "" };
            eprintln!("camera {why}, reconnecting");
            return Some(format!("A kamera {why}. Újracsatlakozás…{rec}"));
        }
        let (s, last) = self.lost.as_ref().unwrap();
        if last.elapsed() < Duration::from_secs(1) {
            return None;
        }
        let s = s.clone();
        let since = self.shared.continuity.lock().unwrap().last.map(|(_, _, t)| t.elapsed().as_secs_f64());
        // Close the old handle first (frees the device), then reopen.
        self.scenario = None;
        self.camera = None;
        match self.start(&s, true) {
            Ok(()) => {
                self.lost = None;
                self.last_error.clear();
                eprintln!("camera back");
                Some(match since {
                    Some(gap) if self.is_recording() => {
                        format!("A kamera újra csatlakozott {gap:.1} s kiesés után, a felvétel folytatódik. Szcenárió: {}", s.name)
                    }
                    _ => format!("A kamera csatlakozott. Szcenárió aktív: {}", s.name),
                })
            }
            // Opened but the settings fail: retrying will not help.
            Err(e) if !self.camera_gone() => {
                self.lost = None;
                self.stop_recording();
                Some(format!("Hiba visszacsatlakozáskor: {e}. A felvétel leállt."))
            }
            Err(e) => {
                if e != self.last_error {
                    eprintln!("reconnect: {e}");
                    self.last_error = e;
                }
                self.lost = Some((s, Instant::now()));
                None
            }
        }
    }

    pub fn stats(&self) -> &Stats {
        &self.shared.stats
    }

    /// Switch to a scenario: open its camera if it is a different device,
    /// then stop, apply the settings and restart.
    /// If the camera cannot be opened, [`Engine::poll`] keeps trying.
    pub fn apply(&mut self, s: &Scenario) -> Result<(), String> {
        if self.is_recording() {
            return Err("cannot change scenario while recording".into());
        }
        self.lost = None;
        *self.shared.continuity.lock().unwrap() = Continuity::default();
        self.look.lock().unwrap().reset();
        self.control.reset();
        let r = self.start(s, false);
        if r.is_err() && self.camera_gone() && s.device != Device::Sim {
            self.lost = Some((s.clone(), Instant::now()));
        }
        r
    }

    /// Open (if needed), configure and start the camera. `rebase`: the
    /// frames continue the previous ones (reconnect).
    fn start(&mut self, s: &Scenario, rebase: bool) -> Result<(), String> {
        self.scenario = None;
        if let Some((_, cam)) = self.camera.as_mut() {
            cam.stop();
        }
        if self.camera.as_ref().is_none_or(|(d, _)| *d != s.device) {
            self.camera = None; // close the old device first
            self.camera = Some((s.device.clone(), camera::open(&s.device)?));
        }
        let cam = &mut self.camera.as_mut().unwrap().1;
        if let Err(e) = cam.apply(&s.camera) {
            // A camera that refused a write may refuse all further ones
            // (Alvium): reopen it on the next apply.
            if e.contains("I/O error") {
                self.camera = None;
            }
            return Err(e);
        }
        // Software crop for cameras that cannot crop themselves.
        let crop = {
            let c = &s.camera;
            let full = c.capture_size.or(cam.geometry().map(|g| g.sensor)).unwrap_or((c.width, c.height));
            if cam.hw_crop() || (c.width, c.height, c.offset_x, c.offset_y) == (full.0, full.1, 0, 0) {
                None
            } else {
                let step = frame::crop_step(c.pixel_format)
                    .ok_or("az MJPEG kép nem vágható szoftveresen: válassz más formátumot vagy a teljes képet")?;
                if c.offset_x + c.width > full.0 || c.offset_y + c.height > full.1 {
                    return Err(format!("a kivágás kilóg a {}×{} képből", full.0, full.1));
                }
                let snap = |v: u32| v / step * step;
                Some((snap(c.offset_x), snap(c.offset_y), snap(c.width), snap(c.height)))
            }
        };
        let shared = self.shared.clone();
        *shared.actual.lock().unwrap() = Actual::default();
        if rebase {
            shared.continuity.lock().unwrap().rebase = true;
        } else {
            shared.stats.received.store(0, Relaxed);
            shared.stats.lost_at_camera.store(0, Relaxed);
        }
        let fps = s.camera.frame_rate;
        let mut last_preview: Option<Instant> = None;
        let sink = Box::new(move |mut frame: Frame| {
            let st = &shared.stats;
            st.received.fetch_add(1, Relaxed);
            shared.actual.lock().unwrap().note(&frame);
            if let Some((x, y, w, h)) = crop
                && x + w <= frame.width
                && y + h <= frame.height
                && frame::crop_step(frame.format).is_some()
            {
                let mut data = shared.pool.get(Frame::byte_size(w, h, frame.format));
                frame::crop(&frame, x, y, w, h, &mut data);
                shared.pool.put(std::mem::replace(&mut frame.data, data));
                (frame.width, frame.height) = (w, h);
            }
            {
                let mut c = shared.continuity.lock().unwrap();
                if c.rebase {
                    c.rebase = false;
                    if let Some((id, ts, at)) = c.last {
                        // Frames the camera would have sent during the gap.
                        let gap = at.elapsed();
                        let missing = ((gap.as_secs_f64() * fps).round() as u64).saturating_sub(1);
                        c.id_offset = (id + 1 + missing).wrapping_sub(frame.id);
                        c.ts_offset = (ts + gap.as_nanos() as u64).wrapping_sub(frame.timestamp_ns);
                        eprintln!("continuing after {:.2} s, {missing} frames missing", gap.as_secs_f64());
                    }
                }
                frame.id = frame.id.wrapping_add(c.id_offset);
                frame.timestamp_ns = frame.timestamp_ns.wrapping_add(c.ts_offset);
                if let Some((prev, _, _)) = c.last
                    && frame.id > prev + 1
                {
                    st.lost_at_camera.fetch_add(frame.id - prev - 1, Relaxed);
                }
                c.last = Some((frame.id, frame.timestamp_ns, Instant::now()));
            }

            if last_preview.is_none_or(|t| t.elapsed() >= PREVIEW_INTERVAL) {
                last_preview = Some(Instant::now());
                copy_to_preview(&shared, &frame);
            }

            let rec = shared.recorder.lock().unwrap();
            match rec.as_ref() {
                Some(r) => r.push(frame, &shared.pool),
                None => shared.pool.put(frame.data),
            }
        });
        cam.start(self.shared.pool.clone(), sink)?;
        self.scenario = Some(s.clone());
        self.last_received = (self.shared.stats.received.load(Relaxed), Instant::now());
        Ok(())
    }

    /// Stop the camera (opening `s.device` if needed) and ask it which modes
    /// it supports. The camera stays stopped until the next `apply`.
    pub fn modes(&mut self, s: &Scenario) -> Result<Vec<Mode>, String> {
        self.stopped_camera(s)?.modes(&s.camera)
    }

    /// Stop the camera (opening `s.device` if needed) and read its limits
    /// for `s`. The camera stays stopped until the next `apply`.
    pub fn limits(&mut self, s: &Scenario) -> Result<Limits, String> {
        self.stopped_camera(s)?.limits(&s.camera)
    }

    /// Fit `s` to what its camera allows (notes say what changed), then
    /// apply it. The settings window uses this: it never asks the camera
    /// for something it refuses. A missing camera: applied as it is (the
    /// engine waits for it).
    pub fn apply_fit(&mut self, s: &mut Scenario) -> Result<Vec<String>, String> {
        if self.is_recording() {
            return Err("cannot change scenario while recording".into());
        }
        let notes = match self.stopped_camera(s) {
            Ok(cam) => match cam.fit(&mut s.camera) {
                Ok(n) => n,
                Err(e) => {
                    // A refused write locks the Alvium: reopen it.
                    if e.contains("I/O error") {
                        self.camera = None;
                    }
                    vec![format!("a kamera határai nem olvashatók: {e}")]
                }
            },
            Err(_) => Vec::new(),
        };
        self.apply(s)?;
        Ok(notes)
    }

    /// Settings that do not touch the camera (name, recording, export,
    /// preview): taken over without a restart.
    pub fn update_meta(&mut self, s: &Scenario) {
        if let Some(a) = self.scenario.as_mut()
            && a.device == s.device
            && a.camera == s.camera
        {
            *a = s.clone();
        }
    }

    /// What the active camera allows around its settings (see [`Ranges`]).
    pub fn ranges(&self) -> Option<Ranges> {
        self.camera.as_ref().and_then(|(_, c)| c.ranges())
    }

    /// Sensor size, smallest size and steps of the active camera.
    pub fn geometry(&self) -> Option<Geometry> {
        self.camera.as_ref().and_then(|(_, c)| c.geometry())
    }

    /// What the camera really delivers (see [`Actual`]).
    pub fn actual(&self) -> Actual {
        *self.shared.actual.lock().unwrap()
    }

    /// The active camera crops itself (else the engine crops in software).
    pub fn hw_crop(&self) -> bool {
        self.camera.as_ref().is_none_or(|(_, c)| c.hw_crop())
    }

    /// Highest frame rate the camera allows with the active settings.
    pub fn max_fps(&self) -> Option<f64> {
        self.camera.as_ref().and_then(|(_, c)| c.max_fps())
    }

    fn stopped_camera(&mut self, s: &Scenario) -> Result<&mut Box<dyn Camera>, String> {
        if self.is_recording() {
            return Err("cannot probe while recording".into());
        }
        self.scenario = None;
        self.lost = None;
        if let Some((_, cam)) = self.camera.as_mut() {
            cam.stop();
        }
        if self.camera.as_ref().is_none_or(|(d, _)| *d != s.device) {
            self.camera = None;
            self.camera = Some((s.device.clone(), camera::open(&s.device)?));
        }
        Ok(&mut self.camera.as_mut().unwrap().1)
    }

    /// Take the latest preview frame, if a new one arrived.
    pub fn take_preview(&self) -> Option<Frame> {
        self.shared.preview.lock().unwrap().take()
    }

    /// Follow the picture `src` with the running scenario's look (view
    /// settings, colour LUT, automatics); returns the 1D table and cube.
    pub fn observe(&self, src: &Source, lut: Option<(String, Arc<Lut>)>) -> (Arc<Vec<f32>>, Option<Arc<Vec<u16>>>) {
        let mut look = self.look.lock().unwrap();
        if let Some(s) = &self.scenario {
            look.set(&s.view, lut);
        }
        look.observe(src);
        look.tables()
    }

    /// Give a preview frame's buffer back for reuse.
    pub fn recycle(&self, f: Frame) {
        self.shared.pool.put(f.data);
    }

    /// Brightness target and crop offset while running (also recording):
    /// written to the streaming camera, no restart.
    pub fn live(&mut self, target: Option<f64>, offset: Option<(u32, u32)>) -> Result<(), String> {
        let (Some(s), Some((_, cam))) = (self.scenario.as_mut(), self.camera.as_mut()) else {
            return Err("a kamera nem fut".into());
        };
        if !cam.hw_crop() && offset.is_some() {
            return Err("ez a kamera futás közben nem vág".into());
        }
        cam.live(target, offset)?;
        if let Some(t) = target {
            s.camera.auto_target = t;
        }
        if let Some((x, y)) = offset {
            (s.camera.offset_x, s.camera.offset_y) = (x, y);
        }
        Ok(())
    }

    /// Exposure, gain and/or frame rate written to the streaming camera
    /// (also while recording); the running scenario follows what the camera
    /// runs now. Its own auto exposure is switched off by then.
    pub fn live_light(&mut self, exposure_us: Option<f64>, gain_db: Option<f64>, fps: Option<f64>) -> Result<(f64, f64, f64), String> {
        let (Some(s), Some((_, cam))) = (self.scenario.as_mut(), self.camera.as_mut()) else {
            return Err("a kamera nem fut".into());
        };
        if s.camera.auto_exposure {
            return Err("a kamera saját automatikája fut".into());
        }
        let (e, g, f) = cam.live_light(exposure_us, gain_db, fps)?;
        (s.camera.exposure_us, s.camera.gain_db, s.camera.frame_rate) = (e, g, f);
        Ok((e, g, f))
    }

    /// The program's exposure control: one step on the measured picture,
    /// written live to the camera (also while recording).
    pub fn regulate(&mut self, m: Measure) -> Result<(), String> {
        let Some(s) = &self.scenario else { return Ok(()) };
        let c = s.camera.clone();
        if !c.program || c.auto_exposure {
            return Ok(());
        }
        let Some(r) = self.ranges() else { return Ok(()) };
        let cap = crate::camera::exposure_cap(&r, c.frame_rate);
        let emin = r.exposure_us.0.max(c.auto_exposure_range.0);
        let gmin = r.gain_db.0.max(c.auto_gain_range.0);
        let bounds = (emin, cap.min(c.auto_exposure_range.1).max(emin), gmin, r.gain_db.1.min(c.auto_gain_range.1).max(gmin));
        if let Some((e, g)) = self.control.step(m, &c, (c.exposure_us, c.gain_db), bounds) {
            self.live_light(Some(e), Some(g), None)?;
        }
        Ok(())
    }

    pub fn is_recording(&self) -> bool {
        self.shared.recorder.lock().unwrap().is_some()
    }

    pub fn start_recording(&mut self, name: &str) -> Result<RecordingInfo, String> {
        let s = self.scenario.as_ref().ok_or("camera is not running")?;
        if self.is_recording() {
            return Err("already recording".into());
        }
        let c = &s.camera;
        let rec = Recorder::start(
            Path::new(&s.recording.output_dir),
            name,
            &s.recording.encoder,
            s.recording.curve8,
            s.recording.gpu_q,
            c.pixel_format,
            Frame::byte_size(c.width, c.height, c.pixel_format),
            c.frame_rate,
            self.shared.pool.clone(),
            s.recording.corrected.then(|| self.look.clone()),
        )?;
        let info = RecordingInfo {
            stats: rec.stats.clone(),
            video: rec.video_path.display().to_string(),
            csv: rec.csv_path.display().to_string(),
        };
        *self.shared.recorder.lock().unwrap() = Some(rec);
        Ok(info)
    }

    /// Stop recording; the files are finalized in the background.
    pub fn stop_recording(&mut self) {
        if let Some(mut r) = self.shared.recorder.lock().unwrap().take() {
            r.stop();
            self.finishing.push(r);
        }
        self.finalizing();
    }

    /// Stop recording and wait until all files are complete.
    pub fn finish_recording(&mut self) {
        self.stop_recording();
        for r in self.finishing.drain(..) {
            r.finish();
        }
    }

    /// Number of stopped recordings still being written.
    pub fn finalizing(&mut self) -> usize {
        self.finishing.retain(|r| !r.stats.finished.load(Relaxed));
        self.finishing.len()
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        if let Some((_, cam)) = self.camera.as_mut() {
            cam.stop();
        }
        self.finish_recording();
    }
}

fn copy_to_preview(shared: &Shared, frame: &Frame) {
    let mut data = shared.pool.get(frame.data.len());
    data.copy_from_slice(&frame.data);
    let copy = Frame { data, ..*frame };
    if let Some(old) = shared.preview.lock().unwrap().replace(copy) {
        shared.pool.put(old.data);
    }
}
