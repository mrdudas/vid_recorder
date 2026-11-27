//! Noise-matched 8-bit curve: 10/12-bit sensor samples are stored as 8-bit
//! codes spaced by the sensor's noise, so the full dynamic range fits in
//! 8 bits and the rounding adds only ~5% to the noise.
//!
//! Noise measured on the Alvium 1800 U-291c (12 bit, 0 dB, 2026-11-27):
//! σ² ≈ r² + k·v with r ≈ 1 DN and k ≈ 0.2 (12-bit units), i.e. ~280 noise
//! steps over 0..4095. Code = 255 · u(v) / u(max), u(v) = ∫ dv / σ(v).
//! At higher gain the noise per DN is larger, so the curve stays safe.

/// Stream tag value (`CURVE` tag) naming the curve and its parameters.
pub const TAG: &str = "noise8 r=1 k=0.2";
const R: f64 = 1.0;
const K: f64 = 0.2;

/// Noise steps from 0 to `v` (12-bit units).
fn steps(v: f64) -> f64 {
    2.0 / K * ((R * R + K * v).sqrt() - R)
}

/// Sensor value (0 .. 2^bits-1) -> 8-bit code.
pub fn encode_table(bits: u32) -> Vec<u8> {
    let max = (1u32 << bits) - 1;
    let to12 = 4095.0 / max as f64;
    let total = steps(max as f64 * to12);
    (0..=max).map(|v| (255.0 * steps(v as f64 * to12) / total).round() as u8).collect()
}

/// 8-bit code -> sensor value at `bits` depth: the mean of the values that
/// map to the code.
pub fn decode_table(bits: u32) -> [u16; 256] {
    let (mut sum, mut n) = ([0f64; 256], [0u32; 256]);
    for (v, &c) in encode_table(bits).iter().enumerate() {
        sum[c as usize] += v as f64;
        n[c as usize] += 1;
    }
    let mut t = [0u16; 256];
    for c in 0..256 {
        t[c] = if n[c] > 0 { (sum[c] / n[c] as f64).round() as u16 } else { 0 };
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_code_used_and_error_below_noise() {
        for bits in [10, 12] {
            let (e, d) = (encode_table(bits), decode_table(bits));
            let max = (1u32 << bits) - 1;
            assert_eq!((e[0], e[max as usize]), (0, 255));
            // 12 bit: every code used. 10 bit: near black one input step is
            // already coarser than the noise, so codes are skipped there.
            assert!(e.windows(2).all(|w| w[0] <= w[1] && (bits < 12 || w[1] - w[0] <= 1)));
            let to12 = 4095.0 / max as f64;
            for v in 0..=max {
                let err = (d[e[v as usize] as usize] as f64 - v as f64) * to12;
                let sigma = (R * R + K * v as f64 * to12).sqrt();
                assert!(err.abs() <= 0.75 * sigma + 0.5 * to12, "{bits} bit {v}: error {err:.2}, noise {sigma:.2}");
            }
        }
    }
}
