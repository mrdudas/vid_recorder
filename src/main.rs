mod camera;
mod encoder;
mod engine;
mod export;
mod frame;
mod lut;
mod magnify;
mod recorder;
mod scenario;
mod u3v;
mod ui;
mod uvc;

use std::path::PathBuf;
use std::process::{Command, ExitCode};
use std::sync::atomic::Ordering::Relaxed;
use std::time::Duration;

use engine::Engine;
use frame::PixelFormat;
use scenario::Scenario;

const USAGE: &str = "\
usage:
  vid_recorder                 start the GUI
  vid_recorder --version       print the version
  vid_recorder cameras         list connected USB3 Vision and UVC cameras
  vid_recorder camera-test <scenario> [seconds=10]
                               record with a saved scenario (its camera)
                               and verify that no frame was lost
  vid_recorder encoders        list encoders that work on this machine
  vid_recorder luts            list and check the preview LUTs in slog/
  vid_recorder record-test [seconds=10] [fps=100] [WxH=1920x1200] [format=Mono8] [encoder=auto]
                               record from the simulated camera and verify
                               that no frame was lost (MKV + CSV)";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        None => gui(),
        Some("cameras") => {
            match u3v::list() {
                Ok(c) if c.is_empty() => println!("USB3 Vision: none"),
                Ok(c) => c.iter().for_each(|c| println!("USB3 Vision: {c}")),
                Err(e) => println!("USB3 Vision: {e}"),
            }
            for c in uvc::list() {
                println!("UVC: {c}");
            }
            Ok(true)
        }
        Some("camera-xml") => u3v::genicam_xml().map(|x| {
            println!("{x}");
            true
        }),
        Some("camera-test") => camera_test(&args[1..]),
        Some("--version") => {
            println!("{}", env!("CARGO_PKG_VERSION"));
            Ok(true)
        }
        Some("encoders") => {
            list_encoders();
            Ok(true)
        }
        Some("record-test") => record_test(&args[1..]),
        Some("luts") => Ok(check_luts()),
        Some("export") => export::run(&args[1..]),
        _ => {
            eprintln!("{USAGE}\n{}", export::USAGE);
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
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default().with_inner_size([1200.0, 850.0]),
        ..Default::default()
    };
    eframe::run_native(
        "vid_recorder",
        options,
        Box::new(|_cc| Ok(Box::new(ui::App::new(Engine::new(), scenario_dir(), PathBuf::from("slog"))))),
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
    for f in [PixelFormat::Mono8, PixelFormat::Mono12, PixelFormat::Yuyv, PixelFormat::Mjpeg] {
        match encoder::resolve("auto", f) {
            Ok(p) => println!("auto, {} -> {}", f.name(), p.id),
            Err(e) => println!("auto, {} -> {e}", f.name()),
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

    s.device = scenario::Device::Sim;
    run_and_verify(Engine::new(), &s, seconds)
}

fn camera_test(args: &[String]) -> Result<bool, String> {
    let name = args.first().ok_or("usage: camera-test <scenario> [seconds]")?;
    let seconds: f64 = args.get(1).map_or(Ok(10.0), |s| s.parse()).map_err(|e| format!("seconds: {e}"))?;
    let s = scenario::load_all(&scenario_dir())
        .into_iter()
        .find(|s| &s.name == name)
        .ok_or_else(|| format!("scenario {name} not found in {}", scenario_dir().display()))?;
    run_and_verify(Engine::new(), &s, seconds)
}

/// Start the scenario, record for `seconds`, then check that every frame
/// reached the MKV and the CSV.
fn run_and_verify(mut engine: Engine, s: &Scenario, seconds: f64) -> Result<bool, String> {
    let c = &s.camera;
    engine.apply(s)?;
    eprintln!("camera: {}", engine.camera_name());
    let name = chrono::Local::now().format("test_%Y%m%d_%H%M%S").to_string();
    let info = engine.start_recording(&name)?;
    eprintln!(
        "recording {} for {seconds}s ({}x{} {} @ {} fps)",
        info.video, c.width, c.height, c.pixel_format.name(), c.frame_rate
    );
    std::thread::sleep(Duration::from_secs_f64(seconds));
    let stop = std::time::Instant::now();
    engine.finish_recording();
    let drain = stop.elapsed().as_secs_f64();
    let lost_camera = engine.stats().lost_at_camera.load(Relaxed);
    let camera_errors = engine.camera_errors();
    drop(engine);

    let st = &info.stats;
    let (queued, written, dropped) = (st.queued.load(Relaxed), st.written.load(Relaxed), st.dropped.load(Relaxed));
    let csv_rows = std::fs::read_to_string(&info.csv)
        .map(|c| c.lines().count().saturating_sub(1) as u64)
        .unwrap_or(0);
    let video_frames = count_video_frames(&info.video);

    println!("encoder             {}", st.encoder.lock().unwrap().unwrap_or("-"));
    if let Some(n) = st.format_note.lock().unwrap().as_ref() {
        println!("format              {n}");
    }
    println!("frames to recorder  {}", queued + dropped);
    println!("written             {written}");
    println!("dropped (queue)     {dropped}");
    println!("lost at camera      {lost_camera}");
    println!("camera errors       {camera_errors}");
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
        && camera_errors == 0
        && written == queued
        && csv_rows == written
        && video_frames.is_none_or(|n| n == written)
        && st.error.lock().unwrap().is_none();
    println!("{}", if ok { "OK: no frame lost" } else { "FAIL" });
    Ok(ok)
}

/// Packet count of the first video stream via ffprobe (next to ffmpeg).
fn count_video_frames(path: &str) -> Option<u64> {
    let out = Command::new(encoder::ffprobe_path())
        .args(["-v", "error", "-select_streams", "v:0", "-count_packets"])
        .args(["-show_entries", "stream=nb_read_packets", "-of", "csv=p=0", path])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}
