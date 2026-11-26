//! Frame data and pixel formats.

use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};

/// Supported pixel formats. Mono/Bayer/YCbCr names match GenICam SFNC
/// `PixelFormat` entries; YUYV, NV12 and MJPEG come from UVC cameras.
///
/// In memory and in recordings everything above 8 bit is unpacked: little
/// endian, 2 bytes per pixel. The packed GenICam formats (`...p`, LSB-first)
/// are what the camera sends over USB; they are unpacked on arrival
/// (bit-exact) and keep their name so the recording says what the camera
/// was set to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PixelFormat {
    Mono8,
    Mono10,
    Mono12,
    Mono16,
    BayerRG8,
    BayerRG10,
    BayerRG12,
    /// 10 bit, 4 pixels in 5 bytes on the wire.
    BayerRG10p,
    /// 12 bit, 2 pixels in 3 bytes on the wire.
    BayerRG12p,
    /// UYVY 4:2:2, 8 bit (Cb Y Cr Y).
    #[serde(rename = "YCbCr422_8_CbYCrY")]
    Uyvy,
    #[serde(rename = "YUYV")]
    Yuyv,
    #[serde(rename = "NV12")]
    Nv12,
    /// Each frame is one complete JPEG image (variable size).
    #[serde(rename = "MJPEG")]
    Mjpeg,
}

impl PixelFormat {
    pub const ALL: [PixelFormat; 13] = [
        PixelFormat::Mono8,
        PixelFormat::Mono10,
        PixelFormat::Mono12,
        PixelFormat::Mono16,
        PixelFormat::BayerRG8,
        PixelFormat::BayerRG10,
        PixelFormat::BayerRG12,
        PixelFormat::BayerRG10p,
        PixelFormat::BayerRG12p,
        PixelFormat::Uyvy,
        PixelFormat::Yuyv,
        PixelFormat::Nv12,
        PixelFormat::Mjpeg,
    ];

    pub fn name(self) -> &'static str {
        match self {
            PixelFormat::Mono8 => "Mono8",
            PixelFormat::Mono10 => "Mono10",
            PixelFormat::Mono12 => "Mono12",
            PixelFormat::Mono16 => "Mono16",
            PixelFormat::BayerRG8 => "BayerRG8",
            PixelFormat::BayerRG10 => "BayerRG10",
            PixelFormat::BayerRG12 => "BayerRG12",
            PixelFormat::BayerRG10p => "BayerRG10p",
            PixelFormat::BayerRG12p => "BayerRG12p",
            PixelFormat::Uyvy => "YCbCr422_8_CbYCrY",
            PixelFormat::Yuyv => "YUYV",
            PixelFormat::Nv12 => "NV12",
            PixelFormat::Mjpeg => "MJPEG",
        }
    }

    /// Bytes per pixel of the first plane (1 for NV12 luma; MJPEG: 1 as an
    /// estimate, real size varies).
    pub fn bytes_per_pixel(self) -> usize {
        match self {
            PixelFormat::Mono8 | PixelFormat::BayerRG8 | PixelFormat::Nv12 | PixelFormat::Mjpeg => 1,
            _ => 2,
        }
    }

    /// Significant bits per pixel.
    pub fn bits(self) -> u32 {
        match self {
            PixelFormat::Mono8 | PixelFormat::BayerRG8 => 8,
            PixelFormat::Yuyv | PixelFormat::Uyvy | PixelFormat::Nv12 | PixelFormat::Mjpeg => 8,
            PixelFormat::Mono10 | PixelFormat::BayerRG10 | PixelFormat::BayerRG10p => 10,
            PixelFormat::Mono12 | PixelFormat::BayerRG12 | PixelFormat::BayerRG12p => 12,
            PixelFormat::Mono16 => 16,
        }
    }

    pub fn is_bayer(self) -> bool {
        matches!(
            self,
            PixelFormat::BayerRG8
                | PixelFormat::BayerRG10
                | PixelFormat::BayerRG12
                | PixelFormat::BayerRG10p
                | PixelFormat::BayerRG12p
        )
    }

    /// Packed on the wire (unpacked to 2 bytes per pixel on arrival).
    /// One sample per pixel (Bayer mosaic or Mono). GPU-encoded recordings
    /// keep these in the luma plane of a YUV stream.
    pub fn is_single_channel(self) -> bool {
        self.is_bayer() || matches!(self, PixelFormat::Mono8 | PixelFormat::Mono10 | PixelFormat::Mono12 | PixelFormat::Mono16)
    }

    pub fn is_packed(self) -> bool {
        matches!(self, PixelFormat::BayerRG10p | PixelFormat::BayerRG12p)
    }

    /// ffmpeg rawvideo input format used for recording. Bayer data is
    /// recorded as a grey image (the mosaic is kept, not demosaiced) so the
    /// raw sensor values survive for later processing. MJPEG is not raw
    /// video; the recorder copies it unchanged.
    pub fn ffmpeg_input(self) -> &'static str {
        match self {
            PixelFormat::Yuyv => "yuyv422",
            PixelFormat::Uyvy => "uyvy422",
            PixelFormat::BayerRG10 | PixelFormat::BayerRG10p => "gray10le",
            PixelFormat::BayerRG12p => "gray12le",
            PixelFormat::Nv12 => "nv12",
            PixelFormat::Mjpeg => "mjpeg",
            PixelFormat::Mono8 | PixelFormat::BayerRG8 => "gray",
            PixelFormat::Mono10 => "gray10le",
            PixelFormat::Mono12 | PixelFormat::BayerRG12 => "gray12le",
            PixelFormat::Mono16 => "gray16le",
        }
    }
}

pub struct Frame {
    /// Frame / block id from the camera. Consecutive frames differ by 1.
    pub id: u64,
    /// Camera timestamp in nanoseconds.
    pub timestamp_ns: u64,
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    pub data: Vec<u8>,
}

impl Frame {
    /// Size of one frame in bytes (MJPEG: an upper-range estimate).
    pub fn byte_size(width: u32, height: u32, format: PixelFormat) -> usize {
        let px = width as usize * height as usize;
        match format {
            PixelFormat::Nv12 => px * 3 / 2,
            _ => px * format.bytes_per_pixel(),
        }
    }
}

/// Cut `(x, y, w, h)` out of `src` into `dst` (software crop, for cameras
/// that cannot crop themselves). Offsets and size must be even for Bayer,
/// YUYV/UYVY and NV12 (keeps the colour pattern); see [`crop_step`].
/// MJPEG cannot be cropped without decoding.
pub fn crop(src: &Frame, x: u32, y: u32, w: u32, h: u32, dst: &mut [u8]) {
    let (x, y, w, h, sw) = (x as usize, y as usize, w as usize, h as usize, src.width as usize);
    let copy_rows = |plane: &[u8], out: &mut [u8], bpp: usize, rows: usize, y0: usize, row_len_src: usize| {
        for (r, line) in out.chunks_exact_mut(w * bpp).take(rows).enumerate() {
            let start = (y0 + r) * row_len_src + x * bpp;
            line.copy_from_slice(&plane[start..start + w * bpp]);
        }
    };
    match src.format {
        PixelFormat::Nv12 => {
            let (luma, chroma) = src.data.split_at(sw * src.height as usize);
            let (dl, dc) = dst.split_at_mut(w * h);
            copy_rows(luma, dl, 1, h, y, sw);
            // Interleaved UV at half height: same bytes per row as luma.
            copy_rows(chroma, dc, 1, h / 2, y / 2, sw);
        }
        f => copy_rows(&src.data, dst, f.bytes_per_pixel(), h, y, sw * f.bytes_per_pixel()),
    }
}

/// Step of software crop offsets and sizes for `format`; `None`: cannot be
/// cropped in software.
pub fn crop_step(format: PixelFormat) -> Option<u32> {
    match format {
        PixelFormat::Mjpeg => None,
        f if f.is_bayer() || matches!(f, PixelFormat::Yuyv | PixelFormat::Uyvy | PixelFormat::Nv12) => Some(2),
        _ => Some(1),
    }
}

/// Unpack GenICam LSB-first packed pixels (`format.is_packed()`) from `src`
/// into little-endian u16 in `dst` (2 bytes per pixel). Bit-exact.
pub fn unpack(format: PixelFormat, src: &[u8], dst: &mut [u8]) {
    let bits = format.bits() as usize;
    let mask = (1u64 << bits) - 1;
    // Whole groups: 4 px / 5 bytes (10 bit) or 2 px / 3 bytes (12 bit).
    let (group_px, group_bytes) = if bits == 10 { (4, 5) } else { (2, 3) };
    let groups = (dst.len() / 2 / group_px).min(src.len() / group_bytes);
    for (s, d) in src.chunks_exact(group_bytes).zip(dst.chunks_exact_mut(group_px * 2)).take(groups) {
        let mut v = 0u64;
        for (i, b) in s.iter().enumerate() {
            v |= (*b as u64) << (8 * i);
        }
        for (i, px) in d.chunks_exact_mut(2).enumerate() {
            px.copy_from_slice(&(((v >> (bits * i)) & mask) as u16).to_le_bytes());
        }
    }
    // Leftover pixels (pixel count not a multiple of the group size).
    for px in groups * group_px..dst.len() / 2 {
        let bit = px * bits;
        let mut v = 0u64;
        for i in 0..3 {
            v |= (*src.get(bit / 8 + i).unwrap_or(&0) as u64) << (8 * i);
        }
        let p = ((v >> (bit % 8)) & mask) as u16;
        dst[px * 2..px * 2 + 2].copy_from_slice(&p.to_le_bytes());
    }
}

/// Recycles frame buffers so steady-state acquisition does not allocate.
#[derive(Clone, Default)]
pub struct BufferPool(Arc<Mutex<Vec<Vec<u8>>>>);

impl BufferPool {
    /// A buffer of exactly `len` bytes (contents unspecified).
    pub fn get(&self, len: usize) -> Vec<u8> {
        let mut buf = self.0.lock().unwrap().pop().unwrap_or_default();
        buf.resize(len, 0);
        buf
    }

    pub fn put(&self, buf: Vec<u8>) {
        let mut pool = self.0.lock().unwrap();
        // Keep the pool bounded; extra buffers are freed.
        if pool.len() < 256 {
            pool.push(buf);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reference LSB-first packer.
    fn pack(values: &[u16], bits: usize) -> Vec<u8> {
        let mut out = vec![0u8; (values.len() * bits).div_ceil(8)];
        for (i, v) in values.iter().enumerate() {
            for b in 0..bits {
                if v >> b & 1 == 1 {
                    let bit = i * bits + b;
                    out[bit / 8] |= 1 << (bit % 8);
                }
            }
        }
        out
    }

    fn roundtrip(format: PixelFormat, n: usize) {
        let bits = format.bits() as usize;
        let values: Vec<u16> = (0..n).map(|i| ((i * 2654435761) >> 7) as u16 & ((1 << bits) - 1) as u16).collect();
        let packed = pack(&values, bits);
        let mut dst = vec![0u8; n * 2];
        unpack(format, &packed, &mut dst);
        let got: Vec<u16> = dst.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        assert_eq!(got, values, "{format:?} n={n}");
    }

    #[test]
    fn crop_cuts_the_right_pixels() {
        // 8x4 Mono16 and NV12 with each byte = its position.
        for format in [PixelFormat::Mono16, PixelFormat::Yuyv, PixelFormat::Nv12, PixelFormat::Mono8] {
            let (w, h) = (8u32, 4u32);
            let len = Frame::byte_size(w, h, format);
            let src = Frame { id: 0, timestamp_ns: 0, width: w, height: h, format, data: (0..len).map(|i| i as u8).collect() };
            let mut dst = vec![0u8; Frame::byte_size(4, 2, format)];
            crop(&src, 2, 2, 4, 2, &mut dst);
            let bpp = if format == PixelFormat::Nv12 { 1 } else { format.bytes_per_pixel() };
            let row = |y: usize| -> Vec<u8> { (0..4 * bpp).map(|i| ((y * 8 * bpp) + 2 * bpp + i) as u8).collect() };
            assert_eq!(&dst[..4 * bpp], &row(2)[..], "{format:?}");
            assert_eq!(&dst[4 * bpp..8 * bpp], &row(3)[..], "{format:?}");
            if format == PixelFormat::Nv12 {
                // Chroma row 1 (of 2), bytes 2..6.
                assert_eq!(&dst[8..12], &[32 + 8 + 2, 32 + 8 + 3, 32 + 8 + 4, 32 + 8 + 5]);
            }
        }
    }

    #[test]
    fn unpack_is_bit_exact() {
        for n in [1, 2, 3, 4, 5, 7, 8, 1920 * 4 + 3] {
            roundtrip(PixelFormat::BayerRG10p, n);
            roundtrip(PixelFormat::BayerRG12p, n);
        }
    }

    /// cargo test --release -- --ignored --nocapture
    #[test]
    #[ignore]
    fn unpack_speed() {
        for format in [PixelFormat::BayerRG10p, PixelFormat::BayerRG12p] {
            let px = 1920 * 1080;
            let src = vec![0x5Au8; px * format.bits() as usize / 8];
            let mut dst = vec![0u8; px * 2];
            let t = std::time::Instant::now();
            for _ in 0..200 {
                unpack(format, &src, &mut dst);
            }
            let per = t.elapsed().as_secs_f64() / 200.0;
            println!("{format:?} 1920x1080: {:.2} ms/frame = {:.0} fps max (one core)", per * 1e3, 1.0 / per);
        }
    }
}
