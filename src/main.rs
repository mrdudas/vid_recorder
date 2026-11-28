mod camera;
mod control;
mod curve;
mod encoder;
mod engine;
mod export;
mod frame;
mod gpu_magnify;
mod gpu_view;
mod lut;
mod magnify;
mod probe;
mod stack;
mod dark;
mod recordings;
mod recorder;
mod scenario;
mod u3v;
mod ui;
mod uvc;

use std::path::PathBuf;
use std::process::ExitCode;
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
  vid_recorder record <scenario> [seconds]
                               record with a saved scenario until the time
                               is up or Enter is pressed (no GUI, e.g. over
                               ssh); waits for a missing camera and keeps
                               recording across unplug / replug
  vid_recorder screenshots [dir=screenshots] [camera|uvc]
                               walk through the GUI pages with the simulated
                               camera and save each page as PNG
  vid_recorder live-test <scenario|sim|u3v>
                               record ~12 s while exposure, gain and frame
                               rate change on the streaming camera; reports
                               write times, brightness and frame continuity
  vid_recorder probe <scenario|sim|u3v> [seconds=5]
                               ask the camera which formats, sizes and max
                               frame rates it supports and record-test each
                               (device, exposure, gain, extra features from
                               the scenario); retries uncompressed if the
                               encoder is too slow
  vid_recorder limits <scenario|u3v>
                               read the camera's limits (size, exposure,
                               gain, max fps per format and height) without
                               recording; takes about a second
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
        Some("record") => record(&args[1..]),
        Some("live-test") => live_test(&args[1..]),
        Some("probe") => probe_camera(&args[1..]),
        Some("screenshots") => screenshots(&args[1..]),
        Some("limits") => limits(&args[1..]),
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

/// Walk through the GUI pages with the simulated camera and save each as
/// PNG (for checking the layout without a camera or a screen at hand).
fn screenshots(args: &[String]) -> Result<bool, String> {
    let dir = PathBuf::from(args.first().map_or("screenshots", String::as_str));
    let camera = args.get(1).is_some_and(|a| a == "camera");
    let uvc = args.get(1).is_some_and(|a| a == "uvc");
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default().with_inner_size([1440.0, 900.0]),
        ..Default::default()
    };
    eframe::run_native(
        "vid_recorder",
        options,
        Box::new(move |cc| {
            let app = ui::App::new(Engine::new(), scenario_dir(), PathBuf::from("slog")).with_gpu(cc.wgpu_render_state.as_ref());
            Ok(Box::new(app.screenshots(dir, camera, uvc)))
        }),
    )
    .map_err(|e| e.to_string())?;
    Ok(true)
}

fn gui() -> Result<bool, String> {
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default().with_inner_size([1200.0, 850.0]).with_maximized(true),
        ..Default::default()
    };
    eframe::run_native(
        "vid_recorder",
        options,
        Box::new(|cc| Ok(Box::new(ui::App::new(Engine::new(), scenario_dir(), PathBuf::from("slog")).with_gpu(cc.wgpu_render_state.as_ref())))),
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

fn record(args: &[String]) -> Result<bool, String> {
    let name = args.first().ok_or("usage: record <scenario> [seconds]")?;
    let seconds: Option<f64> = args.get(1).map(|s| s.parse()).transpose().map_err(|e| format!("seconds: {e}"))?;
    let s = scenario_or_device(name)?;
    let mut engine = Engine::new();
    let wait = std::time::Instant::now();
    if let Err(e) = engine.apply(&s) {
        if !engine.waiting_for_camera() {
            return Err(e);
        }
        eprintln!("no camera ({e}), waiting…");
        while engine.waiting_for_camera() {
            if let Some(m) = engine.poll() {
                eprintln!("{m}");
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        engine.scenario.as_ref().ok_or("camera did not start")?;
        eprintln!("camera found after {:.0} s", wait.elapsed().as_secs_f64());
    }
    let rec = chrono::Local::now().format("rec_%Y%m%d_%H%M%S").to_string();
    let info = engine.start_recording(&rec)?;
    eprintln!("camera: {}\nrecording {} ({})", engine.camera_name(), info.video, seconds.map_or("Enter stops".into(), |s| format!("{s} s")));
    let (tx, rx) = std::sync::mpsc::channel();
    if seconds.is_none() {
        std::thread::spawn(move || {
            let _ = std::io::stdin().read_line(&mut String::new());
            let _ = tx.send(());
        });
    }
    let start = std::time::Instant::now();
    let mut report = std::time::Instant::now();
    // No window: the look (automatics) follows the preview frames here.
    let lut = s.preview_lut.as_ref().and_then(|n| Some((n.clone(), std::sync::Arc::new(lut::Lut::load(&PathBuf::from("slog").join(n)).ok()?))));
    while seconds.is_none_or(|s| start.elapsed().as_secs_f64() < s) && !matches!(rx.try_recv(), Ok(())) && engine.is_recording() {
        if let Some(f) = engine.take_preview() {
            if let Some(src) = gpu_view::Source::from_frame(&f) {
                engine.observe(&src, lut.clone());
                if let Err(e) = engine.regulate(control::Measure::of(&src)) {
                    eprintln!("expozíció: {e}");
                }
            }
            engine.recycle(f);
        }
        if let Some(m) = engine.poll() {
            eprintln!("{:.1} s: {m}", start.elapsed().as_secs_f64());
        }
        if report.elapsed() >= Duration::from_secs(5) {
            report = std::time::Instant::now();
            let st = &info.stats;
            eprintln!(
                "{:.0} s: written {} · queued {} · lost at camera {}{}",
                start.elapsed().as_secs_f64(),
                st.written.load(Relaxed),
                st.queued.load(Relaxed) - st.written.load(Relaxed),
                engine.stats().lost_at_camera.load(Relaxed),
                if engine.waiting_for_camera() { " · WAITING FOR CAMERA" } else { "" }
            );
            if let Some(c) = engine.scenario.as_ref().map(|s| &s.camera).filter(|c| c.program) {
                eprintln!("   expozíció {:.0} µs · erősítés {:.1} dB · {}", c.exposure_us, c.gain_db, engine.control.status);
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    engine.finish_recording();
    let st = &info.stats;
    println!("video               {}", info.video);
    println!("written             {}", st.written.load(Relaxed));
    println!("dropped (queue)     {}", st.dropped.load(Relaxed));
    println!("lost at camera      {}", engine.stats().lost_at_camera.load(Relaxed));
    if let Some(e) = st.error.lock().unwrap().as_ref() {
        println!("error               {e}");
    }
    Ok(st.error.lock().unwrap().is_none())
}

/// Scenario by name, or the defaults for "sim" / "u3v".
/// Exposure, gain and frame rate written while the camera streams and
/// records: how long each write takes, whether the picture follows, and
/// whether any frame goes missing (frame IDs in the CSV).
fn live_test(args: &[String]) -> Result<bool, String> {
    let mut s = scenario_or_device(args.first().ok_or("usage: live-test <scenario|sim|u3v>")?)?;
    s.camera.auto_exposure = false;
    s.recording.corrected = false;
    s.recording.curve8 = false;
    s.recording.encoder = "auto".into();
    let mut engine = Engine::new();
    let mut s2 = s.clone();
    let notes = engine.apply_fit(&mut s2)?;
    for n in notes {
        eprintln!("fit: {n}");
    }
    let (e0, g0, f0) = (s2.camera.exposure_us, s2.camera.gain_db, s2.camera.frame_rate);
    eprintln!("start: {e0:.0} µs, {g0:.1} dB, {f0:.2} fps, {}", s2.camera.pixel_format.name());
    let name = chrono::Local::now().format("_livetest_%H%M%S").to_string();
    let info = engine.start_recording(&name)?;
    // Mean brightness of the preview frames over a while.
    let mean = |engine: &Engine, secs: f64| -> f64 {
        let t = std::time::Instant::now();
        let (mut sum, mut n) = (0.0, 0);
        while t.elapsed().as_secs_f64() < secs {
            if let Some(f) = engine.take_preview() {
                if let Some(src) = gpu_view::Source::from_frame(&f) {
                    let (m, hist) = src.stats(None);
                    let tot: u64 = hist.iter().map(|&h| h as u64).sum();
                    let avg = m.map(|m| (m[0] + m[1] + m[2]) / 3.0).unwrap_or_else(|| {
                        hist.iter().enumerate().map(|(i, &h)| i as f64 * h as f64).sum::<f64>() / (tot.max(1) as f64 * (hist.len() - 1) as f64)
                    });
                    sum += avg;
                    n += 1;
                }
                engine.recycle(f);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        sum / n.max(1) as f64
    };
    let steps: Vec<(&str, Option<f64>, Option<f64>, Option<f64>)> = vec![
        ("alap", None, None, None),
        ("expozíció /2", Some(e0 / 2.0), None, None),
        ("expozíció vissza", Some(e0), None, None),
        ("erősítés +6 dB", None, Some(g0 + 6.0), None),
        ("erősítés vissza", None, Some(g0), None),
        ("fps /2", None, None, Some(f0 / 2.0)),
        ("fps vissza", None, None, Some(f0)),
        ("expozíció /4 + erősítés +12 dB", Some(e0 / 4.0), Some(g0 + 12.0), None),
        ("vissza", Some(e0), Some(g0), None),
    ];
    for (label, e, g, f) in steps {
        let t = std::time::Instant::now();
        let r = if e.is_none() && g.is_none() && f.is_none() { Ok((e0, g0, f0)) } else { engine.live_light(e, g, f) };
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        std::thread::sleep(Duration::from_millis(300));
        let m = mean(&engine, 1.0);
        match r {
            Ok((e, g, f)) => println!("{label:32} {ms:6.1} ms  →  {e:8.0} µs {g:5.1} dB {f:7.2} fps   átlag {:5.1}%", m * 100.0),
            Err(err) => println!("{label:32} {ms:6.1} ms  →  HIBA: {err}   átlag {:5.1}%", m * 100.0),
        }
    }
    engine.finish_recording();
    let st = &info.stats;
    // Frame continuity from the CSV.
    let csv = std::fs::read_to_string(&info.csv).map_err(|e| e.to_string())?;
    let rows: Vec<(u64, u64)> = csv.lines().skip(1).filter_map(|l| {
        let (t, i) = l.split_once(',')?;
        Some((t.parse().ok()?, i.parse().ok()?))
    }).collect();
    let gaps: u64 = rows.windows(2).map(|w| w[1].1.saturating_sub(w[0].1 + 1)).sum();
    let max_dt = rows.windows(2).map(|w| w[1].0.saturating_sub(w[0].0)).max().unwrap_or(0) as f64 / 1e6;
    println!("képek {} · sorszám-hiány {gaps} · eldobva (sor) {} · kamera vesztett {} · leghosszabb képköz {max_dt:.1} ms", rows.len(), st.dropped.load(Relaxed), engine.stats().lost_at_camera.load(Relaxed));
    for p in [&info.video, &info.csv] {
        let _ = std::fs::remove_file(p);
    }
    Ok(gaps == 0)
}

fn scenario_or_device(name: &str) -> Result<Scenario, String> {
    match scenario::load_all(&scenario_dir()).into_iter().find(|s| s.name == name) {
        Some(s) => Ok(s),
        None if name == "sim" => Ok(Scenario { device: scenario::Device::Sim, ..Scenario::default() }),
        None if name == "u3v" => Ok(Scenario { device: scenario::Device::U3v, ..Scenario::default() }),
        None if name.starts_with("uvc:") => {
            Ok(Scenario { device: scenario::Device::Uvc { name: name[4..].to_string() }, ..Scenario::default() })
        }
        None => Err(format!("scenario {name} not found in {}", scenario_dir().display())),
    }
}

fn limits(args: &[String]) -> Result<bool, String> {
    let s = scenario_or_device(args.first().ok_or("usage: limits <scenario|u3v>")?)?;
    let t = std::time::Instant::now();
    let l = Engine::new().limits(&s)?;
    let c = &s.camera;
    println!("read in {:.2} s", t.elapsed().as_secs_f64());
    println!("sensor        {}x{} (min {}x{}, steps W {} H {} X {} Y {})", l.sensor.0, l.sensor.1, l.min_size.0, l.min_size.1, l.step[0], l.step[1], l.step[2], l.step[3]);
    println!("exposure      {:.1} .. {:.0} µs", l.exposure_us.0, l.exposure_us.1);
    println!("gain          {:.1} .. {:.1} dB", l.gain_db.0, l.gain_db.1);
    println!("max fps       {} ({}x{}+{}+{} {})", l.max_fps.map_or("?".into(), |f| format!("{f:.1}")), c.width, c.height, c.offset_x, c.offset_y, c.pixel_format.name());
    print!("{:<14}", format!("width {}", c.width));
    l.heights.iter().for_each(|h| print!("{:>8}", format!("h{h}")));
    println!();
    for (f, row) in &l.table {
        print!("{:<14}", f.name());
        row.iter().for_each(|v| print!("{:>8}", v.map_or("-".into(), |v| format!("{v:.1}"))));
        println!();
    }
    Ok(true)
}

fn probe_camera(args: &[String]) -> Result<bool, String> {
    let name = args.first().ok_or("usage: probe <scenario|sim|u3v|uvc:name> [seconds]")?;
    let seconds: f64 = args.get(1).map_or(Ok(5.0), |s| s.parse()).map_err(|e| format!("seconds: {e}"))?;
    let base = scenario_or_device(name)?;
    let mut engine = Engine::new();
    let modes = engine.modes(&base)?;
    eprintln!("camera: {}, {} modes, ~{:.0} s", engine.camera_name(), modes.len(), modes.len() as f64 * (seconds + 1.0));
    for m in &modes {
        let how = if m.reported { "reported" } else { "accepted when opened" };
        eprintln!("  {:<18} {:>9}  up to {} fps  ({how})", m.format.name(), format!("{}x{}", m.width, m.height), m.fps);
    }
    let mut queue: std::collections::VecDeque<Scenario> = probe::plan(&base, &modes).into();
    let mut any_ok = false;
    while let Some(s) = queue.pop_front() {
        let c = &s.camera;
        let v = probe::Test::run(&mut engine, &s, seconds);
        let verdict = v.text.lines().next().unwrap_or("");
        println!("{:<18} {:>9} {:>7} fps  {:<5} {verdict}", c.pixel_format.name(), format!("{}x{}", c.width, c.height), c.frame_rate, s.recording.encoder);
        println!("    {}", v.text.lines().nth(1).unwrap_or(""));
        any_ok |= v.level == probe::Level::Ok;
        if let Some(r) = probe::raw_retry(&s, &v).or_else(|| probe::lower_rate(&s, &v, &modes)) {
            queue.push_front(r);
        }
    }
    Ok(any_ok)
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
    let video_frames = export::count_frames(&info.video).ok();

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

