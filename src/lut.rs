//! `.cube` LUT (1D or 3D) for the live preview. Recordings never get a LUT.

use std::path::Path;

pub struct Lut {
    size: usize,
    is_3d: bool,
    /// RGB entries; for 3D, red changes fastest (cube file order).
    data: Vec<[f32; 3]>,
    min: [f32; 3],
    max: [f32; 3],
}

impl Lut {
    pub fn load(path: &Path) -> Result<Lut, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        Self::parse(&text).map_err(|e| format!("{}: {e}", path.display()))
    }

    pub fn parse(text: &str) -> Result<Lut, String> {
        let (mut size, mut is_3d) = (0usize, false);
        let (mut min, mut max) = ([0.0f32; 3], [1.0f32; 3]);
        let mut data = Vec::new();
        let triple = |parts: &[&str]| -> Result<[f32; 3], String> {
            if parts.len() != 3 {
                return Err(format!("expected 3 numbers: {}", parts.join(" ")));
            }
            let mut v = [0.0; 3];
            for (o, p) in v.iter_mut().zip(parts) {
                *o = p.parse().map_err(|e| format!("bad number {p}: {e}"))?;
            }
            Ok(v)
        };
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let parts: Vec<&str> = line.split_whitespace().collect();
            match parts[0] {
                "TITLE" => {}
                "LUT_1D_SIZE" | "LUT_3D_SIZE" => {
                    is_3d = parts[0] == "LUT_3D_SIZE";
                    size = parts.get(1).and_then(|s| s.parse().ok()).ok_or("bad LUT size")?;
                }
                "DOMAIN_MIN" => min = triple(&parts[1..])?,
                "DOMAIN_MAX" => max = triple(&parts[1..])?,
                // Unknown keywords (e.g. LUT_1D_INPUT_RANGE) are ignored.
                k if k.chars().next().is_some_and(|c| c.is_ascii_alphabetic()) => {}
                _ => data.push(triple(&parts)?),
            }
        }
        let expected = if is_3d { size * size * size } else { size };
        if size < 2 || data.len() != expected {
            return Err(format!("expected {expected} entries, found {}", data.len()));
        }
        Ok(Lut { size, is_3d, data, min, max })
    }

    /// Map an RGB value in 0..=1.
    pub fn apply(&self, rgb: [f32; 3]) -> [f32; 3] {
        let n = (self.size - 1) as f32;
        let pos: [f32; 3] =
            std::array::from_fn(|c| ((rgb[c] - self.min[c]) / (self.max[c] - self.min[c])).clamp(0.0, 1.0) * n);
        if !self.is_3d {
            return std::array::from_fn(|c| {
                let i = (pos[c] as usize).min(self.size - 2);
                let t = pos[c] - i as f32;
                self.data[i][c] * (1.0 - t) + self.data[i + 1][c] * t
            });
        }
        // Trilinear interpolation.
        let i: [usize; 3] = std::array::from_fn(|c| (pos[c] as usize).min(self.size - 2));
        let t: [f32; 3] = std::array::from_fn(|c| pos[c] - i[c] as f32);
        let s = self.size;
        let at = |r: usize, g: usize, b: usize| self.data[r + g * s + b * s * s];
        let mut out = [0.0f32; 3];
        for (dr, wr) in [(0, 1.0 - t[0]), (1, t[0])] {
            for (dg, wg) in [(0, 1.0 - t[1]), (1, t[1])] {
                for (db, wb) in [(0, 1.0 - t[2]), (1, t[2])] {
                    let v = at(i[0] + dr, i[1] + dg, i[2] + db);
                    let w = wr * wg * wb;
                    for c in 0..3 {
                        out[c] += v[c] * w;
                    }
                }
            }
        }
        out
    }
}

/// `.cube` files under `dir` (recursive), as paths relative to `dir` with
/// '/' separators, sorted.
pub fn list(dir: &Path) -> Vec<String> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<String>) {
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let path = e.path();
            if path.is_dir() {
                walk(base, &path, out);
            } else if path.extension().is_some_and(|x| x.eq_ignore_ascii_case("cube"))
                && let Ok(rel) = path.strip_prefix(base)
            {
                let parts: Vec<_> = rel.components().map(|c| c.as_os_str().to_string_lossy()).collect();
                out.push(parts.join("/"));
            }
        }
    }
    let mut v = Vec::new();
    walk(dir, dir, &mut v);
    v.sort();
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_3d() {
        let mut s = String::from("LUT_3D_SIZE 2\n");
        for b in 0..2 {
            for g in 0..2 {
                for r in 0..2 {
                    s += &format!("{r} {g} {b}\n");
                }
            }
        }
        let lut = Lut::parse(&s).unwrap();
        let out = lut.apply([0.25, 0.5, 0.75]);
        for (o, e) in out.iter().zip([0.25, 0.5, 0.75]) {
            assert!((o - e).abs() < 1e-5);
        }
    }

    #[test]
    fn invert_1d() {
        let lut = Lut::parse("LUT_1D_SIZE 2\n1 1 1\n0 0 0\n").unwrap();
        assert_eq!(lut.apply([0.0; 3]), [1.0; 3]);
        assert_eq!(lut.apply([1.0; 3]), [0.0; 3]);
    }
}
