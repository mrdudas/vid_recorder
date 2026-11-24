mod camera;
mod encoder;
mod engine;
mod frame;
mod lut;
mod recorder;
mod scenario;
mod ui;

use std::path::PathBuf;
use std::process::{Command, ExitCode};
use std::sync::atomic::Ordering::Relaxed;
use std::time::Duration;

use camera::SimCamera;
use engine::Engine;
use frame::PixelFormat;
use scenario::Scenario;

const USAGE: &str = "\
usage:
  vid_recorder                 start the GUI (simulated camera for now)
  vid_recorder encoders        list encoders that work on this machine
  vid_recorder luts            list and check the preview LUTs in slog/
  vid_recorder record-test [seconds=10] [fps=100] [WxH=1920x1200] [format=Mono8] [encoder=auto]
                               record from the simulated camera and verify
                               that no frame was lost (MKV + CSV)";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        None => gui(),
        Some("encoders") => {
            list_encoders();
            Ok(true)
        }
        Some("record-test") => record_test(&args[1..]),
        Some("luts") => Ok(check_luts()),
        _ => {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn scenario_dir() -> PathBuf {
    PathBuf::from("scenarios")
}

fn gui() -> Result<bool, String> {
    let engine = Engine::new(Box::new(SimCamera::new()));
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default().with_inner_size([1200.0, 850.0]),
        ..Default::default()
    };
    eframe::run_native(
        "vid_recorder",
        options,
        Box::new(|_cc| Ok(Box::new(ui::App::new(engine, scenario_dir(), PathBuf::from("slog"))))),
    )
    .map_err(|e| e.to_string())?;
    Ok(true)
}

fn check_luts() -> bool {
    let dir = PathBuf::from("slog");
    let names = lut::list(&dir);
    let mut ok = true;
    for name in &names {
        match lut::Lut::load(&dir.join(name)) {
            Ok(_) => println!("  ok  {name}"),
            Err(e) => {
                ok = false;
                println!("  ERR {e}");
            }
        }
    }
    println!("{} LUT(s) in {}", names.len(), dir.display());
    ok
}

fn list_encoders() {
    println!("ffmpeg: {}", encoder::ffmpeg_path().display());
    let avail = encoder::available();
    for p in encoder::PROFILES {
        let ok = avail.iter().any(|a| a.id == p.id);
        let kind = if p.hardware { "hw" } else { "sw" };
        println!("  [{}] {:<18} {kind}  {}", if ok { "x" } else { " " }, p.id, p.label);
    }
    for bits in [8, 12] {
        match encoder::resolve("auto", bits) {
            Ok(p) => println!("auto, {bits} bit -> {}", p.id),
            Err(e) => println!("auto, {bits} bit -> {e}"),
        }
    }
}

fn record_test(args: &[String]) -> Result<bool, String> {
    let arg = |i: usize, default: &str| args.get(i).cloned().unwrap_or_else(|| default.to_string());
    let seconds: f64 = arg(0, "10").parse().map_err(|e| format!("seconds: {e}"))?;
    let fps: f64 = arg(1, "100").parse().map_err(|e| format!("fps: {e}"))?;
    let size = arg(2, "1920x1200");
    let (w, h) = size.split_once('x').ok_or("size must be WxH")?;
    let fmt_name = arg(3, "Mono8");
    let format = PixelFormat::ALL
        .into_iter()
        .find(|f| f.name().eq_ignore_ascii_case(&fmt_name))
        .ok_or_else(|| format!("unknown pixel format {fmt_name}"))?;

    let mut s = Scenario::default();
    s.name = "record-test".into();
    s.camera.width = w.parse().map_err(|e| format!("width: {e}"))?;
    s.camera.height = h.parse().map_err(|e| format!("height: {e}"))?;
    s.camera.frame_rate = fps;
    s.camera.pixel_format = format;
    s.recording.encoder = arg(4, "auto");

    let mut engine = Engine::new(Box::new(SimCamera::new()));
    engine.apply(&s)?;
    let name = chrono::Local::now().format("test_%Y%m%d_%H%M%S").to_string();
    let info = engine.start_recording(&name)?;
    eprintln!("recording {} with {} for {seconds}s ({}x{} {} @ {fps} fps)", info.video, info.encoder, w, h, format.name());
    std::thread::sleep(Duration::from_secs_f64(seconds));
    let stop = std::time::Instant::now();
    engine.finish_recording();
    let drain = stop.elapsed().as_secs_f64();
    let lost_camera = engine.stats().lost_at_camera.load(Relaxed);
    drop(engine);

    let st = &info.stats;
    let (queued, written, dropped) = (st.queued.load(Relaxed), st.written.load(Relaxed), st.dropped.load(Relaxed));
    let csv_rows = std::fs::read_to_string(&info.csv)
        .map(|c| c.lines().count().saturating_sub(1) as u64)
        .unwrap_or(0);
    let video_frames = count_video_frames(&info.video);

    println!("frames to recorder  {}", queued + dropped);
    println!("written             {written}");
    println!("dropped (queue)     {dropped}");
    println!("lost at camera      {lost_camera}");
    println!("csv rows            {csv_rows}");
    println!("finish after stop   {drain:.2} s (large = encoder slower than camera)");
    match video_frames {
        Some(n) => println!("frames in video     {n}"),
        None => println!("frames in video     ? (ffprobe failed)"),
    }
    if let Some(e) = st.error.lock().unwrap().as_ref() {
        println!("error               {e}");
    }
    let ok = dropped == 0
        && lost_camera == 0
        && written == queued
        && csv_rows == written
        && video_frames.is_none_or(|n| n == written)
        && st.error.lock().unwrap().is_none();
    println!("{}", if ok { "OK: no frame lost" } else { "FAIL" });
    Ok(ok)
}

/// Packet count of the first video stream via ffprobe (next to ffmpeg).
fn count_video_frames(path: &str) -> Option<u64> {
    let ffmpeg = encoder::ffmpeg_path();
    let ffprobe = ffmpeg.with_file_name(if cfg!(windows) { "ffprobe.exe" } else { "ffprobe" });
    let out = Command::new(ffprobe)
        .args(["-v", "error", "-select_streams", "v:0", "-count_packets"])
        .args(["-show_entries", "stream=nb_read_packets", "-of", "csv=p=0", path])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}
