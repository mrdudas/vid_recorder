//! Recording tests: record a scenario for a few seconds and judge whether
//! it keeps every frame. Camera discovery runs one test per mode the camera
//! reports (`plan`), so all usable settings are found without trying them
//! by hand.

use std::sync::atomic::Ordering::Relaxed;
use std::time::{Duration, Instant};

use crate::camera::Mode;
use crate::engine::{Engine, RecordingInfo};
use crate::frame::{Frame, PixelFormat};
use crate::recorder::QUEUE_BUDGET_BYTES;
use crate::scenario::Scenario;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Level {
    /// No frame lost, encoder keeps up.
    Ok,
    /// No frame lost yet, but the encoder is slower than the camera.
    Short,
    Fail,
}

#[derive(Clone, Debug)]
pub struct Verdict {
    pub level: Level,
    /// First line: the verdict; second line: the numbers.
    pub text: String,
    /// The encoder could not keep up (a faster encoder may help).
    pub encoder_slow: bool,
}

impl Verdict {
    pub fn fail(text: String) -> Verdict {
        Verdict { level: Level::Fail, text, encoder_slow: false }
    }
}

/// A running recording test.
pub struct Test {
    info: RecordingInfo,
    started: Instant,
    stopped: Option<Instant>,
    secs: f64,
    fps: f64,
    frame_bytes: usize,
}

impl Test {
    /// Switch to `s` and start recording it for `secs` seconds.
    pub fn start(engine: &mut Engine, s: &Scenario, secs: f64) -> Result<Test, Verdict> {
        engine.apply(s).map_err(|e| Verdict::fail(format!("A beállítás nem megy: {e}")))?;
        let name = chrono::Local::now().format("_teszt_%Y%m%d_%H%M%S_%3f").to_string();
        let info = engine.start_recording(&name).map_err(|e| Verdict::fail(format!("A felvétel nem indul: {e}")))?;
        let c = &s.camera;
        Ok(Test {
            info,
            started: Instant::now(),
            stopped: None,
            secs,
            fps: c.frame_rate,
            frame_bytes: Frame::byte_size(c.width, c.height, c.pixel_format),
        })
    }

    pub fn elapsed(&self) -> f64 {
        self.started.elapsed().as_secs_f64()
    }

    /// Advance the test without blocking. Returns the verdict once the files
    /// are complete; they are deleted then.
    pub fn poll(&mut self, engine: &mut Engine) -> Option<Verdict> {
        if self.stopped.is_none() && self.elapsed() >= self.secs {
            engine.stop_recording();
            self.stopped = Some(Instant::now());
        }
        let stopped = self.stopped?;
        let st = &self.info.stats;
        if !st.finished.load(Relaxed) {
            return None;
        }
        let lost = engine.stats().lost_at_camera.load(Relaxed);
        let v = self.evaluate(lost, stopped.elapsed().as_secs_f64(), engine.camera_errors());
        let _ = std::fs::remove_file(&self.info.video);
        let _ = std::fs::remove_file(&self.info.csv);
        Some(v)
    }

    /// Run to the end (command line).
    pub fn run(engine: &mut Engine, s: &Scenario, secs: f64) -> Verdict {
        let mut t = match Test::start(engine, s, secs) {
            Ok(t) => t,
            Err(v) => return v,
        };
        loop {
            if let Some(v) = t.poll(engine) {
                return v;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// `finish_s`: time from stop until the files were complete (how far the
    /// encoder was behind).
    fn evaluate(&self, lost: u64, finish_s: f64, camera_errors: u64) -> Verdict {
        let st = &self.info.stats;
        let written = st.written.load(Relaxed);
        let dropped = st.dropped.load(Relaxed);
        let frames = st.queued.load(Relaxed) + dropped;
        let cam_fps = frames as f64 / self.secs;
        let enc_fps = written as f64 / (self.secs + finish_s);
        let encoder = st.encoder.lock().unwrap().unwrap_or("-");
        let numbers = format!(
            "{frames} kocka ({cam_fps:.1} fps), elveszett a kameránál: {lost}, hibás: {camera_errors}, eldobva: {dropped}, \
             kódoló ({encoder}): ~{enc_fps:.0} fps, lezárás a leállítás után: {finish_s:.1} s"
        );
        let fail = |why: &str| Verdict::fail(format!("NEM JÓ: {why}\n{numbers}"));
        if let Some(e) = st.error.lock().unwrap().as_ref() {
            return fail(&format!("hiba: {e}"));
        }
        if frames == 0 {
            return fail("nem jött képkocka a kamerából.");
        }
        if dropped > 0 || lost > 0 || camera_errors > 0 {
            let mut v = fail("képkockák vesztek el.");
            v.encoder_slow = dropped > 0 && lost == 0 && camera_errors == 0;
            return v;
        }
        if let Some(note) = st.format_note.lock().unwrap().as_ref() {
            return fail(&format!("a kamera más formátumot ad: {note}"));
        }
        if cam_fps < self.fps * 0.95 {
            return fail(&format!("a kamera csak {cam_fps:.1} fps-t ad a kért {} helyett.", self.fps));
        }
        // No loss, but an encoder slower than the camera only lasts until
        // the queue is full.
        if finish_s > 1.0 && enc_fps < cam_fps {
            let queue = (QUEUE_BUDGET_BYTES / self.frame_bytes.max(1)) as f64;
            let lasts = queue / (cam_fps - enc_fps) + self.secs;
            return Verdict {
                encoder_slow: true,
                level: Level::Short,
                text: format!(
                    "RÖVID FELVÉTELHEZ JÓ: a kódoló lassabb a kameránál, kb. {lasts:.0} s után veszne el képkocka.\n{numbers}"
                ),
            };
        }
        Verdict {
            level: Level::Ok,
            text: format!("OK: nem veszett el képkocka, a kódoló bírja a tempót.\n{numbers}"),
            encoder_slow: false,
        }
    }
}

/// One test scenario per mode, based on `base` (device, exposure, gain,
/// extra features, output dir), with the "auto" encoder.
pub fn plan(base: &Scenario, modes: &[Mode]) -> Vec<Scenario> {
    modes
        .iter()
        .map(|m| {
            let mut s = base.clone();
            s.camera.pixel_format = m.format;
            s.camera.width = m.width;
            s.camera.height = m.height;
            s.camera.offset_x = 0;
            s.camera.offset_y = 0;
            s.camera.frame_rate = m.fps;
            s.recording.encoder = "auto".into();
            s.name = format!("{}_{}x{}_{}fps", m.format.name(), m.width, m.height, m.fps);
            s
        })
        .collect()
}

/// After a failed "auto" test: the same scenario uncompressed, if that can
/// help (encoder too slow, not the camera).
pub fn raw_retry(s: &Scenario, v: &Verdict) -> Option<Scenario> {
    if s.recording.encoder != "auto" || s.camera.pixel_format == PixelFormat::Mjpeg || !v.encoder_slow {
        return None;
    }
    let mut r = s.clone();
    r.recording.encoder = "raw".into();
    Some(r)
}
