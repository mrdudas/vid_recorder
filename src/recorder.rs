//! Recording: frames go through a queue to a writer thread that feeds ffmpeg
//! (MKV) and writes the timestamp CSV next to it.
//!
//! The acquisition thread never blocks on disk or encoder: it only pushes
//! into the queue. The queue is sized from a memory budget so that encoder or
//! disk stalls are absorbed. If it ever overflows the frame is counted as
//! dropped, which must not happen in a correctly sized setup.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use crate::encoder::{self, Profile};
use crate::frame::{BufferPool, Frame, PixelFormat};

/// Memory the recording queue may hold before frames are dropped.
pub const QUEUE_BUDGET_BYTES: usize = 2 << 30;

#[derive(Default)]
pub struct RecStats {
    pub written: AtomicU64,
    pub dropped: AtomicU64,
    pub queued: AtomicU64,
    pub finished: AtomicBool,
    pub error: Mutex<Option<String>>,
    /// Encoder actually used (chosen from the first frame's format).
    pub encoder: Mutex<Option<&'static str>>,
    /// Set when the camera delivers another format than the scenario asked for.
    pub format_note: Mutex<Option<String>>,
}

pub struct Recorder {
    tx: Option<SyncSender<Frame>>,
    pub stats: Arc<RecStats>,
    pub video_path: PathBuf,
    pub csv_path: PathBuf,
    thread: Option<JoinHandle<()>>,
}

impl Recorder {
    /// Create `<dir>/<name>.mkv` (`.nut` for uncompressed) and `<dir>/<name>.csv`. ffmpeg starts with the
    /// first frame, so the stream format always matches the actual frames.
    pub fn start(
        dir: &Path,
        name: &str,
        encoder_id: &str,
        format: PixelFormat,
        frame_bytes: usize,
        fps: f64,
        pool: BufferPool,
    ) -> Result<Recorder, String> {
        // Fail early if nothing can encode the requested format; the encoder
        // is chosen again from the first frame's actual format.
        let container = encoder::resolve(encoder_id, format)?.container;
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let video_path = dir.join(format!("{name}.{container}"));
        let csv_path = dir.join(format!("{name}.csv"));
        if video_path.exists() || csv_path.exists() {
            return Err(format!("{} already exists", video_path.display()));
        }
        let mut csv = BufWriter::with_capacity(1 << 20, File::create(&csv_path).map_err(|e| e.to_string())?);
        writeln!(csv, "timestamp,frameID").map_err(|e| e.to_string())?;

        let capacity = (QUEUE_BUDGET_BYTES / frame_bytes.max(1)).clamp(16, 100_000);
        let (tx, rx) = sync_channel(capacity);
        let stats = Arc::new(RecStats::default());
        let w = Writer {
            rx,
            csv,
            pool,
            stats: stats.clone(),
            video_path: video_path.clone(),
            encoder_id: encoder_id.to_string(),
            requested: format,
            fps,
            ffmpeg: None,
        };
        let thread = std::thread::Builder::new()
            .name("recorder".into())
            .spawn(move || w.run())
            .map_err(|e| e.to_string())?;
        Ok(Recorder { tx: Some(tx), stats, video_path, csv_path, thread: Some(thread) })
    }

    /// Hand a frame to the writer. Never blocks.
    pub fn push(&self, frame: Frame, pool: &BufferPool) {
        let Some(tx) = &self.tx else { return };
        match tx.try_send(frame) {
            Ok(()) => {
                self.stats.queued.fetch_add(1, Relaxed);
            }
            Err(TrySendError::Full(f) | TrySendError::Disconnected(f)) => {
                self.stats.dropped.fetch_add(1, Relaxed);
                pool.put(f.data);
            }
        }
    }

    /// Stop accepting frames. The writer drains the queue and finalizes the
    /// files in the background; `stats.finished` turns true when done.
    pub fn stop(&mut self) {
        self.tx = None;
    }

    /// Stop and wait until the files are complete.
    pub fn finish(mut self) {
        self.stop();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

struct Writer {
    rx: Receiver<Frame>,
    csv: BufWriter<File>,
    pool: BufferPool,
    stats: Arc<RecStats>,
    video_path: PathBuf,
    encoder_id: String,
    requested: PixelFormat,
    fps: f64,
    ffmpeg: Option<(Child, ChildStdin)>,
}

impl Writer {
    fn run(mut self) {
        while let Ok(frame) = self.rx.recv() {
            if let Err(e) = self.write(&frame) {
                self.fail(e);
            }
            self.pool.put(frame.data);
        }
        if let Err(e) = self.csv.flush() {
            self.fail(format!("csv: {e}"));
        }
        if let Some((child, stdin)) = self.ffmpeg.take() {
            drop(stdin);
            match child.wait_with_output() {
                Ok(out) if !out.status.success() => {
                    self.fail(format!("ffmpeg: {}", String::from_utf8_lossy(&out.stderr).trim()))
                }
                Err(e) => self.fail(format!("ffmpeg: {e}")),
                _ => {}
            }
        }
        self.stats.finished.store(true, Relaxed);
    }

    fn fail(&self, msg: String) {
        let mut err = self.stats.error.lock().unwrap();
        if err.is_none() {
            eprintln!("recording error: {msg}");
            *err = Some(msg);
        }
    }

    fn write(&mut self, f: &Frame) -> Result<(), String> {
        if self.ffmpeg.is_none() {
            self.ffmpeg = Some(self.spawn_ffmpeg(f)?);
        }
        let (_, stdin) = self.ffmpeg.as_mut().unwrap();
        stdin.write_all(&f.data).map_err(|e| format!("ffmpeg input: {e}"))?;
        writeln!(self.csv, "{},{}", f.timestamp_ns, f.id).map_err(|e| format!("csv: {e}"))?;
        self.stats.written.fetch_add(1, Relaxed);
        Ok(())
    }

    fn spawn_ffmpeg(&self, f: &Frame) -> Result<(Child, ChildStdin), String> {
        if f.format != self.requested {
            let note = format!("a kamera {}-t ad (kért: {})", f.format.name(), self.requested.name());
            eprintln!("note: {note}");
            *self.stats.format_note.lock().unwrap() = Some(note);
        }
        let encoder: &Profile = encoder::resolve(&self.encoder_id, f.format)?;
        *self.stats.encoder.lock().unwrap() = Some(encoder.label);
        let mut cmd = encoder::low_priority(&encoder::ffmpeg_path());
        cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin"]);
        if f.format == PixelFormat::Mjpeg {
            // Concatenated JPEG frames, copied into the MKV unchanged.
            cmd.args(["-f", "mjpeg"]);
        } else {
            // FFVHuff keeps >8-bit samples bit-exact only as plain 16-bit
            // words; the PIXEL_FORMAT tag tells the real depth.
            let pix_fmt = if encoder.id == "ffvhuff" && f.format.bits() > 8 { "gray16le" } else { f.format.ffmpeg_input() };
            cmd.args(["-f", "rawvideo", "-pix_fmt", pix_fmt])
                .args(["-video_size", &format!("{}x{}", f.width, f.height)]);
        }
        let mut child = cmd
            .args(["-framerate", &format!("{}", self.fps)])
            .args(["-i", "pipe:0"])
            .args(encoder.args)
            // Bayer is stored as grey; the tag tells the export what it is.
            .args(["-metadata:s:v:0", &format!("PIXEL_FORMAT={}", f.format.name())])
            .arg("-y")
            .arg(&self.video_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("cannot start ffmpeg ({}): {e}", encoder::ffmpeg_path().display()))?;
        let stdin = child.stdin.take().unwrap();
        Ok((child, stdin))
    }
}
