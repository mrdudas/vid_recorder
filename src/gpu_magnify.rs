//! Motion magnification on the GPU: the same methods as `magnify.rs`
//! (phase-based with Riesz pyramids, also for the tremor bands, and linear
//! Eulerian), as WGSL compute shaders. The CPU code stays the reference;
//! the tests compare the two.
//!
//! Every frame is one submission: the luma goes up, the Laplacian pyramid,
//! the per-pixel phase and temporal filters (their state stays on the GPU),
//! the spatial smoothing and the collapse run as a fixed list of dispatches
//! built once, and the magnified luma of every band comes back.

use crate::gpu_view::Gpu;
use crate::magnify::{Band, LEVELS, LINEAR_WEIGHT, Plane, butter_band};
use eframe::egui_wgpu::wgpu;
use std::sync::Arc;

const HEADER: &str = r#"
struct P { w: u32, h: u32, sw: u32, sh: u32, a: f32, b: f32, c: f32, d: f32, k: array<vec4<f32>, 8> };
@group(0) @binding(0) var<uniform> p: P;
// OpenCV BORDER_REFLECT_101.
fn refl(i: i32, n: i32) -> i32 {
    if (n == 1) { return 0; }
    var j = i;
    if (j < 0) { j = -j; }
    if (j >= n) { j = 2 * n - 2 - j; }
    return clamp(j, 0, n - 1);
}
fn kw(i: u32) -> f32 { return p.k[i / 4u][i % 4u]; }
fn k5(i: i32) -> f32 {
    var k = array<f32, 5>(0.0625, 0.25, 0.375, 0.25, 0.0625);
    return k[i];
}
// cv2.pyrUp taps along one axis: index and weight of tap t (0..2).
fn up_tap(x: i32, t: i32, n: i32) -> vec2<f32> {
    let i = x / 2;
    if ((x & 1) == 0) {
        let w = select(0.125, 0.75, t == 1);
        return vec2<f32>(f32(refl(i - 1 + t, n)), w);
    }
    if (t == 2) { return vec2<f32>(0.0, 0.0); }
    return vec2<f32>(f32(refl(i + t, n)), 0.5);
}
"#;

/// Gaussian blur and 2x decimation (cv2.pyrDown): src sw×sh → out w×h.
const DOWN: &str = r#"
@group(0) @binding(1) var<storage, read> src: array<f32>;
@group(0) @binding(2) var<storage, read_write> dst: array<f32>;
@compute @workgroup_size(16, 16) fn main(@builtin(global_invocation_id) g: vec3<u32>) {
    if (g.x >= p.w || g.y >= p.h) { return; }
    let sw = i32(p.sw);
    let sh = i32(p.sh);
    var v = 0.0;
    for (var j = 0; j < 5; j++) {
        let r = refl(2 * i32(g.y) + j - 2, sh) * sw;
        var acc = 0.0;
        for (var i = 0; i < 5; i++) {
            acc += k5(i) * src[r + refl(2 * i32(g.x) + i - 2, sw)];
        }
        v += k5(j) * acc;
    }
    dst[g.y * p.w + g.x] = v;
}
"#;

/// Upsampled small (sw×sh) to w×h, `b` × big added: lap = big − up(small)
/// (b = 1, a = −1) or g = up(small) + level (a = 1, b = 1).
const UP: &str = r#"
@group(0) @binding(1) var<storage, read> small: array<f32>;
@group(0) @binding(2) var<storage, read> big: array<f32>;
@group(0) @binding(3) var<storage, read_write> dst: array<f32>;
@compute @workgroup_size(16, 16) fn main(@builtin(global_invocation_id) g: vec3<u32>) {
    if (g.x >= p.w || g.y >= p.h) { return; }
    let sw = i32(p.sw);
    let sh = i32(p.sh);
    var u = 0.0;
    for (var ty = 0; ty < 3; ty++) {
        let ry = up_tap(i32(g.y), ty, sh);
        if (ry.y == 0.0) { continue; }
        var acc = 0.0;
        for (var tx = 0; tx < 3; tx++) {
            let rx = up_tap(i32(g.x), tx, sw);
            acc += rx.y * small[i32(ry.x) * sw + i32(rx.x)];
        }
        u += ry.y * acc;
    }
    let i = g.y * p.w + g.x;
    dst[i] = p.a * u + p.b * big[i];
}
"#;

/// Riesz transform pair (3×3, reflect-101) into rr = [r1, r2].
const RIESZ: &str = r#"
@group(0) @binding(1) var<storage, read> re: array<f32>;
@group(0) @binding(2) var<storage, read_write> rr: array<f32>;
@compute @workgroup_size(16, 16) fn main(@builtin(global_invocation_id) g: vec3<u32>) {
    if (g.x >= p.w || g.y >= p.h) { return; }
    let w = i32(p.w);
    let h = i32(p.h);
    var c = array<f32, 3>(0.12, 0.34, 0.12);
    var r1 = 0.0;
    var r2 = 0.0;
    for (var j = 0; j < 3; j++) {
        let y = refl(i32(g.y) + j - 1, h);
        for (var i = 0; i < 3; i++) {
            let x = refl(i32(g.x) + i - 1, w);
            let v = re[y * w + x];
            // RIESZ: columns −c, 0, +c; RIESZ_T: rows −c, 0, +c.
            r1 += f32(i - 1) * c[j] * v;
            r2 += f32(j - 1) * c[i] * v;
        }
    }
    let n = p.w * p.h;
    let k = g.y * p.w + g.x;
    rr[k] = r1;
    rr[n + k] = r2;
}
"#;

/// Quaternionic phase difference to the previous frame, leaky
/// integration (leak = a), amplitude; prev := current. First frame
/// (b = 1 in `frame`): prev is the current frame.
const STEP: &str = r#"
struct F { first: u32, _a: u32, _b: u32, _c: u32 };
@group(0) @binding(1) var<storage, read> re: array<f32>;
@group(0) @binding(2) var<storage, read> rr: array<f32>;
@group(0) @binding(3) var<storage, read_write> prev: array<f32>;
@group(0) @binding(4) var<storage, read_write> ph: array<f32>;
@group(0) @binding(5) var<storage, read_write> amp: array<f32>;
@group(0) @binding(6) var<storage, read_write> pcs: array<f32>;
@group(0) @binding(7) var<uniform> f: F;
@compute @workgroup_size(16, 16) fn main(@builtin(global_invocation_id) g: vec3<u32>) {
    if (g.x >= p.w || g.y >= p.h) { return; }
    let n = p.w * p.h;
    let i = g.y * p.w + g.x;
    let a = re[i];
    let b = rr[i];
    let c = rr[n + i];
    if (f.first == 1u) {
        prev[i] = a;
        prev[n + i] = b;
        prev[2u * n + i] = c;
    }
    let pa = prev[i];
    let pb = prev[n + i];
    let pc = prev[2u * n + i];
    let qr = a * pa + b * pb + c * pc;
    let qx = -a * pb + pa * b;
    let qy = -a * pc + pa * c;
    let qamp = sqrt(qr * qr + qx * qx + qy * qy) + 1e-9;
    let v = sqrt(qx * qx + qy * qy);
    let dphi = atan2(v, qr);
    let nrm = v + 1e-9;
    let c0 = p.a * ph[2u * i] + dphi * qx / nrm;
    let s0 = p.a * ph[2u * i + 1u] + dphi * qy / nrm;
    ph[2u * i] = c0;
    ph[2u * i + 1u] = s0;
    amp[i] = sqrt(qamp);
    pcs[i] = c0;
    pcs[n + i] = s0;
    prev[i] = a;
    prev[n + i] = b;
    prev[2u * n + i] = c;
}
"#;

/// Two signals (cos, sin phase) through the band-pass (`sw` sections,
/// coefficients in k), times the amplitude.
const IIR2: &str = r#"
@group(0) @binding(1) var<storage, read> x: array<f32>;
@group(0) @binding(2) var<storage, read> amp: array<f32>;
@group(0) @binding(3) var<storage, read_write> z: array<f32>;
@group(0) @binding(4) var<storage, read_write> y: array<f32>;
@compute @workgroup_size(16, 16) fn main(@builtin(global_invocation_id) g: vec3<u32>) {
    if (g.x >= p.w || g.y >= p.h) { return; }
    let n = p.w * p.h;
    let i = g.y * p.w + g.x;
    let ns = p.sw;
    for (var s = 0u; s < 2u; s++) {
        var v = x[s * n + i];
        let base = (s * n + i) * ns * 2u;
        for (var q = 0u; q < ns; q++) {
            let b0 = kw(q * 5u);
            let b1 = kw(q * 5u + 1u);
            let b2 = kw(q * 5u + 2u);
            let a1 = kw(q * 5u + 3u);
            let a2 = kw(q * 5u + 4u);
            let z0 = z[base + 2u * q];
            let z1 = z[base + 2u * q + 1u];
            let o = b0 * v + z0;
            z[base + 2u * q] = b1 * v - a1 * o + z1;
            z[base + 2u * q + 1u] = b2 * v - a2 * o;
            v = o;
        }
        y[s * n + i] = v * amp[i];
    }
}
"#;

/// One signal through the band-pass, added back: y = x + a · bandpass(x).
const IIR_ADD: &str = r#"
@group(0) @binding(1) var<storage, read> x: array<f32>;
@group(0) @binding(2) var<storage, read_write> z: array<f32>;
@group(0) @binding(3) var<storage, read_write> y: array<f32>;
@compute @workgroup_size(16, 16) fn main(@builtin(global_invocation_id) g: vec3<u32>) {
    if (g.x >= p.w || g.y >= p.h) { return; }
    let i = g.y * p.w + g.x;
    let ns = p.sw;
    var v = x[i];
    for (var q = 0u; q < ns; q++) {
        let b0 = kw(q * 5u);
            let b1 = kw(q * 5u + 1u);
            let b2 = kw(q * 5u + 2u);
            let a1 = kw(q * 5u + 3u);
            let a2 = kw(q * 5u + 4u);
        let k = (i * ns + q) * 2u;
        let o = b0 * v + z[k];
        z[k] = b1 * v - a1 * o + z[k + 1u];
        z[k + 1u] = b2 * v - a2 * o;
        v = o;
    }
    y[i] = x[i] + p.a * v;
}
"#;

/// Separable Gaussian, one direction (sh = 0: along x, 1: along y), radius
/// sw, weights in k; z of the dispatch picks the plane.
const BLUR: &str = r#"
@group(0) @binding(1) var<storage, read> src: array<f32>;
@group(0) @binding(2) var<storage, read_write> dst: array<f32>;
@compute @workgroup_size(16, 16) fn main(@builtin(global_invocation_id) g: vec3<u32>) {
    if (g.x >= p.w || g.y >= p.h) { return; }
    let w = i32(p.w);
    let h = i32(p.h);
    let off = i32(g.z * p.w * p.h);
    let r = i32(p.sw);
    let x = i32(g.x);
    let y = i32(g.y);
    var v = 0.0;
    if (p.sh == 0u) {
        let row = off + y * w;
        if (x >= r && x + r < w) {
            for (var t = -r; t <= r; t++) {
                let i = u32(t + r);
                v += p.k[i >> 2u][i & 3u] * src[row + x + t];
            }
        } else {
            for (var t = -r; t <= r; t++) {
                let i = u32(t + r);
                v += p.k[i >> 2u][i & 3u] * src[row + refl(x + t, w)];
            }
        }
    } else {
        if (y >= r && y + r < h) {
            for (var t = -r; t <= r; t++) {
                let i = u32(t + r);
                v += p.k[i >> 2u][i & 3u] * src[off + (y + t) * w + x];
            }
        } else {
            for (var t = -r; t <= r; t++) {
                let i = u32(t + r);
                v += p.k[i >> 2u][i & 3u] * src[off + refl(y + t, h) * w + x];
            }
        }
    }
    dst[off + y * w + x] = v;
}
"#;

/// Magnified level: the smoothed phase (c, s weighted by amplitude, over
/// the smoothed amplitude) times alpha (a) turns re back.
const OUT: &str = r#"
@group(0) @binding(1) var<storage, read> re: array<f32>;
@group(0) @binding(2) var<storage, read> rr: array<f32>;
@group(0) @binding(3) var<storage, read> den: array<f32>;
@group(0) @binding(4) var<storage, read> cs: array<f32>;
@group(0) @binding(5) var<storage, read_write> o: array<f32>;
@compute @workgroup_size(16, 16) fn main(@builtin(global_invocation_id) g: vec3<u32>) {
    if (g.x >= p.w || g.y >= p.h) { return; }
    let n = p.w * p.h;
    let i = g.y * p.w + g.x;
    let d = den[i] + 1e-9;
    let fc = cs[i] / d;
    let fs = cs[n + i] / d;
    let mag = p.a * sqrt(fc * fc + fs * fs) + 1e-9;
    let k = sin(mag) / mag * p.a;
    o[i] = cos(mag) * re[i] - k * fc * rr[i] - k * fs * rr[n + i];
}
"#;

/// 16-bit RGB from the decoder (rgb48le, two samples per word) into
/// R, G, B planes and luma on the 0..255 scale (as `export::read_frame` and
/// `magnify::luma`).
const UNPACK: &str = r#"
@group(0) @binding(1) var<storage, read> raw: array<u32>;
@group(0) @binding(2) var<storage, read_write> rgb: array<f32>;
@group(0) @binding(3) var<storage, read_write> luma: array<f32>;
fn sample(s: u32) -> f32 {
    let w = raw[s / 2u];
    return f32((w >> (16u * (s % 2u))) & 0xffffu) / 257.0;
}
@compute @workgroup_size(16, 16) fn main(@builtin(global_invocation_id) g: vec3<u32>) {
    if (g.x >= p.w || g.y >= p.h) { return; }
    let n = p.w * p.h;
    let i = g.y * p.w + g.x;
    let r = sample(3u * i);
    let gg = sample(3u * i + 1u);
    let b = sample(3u * i + 2u);
    rgb[i] = r;
    rgb[n + i] = gg;
    rgb[2u * n + i] = b;
    luma[i] = 0.299 * r + 0.587 * gg + 0.114 * b;
}
"#;

/// The export grid (cols = sw, panels = sh): the frame, then every band's
/// magnified luma applied to the colours (c + new − luma), as rgb0 bytes.
const COMPOSE: &str = r#"
@group(0) @binding(1) var<storage, read> rgb: array<f32>;
@group(0) @binding(2) var<storage, read> luma: array<f32>;
@group(0) @binding(3) var<storage, read> bands: array<f32>;
@group(0) @binding(4) var<storage, read_write> grid: array<u32>;
fn byte(v: f32) -> u32 { return u32(clamp(round(v), 0.0, 255.0)); }
@compute @workgroup_size(16, 16) fn main(@builtin(global_invocation_id) g: vec3<u32>) {
    let gw = p.w * p.sw;
    let gh = p.h * ((p.sh + p.sw - 1u) / p.sw);
    if (g.x >= gw || g.y >= gh) { return; }
    let panel = (g.y / p.h) * p.sw + g.x / p.w;
    let n = p.w * p.h;
    let i = (g.y % p.h) * p.w + g.x % p.w;
    var c = vec3<f32>(0.0);
    if (panel < p.sh) {
        c = vec3<f32>(rgb[i], rgb[n + i], rgb[2u * n + i]);
        if (panel > 0u) {
            c += bands[(panel - 1u) * n + i] - luma[i];
        }
    }
    grid[g.y * gw + g.x] = byte(c.x) | (byte(c.y) << 8u) | (byte(c.z) << 16u);
}
"#;

#[derive(Clone, Copy)]
enum K {
    Down,
    Up,
    Riesz,
    Step,
    Iir2,
    IirAdd,
    Blur,
    Out,
    Unpack,
    Compose,
}

const KERNELS: [(K, &str); 10] = [
    (K::Down, DOWN),
    (K::Up, UP),
    (K::Riesz, RIESZ),
    (K::Step, STEP),
    (K::Iir2, IIR2),
    (K::IirAdd, IIR_ADD),
    (K::Blur, BLUR),
    (K::Out, OUT),
    (K::Unpack, UNPACK),
    (K::Compose, COMPOSE),
];

/// Parameters of one dispatch (the `P` struct).
#[derive(Clone, Copy, Default)]
struct Params {
    w: u32,
    h: u32,
    sw: u32,
    sh: u32,
    a: f32,
    b: f32,
    k: [f32; 32],
}

impl Params {
    fn bytes(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(160);
        for x in [self.w, self.h, self.sw, self.sh] {
            v.extend(x.to_le_bytes());
        }
        for x in [self.a, self.b, 0.0, 0.0].iter().chain(&self.k) {
            v.extend(x.to_le_bytes());
        }
        v
    }
}

struct Op {
    kernel: usize,
    bind: wgpu::BindGroup,
    groups: [u32; 3],
}

/// Buffers and the fixed list of dispatches of one method.
struct Graph {
    gpu: Arc<Gpu>,
    pipes: Vec<wgpu::ComputePipeline>,
    bufs: Vec<wgpu::Buffer>,
    ops: Vec<Op>,
    frame: wgpu::Buffer,
    first: bool,
    input: usize,
    outputs: Vec<usize>,
    staging: wgpu::Buffer,
    w: usize,
    h: usize,
}

/// Gaussian weights, radius 3 sigma (as `magnify::blur`).
fn gauss(sigma: f32) -> (u32, [f32; 32]) {
    let r = (3.0 * sigma).ceil() as i32;
    let mut k = [0f32; 32];
    let sum: f32 = (-r..=r).map(|i| (-(i * i) as f32 / (2.0 * sigma * sigma)).exp()).sum();
    for i in -r..=r {
        k[(i + r) as usize] = (-(i * i) as f32 / (2.0 * sigma * sigma)).exp() / sum;
    }
    (r as u32, k)
}

/// Band-pass sections into the parameter block.
fn sos_params(b: &Band, fps: f64) -> (u32, [f32; 32]) {
    let sos = butter_band(2, b.lo, b.hi, fps);
    let mut k = [0f32; 32];
    for (q, s) in sos.iter().enumerate() {
        for (j, v) in s.iter().enumerate() {
            k[q * 5 + j] = *v as f32;
        }
    }
    (sos.len() as u32, k)
}

impl Graph {
    fn new(gpu: Arc<Gpu>, w: usize, h: usize) -> Graph {
        let d = gpu.device();
        let pipes = KERNELS
            .iter()
            .map(|(_, body)| {
                let module = d.create_shader_module(wgpu::ShaderModuleDescriptor {
                    label: Some("magnify"),
                    source: wgpu::ShaderSource::Wgsl(format!("{HEADER}{body}").into()),
                });
                d.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                    label: Some("magnify"),
                    layout: None,
                    module: &module,
                    entry_point: Some("main"),
                    compilation_options: Default::default(),
                    cache: None,
                })
            })
            .collect();
        let frame = d.create_buffer(&wgpu::BufferDescriptor {
            label: Some("frame"),
            size: 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let staging = d.create_buffer(&wgpu::BufferDescriptor { label: None, size: 4, usage: wgpu::BufferUsages::MAP_READ, mapped_at_creation: false });
        Graph { gpu, pipes, bufs: Vec::new(), ops: Vec::new(), frame, first: true, input: 0, outputs: Vec::new(), staging, w, h }
    }

    /// A zeroed buffer of `n` floats.
    fn buf(&mut self, n: usize) -> usize {
        self.bufs.push(self.gpu.device().create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: (n.max(1) * 4) as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        }));
        self.bufs.len() - 1
    }

    /// One dispatch over w×h (×planes) with buffers bound at 1, 2, ….
    fn op(&mut self, k: K, p: Params, bufs: &[usize], planes: u32) {
        let op = self.make_op(k, p, bufs, planes);
        self.ops.push(op);
    }

    fn make_op(&mut self, k: K, p: Params, bufs: &[usize], planes: u32) -> Op {
        let d = self.gpu.device();
        let ub = d.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: 160,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        self.gpu.queue().write_buffer(&ub, 0, &p.bytes());
        let kernel = KERNELS.iter().position(|(x, _)| std::mem::discriminant(x) == std::mem::discriminant(&k)).unwrap();
        let mut entries = vec![wgpu::BindGroupEntry { binding: 0, resource: ub.as_entire_binding() }];
        for (i, &b) in bufs.iter().enumerate() {
            entries.push(wgpu::BindGroupEntry { binding: i as u32 + 1, resource: self.bufs[b].as_entire_binding() });
        }
        if matches!(k, K::Step) {
            entries.push(wgpu::BindGroupEntry { binding: 7, resource: self.frame.as_entire_binding() });
        }
        let bind = d.create_bind_group(&wgpu::BindGroupDescriptor { label: None, layout: &self.pipes[kernel].get_bind_group_layout(0), entries: &entries });
        Op { kernel, bind, groups: [p.w.div_ceil(16), p.h.div_ceil(16), planes] }
    }

    /// Laplacian pyramid of the input: (levels, sizes); level LEVELS is
    /// the low-pass residual.
    fn laplacian(&mut self, src: usize) -> (Vec<usize>, Vec<(u32, u32)>) {
        let mut sizes = vec![(self.w as u32, self.h as u32)];
        for _ in 0..LEVELS {
            let (w, h) = *sizes.last().unwrap();
            sizes.push((w.div_ceil(2), h.div_ceil(2)));
        }
        let mut gs = vec![src];
        for l in 0..LEVELS {
            let (w, h) = sizes[l + 1];
            let b = self.buf((w * h) as usize);
            self.op(K::Down, Params { w, h, sw: sizes[l].0, sh: sizes[l].1, ..Default::default() }, &[gs[l], b], 1);
            gs.push(b);
        }
        let mut lap = Vec::new();
        for l in 0..LEVELS {
            let (w, h) = sizes[l];
            let b = self.buf((w * h) as usize);
            let (sw, sh) = sizes[l + 1];
            self.op(K::Up, Params { w, h, sw, sh, a: -1.0, b: 1.0, ..Default::default() }, &[gs[l + 1], gs[l], b], 1);
            lap.push(b);
        }
        lap.push(gs[LEVELS]);
        (lap, sizes)
    }

    /// Collapse levels (finest first) into a new full-size buffer.
    fn collapse(&mut self, levels: &[usize], sizes: &[(u32, u32)]) -> usize {
        let mut g = levels[LEVELS];
        for l in (0..LEVELS).rev() {
            let (w, h) = sizes[l];
            let b = self.buf((w * h) as usize);
            let (sw, sh) = sizes[l + 1];
            self.op(K::Up, Params { w, h, sw, sh, a: 1.0, b: 1.0, ..Default::default() }, &[g, levels[l], b], 1);
            g = b;
        }
        g
    }

    /// Separable blur of `planes` planes of `src` (w×h each) into a new buffer.
    fn blur(&mut self, src: usize, (w, h): (u32, u32), planes: u32, sigma: f32) -> usize {
        let n = (w * h) as usize * planes as usize;
        let (t, o) = (self.buf(n), self.buf(n));
        let (r, k) = gauss(sigma);
        self.op(K::Blur, Params { w, h, sw: r, sh: 0, k, ..Default::default() }, &[src, t], planes);
        self.op(K::Blur, Params { w, h, sw: r, sh: 1, k, ..Default::default() }, &[t, o], planes);
        o
    }

    fn finish(&mut self) {
        let n = self.w * self.h * 4 * self.outputs.len();
        self.staging = self.gpu.device().create_buffer(&wgpu::BufferDescriptor {
            label: Some("out"),
            size: n.max(4) as u64,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
    }

    /// One frame: luma in, one plane per output out.
    fn run(&mut self, luma: &Plane) -> Vec<Plane> {
        assert_eq!((luma.w, luma.h), (self.w, self.h), "frame size changed");
        let (d, q) = (self.gpu.device(), self.gpu.queue());
        let bytes: Vec<u8> = luma.d.iter().flat_map(|v| v.to_le_bytes()).collect();
        q.write_buffer(&self.bufs[self.input], 0, &bytes);
        q.write_buffer(&self.frame, 0, &[self.first as u32, 0, 0, 0].map(u32::to_le_bytes).concat());
        self.first = false;
        let mut enc = d.create_command_encoder(&Default::default());
        {
            let mut pass = enc.begin_compute_pass(&Default::default());
            for op in &self.ops {
                pass.set_pipeline(&self.pipes[op.kernel]);
                pass.set_bind_group(0, &op.bind, &[]);
                pass.dispatch_workgroups(op.groups[0], op.groups[1], op.groups[2]);
            }
        }
        let n = (self.w * self.h * 4) as u64;
        for (i, &o) in self.outputs.iter().enumerate() {
            enc.copy_buffer_to_buffer(&self.bufs[o], 0, &self.staging, i as u64 * n, n);
        }
        let index = q.submit([enc.finish()]);
        self.staging.slice(..).map_async(wgpu::MapMode::Read, |_| {});
        d.poll(wgpu::PollType::Wait { submission_index: Some(index), timeout: None }).expect("GPU");
        let out = {
            let data = self.staging.slice(..).get_mapped_range().expect("GPU readback");
            (0..self.outputs.len())
                .map(|i| {
                    let s = &data[i * n as usize..(i + 1) * n as usize];
                    Plane { w: self.w, h: self.h, d: s.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect() }
                })
                .collect()
        };
        self.staging.unmap();
        out
    }
}

/// Dispatches of the phase-based method on a Laplacian pyramid; the
/// collapsed luma of every band.
fn phase_ops(g: &mut Graph, lap: &[usize], sizes: &[(u32, u32)], bands: &[Band], fps: f64) -> Vec<usize> {
    let lo = bands.iter().map(|b| b.lo).fold(f64::MAX, f64::min);
    let leak = (-2.0 * std::f64::consts::PI * lo / 10.0 / fps).exp() as f32;
    // Per band: the levels to collapse (finest and coarsest unchanged).
    let mut levels: Vec<Vec<usize>> = bands.iter().map(|_| lap.to_vec()).collect();
    for l in 1..LEVELS {
        let (w, h) = sizes[l];
        let n = (w * h) as usize;
        let (rr, prev, ph, amp, pcs) = (g.buf(2 * n), g.buf(3 * n), g.buf(2 * n), g.buf(n), g.buf(2 * n));
        let p = Params { w, h, ..Default::default() };
        g.op(K::Riesz, p, &[lap[l], rr], 1);
        g.op(K::Step, Params { a: leak, ..p }, &[lap[l], rr, prev, ph, amp, pcs], 1);
        let den = g.blur(amp, (w, h), 1, 2.0);
        for (bi, band) in bands.iter().enumerate() {
            let (ns, k) = sos_params(band, fps);
            let (z, cs) = (g.buf(2 * n * ns as usize * 2), g.buf(2 * n));
            g.op(K::Iir2, Params { sw: ns, k, ..p }, &[pcs, amp, z, cs], 1);
            let csb = g.blur(cs, (w, h), 2, 2.0);
            let o = g.buf(n);
            g.op(K::Out, Params { a: band.alpha, ..p }, &[lap[l], rr, den, csb, o], 1);
            levels[bi][l] = o;
        }
    }
    levels.iter().map(|lv| g.collapse(lv, sizes)).collect()
}

/// Dispatches of the linear method; the collapsed luma of every band.
fn linear_ops(g: &mut Graph, lap: &[usize], sizes: &[(u32, u32)], bands: &[Band], fps: f64) -> Vec<usize> {
    let mut outs = Vec::new();
    for band in bands {
        let (ns, k) = sos_params(band, fps);
        let mut levels = lap.to_vec();
        for (l, &wt) in LINEAR_WEIGHT.iter().enumerate() {
            if wt == 0.0 {
                continue;
            }
            let (w, h) = sizes[l];
            let n = (w * h) as usize;
            let (z, o) = (g.buf(n * ns as usize * 2), g.buf(n));
            g.op(K::IirAdd, Params { w, h, sw: ns, a: band.alpha * wt, k, ..Default::default() }, &[lap[l], z, o], 1);
            levels[l] = o;
        }
        outs.push(g.collapse(&levels, sizes));
    }
    outs
}

/// Phase-based motion magnification of luma on the GPU, one output per band
/// (as [`crate::magnify::Phase`]).
pub struct Phase {
    graph: Graph,
}

impl Phase {
    pub fn band_count(&self) -> usize {
        self.graph.outputs.len()
    }

    pub fn new(gpu: Arc<Gpu>, bands: &[Band], fps: f64, w: usize, h: usize) -> Phase {
        let mut g = Graph::new(gpu, w, h);
        g.input = g.buf(w * h);
        let (lap, sizes) = g.laplacian(g.input);
        g.outputs = phase_ops(&mut g, &lap, &sizes, bands, fps);
        g.finish();
        Phase { graph: g }
    }

    pub fn process(&mut self, luma: &Plane) -> Vec<Plane> {
        self.graph.run(luma)
    }
}

/// Linear Eulerian magnification on the GPU (as [`crate::magnify::Linear`]).
pub struct Linear {
    graph: Graph,
}

impl Linear {
    pub fn band_count(&self) -> usize {
        self.graph.outputs.len()
    }

    pub fn new(gpu: Arc<Gpu>, bands: &[Band], fps: f64, w: usize, h: usize) -> Linear {
        let mut g = Graph::new(gpu, w, h);
        g.input = g.buf(w * h);
        let (lap, sizes) = g.laplacian(g.input);
        g.outputs = linear_ops(&mut g, &lap, &sizes, bands, fps);
        g.finish();
        Linear { graph: g }
    }

    pub fn process(&mut self, luma: &Plane) -> Vec<Plane> {
        self.graph.run(luma)
    }
}

/// A method of the GPU export.
pub enum Method {
    Phase(Vec<Band>),
    Linear(Vec<Band>),
}

/// Frames in flight in the GPU export.
const EXPORT_SLOTS: usize = 3;

/// The whole export frame on the GPU: the decoder's 16-bit RGB goes up,
/// the luma, one shared Laplacian pyramid, every method's bands, the
/// colours and the grid are computed there, and the grid comes back as rgb0
/// bytes for the encoder. Frames are pipelined (`submit` / `take`).
pub struct Export {
    g: Graph,
    raw: usize,
    bands: usize,
    grid: usize,
    compose: Op,
    slots: Vec<wgpu::Buffer>,
    /// Slot and submission of frames in flight; `None` slot: warm-up frame
    /// (no grid read back).
    pending: std::collections::VecDeque<(Option<usize>, wgpu::SubmissionIndex)>,
    next: usize,
    out: Vec<u8>,
    panels: usize,
    cols: usize,
}

impl Export {
    /// Grid of `cols` columns: the frame and then every band (`w`×`h` panels).
    pub fn new(gpu: Arc<Gpu>, methods: &[Method], fps: f64, w: usize, h: usize, cols: usize) -> Export {
        let mut g = Graph::new(gpu, w, h);
        let n = w * h;
        let raw = g.buf((3 * n).div_ceil(2));
        let (rgb, luma) = (g.buf(3 * n), g.buf(n));
        let p = Params { w: w as u32, h: h as u32, ..Default::default() };
        g.op(K::Unpack, p, &[raw, rgb, luma], 1);
        let (lap, sizes) = g.laplacian(luma);
        let mut outs = Vec::new();
        for m in methods {
            outs.extend(match m {
                Method::Phase(b) => phase_ops(&mut g, &lap, &sizes, b, fps),
                Method::Linear(b) => linear_ops(&mut g, &lap, &sizes, b, fps),
            });
        }
        let bands = g.buf(n * outs.len().max(1));
        let panels = 1 + outs.len();
        let rows = panels.div_ceil(cols);
        let grid = g.buf(cols * w * rows * h);
        let compose = g.make_op(K::Compose, Params { w: w as u32, h: h as u32, sw: cols as u32, sh: panels as u32, ..Default::default() }, &[rgb, luma, bands, grid], 1);
        let compose = Op { groups: [((cols * w) as u32).div_ceil(16), ((rows * h) as u32).div_ceil(16), 1], ..compose };
        g.outputs = outs;
        let size = (cols * w * rows * h * 4) as u64;
        let slots = (0..EXPORT_SLOTS)
            .map(|_| {
                g.gpu.device().create_buffer(&wgpu::BufferDescriptor {
                    label: Some("grid"),
                    size,
                    usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                })
            })
            .collect();
        Export { g, raw, bands, grid, compose, slots, pending: Default::default(), next: 0, out: Vec::new(), panels, cols }
    }

    pub fn grid_size(&self) -> (usize, usize) {
        (self.cols * self.g.w, self.panels.div_ceil(self.cols) * self.g.h)
    }

    /// All slots busy: `take` before the next `submit`.
    pub fn full(&self) -> bool {
        self.pending.len() >= EXPORT_SLOTS
    }

    /// Frames in flight.
    pub fn pending(&self) -> usize {
        self.pending.len()
    }

    /// One decoded frame (rgb48le); `keep`: its grid is wanted (not warm-up).
    pub fn submit(&mut self, rgb48: &[u8], keep: bool) {
        let g = &mut self.g;
        let (d, q) = (g.gpu.device().clone(), g.gpu.queue().clone());
        let mut bytes = rgb48.to_vec();
        bytes.resize(bytes.len().next_multiple_of(4), 0);
        q.write_buffer(&g.bufs[self.raw], 0, &bytes);
        q.write_buffer(&g.frame, 0, &[g.first as u32, 0, 0, 0].map(u32::to_le_bytes).concat());
        g.first = false;
        let mut enc = d.create_command_encoder(&Default::default());
        {
            let mut pass = enc.begin_compute_pass(&Default::default());
            for op in &g.ops {
                pass.set_pipeline(&g.pipes[op.kernel]);
                pass.set_bind_group(0, &op.bind, &[]);
                pass.dispatch_workgroups(op.groups[0], op.groups[1], op.groups[2]);
            }
        }
        let slot = keep.then_some(self.next);
        if keep {
            let n = (g.w * g.h * 4) as u64;
            for (i, &o) in g.outputs.iter().enumerate() {
                enc.copy_buffer_to_buffer(&g.bufs[o], 0, &g.bufs[self.bands], i as u64 * n, n);
            }
            {
                let mut pass = enc.begin_compute_pass(&Default::default());
                pass.set_pipeline(&g.pipes[self.compose.kernel]);
                pass.set_bind_group(0, &self.compose.bind, &[]);
                pass.dispatch_workgroups(self.compose.groups[0], self.compose.groups[1], 1);
            }
            let buf = &self.slots[self.next];
            enc.copy_buffer_to_buffer(&g.bufs[self.grid], 0, buf, 0, buf.size());
            self.next = (self.next + 1) % EXPORT_SLOTS;
        }
        let index = q.submit([enc.finish()]);
        if let Some(s) = slot {
            self.slots[s].slice(..).map_async(wgpu::MapMode::Read, |_| {});
        }
        self.pending.push_back((slot, index));
    }

    /// The oldest frame's grid (rgb0), waiting for the GPU; `Some(None)` for
    /// a warm-up frame, `None` when nothing is in flight.
    pub fn take(&mut self) -> Option<Option<&[u8]>> {
        let (slot, index) = self.pending.pop_front()?;
        let d = self.g.gpu.device();
        d.poll(wgpu::PollType::Wait { submission_index: Some(index), timeout: None }).expect("GPU");
        let Some(s) = slot else { return Some(None) };
        let buf = &self.slots[s];
        self.out.clear();
        self.out.extend_from_slice(&buf.slice(..).get_mapped_range().expect("GPU readback"));
        buf.unmap();
        Some(Some(&self.out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::magnify;

    /// A textured pattern that moves a little (sub-pixel) frame by frame.
    fn frame(w: usize, h: usize, t: usize) -> Plane {
        let dx = 0.6 * (t as f32 * 0.9).sin();
        let d = (0..w * h)
            .map(|i| {
                let (x, y) = ((i % w) as f32 + dx, (i / w) as f32);
                128.0 + 60.0 * (x * 0.31).sin() * (y * 0.23).cos() + 30.0 * ((x + y) * 0.11).sin()
            })
            .collect();
        Plane { w, h, d }
    }

    fn max_diff(a: &[Plane], b: &[Plane]) -> f32 {
        a.iter().zip(b).flat_map(|(a, b)| a.d.iter().zip(&b.d).map(|(x, y)| (x - y).abs())).fold(0.0, f32::max)
    }

    fn gpu() -> Option<Arc<Gpu>> {
        Gpu::headless().ok()
    }

    #[test]
    fn gpu_phase_matches_cpu() {
        let Some(g) = gpu() else { return };
        let (w, h) = (97, 61);
        let bands = [Band { lo: 0.8, hi: 1.5, alpha: 30.0 }, Band { lo: 3.0, hi: 5.0, alpha: 20.0 }];
        let mut cpu = magnify::Phase::new(&bands, 25.0);
        let mut gp = Phase::new(g, &bands, 25.0, w, h);
        // The phase method is ill-conditioned at isolated weakly textured
        // pixels: the CPU itself differs there by up to ~15 levels with
        // 1e-4 of input noise (see `phase_sensitivity`). So: the mean
        // difference and the share of pixels off by more than one level.
        let (mut sum, mut off, mut n) = (0f64, 0usize, 0usize);
        for t in 0..60 {
            let f = frame(w, h, t);
            for (a, b) in cpu.process(&f).iter().zip(&gp.process(&f)) {
                for (x, y) in a.d.iter().zip(&b.d) {
                    let d = (x - y).abs();
                    sum += d as f64;
                    off += (d > 1.0) as usize;
                    n += 1;
                }
            }
        }
        let (mean, share) = (sum / n as f64, off as f64 / n as f64);
        // CPU against itself with 1e-4 of noise: mean 0.027, share 0.0061;
        // GPU against CPU: 0.032 and 0.0079 (2026-11-28). Allow about twice.
        assert!(mean < 0.06 && share < 0.015, "mean difference {mean:.4}, share over 1 level {share:.5}");
    }

    /// How sensitive the CPU method itself is: the same frames with 1e-4
    /// of noise (`--ignored --nocapture`).
    #[test]
    #[ignore]
    fn phase_sensitivity() {
        let (w, h) = (97, 61);
        let bands = [Band { lo: 0.8, hi: 1.5, alpha: 30.0 }, Band { lo: 3.0, hi: 5.0, alpha: 20.0 }];
        let mut a = magnify::Phase::new(&bands, 25.0);
        let mut b = magnify::Phase::new(&bands, 25.0);
        let (mut sum, mut off, mut n) = (0f64, 0usize, 0usize);
        for t in 0..60 {
            let f = frame(w, h, t);
            let mut g = f.clone();
            g.d.iter_mut().enumerate().for_each(|(i, v)| *v += 1e-4 * ((i * 7919 + t) % 13) as f32 / 13.0);
            let (x, y) = (a.process(&f), b.process(&g));
            for (p, q) in x.iter().zip(&y) {
                for (u, v) in p.d.iter().zip(&q.d) {
                    let d = (u - v).abs();
                    sum += d as f64;
                    off += (d > 1.0) as usize;
                    n += 1;
                }
            }
        }
        eprintln!("CPU vs CPU + 1e-4: mean {:.4}, share over 1 level {:.5}", sum / n as f64, off as f64 / n as f64);
    }

    #[test]
    fn gpu_linear_matches_cpu() {
        let Some(g) = gpu() else { return };
        let (w, h) = (97, 61);
        let bands = [Band { lo: 0.8, hi: 1.5, alpha: 25.0 }, Band { lo: 0.15, hi: 0.5, alpha: 15.0 }];
        let mut cpu = magnify::Linear::new(&bands, 25.0);
        let mut gp = Linear::new(g, &bands, 25.0, w, h);
        let mut worst = 0f32;
        for t in 0..60 {
            let f = frame(w, h, t);
            worst = worst.max(max_diff(&cpu.process(&f), &gp.process(&f)));
        }
        assert!(worst < 0.01, "max difference {worst}");
    }

    /// The whole-frame export grid equals the per-method path: the frame,
    /// then per band the colours with the magnified luma change.
    #[test]
    fn gpu_export_grid_matches_per_method_path() {
        let Some(g) = gpu() else { return };
        let (w, h) = (64, 40);
        let bands = vec![Band { lo: 0.8, hi: 1.5, alpha: 30.0 }, Band { lo: 3.0, hi: 5.0, alpha: 20.0 }];
        let mut reference = Phase::new(g.clone(), &bands, 25.0, w, h);
        let mut ex = Export::new(g, &[Method::Phase(bands)], 25.0, w, h, 2);
        assert_eq!(ex.grid_size(), (2 * w, 2 * h));
        // The luma is computed on the GPU here, on the CPU in the reference:
        // rounding differences, amplified at a few pixels by the phase
        // method (see `gpu_phase_matches_cpu`).
        let (mut off, mut n) = (0usize, 0usize);
        for t in 0..20 {
            // An RGB frame: luma pattern, colours offset.
            let y = frame(w, h, t);
            let rgb: Vec<[f32; 3]> = y.d.iter().map(|&v| [v * 0.9 + 10.0, v, v * 0.8 + 20.0]).collect();
            let raw: Vec<u8> = rgb.iter().flat_map(|c| c.map(|v| ((v * 257.0).round() as u16).to_le_bytes())).flatten().collect();
            // Reference: luma as the GPU unpacks it, per-method magnification.
            let px: Vec<[f32; 3]> = raw.chunks_exact(6).map(|c| std::array::from_fn(|k| u16::from_le_bytes([c[2 * k], c[2 * k + 1]]) as f32 / 257.0)).collect();
            let luma = Plane { w, h, d: px.iter().map(|c| 0.299 * c[0] + 0.587 * c[1] + 0.114 * c[2]).collect() };
            let mags = reference.process(&luma);
            ex.submit(&raw, true);
            let grid = ex.take().unwrap().unwrap().to_vec();
            for gy in 0..2 * h {
                for gx in 0..2 * w {
                    let (panel, i) = ((gy / h) * 2 + gx / w, (gy % h) * w + gx % w);
                    let want: [f32; 3] = match panel {
                        0 => px[i],
                        1 | 2 => std::array::from_fn(|k| px[i][k] + mags[panel - 1].d[i] - luma.d[i]),
                        _ => [0.0; 3],
                    };
                    let got = &grid[(gy * 2 * w + gx) * 4..][..3];
                    for k in 0..3 {
                        let d = (got[k] as i32 - want[k].round().clamp(0.0, 255.0) as i32).abs();
                        // The frame itself and the empty panel exactly.
                        assert!(panel == 1 || panel == 2 || d == 0, "frame {t}, panel {panel}: {d}");
                        off += (d > 1) as usize;
                        n += 1;
                    }
                }
            }
        }
        assert!((off as f64) < n as f64 * 0.01, "{off} of {n} bytes off by more than 1");
    }

    /// Time of single kernels on the level-1 size of 1072×1074 (`--ignored`).
    #[test]
    #[ignore]
    fn gpu_kernel_times() {
        let Some(gp) = gpu() else { return };
        let (w, h) = (536u32, 537u32);
        let n = (w * h) as usize;
        let time = |name: &str, build: &dyn Fn(&mut Graph)| {
            let mut g = Graph::new(gp.clone(), w as usize, h as usize);
            for _ in 0..50 {
                build(&mut g);
            }
            let (d, q) = (g.gpu.device().clone(), g.gpu.queue().clone());
            let run = |g: &Graph| {
                let mut enc = d.create_command_encoder(&Default::default());
                {
                    let mut pass = enc.begin_compute_pass(&Default::default());
                    for op in &g.ops {
                        pass.set_pipeline(&g.pipes[op.kernel]);
                        pass.set_bind_group(0, &op.bind, &[]);
                        pass.dispatch_workgroups(op.groups[0], op.groups[1], op.groups[2]);
                    }
                }
                let i = q.submit([enc.finish()]);
                d.poll(wgpu::PollType::Wait { submission_index: Some(i), timeout: None }).unwrap();
            };
            run(&g);
            let t = std::time::Instant::now();
            for _ in 0..5 {
                run(&g);
            }
            eprintln!("{name}: {:.3} ms / dispatch", t.elapsed().as_secs_f64() * 1000.0 / 250.0);
        };
        let p = Params { w, h, ..Default::default() };
        time("blur 2 planes, one direction", &|g| {
            let (a, b) = (g.buf(2 * n), g.buf(2 * n));
            let (r, k) = gauss(2.0);
            g.op(K::Blur, Params { sw: r, sh: 1, k, ..p }, &[a, b], 2);
        });
        time("iir2", &|g| {
            let (x, amp, z, y) = (g.buf(2 * n), g.buf(n), g.buf(8 * n), g.buf(2 * n));
            let (ns, k) = sos_params(&Band { lo: 3.0, hi: 5.0, alpha: 1.0 }, 120.0);
            g.op(K::Iir2, Params { sw: ns, k, ..p }, &[x, amp, z, y], 1);
        });
        time("out", &|g| {
            let (re, rr, den, cs, o) = (g.buf(n), g.buf(2 * n), g.buf(n), g.buf(2 * n), g.buf(n));
            g.op(K::Out, Params { a: 30.0, ..p }, &[re, rr, den, cs, o], 1);
        });
        time("step", &|g| {
            let (re, rr, prev, ph, amp, pcs) = (g.buf(n), g.buf(2 * n), g.buf(3 * n), g.buf(2 * n), g.buf(n), g.buf(2 * n));
            g.op(K::Step, Params { a: 0.99, ..p }, &[re, rr, prev, ph, amp, pcs], 1);
        });
        time("riesz", &|g| {
            let (re, rr) = (g.buf(n), g.buf(2 * n));
            g.op(K::Riesz, p, &[re, rr], 1);
        });
    }

    /// Frames per second at 1072×1074 (`--ignored`).
    #[test]
    #[ignore]
    fn gpu_magnify_speed() {
        let Some(g) = gpu() else { return };
        let (w, h) = (1072, 1074);
        let tremor: Vec<Band> = [(3.0, 5.0), (5.0, 8.0), (8.0, 12.0)].iter().map(|&(lo, hi)| Band { lo, hi, alpha: 30.0 }).collect();
        let pulse = [Band { lo: 0.8, hi: 1.5, alpha: 30.0 }, Band { lo: 0.15, hi: 0.5, alpha: 20.0 }];
        let f = frame(w, h, 0);
        let run = |name: &str, f: &Plane, m: &mut dyn FnMut(&Plane) -> Vec<Plane>| {
            m(f);
            let t = std::time::Instant::now();
            for _ in 0..60 {
                m(f);
            }
            eprintln!("{name}: {:.0} kép/s", 60.0 / t.elapsed().as_secs_f64());
        };
        let mut p = Phase::new(g.clone(), &pulse, 120.0, w, h);
        run("phase (2 sáv)", &f, &mut |f| p.process(f));
        let mut t3 = Phase::new(g.clone(), &tremor, 120.0, w, h);
        run("tremor (3 sáv)", &f, &mut |f| t3.process(f));
        let mut l = Linear::new(g, &pulse, 120.0, w, h);
        run("linear (2 sáv)", &f, &mut |f| l.process(f));
    }
}
