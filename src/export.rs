//! `export`: motion / colour magnified view of a recording (H.264).
//!
//! The recording is decoded by ffmpeg, processed frame by frame
//! ([`crate::magnify`]) and written as a grid: the original first, then one
//! panel per method and band. The recording itself is never changed.

use std::io::{ErrorKind, Read, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Instant;

use rayon::prelude::*;

use crate::encoder::{ffmpeg_path, ffprobe_path};
use crate::magnify::{self, Band, Plane};

pub const USAGE: &str = "\
  vid_recorder export <recording> <output> <methods> [options]
                               export a recording; methods (comma
                               separated, one panel per band):
                 native        the recording itself (no grid, no processing)
                 phase         phase-based (Riesz) motion, pulse + breath
                 tremor        phase-based motion, 3-5, 5-8, 8-12 Hz
                 linear        linear Eulerian motion, pulse + breath
                 broadband     linear Eulerian motion, no temporal filter
                 colour        colour change on skin, pulse (colour input)
               options:
                 band=pulse|breath|tremor|LO-HI[,..]  temporal band(s), Hz
                 alpha=N[,N..]  magnification (one, or one per band)
                 start=S dur=S  part of the recording (seconds)
                 width=N        process at this width (faster)
                 bayer=rggb|bggr|grbg|gbrg  mosaic (default: from the tag)
                 fps=N          output frame rate (frames dropped/repeated)
                 slow           keep every frame, play at fps (default 25)
                 codec=h264|hevc|prores|ffv1  (default h264; ffv1 lossless)
                 crf=N          quality for h264/hevc (lower = better)";

const PULSE: (f64, f64) = (0.8, 1.5);
const BREATH: (f64, f64) = (0.15, 0.5);
const TREMOR: [(f64, f64); 3] = [(3.0, 5.0), (5.0, 8.0), (8.0, 12.0)];
/// Seconds decoded before `start` so the temporal filters settle.
const WARM_UP: f64 = 8.0;

enum Method {
    Phase(magnify::Phase),
    Linear(magnify::Linear),
    Broadband(magnify::Broadband),
    Colour(magnify::Colour),
}

#[derive(Default)]
struct Options {
    bands: Option<Vec<(f64, f64)>>,
    alphas: Option<Vec<f32>>,
    start: f64,
    dur: Option<f64>,
    width: Option<usize>,
    bayer: Option<String>,
    slow: bool,
    fps: Option<f64>,
    codec: Option<String>,
    crf: Option<u32>,
}

fn parse_options(args: &[String]) -> Result<Options, String> {
    let mut o = Options::default();
    for a in args {
        if a == "slow" {
            o.slow = true;
            continue;
        }
        let (k, v) = a.split_once('=').ok_or_else(|| format!("unknown option {a}"))?;
        let num = |v: &str| v.parse::<f64>().map_err(|e| format!("{k}: {e}"));
        match k {
            "band" => {
                let mut bands = Vec::new();
                for b in v.split(',') {
                    match b {
                        "pulse" => bands.push(PULSE),
                        "breath" => bands.push(BREATH),
                        "tremor" => bands.extend(TREMOR),
                        _ => {
                            let (lo, hi) = b.split_once('-').ok_or_else(|| format!("band {b}: use LO-HI"))?;
                            bands.push((num(lo)?, num(hi)?));
                        }
                    }
                }
                o.bands = Some(bands);
            }
            "alpha" => o.alphas = Some(v.split(',').map(|a| num(a).map(|a| a as f32)).collect::<Result<_, _>>()?),
            "start" => o.start = num(v)?,
            "dur" => o.dur = Some(num(v)?),
            "width" => o.width = Some(num(v)? as usize & !1),
            "bayer" => o.bayer = Some(v.to_ascii_lowercase()),
            "fps" => o.fps = Some(num(v)?).filter(|f| *f > 0.0),
            "codec" => o.codec = Some(v.to_ascii_lowercase()),
            "crf" => o.crf = Some(num(v)? as u32),
            _ => return Err(format!("unknown option {a}")),
        }
    }
    Ok(o)
}

fn build_method(name: &str, o: &Options, fps: f64, colour_input: bool) -> Result<Method, String> {
    let (bands, alphas): (Vec<(f64, f64)>, Vec<f32>) = match name {
        "phase" => (vec![PULSE, BREATH], vec![30.0, 20.0]),
        "tremor" => (TREMOR.to_vec(), vec![30.0]),
        "linear" => (vec![PULSE, BREATH], vec![25.0, 15.0]),
        "colour" => (vec![PULSE], vec![80.0]),
        "broadband" => (Vec::new(), vec![5.0, 15.0, 40.0]),
        _ => return Err(format!("unknown method {name}")),
    };
    let alphas = o.alphas.clone().unwrap_or(alphas);
    if name == "broadband" {
        return Ok(Method::Broadband(magnify::Broadband::new(&alphas)));
    }
    let bands = o.bands.clone().unwrap_or(bands);
    if alphas.len() != 1 && alphas.len() != bands.len() {
        return Err(format!("{name}: {} alpha values for {} bands", alphas.len(), bands.len()));
    }
    let bands: Vec<Band> = bands
        .iter()
        .enumerate()
        .map(|(i, &(lo, hi))| Band { lo, hi, alpha: alphas[if alphas.len() == 1 { 0 } else { i }] })
        .collect();
    for b in &bands {
        if !(0.0 < b.lo && b.lo < b.hi && b.hi < fps / 2.0) {
            return Err(format!("band {}-{} Hz is not usable at {fps:.2} fps (upper edge must be below {:.2} Hz)", b.lo, b.hi, fps / 2.0));
        }
    }
    Ok(match name {
        "phase" | "tremor" => Method::Phase(magnify::Phase::new(&bands, fps)),
        "linear" => Method::Linear(magnify::Linear::new(&bands, fps)),
        _ => {
            if !colour_input {
                return Err("colour needs a colour recording".into());
            }
            if bands.len() != 1 {
                return Err("colour takes one band".into());
            }
            Method::Colour(magnify::Colour::new(bands[0], fps))
        }
    })
}

struct Probe {
    w: usize,
    h: usize,
    fps: f64,
    gray: bool,
    /// PIXEL_FORMAT tag written by the recorder (e.g. "BayerRG12").
    tag: Option<String>,
}

fn probe(input: &str) -> Result<Probe, String> {
    let out = Command::new(ffprobe_path())
        .args(["-v", "error", "-select_streams", "v:0"])
        .args(["-show_entries", "stream=width,height,pix_fmt,avg_frame_rate:stream_tags", "-of", "default=nw=1", input])
        .output()
        .map_err(|e| format!("ffprobe: {e}"))?;
    if !out.status.success() {
        return Err(format!("ffprobe {input}: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let field = |k: &str| text.lines().find_map(|l| l.split_once('=').filter(|(key, _)| key.eq_ignore_ascii_case(k)).map(|(_, v)| v.to_string()));
    let num = |k: &str| field(k).and_then(|v| v.parse::<usize>().ok()).ok_or_else(|| format!("ffprobe: no {k}"));
    let fps = field("avg_frame_rate")
        .and_then(|r| {
            let (n, d) = r.split_once('/')?;
            Some(n.parse::<f64>().ok()? / d.parse::<f64>().ok()?)
        })
        .filter(|f| f.is_finite() && *f > 0.0)
        .ok_or("ffprobe: no frame rate")?;
    Ok(Probe {
        w: num("width")?,
        h: num("height")?,
        fps,
        gray: field("pix_fmt").is_some_and(|p| p.starts_with("gray")),
        tag: field("TAG:PIXEL_FORMAT"),
    })
}

/// Real frame rate from the recording's timestamp CSV (median interval),
/// if there is one next to it.
fn csv_fps(input: &str) -> Option<f64> {
    let text = std::fs::read_to_string(Path::new(input).with_extension("csv")).ok()?;
    let ts: Vec<u64> = text.lines().skip(1).filter_map(|l| l.split(',').next()?.trim().parse().ok()).collect();
    let mut d: Vec<u64> = ts.windows(2).filter_map(|w| w[1].checked_sub(w[0])).filter(|&d| d > 0).collect();
    if d.len() < 10 {
        return None;
    }
    d.sort_unstable();
    Some(1e9 / d[d.len() / 2] as f64)
}

/// "BayerRG12p" -> "rggb".
fn bayer_from_tag(tag: &str) -> Option<String> {
    let p = tag.strip_prefix("Bayer")?.get(..2)?.to_ascii_lowercase();
    Some(match p.as_str() {
        "rg" => "rggb",
        "bg" => "bggr",
        "gr" => "grbg",
        "gb" => "gbrg",
        _ => return None,
    }
    .into())
}

/// Decoder: raw 16-bit frames (grey or RGB) on stdout. A Bayer recording
/// (stored as grey) is reinterpreted and demosaiced by a second ffmpeg.
fn spawn_decoder(input: &str, p: &Probe, o: &Options, from: f64, len: Option<f64>, size: (usize, usize), colour: bool) -> Result<Vec<Child>, String> {
    let scale = (size != (p.w, p.h)).then(|| format!("scale={}:{}:flags=area", size.0, size.1));
    let mut first = Command::new(ffmpeg_path());
    first.args(["-v", "error", "-nostdin"]);
    if from > 0.0 {
        first.args(["-ss", &format!("{from}")]);
    }
    first.args(["-i", input]);
    if let Some(len) = len {
        first.args(["-t", &format!("{len}")]);
    }
    first.args(["-an", "-f", "rawvideo"]);
    let bayer = o.bayer.as_ref().filter(|_| p.gray);
    match bayer {
        None => {
            if let Some(s) = &scale {
                first.args(["-vf", s]);
            }
            first.args(["-pix_fmt", if colour { "rgb48le" } else { "gray16le" }]);
        }
        Some(_) => {
            first.args(["-pix_fmt", "gray16le"]);
        }
    }
    let mut a = first.arg("-").stdout(Stdio::piped()).spawn().map_err(|e| format!("ffmpeg: {e}"))?;
    let Some(pattern) = bayer else { return Ok(vec![a]) };
    let mut second = Command::new(ffmpeg_path());
    second
        .args(["-v", "error", "-f", "rawvideo", "-pix_fmt", &format!("bayer_{pattern}16le")])
        .args(["-s", &format!("{}x{}", p.w, p.h), "-i", "-", "-f", "rawvideo"]);
    if let Some(s) = &scale {
        second.args(["-vf", s]);
    }
    let b = second
        .args(["-pix_fmt", "rgb48le", "-"])
        .stdin(a.stdout.take().unwrap())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| format!("ffmpeg (demosaic): {e}"))?;
    Ok(vec![b, a])
}

/// Output codec arguments.
fn codec_args(o: &Options) -> Result<Vec<String>, String> {
    let crf = |d: u32| o.crf.unwrap_or(d).to_string();
    let a: Vec<String> = match o.codec.as_deref().unwrap_or("h264") {
        "h264" => vec!["-c:v".into(), "libx264".into(), "-preset".into(), "medium".into(), "-crf".into(), crf(18), "-pix_fmt".into(), "yuv420p".into()],
        "hevc" => vec!["-c:v".into(), "libx265".into(), "-preset".into(), "medium".into(), "-crf".into(), crf(20), "-pix_fmt".into(), "yuv420p10le".into(), "-tag:v".into(), "hvc1".into()],
        "prores" => vec!["-c:v".into(), "prores_ks".into(), "-profile:v".into(), "3".into(), "-pix_fmt".into(), "yuv422p10le".into()],
        "ffv1" => vec!["-c:v".into(), "ffv1".into(), "-level".into(), "3".into(), "-slices".into(), "16".into()],
        c => return Err(format!("unknown codec {c} (h264, hevc, prores, ffv1)")),
    };
    Ok(a)
}

/// Output filters (even size for chroma subsampling, `fps=` resampling)
/// and the frame rate the frames are timed at (`slow`: every frame at
/// `fps=`, default 25).
fn output_filters(o: &Options, fps: f64) -> (Vec<String>, f64) {
    let mut vf = Vec::new();
    if o.codec.as_deref() != Some("ffv1") {
        vf.push("pad=ceil(iw/2)*2:ceil(ih/2)*2".to_string());
    }
    if let (false, Some(f)) = (o.slow, o.fps) {
        vf.push(format!("fps={f}"));
    }
    (vf, if o.slow { o.fps.unwrap_or(25.0) } else { fps })
}

/// Encoder reading raw frames (`pix_fmt`, `w`x`h`) from `stdin`. The input
/// is timed at `rate`; `fps=` resamples to real time, `slow` retimes.
fn spawn_encoder(output: &str, pix_fmt: &str, (w, h): (usize, usize), fps: f64, o: &Options, stdin: Stdio) -> Result<Child, String> {
    let (vf, rate) = output_filters(o, fps);
    let mut cmd = Command::new(ffmpeg_path());
    cmd.args(["-v", "error", "-y", "-f", "rawvideo", "-pix_fmt", pix_fmt])
        .args(["-s", &format!("{w}x{h}"), "-r", &format!("{rate}"), "-i", "-"]);
    if !vf.is_empty() {
        cmd.args(["-vf", &vf.join(",")]);
    }
    cmd.args(codec_args(o)?).arg(output).stdin(stdin).spawn().map_err(|e| format!("ffmpeg (encoder): {e}"))
}

/// Next frame as 1 (grey) or 3 (R, G, B) planes in 0..255, `None` at the end.
fn read_frame(src: &mut impl Read, w: usize, h: usize, ch: usize, buf: &mut [u8]) -> Result<Option<Vec<Plane>>, String> {
    match src.read_exact(buf) {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(format!("decoder: {e}")),
    }
    let planes = (0..ch)
        .into_par_iter()
        .map(|c| {
            let d = buf.par_chunks_exact(2 * ch).map(|px| u16::from_le_bytes([px[2 * c], px[2 * c + 1]]) as f32 / 257.0).collect();
            Plane { w, h, d }
        })
        .collect();
    Ok(Some(planes))
}

/// `c + (new_luma - luma)`: luma change applied to one colour channel.
fn add_luma_change(c: &Plane, new_luma: &Plane, luma: &Plane) -> Plane {
    let d = c.d.par_iter().zip(&new_luma.d).zip(&luma.d).map(|((c, n), y)| c + n - y).collect();
    Plane { w: c.w, h: c.h, d }
}

pub fn run(args: &[String]) -> Result<bool, String> {
    let [input, output, methods, rest @ ..] = args else {
        return Err(format!("usage:\n{USAGE}"));
    };
    let mut o = parse_options(rest)?;
    let p = probe(input)?;
    if o.bayer.is_none() && p.gray {
        o.bayer = p.tag.as_deref().and_then(bayer_from_tag);
    }
    let colour = !p.gray || o.bayer.is_some();
    let ch = if colour { 3 } else { 1 };
    let fps = csv_fps(input).unwrap_or(p.fps);
    let (w, h) = match o.width {
        Some(nw) if nw < p.w => (nw, ((nw as f64 * p.h as f64 / p.w as f64).round() as usize) & !1),
        _ => (p.w, p.h),
    };
    eprintln!(
        "{input}: {}x{} {} @ {fps:.3} fps{}, processing at {w}x{h}",
        p.w,
        p.h,
        if colour { "colour" } else { "grey" },
        o.bayer.as_ref().map(|b| format!(" (Bayer {b})")).unwrap_or_default()
    );
    codec_args(&o)?;

    if methods == "native" {
        let t0 = Instant::now();
        let ok = if o.bayer.is_some() {
            // Demosaic decoder straight into the encoder (16 bit in between).
            let mut dec = spawn_decoder(input, &p, &o, o.start, o.dur, (w, h), colour)?;
            let pipe = Stdio::from(dec[0].stdout.take().unwrap());
            let mut enc = spawn_encoder(output, "rgb48le", (w, h), fps, &o, pipe)?;
            let ok = enc.wait().map_err(|e| e.to_string())?.success();
            dec.iter_mut().for_each(|c| _ = c.wait());
            ok
        } else {
            // One ffmpeg; the source pixel format is kept where the codec allows.
            let mut cmd = Command::new(ffmpeg_path());
            cmd.args(["-v", "error", "-y", "-nostdin"]);
            if o.start > 0.0 {
                cmd.args(["-ss", &format!("{}", o.start)]);
            }
            if let Some(d) = o.dur {
                cmd.args(["-t", &format!("{d}")]); // of the recording, before retiming
            }
            cmd.args(["-i", input]);
            let mut vf = Vec::new();
            if (w, h) != (p.w, p.h) {
                vf.push(format!("scale={w}:{h}:flags=area"));
            }
            if !o.slow {
                vf.push(format!("setpts=N/({fps}*TB)")); // real frame rate (timestamp CSV)
            }
            let (filters, rate) = output_filters(&o, fps);
            vf.extend(filters);
            if o.slow {
                // every frame, retimed
                vf.push(format!("setpts=N/({rate}*TB)"));
                cmd.args(["-r", &format!("{rate}")]);
            }
            if !vf.is_empty() {
                cmd.args(["-vf", &vf.join(",")]);
            }
            cmd.arg("-an").args(codec_args(&o)?).arg(output);
            cmd.status().map_err(|e| format!("ffmpeg: {e}"))?.success()
        };
        eprintln!("-> {output} ({:.0} s)", t0.elapsed().as_secs_f64());
        return Ok(ok);
    }

    let mut methods: Vec<Method> =
        methods.split(',').map(|m| build_method(m.trim(), &o, fps, colour)).collect::<Result<_, _>>()?;

    let warm = o.start.min(WARM_UP);
    let from = o.start - warm;
    let len = o.dur.map(|d| d + warm);
    let warm_frames = (warm * p.fps).round() as usize;
    let mut buf = vec![0u8; w * h * ch * 2];

    // Broadband needs the temporal mean of the whole part first.
    if methods.iter().any(|m| matches!(m, Method::Broadband(_))) {
        let mut dec = spawn_decoder(input, &p, &o, from, len, (w, h), colour)?;
        let mut out = dec[0].stdout.take().unwrap();
        while let Some(f) = read_frame(&mut out, w, h, ch, &mut buf)? {
            let y = if ch == 3 { magnify::luma(&f) } else { f[0].clone() };
            for m in &mut methods {
                if let Method::Broadband(b) = m {
                    b.add_to_mean(&y);
                }
            }
        }
        dec.iter_mut().for_each(|c| _ = c.wait());
        for m in &mut methods {
            if let Method::Broadband(b) = m {
                b.finish_mean();
            }
        }
    }

    let panels = 1 + methods
        .iter()
        .map(|m| match m {
            Method::Phase(p) => p.band_count(),
            Method::Linear(l) => l.band_count(),
            Method::Broadband(b) => b.band_count(),
            Method::Colour(_) => 1,
        })
        .sum::<usize>();
    let cols = (panels as f64).sqrt().ceil() as usize;
    let rows = panels.div_ceil(cols);
    let (gw, gh) = (cols * w, rows * h);
    let mut enc = spawn_encoder(output, if colour { "rgb24" } else { "gray" }, (gw, gh), fps, &o, Stdio::piped())?;
    let mut enc_in = enc.stdin.take().unwrap();

    let mut dec = spawn_decoder(input, &p, &o, from, len, (w, h), colour)?;
    let mut src = dec[0].stdout.take().unwrap();
    let mut grid = vec![0u8; gw * gh * ch];
    let (t0, mut n, mut written) = (Instant::now(), 0usize, 0usize);
    while let Some(frame) = read_frame(&mut src, w, h, ch, &mut buf)? {
        let y = if ch == 3 { magnify::luma(&frame) } else { frame[0].clone() };
        let mut out: Vec<Vec<Plane>> = vec![frame.clone()];
        for m in &mut methods {
            let lumas = match m {
                Method::Phase(p) => p.process(&y),
                Method::Linear(l) => l.process(&y),
                Method::Broadband(b) => b.process(&y),
                Method::Colour(c) => {
                    out.push(c.process(&frame));
                    continue;
                }
            };
            // Motion is magnified on luma; colour keeps its chroma.
            for l in lumas {
                out.push(if ch == 1 { vec![l] } else { frame.iter().map(|c| add_luma_change(c, &l, &y)).collect() });
            }
        }
        n += 1;
        if n <= warm_frames {
            continue;
        }
        grid.par_chunks_mut(gw * ch).enumerate().for_each(|(gy, row)| {
            let (pr, y) = (gy / h, gy % h);
            for (pc, dst) in row.chunks_mut(w * ch).enumerate() {
                let Some(panel) = out.get(pr * cols + pc) else { continue };
                for (x, px) in dst.chunks_exact_mut(ch).enumerate() {
                    for (v, plane) in px.iter_mut().zip(panel) {
                        *v = plane.d[y * w + x].round().clamp(0.0, 255.0) as u8;
                    }
                }
            }
        });
        enc_in.write_all(&grid).map_err(|e| format!("encoder: {e}"))?;
        written += 1;
        if written % (fps.round() as usize * 10).max(1) == 0 {
            let s = t0.elapsed().as_secs_f64();
            eprintln!("{:.0} s of video, {:.1} frames/s", written as f64 / fps, written as f64 / s);
        }
    }
    drop(enc_in);
    let ok = enc.wait().map_err(|e| e.to_string())?.success();
    dec.iter_mut().for_each(|c| _ = c.wait());
    eprintln!("{written} frames -> {output} ({:.0} s)", t0.elapsed().as_secs_f64());
    Ok(ok && written > 0)
}
