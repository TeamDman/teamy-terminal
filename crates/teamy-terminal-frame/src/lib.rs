//! Bounded, renderer-neutral terminal frame packets.
//!
//! The packet format intentionally contains no native window, Vulkan, DirectX,
//! Minecraft, or process-hosting types. It is suitable for archive/keyframe
//! PNGs and for live raw dirty-tile transport across a future boundary.

use eyre::Context;
use eyre::Result;
use std::io::Cursor;

/// Maximum supported frame width and height for the portable boundary.
pub const MAX_FRAME_DIMENSION: u32 = 4096;
/// Maximum encoded packet size accepted by the portable boundary.
pub const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;
const BYTES_PER_PIXEL: u32 = 4;

/// Pixel formats currently understood by the transport contract.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FrameFormat {
    /// Red, green, blue, alpha byte order.
    Rgba8,
    /// Blue, green, red, alpha byte order.
    Bgra8,
}

impl FrameFormat {
    const fn bytes_per_pixel() -> u32 {
        BYTES_PER_PIXEL
    }
}

/// Whether a packet is a complete frame or one tile within a complete frame.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FrameKind {
    /// Replace the complete frame at the given dimensions.
    Full,
    /// Replace one bounded tile within the complete frame.
    DirtyTile {
        /// Tile origin in the complete frame.
        x: u32,
        /// Tile origin in the complete frame.
        y: u32,
        /// Tile width in pixels.
        width: u32,
        /// Tile height in pixels.
        height: u32,
    },
}

impl FrameKind {
    const fn payload_dimensions(self, frame_width: u32, frame_height: u32) -> (u32, u32) {
        match self {
            Self::Full => (frame_width, frame_height),
            Self::DirtyTile { width, height, .. } => (width, height),
        }
    }
}

/// Encoding used for the packet bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FrameCompression {
    /// Tight row-major bytes.
    Raw,
    /// Lossless PNG bytes.
    Png,
}

/// Portable metadata describing one frame packet.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FrameDescriptor {
    /// Monotonically increasing producer sequence.
    pub sequence: u64,
    /// Complete frame width in pixels.
    pub width: u32,
    /// Complete frame height in pixels.
    pub height: u32,
    /// Bytes between rows in the encoded raw payload.
    pub stride: u32,
    /// Byte order of the payload pixels.
    pub format: FrameFormat,
    /// Full-frame or dirty-tile semantics.
    pub kind: FrameKind,
    /// Encoding of the packet bytes.
    pub compression: FrameCompression,
}

/// Bounded portable frame metadata and payload.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FramePacket {
    /// Metadata needed to place and decode this packet.
    pub descriptor: FrameDescriptor,
    /// Raw or losslessly encoded payload bytes.
    pub bytes: Vec<u8>,
}

/// Bounds accepted by a frame consumer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FrameLimits {
    /// Maximum complete-frame width.
    pub max_width: u32,
    /// Maximum complete-frame height.
    pub max_height: u32,
    /// Maximum encoded packet bytes.
    pub max_bytes: usize,
}

impl Default for FrameLimits {
    fn default() -> Self {
        Self {
            max_width: MAX_FRAME_DIMENSION,
            max_height: MAX_FRAME_DIMENSION,
            max_bytes: MAX_FRAME_BYTES,
        }
    }
}

impl FramePacket {
    /// Adopt an already-packed raw packet, including a BGRA8 payload.
    ///
    /// # Errors
    ///
    /// Returns an error when the descriptor is not raw or the packet violates
    /// the portable bounds.
    pub fn raw(descriptor: FrameDescriptor, bytes: Vec<u8>) -> Result<Self> {
        if descriptor.compression != FrameCompression::Raw {
            return Err(eyre::eyre!("raw packet constructor requires raw compression"));
        }
        let packet = Self { descriptor, bytes };
        packet.validate()?;
        Ok(packet)
    }

    /// Encode a complete RGBA8 frame.
    ///
    /// # Errors
    ///
    /// Returns an error when dimensions, source bytes, or the encoded payload
    /// exceed the portable bounds.
    pub fn full_rgba(
        sequence: u64,
        width: u32,
        height: u32,
        pixels: &[u8],
        compression: FrameCompression,
    ) -> Result<Self> {
        Self::from_rgba(
            sequence,
            width,
            height,
            pixels,
            FrameKind::Full,
            compression,
        )
    }

    /// Encode one dirty tile from a complete RGBA8 frame.
    ///
    /// # Errors
    ///
    /// Returns an error when the tile, source bytes, dimensions, or encoded
    /// payload are invalid or exceed the portable bounds.
    pub fn dirty_rgba(
        sequence: u64,
        width: u32,
        height: u32,
        pixels: &[u8],
        tile: FrameKind,
        compression: FrameCompression,
    ) -> Result<Self> {
        if !matches!(tile, FrameKind::DirtyTile { .. }) {
            return Err(eyre::eyre!("dirty packet requires a DirtyTile frame kind"));
        }
        Self::from_rgba(sequence, width, height, pixels, tile, compression)
    }

    /// Validate and encode an RGBA8 source frame or tile.
    ///
    /// # Errors
    ///
    /// Returns an error when dimensions, tile bounds, source bytes, or the
    /// encoded payload are invalid or exceed the portable bounds.
    ///
    /// # Panics
    ///
    /// This function does not panic for a valid portable frame. Internal
    /// conversion assertions are protected by the preceding bound checks.
    pub fn from_rgba(
        sequence: u64,
        width: u32,
        height: u32,
        pixels: &[u8],
        kind: FrameKind,
        compression: FrameCompression,
    ) -> Result<Self> {
        validate_dimensions(width, height, FrameLimits::default())?;
        let (payload_width, payload_height) = kind.payload_dimensions(width, height);
        validate_kind(kind, width, height)?;
        let full_len = checked_len(width, height, BYTES_PER_PIXEL)?;
        if pixels.len() != full_len {
            return Err(eyre::eyre!(
                "RGBA source has {} bytes, expected {full_len}",
                pixels.len()
            ));
        }
        let raw = match kind {
            FrameKind::Full => pixels.to_vec(),
            FrameKind::DirtyTile {
                x,
                y,
                width: tile_width,
                height: tile_height,
            } => {
                let row_bytes = usize::try_from(tile_width)
                    .expect("validated tile width fits usize")
                    .checked_mul(usize::try_from(BYTES_PER_PIXEL).expect("constant fits"))
                    .ok_or_else(|| eyre::eyre!("dirty tile row size overflows"))?;
                let frame_stride = usize::try_from(width)
                    .expect("validated frame width fits usize")
                    .checked_mul(usize::try_from(BYTES_PER_PIXEL).expect("constant fits"))
                    .ok_or_else(|| eyre::eyre!("frame stride overflows"))?;
                let x_offset = usize::try_from(x)
                    .expect("validated tile x fits usize")
                    .checked_mul(usize::try_from(BYTES_PER_PIXEL).expect("constant fits"))
                    .ok_or_else(|| eyre::eyre!("tile x offset overflows"))?;
                let mut tile_pixels = Vec::with_capacity(
                    row_bytes
                        .checked_mul(
                            usize::try_from(tile_height).expect("validated height fits usize"),
                        )
                        .ok_or_else(|| eyre::eyre!("dirty tile size overflows"))?,
                );
                for row in y..y + tile_height {
                    let start = usize::try_from(row)
                        .expect("validated tile y fits usize")
                        .checked_mul(frame_stride)
                        .and_then(|start| start.checked_add(x_offset))
                        .ok_or_else(|| eyre::eyre!("dirty tile source offset overflows"))?;
                    let end = start
                        .checked_add(row_bytes)
                        .ok_or_else(|| eyre::eyre!("dirty tile source end overflows"))?;
                    let source = pixels
                        .get(start..end)
                        .ok_or_else(|| eyre::eyre!("dirty tile exceeds source pixels"))?;
                    tile_pixels.extend_from_slice(source);
                }
                tile_pixels
            }
        };
        let stride = payload_width
            .checked_mul(BYTES_PER_PIXEL)
            .ok_or_else(|| eyre::eyre!("frame stride overflows"))?;
        let bytes = match compression {
            FrameCompression::Raw => raw,
            FrameCompression::Png => encode_png(payload_width, payload_height, &raw)?,
        };
        let packet = Self {
            descriptor: FrameDescriptor {
                sequence,
                width,
                height,
                stride,
                format: FrameFormat::Rgba8,
                kind,
                compression,
            },
            bytes,
        };
        packet.validate()?;
        Ok(packet)
    }

    /// Validate this packet against the default portable bounds.
    ///
    /// # Errors
    ///
    /// Returns an error when metadata, dimensions, payload size, or encoded
    /// pixel data is invalid.
    pub fn validate(&self) -> Result<()> {
        self.validate_with(FrameLimits::default())
    }

    /// Validate this packet against consumer-provided bounds.
    ///
    /// # Errors
    ///
    /// Returns an error when metadata, dimensions, payload size, or encoded
    /// pixel data exceeds the supplied limits or is malformed.
    pub fn validate_with(&self, limits: FrameLimits) -> Result<()> {
        let descriptor = self.descriptor;
        validate_dimensions(descriptor.width, descriptor.height, limits)?;
        validate_kind(descriptor.kind, descriptor.width, descriptor.height)?;
        let (payload_width, payload_height) = descriptor
            .kind
            .payload_dimensions(descriptor.width, descriptor.height);
        let expected_stride = payload_width
            .checked_mul(FrameFormat::bytes_per_pixel())
            .ok_or_else(|| eyre::eyre!("frame stride overflows"))?;
        if descriptor.stride != expected_stride {
            return Err(eyre::eyre!(
                "frame stride {} does not match expected {expected_stride}",
                descriptor.stride
            ));
        }
        if self.bytes.len() > limits.max_bytes {
            return Err(eyre::eyre!(
                "frame packet has {} bytes, exceeding the {} byte limit",
                self.bytes.len(),
                limits.max_bytes
            ));
        }
        if descriptor.compression == FrameCompression::Png
            && descriptor.format != FrameFormat::Rgba8
        {
            return Err(eyre::eyre!("PNG packets must declare RGBA8 format"));
        }
        match descriptor.compression {
            FrameCompression::Raw => {
                let expected = checked_len(payload_width, payload_height, BYTES_PER_PIXEL)?;
                if self.bytes.len() != expected {
                    return Err(eyre::eyre!(
                        "raw frame has {} bytes, expected {expected}",
                        self.bytes.len()
                    ));
                }
            }
            FrameCompression::Png => {
                validate_png(&self.bytes, payload_width, payload_height, limits.max_bytes)?;
            }
        }
        Ok(())
    }

    /// Return the encoded payload bytes.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

fn validate_dimensions(width: u32, height: u32, limits: FrameLimits) -> Result<()> {
    if width == 0 || height == 0 || width > limits.max_width || height > limits.max_height {
        return Err(eyre::eyre!(
            "frame dimensions {}x{} exceed the {}x{} bounds",
            width,
            height,
            limits.max_width,
            limits.max_height
        ));
    }
    checked_len(width, height, BYTES_PER_PIXEL)?;
    Ok(())
}

fn validate_kind(kind: FrameKind, width: u32, height: u32) -> Result<()> {
    if let FrameKind::DirtyTile {
        x,
        y,
        width: tile_width,
        height: tile_height,
    } = kind
    {
        let right = x
            .checked_add(tile_width)
            .ok_or_else(|| eyre::eyre!("dirty tile x extent overflows"))?;
        let bottom = y
            .checked_add(tile_height)
            .ok_or_else(|| eyre::eyre!("dirty tile y extent overflows"))?;
        if tile_width == 0 || tile_height == 0 || right > width || bottom > height {
            return Err(eyre::eyre!(
                "dirty tile {}x{}+{}+{} is outside frame {}x{}",
                tile_width,
                tile_height,
                x,
                y,
                width,
                height
            ));
        }
    }
    Ok(())
}

fn checked_len(width: u32, height: u32, bytes_per_pixel: u32) -> Result<usize> {
    usize::try_from(width)
        .ok()
        .and_then(|width| width.checked_mul(usize::try_from(height).ok()?))
        .and_then(|pixels| pixels.checked_mul(usize::try_from(bytes_per_pixel).ok()?))
        .ok_or_else(|| eyre::eyre!("frame byte length overflows"))
}

fn encode_png(width: u32, height: u32, pixels: &[u8]) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    {
        let mut encoder = png::Encoder::new(Cursor::new(&mut output), width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .wrap_err("failed to write portable frame PNG header")?;
        writer
            .write_image_data(pixels)
            .wrap_err("failed to write portable frame PNG pixels")?;
        writer
            .finish()
            .wrap_err("failed to finish portable frame PNG")?;
    };
    Ok(output)
}

fn validate_png(bytes: &[u8], width: u32, height: u32, max_bytes: usize) -> Result<()> {
    if bytes.len() > max_bytes {
        return Err(eyre::eyre!("PNG frame exceeds the packet byte limit"));
    }
    let mut png_decoder = png::Decoder::new(Cursor::new(bytes));
    png_decoder.set_transformations(png::Transformations::IDENTITY);
    let mut reader = png_decoder
        .read_info()
        .wrap_err("failed to read portable frame PNG header")?;
    let output_size = reader
        .output_buffer_size()
        .ok_or_else(|| eyre::eyre!("PNG decoded size is unavailable"))?;
    if output_size > max_bytes {
        return Err(eyre::eyre!("decoded PNG exceeds the packet byte limit"));
    }
    let mut decoded_pixels = vec![0_u8; output_size];
    let info = reader
        .next_frame(&mut decoded_pixels)
        .wrap_err("failed to decode portable frame PNG")?;
    if info.width != width
        || info.height != height
        || info.color_type != png::ColorType::Rgba
        || info.bit_depth != png::BitDepth::Eight
    {
        return Err(eyre::eyre!(
            "PNG payload does not match RGBA8 dimensions {}x{}",
            width,
            height
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pixels(width: u32, height: u32) -> Vec<u8> {
        (0..checked_len(width, height, BYTES_PER_PIXEL).expect("test frame fits"))
            .map(|value| u8::try_from(value % 251).expect("test byte fits"))
            .collect()
    }

    #[test]
    fn raw_full_frame_round_trips_metadata_and_bounds() {
        let packet = FramePacket::full_rgba(7, 4, 3, &pixels(4, 3), FrameCompression::Raw)
            .expect("raw packet should encode");
        assert_eq!(packet.descriptor.sequence, 7);
        assert_eq!(packet.descriptor.stride, 16);
        assert_eq!(packet.descriptor.kind, FrameKind::Full);
        assert_eq!(packet.bytes().len(), 48);
        packet.validate().expect("raw packet should validate");
        let bgra = FramePacket::raw(
            FrameDescriptor {
                sequence: 8,
                width: 4,
                height: 3,
                stride: 16,
                format: FrameFormat::Bgra8,
                kind: FrameKind::Full,
                compression: FrameCompression::Raw,
            },
            vec![0; 48],
        )
        .expect("BGRA raw packet should validate");
        assert_eq!(bgra.descriptor.format, FrameFormat::Bgra8);
    }

    #[test]
    fn dirty_tile_packs_tight_rows_and_png_validates() {
        let source = pixels(4, 3);
        let tile = FrameKind::DirtyTile {
            x: 1,
            y: 1,
            width: 2,
            height: 2,
        };
        let raw = FramePacket::dirty_rgba(8, 4, 3, &source, tile, FrameCompression::Raw)
            .expect("dirty packet should encode");
        assert_eq!(raw.descriptor.stride, 8);
        assert_eq!(
            raw.bytes(),
            &source[20..28]
                .iter()
                .chain(source[36..44].iter())
                .copied()
                .collect::<Vec<_>>()
        );
        let png = FramePacket::dirty_rgba(9, 4, 3, &source, tile, FrameCompression::Png)
            .expect("dirty PNG should encode");
        png.validate().expect("dirty PNG should validate");
    }

    #[test]
    fn malformed_dimensions_and_tiles_are_rejected() {
        let _ = FramePacket::full_rgba(1, 0, 1, &[], FrameCompression::Raw).unwrap_err();
        let source = pixels(4, 3);
        let _ = FramePacket::dirty_rgba(
            1,
            4,
            3,
            &source,
            FrameKind::DirtyTile {
                x: 3,
                y: 2,
                width: 2,
                height: 1,
            },
            FrameCompression::Raw,
        )
        .unwrap_err();
    }
}
