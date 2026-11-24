//! Ties camera, preview and recorder together.
//!
//! On the acquisition thread every frame is: checked for id gaps, copied to
//! the preview slot if 40 ms passed since the last copy (max 25 fps), then
//! moved to the recorder if recording. Nothing there waits on the UI, the
//! encoder or the disk.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::camera::{self, Camera};
use crate::frame::{BufferPool, Frame};
use crate::recorder::{RecStats, Recorder};
use crate::scenario::{Device, Scenario};

pub const PREVIEW_INTERVAL: Duration = Duration::from_millis(40);

#[derive(Default)]
pub struct Stats {
    pub received: AtomicU64,
    /// Frames missing according to the camera's frame id sequence.
    pub lost_at_camera: AtomicU64,
}

struct Shared {
    pool: BufferPool,
    stats: Stats,
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
}

pub struct RecordingInfo {
    pub stats: Arc<RecStats>,
    pub video: String,
    pub csv: String,
}

impl Engine {
    pub fn new() -> Self {
        Engine {
            camera: None,
            shared: Arc::new(Shared {
                pool: BufferPool::default(),
                stats: Stats::default(),
                preview: Mutex::new(None),
                recorder: Mutex::new(None),
            }),
            scenario: None,
            finishing: Vec::new(),
        }
    }

    pub fn camera_name(&self) -> String {
        self.camera.as_ref().map_or("nincs kamera".into(), |(_, c)| c.name())
    }

    pub fn camera_errors(&self) -> u64 {
        self.camera.as_ref().map_or(0, |(_, c)| c.errors())
    }

    /// The camera was unplugged (see [`Engine::reconnect`]).
    pub fn camera_lost(&self) -> bool {
        self.camera.as_ref().is_some_and(|(_, c)| c.disconnected())
    }

    /// Close the camera and open it again with the scenario's settings, e.g.
    /// after it was unplugged and plugged back in.
    pub fn reconnect(&mut self, s: &Scenario) -> Result<(), String> {
        self.scenario = None;
        self.camera = None;
        self.apply(s)
    }

    pub fn stats(&self) -> &Stats {
        &self.shared.stats
    }

    /// Switch to a scenario: open its camera if it is a different device,
    /// then stop, apply the settings and restart.
    pub fn apply(&mut self, s: &Scenario) -> Result<(), String> {
        if self.is_recording() {
            return Err("cannot change scenario while recording".into());
        }
        self.scenario = None;
        if let Some((_, cam)) = self.camera.as_mut() {
            cam.stop();
        }
        if self.camera.as_ref().is_none_or(|(d, _)| *d != s.device) {
            self.camera = None; // close the old device first
            self.camera = Some((s.device.clone(), camera::open(&s.device)?));
        }
        let cam = &mut self.camera.as_mut().unwrap().1;
        cam.apply(&s.camera)?;
        let shared = self.shared.clone();
        shared.stats.received.store(0, Relaxed);
        shared.stats.lost_at_camera.store(0, Relaxed);
        let mut last_id: Option<u64> = None;
        let mut last_preview: Option<Instant> = None;
        let sink = Box::new(move |frame: Frame| {
            let st = &shared.stats;
            st.received.fetch_add(1, Relaxed);
            if let Some(prev) = last_id
                && frame.id > prev + 1
            {
                st.lost_at_camera.fetch_add(frame.id - prev - 1, Relaxed);
            }
            last_id = Some(frame.id);

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
        Ok(())
    }

    /// Take the latest preview frame, if a new one arrived.
    pub fn take_preview(&self) -> Option<Frame> {
        self.shared.preview.lock().unwrap().take()
    }

    /// Give a preview frame's buffer back for reuse.
    pub fn recycle(&self, f: Frame) {
        self.shared.pool.put(f.data);
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
            c.pixel_format,
            Frame::byte_size(c.width, c.height, c.pixel_format),
            c.frame_rate,
            self.shared.pool.clone(),
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
