//! GUI: main page (scenario select, record start/end, preview) and settings
//! page (edit, try and save scenarios). Preview refreshes at most 25 fps.

use std::path::PathBuf;
use std::sync::atomic::Ordering::Relaxed;
use std::time::{Duration, Instant};

use eframe::egui;

use crate::encoder;
use crate::engine::{Engine, PREVIEW_INTERVAL, RecordingInfo};
use crate::recorder::RecStats;
use crate::frame::{Frame, PixelFormat};
use crate::lut::{self, Lut};
use crate::scenario::{self, Device, Scenario, View};

/// Loaded preview LUT: file name, LUT, 8-bit grey fast-path table.
struct PreviewLut {
    name: String,
    lut: Lut,
    gray: Vec<[u8; 3]>,
}

/// Preview images are downscaled to at most this width.
const PREVIEW_MAX_WIDTH: usize = 960;

#[derive(PartialEq)]
enum Page {
    Main,
    Settings,
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
    recording: Option<(RecordingInfo, Instant)>,
    /// Running recording test (settings page) and its last result.
    test: Option<RecTest>,
    test_secs: f64,
    test_result: Option<(bool, String)>,
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
    /// Scenario to reopen while its camera is unplugged, and the last try.
    reconnect: Option<(Scenario, Instant)>,
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
        let edit = scenarios[0].clone();
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
            test_secs: 10.0,
            test_result: None,
            message: String::new(),
            texture: None,
            lut_dir,
            lut: None,
            devices: find_devices(),
            wb_gains: [1.0; 3],
            levels: (0.0, 1.0),
            reconnect: None,
        };
        let first = app.scenarios[0].clone();
        app.apply(&first);
        app
    }

    fn apply(&mut self, s: &Scenario) {
        self.message = match self.engine.apply(s) {
            Ok(()) => format!("Szcenárió aktív: {}", s.name),
            Err(e) => format!("Hiba: {e}"),
        };
    }

    /// Notice an unplugged camera: end a running recording (it keeps every
    /// frame received so far) and retry opening the camera once a second.
    fn watch_camera(&mut self) {
        if self.engine.camera_lost() && self.reconnect.is_none() {
            let Some(s) = self.engine.scenario.clone() else { return };
            self.message = "A kamera levált. Újracsatlakozás…".into();
            if self.recording.is_some() {
                self.engine.stop_recording();
                if let Some((info, _)) = self.recording.take() {
                    self.message = format!(
                        "A kamera levált, a felvétel leállt: {} – {} képkocka. Újracsatlakozás…",
                        info.video,
                        info.stats.queued.load(Relaxed)
                    );
                }
                self.rec_name.clear();
            }
            self.reconnect = Some((s, Instant::now()));
        }
        if let Some((s, last)) = &self.reconnect
            && last.elapsed() >= Duration::from_secs(1)
        {
            let s = s.clone();
            match self.engine.reconnect(&s) {
                Ok(()) => {
                    self.reconnect = None;
                    self.message = format!("A kamera újra csatlakozott. Szcenárió aktív: {}", s.name);
                }
                Err(_) => self.reconnect = Some((s, Instant::now())),
            }
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
        if let Some(frame) = self.engine.take_preview() {
            let view = self.engine.scenario.as_ref().map(|s| s.view.clone()).unwrap_or_default();
            let wb = view.auto_white_balance.then_some(self.wb_gains);
            let levels = view.auto_levels.then_some(self.levels);
            let img = to_color_image(&frame, PREVIEW_MAX_WIDTH, self.lut.as_ref(), wb, &tone_table(&view, levels));
            self.engine.recycle(frame);
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
    fn view_controls(&mut self, ui: &mut egui::Ui) {
        let Some(s) = self.engine.scenario.as_mut() else { return };
        let v = &mut s.view;
        ui.horizontal(|ui| {
            ui.checkbox(&mut v.auto_white_balance, "Automatikus fehéregyensúly");
            ui.checkbox(&mut v.auto_levels, "Automatikus fényerő");
            ui.label("Fényerő");
            ui.add(egui::Slider::new(&mut v.brightness, -0.5..=0.5).fixed_decimals(2));
            ui.label("Kontraszt");
            ui.add(egui::Slider::new(&mut v.contrast, 0.25..=4.0).logarithmic(true).fixed_decimals(2));
            if ui.button("Alaphelyzet").clicked() {
                *v = View { auto_white_balance: v.auto_white_balance, auto_levels: v.auto_levels, ..View::default() };
            }
        });
        ui.label("Csak az élőképre hat, a felvétel nyers marad.");
    }

    fn preview(&self, ui: &mut egui::Ui) {
        match &self.texture {
            Some(t) => {
                ui.add(egui::Image::new(t).shrink_to_fit());
            }
            None => {
                ui.label("Nincs kép.");
            }
        }
    }

    fn main_page(&mut self, ui: &mut egui::Ui) {
        let recording = self.recording.is_some();
        ui.horizontal(|ui| {
            ui.label("Szcenárió:");
            let current = self.engine.scenario.as_ref().map(|s| s.name.clone()).unwrap_or_default();
            let mut chosen = None;
            ui.add_enabled_ui(!recording, |ui| {
                egui::ComboBox::from_id_salt("scenario")
                    .selected_text(&current)
                    .show_ui(ui, |ui| {
                        for s in &self.scenarios {
                            if ui.selectable_label(s.name == current, &s.name).clicked() && s.name != current {
                                chosen = Some(s.clone());
                            }
                        }
                    });
            });
            if let Some(s) = chosen {
                self.apply(&s);
            }
        });

        ui.horizontal(|ui| {
            ui.label("Felvétel neve:");
            ui.add_enabled(
                !recording,
                egui::TextEdit::singleline(&mut self.rec_name).hint_text("üresen: automatikus (dátum, idő)"),
            );
            if !recording {
                if ui.button("⏺ Record Start").clicked() {
                    let name = if self.rec_name.trim().is_empty() {
                        chrono::Local::now().format("rec_%Y%m%d_%H%M%S").to_string()
                    } else {
                        self.rec_name.trim().to_string()
                    };
                    match self.engine.start_recording(&name) {
                        Ok(info) => {
                            self.message = format!("Felvétel: {}", info.video);
                            self.recording = Some((info, Instant::now()));
                        }
                        Err(e) => self.message = format!("Hiba: {e}"),
                    }
                }
            } else if ui.button("⏹ Record End").clicked() {
                self.engine.stop_recording();
                if let Some((info, _)) = self.recording.take() {
                    self.message = format!(
                        "Felvétel leállítva: {} – {} képkocka, {} eldobva",
                        info.video,
                        info.stats.queued.load(Relaxed),
                        info.stats.dropped.load(Relaxed)
                    );
                }
                self.rec_name.clear();
            }
        });

        self.status_line(ui);
        ui.separator();
        self.view_controls(ui);
        self.preview(ui);
    }

    fn status_line(&mut self, ui: &mut egui::Ui) {
        let st = self.engine.stats();
        let mut line = format!(
            "{} · fogadott: {} · kameránál elveszett: {} · hibás kép: {}",
            self.engine.camera_name(),
            st.received.load(Relaxed),
            st.lost_at_camera.load(Relaxed),
            self.engine.camera_errors()
        );
        if let Some((info, started)) = &self.recording {
            line += &format!(
                " · ⏺ {:.0} s · írva: {} · sorban: {} · eldobva: {}",
                started.elapsed().as_secs_f64(),
                info.stats.written.load(Relaxed),
                info.stats.queued.load(Relaxed) - info.stats.written.load(Relaxed),
                info.stats.dropped.load(Relaxed)
            );
            if let Some(enc) = *info.stats.encoder.lock().unwrap() {
                line += &format!(" · {enc}");
            }
            if let Some(n) = info.stats.format_note.lock().unwrap().as_ref() {
                line += &format!(" · {n}");
            }
            if let Some(e) = info.stats.error.lock().unwrap().as_ref() {
                line += &format!(" · HIBA: {e}");
            }
        }
        let fin = self.engine.finalizing();
        if fin > 0 {
            line += &format!(" · lezárás alatt: {fin}");
        }
        ui.label(line);
        if !self.message.is_empty() {
            ui.label(&self.message);
        }
    }

    fn settings_page(&mut self, ui: &mut egui::Ui) {
        let recording = self.recording.is_some() || self.test.is_some();
        egui::Panel::left("scenario_list").resizable(false).show(ui, |ui| {
            ui.heading("Szcenáriók");
            let mut pick = None;
            for s in &self.scenarios {
                if ui.selectable_label(s.name == self.edit.name, &s.name).clicked() {
                    pick = Some(s.clone());
                }
            }
            if let Some(s) = pick {
                self.extra_json = extra_to_json(&s);
                self.edit = s;
            }
            ui.separator();
            if ui.button("Új").clicked() {
                self.edit = Scenario { name: "uj_szcenario".into(), ..self.edit.clone() };
            }
        });

        ui.add_enabled_ui(!recording, |ui| {
            egui::Grid::new("settings").num_columns(2).show(ui, |ui| {
                let e = &mut self.edit;
                ui.label("Név");
                ui.text_edit_singleline(&mut e.name);
                ui.end_row();

                ui.label("Kamera");
                ui.horizontal(|ui| {
                    egui::ComboBox::from_id_salt("device").selected_text(e.device.label()).show_ui(ui, |ui| {
                        for d in &self.devices {
                            ui.selectable_value(&mut e.device, d.clone(), d.label());
                        }
                    });
                    if ui.button("⟳").on_hover_text("Kameralista frissítése").clicked() {
                        self.devices = find_devices();
                    }
                });
                ui.end_row();

                ui.label("Pixelformátum");
                egui::ComboBox::from_id_salt("pixfmt").selected_text(e.camera.pixel_format.name()).show_ui(ui, |ui| {
                    for f in PixelFormat::ALL {
                        ui.selectable_value(&mut e.camera.pixel_format, f, f.name());
                    }
                });
                ui.end_row();

                ui.label("Szélesség × magasság");
                ui.horizontal(|ui| {
                    ui.add(egui::DragValue::new(&mut e.camera.width).range(8..=16384));
                    ui.add(egui::DragValue::new(&mut e.camera.height).range(8..=16384));
                });
                ui.end_row();

                ui.label("Eltolás X, Y");
                ui.horizontal(|ui| {
                    ui.add(egui::DragValue::new(&mut e.camera.offset_x));
                    ui.add(egui::DragValue::new(&mut e.camera.offset_y));
                });
                ui.end_row();

                ui.label("Kép tükrözése (kamerán)");
                ui.horizontal(|ui| {
                    ui.checkbox(&mut e.camera.reverse_x, "vízszintesen");
                    ui.checkbox(&mut e.camera.reverse_y, "függőlegesen");
                })
                .response
                .on_hover_text("Mindkettő: 180°-os forgatás. A felvételre is hat.");
                ui.end_row();

                ui.label("Képfrekvencia [fps]");
                ui.add(egui::DragValue::new(&mut e.camera.frame_rate).range(0.1..=1000.0).speed(0.5));
                ui.end_row();

                ui.label("Automatikus expozíció");
                ui.checkbox(&mut e.camera.auto_exposure, "a kamera állítja az expozíciót és az erősítést")
                    .on_hover_text("A felvételre is hat: a nyers jel fényessége menet közben változhat.");
                ui.end_row();

                let manual = !e.camera.auto_exposure;
                ui.label("Expozíció [µs]");
                ui.add_enabled(manual, egui::DragValue::new(&mut e.camera.exposure_us).range(1.0..=10_000_000.0).speed(10.0));
                ui.end_row();

                ui.label("Erősítés [dB]");
                ui.add_enabled(manual, egui::DragValue::new(&mut e.camera.gain_db).range(0.0..=48.0).speed(0.1));
                ui.end_row();

                ui.label("Kódoló");
                egui::ComboBox::from_id_salt("encoder").selected_text(&e.recording.encoder).show_ui(ui, |ui| {
                    ui.selectable_value(&mut e.recording.encoder, "auto".to_string(), "auto");
                    for p in encoder::available() {
                        ui.selectable_value(&mut e.recording.encoder, p.id.to_string(), p.label);
                    }
                });
                ui.end_row();

                ui.label("Élőkép LUT (S-Log)");
                let current = e.preview_lut.clone().unwrap_or_else(|| "nincs".into());
                egui::ComboBox::from_id_salt("lut").selected_text(current).show_ui(ui, |ui| {
                    ui.selectable_value(&mut e.preview_lut, None, "nincs");
                    for name in lut::list(&self.lut_dir) {
                        ui.selectable_value(&mut e.preview_lut, Some(name.clone()), name);
                    }
                });
                ui.end_row();

                ui.label("Kimeneti mappa");
                let mut dir = e.recording.output_dir.display().to_string();
                if ui.text_edit_singleline(&mut dir).changed() {
                    e.recording.output_dir = dir.into();
                }
                ui.end_row();

                ui.label("További kamerabeállítások\n(JSON: név → érték)");
                ui.add(egui::TextEdit::multiline(&mut self.extra_json).code_editor().desired_rows(3));
                ui.end_row();
            });

            ui.horizontal(|ui| {
                if ui.button("Kipróbálás").clicked()
                    && let Some(s) = self.edited()
                {
                    self.apply(&s);
                }
                if ui.button("Mentés").clicked()
                    && let Some(s) = self.edited()
                {
                    self.message = match scenario::save(&self.dir, &s) {
                        Ok(p) => format!("Mentve: {}", p.display()),
                        Err(e) => format!("Hiba mentéskor: {e}"),
                    };
                    self.reload();
                }
                if ui.button("Törlés").clicked() {
                    self.message = match scenario::delete(&self.dir, &self.edit.name) {
                        Ok(()) => format!("Törölve: {}", self.edit.name),
                        Err(e) => format!("Hiba törléskor: {e}"),
                    };
                    self.reload();
                }
            });

            ui.horizontal(|ui| {
                if ui
                    .button("Felvételi teszt")
                    .on_hover_text("Rögzít a megadott ideig, ellenőrzi, hogy nem veszett-e el képkocka, és hogy a kódoló bírja-e a tempót. A tesztfájlokat utána törli.")
                    .clicked()
                    && let Some(s) = self.edited()
                {
                    self.start_test(&s);
                }
                ui.add(egui::DragValue::new(&mut self.test_secs).range(2.0..=600.0).suffix(" s"));
            });
        });
        if let Some(t) = &self.test {
            ui.label(format!("Felvételi teszt fut… {:.0} s", t.started.elapsed().as_secs_f64()));
        }
        if let Some((ok, text)) = &self.test_result {
            let color = if *ok { egui::Color32::from_rgb(40, 160, 70) } else { egui::Color32::from_rgb(200, 60, 50) };
            ui.colored_label(color, text);
        }

        self.status_line(ui);
        ui.separator();
        self.view_controls(ui);
        self.preview(ui);
    }

    fn start_test(&mut self, s: &Scenario) {
        self.test_result = None;
        if let Err(e) = self.engine.apply(s) {
            self.test_result = Some((false, format!("A szcenárió nem indul: {e}")));
            return;
        }
        let st = self.engine.stats();
        let lost_before = st.lost_at_camera.load(Relaxed);
        let name = chrono::Local::now().format("_teszt_%Y%m%d_%H%M%S").to_string();
        match self.engine.start_recording(&name) {
            Ok(info) => {
                let c = &s.camera;
                self.test = Some(RecTest {
                    info,
                    started: Instant::now(),
                    stopped: None,
                    secs: self.test_secs,
                    lost_before,
                    frame_bytes: Frame::byte_size(c.width, c.height, c.pixel_format),
                })
            }
            Err(e) => self.test_result = Some((false, format!("A felvétel nem indul: {e}"))),
        }
    }

    /// Advance the recording test: stop after its time, evaluate once the
    /// files are complete, then delete them.
    fn poll_test(&mut self) {
        let Some(t) = &mut self.test else { return };
        if t.stopped.is_none() && t.started.elapsed().as_secs_f64() >= t.secs {
            self.engine.stop_recording();
            t.stopped = Some(Instant::now());
        }
        let Some(stopped) = t.stopped else { return };
        if !t.info.stats.finished.load(Relaxed) {
            return;
        }
        let t = self.test.take().unwrap();
        let st = &t.info.stats;
        let lost = self.engine.stats().lost_at_camera.load(Relaxed) - t.lost_before;
        self.test_result = Some(evaluate(&t, st, lost, stopped.elapsed().as_secs_f64(), self.engine.camera_errors()));
        let _ = std::fs::remove_file(&t.info.video);
        let _ = std::fs::remove_file(&t.info.csv);
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
        self.watch_camera();
        self.update_preview(&ctx);
        self.poll_test();
        egui::Panel::top("tabs").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.page, Page::Main, "Főoldal");
                ui.selectable_value(&mut self.page, Page::Settings, "Beállítások");
            });
        });
        egui::CentralPanel::default().show(ui, |ui| match self.page {
            Page::Main => self.main_page(ui),
            Page::Settings => self.settings_page(ui),
        });
        ctx.request_repaint_after(PREVIEW_INTERVAL);
    }

    fn on_exit(&mut self) {
        self.engine.finish_recording();
    }
}

struct RecTest {
    info: RecordingInfo,
    started: Instant,
    stopped: Option<Instant>,
    secs: f64,
    lost_before: u64,
    frame_bytes: usize,
}

/// Verdict of a recording test. `finish_s`: time from stop until the files
/// were complete (how far the encoder was behind).
fn evaluate(t: &RecTest, st: &RecStats, lost: u64, finish_s: f64, camera_errors: u64) -> (bool, String) {
    let written = st.written.load(Relaxed);
    let dropped = st.dropped.load(Relaxed);
    let frames = st.queued.load(Relaxed) + dropped;
    let cam_fps = frames as f64 / t.secs;
    let enc_fps = written as f64 / (t.secs + finish_s);
    let encoder = st.encoder.lock().unwrap().unwrap_or("-");
    let mut text = format!(
        "{frames} kocka ({cam_fps:.1} fps), elveszett a kameránál: {lost}, hibás: {camera_errors}, eldobva: {dropped}, \
         kódoló ({encoder}): ~{enc_fps:.0} fps, lezárás a leállítás után: {finish_s:.1} s"
    );
    if let Some(e) = st.error.lock().unwrap().as_ref() {
        return (false, format!("HIBA: {e}\n{text}"));
    }
    if dropped > 0 || lost > 0 || camera_errors > 0 {
        return (false, format!("NEM JÓ: képkockák vesztek el.\n{text}"));
    }
    // No loss, but an encoder slower than the camera only lasts until the
    // queue is full.
    if finish_s > 1.0 && enc_fps < cam_fps {
        let queue = (crate::recorder::QUEUE_BUDGET_BYTES / t.frame_bytes.max(1)) as f64;
        let lasts = queue / (cam_fps - enc_fps);
        text = format!(
            "RÖVID FELVÉTELHEZ JÓ: a kódoló lassabb a kameránál, kb. {lasts:.0} s felvétel után veszne el képkocka.\n{text}"
        );
        return (false, text);
    }
    (true, format!("OK: nem veszett el képkocka, a kódoló bírja a tempót.\n{text}"))
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
