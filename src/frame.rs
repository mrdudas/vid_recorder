//! Frame data and pixel formats.

use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};

/// Supported pixel formats. Mono/Bayer names match GenICam SFNC
/// `PixelFormat` entries (above 8 bit: unpacked, little endian, 2 bytes per
/// pixel). YUYV, NV12 and MJPEG come from UVC cameras.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PixelFormat {
    Mono8,
    Mono10,
    Mono12,
    Mono16,
    BayerRG8,
    BayerRG12,
    #[serde(rename = "YUYV")]
    Yuyv,
    #[serde(rename = "NV12")]
    Nv12,
    /// Each frame is one complete JPEG image (variable size).
    #[serde(rename = "MJPEG")]
    Mjpeg,
}

impl PixelFormat {
    pub const ALL: [PixelFormat; 9] = [
        PixelFormat::Mono8,
        PixelFormat::Mono10,
        PixelFormat::Mono12,
        PixelFormat::Mono16,
        PixelFormat::BayerRG8,
        PixelFormat::BayerRG12,
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
            PixelFormat::BayerRG12 => "BayerRG12",
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
            PixelFormat::Yuyv | PixelFormat::Nv12 | PixelFormat::Mjpeg => 8,
            PixelFormat::Mono10 => 10,
            PixelFormat::Mono12 | PixelFormat::BayerRG12 => 12,
            PixelFormat::Mono16 => 16,
        }
    }

    pub fn is_bayer(self) -> bool {
        matches!(self, PixelFormat::BayerRG8 | PixelFormat::BayerRG12)
    }

    /// ffmpeg rawvideo input format used for recording. Bayer data is
    /// recorded as a grey image (the mosaic is kept, not demosaiced) so the
    /// raw sensor values survive for later processing. MJPEG is not raw
    /// video; the recorder copies it unchanged.
    pub fn ffmpeg_input(self) -> &'static str {
        match self {
            PixelFormat::Yuyv => "yuyv422",
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
