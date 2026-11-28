//! GPU picture: raw camera samples to the screen in one shader pass.
//!
//! The samples are uploaded as they are (8 or 16 bit, Bayer mosaic or grey,
//! or planar RGB). The fragment shader demosaics (bilinear at full size, a
//! box per colour when shrunk), then looks each channel up in a 1D table
//! and, with a colour LUT, the result in a 3D cube with hardware trilinear
//! interpolation:
//!
//! - **1D table** (per channel, 4096 entries, linear input): white balance,
//!   levels, and either the display tone (no LUT) or a shaper (`sqrt`) that
//!   spreads the cube's grid evenly over the brightness range;
//! - **3D cube** (33³, half float): the colour LUT (channel mixing), then
//!   brightness and contrast.
//!
//! Every slider only rebuilds these tables; the pixels are never touched on
//! the CPU. Nothing is rounded to 8 bit before the screen.

use crate::frame::Frame;
use crate::lut::Lut;
use crate::scenario::View;
use crate::stack;
use eframe::egui;
use eframe::egui_wgpu::{self, wgpu};
use std::borrow::Cow;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Instant;

/// Stream tag value (`LOOK` tag) of a corrected recording.
pub const LOOK_TAG: &str = "corrected p010 bt709 full";

/// Entries of the 1D table.
pub const N1: usize = 4096;
/// Grid points per side of the 3D cube.
pub const CUBE: usize = 33;
/// Bins of the brightness histogram (auto-levels).
pub const HIST: usize = 1024;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Kind {
    Gray = 0,
    /// RGGB mosaic.
    Bayer = 1,
    /// Three planes (R, G, B) stacked: the texture is `height * 3` rows.
    Rgb = 2,
}

/// Samples ready for upload: 1 or 2 bytes (little endian), white at `max`.
/// Camera frames are borrowed (no copy), converted pictures owned.
pub struct Source<'a> {
    pub width: u32,
    pub height: u32,
    pub kind: Kind,
    pub max: f32,
    pub wide: bool,
    pub data: Cow<'a, [u8]>,
}

impl<'a> Source<'a> {
    /// A camera or recording frame. Grey and Bayer go up as they are;
    /// YUV and MJPEG are converted to RGB first.
    pub fn from_frame(f: &'a Frame) -> Option<Source<'a>> {
        let fmt = f.format;
        if fmt.is_single_channel() {
            let wide = fmt.bytes_per_pixel() == 2;
            let need = f.width as usize * f.height as usize * fmt.bytes_per_pixel();
            if f.data.len() < need || f.width < 2 || f.height < 2 {
                return None;
            }
            let kind = if fmt.is_bayer() { Kind::Bayer } else { Kind::Gray };
            let max = ((1u32 << fmt.bits()) - 1) as f32;
            return Some(Source { width: f.width, height: f.height, kind, max, wide, data: Cow::Borrowed(&f.data[..need]) });
        }
        let img = stack::to_image(f, None)?;
        // YUV and MJPEG are 8 bit.
        Some(Source::from_image(&img, true))
    }

    /// A linear image (0..1): averaged, dark-subtracted or converted.
    /// `narrow`: 8 bit is enough (8-bit sources).
    pub fn from_image(img: &stack::Image, narrow: bool) -> Source<'static> {
        let (w, h, ch) = (img.width, img.height, img.channels);
        let kind = if ch == 1 { Kind::Gray } else { Kind::Rgb };
        let max = if narrow { 255.0 } else { 65535.0 };
        let mut data = Vec::with_capacity(w * h * ch * if narrow { 1 } else { 2 });
        // Planar: all R, then all G, then all B.
        for c in 0..ch {
            for p in img.data.iter().skip(c).step_by(ch) {
                let v = (p.clamp(0.0, 1.0) * max + 0.5) as u16;
                if narrow {
                    data.push(v as u8);
                } else {
                    data.extend(v.to_le_bytes());
                }
            }
        }
        Source { width: w as u32, height: h as u32, kind, max, wide: !narrow, data: Cow::Owned(data) }
    }

    /// 8-bit RGB pixels (decoded video).
    pub fn from_rgb8(width: usize, height: usize, rgb: &[u8]) -> Source<'static> {
        let mut data = Vec::with_capacity(width * height * 3);
        for c in 0..3 {
            data.extend(rgb.iter().skip(c).step_by(3).take(width * height));
        }
        Source { width: width as u32, height: height as u32, kind: Kind::Rgb, max: 255.0, wide: false, data: Cow::Owned(data) }
    }

    fn sample(&self, i: usize) -> f32 {
        let v = if self.wide { u16::from_le_bytes([self.data[2 * i], self.data[2 * i + 1]]) } else { self.data[i] as u16 };
        v as f32 / self.max
    }

    /// Linear R, G, B (0..1) at `(x, y)`; Bayer: the 2×2 cell there.
    pub fn rgb(&self, x: usize, y: usize) -> [f32; 3] {
        let (w, h) = (self.width as usize, self.height as usize);
        match self.kind {
            Kind::Gray => [self.sample(y * w + x); 3],
            Kind::Bayer => {
                let (x, y) = (x & !1, y & !1);
                let g = (self.sample(y * w + x + 1) + self.sample((y + 1) * w + x)) / 2.0;
                [self.sample(y * w + x), g, self.sample((y + 1) * w + x + 1)]
            }
            Kind::Rgb => std::array::from_fn(|c| self.sample(c * w * h + y * w + x)),
        }
    }

    /// From about 64k pixels spread over the picture: mean R, G, B of the
    /// unsaturated pixels (colour only, for the grey-world white balance)
    /// and a histogram of the brightness after white balance `wb`.
    pub fn stats(&self, wb: Option<[f32; 3]>) -> (Option<[f64; 3]>, Vec<u32>) {
        let (w, h) = (self.width as usize, self.height as usize);
        let step = ((w * h / 65536) as f64).sqrt().max(1.0) as usize;
        let step = if self.kind == Kind::Bayer { step.next_multiple_of(2) } else { step };
        let (mut sum, mut n) = ([0f64; 3], 0u64);
        let mut hist = vec![0u32; HIST];
        let g = wb.unwrap_or([1.0; 3]);
        for y in (0..h.saturating_sub(1)).step_by(step) {
            for x in (0..w.saturating_sub(1)).step_by(step) {
                let p = self.rgb(x, y);
                if self.kind != Kind::Gray && p.iter().all(|&c| c < 0.98) {
                    (0..3).for_each(|c| sum[c] += p[c] as f64);
                    n += 1;
                }
                let p: [f32; 3] = std::array::from_fn(|c| (p[c] * g[c]).min(1.0));
                let l = (p[0] + 2.0 * p[1] + p[2]) / 4.0;
                hist[((l * (HIST - 1) as f32) as usize).min(HIST - 1)] += 1;
            }
        }
        ((n > 0).then(|| sum.map(|c| c / n as f64)), hist)
    }
}

/// Black and white point (0..1) at 0.5 % and 99.5 % of a histogram.
pub fn levels_of(hist: &[u32]) -> Option<(f32, f32)> {
    let total: u64 = hist.iter().map(|&h| h as u64).sum();
    if total == 0 {
        return None;
    }
    let pct = |p: f64| {
        let mut acc = 0u64;
        hist.iter().position(|&h| {
            acc += h as u64;
            acc as f64 >= p * total as f64
        })
        .unwrap_or(hist.len() - 1) as f32
            / (hist.len() - 1) as f32
    };
    let lo = pct(0.005);
    Some((lo, pct(0.995).max(lo + 0.05)))
}

/// Grey-world gains (R, G, B) from mean R, G, B.
pub fn wb_of([r, g, b]: [f64; 3]) -> [f32; 3] {
    [g / r.max(1e-4), 1.0, g / b.max(1e-4)].map(|x| x.clamp(0.25, 4.0) as f32)
}

/// The tone curve on a levelled value: gamma, shadows and highlights,
/// contrast around mid grey, brightness.
fn tone(v: &View, x: f32) -> f32 {
    let mut x = x.clamp(0.0, 1.0).powf(1.0 / v.gamma.max(0.1));
    // Bumps that vanish at 0 and 1 (peaks at 1/4 and 3/4).
    x += v.shadows * 1.6 * x * (1.0 - x).powi(3);
    x += v.highlights * 1.6 * x.powi(3) * (1.0 - x);
    ((x - 0.5) * v.contrast + 0.5 + v.brightness).clamp(0.0, 1.0)
}

/// Saturation and vibrance around the luma (BT.709).
fn saturate(v: &View, c: [f32; 3]) -> [f32; 3] {
    if v.saturation == 1.0 && v.vibrance == 0.0 {
        return c;
    }
    let y = 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
    let s = c[0].max(c[1]).max(c[2]) - c[0].min(c[1]).min(c[2]);
    let k = (v.saturation * (1.0 + v.vibrance * (1.0 - s))).max(0.0);
    c.map(|x| (y + (x - y) * k).clamp(0.0, 1.0))
}

/// The cube is needed for a colour LUT or for mixing the channels.
pub fn needs_cube(v: &View, lut: bool) -> bool {
    lut || v.saturation != 1.0 || v.vibrance != 0.0
}

/// Gains per channel: white balance, temperature and tint.
fn gains(v: &View, wb: Option<[f32; 3]>) -> [f32; 3] {
    let g = wb.unwrap_or([1.0; 3]);
    let warm = (0.35 * v.temperature).exp();
    [g[0] * warm, g[1] * (-0.25 * v.tint).exp(), g[2] / warm]
}

/// The 1D table, R then G then B, `N1` entries each over the linear input
/// 0..1: white balance, levels, exposure compensation, then the tone (or,
/// with the cube, the shaper it is indexed with).
pub fn table(view: &View, levels: Option<(f32, f32)>, wb: Option<[f32; 3]>, cube: bool) -> Vec<f32> {
    let g = gains(view, wb);
    let levels = levels.or((view.black != 0.0 || view.white != 1.0).then_some((view.black, view.white.max(view.black + 0.01))));
    let ev = 2f32.powf(view.exposure_ev);
    let mut t = Vec::with_capacity(3 * N1);
    for gain in g {
        for i in 0..N1 {
            let mut x = (i as f32 / (N1 - 1) as f32 * gain).min(1.0);
            if let Some((lo, hi)) = levels {
                x = ((x - lo) / (hi - lo)).clamp(0.0, 1.0);
            }
            x = (x * ev).min(1.0);
            t.push(if cube { x.sqrt() } else { tone(view, x) });
        }
    }
    t
}

/// The 3D cube over the shaper's grid (R fastest, then G, then B), RGBA
/// half floats: the colour LUT (if any), the tone, saturation and vibrance.
pub fn cube(view: &View, lut: Option<&Lut>) -> Vec<u16> {
    let n = CUBE;
    let mut out = Vec::with_capacity(n * n * n * 4);
    let lin = |i: usize| (i as f32 / (n - 1) as f32).powi(2);
    for b in 0..n {
        for g in 0..n {
            for r in 0..n {
                let x = [lin(r), lin(g), lin(b)];
                let y = saturate(view, lut.map_or(x, |l| l.apply(x)).map(|c| tone(view, c)));
                out.extend(y.map(|c| half::f16::from_f32(c).to_bits()));
                out.push(half::f16::ONE.to_bits());
            }
        }
    }
    out
}

/// What the picture looks like, shared by the live view and the recording:
/// the scenario's view settings, the colour LUT, and the automatic white
/// balance and levels. The automatics follow the picture at most at
/// `View::auto_rate` Hz (a first-order lag), so a recording of the
/// corrected picture changes only slowly.
pub struct Look {
    view: View,
    lut: Option<(String, Arc<Lut>)>,
    wb: [f32; 3],
    levels: (f32, f32),
    last: Option<Instant>,
    table: Arc<Vec<f32>>,
    cube: Option<Arc<Vec<u16>>>,
    cube_key: Option<(Option<String>, [f32; 7])>,
}

impl Default for Look {
    fn default() -> Self {
        let view = View::default();
        let table = Arc::new(table(&view, None, None, false));
        Look { view, lut: None, wb: [1.0; 3], levels: (0.0, 1.0), last: None, table, cube: None, cube_key: None }
    }
}

impl Look {
    /// View settings and colour LUT (name, LUT) of the running scenario.
    pub fn set(&mut self, view: &View, lut: Option<(String, Arc<Lut>)>) {
        let v = view;
        let key = needs_cube(v, lut.is_some()).then(|| {
            (lut.as_ref().map(|l| l.0.clone()), [v.brightness, v.contrast, v.gamma, v.shadows, v.highlights, v.saturation, v.vibrance])
        });
        if key != self.cube_key {
            self.cube = key.as_ref().map(|_| Arc::new(cube(v, lut.as_ref().map(|l| &*l.1))));
            self.cube_key = key;
        }
        let changed = *view != self.view || lut.as_ref().map(|l| &l.0) != self.lut.as_ref().map(|l| &l.0);
        self.lut = lut;
        if changed {
            self.view = view.clone();
            self.rebuild();
        }
    }

    /// Follow the picture: grey-world white balance and auto-levels.
    pub fn observe(&mut self, src: &Source) {
        let now = Instant::now();
        // First picture: at once. Then a first-order lag at auto_rate Hz.
        let a = match self.last {
            None => 1.0,
            Some(t) => 1.0 - (-std::f32::consts::TAU * self.view.auto_rate.max(0.001) * now.duration_since(t).as_secs_f32().min(1.0)).exp(),
        };
        self.last = Some(now);
        let wb = self.view.auto_white_balance.then_some(self.wb);
        let (means, hist) = src.stats(wb);
        if let Some(m) = means {
            let target = wb_of(m);
            for c in 0..3 {
                self.wb[c] += a * (target[c] - self.wb[c]);
            }
        }
        if let Some((lo, hi)) = levels_of(&hist) {
            self.levels = (self.levels.0 + a * (lo - self.levels.0), self.levels.1 + a * (hi - self.levels.1));
        }
        self.rebuild();
    }

    /// Start the automatics anew (another camera or scenario).
    pub fn reset(&mut self) {
        self.last = None;
    }

    fn rebuild(&mut self) {
        let v = &self.view;
        let wb = v.auto_white_balance.then_some(self.wb);
        let levels = v.auto_levels.then_some(self.levels);
        self.table = Arc::new(table(v, levels, wb, self.cube.is_some()));
    }

    /// The 1D table and the cube (`None`: no colour LUT).
    pub fn tables(&self) -> (Arc<Vec<f32>>, Option<Arc<Vec<u16>>>) {
        (self.table.clone(), self.cube.clone())
    }
}

const SHADER: &str = r#"
struct U { size: vec2<u32>, kind: u32, maxv: f32, step: u32, cube_on: u32, srgb: u32, n1: u32 };
@group(0) @binding(0) var<uniform> u: U;
@group(0) @binding(1) var raw: texture_2d<u32>;
@group(0) @binding(2) var table: texture_2d<f32>;
@group(0) @binding(3) var cube: texture_3d<f32>;
@group(0) @binding(4) var samp: sampler;

struct VOut { @builtin(position) pos: vec4<f32>, @location(0) uv: vec2<f32> };

@vertex fn vs(@builtin(vertex_index) i: u32) -> VOut {
    let uv = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    var o: VOut;
    o.pos = vec4<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, 0.0, 1.0);
    o.uv = uv;
    return o;
}

// Inside the picture; a Bayer site keeps its colour (steps of 2).
fn fold(p: vec2<i32>) -> vec2<i32> {
    let s = vec2<i32>(u.size);
    var q = p;
    if (q.x < 0) { q.x = q.x + 2; }
    if (q.y < 0) { q.y = q.y + 2; }
    if (q.x >= s.x) { q.x = q.x - 2; }
    if (q.y >= s.y) { q.y = q.y - 2; }
    return clamp(q, vec2<i32>(0), s - 1);
}

fn at(p: vec2<i32>) -> f32 { return f32(textureLoad(raw, p, 0).r); }

// RGGB: 0 red, 1 green, 2 blue.
fn colour(p: vec2<i32>) -> u32 { return u32(p.x & 1) + u32(p.y & 1); }

fn bayer(p: vec2<i32>) -> vec3<f32> {
    var acc = vec3<f32>(0.0);
    var wsum = vec3<f32>(0.0);
    let n = i32(u.step);
    if (n <= 1) {
        // Bilinear: the site itself (4), orthogonal (2), diagonal (1).
        for (var dy = -1; dy <= 1; dy++) {
            for (var dx = -1; dx <= 1; dx++) {
                let q = fold(p + vec2<i32>(dx, dy));
                let w = f32(4 >> u32(abs(dx) + abs(dy)));
                let c = colour(q);
                acc[c] += w * at(q);
                wsum[c] += w;
            }
        }
    } else {
        // Shrunk: every site of an n×n box (n even).
        let o = p & vec2<i32>(-2);
        for (var dy = 0; dy < n; dy++) {
            for (var dx = 0; dx < n; dx++) {
                let q = fold(o + vec2<i32>(dx, dy));
                let c = colour(q);
                acc[c] += at(q);
                wsum[c] += 1.0;
            }
        }
    }
    return acc / max(wsum, vec3<f32>(1.0));
}

fn plain(p: vec2<i32>) -> vec3<f32> {
    let n = i32(u.step);
    let s = vec2<i32>(u.size);
    var acc = vec3<f32>(0.0);
    for (var dy = 0; dy < n; dy++) {
        for (var dx = 0; dx < n; dx++) {
            let q = clamp(p + vec2<i32>(dx, dy), vec2<i32>(0), s - 1);
            if (u.kind == 0u) {
                acc += vec3<f32>(at(q));
            } else {
                acc += vec3<f32>(at(q), at(q + vec2<i32>(0, s.y)), at(q + vec2<i32>(0, 2 * s.y)));
            }
        }
    }
    return acc / f32(n * n);
}

fn lookup(c: u32, v: f32) -> f32 {
    let f = clamp(v, 0.0, 1.0) * f32(u.n1 - 1u);
    let i = u32(f);
    let j = min(i + 1u, u.n1 - 1u);
    let a = textureLoad(table, vec2<u32>(i, c), 0).r;
    let b = textureLoad(table, vec2<u32>(j, c), 0).r;
    return mix(a, b, f - f32(i));
}

fn to_linear(c: vec3<f32>) -> vec3<f32> {
    let lo = c / 12.92;
    let hi = pow((c + 0.055) / 1.055, vec3<f32>(2.4));
    return select(hi, lo, c <= vec3<f32>(0.04045));
}

// The corrected colour at picture pixel p (display values 0..1).
fn graded(p: vec2<i32>) -> vec3<f32> {
    var x: vec3<f32>;
    if (u.kind == 1u) { x = bayer(p); } else { x = plain(p); }
    x = x / u.maxv;
    var y = vec3<f32>(lookup(0u, x.r), lookup(1u, x.g), lookup(2u, x.b));
    if (u.cube_on == 1u) {
        let n = f32(textureDimensions(cube).x);
        y = textureSampleLevel(cube, samp, y * (n - 1.0) / n + 0.5 / n, 0.0).rgb;
    }
    return y;
}

@fragment fn fs(v: VOut) -> @location(0) vec4<f32> {
    let p = vec2<i32>(floor(clamp(v.uv, vec2<f32>(0.0), vec2<f32>(0.99999)) * vec2<f32>(u.size)));
    var y = graded(p);
    if (u.srgb == 1u) { y = to_linear(y); }
    return vec4<f32>(y, 1.0);
}

// Recording: BT.709 full range, 10 bit in the high bits of 16 (P010).
fn luma(c: vec3<f32>) -> f32 { return dot(c, vec3<f32>(0.2126, 0.7152, 0.0722)); }
fn p010(v: f32) -> u32 { return u32(round(clamp(v, 0.0, 1.0) * 1023.0)) << 6u; }

@fragment fn fs_y(v: VOut) -> @location(0) vec4<u32> {
    return vec4<u32>(p010(luma(graded(vec2<i32>(v.pos.xy)))), 0u, 0u, 1u);
}

// Chroma of a 2×2 block (4:2:0).
@fragment fn fs_uv(v: VOut) -> @location(0) vec4<u32> {
    let o = vec2<i32>(v.pos.xy) * 2;
    let c = (graded(o) + graded(o + vec2<i32>(1, 0)) + graded(o + vec2<i32>(0, 1)) + graded(o + vec2<i32>(1, 1))) / 4.0;
    let y = luma(c);
    return vec4<u32>(p010((c.b - y) / 1.8556 + 0.5), p010((c.r - y) / 1.5748 + 0.5), 0u, 1u);
}
"#;

/// Pipeline shared by all pictures.
pub struct Gpu {
    device: wgpu::Device,
    queue: wgpu::Queue,
    layout: wgpu::BindGroupLayout,
    pipeline: wgpu::RenderPipeline,
    /// Recording: luma (R16Uint) and chroma (Rg16Uint) targets.
    rec_y: wgpu::RenderPipeline,
    rec_uv: wgpu::RenderPipeline,
    sampler: wgpu::Sampler,
    srgb: bool,
}

impl Gpu {
    pub fn new(rs: &egui_wgpu::RenderState) -> Arc<Gpu> {
        Gpu::build(rs.device.clone(), rs.queue.clone(), rs.target_format)
    }

    fn build(device: wgpu::Device, queue: wgpu::Queue, target: wgpu::TextureFormat) -> Arc<Gpu> {
        let entry = |binding, ty| wgpu::BindGroupLayoutEntry { binding, visibility: wgpu::ShaderStages::FRAGMENT, ty, count: None };
        let tex = |sample_type, view_dimension| wgpu::BindingType::Texture { sample_type, view_dimension, multisampled: false };
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("view"),
            entries: &[
                entry(0, wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None }),
                entry(1, tex(wgpu::TextureSampleType::Uint, wgpu::TextureViewDimension::D2)),
                entry(2, tex(wgpu::TextureSampleType::Float { filterable: false }, wgpu::TextureViewDimension::D2)),
                entry(3, tex(wgpu::TextureSampleType::Float { filterable: true }, wgpu::TextureViewDimension::D3)),
                entry(4, wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering)),
            ],
        });
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some("view"), source: wgpu::ShaderSource::Wgsl(SHADER.into()) });
        let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor { label: Some("view"), bind_group_layouts: &[Some(&layout)], ..Default::default() });
        let pipe = |entry: &str, format| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(entry),
                layout: Some(&pl),
                vertex: wgpu::VertexState { module: &module, entry_point: Some("vs"), compilation_options: Default::default(), buffers: &[] },
                fragment: Some(wgpu::FragmentState {
                    module: &module,
                    entry_point: Some(entry),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState { format, blend: None, write_mask: wgpu::ColorWrites::ALL })],
                }),
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                cache: None,
            })
        };
        let pipeline = pipe("fs", target);
        let rec_y = pipe("fs_y", wgpu::TextureFormat::R16Uint);
        let rec_uv = pipe("fs_uv", wgpu::TextureFormat::Rg16Uint);
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("cube"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        Arc::new(Gpu { device, queue, layout, pipeline, rec_y, rec_uv, sampler, srgb: target.is_srgb() })
    }

    /// A GPU of its own, without a window (recording, command line).
    /// Made once per process.
    pub fn headless() -> Result<Arc<Gpu>, String> {
        static GPU: std::sync::OnceLock<Result<Arc<Gpu>, String>> = std::sync::OnceLock::new();
        GPU.get_or_init(|| {
            let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
            let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                ..Default::default()
            }))
            .map_err(|e| format!("nincs GPU: {e}"))?;
            let (device, queue) =
                pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default())).map_err(|e| format!("GPU: {e}"))?;
            Ok(Gpu::build(device, queue, wgpu::TextureFormat::Rgba8Unorm))
        })
        .clone()
    }

    pub fn device(&self) -> &wgpu::Device {
        &self.device
    }

    pub fn queue(&self) -> &wgpu::Queue {
        &self.queue
    }

    fn texture(&self, label: &str, size: wgpu::Extent3d, dimension: wgpu::TextureDimension, format: wgpu::TextureFormat) -> wgpu::Texture {
        self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension,
            format,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        })
    }

    fn write(&self, t: &wgpu::Texture, data: &[u8], bytes_per_row: u32) {
        let size = t.size();
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo { texture: t, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            data,
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(bytes_per_row), rows_per_image: Some(size.height) },
            size,
        );
    }
}

/// One picture on the GPU (the live view, the player).
pub struct Picture {
    gpu: Arc<Gpu>,
    raw: wgpu::Texture,
    table: wgpu::Texture,
    cube: wgpu::Texture,
    uniform: wgpu::Buffer,
    bind: wgpu::BindGroup,
    size: (u32, u32),
    kind: Kind,
    max: f32,
    /// The tables on the GPU (uploaded again only when they change).
    table_now: Option<Arc<Vec<f32>>>,
    cube_now: Option<Arc<Vec<u16>>>,
}

fn extent(width: u32, height: u32, depth: u32) -> wgpu::Extent3d {
    wgpu::Extent3d { width, height, depth_or_array_layers: depth }
}

impl Picture {
    pub fn new(gpu: Arc<Gpu>, src: &Source) -> Picture {
        let raw = Picture::raw_texture(&gpu, src);
        let table = gpu.texture("table", extent(N1 as u32, 3, 1), wgpu::TextureDimension::D2, wgpu::TextureFormat::R32Float);
        let cube = gpu.texture("cube", extent(CUBE as u32, CUBE as u32, CUBE as u32), wgpu::TextureDimension::D3, wgpu::TextureFormat::Rgba16Float);
        let uniform = gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("view"),
            size: 32,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let bind = Picture::bind(&gpu, &uniform, &raw, &table, &cube);
        let mut p = Picture { gpu, raw, table, cube, uniform, bind, size: (0, 0), kind: src.kind, max: src.max, table_now: None, cube_now: None };
        p.upload(src);
        p
    }

    fn raw_texture(gpu: &Gpu, src: &Source) -> wgpu::Texture {
        let rows = if src.kind == Kind::Rgb { src.height * 3 } else { src.height };
        let format = if src.wide { wgpu::TextureFormat::R16Uint } else { wgpu::TextureFormat::R8Uint };
        gpu.texture("raw", extent(src.width, rows, 1), wgpu::TextureDimension::D2, format)
    }

    fn bind(gpu: &Gpu, uniform: &wgpu::Buffer, raw: &wgpu::Texture, table: &wgpu::Texture, cube: &wgpu::Texture) -> wgpu::BindGroup {
        let view = |t: &wgpu::Texture| t.create_view(&wgpu::TextureViewDescriptor::default());
        gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("view"),
            layout: &gpu.layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: uniform.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&view(raw)) },
                wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(&view(table)) },
                wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::TextureView(&view(cube)) },
                wgpu::BindGroupEntry { binding: 4, resource: wgpu::BindingResource::Sampler(&gpu.sampler) },
            ],
        })
    }

    /// New samples (the texture is made anew if the size or kind changed).
    pub fn upload(&mut self, src: &Source) {
        let rows = if src.kind == Kind::Rgb { src.height * 3 } else { src.height };
        let format = if src.wide { wgpu::TextureFormat::R16Uint } else { wgpu::TextureFormat::R8Uint };
        if self.raw.size() != extent(src.width, rows, 1) || self.raw.format() != format {
            self.raw = Picture::raw_texture(&self.gpu, src);
            self.bind = Picture::bind(&self.gpu, &self.uniform, &self.raw, &self.table, &self.cube);
        }
        let bpp = if src.wide { 2 } else { 1 };
        let need = (src.width * rows * bpp) as usize;
        if src.data.len() >= need {
            self.gpu.write(&self.raw, &src.data[..need], src.width * bpp);
        }
        (self.size, self.kind, self.max) = ((src.width, src.height), src.kind, src.max);
    }

    /// The 1D table (see [`table`]) and the cube (`None`: no colour LUT).
    pub fn set_look(&mut self, table: &Arc<Vec<f32>>, cube: Option<&Arc<Vec<u16>>>) {
        if self.table_now.as_ref().is_none_or(|t| !Arc::ptr_eq(t, table)) {
            let bytes: Vec<u8> = table.iter().flat_map(|v| v.to_le_bytes()).collect();
            self.gpu.write(&self.table, &bytes, N1 as u32 * 4);
            self.table_now = Some(table.clone());
        }
        if let Some(c) = cube
            && self.cube_now.as_ref().is_none_or(|n| !Arc::ptr_eq(n, c))
        {
            let bytes: Vec<u8> = c.iter().flat_map(|v| v.to_le_bytes()).collect();
            self.gpu.write(&self.cube, &bytes, CUBE as u32 * 8);
        }
        self.cube_now = cube.cloned();
    }

    pub fn size(&self) -> egui::Vec2 {
        egui::vec2(self.size.0 as f32, self.size.1 as f32)
    }

    /// Draw the picture into `rect` (screen points).
    pub fn paint(&self, painter: &egui::Painter, rect: egui::Rect) {
        let ppp = painter.ctx().pixels_per_point();
        // Picture pixels per screen pixel: averaged over a box when shrunk.
        let shrink = (self.size.0 as f32 / (rect.width() * ppp).max(1.0)).max(self.size.1 as f32 / (rect.height() * ppp).max(1.0));
        let mut step = (shrink.round() as u32).clamp(1, 8);
        if self.kind == Kind::Bayer && step > 1 {
            step = step.next_multiple_of(2);
        }
        painter.add(egui_wgpu::Callback::new_paint_callback(rect, self.call(step)));
    }

    fn call(&self, step: u32) -> Paint {
        let data = [
            self.size.0,
            self.size.1,
            self.kind as u32,
            self.max.to_bits(),
            step,
            self.cube_now.is_some() as u32,
            self.gpu.srgb as u32,
            N1 as u32,
        ];
        Paint { gpu: self.gpu.clone(), bind: self.bind.clone(), uniform: self.uniform.clone(), data }
    }
}

/// Frames in flight on the GPU while recording.
const SLOTS: usize = 4;

/// Recording: every frame through the same shader as the live view, out as
/// P010 (10-bit 4:2:0, BT.709 full range) for the encoder. Frames are
/// pipelined: up to `SLOTS` on the GPU, read back in order.
pub struct Converter {
    pic: Picture,
    y: wgpu::Texture,
    uv: wgpu::Texture,
    /// Readback buffers and the submission each is waiting for.
    slots: Vec<wgpu::Buffer>,
    pending: VecDeque<(usize, wgpu::SubmissionIndex)>,
    next: usize,
    /// One frame of P010, rows without padding.
    out: Vec<u8>,
}

impl Converter {
    /// Size and kind from the first frame; `None` if it cannot be shown.
    pub fn new(gpu: Arc<Gpu>, f: &Frame) -> Option<Converter> {
        let src = Source::from_frame(f)?;
        let (w, h) = (src.width, src.height);
        let pic = Picture::new(gpu.clone(), &src);
        let target = |label, w, h, format| {
            gpu.device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: extent(w, h, 1),
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            })
        };
        let y = target("y", w, h, wgpu::TextureFormat::R16Uint);
        let uv = target("uv", w.div_ceil(2), h.div_ceil(2), wgpu::TextureFormat::Rg16Uint);
        let (yr, uvr) = Converter::rows(w);
        let size = (yr * h + uvr * h.div_ceil(2)) as u64;
        let slots = (0..SLOTS)
            .map(|_| {
                gpu.device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("readback"),
                    size,
                    usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                    mapped_at_creation: false,
                })
            })
            .collect();
        Some(Converter { pic, y, uv, slots, pending: VecDeque::new(), next: 0, out: Vec::new() })
    }

    /// Bytes per row of luma and chroma in the readback buffer (aligned).
    fn rows(w: u32) -> (u32, u32) {
        let a = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        ((w * 2).next_multiple_of(a), (w.div_ceil(2) * 4).next_multiple_of(a))
    }

    pub fn size(&self) -> (u32, u32) {
        self.pic.size
    }

    /// All slots busy: `take` the oldest before the next `submit`.
    pub fn full(&self) -> bool {
        self.pending.len() >= SLOTS
    }

    /// Correct `f` with the given tables on the GPU (no waiting).
    pub fn submit(&mut self, f: &Frame, table: &Arc<Vec<f32>>, cube: Option<&Arc<Vec<u16>>>) -> Result<(), String> {
        let src = Source::from_frame(f).ok_or("a képkocka nem alakítható")?;
        if (src.width, src.height) != self.pic.size {
            return Err(format!("a képméret felvétel közben változott: {}×{}", src.width, src.height));
        }
        self.pic.upload(&src);
        self.pic.set_look(table, cube);
        let g = self.pic.gpu.clone();
        let call = self.pic.call(1);
        let bytes: Vec<u8> = call.data.iter().flat_map(|v| v.to_le_bytes()).collect();
        g.queue.write_buffer(&self.pic.uniform, 0, &bytes);
        let mut enc = g.device.create_command_encoder(&Default::default());
        for (target, pipeline) in [(&self.y, &g.rec_y), (&self.uv, &g.rec_uv)] {
            let view = target.create_view(&Default::default());
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
                })],
                ..Default::default()
            });
            pass.set_pipeline(pipeline);
            pass.set_bind_group(0, &self.pic.bind, &[]);
            pass.draw(0..3, 0..1);
        }
        let (w, h) = self.pic.size;
        let (yr, uvr) = Converter::rows(w);
        let buf = &self.slots[self.next];
        let copy = |enc: &mut wgpu::CommandEncoder, t: &wgpu::Texture, offset: u64, row: u32| {
            enc.copy_texture_to_buffer(
                wgpu::TexelCopyTextureInfo { texture: t, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
                wgpu::TexelCopyBufferInfo { buffer: buf, layout: wgpu::TexelCopyBufferLayout { offset, bytes_per_row: Some(row), rows_per_image: None } },
                t.size(),
            );
        };
        copy(&mut enc, &self.y, 0, yr);
        copy(&mut enc, &self.uv, (yr * h) as u64, uvr);
        let index = g.queue.submit([enc.finish()]);
        buf.slice(..).map_async(wgpu::MapMode::Read, |_| {});
        self.pending.push_back((self.next, index));
        self.next = (self.next + 1) % SLOTS;
        Ok(())
    }

    /// The oldest corrected frame as P010 (waits for the GPU); `None` if
    /// nothing is in flight.
    pub fn take(&mut self) -> Option<Result<&[u8], String>> {
        let (slot, index) = self.pending.pop_front()?;
        let g = &self.pic.gpu;
        if let Err(e) = g.device.poll(wgpu::PollType::Wait { submission_index: Some(index), timeout: None }) {
            return Some(Err(format!("GPU: {e}")));
        }
        let (w, h) = self.pic.size;
        let (yr, uvr) = Converter::rows(w);
        let buf = &self.slots[slot];
        self.out.clear();
        {
            let data = match buf.slice(..).get_mapped_range() {
                Ok(d) => d,
                Err(e) => return Some(Err(format!("GPU: {e:?}"))),
            };
            for r in 0..h as usize {
                self.out.extend_from_slice(&data[r * yr as usize..][..w as usize * 2]);
            }
            let base = (yr * h) as usize;
            for r in 0..h.div_ceil(2) as usize {
                self.out.extend_from_slice(&data[base + r * uvr as usize..][..w.div_ceil(2) as usize * 4]);
            }
        }
        buf.unmap();
        Some(Ok(&self.out))
    }
}

struct Paint {
    gpu: Arc<Gpu>,
    bind: wgpu::BindGroup,
    uniform: wgpu::Buffer,
    data: [u32; 8],
}

impl egui_wgpu::CallbackTrait for Paint {
    fn prepare(
        &self,
        _device: &wgpu::Device,
        queue: &wgpu::Queue,
        _screen: &egui_wgpu::ScreenDescriptor,
        _encoder: &mut wgpu::CommandEncoder,
        _resources: &mut egui_wgpu::CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        let bytes: Vec<u8> = self.data.iter().flat_map(|v| v.to_le_bytes()).collect();
        queue.write_buffer(&self.uniform, 0, &bytes);
        Vec::new()
    }

    fn paint(&self, _info: egui::PaintCallbackInfo, pass: &mut wgpu::RenderPass<'static>, _resources: &egui_wgpu::CallbackResources) {
        pass.set_pipeline(&self.gpu.pipeline);
        pass.set_bind_group(0, &self.bind, &[]);
        pass.draw(0..3, 0..1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_without_lut_is_the_old_tone() {
        let v = View::default();
        let t = table(&v, None, None, false);
        assert_eq!(t.len(), 3 * N1);
        assert!((t[N1 - 1] - 1.0).abs() < 1e-6 && t[0].abs() < 1e-6);
        // Levels stretch the linear range: 0.25 → 0, 0.75 → 1.
        let t = table(&v, Some((0.25, 0.75)), None, false);
        let at = |x: f32| t[(x * (N1 - 1) as f32).round() as usize];
        assert!(at(0.25) < 0.01 && at(0.75) > 0.99 && (at(0.5) - 0.5).abs() < 0.01);
    }

    /// Draw `p` at `w`×`h` on a headless GPU and read the pixels back.
    fn render(p: &Picture, w: u32, h: u32, step: u32) -> Vec<[u8; 4]> {
        let g = &p.gpu;
        let call = p.call(step);
        let bytes: Vec<u8> = call.data.iter().flat_map(|v| v.to_le_bytes()).collect();
        g.queue.write_buffer(&call.uniform, 0, &bytes);
        let target = g.device.create_texture(&wgpu::TextureDescriptor {
            label: None,
            size: extent(w, h, 1),
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let row = (w * 4).next_multiple_of(256);
        let out = g.device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: (row * h) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let view = target.create_view(&Default::default());
        let mut enc = g.device.create_command_encoder(&Default::default());
        {
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
                })],
                ..Default::default()
            });
            pass.set_pipeline(&g.pipeline);
            pass.set_bind_group(0, &call.bind, &[]);
            pass.draw(0..3, 0..1);
        }
        enc.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo { texture: &target, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            wgpu::TexelCopyBufferInfo { buffer: &out, layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(row), rows_per_image: Some(h) } },
            extent(w, h, 1),
        );
        g.queue.submit([enc.finish()]);
        out.slice(..).map_async(wgpu::MapMode::Read, |r| r.unwrap());
        g.device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
        let data = out.slice(..).get_mapped_range().unwrap();
        (0..h).flat_map(|y| (0..w).map(move |x| (y, x))).map(|(y, x)| {
            let i = (y * row + x * 4) as usize;
            [data[i], data[i + 1], data[i + 2], data[i + 3]]
        }).collect()
    }

    /// A headless GPU (any adapter, also a software one); `None`: skip.
    fn gpu() -> Option<Arc<Gpu>> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default())).ok()?;
        eprintln!("GPU: {:?}", adapter.get_info().name);
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default())).ok()?;
        Some(Gpu::build(device, queue, wgpu::TextureFormat::Rgba8Unorm))
    }

    /// RGGB mosaic of one flat colour (12 bit).
    fn flat_bayer(w: u32, h: u32, rgb: [u16; 3]) -> Source<'static> {
        let mut data = Vec::new();
        for y in 0..h {
            for x in 0..w {
                data.extend(rgb[((x & 1) + (y & 1)) as usize].to_le_bytes());
            }
        }
        Source { width: w, height: h, kind: Kind::Bayer, max: 4095.0, wide: true, data: Cow::Owned(data) }
    }

    fn close(px: [u8; 4], want: [f32; 3], tol: f32) -> bool {
        (0..3).all(|c| (px[c] as f32 - want[c] * 255.0).abs() <= tol)
    }

    #[test]
    fn gpu_bayer_flat_colour_survives_demosaic() {
        let Some(g) = gpu() else { return };
        let rgb = [1000u16, 2000, 3000];
        let mut p = Picture::new(g, &flat_bayer(16, 12, rgb));
        p.set_look(&Arc::new(table(&View::default(), None, None, false)), None);
        let want = rgb.map(|v| v as f32 / 4095.0);
        // Bilinear at full size, and the box when shrunk.
        for (w, h, step) in [(16, 12, 1), (8, 6, 2)] {
            let out = render(&p, w, h, step);
            assert!(out.iter().all(|&px| close(px, want, 1.0)), "{step}: {:?}", &out[..4]);
        }
    }

    #[test]
    fn gpu_grey_levels_in_the_table() {
        let Some(g) = gpu() else { return };
        let src = Source { width: 256, height: 2, kind: Kind::Gray, max: 255.0, wide: false, data: (0..512).map(|i| (i % 256) as u8).collect::<Vec<u8>>().into() };
        let mut p = Picture::new(g, &src);
        p.set_look(&Arc::new(table(&View::default(), Some((0.25, 0.75)), None, false)), None);
        let out = render(&p, 256, 2, 1);
        for (x, px) in out.iter().take(256).enumerate() {
            let want = ((x as f32 / 255.0 - 0.25) / 0.5).clamp(0.0, 1.0);
            assert!(close(*px, [want; 3], 1.5), "{x}: {px:?} vs {want}");
        }
    }

    #[test]
    fn gpu_cube_mixes_channels() {
        let Some(g) = gpu() else { return };
        // 3D LUT swapping red and blue (R fastest in the file).
        let mut text = String::from("LUT_3D_SIZE 2\n");
        for b in 0..2 {
            for gg in 0..2 {
                for r in 0..2 {
                    text += &format!("{b} {gg} {r}\n");
                }
            }
        }
        let lut = Lut::parse(&text).unwrap();
        let rgb = [1000u16, 2000, 3000];
        let mut p = Picture::new(g, &flat_bayer(16, 12, rgb));
        let c = Arc::new(cube(&View::default(), Some(&lut)));
        p.set_look(&Arc::new(table(&View::default(), None, None, true)), Some(&c));
        let out = render(&p, 16, 12, 1);
        let want = [3000.0 / 4095.0, 2000.0 / 4095.0, 1000.0 / 4095.0];
        assert!(out.iter().all(|&px| close(px, want, 2.0)), "{:?}", &out[..4]);
    }

    #[test]
    fn gpu_planar_rgb() {
        let Some(g) = gpu() else { return };
        let rgb: Vec<u8> = (0..4 * 4).flat_map(|_| [10u8, 128, 250]).collect();
        let mut p = Picture::new(g, &Source::from_rgb8(4, 4, &rgb));
        p.set_look(&Arc::new(table(&View::default(), None, None, false)), None);
        let out = render(&p, 4, 4, 1);
        assert!(out.iter().all(|&px| px[..3] == [10, 128, 250]), "{:?}", &out[..2]);
    }

    /// Full speed: upload a 12-bit 1072×1074 mosaic, correct it with the
    /// cube, read the RGBA back; frames per second (`--ignored`).
    #[test]
    #[ignore]
    fn gpu_speed() {
        let Some(g) = gpu() else { return };
        for (w, h) in [(1072, 1074), (1944, 1470)] {
        let src = flat_bayer(w, h, [1000, 2000, 3000]);
        let mut p = Picture::new(g.clone(), &src);
        let lut = Lut::parse("LUT_3D_SIZE 2\n0 0 0\n1 0 0\n0 1 0\n1 1 0\n0 0 1\n1 0 1\n0 1 1\n1 1 1\n").unwrap();
        p.set_look(&Arc::new(table(&View::default(), None, None, true)), Some(&Arc::new(cube(&View::default(), Some(&lut)))));
        let n = 240;
        let t = std::time::Instant::now();
        for _ in 0..n {
            p.upload(&src);
            render(&p, w, h, 1);
        }
        let fps = n as f64 / t.elapsed().as_secs_f64();
        eprintln!("{w}x{h}: {fps:.0} fps upload + shader + readback");
        }
    }

    /// Recording: a flat Bayer colour comes back as the BT.709 full-range
    /// P010 of the same corrected colour, frames in order.
    #[test]
    fn gpu_converter_p010() {
        let Some(g) = gpu() else { return };
        let (w, h) = (16u32, 12u32);
        let frame = |v: [u16; 3], id| {
            let src = flat_bayer(w, h, v);
            Frame { id, timestamp_ns: 0, width: w, height: h, format: crate::frame::PixelFormat::BayerRG12, data: src.data.into_owned() }
        };
        let mut c = Converter::new(g, &frame([0; 3], 0)).unwrap();
        let t = Arc::new(table(&View::default(), None, None, false));
        let colours: Vec<[u16; 3]> = (0..10).map(|i| [400 * i, 2000, 4095 - 400 * i]).collect();
        let mut out = Vec::new();
        for (i, v) in colours.iter().enumerate() {
            if c.full() {
                out.push(c.take().unwrap().unwrap().to_vec());
            }
            c.submit(&frame(*v, i as u64), &t, None).unwrap();
        }
        while let Some(r) = c.take() {
            out.push(r.unwrap().to_vec());
        }
        assert_eq!(out.len(), colours.len());
        for (v, o) in colours.iter().zip(&out) {
            assert_eq!(o.len(), (w * h * 2 + w * h) as usize);
            let [r, gg, b] = v.map(|x| x as f32 / 4095.0);
            let y = 0.2126 * r + 0.7152 * gg + 0.0722 * b;
            let word = |i: usize| (u16::from_le_bytes([o[2 * i], o[2 * i + 1]]) >> 6) as f32 / 1023.0;
            let cb = (b - y) / 1.8556 + 0.5;
            let cr = (r - y) / 1.5748 + 0.5;
            let uv = (w * h) as usize;
            assert!((word(0) - y).abs() < 1.5 / 1023.0, "Y {} vs {y}", word(0));
            assert!((word(uv) - cb).abs() < 1.5 / 1023.0 && (word(uv + 1) - cr).abs() < 1.5 / 1023.0, "{} {} vs {cb} {cr}", word(uv), word(uv + 1));
        }
    }

    #[test]
    fn levels_from_histogram() {
        let mut h = vec![0u32; HIST];
        h[100] = 1000;
        h[900] = 1000;
        let (lo, hi) = levels_of(&h).unwrap();
        assert!((lo - 100.0 / 1023.0).abs() < 1e-3 && (hi - 900.0 / 1023.0).abs() < 1e-3);
    }
}
