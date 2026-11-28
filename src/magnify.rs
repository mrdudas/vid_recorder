//! Motion and colour magnification for the export.
//!
//! Rust port of the methods developed for the G_matyas analysis (Oct 2026,
//! Python reference scripts in that project's `scripts/` folder):
//! - phase-based motion magnification with Riesz pyramids (Wadhwa et al. 2014),
//!   also used for the tremor bands,
//! - linear Eulerian magnification with a temporal band-pass,
//! - linear Eulerian magnification without temporal filtering (broadband),
//! - colour magnification on skin.
//!
//! Images are single-channel `f32` planes in the 0..255 range. All temporal
//! filters are causal and streaming (per-pixel IIR), so a recording of any
//! length is processed frame by frame.

use std::f64::consts::PI;

use rayon::prelude::*;

/// Laplacian pyramid levels (plus the low-pass residual).
pub const LEVELS: usize = 5;

#[derive(Clone)]
pub struct Plane {
    pub w: usize,
    pub h: usize,
    pub d: Vec<f32>,
}

impl Plane {
    pub fn new(w: usize, h: usize) -> Self {
        Plane { w, h, d: vec![0.0; w * h] }
    }

    fn zip(&self, o: &Plane, f: impl Fn(f32, f32) -> f32) -> Plane {
        Plane { w: self.w, h: self.h, d: self.d.iter().zip(&o.d).map(|(&a, &b)| f(a, b)).collect() }
    }
}

/// OpenCV BORDER_REFLECT_101 index.
fn refl(i: isize, n: usize) -> usize {
    let n = n as isize;
    if n == 1 {
        return 0;
    }
    let mut i = i;
    if i < 0 {
        i = -i;
    }
    if i >= n {
        i = 2 * n - 2 - i;
    }
    i.clamp(0, n - 1) as usize
}

/// Below this many elements an operation runs on the calling thread.
const PAR_MIN: usize = 1 << 14;

/// Fills `out` (`w` wide) row by row, rows spread over the worker threads.
fn par_rows(out: &mut [f32], w: usize, f: impl Fn(usize, &mut [f32]) + Sync + Send) {
    if out.len() < PAR_MIN {
        out.chunks_mut(w).enumerate().for_each(|(y, r)| f(y, r));
    } else {
        out.par_chunks_mut(w).enumerate().for_each(|(y, r)| f(y, r));
    }
}

/// `f(i, &mut a[i], &mut b[i * bs..(i + 1) * bs])` for every element.
fn par_zip3(a: &mut [f32], b: &mut [f32], bs: usize, f: impl Fn(usize, &mut f32, &mut [f32]) + Sync + Send) {
    const CHUNK: usize = 4096;
    let run = |(k, (a, b)): (usize, (&mut [f32], &mut [f32]))| {
        for (j, (a, b)) in a.iter_mut().zip(b.chunks_exact_mut(bs)).enumerate() {
            f(k * CHUNK + j, a, b);
        }
    };
    if a.len() < PAR_MIN {
        a.chunks_mut(CHUNK).zip(b.chunks_mut(CHUNK * bs)).enumerate().for_each(run);
    } else {
        a.par_chunks_mut(CHUNK).zip(b.par_chunks_mut(CHUNK * bs)).enumerate().for_each(run);
    }
}

const K5: [f32; 5] = [1.0 / 16.0, 4.0 / 16.0, 6.0 / 16.0, 4.0 / 16.0, 1.0 / 16.0];

/// Gaussian blur and 2x decimation (cv2.pyrDown).
fn pyr_down(s: &Plane) -> Plane {
    let (w2, h2) = (s.w.div_ceil(2), s.h.div_ceil(2));
    let mut t = Plane::new(w2, s.h);
    par_rows(&mut t.d, w2, |y, out| {
        let row = &s.d[y * s.w..(y + 1) * s.w];
        for (x, o) in out.iter_mut().enumerate() {
            let c = 2 * x as isize;
            *o = (0..5).map(|j| K5[j] * row[refl(c + j as isize - 2, s.w)]).sum();
        }
    });
    let mut o = Plane::new(w2, h2);
    par_rows(&mut o.d, w2, |y, out| {
        let c = 2 * y as isize;
        let rows: [&[f32]; 5] = std::array::from_fn(|j| {
            let r = refl(c + j as isize - 2, s.h) * w2;
            &t.d[r..r + w2]
        });
        for (x, o) in out.iter_mut().enumerate() {
            *o = (0..5).map(|j| K5[j] * rows[j][x]).sum();
        }
    });
    o
}

/// 2x upsampling to `w`x`h` with Gaussian interpolation (cv2.pyrUp).
/// Zero insertion followed by the 5-tap kernel (x2 per axis) reduces to:
/// even samples (a + 6b + c) / 8, odd samples (b + c) / 2.
fn pyr_up(s: &Plane, w: usize, h: usize) -> Plane {
    let mut t = Plane::new(w, s.h);
    par_rows(&mut t.d, w, |y, out| {
        let row = &s.d[y * s.w..(y + 1) * s.w];
        let get = |i: isize| row[refl(i, s.w)];
        for (x, o) in out.iter_mut().enumerate() {
            let i = (x / 2) as isize;
            *o = if x % 2 == 0 { (get(i - 1) + 6.0 * get(i) + get(i + 1)) / 8.0 } else { (get(i) + get(i + 1)) / 2.0 };
        }
    });
    let mut o = Plane::new(w, h);
    par_rows(&mut o.d, w, |y, out| {
        let i = (y / 2) as isize;
        let row = |k: isize| {
            let r = refl(k, s.h) * w;
            &t.d[r..r + w]
        };
        if y % 2 == 0 {
            let (a, b, c) = (row(i - 1), row(i), row(i + 1));
            for (x, o) in out.iter_mut().enumerate() {
                *o = (a[x] + 6.0 * b[x] + c[x]) / 8.0;
            }
        } else {
            let (b, c) = (row(i), row(i + 1));
            for (x, o) in out.iter_mut().enumerate() {
                *o = (b[x] + c[x]) / 2.0;
            }
        }
    });
    o
}

fn laplacian(img: &Plane) -> Vec<Plane> {
    let mut levels = Vec::with_capacity(LEVELS + 1);
    let mut g = img.clone();
    for _ in 0..LEVELS {
        let d = pyr_down(&g);
        let u = pyr_up(&d, g.w, g.h);
        levels.push(g.zip(&u, |a, b| a - b));
        g = d;
    }
    levels.push(g);
    levels
}

fn collapse(levels: &[Plane]) -> Plane {
    let mut g = levels[LEVELS].clone();
    for l in levels[..LEVELS].iter().rev() {
        g = pyr_up(&g, l.w, l.h).zip(l, |a, b| a + b);
    }
    g
}

/// 3x3 correlation (cv2.filter2D), reflect-101 border.
fn conv3(s: &Plane, k: &[[f32; 3]; 3]) -> Plane {
    let mut o = Plane::new(s.w, s.h);
    par_rows(&mut o.d, s.w, |y, out| {
        let rows: [usize; 3] = std::array::from_fn(|j| refl(y as isize + j as isize - 1, s.h) * s.w);
        for (x, o) in out.iter_mut().enumerate() {
            let cols: [usize; 3] = std::array::from_fn(|i| refl(x as isize + i as isize - 1, s.w));
            let mut v = 0.0;
            for j in 0..3 {
                for i in 0..3 {
                    v += k[j][i] * s.d[rows[j] + cols[i]];
                }
            }
            *o = v;
        }
    });
    o
}

/// Separable Gaussian blur, radius 3 sigma, reflect-101 border.
fn blur(s: &Plane, sigma: f32) -> Plane {
    let r = (3.0 * sigma).ceil() as isize;
    let mut k: Vec<f32> = (-r..=r).map(|i| (-(i * i) as f32 / (2.0 * sigma * sigma)).exp()).collect();
    let sum: f32 = k.iter().sum();
    k.iter_mut().for_each(|v| *v /= sum);
    let mut t = Plane::new(s.w, s.h);
    par_rows(&mut t.d, s.w, |y, out| {
        let row = &s.d[y * s.w..(y + 1) * s.w];
        for (x, o) in out.iter_mut().enumerate() {
            *o = (-r..=r).map(|i| k[(i + r) as usize] * row[refl(x as isize + i, s.w)]).sum();
        }
    });
    let mut o = Plane::new(s.w, s.h);
    par_rows(&mut o.d, s.w, |y, out| {
        out.fill(0.0);
        for (i, &kv) in (-r..=r).zip(&k) {
            let ri = refl(y as isize + i, s.h) * s.w;
            for (o, &v) in out.iter_mut().zip(&t.d[ri..ri + s.w]) {
                *o += kv * v;
            }
        }
    });
    o
}

// ---------------------------------------------------------------- filters

#[derive(Clone, Copy)]
struct C(f64, f64);

impl C {
    fn add(self, o: C) -> C {
        C(self.0 + o.0, self.1 + o.1)
    }
    fn sub(self, o: C) -> C {
        C(self.0 - o.0, self.1 - o.1)
    }
    fn mul(self, o: C) -> C {
        C(self.0 * o.0 - self.1 * o.1, self.0 * o.1 + self.1 * o.0)
    }
    fn div(self, o: C) -> C {
        let d = o.0 * o.0 + o.1 * o.1;
        C((self.0 * o.0 + self.1 * o.1) / d, (self.1 * o.0 - self.0 * o.1) / d)
    }
    fn sqrt(self) -> C {
        let r = self.0.hypot(self.1);
        C(((r + self.0) / 2.0).sqrt(), ((r - self.0) / 2.0).sqrt().copysign(self.1))
    }
}

/// Digital Butterworth band-pass as second-order sections
/// `[b0, b1, b2, a1, a2]`, same design as scipy `butter(order, [lo, hi],
/// "band", fs=fs, output="sos")`.
pub fn butter_band(order: usize, lo: f64, hi: f64, fs: f64) -> Vec<[f64; 5]> {
    let fs2 = 4.0; // scipy designs at fs = 2; bilinear transform uses 2 * fs
    let (w1, w2) = (fs2 * (PI * lo / fs).tan(), fs2 * (PI * hi / fs).tan());
    let (bw, wo2) = (w2 - w1, w1 * w2);
    let mut poles = Vec::new();
    for m in (1 - order as i64..order as i64).step_by(2) {
        let th = PI * m as f64 / (2.0 * order as f64);
        let p = C(-th.cos() * bw / 2.0, -th.sin() * bw / 2.0);
        let s = p.mul(p).sub(C(wo2, 0.0)).sqrt();
        poles.push(p.add(s));
        poles.push(p.sub(s));
    }
    // Analog zeros: `order` at the origin. Digital: `order` at +1, `order` at -1.
    let mut den = C(1.0, 0.0);
    for &p in &poles {
        den = den.mul(C(fs2, 0.0).sub(p));
    }
    let k = bw.powi(order as i32) * C(fs2.powi(order as i32), 0.0).div(den).0;
    let mut sos: Vec<[f64; 5]> = poles
        .iter()
        .map(|&p| C(fs2, 0.0).add(p).div(C(fs2, 0.0).sub(p)))
        .filter(|z| z.1 > 0.0)
        .map(|z| [1.0, 0.0, -1.0, -2.0 * z.0, z.0 * z.0 + z.1 * z.1])
        .collect();
    assert_eq!(sos.len(), order, "band-pass poles must come in conjugate pairs");
    for c in &mut sos[0][..3] {
        *c *= k;
    }
    sos
}

/// Per-pixel streaming IIR band-pass (transposed direct form II).
pub struct BandPass {
    sos: Vec<[f32; 5]>,
    z: Vec<f32>,
}

impl BandPass {
    pub fn new(lo: f64, hi: f64, fps: f64) -> Self {
        let sos = butter_band(2, lo, hi, fps).iter().map(|s| s.map(|v| v as f32)).collect();
        BandPass { sos, z: Vec::new() }
    }

    fn apply(&mut self, x: &[f32]) -> Vec<f32> {
        let ns = self.sos.len();
        if self.z.is_empty() {
            self.z = vec![0.0; x.len() * ns * 2];
        }
        let sos = &self.sos;
        let mut out = vec![0.0; x.len()];
        par_zip3(&mut out, &mut self.z, ns * 2, |i, o, z| {
            let mut y = x[i];
            for (s, z) in sos.iter().zip(z.chunks_exact_mut(2)) {
                let v = s[0] * y + z[0];
                z[0] = s[1] * y - s[3] * v + z[1];
                z[1] = s[2] * y - s[4] * v;
                y = v;
            }
            *o = y;
        });
        out
    }

    fn plane(&mut self, p: &Plane) -> Plane {
        Plane { w: p.w, h: p.h, d: self.apply(&p.d) }
    }
}

/// Temporal band and its magnification factor.
#[derive(Clone, Copy, Debug)]
pub struct Band {
    pub lo: f64,
    pub hi: f64,
    pub alpha: f32,
}

// ---------------------------------------------------------- phase (Riesz)

/// Approximate Riesz transform kernels (Wadhwa et al. 2014).
const RIESZ: [[f32; 3]; 3] = [[-0.12, 0.0, 0.12], [-0.34, 0.0, 0.34], [-0.12, 0.0, 0.12]];
const RIESZ_T: [[f32; 3]; 3] = [[-0.12, -0.34, -0.12], [0.0, 0.0, 0.0], [0.12, 0.34, 0.12]];

struct PhaseLevel {
    prev: Option<(Plane, Plane, Plane)>,
    /// Cumulative phase times orientation (cos, sin), interleaved.
    phase: Vec<f32>,
    filters: Vec<(BandPass, BandPass)>,
}

/// Phase-based motion magnification of luma, one output per band. The
/// phase change between frames is measured with the quaternionic Riesz
/// pyramid, band-passed in time, smoothed in space (amplitude-weighted) and
/// multiplied; the finest level is left alone (noise).
pub struct Phase {
    pub(crate) bands: Vec<Band>,
    levels: Vec<PhaseLevel>,
    /// Leaky integration of the phase keeps it bounded (high-pass at a
    /// tenth of the lowest band edge).
    leak: f32,
}

impl Phase {
    pub fn band_count(&self) -> usize {
        self.bands.len()
    }

    pub fn new(bands: &[Band], fps: f64) -> Self {
        let lo = bands.iter().map(|b| b.lo).fold(f64::MAX, f64::min);
        let levels = (0..LEVELS)
            .map(|_| PhaseLevel {
                prev: None,
                phase: Vec::new(),
                filters: bands.iter().map(|b| (BandPass::new(b.lo, b.hi, fps), BandPass::new(b.lo, b.hi, fps))).collect(),
            })
            .collect();
        Phase { bands: bands.to_vec(), levels, leak: (-2.0 * PI * lo / 10.0 / fps).exp() as f32 }
    }

    pub fn process(&mut self, luma: &Plane) -> Vec<Plane> {
        let lap = laplacian(luma);
        let mut out: Vec<Vec<Plane>> = vec![lap.clone(); self.bands.len()];
        for l in 1..LEVELS {
            let re = &lap[l];
            let (r1, r2) = (conv3(re, &RIESZ), conv3(re, &RIESZ_T));
            let st = &mut self.levels[l];
            let n = re.d.len();
            if st.prev.is_none() {
                st.prev = Some((re.clone(), r1.clone(), r2.clone()));
                st.phase = vec![0.0; 2 * n];
            }
            let (pr, p1, p2) = st.prev.as_ref().unwrap();
            let mut amp = Plane::new(re.w, re.h);
            let leak = self.leak;
            par_zip3(&mut amp.d, &mut st.phase, 2, |i, amp, ph| {
                // q * conj(q_prev): phase difference to the previous frame.
                let qr = re.d[i] * pr.d[i] + r1.d[i] * p1.d[i] + r2.d[i] * p2.d[i];
                let qx = -re.d[i] * p1.d[i] + pr.d[i] * r1.d[i];
                let qy = -re.d[i] * p2.d[i] + pr.d[i] * r2.d[i];
                let qamp = (qr * qr + qx * qx + qy * qy).sqrt() + 1e-9;
                // acos(qr / |q|), well conditioned also near 0 (where acos
                // turns tiny rounding differences into large angles).
                let v = (qx * qx + qy * qy).sqrt();
                let dphi = v.atan2(qr);
                let nrm = v + 1e-9;
                ph[0] = leak * ph[0] + dphi * qx / nrm;
                ph[1] = leak * ph[1] + dphi * qy / nrm;
                *amp = qamp.sqrt();
            });
            let (pc, ps): (Vec<f32>, Vec<f32>) = st.phase.chunks_exact(2).map(|p| (p[0], p[1])).unzip();
            let den = blur(&amp, 2.0);
            for (b, (band, (fc, fs))) in self.bands.iter().zip(st.filters.iter_mut()).enumerate() {
                let c = Plane { w: re.w, h: re.h, d: fc.apply(&pc) };
                let s = Plane { w: re.w, h: re.h, d: fs.apply(&ps) };
                let c = blur(&c.zip(&amp, |a, b| a * b), 2.0);
                let s = blur(&s.zip(&amp, |a, b| a * b), 2.0);
                let o = &mut out[b][l];
                let a = band.alpha;
                par_rows(&mut o.d, re.w, |y, row| {
                    for (x, o) in row.iter_mut().enumerate() {
                        let i = y * re.w + x;
                        let d = den.d[i] + 1e-9;
                        let (fc, fs) = (c.d[i] / d, s.d[i] / d);
                        let mag = a * (fc * fc + fs * fs).sqrt() + 1e-9;
                        let k = mag.sin() / mag * a;
                        *o = mag.cos() * re.d[i] - k * fc * r1.d[i] - k * fs * r2.d[i];
                    }
                });
            }
            st.prev = Some((re.clone(), r1, r2));
        }
        out.iter().map(|l| collapse(l)).collect()
    }
}

// ------------------------------------------------------------ linear EVM

/// Linear Eulerian magnification of luma with a temporal band-pass on every
/// Laplacian level; the finest level is skipped and the next one halved
/// (noise).
pub struct Linear {
    pub(crate) bands: Vec<Band>,
    filters: Vec<Vec<BandPass>>,
}

pub const LINEAR_WEIGHT: [f32; LEVELS + 1] = [0.0, 0.5, 1.0, 1.0, 1.0, 1.0];

impl Linear {
    pub fn band_count(&self) -> usize {
        self.bands.len()
    }

    pub fn new(bands: &[Band], fps: f64) -> Self {
        let filters = bands.iter().map(|b| (0..=LEVELS).map(|_| BandPass::new(b.lo, b.hi, fps)).collect()).collect();
        Linear { bands: bands.to_vec(), filters }
    }

    pub fn process(&mut self, luma: &Plane) -> Vec<Plane> {
        let lap = laplacian(luma);
        self.bands
            .iter()
            .zip(&mut self.filters)
            .map(|(b, fs)| {
                let amp: Vec<Plane> = lap
                    .iter()
                    .zip(fs.iter_mut())
                    .zip(LINEAR_WEIGHT)
                    .map(|((l, f), w)| {
                        let d = f.plane(l);
                        l.zip(&d, |v, d| v + d * b.alpha * w)
                    })
                    .collect();
                collapse(&amp)
            })
            .collect()
    }
}

/// Linear Eulerian magnification without temporal filtering: every level is
/// amplified by its deviation from the clip's temporal mean (needs a first
/// pass over the clip, see [`Broadband::add_to_mean`]).
pub struct Broadband {
    sum: Vec<Vec<f64>>,
    n: usize,
    mean: Vec<Plane>,
    alphas: Vec<f32>,
}

impl Broadband {
    pub fn band_count(&self) -> usize {
        self.alphas.len()
    }

    pub fn new(alphas: &[f32]) -> Self {
        Broadband { sum: Vec::new(), n: 0, mean: Vec::new(), alphas: alphas.to_vec() }
    }

    pub fn add_to_mean(&mut self, luma: &Plane) {
        let lap = laplacian(luma);
        if self.sum.is_empty() {
            self.sum = lap.iter().map(|l| vec![0.0; l.d.len()]).collect();
        }
        for (s, l) in self.sum.iter_mut().zip(&lap) {
            s.iter_mut().zip(&l.d).for_each(|(s, &v)| *s += v as f64);
        }
        if self.mean.is_empty() {
            self.mean = lap; // sizes; values are set in `finish_mean`
        }
        self.n += 1;
    }

    pub fn finish_mean(&mut self) {
        let n = self.n.max(1) as f64;
        for (m, s) in self.mean.iter_mut().zip(&self.sum) {
            m.d.iter_mut().zip(s).for_each(|(m, &s)| *m = (s / n) as f32);
        }
    }

    pub fn process(&self, luma: &Plane) -> Vec<Plane> {
        let lap = laplacian(luma);
        self.alphas
            .iter()
            .map(|&a| {
                let amp: Vec<Plane> = lap.iter().zip(&self.mean).map(|(l, m)| l.zip(m, |v, m| v + a * (v - m))).collect();
                collapse(&amp)
            })
            .collect()
    }
}

// ---------------------------------------------------------------- colour

/// Colour magnification: the band-passed coarse Gaussian level (spatially
/// pooled colour) is amplified and added back on skin only.
pub struct Colour {
    band: Band,
    filter: BandPass,
}

const COLOUR_LEVELS: usize = 4;

impl Colour {
    pub fn new(band: Band, fps: f64) -> Self {
        Colour { band, filter: BandPass::new(band.lo, band.hi, fps) }
    }

    /// `rgb`: R, G, B planes. Returns the magnified R, G, B planes.
    pub fn process(&mut self, rgb: &[Plane]) -> Vec<Plane> {
        let mask = skin_mask(rgb);
        let mut small = Vec::new();
        let mut sizes = Vec::new();
        for ch in rgb {
            let mut g = ch.clone();
            sizes.clear();
            for _ in 0..COLOUR_LEVELS {
                sizes.push((g.w, g.h));
                g = pyr_down(&g);
            }
            small.push(g);
        }
        let (sw, sh) = (small[0].w, small[0].h);
        let joined: Vec<f32> = small.iter().flat_map(|p| p.d.iter().copied()).collect();
        let filtered = self.filter.apply(&joined);
        (0..3).map(|c| {
            let mut d = Plane { w: sw, h: sh, d: filtered[c * sw * sh..(c + 1) * sw * sh].iter().map(|v| v * self.band.alpha).collect() };
            for &(w, h) in sizes.iter().rev() {
                d = pyr_up(&d, w, h);
            }
            let mut o = rgb[c].clone();
            o.d.par_iter_mut().zip(&d.d).zip(&mask.d).for_each(|((o, d), m)| *o += d * m);
            o
        })
        .collect()
    }
}

/// Soft skin mask (YCrCb thresholds, morphological opening, blur).
fn skin_mask(rgb: &[Plane]) -> Plane {
    let (w, h) = (rgb[0].w, rgb[0].h);
    let mut m = Plane::new(w, h);
    par_rows(&mut m.d, w, |row, out| {
        for (x, o) in out.iter_mut().enumerate() {
            let i = row * w + x;
            let (r, g, b) = (rgb[0].d[i], rgb[1].d[i], rgb[2].d[i]);
            let y = 0.299 * r + 0.587 * g + 0.114 * b;
            let cr = (r - y) * 0.713 + 128.0;
            let cb = (b - y) * 0.564 + 128.0;
            *o = (cr > 135.0 && cr < 173.0 && cb > 77.0 && cb < 127.0 && y > 50.0) as u8 as f32;
        }
    });
    let m = morph(&morph(&m, f32::min), f32::max); // opening, 5x5
    blur(&m, 4.0)
}

/// 5x5 erosion (`f32::min`) or dilation (`f32::max`), separable.
fn morph(s: &Plane, op: fn(f32, f32) -> f32) -> Plane {
    let w = s.w;
    let mut t = Plane::new(w, s.h);
    par_rows(&mut t.d, w, |y, out| {
        let row = &s.d[y * w..(y + 1) * w];
        for (x, o) in out.iter_mut().enumerate() {
            *o = [-2, -1, 1, 2].iter().fold(row[x], |v, &k| op(v, row[refl(x as isize + k, w)]));
        }
    });
    let mut o = Plane::new(w, s.h);
    par_rows(&mut o.d, w, |y, out| {
        out.copy_from_slice(&t.d[y * w..(y + 1) * w]);
        for k in [-2, -1, 1, 2] {
            let r = refl(y as isize + k, s.h) * w;
            out.iter_mut().zip(&t.d[r..r + w]).for_each(|(o, &v)| *o = op(*o, v));
        }
    });
    o
}

pub fn luma(rgb: &[Plane]) -> Plane {
    let d = (0..rgb[0].d.len()).into_par_iter().map(|i| 0.299 * rgb[0].d[i] + 0.587 * rgb[1].d[i] + 0.114 * rgb[2].d[i]).collect();
    Plane { w: rgb[0].w, h: rgb[0].h, d }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// |H(f)| of a second-order-section cascade.
    fn gain(sos: &[[f64; 5]], f: f64, fs: f64) -> f64 {
        let w = 2.0 * PI * f / fs;
        let z1 = C(w.cos(), -w.sin());
        let z2 = z1.mul(z1);
        let mut h = C(1.0, 0.0);
        for s in sos {
            let num = C(s[0], 0.0).add(C(s[1], 0.0).mul(z1)).add(C(s[2], 0.0).mul(z2));
            let den = C(1.0, 0.0).add(C(s[3], 0.0).mul(z1)).add(C(s[4], 0.0).mul(z2));
            h = h.mul(num.div(den));
        }
        h.0.hypot(h.1)
    }

    #[test]
    fn butterworth_matches_scipy() {
        // scipy.signal.butter(2, [lo, hi], "band", fs=fs, output="sos"),
        // |H| at lo/2, (lo+hi)/2, 2*hi.
        let cases = [
            (0.8, 1.5, 25.0, [0.073031, 0.99976, 0.066829]),
            (0.15, 0.5, 25.0, [0.141812, 0.997405, 0.140566]),
            (3.0, 5.0, 120.0, [0.055583, 0.999885, 0.053259]),
            (8.0, 12.0, 120.0, [0.041401, 0.999966, 0.031645]),
        ];
        for (lo, hi, fs, want) in cases {
            let sos = butter_band(2, lo, hi, fs);
            for (f, w) in [lo / 2.0, (lo + hi) / 2.0, hi * 2.0].into_iter().zip(want) {
                let g = gain(&sos, f, fs);
                assert!((g - w).abs() < 1e-5, "{lo}-{hi} Hz @ {fs}: |H({f})| = {g}, scipy {w}");
            }
        }
    }

    #[test]
    fn pyramid_round_trip() {
        let mut p = Plane::new(37, 23);
        p.d.iter_mut().enumerate().for_each(|(i, v)| *v = ((i * 7919) % 255) as f32);
        let r = collapse(&laplacian(&p));
        let err = p.d.iter().zip(&r.d).map(|(a, b)| (a - b).abs()).fold(0.0, f32::max);
        assert!(err < 1e-3, "max error {err}");
    }
}

