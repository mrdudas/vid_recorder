//! Averaging of preview frames (noise reduction), optionally aligned
//! (shift + rotation, e.g. a telescope). Only the preview and saved still
//! images use it: the recording stays raw.
//!
//! Runs on its own thread; frames that arrive while it is busy are skipped.
//! Depth: a fixed number of frames, or automatic: frames are added until
//! one more changes the average by less than [`SETTLED`], and that many
//! are kept (a running average of that depth follows the scene).

use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use crate::dark::Dark;
use crate::frame::{Frame, PixelFormat};

/// Relative change of the average per added frame below which the
/// automatic depth stops growing.
const SETTLED: f64 = 0.002;
/// Largest automatic depth.
const MAX_DEPTH: u32 = 1000;
/// Width the alignment works at (and the preview copy).
const ALIGN_WIDTH: usize = 512;
const PREVIEW_WIDTH: usize = 960;

/// Linear image, values 0..1, 1 (mono) or 3 (RGB) channels.
#[derive(Clone)]
pub struct Image {
    pub width: usize,
    pub height: usize,
    pub channels: usize,
    pub data: Vec<f32>,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Settings {
    /// Frames to average; 0: automatic.
    pub frames: u32,
    pub align: bool,
    /// Dark frame subtracted (a change restarts the average).
    pub dark: bool,
}

#[derive(Clone, Default, Debug)]
pub struct Status {
    /// Frames added since the start.
    pub added: u64,
    /// Current depth of the average.
    pub depth: u32,
    /// Automatic depth found.
    pub settled: bool,
    /// Frames the alignment could not place.
    pub rejected: u64,
    /// Last frame's offset to the first: x, y [px], rotation [°].
    pub motion: (f32, f32, f32),
    /// Result of the last save.
    pub saved: Option<Result<String, String>>,
}

enum Msg {
    Frame(Frame, Settings, Option<Arc<Dark>>),
    Save(std::path::PathBuf),
    Reset,
}

/// Preview-size copy of the average and the status, with a counter that
/// changes on every new one.
type Output = Arc<Mutex<(u64, Option<Image>, Status)>>;

pub struct Stacker {
    tx: Option<SyncSender<Msg>>,
    out: Output,
    seen: u64,
    thread: Option<JoinHandle<()>>,
}

impl Stacker {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::sync_channel::<Msg>(1);
        let out: Output = Arc::new(Mutex::new((0, None, Status::default())));
        let o = out.clone();
        let thread = std::thread::Builder::new()
            .name("stack".into())
            .spawn(move || {
                let mut st = State::default();
                for msg in rx {
                    match msg {
                        Msg::Frame(f, s, dark) => {
                            if let Some(img) = to_image(&f, dark.as_deref()) {
                                st.add(img, s);
                                let preview = st.mean.as_ref().map(|m| downscale(m, PREVIEW_WIDTH));
                                let mut g = o.lock().unwrap();
                                g.0 += 1;
                                g.1 = preview;
                                g.2 = Status { saved: g.2.saved.take(), ..st.status.clone() };
                            }
                        }
                        Msg::Save(path) => {
                            let r = match &st.mean {
                                Some(m) => save_png16(&path, m).map(|()| path.display().to_string()),
                                None => Err("még nincs átlagolt kép".into()),
                            };
                            let mut g = o.lock().unwrap();
                            g.0 += 1;
                            g.2.saved = Some(r);
                        }
                        Msg::Reset => st = State::default(),
                    }
                }
            })
            .expect("stack thread");
        Stacker { tx: Some(tx), out, seen: 0, thread: Some(thread) }
    }

    /// Hand a frame over; returns it if the stacker is still busy.
    pub fn push(&self, f: Frame, s: Settings, dark: Option<Arc<Dark>>) -> Option<Frame> {
        match self.tx.as_ref()?.try_send(Msg::Frame(f, s, dark)) {
            Ok(()) => None,
            Err(TrySendError::Full(Msg::Frame(f, _, _)) | TrySendError::Disconnected(Msg::Frame(f, _, _))) => Some(f),
            Err(_) => None,
        }
    }

    pub fn reset(&self) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(Msg::Reset);
        }
    }

    /// Save the full-size average as a 16-bit linear PNG.
    pub fn save(&self, path: std::path::PathBuf) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(Msg::Save(path));
        }
    }

    /// The new preview-size average, if there is one since the last call.
    pub fn take(&mut self) -> Option<Image> {
        let mut g = self.out.lock().unwrap();
        if g.0 == self.seen {
            return None;
        }
        self.seen = g.0;
        g.1.take()
    }

    pub fn status(&self) -> Status {
        self.out.lock().unwrap().2.clone()
    }
}

impl Drop for Stacker {
    fn drop(&mut self) {
        self.tx = None;
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Rigid motion from the reference (average) to a frame: a reference
/// point p maps to R(theta)(p - c) + c + t, in alignment-size pixels.
#[derive(Clone, Copy, Default, Debug)]
struct Motion {
    theta: f64,
    tx: f64,
    ty: f64,
}

#[derive(Default)]
struct State {
    mean: Option<Image>,
    /// Frames in each pixel's average (pixels the aligned frames do not
    /// cover have fewer).
    count: Vec<u32>,
    depth: u32,
    /// Consecutive frames with a change below SETTLED.
    calm: u32,
    /// Consecutive frames the alignment rejected.
    misses: u32,
    motion: Motion,
    status: Status,
}

impl State {
    fn add(&mut self, img: Image, s: Settings) {
        let fits = self.mean.as_ref().is_some_and(|m| (m.width, m.height, m.channels) == (img.width, img.height, img.channels));
        if !fits {
            *self = State { count: vec![1; img.width * img.height], depth: 1, mean: Some(img), ..State::default() };
            self.status = Status { added: 1, depth: 1, ..Status::default() };
            return;
        }
        let mean = self.mean.as_mut().unwrap();
        // Motion of the new frame against the current average.
        let k = img.width.div_ceil(ALIGN_WIDTH) as f64;
        let m = if s.align {
            let reference = luminance(mean, ALIGN_WIDTH);
            let frame = luminance(&img, ALIGN_WIDTH);
            match align(&reference, &frame, self.motion) {
                Some(m) => m,
                None => {
                    self.status.rejected += 1;
                    self.misses += 1;
                    // The scene changed completely: start over from this frame.
                    if self.misses >= 10 {
                        let rejected = self.status.rejected;
                        self.mean = None;
                        self.add(img, s);
                        self.status.rejected = rejected;
                    }
                    return;
                }
            }
        } else {
            Motion::default()
        };
        self.motion = m;
        self.misses = 0;
        let limit = match s.frames {
            0 if self.status.settled => self.depth,
            0 => MAX_DEPTH,
            n => n,
        };
        let (w, h, ch) = (img.width, img.height, img.channels);
        let (c, s_, t) = (m.theta.cos(), m.theta.sin(), (m.tx * k, m.ty * k));
        let (cx, cy) = (w as f64 / 2.0, h as f64 / 2.0);
        // Rows in parallel: each thread its band of the average.
        let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).min(16);
        let rows = h.div_ceil(threads);
        let img = &img;
        let (change, level) = std::thread::scope(|sc| {
            let bands: Vec<_> = mean
                .data
                .chunks_mut(rows * w * ch)
                .zip(self.count.chunks_mut(rows * w))
                .enumerate()
                .map(|(b, (md, cnt))| {
                    sc.spawn(move || {
                        let (mut change, mut level) = (0.0f64, 0.0f64);
                        let mut px = [0f32; 3];
                        for (r, (mrow, crow)) in md.chunks_mut(w * ch).zip(cnt.chunks_mut(w)).enumerate() {
                            let y = b * rows + r;
                            for x in 0..w {
                                let ok = if s.align {
                                    let (dx, dy) = (x as f64 - cx, y as f64 - cy);
                                    bilinear(img, c * dx - s_ * dy + cx + t.0, s_ * dx + c * dy + cy + t.1, &mut px[..ch])
                                } else {
                                    let i = (y * w + x) * ch;
                                    px[..ch].copy_from_slice(&img.data[i..i + ch]);
                                    true
                                };
                                if !ok {
                                    continue;
                                }
                                let n = (crow[x] + 1).min(limit);
                                crow[x] = n;
                                for (j, &p) in px[..ch].iter().enumerate() {
                                    let a = &mut mrow[x * ch + j];
                                    let d = (p - *a) / n as f32;
                                    *a += d;
                                    change += d.abs() as f64;
                                    level += *a as f64;
                                }
                            }
                        }
                        (change, level)
                    })
                })
                .collect();
            bands.into_iter().map(|h| h.join().unwrap()).fold((0.0, 0.0), |a, b| (a.0 + b.0, a.1 + b.1))
        });
        self.status.added += 1;
        let rel = change / level.max(1e-9);
        if s.frames == 0 && !self.status.settled {
            self.calm = if rel < SETTLED { self.calm + 1 } else { 0 };
            let frames = self.status.added as u32;
            if (self.calm >= 3 && frames >= 5) || frames >= MAX_DEPTH {
                self.status.settled = true;
                self.depth = frames;
            }
        }
        if s.frames != 0 {
            self.status.settled = false;
        }
        self.status.depth = if s.frames == 0 { if self.status.settled { self.depth } else { self.status.added as u32 } } else { s.frames.min(self.status.added as u32) };
        self.status.motion = ((m.tx * k) as f32, (m.ty * k) as f32, m.theta.to_degrees() as f32);
    }
}

fn bilinear(img: &Image, u: f64, v: f64, out: &mut [f32]) -> bool {
    let (w, h, ch) = (img.width, img.height, img.channels);
    if u < 0.0 || v < 0.0 || u > (w - 1) as f64 || v > (h - 1) as f64 {
        return false;
    }
    let (x0, y0) = (u as usize, v as usize);
    let (x1, y1) = ((x0 + 1).min(w - 1), (y0 + 1).min(h - 1));
    let (fx, fy) = ((u - x0 as f64) as f32, (v - y0 as f64) as f32);
    let p = |x: usize, y: usize, c: usize| img.data[(y * w + x) * ch + c];
    for (c, o) in out.iter_mut().enumerate() {
        let top = p(x0, y0, c) * (1.0 - fx) + p(x1, y0, c) * fx;
        let bottom = p(x0, y1, c) * (1.0 - fx) + p(x1, y1, c) * fx;
        *o = top * (1.0 - fy) + bottom * fy;
    }
    true
}

/// Grey image for the alignment.
#[derive(Clone)]
struct Gray {
    w: usize,
    h: usize,
    d: Vec<f32>,
}

impl Gray {
    fn at(&self, x: usize, y: usize) -> f32 {
        self.d[y * self.w + x]
    }

    fn sample(&self, u: f64, v: f64) -> Option<f32> {
        if u < 0.0 || v < 0.0 || u >= (self.w - 1) as f64 || v >= (self.h - 1) as f64 {
            return None;
        }
        let (x, y) = (u as usize, v as usize);
        let (fx, fy) = ((u - x as f64) as f32, (v - y as f64) as f32);
        let top = self.at(x, y) * (1.0 - fx) + self.at(x + 1, y) * fx;
        let bottom = self.at(x, y + 1) * (1.0 - fx) + self.at(x + 1, y + 1) * fx;
        Some(top * (1.0 - fy) + bottom * fy)
    }

    fn half(&self) -> Gray {
        let (w, h) = (self.w / 2, self.h / 2);
        let d = (0..w * h)
            .map(|i| {
                let (x, y) = (i % w * 2, i / w * 2);
                (self.at(x, y) + self.at(x + 1, y) + self.at(x, y + 1) + self.at(x + 1, y + 1)) / 4.0
            })
            .collect();
        Gray { w, h, d }
    }
}

/// Box-downscaled luminance, at most `max_w` wide (integer factor).
fn luminance(img: &Image, max_w: usize) -> Gray {
    let k = img.width.div_ceil(max_w).max(1);
    let (w, h, ch) = (img.width / k, img.height / k, img.channels);
    let mut d = vec![0f32; w * h];
    for (i, o) in d.iter_mut().enumerate() {
        let (x0, y0) = (i % w * k, i / w * k);
        let mut s = 0.0;
        for y in y0..y0 + k {
            for x in x0..x0 + k {
                let p = &img.data[(y * img.width + x) * ch..][..ch];
                s += if ch == 3 { 0.25 * p[0] + 0.5 * p[1] + 0.25 * p[2] } else { p[0] };
            }
        }
        *o = s / (k * k) as f32;
    }
    Gray { w, h, d }
}

pub fn downscale(img: &Image, max_w: usize) -> Image {
    let k = img.width.div_ceil(max_w).max(1);
    if k == 1 {
        return img.clone();
    }
    let (w, h, ch) = (img.width / k, img.height / k, img.channels);
    let mut data = vec![0f32; w * h * ch];
    for y in 0..h * k {
        for x in 0..w * k {
            let src = &img.data[(y * img.width + x) * ch..][..ch];
            let dst = &mut data[((y / k) * w + x / k) * ch..][..ch];
            for c in 0..ch {
                dst[c] += src[c];
            }
        }
    }
    let n = (k * k) as f32;
    data.iter_mut().for_each(|v| *v /= n);
    Image { width: w, height: h, channels: ch, data }
}

/// Motion from `reference` to `frame` (rigid: shift + rotation), refined
/// from `guess` coarse to fine (Gauss-Newton on an image pyramid, with a
/// shift search on the coarsest level). `None` if it does not converge.
fn align(reference: &Gray, frame: &Gray, guess: Motion) -> Option<Motion> {
    let mut refs = vec![reference.clone()];
    let mut frames = vec![frame.clone()];
    while refs.last().unwrap().w >= 96 && refs.last().unwrap().h >= 32 {
        refs.push(refs.last().unwrap().half());
        frames.push(frames.last().unwrap().half());
    }
    let top = refs.len() - 1;
    let scale = (1u32 << top) as f64;
    let mut m = Motion { theta: guess.theta, tx: guess.tx / scale, ty: guess.ty / scale };
    m = search_shift(&refs[top], &frames[top], m);
    for level in (0..=top).rev() {
        m = refine(&refs[level], &frames[level], m)?;
        if level > 0 {
            m.tx *= 2.0;
            m.ty *= 2.0;
        }
    }
    let size = reference.w.max(reference.h) as f64;
    (m.tx.is_finite() && m.ty.is_finite() && m.tx.abs() < size && m.ty.abs() < size && m.theta.abs() < 0.8).then_some(m)
}

/// Best whole-pixel shift around `m` (±1/6 of the width), by mean
/// absolute difference.
fn search_shift(r: &Gray, f: &Gray, m: Motion) -> Motion {
    let reach = (r.w / 6).max(2) as i64;
    let (cx, cy) = (r.w as f64 / 2.0, r.h as f64 / 2.0);
    let (c, s) = (m.theta.cos(), m.theta.sin());
    let mut best = (f64::MAX, m);
    // Nearest shifts first; a farther one must be clearly better (periodic
    // patterns match equally well a period away).
    let mut offsets: Vec<(i64, i64)> = (-reach..=reach).flat_map(|dy| (-reach..=reach).map(move |dx| (dx, dy))).collect();
    offsets.sort_by_key(|&(dx, dy)| dx * dx + dy * dy);
    for (dx, dy) in offsets {
        {
            let (tx, ty) = (m.tx + dx as f64, m.ty + dy as f64);
            let (mut sum, mut n) = (0.0f64, 0u32);
            for y in 0..r.h {
                for x in 0..r.w {
                    let (px, py) = (x as f64 - cx, y as f64 - cy);
                    if let Some(v) = f.sample(c * px - s * py + cx + tx, s * px + c * py + cy + ty) {
                        sum += (v - r.at(x, y)).abs() as f64;
                        n += 1;
                    }
                }
            }
            // Enough overlap to compare (a third of the image).
            if n as usize * 3 >= r.w * r.h && sum / (n as f64) < best.0 * 0.98 {
                best = (sum / n as f64, Motion { tx, ty, ..m });
            }
        }
    }
    best.1
}

/// Gauss-Newton on (theta, tx, ty), forward compositional.
fn refine(r: &Gray, f: &Gray, mut m: Motion) -> Option<Motion> {
    let (cx, cy) = (r.w as f64 / 2.0, r.h as f64 / 2.0);
    // Every other pixel on the larger levels: plenty of equations.
    let step = if r.w > 200 { 2 } else { 1 };
    for _ in 0..12 {
        let (c, s) = (m.theta.cos(), m.theta.sin());
        let mut hm = [[0f64; 3]; 3];
        let mut b = [0f64; 3];
        for y in (1..r.h - 1).step_by(step) {
            for x in (1..r.w - 1).step_by(step) {
                let (px, py) = (x as f64 - cx, y as f64 - cy);
                let (u, v) = (c * px - s * py + cx + m.tx, s * px + c * py + cy + m.ty);
                let (Some(i), Some(il), Some(ir), Some(iu), Some(id)) =
                    (f.sample(u, v), f.sample(u - 1.0, v), f.sample(u + 1.0, v), f.sample(u, v - 1.0), f.sample(u, v + 1.0))
                else {
                    continue;
                };
                let (gu, gv) = (((ir - il) / 2.0) as f64, ((id - iu) / 2.0) as f64);
                // Gradient of the warped frame in reference coordinates.
                let (gx, gy) = (gu * c + gv * s, -gu * s + gv * c);
                let j = [gx, gy, -gx * py + gy * px];
                let e = (i - r.at(x, y)) as f64;
                for a in 0..3 {
                    for bb in 0..3 {
                        hm[a][bb] += j[a] * j[bb];
                    }
                    b[a] += j[a] * e;
                }
            }
        }
        // Damping: a direction the image does not fix (e.g. along
        // stripes) stays at the guess instead of making H singular.
        let tr = (hm[0][0] + hm[1][1]) / 2.0;
        hm[0][0] += 1e-3 * tr + 1e-9;
        hm[1][1] += 1e-3 * tr + 1e-9;
        hm[2][2] += 1e-3 * hm[2][2] + 1e-9;
        let d = solve3(hm, b)?;
        let (dtx, dty, dth) = (-d[0], -d[1], -d[2]);
        // m ∘ delta: t = t + R(theta) * t_delta.
        m.tx += c * dtx - s * dty;
        m.ty += s * dtx + c * dty;
        m.theta += dth;
        if dtx.abs() < 0.01 && dty.abs() < 0.01 && dth.abs() < 1e-5 {
            break;
        }
    }
    Some(m)
}

fn solve3(a: [[f64; 3]; 3], b: [f64; 3]) -> Option<[f64; 3]> {
    let det = |m: [[f64; 3]; 3]| {
        m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1]) - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
            + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0])
    };
    let d = det(a);
    if d.abs() < 1e-12 {
        return None;
    }
    Some(std::array::from_fn(|k| {
        let mut m = a;
        for r in 0..3 {
            m[r][k] = b[r];
        }
        det(m) / d
    }))
}

/// Linear image of a frame, full resolution; Bayer as 2x2 super pixels
/// (half size), YUV converted to RGB. A dark frame of the same raw format
/// and size is subtracted pixel by pixel (values may go below 0).
pub fn to_image(f: &Frame, dark: Option<&Dark>) -> Option<Image> {
    let (w, h) = (f.width as usize, f.height as usize);
    let d = &f.data;
    if f.format == PixelFormat::Mjpeg {
        let options = zune_jpeg::zune_core::options::DecoderOptions::default()
            .jpeg_set_out_colorspace(zune_jpeg::zune_core::colorspace::ColorSpace::RGB);
        let mut dec = zune_jpeg::JpegDecoder::new_with_options(zune_jpeg::zune_core::bytestream::ZCursor::new(&d[..]), options);
        let rgb = dec.decode().ok()?;
        let info = dec.info()?;
        let data = rgb.iter().map(|&v| v as f32 / 255.0).collect();
        return Some(Image { width: info.width as usize, height: info.height as usize, channels: 3, data });
    }
    if d.len() < Frame::byte_size(f.width, f.height, f.format) || w < 2 || h < 2 {
        return None;
    }
    let max = ((1u32 << f.format.bits()) - 1) as f32;
    let bpp = f.format.bytes_per_pixel();
    let dark = dark.filter(|k| (k.meta.format, k.meta.width, k.meta.height) == (f.format, f.width, f.height)).map(|k| &k.data);
    let mono = |x: usize, y: usize| -> f32 {
        let i = (y * w + x) * bpp;
        let v = if bpp == 1 { d[i] as f32 } else { u16::from_le_bytes([d[i], d[i + 1]]) as f32 };
        v / max - dark.map_or(0.0, |k| k[y * w + x])
    };
    let yuv = |y: u8, u: u8, v: u8| -> [f32; 3] {
        let (c, dd, e) = (y as f32 - 16.0, u as f32 - 128.0, v as f32 - 128.0);
        [1.164 * c + 1.596 * e, 1.164 * c - 0.392 * dd - 0.813 * e, 1.164 * c + 2.017 * dd].map(|x| (x / 255.0).clamp(0.0, 1.0))
    };
    Some(match f.format {
        fmt if fmt.is_bayer() => {
            let (ow, oh) = (w / 2, h / 2);
            let mut data = Vec::with_capacity(ow * oh * 3);
            for y in 0..oh {
                for x in 0..ow {
                    let (x, y) = (x * 2, y * 2);
                    data.extend([mono(x, y), (mono(x + 1, y) + mono(x, y + 1)) / 2.0, mono(x + 1, y + 1)]);
                }
            }
            Image { width: ow, height: oh, channels: 3, data }
        }
        PixelFormat::Yuyv | PixelFormat::Uyvy | PixelFormat::Nv12 => {
            let mut data = Vec::with_capacity(w * h * 3);
            for y in 0..h {
                for x in 0..w {
                    let pair = (y * w + (x & !1)) * 2;
                    let px = match f.format {
                        PixelFormat::Yuyv => yuv(d[(y * w + x) * 2], d[pair + 1], d[pair + 3]),
                        PixelFormat::Uyvy => yuv(d[(y * w + x) * 2 + 1], d[pair], d[pair + 2]),
                        _ => {
                            let uv = w * h + (y / 2) * w + (x & !1);
                            yuv(d[y * w + x], d[uv], d[uv + 1])
                        }
                    };
                    data.extend(px);
                }
            }
            Image { width: w, height: h, channels: 3, data }
        }
        _ => Image { width: w, height: h, channels: 1, data: (0..w * h).map(|i| mono(i % w, i / w)).collect() },
    })
}

/// 16-bit linear PNG (grey or RGB), no white balance, LUT or levels.
fn save_png16(path: &std::path::Path, img: &Image) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let file = std::io::BufWriter::new(std::fs::File::create(path).map_err(|e| e.to_string())?);
    let mut enc = png::Encoder::new(file, img.width as u32, img.height as u32);
    enc.set_color(if img.channels == 3 { png::ColorType::Rgb } else { png::ColorType::Grayscale });
    enc.set_depth(png::BitDepth::Sixteen);
    let mut wr = enc.write_header().map_err(|e| e.to_string())?;
    let bytes: Vec<u8> = img.data.iter().flat_map(|&v| ((v.clamp(0.0, 1.0) * 65535.0 + 0.5) as u16).to_be_bytes()).collect();
    wr.write_image_data(&bytes).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A field of blurred "stars".
    fn stars(w: usize, h: usize, m: Motion) -> Gray {
        let pts: Vec<(f64, f64)> = (0..60).map(|i| ((i * 37 % 97) as f64 / 97.0 * w as f64, (i * 61 % 89) as f64 / 89.0 * h as f64)).collect();
        let (cx, cy) = (w as f64 / 2.0, h as f64 / 2.0);
        let (c, s) = (m.theta.cos(), m.theta.sin());
        let d = (0..w * h)
            .map(|i| {
                let (x, y) = ((i % w) as f64, (i / w) as f64);
                pts.iter()
                    .map(|&(px, py)| {
                        // Star at reference point p appears at R(p-c)+c+t.
                        let (qx, qy) = (c * (px - cx) - s * (py - cy) + cx + m.tx, s * (px - cx) + c * (py - cy) + cy + m.ty);
                        (-((x - qx).powi(2) + (y - qy).powi(2)) / 8.0).exp() as f32
                    })
                    .sum()
            })
            .collect();
        Gray { w, h, d }
    }

    #[test]
    fn align_finds_shift_and_rotation() {
        let truth = Motion { theta: 2f64.to_radians(), tx: 13.3, ty: -7.6 };
        let r = stars(320, 240, Motion::default());
        let f = stars(320, 240, truth);
        let m = align(&r, &f, Motion::default()).expect("aligned");
        assert!((m.tx - truth.tx).abs() < 0.3 && (m.ty - truth.ty).abs() < 0.3, "{m:?}");
        assert!((m.theta - truth.theta).abs() < 0.2f64.to_radians(), "{m:?}");
    }

    #[test]
    fn align_stripes_does_not_fail() {
        // Diagonal stripes (the simulated camera): only the shift across
        // them is defined; alignment must still succeed.
        let stripes = |off: f32| Gray {
            w: 320,
            h: 180,
            d: (0..320 * 180).map(|i| (((i % 320 + i / 320) as f32 + off) / 40.0 * std::f32::consts::TAU).sin()).collect(),
        };
        let m = align(&stripes(0.0), &stripes(6.0), Motion::default()).expect("aligned");
        // Frame = reference shifted by -6 along x+y: tx + ty = -6.
        assert!((m.tx + m.ty + 6.0).abs() < 0.5, "{m:?}");
    }

    #[test]
    fn average_settles() {
        // Constant image with noise: the automatic depth stops growing.
        let mut st = State::default();
        let mut seed = 1u32;
        for _ in 0..400 {
            let data = (0..64 * 48)
                .map(|_| {
                    seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                    0.5 + (seed >> 8) as f32 / (1u32 << 24) as f32 * 0.2 - 0.1
                })
                .collect();
            st.add(Image { width: 64, height: 48, channels: 1, data }, Settings { frames: 0, align: false, dark: false });
        }
        assert!(st.status.settled && st.status.depth > 5 && st.status.depth < 400, "{:?}", st.status);
    }
}

#[cfg(test)]
mod speed {
    use super::*;

    #[test]
    #[ignore]
    fn add_speed() {
        let (w, h) = (1920, 1080);
        let img = Image { width: w, height: h, channels: 3, data: (0..w * h * 3).map(|i| ((i * 7919) % 1000) as f32 / 1000.0).collect() };
        for align in [false, true] {
            let mut st = State::default();
            st.add(img.clone(), Settings { frames: 0, align, dark: false });
            let t = std::time::Instant::now();
            for _ in 0..10 {
                st.add(img.clone(), Settings { frames: 0, align, dark: false });
            }
            eprintln!("align {align}: {:.1} ms/frame", t.elapsed().as_secs_f64() * 100.0);
        }
    }
}
