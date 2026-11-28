//! The program's exposure control loop: measures the incoming raw picture
//! and drives the sensor live (exposure first, gain only when the exposure
//! is at its limit), without stopping the stream or a recording.
//!
//! Measured on the Alvium 1800 U-291c (2026-11-28, `live-test`): exposure
//! and gain are written in 1–2.5 ms while streaming and recording, no frame
//! goes missing, and the picture follows at once. The frame rate is locked
//! while streaming.

use crate::gpu_view::{Kind, Source};
use crate::scenario::{CameraSettings, Metering};
use std::time::{Duration, Instant};

/// Bins of the brightness histogram.
const BINS: usize = 1024;
/// Brighter than this (of full scale) counts as burnt out.
const CLIP: f32 = 0.985;
/// Darker than this counts as crushed.
const CRUSH: f32 = 0.01;

/// What the raw picture looks like (linear, 0..1 of full scale).
#[derive(Clone, Copy, Debug, Default)]
pub struct Measure {
    /// Share of pixels with a burnt-out channel.
    pub clipped: f32,
    /// Share of pixels with every channel near black.
    pub crushed: f32,
    /// The bright end: 99.5th percentile of the brightest channel.
    pub hi: f32,
    /// Mean brightness (luma).
    pub mean: f32,
}

impl Measure {
    /// From about 64k pixels spread over the picture.
    pub fn of(src: &Source) -> Measure {
        let (w, h) = (src.width as usize, src.height as usize);
        let step = ((w * h / 65536) as f64).sqrt().max(1.0) as usize;
        let step = if src.kind == Kind::Bayer { step.next_multiple_of(2) } else { step };
        let mut hist = vec![0u32; BINS];
        let (mut n, mut clipped, mut crushed, mut sum) = (0u32, 0u32, 0u32, 0f64);
        for y in (0..h.saturating_sub(1)).step_by(step) {
            for x in (0..w.saturating_sub(1)).step_by(step) {
                let p = src.rgb(x, y);
                let top = p[0].max(p[1]).max(p[2]);
                hist[((top * (BINS - 1) as f32) as usize).min(BINS - 1)] += 1;
                clipped += (top >= CLIP) as u32;
                crushed += (top <= CRUSH) as u32;
                sum += ((p[0] + 2.0 * p[1] + p[2]) / 4.0) as f64;
                n += 1;
            }
        }
        if n == 0 {
            return Measure::default();
        }
        let mut acc = 0u32;
        let limit = (n as f32 * 0.995) as u32;
        let hi = hist.iter().position(|&c| {
            acc += c;
            acc >= limit
        });
        Measure {
            clipped: clipped as f32 / n as f32,
            crushed: crushed as f32 / n as f32,
            hi: hi.unwrap_or(BINS - 1) as f32 / (BINS - 1) as f32,
            mean: (sum / n as f64) as f32,
        }
    }
}

/// Exposure and gain the loop may use: (exposure min, max [µs], gain
/// min, max [dB]).
pub type Bounds = (f64, f64, f64, f64);

/// State of the loop between frames.
#[derive(Default)]
pub struct Control {
    last: Option<Instant>,
    /// Last write: frames already on their way still show the old setting.
    wrote: Option<Instant>,
    /// Last measurement and what the loop thinks of it.
    pub measure: Measure,
    pub status: String,
}

/// After a write, measurements are skipped for this long (frames in the
/// camera, on USB and in the preview still show the old exposure).
const SETTLE: Duration = Duration::from_millis(80);
/// Errors smaller than this (natural log, ~4%) are left alone.
const DEADBAND: f64 = 0.04;

impl Control {
    pub fn reset(&mut self) {
        *self = Control::default();
    }

    /// One step: the new (exposure, gain) to write, if it should change.
    /// `now`: what the camera runs.
    pub fn step(&mut self, m: Measure, c: &CameraSettings, now: (f64, f64), b: Bounds) -> Option<(f64, f64)> {
        self.measure = m;
        let t = Instant::now();
        if self.wrote.is_some_and(|w| t.duration_since(w) < SETTLE) {
            return None;
        }
        let dt = self.last.map_or(0.04, |l| t.duration_since(l).as_secs_f64().min(0.25));
        self.last = Some(t);
        let target = c.program_target.clamp(0.01, 0.98);
        // How much more (>1) or less light the sensor should get.
        let mut ratio = match c.metering {
            Metering::Highlights => target / (m.hi as f64).max(1e-3),
            // Keep the highlights: the bright end stays below 95%.
            Metering::Mean => (target / (m.mean as f64).max(1e-3)).min(0.95 / (m.hi as f64).max(1e-3)),
        };
        if m.hi >= CLIP {
            // Burnt out: the true brightness is unknown, step down firmly.
            ratio = ratio.min(0.7);
        }
        let err = ratio.ln();
        let (e, g) = now;
        let (emin, emax, gmin, gmax) = b;
        let light = e * 10f64.powf(g / 20.0);
        let at_limit = (err > 0.0 && e >= emax * 0.999 && g >= gmax - 0.01) || (err < 0.0 && e <= emin * 1.001 && g <= gmin + 0.01);
        self.status = format!(
            "csúcsfény {:.0}% · átlag {:.0}% · beégett {:.1}% · bebukott {:.1}%{}",
            m.hi * 100.0,
            m.mean * 100.0,
            m.clipped * 100.0,
            m.crushed * 100.0,
            if at_limit { " · határon" } else { "" }
        );
        if err.abs() < DEADBAND || at_limit {
            return None;
        }
        // First-order lag at program_rate Hz.
        let a = 1.0 - (-std::f64::consts::TAU * c.program_rate.max(0.01) * dt).exp();
        let want = light * (err * a).exp();
        // Exposure first (less noise), gain only for what is still missing.
        let e2 = want.clamp(emin, emax);
        let g2 = (20.0 * (want / e2).log10()).clamp(gmin, gmax);
        if (e2 / e - 1.0).abs() < 0.003 && (g2 - g).abs() < 0.03 {
            return None;
        }
        self.wrote = Some(t);
        Some((e2, g2))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sensor seeing a scene whose bright end is at `hi_k` × light and
    /// mean at `mean_k` × light (light 1 = 4000 µs at 0 dB).
    fn run(metering: Metering, target: f64, hi_k: f64, mean_k: f64) -> (f64, f64, Measure) {
        let c = CameraSettings { program: true, metering, program_target: target, program_rate: 5.0, ..crate::scenario::Scenario::default().camera };
        let b = (10.0, 8000.0, 0.0, 24.0);
        let (mut e, mut g) = (100.0, 0.0);
        let mut ctl = Control::default();
        let mut m = Measure::default();
        for _ in 0..400 {
            let light = e * 10f64.powf(g / 20.0) / 4000.0;
            m = Measure { hi: (hi_k * light).min(1.0) as f32, mean: (mean_k * light).min(1.0) as f32, clipped: if hi_k * light > 0.985 { 0.05 } else { 0.0 }, crushed: 0.0 };
            ctl.wrote = None;
            ctl.last = Some(Instant::now() - Duration::from_millis(40));
            if let Some((e2, g2)) = ctl.step(m, &c, (e, g), b) {
                (e, g) = (e2, g2);
            }
        }
        (e, g, m)
    }

    #[test]
    fn highlights_settle_below_full_scale_exposure_first() {
        let (e, g, m) = run(Metering::Highlights, 0.8, 0.9, 0.2);
        assert!((m.hi - 0.8).abs() < 0.05, "{m:?}");
        // 0.8 / 0.9 × 4000 µs ≈ 3556 µs: within the exposure range, no gain.
        assert!((e - 3556.0).abs() < 200.0 && g < 0.1, "{e} {g}");
    }

    #[test]
    fn gain_only_when_exposure_is_at_its_limit() {
        // Mean 0.5 needs light 2.5 = 10000 µs: 8000 µs + ~1.9 dB.
        let (e, g, m) = run(Metering::Mean, 0.5, 0.3, 0.2);
        assert!(e > 7990.0 && (g - 1.94).abs() < 0.3 && (m.mean - 0.5).abs() < 0.03, "{e} {g} {m:?}");
    }

    #[test]
    fn mean_metering_keeps_the_highlights() {
        // Mean 0.5 would burn the bright end (0.9 × 2.5): it stops below.
        let (_, _, m) = run(Metering::Mean, 0.5, 0.9, 0.2);
        assert!(m.hi < 0.985 && m.clipped == 0.0, "{m:?}");
    }
}
