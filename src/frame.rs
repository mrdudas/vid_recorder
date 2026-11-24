//! Frame data and pixel formats.

use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};

/// Supported pixel formats. Names match GenICam SFNC `PixelFormat` entries.
/// Formats above 8 bit are unpacked, little endian, 2 bytes per pixel.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PixelFormat {
    Mono8,
    Mono10,
    Mono12,
    Mono16,
    BayerRG8,
    BayerRG12,
}

impl PixelFormat {
    pub const ALL: [PixelFormat; 6] = [
        PixelFormat::Mono8,
        PixelFormat::Mono10,
        PixelFormat::Mono12,
        PixelFormat::Mono16,
        PixelFormat::BayerRG8,
        PixelFormat::BayerRG12,
    ];

    pub fn name(self) -> &'static str {
        match self {
            PixelFormat::Mono8 => "Mono8",
            PixelFormat::Mono10 => "Mono10",
            PixelFormat::Mono12 => "Mono12",
            PixelFormat::Mono16 => "Mono16",
            PixelFormat::BayerRG8 => "BayerRG8",
            PixelFormat::BayerRG12 => "BayerRG12",
        }
    }

    pub fn bytes_per_pixel(self) -> usize {
        match self {
            PixelFormat::Mono8 | PixelFormat::BayerRG8 => 1,
            _ => 2,
        }
    }

    /// Significant bits per pixel.
    pub fn bits(self) -> u32 {
        match self {
            PixelFormat::Mono8 | PixelFormat::BayerRG8 => 8,
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
    /// raw sensor values survive for later processing.
    pub fn ffmpeg_input(self) -> &'static str {
        match self {
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
    pub fn byte_size(width: u32, height: u32, format: PixelFormat) -> usize {
        width as usize * height as usize * format.bytes_per_pixel()
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
