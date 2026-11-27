//! GUI: main page (scenario select, record start/end, preview) and settings
//! page (edit, try and save scenarios). Preview refreshes at most 25 fps.

use std::path::PathBuf;
use std::sync::atomic::Ordering::Relaxed;
use std::time::{Duration, Instant};

use eframe::egui;

use crate::camera::{Geometry, Limits, Mode};
use crate::encoder;
use crate::engine::{Engine, PREVIEW_INTERVAL, RecordingInfo};
use crate::probe::{self, Level, Verdict};
use crate::frame::{Frame, PixelFormat};
use crate::lut::{self, Lut};
use crate::recordings::{self, ExportSettings, Job, JobState, MediaInfo, Recording, Still};
use crate::scenario::{self, Device, Scenario, View};
use crate::dark::{self, Dark};
use crate::stack::{self, Stacker};

/// Loaded preview LUT: file name, LUT, 8-bit grey fast-path table.
struct PreviewLut {
    name: String,
    lut: Lut,
    gray: Vec<[u8; 3]>,
}

/// Preview images are downscaled to at most this width.
const PREVIEW_MAX_WIDTH: usize = 960;

#[derive(Clone, Copy, PartialEq)]
enum Page {
    Main,
    /// The settings window is open (crops edit the edited scenario).
    Settings,
}

/// Magnification methods of `export` and their labels.
const MAGNIFY: [(&str, &str); 5] = [
    ("phase", "Fázisalapú mozgás (pulzus, légzés)"),
    ("tremor", "Tremor (3–5, 5–8, 8–12 Hz)"),
    ("linear", "Lineáris Euler-mozgás (pulzus, légzés)"),
    ("broadband", "Szélessávú mozgás (szűretlen)"),
    ("colour", "Színváltozás a bőrön (pulzus)"),
];

/// Recordings list (left panel) and exports.
#[derive(Default)]
struct RecPage {
    list: Vec<Recording>,
    /// Waiting exports, the running one, and finished ones' results.
    queue: std::collections::VecDeque<(PathBuf, ExportSettings)>,
    job: Option<Job>,
    log: Vec<(bool, String)>,
    loaded: bool,
    refreshed: Option<Instant>,
    /// Folders added by hand (besides the scenarios' output folders).
    folders: Vec<PathBuf>,
    /// The open recording card, and the one waiting for delete confirmation.
    expanded: Option<PathBuf>,
    confirm_delete: Option<PathBuf>,
    viewer: Option<Viewer>,
}

/// Extra folders on the recordings page, one path per line.
const FOLDERS_FILE: &str = "recording_folders.txt";
/// Name of the active scenario, loaded again at the next start.
const ACTIVE_FILE: &str = "active_scenario.txt";

/// Browses one video: the frame at `t` is decoded on a worker thread.
struct Viewer {
    path: PathBuf,
    info: MediaInfo,
    t: f64,
    /// Time of the frame on screen / being decoded.
    shown: Option<f64>,
    busy: Option<std::sync::mpsc::Receiver<Result<egui::ColorImage, String>>>,
    texture: Option<egui::TextureHandle>,
    error: Option<String>,
    /// Real frame rate from the timestamp CSV (the container's may differ).
    real_fps: Option<f64>,
    /// Running playback.
    player: Option<Player>,
}

/// Plays a video at 25 pictures per second of real time: ffmpeg decodes on
/// a worker thread, which paces the pictures.
struct Player {
    rx: std::sync::mpsc::Receiver<(f64, Result<egui::ColorImage, String>)>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl Player {
    /// From `t0` seconds of video; `speed`: real seconds per video second.
    fn start(path: PathBuf, info: MediaInfo, t0: f64, speed: f64) -> Player {
        const RATE: f64 = 25.0;
        let (tx, rx) = std::sync::mpsc::sync_channel(2);
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let quit = stop.clone();
        std::thread::spawn(move || {
            let mut d = match recordings::Decoder::open(&path, &info, t0, PREVIEW_MAX_WIDTH as u32, Some(RATE), None) {
                Ok(d) => d,
                Err(e) => {
                    let _ = tx.send((t0, Err(e)));
                    return;
                }
            };
            let start = Instant::now();
            let mut n = 0u64;
            while !quit.load(Relaxed) {
                let Some(f) = d.next() else { break };
                let due = start + Duration::from_secs_f64(n as f64 / RATE * speed);
                if let Some(w) = due.checked_duration_since(Instant::now()) {
                    std::thread::sleep(w);
                }
                if tx.send((t0 + n as f64 / RATE, f.and_then(still_image))).is_err() {
                    break;
                }
                n += 1;
            }
        });
        Player { rx, stop }
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.stop.store(true, Relaxed);
    }
}

/// CPU, memory and disk, refreshed once a second.
struct SysInfo {
    sys: sysinfo::System,
    at: Option<Instant>,
    cpu: f32,
    mem: (u64, u64),
    /// Free and total bytes on the recording disk.
    disk: Option<(u64, u64)>,
}

pub struct App {
    engine: Engine,
    dir: PathBuf,
    scenarios: Vec<Scenario>,
    page: Page,
    /// Working copy on the settings page.
    edit: Scenario,
    extra_json: String,
    rec_name: String,
    recording: Option<Rec>,
    /// Running recording test (settings page) and its last result.
    test: Option<(probe::Test, Scenario)>,
    test_secs: f64,
    test_result: Option<Verdict>,
    /// Camera discovery: modes still to test, results so far, and the
    /// scenario to return to afterwards (`Some` while discovery runs).
    probe_queue: std::collections::VecDeque<Scenario>,
    probe_results: Vec<(Scenario, Verdict)>,
    probe_base: Option<Scenario>,
    message: String,
    texture: Option<egui::TextureHandle>,
    lut_dir: PathBuf,
    lut: Option<PreviewLut>,
    /// Cameras offered on the settings page (refreshed on request).
    devices: Vec<Device>,
    /// Smoothed auto white balance gains (R, G, B) for the preview.
    wb_gains: [f32; 3],
    /// Smoothed auto-levels black and white points (0..1) for the preview.
    levels: (f32, f32),
    /// Camera limits for the device they were read from (or why not).
    limits: Option<(Device, Result<Limits, String>)>,
    /// Read the limits on the next frame (after the message is shown).
    limits_pending: bool,
    /// Modes the camera reported or accepted when opened (or why not).
    modes: Option<(Device, Result<Vec<Mode>, String>)>,
    /// Geometry of the camera last applied (kept while it is away).
    geometry: Option<(Device, Geometry)>,
    /// Size of the last preview frame (camera pixels).
    frame_size: (u32, u32),
    /// Its crop offset on the sensor.
    frame_pos: (u32, u32),
    /// Crop selection being dragged on the preview.
    crop_drag: Option<CropDrag>,
    /// Camera target brightness while its slider is dragged.
    light_drag: Option<f64>,
    rec: RecPage,
    /// Screenshot mode (`vid_recorder screenshots`).
    shots: Option<Shots>,
    sys: SysInfo,
    /// The last finished recording (closing state, losses).
    last_rec: Option<(PathBuf, std::sync::Arc<crate::recorder::RecStats>)>,
    /// Result of each export started in this session: output or error.
    export_result: std::collections::BTreeMap<PathBuf, Result<PathBuf, String>>,
    confirm_delete_scenario: bool,
    /// Spacing set up (first frame).
    styled: bool,
    /// Preview averaging and the settings it runs with (a change restarts it).
    stacker: Stacker,
    stack_settings: Option<(String, stack::Settings)>,
    /// Dark frame of a scenario (loaded once per name; `None`: has none).
    dark: Option<(String, Option<std::sync::Arc<Dark>>)>,
    /// Dark frames being taken, and how many to take.
    dark_capture: Option<dark::Capture>,
    dark_frames: u32,
}

/// A running recording on the main page.
struct Rec {
    info: RecordingInfo,
    started: Instant,
    paused_since: Option<Instant>,
    paused_total: Duration,
    /// Camera losses counted before the recording started.
    lost_before: u64,
}

impl Rec {
    /// Recorded time, pauses excluded.
    fn elapsed(&self) -> Duration {
        let paused = self.paused_total + self.paused_since.map_or(Duration::ZERO, |p| p.elapsed());
        self.started.elapsed().saturating_sub(paused)
    }

    fn set_paused(&mut self, pause: bool) {
        match (pause, self.paused_since) {
            (true, None) => self.paused_since = Some(Instant::now()),
            (false, Some(p)) => {
                self.paused_total += p.elapsed();
                self.paused_since = None;
            }
            _ => return,
        }
        self.info.stats.paused.store(pause, Relaxed);
    }
}

/// Camcorder-style mark in the preview's corner: blinking red "● REC" (or
/// "⏸ PAUSE") and the recorded time.
fn rec_badge(painter: &egui::Painter, rect: egui::Rect, r: &Rec) {
    let t = r.elapsed().as_secs();
    let time = format!("{:02}:{:02}:{:02}", t / 3600, t / 60 % 60, t % 60);
    let blink = r.started.elapsed().as_millis() % 1000 < 600;
    let font = egui::FontId::proportional(20.0);
    let at = rect.left_top() + egui::vec2(14.0, 12.0);
    let bg = egui::Rect::from_min_size(at - egui::vec2(8.0, 4.0), egui::vec2(190.0, 32.0));
    painter.rect_filled(bg, 6.0, egui::Color32::from_black_alpha(140));
    let (label, color) = if r.paused_since.is_some() {
        ("⏸ PAUSE", egui::Color32::from_rgb(255, 190, 0))
    } else {
        ("● REC", egui::Color32::from_rgb(255, 40, 40))
    };
    if blink {
        painter.text(at, egui::Align2::LEFT_TOP, label, font.clone(), color);
    }
    painter.text(at + egui::vec2(96.0, 0.0), egui::Align2::LEFT_TOP, time, font, egui::Color32::WHITE);
}

/// Edges of the crop selection held by the mouse (all four: moving it).
#[derive(Clone, Copy)]
struct Grab {
    l: bool,
    r: bool,
    t: bool,
    b: bool,
}

/// A crop selection being dragged, in sensor pixels.
struct CropDrag {
    /// `None`: a new selection drawn from `from`.
    grab: Option<Grab>,
    from: egui::Pos2,
    start: egui::Rect,
    sel: egui::Rect,
}

/// USB3 Vision (if one is connected), every UVC camera, and the simulator.
fn find_devices() -> Vec<Device> {
    let mut v = Vec::new();
    if crate::u3v::list().is_ok_and(|l| !l.is_empty()) {
        v.push(Device::U3v);
    }
    v.extend(crate::uvc::list().into_iter().map(|name| Device::Uvc { name }));
    v.push(Device::Sim);
    v
}

impl App {
    pub fn new(engine: Engine, dir: PathBuf, lut_dir: PathBuf) -> Self {
        let mut scenarios = scenario::load_all(&dir);
        if scenarios.is_empty() {
            let s = Scenario::default();
            let _ = scenario::save(&dir, &s);
            scenarios.push(s);
        }
        // The scenario that was active at the last exit.
        let last = std::fs::read_to_string(ACTIVE_FILE).unwrap_or_default();
        let edit = scenarios.iter().find(|s| s.name == last.trim()).unwrap_or(&scenarios[0]).clone();
        let mut app = App {
            engine,
            dir,
            extra_json: extra_to_json(&edit),
            edit,
            scenarios,
            page: Page::Main,
            rec_name: String::new(),
            recording: None,
            test: None,
            test_secs: 5.0,
            test_result: None,
            probe_queue: Default::default(),
            probe_results: Vec::new(),
            probe_base: None,
            message: String::new(),
            texture: None,
            lut_dir,
            lut: None,
            devices: find_devices(),
            wb_gains: [1.0; 3],
            levels: (0.0, 1.0),
            limits: None,
            limits_pending: false,
            modes: None,
            geometry: None,
            frame_size: (0, 0),
            frame_pos: (0, 0),
            crop_drag: None,
            light_drag: None,
            rec: RecPage::default(),
            shots: None,
            last_rec: None,
            sys: SysInfo { sys: sysinfo::System::new(), at: None, cpu: 0.0, mem: (0, 0), disk: None },
            export_result: Default::default(),
            confirm_delete_scenario: false,
            styled: false,
            stacker: Stacker::new(),
            stack_settings: None,
            dark: None,
            dark_capture: None,
            dark_frames: 32,
        };
        let first = app.edit.clone();
        app.apply(&first);
        app
    }

    /// Screenshot mode: every scenario runs on the simulated camera (not
    /// saved), the pages are walked through and saved as PNG into `dir`.
    /// With `camera` the scenarios' own cameras are used and a longer walk
    /// exercises crop, pause, limits, averaging and dark calibration; its
    /// recording and dark frame are left for checking.
    pub fn screenshots(mut self, dir: PathBuf, camera: bool) -> Self {
        if !camera {
            for s in &mut self.scenarios {
                s.device = Device::Sim;
            }
            self.edit.device = Device::Sim;
        }
        let first = self.scenarios[0].clone();
        self.apply(&first);
        let steps: &'static [Shot] = if camera { &CAMERA_SHOTS } else { &SHOTS };
        self.shots = Some(Shots { dir, steps, cleanup: !camera, step: 0, started: None, pending: None });
        self
    }

    /// Crop the page's scenario to the centre `w`×`h` (screenshot walk).
    fn crop_centre(&mut self, w: f32, h: f32) {
        let Some(g) = self.geometry() else { return };
        let sensor = egui::vec2(g.sensor.0 as f32, g.sensor.1 as f32);
        let min = egui::vec2(g.min_size.0 as f32, g.min_size.1 as f32);
        let sel = egui::Rect::from_center_size((sensor / 2.0).to_pos2(), egui::vec2(w, h));
        self.crop_to(sel, g.step, min, sensor);
    }

    /// Make `s` the active scenario: the camera switches to it, the main
    /// and the settings page both show it, and it is loaded at the next start.
    fn apply(&mut self, s: &Scenario) {
        if self.edit.name != s.name || self.edit != *s {
            self.extra_json = extra_to_json(s);
            self.edit = s.clone();
            self.confirm_delete_scenario = false;
        }
        if self.shots.is_none() {
            let _ = std::fs::write(ACTIVE_FILE, &s.name);
        }
        self.message = match self.engine.apply(s) {
            Ok(()) => format!("Szcenárió aktív: {}", s.name),
            Err(e) if self.engine.waiting_for_camera() => format!("Nincs kamera ({e}). Várakozás a csatlakozásra…"),
            Err(e) => format!("Hiba: {e}"),
        };
    }

    /// Unplugged, hung or missing camera: the engine reopens it and keeps
    /// a running recording going; show what happened.
    fn watch_camera(&mut self) {
        if let Some(m) = self.engine.poll() {
            self.message = m;
        }
        // Gave up (settings fail after reconnect) and stopped the recording.
        if self.recording.is_some() && !self.engine.is_recording() {
            self.recording = None;
            self.rec_name.clear();
        }
    }

    fn reload(&mut self) {
        self.scenarios = scenario::load_all(&self.dir);
    }

    /// Load the active scenario's preview LUT if it changed.
    fn sync_lut(&mut self) {
        let wanted = self.engine.scenario.as_ref().and_then(|s| s.preview_lut.clone());
        if wanted.as_deref() == self.lut.as_ref().map(|l| l.name.as_str()) {
            return;
        }
        self.lut = None;
        if let Some(name) = wanted {
            match Lut::load(&self.lut_dir.join(&name)) {
                Ok(lut) => {
                    let gray = lut.gray_table();
                    self.lut = Some(PreviewLut { name, lut, gray });
                }
                Err(e) => {
                    self.message = format!("LUT hiba: {e}");
                    // Remember the name so the failing load is not retried every frame.
                    if let Some(s) = self.engine.scenario.as_mut() {
                        s.preview_lut = None;
                    }
                }
            }
        }
    }

    fn update_preview(&mut self, ctx: &egui::Context) {
        self.sync_lut();
        if let (Some(g), Some(s)) = (self.engine.geometry(), &self.engine.scenario) {
            self.geometry = Some((s.device.clone(), g));
        }
        if let Some(frame) = self.engine.take_preview() {
            self.frame_size = (frame.width, frame.height);
            if let Some(s) = &self.engine.scenario {
                self.frame_pos = (s.camera.offset_x, s.camera.offset_y);
            }
            let view = self.engine.scenario.as_ref().map(|s| s.view.clone()).unwrap_or_default();
            let wb = view.auto_white_balance.then_some(self.wb_gains);
            let levels = view.auto_levels.then_some(self.levels);
            let tone = tone_table(&view, levels);
            let name = self.engine.scenario.as_ref().map_or(String::new(), |s| s.name.clone());
            self.take_dark(&frame);
            // The dark frame, if switched on and it fits these frames.
            let shape = Some((frame.format, frame.width, frame.height));
            let dark = self.dark_of(shape).filter(|d| view.dark && self.engine.scenario.as_ref().is_some_and(|s| d.check(&s.camera, shape).is_ok()));
            let img = if view.average {
                // Restart on a new scenario or other averaging settings.
                let settings = stack::Settings { frames: view.average_frames, align: view.align, dark: dark.is_some() };
                if self.stack_settings.as_ref() != Some(&(name.clone(), settings)) {
                    self.stacker.reset();
                    self.stack_settings = Some((name, settings));
                }
                if let Some(f) = self.stacker.push(frame, settings, dark) {
                    self.engine.recycle(f);
                }
                match self.stacker.take() {
                    Some(avg) => image_to_color(&avg, self.lut.as_ref(), wb, &tone),
                    None => return,
                }
            } else if let Some(d) = dark {
                self.stack_settings = None;
                let img = stack::to_image(&frame, Some(&d)).map(|i| stack::downscale(&i, PREVIEW_MAX_WIDTH));
                self.engine.recycle(frame);
                img.and_then(|i| image_to_color(&i, self.lut.as_ref(), wb, &tone))
            } else {
                self.stack_settings = None;
                let img = to_color_image(&frame, PREVIEW_MAX_WIDTH, self.lut.as_ref(), wb, &tone);
                self.engine.recycle(frame);
                img
            };
            let Some((img, means, hist)) = img else { return };
            // Auto-levels: 0.5 % and 99.5 % of the brightness histogram.
            let total: u32 = hist.iter().sum();
            let pct = |p: f32| {
                let mut acc = 0;
                hist.iter().position(|&h| { acc += h; acc as f32 >= p * total as f32 }).unwrap_or(255) as f32 / 255.0
            };
            if total > 0 {
                let (lo, hi) = (pct(0.005), pct(0.995));
                let hi = hi.max(lo + 0.05);
                self.levels = (0.8 * self.levels.0 + 0.2 * lo, 0.8 * self.levels.1 + 0.2 * hi);
            }
            // Grey world: scale R and B so their means match G; smoothed
            // over frames so the preview does not flicker.
            if let Some([r, g, b]) = means {
                let target = [g / r.max(1e-3), 1.0, g / b.max(1e-3)].map(|x| x.clamp(0.25, 4.0) as f32);
                for c in 0..3 {
                    self.wb_gains[c] = 0.8 * self.wb_gains[c] + 0.2 * target[c];
                }
            }
            match &mut self.texture {
                Some(t) => t.set(img, egui::TextureOptions::LINEAR),
                None => self.texture = Some(ctx.load_texture("preview", img, egui::TextureOptions::LINEAR)),
            }
        }
    }

    /// White balance, brightness and contrast of the active scenario's
    /// preview. Saved with the scenario on the settings page.
    /// The camera's dark frame that fits the running settings best
    /// (looked up again when the camera, settings or frames change).
    fn dark_of(&mut self, frames: Option<(PixelFormat, u32, u32)>) -> Option<std::sync::Arc<Dark>> {
        let s = self.engine.scenario.as_ref()?;
        let camera = self.engine.camera_name();
        let key = format!("{camera}|{:?}|{frames:?}", s.camera);
        if self.dark.as_ref().is_none_or(|(k, _)| *k != key) {
            let d = dark::find(&camera, &s.camera, frames).and_then(|p| Dark::load(&p).ok()).map(std::sync::Arc::new);
            self.dark = Some((key, d));
        }
        self.dark.as_ref().and_then(|(_, d)| d.clone())
    }

    /// While dark frames are being taken: add this one; save when done.
    fn take_dark(&mut self, f: &Frame) {
        let Some(cap) = self.dark_capture.as_mut() else { return };
        let Some(s) = self.engine.scenario.clone() else { return };
        if let Err(e) = cap.add(f) {
            self.message = format!("Sötétkép: {e}");
            self.dark_capture = None;
            return;
        }
        if !cap.done() {
            return;
        }
        let Some(d) = self.dark_capture.take().and_then(|c| c.finish(&s.camera)) else { return };
        let path = dark::path(&self.engine.camera_name(), &d.meta);
        self.message = match d.save(&path) {
            Ok(()) => format!(
                "Sötétkép kész: {} kép, átlagszint {:.2}%, {} forró pixel. Most vedd le a takarást az objektívről.",
                d.meta.frames,
                d.meta.mean * 100.0,
                d.meta.hot
            ),
            Err(e) => format!("A sötétkép nem menthető: {e}"),
        };
        self.dark = None;
        if let Some(a) = self.engine.scenario.as_mut() {
            a.view.dark = true;
        }
        self.edit.view.dark = true;
        self.stacker.reset();
    }

    /// Dark calibration of the camera: cover the lens, take, use as filter.
    fn dark_section(&mut self, ui: &mut egui::Ui) {
        let frames = (self.frame_size.0 > 0).then_some(()).and(self.engine.actual().format).map(|f| (f, self.frame_size.0, self.frame_size.1));
        let dark = self.dark_of(frames);
        let camera = self.engine.camera_name();
        let running = self.engine.scenario.clone();
        let mut delete = None;
        section(ui, "Sötétkalibráció", true, |ui| {
            let Some(s) = running else {
                ui.label("A kamera nem fut.");
                return;
            };
            if !dark::supported(s.camera.pixel_format) {
                ui.label(format!("{} képnél nem lehet: csak nyers (Mono, Bayer) formátumnál.", s.camera.pixel_format.name()));
                return;
            }
            ui.small("A szenzor minden pixele fény nélkül is ad egy alapértéket (sötétszint, mintázat, forró pixelek). A sötétkép ezt méri meg, és szűrőként levonja a képből. A kamerához tartozik, formátumonként, kivágásonként, expozíciónként és erősítésenként egy.");
            ui.add_space(4.0);
            match &self.dark_capture {
                Some(c) => {
                    let (n, target) = (c.n, c.target);
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(egui::RichText::new(format!("Sötétkép felvétele: {n}/{target} – az objektív maradjon letakarva!")).strong());
                        if ui.button("Mégse").clicked() {
                            self.dark_capture = None;
                        }
                    });
                }
                None => {
                    ui.label("1. Takard le az objektívet, hogy ne érje fény.");
                    ui.horizontal(|ui| {
                        ui.label("2.");
                        if ui
                            .add(egui::Button::new(egui::RichText::new("◼  Sötétkép felvétele").strong()).min_size(egui::vec2(180.0, 30.0)))
                            .on_hover_text(format!(
                                "{} kép átlaga, a mostani beállításokkal: {} {}×{}, {} µs, {} dB.",
                                self.dark_frames, s.camera.pixel_format.name(), s.camera.width, s.camera.height, s.camera.exposure_us, s.camera.gain_db
                            ))
                            .clicked()
                        {
                            self.dark_capture = Some(dark::Capture::new(self.dark_frames));
                            self.message = "Sötétkép felvétele…".into();
                        }
                        ui.add(egui::DragValue::new(&mut self.dark_frames).range(4..=1000).suffix(" kép"));
                    });
                    ui.label("3. Vedd le a takarást; a sötétkép szűrőként levonódik.");
                }
            }
            ui.add_space(4.0);
            match &dark {
                Some(d) => {
                    let m = &d.meta;
                    let mut on = s.view.dark;
                    if ui
                        .checkbox(&mut on, egui::RichText::new("Sötétkép levonása (szűrő)").strong())
                        .on_hover_text("Az élőképre, az átlagolásra és a mentett képre hat. A felvétel nyers marad.")
                        .changed()
                    {
                        if let Some(a) = self.engine.scenario.as_mut() {
                            a.view.dark = on;
                        }
                        self.edit.view.dark = on;
                    }
                    ui.small(format!(
                        "Használt sötétkép: {} {}×{}, {} µs, {} dB · {} kép, {} · átlagszint {:.2}%, {} forró pixel",
                        m.format.name(), m.width, m.height, m.exposure_us, m.gain_db, m.frames, m.taken, m.mean * 100.0, m.hot
                    ));
                    if let Ok(w) = d.check(&s.camera, frames)
                        && !w.is_empty()
                    {
                        ui.colored_label(ORANGE, format!("{}: érdemes újat felvenni.", w.join("; ")));
                    }
                }
                None => {
                    ui.colored_label(ORANGE, "Ehhez a beállításhoz (formátum, méret, kivágás) még nincs sötétkép.");
                }
            }
            let all = dark::list(&camera);
            if !all.is_empty() {
                egui::CollapsingHeader::new(format!("A kamera sötétképei ({})", all.len())).show(ui, |ui| {
                    egui::Grid::new("darks").striped(true).show(ui, |ui| {
                        for (p, m) in &all {
                            ui.label(format!("{} {}×{} ({}, {})", m.format.name(), m.width, m.height, m.offset_x, m.offset_y));
                            ui.label(if m.auto_exposure { "auto".into() } else { format!("{} µs, {} dB", m.exposure_us, m.gain_db) });
                            ui.label(&m.taken);
                            if ui.small_button("Törlés").clicked() {
                                delete = Some(p.clone());
                            }
                            ui.end_row();
                        }
                    });
                });
            }
        });
        if let Some(p) = delete {
            let _ = std::fs::remove_file(&p);
            self.dark = None;
            self.stacker.reset();
        }
    }

    fn view_controls(&mut self, ui: &mut egui::Ui) {
        self.camera_light(ui);
        let Some(s) = self.engine.scenario.as_mut() else { return };
        let v = &mut s.view;
        ui.add_space(4.0);
        ui.label(egui::RichText::new("Megjelenítés (csak az élőkép)").strong());
        ui.checkbox(&mut v.auto_white_balance, "Automatikus fehéregyensúly");
        ui.checkbox(&mut v.auto_levels, "Automatikus szintek");
        ui.checkbox(&mut v.average, "Átlagolás (zajcsökkentés)")
            .on_hover_text("Az élőkép több kép átlaga. A felvétel nyers marad; az átlagolt kép elmenthető.");
        if v.average {
            let mut restart = false;
            let mut save = false;
            ui.indent("average", |ui| {
                ui.horizontal(|ui| {
                    let mut auto = v.average_frames == 0;
                    if ui.checkbox(&mut auto, "automatikus képszám").on_hover_text("Addig vesz hozzá képet, amíg egy újabb már alig változtat az átlagon (0,2%).").changed() {
                        v.average_frames = if auto { 0 } else { 16 };
                    }
                    if !auto {
                        ui.add(egui::DragValue::new(&mut v.average_frames).range(2..=1000).suffix(" kép"));
                    }
                });
                ui.checkbox(&mut v.align, "Igazítás (elmozdulás és elfordulás kompenzálása)")
                    .on_hover_text("Minden képet az átlaghoz igazít eltolással és forgatással (pl. csillagos ég, rezgés). Lassabb.");
                let st = self.stacker.status();
                let depth = if v.average_frames == 0 && !st.settled { format!("{} kép, még nő", st.depth) } else { format!("{} kép", st.depth) };
                let mut line = format!("Átlag: {depth}");
                if v.align {
                    line += &format!(" · elmozdulás {:.1}, {:.1} px, {:.2}°", st.motion.0, st.motion.1, st.motion.2);
                    if st.rejected > 0 {
                        line += &format!(" · {} kép nem volt igazítható", st.rejected);
                    }
                }
                ui.small(line);
                ui.horizontal(|ui| {
                    restart = ui.button("Újrakezdés").clicked();
                    save = ui.button("Átlagolt kép mentése").on_hover_text("16 bites, lineáris PNG a teljes felbontásban (Bayer: fél felbontás), LUT, fehéregyensúly és szintek nélkül, a szcenárió mappájába.").clicked();
                });
                match &st.saved {
                    Some(Ok(p)) => {
                        ui.small(format!("Mentve: {p}"));
                    }
                    Some(Err(e)) => {
                        ui.colored_label(RED, format!("Mentési hiba: {e}"));
                    }
                    None => {}
                }
            });
            if restart {
                self.stacker.reset();
            }
            if save {
                let dir = PathBuf::from(&s.recording.output_dir);
                self.stacker.save(dir.join(chrono::Local::now().format("atlag_%Y%m%d_%H%M%S.png").to_string()));
            }
        }
        egui::Grid::new("view").num_columns(2).show(ui, |ui| {
            ui.label("Fényerő");
            ui.add(egui::Slider::new(&mut v.brightness, -0.5..=0.5).fixed_decimals(2));
            ui.end_row();
            ui.label("Kontraszt");
            ui.add(egui::Slider::new(&mut v.contrast, 0.25..=4.0).logarithmic(true).fixed_decimals(2));
            ui.end_row();
        });
        if ui.button("Alaphelyzet").clicked() {
            *v = View { auto_white_balance: v.auto_white_balance, auto_levels: v.auto_levels, ..View::default() };
        }
        let frames = self.engine.actual().format.map(|f| (f, self.frame_size.0, self.frame_size.1));
        if self.dark_of(frames).is_some()
            && let Some(s) = self.engine.scenario.as_mut()
        {
            ui.checkbox(&mut s.view.dark, "Sötétkép levonása").on_hover_text("A kamera sötétképe (Beállítások → Sötétkalibráció).");
            self.edit.view.dark = s.view.dark;
        }
        ui.small("A megjelenítés csak az élőképre hat, a felvétel nyers marad.");
    }

    /// Brightness at the source: the camera's auto exposure (longest
    /// exposure the frame rate allows, then the least gain) aims at the
    /// target. Applied to the camera when the slider is released.
    fn camera_light(&mut self, ui: &mut egui::Ui) {
        let Some(s) = self.engine.scenario.as_ref().filter(|s| s.device == Device::U3v) else { return };
        let mut c = s.camera.clone();
        if let Some(t) = self.light_drag {
            c.auto_target = t;
        }
        let mut commit = false;
        ui.label(egui::RichText::new("Kamera (a forrásnál)").strong());
        let recording = self.engine.is_recording();
        ui.vertical(|ui| {
            commit |= ui
                .add_enabled(!recording, egui::Checkbox::new(&mut c.auto_exposure, "automatikus expozíció"))
                .on_hover_text("A kamera a lehető leghosszabb expozíciót használja (az fps-ből: egy képkocka ideje), és csak ha ez kevés, növeli az erősítést, a lehető legkevésbé.")
                .changed();
            if c.auto_exposure {
                ui.horizontal(|ui| {
                    ui.label("Célfényerő");
                    let r = ui
                        .add(egui::Slider::new(&mut c.auto_target, 5.0..=80.0).fixed_decimals(0).suffix(" %"))
                        .on_hover_text("Erre az átlagos fényerőre állít a kamera. Ha a világos részek beégnek, csökkentsd. A felvételre is hat.");
                    if r.dragged() {
                        self.light_drag = Some(c.auto_target);
                    }
                    commit |= r.drag_stopped() || (r.changed() && !r.dragged());
                });
            } else {
                ui.small(format!("Kézi: {:.0} µs, {:.1} dB (Beállítások)", c.exposure_us, c.gain_db));
            }
        });
        if commit {
            self.light_drag = None;
            self.set_camera_light(c.auto_exposure, c.auto_target);
        }
    }

    /// Auto exposure on/off and its target: on the main page the running
    /// scenario only (not saved), on the settings page the edited one.
    fn set_camera_light(&mut self, auto: bool, target: f64) {
        let main = self.page == Page::Main;
        // Only the target changes: written to the streaming camera (this
        // also works while recording), else a restart with the new settings.
        let running_auto = self.engine.scenario.as_ref().is_some_and(|s| s.camera.auto_exposure);
        let recording = self.engine.is_recording();
        if auto && running_auto && (main || recording) {
            match self.engine.live(Some(target), None) {
                Ok(()) => {
                    self.edit.camera.auto_target = target;
                    self.message = format!("Kamera célfényerő: {target:.0}%");
                    return;
                }
                Err(e) if recording => {
                    self.message = format!("A fényerő most nem állítható: {e}");
                    return;
                }
                Err(_) => {}
            }
        }
        let Some(mut s) = (if main { self.engine.scenario.clone() } else { self.edited() }) else { return };
        let before = self.engine.scenario.clone();
        (s.camera.auto_exposure, s.camera.auto_target) = (auto, target);
        match self.engine.apply(&s) {
            Ok(()) => {
                (self.edit.camera.auto_exposure, self.edit.camera.auto_target) = (auto, target);
                self.message = if auto { format!("Kamera célfényerő: {target:.0}%") } else { "Kézi expozíció".into() };
            }
            Err(e) => {
                if let Some(b) = before {
                    self.apply(&b);
                }
                self.message = format!("A kamera nem fogadta el: {e}. Visszaállítva.");
            }
        }
    }

    /// The preview scaled to fill the available space. With `sensor_view`
    /// the canvas is the whole sensor, the (possibly cropped) frame sits at
    /// its offset, and the edited crop is drawn on it; with `crop` the
    /// selection can be moved, resized by its edges and corners, or drawn
    /// anew (see `crop_to`).
    fn preview(&mut self, ui: &mut egui::Ui, sensor_view: bool, crop: Crop) {
        let Some(t) = &self.texture else {
            ui.centered_and_justified(|ui| ui.label(egui::RichText::new("Nincs kép").size(18.0).weak()));
            return;
        };
        let (tex, ts) = (t.id(), t.size_vec2());
        let area = ui.available_rect_before_wrap();
        let fs = egui::vec2(self.frame_size.0 as f32, self.frame_size.1 as f32);
        let geo = self.geometry();
        let sensor_view = sensor_view && fs.x > 0.0 && geo.is_some();
        // Canvas size and where the frame lies on it, in camera pixels: the
        // sensor stays the canvas even while the camera is not running.
        let (canvas, img) = match geo {
            Some(g) if sensor_view => {
                let off = egui::pos2(self.frame_pos.0 as f32, self.frame_pos.1 as f32);
                let sensor = egui::vec2(g.sensor.0 as f32, g.sensor.1 as f32);
                (sensor, egui::Rect::from_min_size(off, fs).intersect(egui::Rect::from_min_size(egui::Pos2::ZERO, sensor)))
            }
            _ => (ts, egui::Rect::from_min_size(egui::Pos2::ZERO, ts)),
        };
        let scale = (area.width() / canvas.x).min(area.height() / canvas.y).max(1e-4);
        let rect = egui::Rect::from_center_size(area.center(), canvas * scale);
        let to_screen = |p: egui::Pos2| rect.min + p.to_vec2() * scale;
        let to_cam = |p: egui::Pos2| ((p - rect.min) / scale).to_pos2().clamp(egui::Pos2::ZERO, canvas.to_pos2());
        let resp = ui.allocate_rect(rect, if crop != Crop::Off { egui::Sense::drag() } else { egui::Sense::hover() });
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, egui::Color32::from_gray(25));
        let uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
        painter.image(tex, egui::Rect::from_min_max(to_screen(img.min), to_screen(img.max)), uv, egui::Color32::WHITE);
        if let Some(r) = &self.recording {
            // Above the crop shading drawn later.
            let top = ui.ctx().layer_painter(egui::LayerId::new(egui::Order::Foreground, ui.scope_id().with("rec_badge")));
            rec_badge(&top.with_clip_rect(rect), rect, r);
        }
        if !sensor_view {
            return;
        }

        // The selection: the edited scenario's crop, or the one being dragged.
        let c = match (self.page, &self.engine.scenario) {
            (Page::Main, Some(a)) => &a.camera,
            _ => &self.edit.camera,
        };
        let roi = egui::Rect::from_min_size(
            egui::pos2(c.offset_x as f32, c.offset_y as f32),
            egui::vec2(c.width as f32, c.height as f32),
        )
        .intersect(egui::Rect::from_min_size(egui::Pos2::ZERO, canvas));
        let sel = self.crop_drag.as_ref().map_or(roi, |d| d.sel);
        let s = egui::Rect::from_min_max(to_screen(sel.min), to_screen(sel.max));
        let shade = egui::Color32::from_black_alpha(150);
        for r in [
            egui::Rect::from_min_max(rect.min, egui::pos2(rect.max.x, s.min.y)),
            egui::Rect::from_min_max(egui::pos2(rect.min.x, s.max.y), rect.max),
            egui::Rect::from_min_max(egui::pos2(rect.min.x, s.min.y), egui::pos2(s.min.x, s.max.y)),
            egui::Rect::from_min_max(egui::pos2(s.max.x, s.min.y), egui::pos2(rect.max.x, s.max.y)),
        ] {
            painter.rect_filled(r, 0.0, shade);
        }
        painter.rect_stroke(s, 0.0, (2.0, egui::Color32::YELLOW), egui::StrokeKind::Middle);
        for p in [s.left_top(), s.right_top(), s.left_bottom(), s.right_bottom()] {
            painter.rect_filled(egui::Rect::from_center_size(p, egui::vec2(9.0, 9.0)), 0.0, egui::Color32::YELLOW);
        }
        painter.text(
            s.left_top() + egui::vec2(6.0, 6.0),
            egui::Align2::LEFT_TOP,
            format!("{}×{}  ({}, {})", sel.width().round(), sel.height().round(), sel.min.x.round(), sel.min.y.round()),
            egui::FontId::proportional(14.0),
            egui::Color32::YELLOW,
        );
        if crop == Crop::Off {
            return;
        }

        // Smallest selection: the camera's minimum size, on its steps.
        let (step, min) = self.geometry().map_or(([8, 2, 8, 8], (8, 8)), |g| (g.step, g.min_size));
        let min = egui::vec2(min.0.div_ceil(step[0]) as f32 * step[0] as f32, min.1.div_ceil(step[1]) as f32 * step[1] as f32);
        // Which part of the selection is under the pointer (screen pixels).
        let hit = |p: egui::Pos2| -> Option<Grab> {
            const M: f32 = 8.0;
            if crop == Crop::Move {
                return s.contains(p).then_some(Grab { l: true, r: true, t: true, b: true });
            }
            if !s.expand(M).contains(p) {
                return None;
            }
            let (dl, dr, dt, db) = ((p.x - s.min.x).abs(), (p.x - s.max.x).abs(), (p.y - s.min.y).abs(), (p.y - s.max.y).abs());
            let g = Grab { l: dl <= M && dl <= dr, r: dr <= M && dr < dl, t: dt <= M && dt <= db, b: db <= M && db < dt };
            if g.l || g.r || g.t || g.b {
                Some(g)
            } else {
                s.contains(p).then_some(Grab { l: true, r: true, t: true, b: true })
            }
        };
        if let Some(p) = resp.hover_pos().filter(|_| self.crop_drag.is_none()) {
            ui.ctx().set_cursor_icon(match hit(p) {
                None if crop == Crop::Move => egui::CursorIcon::Default,
                None => egui::CursorIcon::Crosshair,
                Some(Grab { l: true, r: true, .. }) => egui::CursorIcon::Move,
                Some(Grab { l: true, t: true, .. } | Grab { r: true, b: true, .. }) => egui::CursorIcon::ResizeNwSe,
                Some(Grab { r: true, t: true, .. } | Grab { l: true, b: true, .. }) => egui::CursorIcon::ResizeNeSw,
                Some(Grab { l: true, .. } | Grab { r: true, .. }) => egui::CursorIcon::ResizeHorizontal,
                Some(_) => egui::CursorIcon::ResizeVertical,
            });
        }
        let Some(p) = resp.interact_pointer_pos() else { return };
        if resp.drag_started() && (crop == Crop::Full || hit(p).is_some()) {
            // Outside the selection: draw a new one.
            self.crop_drag = Some(CropDrag { grab: hit(p), from: to_cam(p), start: sel, sel });
        }
        let Some(d) = &mut self.crop_drag else { return };
        let q = to_cam(p);
        let delta = q - d.from;
        let a = d.start;
        d.sel = match d.grab {
            None => egui::Rect::from_two_pos(d.from, q),
            Some(Grab { l: true, r: true, .. }) => a.translate(egui::vec2(
                delta.x.clamp(-a.min.x, canvas.x - a.max.x),
                delta.y.clamp(-a.min.y, canvas.y - a.max.y),
            )),
            Some(g) => {
                let mut r = a;
                if g.l {
                    r.min.x = (a.min.x + delta.x).clamp(0.0, (a.max.x - min.x).max(0.0));
                }
                if g.r {
                    r.max.x = (a.max.x + delta.x).clamp(a.min.x + min.x, canvas.x);
                }
                if g.t {
                    r.min.y = (a.min.y + delta.y).clamp(0.0, (a.max.y - min.y).max(0.0));
                }
                if g.b {
                    r.max.y = (a.max.y + delta.y).clamp(a.min.y + min.y, canvas.y);
                }
                r
            }
        };
        if resp.drag_stopped() {
            let d = self.crop_drag.take().unwrap();
            // A click (no real drag) does not change the crop.
            if d.grab.is_some() || d.sel.width() * scale >= 5.0 && d.sel.height() * scale >= 5.0 {
                self.crop_to(d.sel, step, min, canvas);
            }
        }
    }

    /// Set the edited scenario's crop to `sel` (sensor pixels, after
    /// ReverseX/Y as SFNC counts offsets), snapped to the camera's steps and
    /// at least `min`, and try it right away.
    fn crop_to(&mut self, sel: egui::Rect, step: [u32; 4], min: egui::Vec2, sensor: egui::Vec2) {
        let snap = |v: f32, st: u32| ((v.max(0.0) / st as f32).round() as u32) * st;
        let down = |v: f32, st: u32| (v.max(0.0) as u32) / st * st;
        let (min_w, min_h) = (min.x as u32, min.y as u32);
        let (sw, sh) = (down(sensor.x, step[0]), down(sensor.y, step[1]));
        let w = snap(sel.width(), step[0]).clamp(min_w.min(sw), sw);
        let h = snap(sel.height(), step[1]).clamp(min_h.min(sh), sh);
        let x = snap(sel.min.x, step[2]).min(down(sensor.x - w as f32, step[2]));
        let y = snap(sel.min.y, step[3]).min(down(sensor.y - h as f32, step[3]));
        // Recording: move the camera's crop as it streams, size unchanged.
        if self.engine.is_recording() {
            let Some(c) = self.engine.scenario.as_ref().map(|s| s.camera.clone()) else { return };
            let x = snap(sel.min.x, step[2]).min(down(sensor.x - c.width as f32, step[2]));
            let y = snap(sel.min.y, step[3]).min(down(sensor.y - c.height as f32, step[3]));
            if (x, y) == (c.offset_x, c.offset_y) {
                return;
            }
            self.message = match self.engine.live(None, Some((x, y))) {
                Ok(()) => {
                    (self.edit.camera.offset_x, self.edit.camera.offset_y) = (x, y);
                    format!("Kivágás helye: ({x}, {y})")
                }
                Err(e) => format!("A kivágás most nem mozgatható: {e}"),
            };
            return;
        }
        // Main page: the running scenario only, nothing is saved. Settings:
        // the edited scenario (saved with it).
        let main = self.page == Page::Main;
        let Some(mut s) = (if main { self.engine.scenario.clone() } else { self.edited() }) else { return };
        let before = self.engine.scenario.clone();
        let c = &mut s.camera;
        if (c.width, c.height, c.offset_x, c.offset_y) == (w, h, x, y) {
            return;
        }
        (c.width, c.height, c.offset_x, c.offset_y) = (w, h, x, y);
        match self.engine.apply(&s) {
            Ok(()) => {
                // One active scenario: the settings page shows the crop too
                // (saved only with Mentés).
                let c = &mut self.edit.camera;
                (c.width, c.height, c.offset_x, c.offset_y) = (w, h, x, y);
                self.message = format!("Kivágás: {w}×{h} ({x}, {y})");
            }
            Err(e) => {
                // Back to the crop that worked, so the camera keeps running.
                if let Some(b) = before {
                    self.apply(&b);
                }
                self.message = format!("A kamera nem fogadta el a {w}×{h} kivágást: {e}. Visszaállítva.");
            }
        }
    }

    /// The camera the page works with: the running one on the main page,
    /// the edited scenario's on the settings page.
    fn page_device(&self) -> &Device {
        match (self.page, &self.engine.scenario) {
            (Page::Main, Some(a)) => &a.device,
            _ => &self.edit.device,
        }
    }

    /// Sensor size, smallest size and steps of the page's camera.
    fn geometry(&self) -> Option<Geometry> {
        match &self.geometry {
            Some((d, g)) if d == self.page_device() => Some(*g),
            _ => self.current_limits().map(|l| Geometry { sensor: l.sensor, min_size: l.min_size, step: l.step }),
        }
    }

    /// Limits read from the edited scenario's camera, if any.
    fn current_limits(&self) -> Option<&Limits> {
        self.limits.as_ref().filter(|(d, _)| d == self.page_device()).and_then(|(_, l)| l.as_ref().ok())
    }

    /// Read the limits (stops the camera ~2-3 s), then restart the camera
    /// with the scenario that ran before.
    fn read_limits(&mut self) {
        let Some(mut s) = self.edited() else { return };
        let before = self.engine.scenario.clone();
        let modes = self.engine.modes(&s);
        // Only USB3 Vision cameras tell their limits.
        let l = if s.device == Device::U3v { self.engine.limits(&s) } else { Err("a kamera nem adja meg a határait".into()) };
        self.message = match (&modes, &l) {
            (Err(e), _) => format!("A kamera nem kérdezhető le: {e}"),
            (Ok(m), _) if m.is_empty() => "A kamera nem adott rögzíthető módot.".into(),
            (Ok(m), Ok(_)) => format!("Kamera felmérve: {} mód, határok kiolvasva.", m.len()),
            (Ok(m), Err(_)) => format!("Kamera felmérve: {} mód.", m.len()),
        };
        // The edited format is one the camera does not have: take its first mode.
        let mut fixed = None;
        if let Ok(m) = &modes
            && let Some(first) = m.first()
            && !m.iter().any(|m| m.format == s.camera.pixel_format)
        {
            let old = s.camera.pixel_format;
            set_mode(&mut self.edit, first);
            set_mode(&mut s, first);
            self.message += &format!(" A kamera nem tud {} formátumot, átállítva: {}.", old.name(), mode_label(first));
            fixed = Some(s.clone());
        }
        self.limits = Some((s.device.clone(), l));
        self.modes = Some((s.device.clone(), modes));
        // Restart what ran before; the edited scenario if it was fixed
        // (or nothing ran).
        let before = before.filter(|b| fixed.is_none() || b.name != s.name);
        if let Some(b) = before.or(fixed).or(Some(s)) {
            if let Err(e) = self.engine.apply(&b) {
                self.message = format!("Hiba: {e}");
            }
        }
    }

    /// Operator page: scenario, record / pause / stop, export of the last
    /// recording. Details and preview look are folded away.
    /// Left panel: cards for the scenario, the live view, the details, the
    /// system and the recordings.
    fn left_panel(&mut self, ui: &mut egui::Ui) {
        card(ui, "Szcenárió", |ui| self.scenario_card(ui));
        card(ui, "Felvételek", |ui| {
            egui::ScrollArea::vertical().id_salt("recordings").max_height(300.0).show(ui, |ui| self.recordings_list(ui));
        });
        card(ui, "Élőkép", |ui| self.view_controls(ui));
        card(ui, "Részletek", |ui| self.details(ui));
        card(ui, "Rendszer", |ui| self.system_info(ui));
    }

    /// Scenario picker and the settings window's cog.
    fn scenario_card(&mut self, ui: &mut egui::Ui) {
        let recording = self.recording.is_some();
        // The active scenario (also while its camera is away).
        let current = self.engine.scenario.as_ref().map_or(self.edit.name.clone(), |s| s.name.clone());
        let mut chosen = None;
        ui.add_enabled_ui(!recording, |ui| {
            ui.horizontal(|ui| {
                egui::ComboBox::from_id_salt("scenario")
                    .width(ui.available_width() - 48.0)
                    .selected_text(egui::RichText::new(&current).size(16.0))
                    .show_ui(ui, |ui| {
                        for s in &self.scenarios {
                            if ui.selectable_label(s.name == current, &s.name).clicked() && s.name != current {
                                chosen = Some(s.clone());
                            }
                        }
                    });
                let open = self.page == Page::Settings;
                let cog = egui::Button::new(egui::RichText::new("⚙").size(20.0)).selected(open).min_size(egui::vec2(40.0, 30.0));
                if ui.add(cog).on_hover_text("Beállítások").clicked() {
                    self.page = if open { Page::Main } else { Page::Settings };
                }
            });
        });
        // A crop made on the preview lasts until the scenario is reloaded.
        let saved = self.scenarios.iter().find(|s| s.name == current).cloned();
        if let (Some(a), Some(saved)) = (&self.engine.scenario, &saved) {
            let roi = |c: &crate::scenario::CameraSettings| (c.width, c.height, c.offset_x, c.offset_y);
            if roi(&a.camera) != roi(&saved.camera) {
                let (cw, ch, x, y) = roi(&a.camera);
                ui.horizontal_wrapped(|ui| {
                    ui.small(format!("Kivágás: {cw}×{ch} ({x}, {y}), nincs mentve."));
                    if ui.add_enabled(!recording, egui::Button::new("Vissza").small()).clicked() {
                        chosen = Some(saved.clone());
                    }
                });
            }
        }
        if let Some(s) = chosen {
            self.apply(&s);
        }
    }

    /// CPU, memory and the recording disk as bars with numbers.
    fn system_info(&mut self, ui: &mut egui::Ui) {
        let si = &mut self.sys;
        if si.at.is_none_or(|t| t.elapsed() >= Duration::from_secs(1)) {
            si.at = Some(Instant::now());
            si.sys.refresh_cpu_usage();
            si.sys.refresh_memory();
            si.cpu = si.sys.global_cpu_usage();
            si.mem = (si.sys.used_memory(), si.sys.total_memory());
            let dir = self.engine.scenario.as_ref().map_or(self.edit.recording.output_dir.clone(), |s| s.recording.output_dir.clone());
            let base = dir.ancestors().find(|d| d.exists()).map(|d| d.to_path_buf());
            si.disk = base.and_then(|d| Some((fs4::available_space(&d).ok()?, fs4::total_space(&d).ok()?)));
        }
        let gb = |b: u64| b as f64 / 1e9;
        let cores = si.sys.cpus().len();
        bar(ui, "CPU", si.cpu / 100.0, format!("{:.0}% ({cores} mag)", si.cpu));
        let (used, total) = si.mem;
        bar(ui, "Memória", used as f32 / total.max(1) as f32, format!("{:.1} / {:.1} GB", gb(used), gb(total)));
        if let Some((free, total)) = si.disk {
            let used = total.saturating_sub(free);
            bar(ui, "Lemez", used as f32 / total.max(1) as f32, format!("{:.0} GB szabad / {:.0} GB", gb(free), gb(total)));
            // How long the free space lasts at the running recording's rate.
            if let Some(r) = &self.recording
                && let Ok(meta) = std::fs::metadata(&r.info.video)
                && r.elapsed().as_secs_f64() > 5.0
            {
                let rate = meta.len() as f64 / r.elapsed().as_secs_f64();
                ui.small(format!("{:.0} MB/s · kb. {:.0} perc fér még el", rate / 1e6, free as f64 / rate / 60.0));
            }
        }
        ui.ctx().request_repaint_after(Duration::from_secs(1));
    }

    /// Recordings newest first, one card each; the open card plays the
    /// recording and lists its exports.
    fn recordings_list(&mut self, ui: &mut egui::Ui) {
        if !self.rec.loaded {
            self.rec.folders = std::fs::read_to_string(FOLDERS_FILE)
                .unwrap_or_default()
                .lines()
                .filter(|l| !l.trim().is_empty())
                .map(|l| PathBuf::from(l.trim()))
                .collect();
        }
        if self.rec.refreshed.is_none_or(|t| t.elapsed() >= Duration::from_secs(10)) {
            self.refresh_recordings();
        }
        let recording = self.recording.is_some();
        let raw: Vec<usize> = (0..self.rec.list.len()).filter(|&i| !self.rec.list[i].export).collect();
        let exports_of = |video: &PathBuf, list: &[Recording]| -> Vec<usize> {
            let stem = video.file_stem().map(|s| format!("{}_", s.to_string_lossy())).unwrap_or_default();
            let dir = video.parent().map(|d| d.join(recordings::EXPORT_DIR));
            (0..list.len())
                .filter(|&i| list[i].export && list[i].video.parent() == dir.as_deref() && list[i].name().starts_with(&stem))
                .collect()
        };
        let mut claimed = std::collections::BTreeSet::new();
        let mut cards: Vec<(usize, Vec<usize>)> = raw
            .iter()
            .map(|&i| {
                let ex = exports_of(&self.rec.list[i].video, &self.rec.list);
                claimed.extend(ex.iter().copied());
                (i, ex)
            })
            .collect();
        // Exports whose recording is gone stand on their own.
        cards.extend((0..self.rec.list.len()).filter(|i| self.rec.list[*i].export && !claimed.contains(i)).map(|i| (i, vec![])));
        cards.sort_by(|a, b| self.rec.list[b.0].modified.cmp(&self.rec.list[a.0].modified));
        if cards.is_empty() {
            ui.weak("Még nincs felvétel.");
        }
        let viewing = self.rec.viewer.as_ref().map(|v| v.path.clone());
        let mut action = None;
        for (i, ex) in cards {
            let r = &self.rec.list[i];
            let (video, is_export, title) = (r.video.clone(), r.export, r.name());
            let when = chrono::DateTime::<chrono::Local>::from(r.modified).format("%Y-%m-%d %H:%M").to_string();
            let len = match (r.seconds, r.frames) {
                (Some(s), Some(f)) => format!("{} · {f} kép · {:.0} fps", hms(s), f as f64 / s),
                (_, Some(f)) => format!("{f} kép"),
                _ => String::new(),
            };
            let sub = format!("{when} · {len} · {:.2} GB", r.bytes as f64 / 1e9);
            let ex: Vec<(PathBuf, String)> =
                ex.iter().map(|&e| (self.rec.list[e].video.clone(), format!("{} · {:.2} GB", self.rec.list[e].name(), self.rec.list[e].bytes as f64 / 1e9))).collect();
            let open = self.rec.expanded.as_ref() == Some(&video);
            let fill = if open { ui.visuals().selection.bg_fill.gamma_multiply(0.35) } else { ui.visuals().faint_bg_color };
            egui::Frame::group(ui.style()).fill(fill).inner_margin(8.0).show(ui, |ui| {
                ui.set_width(ui.available_width());
                let head = ui.add_enabled_ui(!recording, |ui| {
                    ui.vertical(|ui| {
                        ui.label(egui::RichText::new(&title).strong());
                        ui.small(&sub);
                    })
                    .response
                });
                let resp = ui.interact(head.response.rect, ui.scope_id().with(("rec_card", i)), egui::Sense::click());
                if !recording && resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                    action = Some(if open { CardAction::Close } else { CardAction::Open(video.clone()) });
                }
                if !open {
                    return;
                }
                ui.separator();
                for (x, label) in &ex {
                    let on = viewing.as_ref() == Some(x);
                    if ui.selectable_label(on, format!("▶ {label}")).on_hover_text("Lejátszás").clicked() {
                        action = Some(CardAction::Play(x.clone()));
                    }
                }
                if !ex.is_empty() && viewing.as_ref() != Some(&video) && ui.small_button("▶ az eredeti felvétel").clicked() {
                    action = Some(CardAction::Play(video.clone()));
                }
                if !is_export {
                    self.export_controls(ui, &video, &mut action);
                }
            });
            ui.add_space(4.0);
        }
        match action {
            Some(CardAction::Open(v)) => {
                self.rec.expanded = Some(v.clone());
                self.rec.confirm_delete = None;
                self.open_viewer(v);
                self.play(true);
            }
            Some(CardAction::Play(v)) => {
                self.open_viewer(v);
                self.play(true);
            }
            Some(CardAction::Close) => {
                self.rec.expanded = None;
                self.rec.viewer = None;
            }
            Some(CardAction::Export(v)) => {
                let export = self.engine.scenario.as_ref().map(|s| s.export.clone()).unwrap_or_default();
                self.export_result.remove(&v);
                self.rec.queue.push_back((v, export));
            }
            Some(CardAction::Delete(v)) => {
                if self.rec.viewer.as_ref().is_some_and(|x| x.path == v) {
                    self.rec.viewer = None;
                }
                match recordings::delete(&v) {
                    Ok(()) => self.message = format!("Törölve: {}", v.display()),
                    Err(e) => self.message = format!("Törlés: {e}"),
                }
                self.rec.expanded = None;
                self.rec.confirm_delete = None;
                self.refresh_recordings();
            }
            None => {}
        }
    }

    /// Export state and buttons of an open recording card.
    fn export_controls(&mut self, ui: &mut egui::Ui, video: &PathBuf, action: &mut Option<CardAction>) {
        let export = self.engine.scenario.as_ref().map(|s| s.export.clone()).unwrap_or_default();
        let closing = self.last_rec.as_ref().is_some_and(|(v, st)| v == video && !st.finished.load(Relaxed));
        if closing {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("A felvétel lezárása folyamatban…");
            });
            return;
        }
        if let Some((_, st)) = self.last_rec.as_ref().filter(|(v, _)| v == video) {
            let dropped = st.dropped.load(Relaxed);
            if dropped > 0 {
                ui.colored_label(RED, format!("{dropped} képkocka elveszett!"));
            }
        }
        if let Some(j) = self.rec.job.as_ref().filter(|j| j.video == *video) {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(format!("Export folyamatban… {:.0} s", j.started.elapsed().as_secs_f64()));
            });
            ui.small(j.last_line.lock().unwrap().clone());
            if ui.button("Megszakítás").clicked() {
                self.cancel_export();
            }
            return;
        }
        if self.rec.queue.iter().any(|(v, _)| v == video) {
            ui.label("Export sorban…");
            return;
        }
        match self.export_result.get(video) {
            Some(Ok(out)) => {
                ui.colored_label(GREEN, format!("✔ Export kész: {}", file_name(&out.display().to_string())));
            }
            Some(Err(e)) => {
                ui.colored_label(RED, format!("Export hiba: {e}"));
            }
            None => {}
        }
        ui.horizontal(|ui| {
            let b = egui::Button::new(egui::RichText::new("⇪  Export").strong().color(egui::Color32::WHITE)).fill(BLUE).min_size(egui::vec2(120.0, 32.0));
            if ui.add_enabled(export.method != "magnify" && video.exists(), b).on_hover_text(export_label(&export)).clicked() {
                *action = Some(CardAction::Export(video.clone()));
            }
            if self.rec.confirm_delete.as_ref() == Some(video) {
                if ui.button(egui::RichText::new("Végleg törlöm").color(RED)).clicked() {
                    *action = Some(CardAction::Delete(video.clone()));
                }
                if ui.button("Mégse").clicked() {
                    self.rec.confirm_delete = None;
                }
            } else if ui.add_enabled(!self.busy(video), egui::Button::new("Törlés").min_size(egui::vec2(80.0, 32.0))).clicked() {
                self.rec.confirm_delete = Some(video.clone());
            }
        });
        ui.small(export_label(&export));
    }

    fn start_recording(&mut self) {
        let name = if self.rec_name.trim().is_empty() {
            chrono::Local::now().format("rec_%Y%m%d_%H%M%S").to_string()
        } else {
            self.rec_name.trim().to_string()
        };
        match self.engine.start_recording(&name) {
            Ok(info) => {
                self.message = format!("Felvétel: {}", info.video);
                let lost_before = self.engine.stats().lost_at_camera.load(Relaxed).max(self.engine.camera_errors());
                self.recording =
                    Some(Rec { info, started: Instant::now(), paused_since: None, paused_total: Duration::ZERO, lost_before });
            }
            Err(e) => self.message = format!("Hiba: {e}"),
        }
    }

    fn stop_recording(&mut self) {
        self.engine.stop_recording();
        if let Some(Rec { info, .. }) = self.recording.take() {
            self.message = format!(
                "Felvétel leállítva: {} – {} képkocka, {} eldobva",
                info.video,
                info.stats.queued.load(Relaxed),
                info.stats.dropped.load(Relaxed)
            );
            self.last_rec = Some((PathBuf::from(&info.video), info.stats.clone()));
            self.rec.expanded = Some(PathBuf::from(&info.video));
        }
        self.rec_name.clear();
        self.refresh_recordings();
    }

    /// Camera state in one colour and sentence.
    fn health(&self) -> (egui::Color32, String) {
        if self.engine.waiting_for_camera() {
            return (ORANGE, "Várakozás a kamerára…".into());
        }
        if self.engine.scenario.is_none() {
            return (RED, "A kamera nem fut".into());
        }
        // A discarded payload is also a gap in the frame IDs: count it once.
        let lost = self.engine.stats().lost_at_camera.load(Relaxed).max(self.engine.camera_errors());
        if let Some(r) = &self.recording {
            let st = &r.info.stats;
            if st.error.lock().unwrap().is_some() {
                return (RED, "Felvételi hiba!".into());
            }
            // Only what happened since the start (a pause is not a loss: the
            // camera keeps sending, the frames are just not written).
            if st.dropped.load(Relaxed) > 0 || lost > r.lost_before {
                return (RED, "Képkocka veszett el a felvételből!".into());
            }
            return (GREEN, format!("Felvétel rendben · {}", self.engine.camera_name()));
        }
        if lost > 0 {
            return (ORANGE, format!("A kamera {lost} képkockát vesztett · {}", self.engine.camera_name()));
        }
        (GREEN, format!("Kamera rendben · {}", self.engine.camera_name()))
    }

    /// Bottom bar: a tape recorder's REC, play, pause and stop, then the
    /// camera state and the last message.
    fn transport_bar(&mut self, ui: &mut egui::Ui) {
        let recording = self.recording.is_some();
        let paused = self.recording.as_ref().is_some_and(|r| r.paused_since.is_some());
        let viewer = !recording && self.rec.viewer.is_some();
        let playing = viewer && self.rec.viewer.as_ref().is_some_and(|v| v.player.is_some());
        ui.horizontal(|ui| {
            ui.add_enabled_ui(!recording, |ui| {
                ui.add(egui::TextEdit::singleline(&mut self.rec_name).hint_text("felvétel neve (üresen: dátum)").desired_width(200.0));
            });
            ui.add_space(6.0);
            let ready = self.engine.scenario.is_some();
            if transport(ui, Icon::Rec, ready && (!recording || paused), recording && !paused, "Felvétel").clicked() {
                if recording {
                    if let Some(r) = &mut self.recording {
                        r.set_paused(false);
                    }
                } else {
                    // From any mode: back to the live view and record.
                    self.rec.viewer = None;
                    self.rec.expanded = None;
                    self.page = Page::Main;
                    self.start_recording();
                }
            }
            if transport(ui, Icon::Play, viewer && !playing, playing, "Lejátszás").clicked() {
                self.play(true);
            }
            if transport(ui, Icon::Pause, (recording && !paused) || playing, paused, "Szünet").clicked() {
                if let Some(r) = &mut self.recording {
                    r.set_paused(true);
                } else {
                    self.play(false);
                }
            }
            if transport(ui, Icon::Stop, recording || viewer, false, "Stop").clicked() {
                if recording {
                    self.stop_recording();
                } else {
                    // Back to the live view.
                    self.rec.viewer = None;
                }
            }
            ui.add_space(10.0);
            if let Some(r) = &self.recording {
                let t = r.elapsed().as_secs_f64();
                let color = if paused { ORANGE } else { RED };
                ui.label(egui::RichText::new(format!("{} {}", if paused { "⏸" } else { "●" }, hms(t))).size(22.0).strong().color(color));
                ui.label(format!("{} · {} kép", file_name(&r.info.video), r.info.stats.written.load(Relaxed)));
            }
        });
        // Second line: camera state, last message, background work.
        ui.horizontal(|ui| {
            let (color, text) = self.health();
            let (rect, _) = ui.allocate_exact_size(egui::vec2(16.0, 16.0), egui::Sense::hover());
            ui.painter().circle_filled(rect.center(), 7.0, color);
            ui.label(egui::RichText::new(text).strong());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.weak(concat!("v", env!("CARGO_PKG_VERSION")));
                if let Some(j) = &self.rec.job {
                    let more = if self.rec.queue.is_empty() { String::new() } else { format!(" (+{} vár)", self.rec.queue.len()) };
                    ui.label(format!("Export: {}{more}", file_name(&j.video.display().to_string())));
                    ui.spinner();
                }
                let fin = self.engine.finalizing();
                if fin > 0 {
                    ui.label(format!("Lezárás: {fin}"));
                    ui.spinner();
                }
                if !self.message.is_empty() {
                    ui.separator();
                    ui.add(egui::Label::new(&self.message).truncate());
                }
            });
        });
    }

    /// Start (resume) or pause the playback of the open recording.
    fn play(&mut self, on: bool) {
        let Some(v) = &mut self.rec.viewer else { return };
        if !on {
            v.player = None;
            return;
        }
        if v.error.is_some() && v.texture.is_none() {
            return;
        }
        let end = (v.info.duration - 1.0 / v.info.fps).max(0.0);
        if v.t >= end - 0.05 {
            v.t = 0.0;
        }
        let speed = v.real_fps.map_or(1.0, |f| v.info.fps / f);
        v.player = Some(Player::start(v.path.clone(), v.info.clone(), v.t, speed));
    }

    /// Technical numbers of the camera and the running recording.
    fn details(&mut self, ui: &mut egui::Ui) {
        let st = self.engine.stats();
        egui::Grid::new("details").num_columns(2).spacing([12.0, 4.0]).max_col_width(ui.available_width() - 140.0).show(ui, |ui| {
            if let Some(a) = self.engine.scenario.clone() {
                ui.label("A kamera küld");
                actual_mode(ui, &a, &self.engine.actual(), !self.engine.hw_crop());
                ui.end_row();
            }
            let mut row = |k: &str, v: String| {
                ui.label(k);
                ui.add(egui::Label::new(v).wrap());
                ui.end_row();
            };
            row("Kamera", self.engine.camera_name());
            let lost = st.lost_at_camera.load(Relaxed);
            let bad = self.engine.camera_errors();
            row("Fogadott kép", format!("{} · elveszett {lost}{}", st.received.load(Relaxed), if bad > 0 { format!(" · hibás {bad}") } else { String::new() }));
            if let Some(c) = self.engine.scenario.as_ref().map(|s| &s.camera) {
                let max = self.engine.max_fps().map_or(String::new(), |m| format!(" (max. {m:.1})"));
                row("Kért kép", format!("{}×{} ({}, {}) {} · {} fps{max}", c.width, c.height, c.offset_x, c.offset_y, c.pixel_format.name(), c.frame_rate));
            }
            if let Some(r) = &self.recording {
                let s = &r.info.stats;
                row(
                    "Felvétel",
                    format!(
                        "{} írva · {} sorban · {} eldobva",
                        s.written.load(Relaxed),
                        s.queued.load(Relaxed) - s.written.load(Relaxed),
                        s.dropped.load(Relaxed)
                    ),
                );
                if let Some(enc) = *s.encoder.lock().unwrap() {
                    row("Kódoló", enc.to_string());
                }
                if let Some(n) = s.format_note.lock().unwrap().as_ref() {
                    row("Formátum", n.clone());
                }
                if let Some(e) = s.error.lock().unwrap().as_ref() {
                    row("Hiba", e.clone());
                }
            }
        });
    }

    /// Settings page: scenario bar on top, then the settings in collapsible
    /// groups, tests at the end.
    fn settings_page(&mut self, ui: &mut egui::Ui) {
        let busy = self.recording.is_some() || self.test.is_some();
        ui.add_enabled_ui(!busy, |ui| self.scenario_bar(ui));
        ui.separator();
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.add_enabled_ui(!busy, |ui| {
                self.camera_section(ui);
                self.image_section(ui);
                self.exposure_section(ui);
                self.dark_section(ui);
                self.recording_section(ui);
                self.preview_section(ui);
                section(ui, "Haladó: további kamerabeállítások", false, |ui| {
                    ui.small("GenICam jellemzők JSON-ban, név → érték, pl. {\"DeviceLinkThroughputLimit\": 450000000}");
                    ui.add(egui::TextEdit::multiline(&mut self.extra_json).code_editor().desired_rows(3).desired_width(f32::INFINITY));
                });
            });
            self.tests_section(ui, busy);
        });
    }

    /// Pick / create / save / delete / try the edited scenario.
    fn scenario_bar(&mut self, ui: &mut egui::Ui) {
        egui::Grid::new("scenario_bar").num_columns(2).min_col_width(LABEL_W).spacing([12.0, 8.0]).show(ui, |ui| {
            ui.label(egui::RichText::new("Szcenárió").strong());
            ui.horizontal(|ui| {
                let mut pick = None;
                egui::ComboBox::from_id_salt("edit_scenario").width(220.0).selected_text(&self.edit.name).show_ui(ui, |ui| {
                    for s in &self.scenarios {
                        if ui.selectable_label(s.name == self.edit.name, &s.name).clicked() {
                            pick = Some(s.clone());
                        }
                    }
                });
                // The chosen one becomes the active scenario everywhere.
                if let Some(s) = pick {
                    self.apply(&s);
                }
                if ui.button("Új").on_hover_text("Új szcenárió a mostani beállításokból").clicked() {
                    self.edit = Scenario { name: "uj_szcenario".into(), ..self.edit.clone() };
                }
            });
            ui.end_row();
            ui.label("Név");
            ui.add(egui::TextEdit::singleline(&mut self.edit.name).desired_width(f32::INFINITY));
            ui.end_row();
        });
        ui.horizontal(|ui| {
            if ui.button("Kipróbálás").on_hover_text("A kamera átáll ezekre a beállításokra, mentés nélkül.").clicked()
                && let Some(s) = self.edited()
            {
                self.apply(&s);
            }
            if ui.button("Mentés").clicked()
                && let Some(s) = self.edited()
            {
                let saved = scenario::save(&self.dir, &s);
                self.reload();
                // The saved version is the active one (new name or settings
                // not yet tried: the camera switches to it).
                if saved.is_ok() && self.engine.scenario.as_ref() != Some(&s) {
                    self.apply(&s);
                }
                self.message = match saved {
                    Ok(p) => format!("Mentve és aktív: {}", p.display()),
                    Err(e) => format!("Hiba mentéskor: {e}"),
                };
            }
            // Unsaved changes of the active scenario.
            let stored = self.scenarios.iter().find(|x| x.name == self.edit.name);
            let differs = stored.is_none_or(|x| x.camera != self.edit.camera || x.recording != self.edit.recording || x.export != self.edit.export || x.preview_lut != self.edit.preview_lut || x.device != self.edit.device);
            if differs {
                ui.colored_label(ORANGE, "● nem mentett módosítás");
            }
            if !self.confirm_delete_scenario {
                if ui.button("Törlés").clicked() {
                    self.confirm_delete_scenario = true;
                }
            } else {
                ui.colored_label(RED, format!("Törlöd: {}?", self.edit.name));
                if ui.button("Igen").clicked() {
                    self.message = match scenario::delete(&self.dir, &self.edit.name) {
                        Ok(()) => format!("Törölve: {}", self.edit.name),
                        Err(e) => format!("Hiba törléskor: {e}"),
                    };
                    self.confirm_delete_scenario = false;
                    self.reload();
                    // Another scenario becomes active.
                    let next = self.scenarios.first().cloned().unwrap_or_default();
                    self.apply(&next);
                }
                if ui.button("Mégse").clicked() {
                    self.confirm_delete_scenario = false;
                }
            }
        });
    }

    /// Modes of the edited scenario's camera, if read.
    fn known_modes(&self) -> &[Mode] {
        match &self.modes {
            Some((d, Ok(m))) if *d == self.edit.device => m,
            _ => &[],
        }
    }

    /// Best discovery result for a format and size on the edited camera:
    /// `None` not tested.
    fn tested(&self, f: PixelFormat, w: u32, h: u32) -> Option<Level> {
        let rank = |l: Level| match l {
            Level::Ok => 2,
            Level::Short => 1,
            Level::Fail => 0,
        };
        self.probe_results
            .iter()
            .filter(|(s, _)| s.device == self.edit.device && (s.camera.pixel_format, s.camera.width, s.camera.height) == (f, w, h))
            .map(|(_, v)| v.level)
            .max_by_key(|&l| rank(l))
    }

    /// The edited camera crops in software (UVC, or a U3V camera without ROI).
    fn sw_crop(&self) -> bool {
        match &self.engine.scenario {
            Some(a) if a.device == self.edit.device => !self.engine.hw_crop(),
            _ => !self.edit.device.hw_crop(),
        }
    }

    fn camera_section(&mut self, ui: &mut egui::Ui) {
        let max_fps = self.current_limits().and_then(|l| l.max_fps);
        let modes = self.known_modes().to_vec();
        let status: Vec<Option<Level>> = modes.iter().map(|m| self.tested(m.format, m.width, m.height)).collect();
        let modes_err = match &self.modes {
            Some((d, Err(e))) if *d == self.edit.device => Some(e.clone()),
            _ => None,
        };
        let sw_crop = self.sw_crop();
        // What the running camera really sends, if it is the edited one.
        let actual = self.engine.scenario.as_ref().filter(|a| a.device == self.edit.device).map(|a| (a.clone(), self.engine.actual()));
        let mut reread = false;
        section(ui, "Kamera", true, |ui| {
            grid(ui, "cam", |ui| {
                let e = &mut self.edit;
                ui.label("Eszköz");
                ui.horizontal(|ui| {
                    egui::ComboBox::from_id_salt("device").width(260.0).selected_text(e.device.label()).show_ui(ui, |ui| {
                        for d in &self.devices {
                            ui.selectable_value(&mut e.device, d.clone(), d.label());
                        }
                    });
                    if ui.button("⟳").on_hover_text("Kameralista frissítése, és a kamera módjainak újraolvasása").clicked() {
                        self.devices = find_devices();
                        reread = true;
                    }
                });
                ui.end_row();

                // A mode the camera reported (or accepted): format, size, fps.
                if !modes.is_empty() {
                    ui.label("Kameramód");
                    let cap = if sw_crop { e.camera.capture_size.unwrap_or((e.camera.width, e.camera.height)) } else { (0, 0) };
                    let current = modes.iter().position(|m| m.format == e.camera.pixel_format && (!sw_crop || (m.width, m.height) == cap));
                    let text = match current {
                        Some(i) if sw_crop => mode_label(&modes[i]),
                        _ => format!("{} {}×{}", e.camera.pixel_format.name(), e.camera.width, e.camera.height),
                    };
                    egui::ComboBox::from_id_salt("mode").width(260.0).selected_text(text).show_ui(ui, |ui| {
                        for (m, st) in modes.iter().zip(&status) {
                            let (mark, tip) = match (st, m.reported) {
                                (Some(Level::Ok), _) => ("✔ ", "A felvételi teszten átment."),
                                (Some(Level::Short), _) => ("⚠ ", "A kódoló lassabb a kameránál: rövid felvételre jó."),
                                (Some(Level::Fail), _) => ("✖ ", "A felvételi teszten nem ment át."),
                                (None, true) => ("", "A kamera megadta; felvételi teszttel még nincs kipróbálva."),
                                (None, false) => ("? ", "Megnyitáskor elfogadta; felvételi teszttel még nincs kipróbálva."),
                            };
                            let ok = *st != Some(Level::Fail);
                            let label = format!("{mark}{}", mode_label(m));
                            if ui.add_enabled(ok, egui::Button::selectable(Some(m) == current.map(|i| &modes[i]), label)).on_hover_text(tip).on_disabled_hover_text(tip).clicked() {
                                set_mode(e, m);
                                e.camera.frame_rate = best_rate(&self.probe_results, &e.device, m).unwrap_or(m.fps);
                            }
                        }
                    });
                    ui.end_row();
                }

                ui.label("Pixelformátum");
                egui::ComboBox::from_id_salt("pixfmt").width(260.0).selected_text(e.camera.pixel_format.name()).show_ui(ui, |ui| {
                    for f in PixelFormat::ALL {
                        let of: Vec<usize> = (0..modes.len()).filter(|&i| modes[i].format == f).collect();
                        let has = modes.is_empty() || !of.is_empty();
                        let works = of.is_empty() || of.iter().any(|&i| status[i] != Some(Level::Fail));
                        let tip = if !has { "A kamera ezt a formátumot nem tudja." } else if !works { "Felvételi teszten nem ment át." } else { "" };
                        let r = ui.add_enabled(has && works, egui::Button::selectable(e.camera.pixel_format == f, f.name()));
                        if !tip.is_empty() {
                            r.clone().on_disabled_hover_text(tip);
                        }
                        if r.clicked() {
                            // The format's largest working mode, if known.
                            match of.iter().find(|&&i| status[i] != Some(Level::Fail)) {
                                Some(&i) => {
                                    set_mode(e, &modes[i]);
                                    e.camera.frame_rate = best_rate(&self.probe_results, &e.device, &modes[i]).unwrap_or(modes[i].fps);
                                }
                                None => e.camera.pixel_format = f,
                            }
                        }
                    }
                });
                ui.end_row();

                ui.label("Képfrekvencia");
                let mode = modes.iter().find(|m| {
                    m.format == e.camera.pixel_format && (!sw_crop || Some((m.width, m.height)) == e.camera.capture_size)
                });
                ui.horizontal(|ui| {
                    ui.add(egui::DragValue::new(&mut e.camera.frame_rate).range(0.1..=1000.0).speed(0.5).suffix(" fps"));
                    match mode {
                        // A UVC camera takes only the rates it lists.
                        Some(m) if !m.rates.is_empty() => {
                            for &r in &m.rates {
                                if ui.selectable_label((e.camera.frame_rate - r).abs() < 1e-3, format!("{r}")).clicked() {
                                    e.camera.frame_rate = r;
                                }
                            }
                        }
                        _ => {
                            if let Some(m) = max_fps.or(mode.map(|m| m.fps))
                                && ui
                                    .button(format!("max. {m:.1}"))
                                    .on_hover_text("A kamera legnagyobb fps-e ezzel a formátummal, kivágással és expozícióval.")
                                    .clicked()
                            {
                                e.camera.frame_rate = m;
                            }
                        }
                    }
                });
                ui.end_row();

                if let Some((a, act)) = &actual {
                    ui.label("A kamera most küld");
                    actual_mode(ui, a, act, sw_crop);
                    ui.end_row();
                }
            });
            if let Some(e) = &modes_err {
                ui.colored_label(ORANGE, format!("A kamera módjai nem olvashatók: {e}"));
            } else if modes.iter().any(|m| !m.reported) {
                ui.small("A kamera nem sorolta fel a módjait: a rendszer megnyitással kitapogatta, mit fogad el (?). A Kamera felderítése felvétellel is kipróbálja őket.");
            }
        });
        if reread {
            self.modes = None;
            self.limits = None;
        }
    }

    fn image_section(&mut self, ui: &mut egui::Ui) {
        let sw_crop = self.sw_crop();
        let mut geo = self.geometry();
        if sw_crop {
            // Crop within the size the camera is opened with.
            let c = &mut self.edit.camera;
            let cap = *c.capture_size.get_or_insert((c.width, c.height));
            let step = frame_crop_step(c.pixel_format);
            geo = Some(Geometry { sensor: cap, min_size: (16, 16), step: [step; 4] });
        }
        section(ui, "Kép és kivágás", true, |ui| {
            if sw_crop {
                let c = &self.edit.camera;
                if c.pixel_format == PixelFormat::Mjpeg {
                    ui.colored_label(ORANGE, "A kamera nem tud vágni, és az MJPEG szoftveresen sem vágható: csak a teljes kép használható.");
                } else {
                    ui.small("A kamera nem tud vágni: a program vágja ki szoftveresen a képből, ugyanúgy, mint a kamera.");
                }
            }
            grid(ui, "img", |ui| {
                let c = &mut self.edit.camera;
                let (step, sensor, min) = geo.map_or(([1; 4], (16384, 16384), (8, 8)), |g| (g.step, g.sensor, g.min_size));
                ui.label("Méret");
                ui.horizontal(|ui| {
                    ui.add(egui::DragValue::new(&mut c.width).range(min.0..=sensor.0).speed(step[0]));
                    ui.label("×");
                    ui.add(egui::DragValue::new(&mut c.height).range(min.1..=sensor.1).speed(step[1]));
                    if geo.is_some() && ui.button("Teljes").on_hover_text(format!("{}×{}", sensor.0, sensor.1)).clicked() {
                        (c.width, c.height, c.offset_x, c.offset_y) = (sensor.0, sensor.1, 0, 0);
                    }
                });
                ui.end_row();

                ui.label("Eltolás (X, Y)");
                ui.horizontal(|ui| {
                    ui.add(egui::DragValue::new(&mut c.offset_x).range(0..=sensor.0.saturating_sub(c.width)).speed(step[2]));
                    ui.add(egui::DragValue::new(&mut c.offset_y).range(0..=sensor.1.saturating_sub(c.height)).speed(step[3]));
                    if geo.is_some() && ui.button("Középre").clicked() {
                        c.offset_x = sensor.0.saturating_sub(c.width) / 2;
                        c.offset_y = sensor.1.saturating_sub(c.height) / 2;
                    }
                });
                ui.end_row();
                // The camera only takes multiples of its steps.
                if geo.is_some() {
                    c.width = (c.width / step[0] * step[0]).min(sensor.0);
                    c.height = (c.height / step[1] * step[1]).min(sensor.1);
                    c.offset_x = (c.offset_x / step[2] * step[2]).min(sensor.0 - c.width);
                    c.offset_y = (c.offset_y / step[3] * step[3]).min(sensor.1 - c.height);
                }

                ui.label("Tükrözés");
                ui.horizontal(|ui| {
                    ui.checkbox(&mut c.reverse_x, "vízszintes");
                    ui.checkbox(&mut c.reverse_y, "függőleges");
                })
                .response
                .on_hover_text("A kamera tükröz, a felvételre is hat. Mindkettő: 180°-os forgatás.");
                ui.end_row();
            });
            ui.small("Kivágás egérrel: húzz ki egy téglalapot az élőképen, vagy fogd meg a szélét.");
        });
    }

    fn exposure_section(&mut self, ui: &mut egui::Ui) {
        let limits = self.current_limits().cloned();
        section(ui, "Expozíció és erősítés", true, |ui| {
            grid(ui, "exp", |ui| {
                let c = &mut self.edit.camera;
                let (exp, gain) = limits.as_ref().map_or(((1.0, 10_000_000.0), (0.0, 48.0)), |l| (l.exposure_us, l.gain_db));
                ui.label("Automatikus");
                ui.checkbox(&mut c.auto_exposure, "a kamera állítja")
                    .on_hover_text("A felvételre is hat: a nyers jel fényessége menet közben változhat.");
                ui.end_row();
                if c.auto_exposure {
                    let period = 1e6 / c.frame_rate;
                    ui.label("Expozíció tartomány");
                    ui.horizontal(|ui| {
                        let (lo, hi) = &mut c.auto_exposure_range;
                        ui.add(egui::DragValue::new(lo).range(0.0..=*hi).speed(10.0).suffix(" µs"));
                        ui.label("–");
                        ui.add(egui::DragValue::new(hi).range(*lo..=exp.1).speed(10.0).suffix(" µs"));
                    })
                    .response
                    .on_hover_text(format!("Ezen belül állíthat az automatika. Egy képkocka ideje {period:.0} µs, a felső határ ne legyen nagyobb, különben az fps nem tartható."));
                    ui.end_row();
                    ui.label("Erősítés tartomány");
                    ui.horizontal(|ui| {
                        let (lo, hi) = &mut c.auto_gain_range;
                        ui.add(egui::DragValue::new(lo).range(0.0..=*hi).speed(0.1).suffix(" dB"));
                        ui.label("–");
                        ui.add(egui::DragValue::new(hi).range(*lo..=gain.1).speed(0.1).suffix(" dB"));
                    })
                    .response
                    .on_hover_text("Nagyobb erősítés: világosabb, de zajosabb kép.");
                    ui.end_row();
                    ui.label("Célfényerő");
                    ui.add(egui::Slider::new(&mut c.auto_target, 5.0..=80.0).fixed_decimals(0).suffix(" %"))
                        .on_hover_text("Erre az átlagos fényerőre állít az automatika. Ha a világos részek beégnek, csökkentsd. A kamera alapértéke kb. 50%, ez világos ablakos jelenetnél a kép ötödét beégeti; 25%-nál kb. 1%-ot. Az élőkép automatikus fényereje a sötétebb képet is kivilágosítja.");
                    ui.end_row();
                } else {
                    ui.label("Expozíció");
                    ui.add(egui::DragValue::new(&mut c.exposure_us).range(exp.0..=exp.1).speed(10.0).suffix(" µs"));
                    ui.end_row();
                    ui.label("Erősítés");
                    ui.add(egui::DragValue::new(&mut c.gain_db).range(gain.0..=gain.1).speed(0.1).suffix(" dB"));
                    ui.end_row();
                }
            });
        });
    }

    fn recording_section(&mut self, ui: &mut egui::Ui) {
        section(ui, "Felvétel és export", true, |ui| {
            grid(ui, "rec", |ui| {
                let e = &mut self.edit;
                ui.label("Kódoló");
                let label = encoder::PROFILES.iter().find(|p| p.id == e.recording.encoder).map_or("auto (veszteségmentes)", |p| p.label);
                egui::ComboBox::from_id_salt("encoder").width(320.0).selected_text(label).show_ui(ui, |ui| {
                    ui.selectable_value(&mut e.recording.encoder, "auto".to_string(), "auto (veszteségmentes)");
                    for p in encoder::available() {
                        ui.selectable_value(&mut e.recording.encoder, p.id.to_string(), p.label);
                    }
                });
                ui.end_row();

                ui.label("8 bites görbe");
                let deep = e.camera.pixel_format.is_single_channel() && e.camera.pixel_format.bits() > 8;
                ui.add_enabled(deep, egui::Checkbox::new(&mut e.recording.curve8, "10/12 bites kép 8 biten, a zajhoz illesztett görbével"))
                    .on_hover_text("A teljes dinamikatartomány megmarad 8 biten: a szinteket a szenzor zajához igazítja (sötétben sűrűn, világosban ritkán), a kerekítés a zajt kb. 5%-kal növeli. Az export és a lejátszó visszaalakítja lineárisra. Fele akkora adat, mint a 16 bites tárolás.")
                    .on_disabled_hover_text("10 vagy 12 bites Bayer/Mono formátumhoz.");
                ui.end_row();

                ui.label("Mappa");
                let mut dir = e.recording.output_dir.display().to_string();
                if ui.add(egui::TextEdit::singleline(&mut dir).desired_width(320.0)).changed() {
                    e.recording.output_dir = dir.into();
                }
                ui.end_row();
            });
            ui.add_space(4.0);
            ui.label(egui::RichText::new("A főoldal Export gombja:").strong());
            export_editor(ui, &mut self.edit.export, "scenario_export");
        });
    }

    fn preview_section(&mut self, ui: &mut egui::Ui) {
        section(ui, "Élőkép", false, |ui| {
            grid(ui, "lut", |ui| {
                let e = &mut self.edit;
                ui.label("S-Log LUT");
                let current = e.preview_lut.clone().unwrap_or_else(|| "nincs".into());
                egui::ComboBox::from_id_salt("lut").width(320.0).selected_text(current).show_ui(ui, |ui| {
                    ui.selectable_value(&mut e.preview_lut, None, "nincs");
                    for name in lut::list(&self.lut_dir) {
                        ui.selectable_value(&mut e.preview_lut, Some(name.clone()), name);
                    }
                });
                ui.end_row();
            });
            self.view_controls(ui);
        });
    }

    /// Limits, recording test and camera discovery, with their results.
    fn tests_section(&mut self, ui: &mut egui::Ui, busy: bool) {
        let limits = self.current_limits().cloned();
        let mut load = None;
        section(ui, "Tesztek és felmérés", false, |ui| {
            ui.add_enabled_ui(!busy, |ui| {
                ui.horizontal_wrapped(|ui| {
                    if ui
                        .button("Határok felmérése")
                        .on_hover_text("Felvétel nélkül, pár másodperc alatt kiolvassa a kamerából: szenzorméret, expozíció- és erősítéstartomány, legnagyobb fps formátumonként és magasságonként.")
                        .clicked()
                    {
                        self.message = "Kamera határainak felmérése…".into();
                        self.limits_pending = true;
                    }
                    if ui
                        .button("Felvételi teszt")
                        .on_hover_text("Rögzít a megadott ideig, és ellenőrzi, hogy nem veszett-e el képkocka, és bírja-e a kódoló a tempót. A tesztfájlokat utána törli.")
                        .clicked()
                        && let Some(s) = self.edited()
                    {
                        self.test_result = None;
                        match probe::Test::start(&mut self.engine, &s, self.test_secs) {
                            Ok(t) => self.test = Some((t, s)),
                            Err(v) => self.test_result = Some(v),
                        }
                    }
                    if ui
                        .button("Kamera felderítése")
                        .on_hover_text("Minden módot felvételi teszttel is kipróbál (lassú). Legtöbbször elég a Határok felmérése.")
                        .clicked()
                        && let Some(s) = self.edited()
                    {
                        self.start_probe(s);
                    }
                    ui.label("tesztidő");
                    ui.add(egui::DragValue::new(&mut self.test_secs).range(2.0..=600.0).suffix(" s"));
                });
            });
            if let Some((t, s)) = &self.test {
                ui.horizontal(|ui| {
                    ui.spinner();
                    let left = self.probe_queue.len();
                    let more = if left > 0 { format!(", még {left} van hátra") } else { String::new() };
                    ui.label(format!("Teszt: {} ({:.0}/{:.0} s){more}", s.name, t.elapsed(), self.test_secs));
                    if left > 0 && ui.button("Leállítás").on_hover_text("A futó teszt még befejeződik.").clicked() {
                        self.probe_queue.clear();
                    }
                });
            }
            if let Some(v) = &self.test_result {
                ui.colored_label(level_color(v.level), &v.text);
            }
            self.probe_table(ui);
            match self.limits.as_ref().filter(|(d, _)| *d == self.edit.device).map(|(_, l)| l) {
                Some(Ok(l)) => {
                    ui.add_space(4.0);
                    ui.label(egui::RichText::new("A kamera határai").strong());
                    ui.add_enabled_ui(!busy, |ui| load = limits_table(ui, l));
                }
                Some(Err(e)) => {
                    ui.label(e);
                }
                None => {}
            }
        });
        if let (Some((f, h, fps)), Some(l)) = (load, limits.as_ref()) {
            let c = &mut self.edit.camera;
            c.pixel_format = f;
            c.width = c.width.min(l.sensor.0) / l.step[0] * l.step[0];
            c.height = h;
            c.offset_x = (l.sensor.0 - c.width) / 2 / l.step[2] * l.step[2];
            c.offset_y = (l.sensor.1 - h) / 2 / l.step[3] * l.step[3];
            c.frame_rate = fps;
        }
    }

    /// Every scenario's output folder (once each).
    fn recording_dirs(&self) -> Vec<PathBuf> {
        let mut dirs: Vec<PathBuf> = self.scenarios.iter().map(|s| s.recording.output_dir.clone()).collect();
        dirs.extend(self.rec.folders.iter().cloned());
        dirs.sort();
        dirs.dedup();
        dirs
    }

    fn open_viewer(&mut self, path: PathBuf) {
        let real_fps = self.rec.list.iter().find(|r| r.video == path).and_then(|r| Some(r.frames? as f64 / r.seconds?));
        self.rec.viewer = Some(match recordings::media_info(&path) {
            Ok(info) => Viewer { path, info, t: 0.0, shown: None, busy: None, texture: None, error: None, real_fps, player: None },
            Err(e) => Viewer {
                path,
                info: MediaInfo { width: 0, height: 0, duration: 0.0, fps: 25.0, pix_fmt: String::new(), format: None, curve: false },
                t: 0.0,
                shown: Some(0.0),
                busy: None,
                texture: None,
                error: Some(e),
                real_fps,
                player: None,
            },
        });
    }

    /// The browsed video: frame, time slider and step buttons.
    fn viewer(&mut self, ui: &mut egui::Ui) {
        let Some(v) = &mut self.rec.viewer else { return };
        // Pictures of the running playback; it ends at the end of the video.
        if let Some(p) = &v.player {
            let mut last = None;
            let mut ended = false;
            loop {
                match p.rx.try_recv() {
                    Ok(x) => last = Some(x),
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(_) => {
                        ended = true;
                        break;
                    }
                }
            }
            if let Some((t, img)) = last {
                match img {
                    Ok(img) => {
                        match &mut v.texture {
                            Some(tex) => tex.set(img, egui::TextureOptions::LINEAR),
                            None => v.texture = Some(ui.ctx().load_texture("viewer", img, egui::TextureOptions::LINEAR)),
                        }
                        v.error = None;
                    }
                    Err(e) => v.error = Some(e),
                }
                v.t = t;
                v.shown = Some(t);
            }
            if ended {
                v.player = None;
            }
            ui.ctx().request_repaint_after(Duration::from_millis(15));
        }
        // Collect a finished decode, start the next if the time moved.
        if let Some(rx) = &v.busy {
            match rx.try_recv() {
                Ok(Ok(img)) => {
                    match &mut v.texture {
                        Some(t) => t.set(img, egui::TextureOptions::LINEAR),
                        None => v.texture = Some(ui.ctx().load_texture("viewer", img, egui::TextureOptions::LINEAR)),
                    }
                    v.error = None;
                    v.busy = None;
                }
                Ok(Err(e)) => {
                    v.error = Some(e);
                    v.busy = None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(_) => v.busy = None,
            }
        }
        if v.player.is_none() && v.busy.is_none() && v.shown != Some(v.t) {
            let (tx, rx) = std::sync::mpsc::channel();
            let (path, info, t) = (v.path.clone(), v.info.clone(), v.t);
            std::thread::spawn(move || {
                let _ = tx.send(recordings::decode_frame(&path, &info, t, PREVIEW_MAX_WIDTH as u32).and_then(still_image));
            });
            v.busy = Some(rx);
            v.shown = Some(v.t);
        }

        let name = v.path.file_name().unwrap_or_default().to_string_lossy().into_owned();
        let fmt = v.info.format.map_or(v.info.pix_fmt.clone(), |f| f.name().to_string());
        ui.horizontal_wrapped(|ui| {
            ui.strong(&name);
            ui.label(format!("{}×{} {fmt} · {:.1} s · {:.2} fps", v.info.width, v.info.height, v.info.duration, v.info.fps));
            if v.busy.is_some() {
                ui.spinner();
            }
        });
        let dt = 1.0 / v.info.fps;
        let end = (v.info.duration - dt).max(0.0);
        ui.horizontal(|ui| {
            let mut step = |ui: &mut egui::Ui, label: &str, d: f64| {
                if ui.add_enabled(v.player.is_none(), egui::Button::new(label)).clicked() {
                    v.t = (v.t + d).clamp(0.0, end);
                }
            };
            step(ui, "⏮ 1 s", -1.0);
            step(ui, "◀", -dt);
            step(ui, "▶", dt);
            step(ui, "1 s ⏭", 1.0);
            let n = (v.t * v.info.fps).round() as u64;
            match v.real_fps {
                Some(f) => ui.label(format!("képkocka {n} · valós idő {:.3} s ({f:.2} fps)", n as f64 / f)),
                None => ui.label(format!("képkocka {n}")),
            };
        });
        ui.spacing_mut().slider_width = ui.available_width() - 80.0;
        let before = v.t;
        let seek = ui.add(egui::Slider::new(&mut v.t, 0.0..=end).suffix(" s").fixed_decimals(2));
        // Seeking while playing: play on from there.
        if v.player.is_some() && seek.changed() && v.t != before {
            let speed = v.real_fps.map_or(1.0, |f| v.info.fps / f);
            v.player = Some(Player::start(v.path.clone(), v.info.clone(), v.t, speed));
        }
        if let Some(e) = &v.error {
            ui.colored_label(egui::Color32::from_rgb(200, 60, 50), e);
        }
        if let Some(t) = &v.texture {
            let area = ui.available_rect_before_wrap();
            let ts = t.size_vec2();
            let scale = (area.width() / ts.x).min(area.height() / ts.y).max(0.01);
            let rect = egui::Rect::from_center_size(area.center(), ts * scale);
            ui.allocate_rect(rect, egui::Sense::hover());
            let uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
            ui.painter().image(t.id(), rect, uv, egui::Color32::WHITE);
        }
    }

    fn refresh_recordings(&mut self) {
        self.rec.list = recordings::list(&self.recording_dirs());
        self.rec.loaded = true;
        self.rec.refreshed = Some(Instant::now());
    }

    /// Recordings that must not be touched: being recorded or exported.
    fn busy(&self, video: &std::path::Path) -> bool {
        let recording = self.recording.as_ref().is_some_and(|r| std::path::Path::new(&r.info.video) == video);
        recording || self.rec.job.as_ref().is_some_and(|j| j.video == video) || self.rec.queue.iter().any(|(v, _)| v == video)
    }

    /// Advance the export queue: finish the running job, start the next.
    fn poll_export(&mut self) {
        if let Some(job) = &mut self.rec.job {
            let state = job.poll();
            let name = job.video.file_name().unwrap_or_default().to_string_lossy().into_owned();
            match state {
                JobState::Running => return,
                JobState::Done => {
                    let deleted = if job.settings.delete_after { ", nyers felvétel törölve" } else { "" };
                    let secs = job.started.elapsed().as_secs_f64();
                    self.rec.log.push((true, format!("{name} → {} ({secs:.0} s){deleted}", job.output.display())));
                    self.message = format!("Export kész: {}", job.output.display());
                    self.export_result.insert(job.video.clone(), Ok(job.output.clone()));
                }
                JobState::Failed(e) => {
                    self.rec.log.push((false, format!("{name}: {e}")));
                    self.message = format!("Export hiba: {name}: {e}");
                    self.export_result.insert(job.video.clone(), Err(e));
                }
            }
            self.rec.job = None;
            self.refresh_recordings();
        }
        while let Some((video, settings)) = self.rec.queue.pop_front() {
            match Job::start(&video, &settings) {
                Ok(j) => {
                    self.rec.job = Some(j);
                    break;
                }
                Err(e) => self.rec.log.push((false, format!("{}: {e}", video.display()))),
            }
        }
    }

    /// Stop the running export and drop the waiting ones.
    fn cancel_export(&mut self) {
        if let Some(mut j) = self.rec.job.take() {
            j.cancel();
            let name = file_name(&j.video.display().to_string());
            self.rec.log.push((false, format!("{name}: megszakítva")));
            self.export_result.insert(j.video.clone(), Err("megszakítva".into()));
        }
        self.rec.queue.clear();
    }

    fn start_probe(&mut self, base: Scenario) {
        self.test_result = None;
        self.probe_results.clear();
        let modes = self.engine.modes(&base);
        self.modes = Some((base.device.clone(), modes.clone()));
        match modes {
            Ok(modes) if !modes.is_empty() => {
                self.probe_queue = probe::plan(&base, &modes).into();
                self.probe_base = Some(base);
            }
            Ok(_) => self.test_result = Some(Verdict::fail("A kamera nem adott rögzíthető módot.".into())),
            Err(e) => self.test_result = Some(Verdict::fail(format!("A kamera nem kérdezhető le: {e}"))),
        }
    }

    /// Advance the running test; during discovery start the next one, and
    /// when all are done go back to the edited scenario.
    fn poll_test(&mut self) {
        if let Some((t, _)) = &mut self.test {
            let Some(v) = t.poll(&mut self.engine) else { return };
            let (_, s) = self.test.take().unwrap();
            if self.probe_base.is_some() {
                let modes = self.known_modes().to_vec();
                if let Some(r) = probe::raw_retry(&s, &v).or_else(|| probe::lower_rate(&s, &v, &modes)) {
                    self.probe_queue.push_front(r);
                }
                self.probe_results.push((s, v));
            } else {
                self.test_result = Some(v);
            }
        }
        if self.probe_base.is_none() {
            return;
        }
        match self.probe_queue.pop_front() {
            Some(s) => match probe::Test::start(&mut self.engine, &s, self.test_secs) {
                Ok(t) => self.test = Some((t, s)),
                Err(v) => self.probe_results.push((s, v)),
            },
            None => {
                let base = self.probe_base.take().unwrap();
                if let Err(e) = self.engine.apply(&base) {
                    self.message = format!("Vissza a szerkesztett szcenárióra: {e}");
                }
            }
        }
    }

    /// Discovery results; "Betöltés" puts a mode into the editor.
    fn probe_table(&mut self, ui: &mut egui::Ui) {
        if self.probe_results.is_empty() {
            return;
        }
        ui.label("Kamera felderítése – a Betöltés gombbal a szerkesztőbe kerül, aztán Mentés:");
        let mut load = None;
        egui::ScrollArea::vertical().id_salt("probe").max_height(220.0).show(ui, |ui| {
            egui::Grid::new("probe_grid").striped(true).show(ui, |ui| {
                for (i, (s, v)) in self.probe_results.iter().enumerate() {
                    let c = &s.camera;
                    ui.label(c.pixel_format.name());
                    ui.label(format!("{}x{}", c.width, c.height));
                    ui.label(format!("{} fps", c.frame_rate));
                    ui.label(&s.recording.encoder);
                    ui.colored_label(level_color(v.level), v.text.lines().next().unwrap_or("")).on_hover_text(&v.text);
                    if ui.button("Betöltés").clicked() {
                        load = Some(i);
                    }
                    ui.end_row();
                }
            });
        });
        if let Some(i) = load {
            let s = self.probe_results[i].0.clone();
            self.extra_json = extra_to_json(&s);
            self.edit = s;
        }
    }

    /// The edited scenario with the extra-features JSON parsed in.
    fn edited(&mut self) -> Option<Scenario> {
        let mut s = self.edit.clone();
        let text = self.extra_json.trim();
        if !text.is_empty() {
            match serde_json::from_str(text) {
                Ok(m) => s.camera.extra = m,
                Err(e) => {
                    self.message = format!("Hibás JSON a további beállításokban: {e}");
                    return None;
                }
            }
        } else {
            s.camera.extra.clear();
        }
        // The preview look is adjusted live on the active scenario.
        if let Some(active) = self.engine.scenario.as_ref().filter(|a| a.name == s.name) {
            s.view = active.view.clone();
        }
        if s.name.trim().is_empty() {
            self.message = "A névnek nem szabad üresnek lennie.".into();
            return None;
        }
        Some(s)
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if let Some(mut sh) = self.shots.take() {
            if sh.tick(self, &ctx) {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            } else {
                self.shots = Some(sh);
            }
        }
        self.watch_camera();
        self.update_preview(&ctx);
        self.poll_test();
        // While recording: live view only, no settings window.
        let recording = self.recording.is_some();
        if recording {
            self.page = Page::Main;
            self.rec.viewer = None;
        }
        if !self.styled {
            self.styled = true;
            ctx.global_style_mut(|s| {
                s.spacing.item_spacing = egui::vec2(8.0, 6.0);
                s.spacing.button_padding = egui::vec2(10.0, 4.0);
                s.spacing.interact_size.y = 24.0;
            });
        }
        if self.page == Page::Settings {
            // First visit with a camera: read its limits once.
            if self.modes.as_ref().is_none_or(|(d, _)| *d != self.edit.device)
                && !self.limits_pending
                && self.recording.is_none()
                && self.test.is_none()
            {
                self.message = "Kamera határainak felmérése…".into();
                self.limits_pending = true;
            } else if self.limits_pending {
                // A frame later, so the message is on screen during the wait.
                self.limits_pending = false;
                self.read_limits();
            }
        }
        self.poll_export();
        egui::Panel::bottom("transport").show(ui, |ui| {
            ui.add_space(6.0);
            self.transport_bar(ui);
            ui.add_space(4.0);
        });
        egui::Panel::left("left").resizable(false).exact_size(400.0).show(ui, |ui| {
            ui.add_space(8.0);
            egui::ScrollArea::vertical().show(ui, |ui| self.left_panel(ui));
        });
        if self.page == Page::Settings {
            let mut open = true;
            egui::Window::new("Beállítások")
                .open(&mut open)
                .default_pos([420.0, 40.0])
                .default_size([620.0, 820.0])
                .collapsible(false)
                .show(&ctx, |ui| self.settings_page(ui));
            if !open {
                self.page = Page::Main;
            }
        }
        // Recording: only the crop position may move (its size is fixed).
        let crop = if self.test.is_some() {
            Crop::Off
        } else if recording {
            if self.engine.hw_crop() { Crop::Move } else { Crop::Off }
        } else {
            Crop::Full
        };
        egui::CentralPanel::default().show(ui, |ui| {
            if self.rec.viewer.is_some() {
                self.viewer(ui);
            } else {
                self.preview(ui, true, crop);
            }
        });
        ctx.request_repaint_after(PREVIEW_INTERVAL);
    }

    fn on_exit(&mut self) {
        if let Some(j) = &mut self.rec.job {
            j.cancel();
        }
        self.engine.finish_recording();
    }
}

const GREEN: egui::Color32 = egui::Color32::from_rgb(40, 150, 70);
const ORANGE: egui::Color32 = egui::Color32::from_rgb(200, 130, 20);
const RED: egui::Color32 = egui::Color32::from_rgb(200, 40, 40);
const BLUE: egui::Color32 = egui::Color32::from_rgb(30, 100, 190);

/// Width of the label column in settings grids.
const LABEL_W: f32 = 150.0;

/// "YUYV 1280×720 · 30 fps" (largest rate).
fn mode_label(m: &Mode) -> String {
    format!("{} {}×{} · {} fps", m.format.name(), m.width, m.height, m.fps)
}

/// Put a camera mode into a scenario: format, frame rate, and the full
/// size (uncropped). Cameras that crop in software are opened at it.
fn set_mode(s: &mut Scenario, m: &Mode) {
    let c = &mut s.camera;
    c.pixel_format = m.format;
    (c.width, c.height, c.offset_x, c.offset_y) = (m.width, m.height, 0, 0);
    c.frame_rate = m.fps;
    c.capture_size = (!s.device.hw_crop()).then_some((m.width, m.height));
}

/// Highest frame rate that passed the recording test for this mode.
fn best_rate(results: &[(Scenario, Verdict)], device: &Device, m: &Mode) -> Option<f64> {
    results
        .iter()
        .filter(|(s, v)| s.device == *device && v.level != Level::Fail)
        .filter(|(s, _)| (s.camera.pixel_format, s.camera.width, s.camera.height) == (m.format, m.width, m.height))
        .map(|(s, _)| s.camera.frame_rate)
        .max_by(f64::total_cmp)
}

fn frame_crop_step(f: PixelFormat) -> u32 {
    crate::frame::crop_step(f).unwrap_or(1)
}

/// What the camera really sends, red where it differs from the request.
fn actual_mode(ui: &mut egui::Ui, s: &Scenario, a: &crate::engine::Actual, sw_crop: bool) {
    let c = &s.camera;
    let Some(f) = a.format else {
        ui.weak("még nem érkezett kép");
        return;
    };
    let want = if sw_crop { c.capture_size.unwrap_or((c.width, c.height)) } else { (c.width, c.height) };
    let fps = a.fps.map_or("…".into(), |v| format!("{v:.1} fps"));
    let text = format!("{} {}×{} · {fps}", f.name(), a.width, a.height);
    let mut problems = Vec::new();
    if f != c.pixel_format {
        problems.push(format!("{} helyett {}-t küld, a felvétel is ilyen lesz", c.pixel_format.name(), f.name()));
    }
    if (a.width, a.height) != want {
        problems.push(format!("{}×{} helyett", want.0, want.1));
    }
    if a.fps.is_some_and(|v| v < c.frame_rate * 0.95) && !c.extra.get("TriggerMode").is_some_and(|v| v == "On") {
        problems.push(format!("a kért {} fps helyett", c.frame_rate));
    }
    ui.vertical(|ui| {
        if problems.is_empty() {
            ui.colored_label(GREEN, text);
        } else {
            ui.colored_label(RED, text);
            ui.colored_label(RED, problems.join("; "));
        }
        if sw_crop && (c.width, c.height) != want {
            ui.small(format!("szoftveresen kivágva: {}×{} ({}, {})", c.width, c.height, c.offset_x, c.offset_y));
        }
    });
}

/// A collapsible group of settings.
/// A card of the left panel.
fn card(ui: &mut egui::Ui, title: &str, add: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::group(ui.style()).inner_margin(10.0).corner_radius(6.0).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.label(egui::RichText::new(title).strong().size(15.0));
        ui.add_space(2.0);
        add(ui);
    });
    ui.add_space(8.0);
}

/// A labelled bar: green, orange above 70%, red above 90%.
fn bar(ui: &mut egui::Ui, label: &str, frac: f32, text: String) {
    let frac = frac.clamp(0.0, 1.0);
    ui.horizontal(|ui| {
        ui.add_sized([62.0, 18.0], egui::Label::new(label));
        let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 18.0), egui::Sense::hover());
        let p = ui.painter();
        p.rect_filled(rect, 3.0, ui.visuals().extreme_bg_color);
        let color = if frac > 0.9 { RED } else if frac > 0.7 { ORANGE } else { GREEN };
        p.rect_filled(egui::Rect::from_min_size(rect.min, egui::vec2(rect.width() * frac, rect.height())), 3.0, color);
        p.text(rect.center(), egui::Align2::CENTER_CENTER, text, egui::FontId::proportional(12.0), ui.visuals().strong_text_color());
    });
}

/// "1:02:03" or "2:03.4".
fn hms(s: f64) -> String {
    let (h, m) = ((s / 3600.0) as u64, (s / 60.0) as u64 % 60);
    if h > 0 { format!("{h}:{m:02}:{:02}", s as u64 % 60) } else { format!("{m}:{:04.1}", s % 60.0) }
}

/// What a recording card asks for.
enum CardAction {
    Open(PathBuf),
    Play(PathBuf),
    Close,
    Export(PathBuf),
    Delete(PathBuf),
}

/// How the crop selection on the preview may be changed.
#[derive(Clone, Copy, PartialEq)]
enum Crop {
    Off,
    /// Moved only (while recording: the size is fixed).
    Move,
    Full,
}

#[derive(Clone, Copy)]
enum Icon {
    Rec,
    Play,
    Pause,
    Stop,
}

/// A tape recorder button: the symbol everyone knows, drawn, not a glyph.
fn transport(ui: &mut egui::Ui, icon: Icon, enabled: bool, active: bool, hint: &str) -> egui::Response {
    let size = egui::vec2(72.0, 48.0);
    let (rect, resp) = ui.allocate_exact_size(size, if enabled { egui::Sense::click() } else { egui::Sense::hover() });
    let v = ui.visuals();
    let fill = if active && matches!(icon, Icon::Rec) {
        egui::Color32::from_rgb(90, 20, 20)
    } else if active {
        v.selection.bg_fill
    } else if enabled && resp.hovered() {
        v.widgets.hovered.bg_fill
    } else {
        v.widgets.inactive.bg_fill
    };
    let p = ui.painter();
    p.rect_filled(rect, 6.0, fill);
    p.rect_stroke(rect, 6.0, v.widgets.inactive.bg_stroke, egui::StrokeKind::Inside);
    let ink = if matches!(icon, Icon::Rec) && active {
        egui::Color32::from_rgb(255, 40, 40)
    } else if !enabled {
        v.weak_text_color().gamma_multiply(0.5)
    } else if matches!(icon, Icon::Rec) {
        RED
    } else {
        v.strong_text_color()
    };
    let c = rect.center();
    let r = 13.0;
    match icon {
        Icon::Rec => {
            p.circle_filled(c, r, ink);
        }
        Icon::Play => {
            let pts = vec![c + egui::vec2(-r * 0.7, -r), c + egui::vec2(-r * 0.7, r), c + egui::vec2(r, 0.0)];
            p.add(egui::Shape::convex_polygon(pts, ink, egui::Stroke::NONE));
        }
        Icon::Pause => {
            for dx in [-r * 0.55, r * 0.55] {
                p.rect_filled(egui::Rect::from_center_size(c + egui::vec2(dx, 0.0), egui::vec2(r * 0.55, r * 2.0)), 1.0, ink);
            }
        }
        Icon::Stop => {
            p.rect_filled(egui::Rect::from_center_size(c, egui::vec2(r * 1.8, r * 1.8)), 2.0, ink);
        }
    }
    let resp = resp.on_hover_text(hint);
    if enabled { resp.on_hover_cursor(egui::CursorIcon::PointingHand) } else { resp }
}

fn section(ui: &mut egui::Ui, title: &str, open: bool, add: impl FnOnce(&mut egui::Ui)) {
    egui::CollapsingHeader::new(egui::RichText::new(title).strong().size(15.0)).default_open(open).show(ui, |ui| {
        add(ui);
        ui.add_space(4.0);
    });
}

/// Label | control rows with the same label width everywhere.
fn grid(ui: &mut egui::Ui, id: &str, add: impl FnOnce(&mut egui::Ui)) {
    egui::Grid::new(id).num_columns(2).min_col_width(LABEL_W).spacing([12.0, 8.0]).show(ui, add);
}

fn file_name(path: &str) -> String {
    std::path::Path::new(path).file_name().map_or(path.to_string(), |n| n.to_string_lossy().into_owned())
}

/// One line saying what an export makes.
fn export_label(x: &ExportSettings) -> String {
    let codec = |c: &str| CODECS.iter().find(|(id, _)| *id == c).map_or(c.to_string(), |(_, l)| l.to_string());
    let what = match x.method.as_str() {
        "archive" => "Archív: veszteségmentes FFV1, bitre pontos".to_string(),
        "native" => format!("Natív videó, {}", codec(&x.codec)),
        m => {
            let names: Vec<&str> = MAGNIFY.iter().filter(|(id, _)| m.split(',').any(|p| p == *id)).map(|(_, l)| *l).collect();
            format!("Nagyítás: {}, {}", names.join(", "), codec(&x.codec))
        }
    };
    if x.delete_after { format!("{what}; utána a nyers felvétel törlődik") } else { what }
}

const CODECS: [(&str, &str); 4] = [
    ("h264", "H.264 (kicsi, veszteséges)"),
    ("hevc", "HEVC 10 bit (kisebb, veszteséges)"),
    ("prores", "ProRes 422 HQ (vágáshoz)"),
    ("ffv1", "FFV1 (veszteségmentes, nagy)"),
];

/// Edit an export setting: kind, magnification methods, codec and options.
fn export_editor(ui: &mut egui::Ui, x: &mut ExportSettings, id: &str) {
    let magnify = x.method != "archive" && x.method != "native";
    ui.horizontal_wrapped(|ui| {
        ui.radio_value(&mut x.method, "archive".into(), "Archív")
            .on_hover_text("A nyers felvétel veszteségmentesen, FFV1-gyel (bitre pontos, kb. 20%-kal kisebb). Az időbélyeg-CSV is mellé kerül.");
        ui.radio_value(&mut x.method, "native".into(), "Natív videó")
            .on_hover_text("Demozaikolt (színes) videó, feldolgozás nélkül, a választott tömörítéssel.");
        if ui.radio(magnify, "Mozgás- és színnagyítás").clicked() && !magnify {
            x.method = "magnify".into();
        }
    });
    if x.method != "archive" && x.method != "native" {
        ui.indent((id, "magnify"), |ui| {
            let mut on: Vec<&str> = MAGNIFY.iter().map(|(m, _)| *m).filter(|m| x.method.split(',').any(|p| p == *m)).collect();
            for (m, label) in MAGNIFY {
                let mut c = on.contains(&m);
                if ui.checkbox(&mut c, label).changed() {
                    if c {
                        on.push(m);
                    } else {
                        on.retain(|o| *o != m);
                    }
                }
            }
            let joined: Vec<&str> = MAGNIFY.iter().map(|(m, _)| *m).filter(|m| on.contains(m)).collect();
            x.method = if joined.is_empty() { "magnify".into() } else { joined.join(",") };
            if joined.is_empty() {
                ui.colored_label(ORANGE, "Válassz legalább egy nagyítást.");
            }
            ui.small("A kimenet rács: az eredeti, majd sávonként egy panel. Lassú; kisebb feldolgozási szélességgel gyorsabb.");
        });
    }
    if x.method != "archive" {
        egui::Grid::new((id, "opts")).num_columns(2).min_col_width(LABEL_W).spacing([12.0, 8.0]).show(ui, |ui| {
            ui.label("Tömörítés");
            let current = CODECS.iter().find(|(c, _)| *c == x.codec).map_or(x.codec.as_str(), |(_, l)| *l);
            egui::ComboBox::from_id_salt((id, "codec")).width(260.0).selected_text(current).show_ui(ui, |ui| {
                for (c, l) in CODECS {
                    ui.selectable_value(&mut x.codec, c.to_string(), l);
                }
            });
            ui.end_row();
            ui.label("Lassítás");
            ui.checkbox(&mut x.slow, "minden képkocka, 25 fps-sel lejátszva");
            ui.end_row();
            ui.label("Feldolgozási szélesség");
            ui.add(egui::DragValue::new(&mut x.width).range(0..=8192).speed(16).custom_formatter(|v, _| {
                if v == 0.0 { "teljes".into() } else { format!("{v} px") }
            }));
            ui.end_row();
            ui.label("Kezdet, hossz");
            ui.horizontal(|ui| {
                ui.add(egui::DragValue::new(&mut x.start).range(0.0..=1e6).speed(0.5).suffix(" s"));
                ui.add(egui::DragValue::new(&mut x.dur).range(0.0..=1e6).speed(0.5).custom_formatter(|v, _| {
                    if v == 0.0 { "végig".into() } else { format!("{v} s") }
                }));
            });
            ui.end_row();
        });
    }
    ui.checkbox(&mut x.delete_after, "Sikeres export után a nyers felvétel törlése")
        .on_hover_text("Csak akkor töröl, ha az export hibátlanul elkészült. A nyers felvétel nem állítható vissza.");
    ui.small(format!("A kimenet a felvétel mappájának {}/ almappájába kerül.", recordings::EXPORT_DIR));
}

/// A screenshot step: file name and what to do before the picture.
type Shot = (&'static str, fn(&mut App));

/// Screenshot steps on the simulated camera.
const SHOTS: [Shot; 6] = [
    ("1_fooldal", |a| a.page = Page::Main),
    ("2_fooldal_felvetel", |a| {
        a.rec_name = "_kepernyokep".into();
        a.start_recording();
    }),
    ("3_fooldal_leallitva", |a| a.stop_recording()),
    ("4_fooldal_export", |a| {
        if let (Some((v, _)), Some(s)) = (a.last_rec.clone(), &a.engine.scenario) {
            a.rec.queue.push_back((v, s.export.clone()));
        }
    }),
    ("5_beallitasok", |a| a.page = Page::Settings),
    ("6_felvetel_lejatszasa", |a| {
        a.page = Page::Main;
        a.refresh_recordings();
        if let Some(r) = a.rec.list.iter().find(|r| !r.export) {
            let v = r.video.clone();
            a.rec.expanded = Some(v.clone());
            a.open_viewer(v);
            a.play(true);
        }
    }),
];

/// Walk on the real camera (`screenshots DIR camera`).
const CAMERA_SHOTS: [Shot; 18] = [
    ("01_fooldal", |a| a.page = Page::Main),
    ("02_fooldal_kivagas", |a| a.crop_centre(960.0, 540.0)),
    ("03_felvetel", |a| {
        a.rec_name = "_kamerateszt".into();
        a.start_recording();
    }),
    ("03b_felvetel_kozben_fenyero", |a| a.set_camera_light(true, 10.0)),
    ("03c_felvetel_kozben_kivagas_mozgatasa", |a| {
        // Move the crop by (64, 32) on the sensor, size unchanged.
        let (Some(g), Some(c)) = (a.geometry(), a.engine.scenario.as_ref().map(|s| s.camera.clone())) else { return };
        let sel = egui::Rect::from_min_size(egui::pos2((c.offset_x + 64) as f32, (c.offset_y + 32) as f32), egui::vec2(c.width as f32, c.height as f32));
        let (sensor, min) = (egui::vec2(g.sensor.0 as f32, g.sensor.1 as f32), egui::vec2(g.min_size.0 as f32, g.min_size.1 as f32));
        a.crop_to(sel, g.step, min, sensor);
    }),
    ("04_szunet", |a| {
        if let Some(r) = &mut a.recording {
            r.set_paused(true);
        }
    }),
    ("05_folytatas", |a| {
        if let Some(r) = &mut a.recording {
            r.set_paused(false);
        }
    }),
    ("06_leallitva", |a| a.stop_recording()),
    ("07_export", |a| {
        if let (Some((v, _)), Some(s)) = (a.last_rec.clone(), &a.engine.scenario) {
            a.rec.queue.push_back((v, s.export.clone()));
        }
    }),
    ("08_szcenario_kivagasa", |a| {
        if let Some(s) = a.scenarios.iter().find(|s| Some(&s.name) == a.engine.scenario.as_ref().map(|x| &x.name)).cloned() {
            a.apply(&s);
        }
    }),
    ("09_beallitasok_hatarok", |a| a.page = Page::Settings),
    ("10_beallitasok_kivagas", |a| a.crop_centre(1920.0, 480.0)),
    ("11_atlagolas", |a| {
        if let Some(s) = a.engine.scenario.as_mut() {
            s.view.average = true;
            s.view.align = true;
        }
    }),
    ("12_sotetkep_felvetel", |a| a.dark_capture = Some(dark::Capture::new(32))),
    ("13_sotetkep_levonas", |a| {
        if let Some(s) = a.engine.scenario.as_mut() {
            s.view.dark = true;
            s.view.average = false;
        }
        a.edit.view.dark = true;
    }),
    ("14_teljes_kep", |a| a.crop_centre(1944.0, 1470.0)),
    ("15_felvetel_lejatszasa", |a| {
        a.page = Page::Main;
        a.refresh_recordings();
        if let Some(r) = a.rec.list.iter().find(|r| r.video.to_string_lossy().contains("_kamerateszt") && !r.export) {
            let v = r.video.clone();
            a.rec.expanded = Some(v.clone());
            a.open_viewer(v);
            a.play(true);
        }
    }),
    ("16_vege", |a| a.page = Page::Main),
];

/// How long each screenshot step runs before the picture (preview, decode).
const SHOT_WAIT: Duration = Duration::from_millis(3000);

struct Shots {
    dir: PathBuf,
    steps: &'static [Shot],
    /// Delete the recording made for the pictures.
    cleanup: bool,
    step: usize,
    started: Option<Instant>,
    pending: Option<std::sync::mpsc::Receiver<std::sync::Arc<egui::ColorImage>>>,
}

impl Shots {
    /// Run the current step; returns true when all pictures are saved.
    fn tick(&mut self, app: &mut App, ctx: &egui::Context) -> bool {
        ctx.request_repaint();
        if let Some(rx) = &self.pending {
            let Ok(img) = rx.try_recv() else { return false };
            let path = self.dir.join(format!("{}.png", self.steps[self.step].0));
            if let Err(e) = save_png(&path, &img) {
                eprintln!("{}: {e}", path.display());
            } else {
                println!("{}", path.display());
            }
            self.pending = None;
            self.started = None;
            self.step += 1;
        }
        if self.step >= self.steps.len() {
            // The recording made for the pictures.
            if let Some(sc) = app.engine.scenario.as_ref().filter(|_| self.cleanup) {
                let dir = &sc.recording.output_dir;
                for d in [dir.clone(), dir.join(recordings::EXPORT_DIR)] {
                    for f in std::fs::read_dir(&d).into_iter().flatten().flatten() {
                        if f.file_name().to_string_lossy().starts_with("_kepernyokep") {
                            let _ = std::fs::remove_file(f.path());
                        }
                    }
                }
            }
            return true;
        }
        match self.started {
            None => {
                (self.steps[self.step].1)(app);
                self.started = Some(Instant::now());
            }
            Some(t) if t.elapsed() >= SHOT_WAIT => {
                let (tx, rx) = std::sync::mpsc::channel();
                ctx.request_screenshot(move |img| {
                    let _ = tx.send(img);
                });
                self.pending = Some(rx);
            }
            Some(_) => {}
        }
        false
    }
}

fn save_png(path: &std::path::Path, img: &egui::ColorImage) -> Result<(), String> {
    let file = std::io::BufWriter::new(std::fs::File::create(path).map_err(|e| e.to_string())?);
    let mut enc = png::Encoder::new(file, img.size[0] as u32, img.size[1] as u32);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut w = enc.write_header().map_err(|e| e.to_string())?;
    let rgba: Vec<u8> = img.pixels.iter().flat_map(|c| c.to_srgba_unmultiplied()).collect();
    w.write_image_data(&rgba).map_err(|e| e.to_string())
}

fn level_color(l: Level) -> egui::Color32 {
    match l {
        Level::Ok => egui::Color32::from_rgb(40, 160, 70),
        Level::Short => egui::Color32::from_rgb(210, 140, 30),
        Level::Fail => egui::Color32::from_rgb(200, 60, 50),
    }
}

fn extra_to_json(s: &Scenario) -> String {
    if s.camera.extra.is_empty() {
        String::new()
    } else {
        serde_json::to_string_pretty(&s.camera.extra).unwrap_or_default()
    }
}

/// Auto-levels (black and white point, if given), then brightness/contrast,
/// as an 8-bit table.
fn tone_table(v: &View, levels: Option<(f32, f32)>) -> [u8; 256] {
    std::array::from_fn(|i| {
        let mut x = i as f32 / 255.0;
        if let Some((lo, hi)) = levels {
            x = (x - lo) / (hi - lo);
        }
        let x = (x - 0.5) * v.contrast + 0.5 + v.brightness;
        (x.clamp(0.0, 1.0) * 255.0 + 0.5) as u8
    })
}

/// Downscaled 8-bit RGB preview: white balance gains (colour sources only),
/// then the LUT, then the tone table. Bayer RGGB is shown by taking each 2×2
/// cell as one pixel (no interpolation); MJPEG is decoded first. Also returns
/// the mean R, G, B before white balance (unsaturated pixels, colour only)
/// for the auto white balance, and a brightness histogram before the tone
/// table for the auto-levels. `None` if the frame cannot be decoded.
/// Preview of a linear averaged image: white balance, LUT and tone as in
/// [`to_color_image`].
fn image_to_color(
    img: &stack::Image,
    lut: Option<&PreviewLut>,
    wb: Option<[f32; 3]>,
    tone: &[u8; 256],
) -> Option<(egui::ColorImage, Option<[f64; 3]>, [u32; 256])> {
    let (w, h, ch) = (img.width, img.height, img.channels);
    if w == 0 || h == 0 {
        return None;
    }
    let to8 = |v: f32| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
    let mut rgb = Vec::with_capacity(w * h * 3);
    let (mut sum, mut n) = ([0f64; 3], 0u64);
    let mut hist = [0u32; 256];
    for p in img.data.chunks_exact(ch) {
        if ch == 1 {
            let v = to8(p[0]);
            let px = lut.map_or([v, v, v], |l| l.gray[v as usize]);
            hist[px[1] as usize] += 1;
            rgb.extend(px.map(|c| tone[c as usize]));
            continue;
        }
        if p.iter().all(|&c| c < 250.0 / 255.0) {
            (0..3).for_each(|c| sum[c] += p[c] as f64 * 255.0);
            n += 1;
        }
        let mut px: [f32; 3] = [p[0], p[1], p[2]];
        if let Some(g) = wb {
            px = std::array::from_fn(|c| (px[c] * g[c]).min(1.0));
        }
        if let Some(l) = lut {
            px = l.lut.apply(px);
        }
        let px = px.map(to8);
        hist[((px[0] as u32 + 2 * px[1] as u32 + px[2] as u32) / 4) as usize] += 1;
        rgb.extend(px.map(|c| tone[c as usize]));
    }
    let means = (n > 0).then(|| sum.map(|c| c / n as f64));
    Some((egui::ColorImage::from_rgb([w, h], &rgb), means, hist))
}

fn to_color_image(
    f: &Frame,
    max_width: usize,
    lut: Option<&PreviewLut>,
    wb: Option<[f32; 3]>,
    tone: &[u8; 256],
) -> Option<(egui::ColorImage, Option<[f64; 3]>, [u32; 256])> {
    let (w, h) = (f.width as usize, f.height as usize);
    let decoded;
    let (data, format, w, h) = if f.format == PixelFormat::Mjpeg {
        let options = zune_jpeg::zune_core::options::DecoderOptions::default()
            .jpeg_set_out_colorspace(zune_jpeg::zune_core::colorspace::ColorSpace::RGB);
        let mut dec =
            zune_jpeg::JpegDecoder::new_with_options(zune_jpeg::zune_core::bytestream::ZCursor::new(&f.data[..]), options);
        decoded = dec.decode().ok()?;
        let info = dec.info()?;
        (&decoded[..], None, info.width as usize, info.height as usize)
    } else {
        (&f.data[..], Some(f.format), w, h)
    };

    let mut step = w.div_ceil(max_width).max(1);
    if format.is_some_and(|f| f.is_bayer() || matches!(f, PixelFormat::Yuyv | PixelFormat::Uyvy | PixelFormat::Nv12)) {
        step = step.next_multiple_of(2);
    }
    let (ow, oh) = (w / step, h / step);
    let need = match format {
        None => w * h * 3,
        Some(fmt) => Frame::byte_size(w as u32, h as u32, fmt),
    };
    if data.len() < need || ow == 0 || oh == 0 {
        return None;
    }
    let bpp = format.map_or(3, |f| f.bytes_per_pixel());
    let shift = format.map_or(0, |f| f.bits().saturating_sub(8));
    let mono = |x: usize, y: usize| -> u8 {
        let i = (y * w + x) * bpp;
        if bpp == 1 {
            data[i]
        } else {
            (u16::from_le_bytes([data[i], data[i + 1]]) >> shift).min(255) as u8
        }
    };
    // BT.601 limited range, as used by UVC cameras.
    let yuv = |y: u8, u: u8, v: u8| -> [u8; 3] {
        let (c, d, e) = (y as f32 - 16.0, u as f32 - 128.0, v as f32 - 128.0);
        let clamp = |x: f32| x.clamp(0.0, 255.0) as u8;
        [clamp(1.164 * c + 1.596 * e), clamp(1.164 * c - 0.392 * d - 0.813 * e), clamp(1.164 * c + 2.017 * d)]
    };

    let mut rgb = Vec::with_capacity(ow * oh * 3);
    let (mut sum, mut n) = ([0u64; 3], 0u64);
    let mut hist = [0u32; 256];
    for oy in 0..oh {
        let y = oy * step;
        for ox in 0..ow {
            let x = ox * step;
            let px: [u8; 3] = match format {
                None => {
                    let i = (y * w + x) * 3;
                    [data[i], data[i + 1], data[i + 2]]
                }
                Some(PixelFormat::Yuyv) => {
                    let i = (y * w + x) * 2;
                    yuv(data[i], data[i + 1], data[i + 3])
                }
                Some(PixelFormat::Uyvy) => {
                    let i = (y * w + x) * 2;
                    yuv(data[i + 1], data[i], data[i + 2])
                }
                Some(PixelFormat::Nv12) => {
                    let uv = w * h + (y / 2) * w + x;
                    yuv(data[y * w + x], data[uv], data[uv + 1])
                }
                Some(f) if f.is_bayer() => {
                    let g = ((mono(x + 1, y) as u16 + mono(x, y + 1) as u16) / 2) as u8;
                    [mono(x, y), g, mono(x + 1, y + 1)]
                }
                Some(_) => {
                    // Mono: fast LUT path through the grey table.
                    let v = mono(x, y);
                    let px = lut.map_or([v, v, v], |l| l.gray[v as usize]);
                    hist[px[1] as usize] += 1;
                    rgb.extend(px.map(|c| tone[c as usize]));
                    continue;
                }
            };
            if px.iter().all(|&c| c < 250) {
                (0..3).for_each(|c| sum[c] += px[c] as u64);
                n += 1;
            }
            let px = match wb {
                Some(g) => std::array::from_fn(|c| (px[c] as f32 * g[c]).min(255.0) as u8),
                None => px,
            };
            let px = match lut {
                Some(l) => l.lut.apply(px.map(|c| c as f32 / 255.0)).map(|c| (c.clamp(0.0, 1.0) * 255.0 + 0.5) as u8),
                None => px,
            };
            hist[((px[0] as u32 + 2 * px[1] as u32 + px[2] as u32) / 4) as usize] += 1;
            rgb.extend(px.map(|c| tone[c as usize]));
        }
    }
    let means = (n > 0).then(|| sum.map(|c| c as f64 / n as f64));
    Some((egui::ColorImage::from_rgb([ow, oh], &rgb), means, hist))
}

/// Camera limits; a click on a frame rate returns (format, height, fps) to
/// load into the editor.
fn limits_table(ui: &mut egui::Ui, l: &Limits) -> Option<(PixelFormat, u32, f64)> {
    ui.label(format!(
        "Szenzor {}×{} · lépés: szél. {}, mag. {}, eltolás {}/{} · expozíció {:.0}–{:.0} µs · erősítés {:.0}–{:.0} dB",
        l.sensor.0, l.sensor.1, l.step[0], l.step[1], l.step[2], l.step[3], l.exposure_us.0, l.exposure_us.1, l.gain_db.0, l.gain_db.1
    ));
    if let Some(m) = l.max_fps {
        ui.label(format!("A szerkesztett beállítással legfeljebb {m:.1} fps."));
    }
    ui.label("Legnagyobb fps magasság szerint (kattintás: betöltés, középre vágva):");
    let mut pick = None;
    egui::ScrollArea::horizontal().id_salt("limits").show(ui, |ui| {
        egui::Grid::new("limits_grid").striped(true).show(ui, |ui| {
            ui.label("");
            for h in &l.heights {
                ui.strong(h.to_string());
            }
            ui.end_row();
            for (f, row) in &l.table {
                ui.label(f.name());
                for (h, v) in l.heights.iter().zip(row) {
                    match v {
                        Some(v) if ui.small_button(format!("{v:.1}")).clicked() => pick = Some((*f, *h, *v)),
                        Some(_) => {}
                        None => {
                            ui.label("–");
                        }
                    }
                }
                ui.end_row();
            }
        });
    });
    pick
}

/// A decoded recording frame for the viewer: raw camera data like the live
/// preview, with one-shot grey-world white balance and auto-levels.
fn still_image(still: Still) -> Result<egui::ColorImage, String> {
    let f = match still {
        Still::Rgb { width, height, data } => return Ok(egui::ColorImage::from_rgb([width, height], &data)),
        Still::Raw(f) => f,
    };
    let plain = tone_table(&View::default(), None);
    let (_, means, hist) = to_color_image(&f, PREVIEW_MAX_WIDTH, None, None, &plain).ok_or("a képkocka nem dekódolható")?;
    let wb = means.map(|[r, g, b]| [g / r.max(1e-3), 1.0, g / b.max(1e-3)].map(|x| x.clamp(0.25, 4.0) as f32));
    let total: u32 = hist.iter().sum();
    let pct = |p: f32| {
        let mut acc = 0;
        hist.iter().position(|&h| {
            acc += h;
            acc as f32 >= p * total as f32
        })
        .unwrap_or(255) as f32
            / 255.0
    };
    let (lo, hi) = (pct(0.005), pct(0.995));
    let tone = tone_table(&View::default(), Some((lo, hi.max(lo + 0.05))));
    to_color_image(&f, PREVIEW_MAX_WIDTH, None, wb, &tone).map(|(img, _, _)| img).ok_or_else(|| "a képkocka nem dekódolható".into())
}
