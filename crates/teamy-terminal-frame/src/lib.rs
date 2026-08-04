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
/// Version of the portable full/dirty RGBA update contract.
pub const RASTER_FRAME_CONTRACT_VERSION: u16 = 1;
/// Maximum ordered dirty regions in one atomic update.
pub const MAX_DIRTY_REGIONS: usize = 64;
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

/// Complete-versus-incremental semantics for one atomic raster update.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RasterUpdateKind {
    /// Replace the complete composed frame.
    Full,
    /// Apply every ordered region to the exact declared base frame.
    DirtyRegions,
}

/// One region within an atomic packed dirty update.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RasterRegion {
    /// Region origin in the complete frame.
    pub x: u32,
    /// Region origin in the complete frame.
    pub y: u32,
    /// Region width in pixels.
    pub width: u32,
    /// Region height in pixels.
    pub height: u32,
    /// Tight row stride, currently `width * 4`.
    pub stride: u32,
    /// Start of this region in the enclosing packed payload.
    pub payload_offset: u32,
    /// Number of payload bytes owned by this region.
    pub payload_length: u32,
}

/// A rectangle requested by the shared damage planner before payload packing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RasterRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// One canonical dirty rectangle paired with already-tight RGBA8 rows.
///
/// GPU readback paths use this form to avoid materializing or copying an
/// unchanged complete host frame before constructing the portable update.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PackedRasterRegion<'a> {
    pub rect: RasterRect,
    pub rgba: &'a [u8],
}

/// Metadata for a versioned complete or base-dependent raster update.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RasterUpdateDescriptor {
    pub contract_version: u16,
    pub sequence: u64,
    pub base_sequence: u64,
    pub width: u32,
    pub height: u32,
    /// Complete-frame raw stride. Dirty updates use zero; each region carries
    /// its own tight stride.
    pub stride: u32,
    pub format: FrameFormat,
    pub kind: RasterUpdateKind,
    pub compression: FrameCompression,
}

/// One bounded atomic full/dirty update. Dirty region payload ranges are
/// contiguous, non-overlapping, and ordered top-to-bottom then left-to-right.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RasterFrameUpdate {
    pub descriptor: RasterUpdateDescriptor,
    pub regions: Vec<RasterRegion>,
    pub bytes: Vec<u8>,
}

/// Pure CPU composition state used by headless and consumer-equivalence tests.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ComposedRasterFrame {
    pub sequence: u64,
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
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
            return Err(eyre::eyre!(
                "raw packet constructor requires raw compression"
            ));
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

impl RasterFrameUpdate {
    /// Construct a complete raw or PNG RGBA8 update.
    ///
    /// # Errors
    ///
    /// Returns an error when dimensions, source bytes, compression, or the
    /// encoded update violate the portable frame contract.
    pub fn full_rgba(
        sequence: u64,
        width: u32,
        height: u32,
        pixels: &[u8],
        compression: FrameCompression,
    ) -> Result<Self> {
        let packet = FramePacket::full_rgba(sequence, width, height, pixels, compression)?;
        let update = Self {
            descriptor: RasterUpdateDescriptor {
                contract_version: RASTER_FRAME_CONTRACT_VERSION,
                sequence,
                base_sequence: 0,
                width,
                height,
                stride: width
                    .checked_mul(BYTES_PER_PIXEL)
                    .ok_or_else(|| eyre::eyre!("frame stride overflows"))?,
                format: FrameFormat::Rgba8,
                kind: RasterUpdateKind::Full,
                compression,
            },
            regions: Vec::new(),
            bytes: packet.bytes,
        };
        update.validate()?;
        Ok(update)
    }

    /// Adopt canonical, tightly packed dirty-region RGBA8 bytes.
    ///
    /// Unlike [`Self::dirty_rgba`], this constructor does not require a
    /// complete CPU-visible source frame. Every payload must contain exactly
    /// `rect.width * rect.height * 4` bytes in tight top-to-bottom rows.
    ///
    /// # Errors
    ///
    /// Returns an error when dimensions, region ordering or bounds, payload
    /// lengths, or the packed update violate the portable frame contract.
    ///
    /// # Panics
    ///
    /// This function does not panic for a valid portable frame. Internal
    /// conversion assertions are protected by the preceding bound checks.
    pub fn dirty_rgba_packed(
        sequence: u64,
        base_sequence: u64,
        width: u32,
        height: u32,
        packed: &[PackedRasterRegion<'_>],
    ) -> Result<Self> {
        validate_dimensions(width, height, FrameLimits::default())?;
        if packed.is_empty() || packed.len() > MAX_DIRTY_REGIONS {
            return Err(eyre::eyre!(
                "dirty update has {} regions, expected 1..={MAX_DIRTY_REGIONS}",
                packed.len()
            ));
        }
        let rects = packed.iter().map(|region| region.rect).collect::<Vec<_>>();
        validate_rect_order_and_overlap(&rects, width, height)?;

        let mut bytes = Vec::new();
        let mut regions = Vec::with_capacity(packed.len());
        for region in packed {
            let stride = region
                .rect
                .width
                .checked_mul(BYTES_PER_PIXEL)
                .ok_or_else(|| eyre::eyre!("dirty region stride overflows"))?;
            let expected = usize::try_from(stride)
                .expect("validated stride fits usize")
                .checked_mul(usize::try_from(region.rect.height).expect("validated height fits"))
                .ok_or_else(|| eyre::eyre!("dirty region byte length overflows"))?;
            if region.rgba.len() != expected {
                return Err(eyre::eyre!(
                    "packed dirty region has {} bytes, expected {expected}",
                    region.rgba.len()
                ));
            }
            let payload_offset =
                u32::try_from(bytes.len()).wrap_err("dirty payload offset exceeds u32")?;
            bytes.extend_from_slice(region.rgba);
            let payload_length =
                u32::try_from(region.rgba.len()).wrap_err("dirty payload length exceeds u32")?;
            regions.push(RasterRegion {
                x: region.rect.x,
                y: region.rect.y,
                width: region.rect.width,
                height: region.rect.height,
                stride,
                payload_offset,
                payload_length,
            });
        }

        Self::dirty_rgba_from_parts(sequence, base_sequence, width, height, regions, bytes)
    }

    fn dirty_rgba_from_parts(
        sequence: u64,
        base_sequence: u64,
        width: u32,
        height: u32,
        regions: Vec<RasterRegion>,
        bytes: Vec<u8>,
    ) -> Result<Self> {
        let update = Self {
            descriptor: RasterUpdateDescriptor {
                contract_version: RASTER_FRAME_CONTRACT_VERSION,
                sequence,
                base_sequence,
                width,
                height,
                stride: 0,
                format: FrameFormat::Rgba8,
                kind: RasterUpdateKind::DirtyRegions,
                compression: FrameCompression::Raw,
            },
            regions,
            bytes,
        };
        update.validate()?;
        Ok(update)
    }

    /// Adopt an already encoded complete PNG without re-encoding it.
    ///
    /// This keeps retained encoder buffers useful at transport boundaries
    /// while still applying the same dimensions, byte bound, and PNG header
    /// validation as `full_rgba`.
    ///
    /// # Errors
    ///
    /// Returns an error when dimensions, the encoded payload, or its PNG
    /// metadata violate the portable frame contract.
    pub fn full_png(sequence: u64, width: u32, height: u32, encoded: Vec<u8>) -> Result<Self> {
        let update = Self {
            descriptor: RasterUpdateDescriptor {
                contract_version: RASTER_FRAME_CONTRACT_VERSION,
                sequence,
                base_sequence: 0,
                width,
                height,
                stride: width
                    .checked_mul(BYTES_PER_PIXEL)
                    .ok_or_else(|| eyre::eyre!("frame stride overflows"))?,
                format: FrameFormat::Rgba8,
                kind: RasterUpdateKind::Full,
                compression: FrameCompression::Png,
            },
            regions: Vec::new(),
            bytes: encoded,
        };
        update.validate()?;
        Ok(update)
    }

    /// Pack ordered dirty rectangles from a complete RGBA8 source frame.
    ///
    /// # Errors
    ///
    /// Returns an error when dimensions, source bytes, rectangle ordering or
    /// bounds, or the packed update violate the portable frame contract.
    ///
    /// # Panics
    ///
    /// This function does not panic for a valid portable frame. Internal
    /// conversion assertions are protected by the preceding bound checks.
    pub fn dirty_rgba(
        sequence: u64,
        base_sequence: u64,
        width: u32,
        height: u32,
        pixels: &[u8],
        rects: &[RasterRect],
    ) -> Result<Self> {
        validate_dimensions(width, height, FrameLimits::default())?;
        let expected = checked_len(width, height, BYTES_PER_PIXEL)?;
        if pixels.len() != expected {
            return Err(eyre::eyre!(
                "RGBA source has {} bytes, expected {expected}",
                pixels.len()
            ));
        }
        if rects.is_empty() || rects.len() > MAX_DIRTY_REGIONS {
            return Err(eyre::eyre!(
                "dirty update has {} regions, expected 1..={MAX_DIRTY_REGIONS}",
                rects.len()
            ));
        }
        validate_rect_order_and_overlap(rects, width, height)?;

        let frame_stride = usize::try_from(width)
            .expect("validated width fits usize")
            .checked_mul(usize::try_from(BYTES_PER_PIXEL).expect("constant fits"))
            .ok_or_else(|| eyre::eyre!("frame stride overflows"))?;
        let mut bytes = Vec::new();
        let mut regions = Vec::with_capacity(rects.len());
        for rect in rects {
            let stride = rect
                .width
                .checked_mul(BYTES_PER_PIXEL)
                .ok_or_else(|| eyre::eyre!("dirty region stride overflows"))?;
            let payload_offset =
                u32::try_from(bytes.len()).wrap_err("dirty payload offset exceeds u32")?;
            let row_bytes = usize::try_from(stride).expect("validated stride fits usize");
            let x_bytes = usize::try_from(rect.x)
                .expect("validated x fits usize")
                .checked_mul(usize::try_from(BYTES_PER_PIXEL).expect("constant fits"))
                .ok_or_else(|| eyre::eyre!("dirty x offset overflows"))?;
            for row in rect.y..rect.y + rect.height {
                let start = usize::try_from(row)
                    .expect("validated row fits usize")
                    .checked_mul(frame_stride)
                    .and_then(|offset| offset.checked_add(x_bytes))
                    .ok_or_else(|| eyre::eyre!("dirty source offset overflows"))?;
                let end = start
                    .checked_add(row_bytes)
                    .ok_or_else(|| eyre::eyre!("dirty source end overflows"))?;
                bytes.extend_from_slice(
                    pixels
                        .get(start..end)
                        .ok_or_else(|| eyre::eyre!("dirty source region is out of bounds"))?,
                );
            }
            let payload_length = u32::try_from(bytes.len())
                .ok()
                .and_then(|end| end.checked_sub(payload_offset))
                .ok_or_else(|| eyre::eyre!("dirty payload length exceeds u32"))?;
            regions.push(RasterRegion {
                x: rect.x,
                y: rect.y,
                width: rect.width,
                height: rect.height,
                stride,
                payload_offset,
                payload_length,
            });
        }
        Self::dirty_rgba_from_parts(sequence, base_sequence, width, height, regions, bytes)
    }

    /// Validate version, bounds, ordering, payload ranges, and base semantics.
    ///
    /// # Errors
    ///
    /// Returns an error when the update violates the default portable frame
    /// bounds or any metadata, payload, ordering, or base-frame invariant.
    pub fn validate(&self) -> Result<()> {
        self.validate_with(FrameLimits::default())
    }

    /// Validate against consumer-provided dimensions and byte bounds.
    ///
    /// # Errors
    ///
    /// Returns an error when the update exceeds the supplied limits or
    /// violates any metadata, payload, ordering, or base-frame invariant.
    pub fn validate_with(&self, limits: FrameLimits) -> Result<()> {
        let descriptor = self.descriptor;
        if descriptor.contract_version != RASTER_FRAME_CONTRACT_VERSION {
            return Err(eyre::eyre!(
                "unsupported raster frame contract version {}",
                descriptor.contract_version
            ));
        }
        validate_dimensions(descriptor.width, descriptor.height, limits)?;
        if descriptor.format != FrameFormat::Rgba8 {
            return Err(eyre::eyre!("V1 raster updates require RGBA8"));
        }
        if self.bytes.len() > limits.max_bytes {
            return Err(eyre::eyre!(
                "raster update has {} bytes, exceeding the {} byte limit",
                self.bytes.len(),
                limits.max_bytes
            ));
        }
        match descriptor.kind {
            RasterUpdateKind::Full => self.validate_full(descriptor, limits),
            RasterUpdateKind::DirtyRegions => self.validate_dirty(descriptor),
        }
    }

    fn validate_full(&self, descriptor: RasterUpdateDescriptor, limits: FrameLimits) -> Result<()> {
        if descriptor.base_sequence != 0 || !self.regions.is_empty() {
            return Err(eyre::eyre!(
                "complete raster update must have base zero and no regions"
            ));
        }
        let expected_stride = descriptor
            .width
            .checked_mul(BYTES_PER_PIXEL)
            .ok_or_else(|| eyre::eyre!("frame stride overflows"))?;
        if descriptor.stride != expected_stride {
            return Err(eyre::eyre!(
                "complete frame stride {} does not match {expected_stride}",
                descriptor.stride
            ));
        }
        match descriptor.compression {
            FrameCompression::Raw => {
                let expected = checked_len(descriptor.width, descriptor.height, BYTES_PER_PIXEL)?;
                if self.bytes.len() != expected {
                    return Err(eyre::eyre!(
                        "raw frame has {} bytes, expected {expected}",
                        self.bytes.len()
                    ));
                }
                Ok(())
            }
            FrameCompression::Png => validate_png(
                &self.bytes,
                descriptor.width,
                descriptor.height,
                limits.max_bytes,
            ),
        }
    }

    fn validate_dirty(&self, descriptor: RasterUpdateDescriptor) -> Result<()> {
        if descriptor.base_sequence == 0
            || descriptor.base_sequence >= descriptor.sequence
            || descriptor.stride != 0
            || descriptor.compression != FrameCompression::Raw
        {
            return Err(eyre::eyre!(
                "dirty update requires an earlier nonzero base, zero frame stride, and raw compression"
            ));
        }
        if self.regions.is_empty() || self.regions.len() > MAX_DIRTY_REGIONS {
            return Err(eyre::eyre!(
                "dirty update has {} regions, expected 1..={MAX_DIRTY_REGIONS}",
                self.regions.len()
            ));
        }
        let rects = self
            .regions
            .iter()
            .map(|region| RasterRect {
                x: region.x,
                y: region.y,
                width: region.width,
                height: region.height,
            })
            .collect::<Vec<_>>();
        validate_rect_order_and_overlap(&rects, descriptor.width, descriptor.height)?;
        let mut next_offset = 0_u32;
        for region in &self.regions {
            let expected_stride = region
                .width
                .checked_mul(BYTES_PER_PIXEL)
                .ok_or_else(|| eyre::eyre!("dirty region stride overflows"))?;
            let expected_length = expected_stride
                .checked_mul(region.height)
                .ok_or_else(|| eyre::eyre!("dirty region payload length overflows"))?;
            if region.stride != expected_stride
                || region.payload_offset != next_offset
                || region.payload_length != expected_length
            {
                return Err(eyre::eyre!(
                    "dirty region stride or packed payload range is invalid"
                ));
            }
            next_offset = next_offset
                .checked_add(region.payload_length)
                .ok_or_else(|| eyre::eyre!("dirty packed payload end overflows"))?;
        }
        if usize::try_from(next_offset).ok() != Some(self.bytes.len()) {
            return Err(eyre::eyre!(
                "dirty packed payload ranges do not cover the payload exactly"
            ));
        }
        Ok(())
    }
}

impl ComposedRasterFrame {
    /// Create composition state from a complete raw update.
    ///
    /// # Errors
    ///
    /// Returns an error when the update is invalid, incomplete, or not raw.
    pub fn from_full(update: &RasterFrameUpdate) -> Result<Self> {
        update.validate()?;
        if update.descriptor.kind != RasterUpdateKind::Full
            || update.descriptor.compression != FrameCompression::Raw
        {
            return Err(eyre::eyre!("composition requires a complete raw frame"));
        }
        Ok(Self {
            sequence: update.descriptor.sequence,
            width: update.descriptor.width,
            height: update.descriptor.height,
            pixels: update.bytes.clone(),
        })
    }

    /// Apply one contiguous-base raw dirty update atomically.
    ///
    /// # Errors
    ///
    /// Returns an error when the update is invalid, does not extend the
    /// current frame, or contains an invalid source or destination range.
    ///
    /// # Panics
    ///
    /// This function does not panic for a valid portable frame. Internal
    /// conversion assertions are protected by update validation.
    pub fn apply(&mut self, update: &RasterFrameUpdate) -> Result<()> {
        update.validate()?;
        if update.descriptor.kind != RasterUpdateKind::DirtyRegions
            || update.descriptor.base_sequence != self.sequence
            || update.descriptor.width != self.width
            || update.descriptor.height != self.height
        {
            return Err(eyre::eyre!(
                "dirty update does not extend the currently composed frame"
            ));
        }
        let frame_stride = usize::try_from(self.width)
            .expect("validated width fits usize")
            .checked_mul(usize::try_from(BYTES_PER_PIXEL).expect("constant fits"))
            .ok_or_else(|| eyre::eyre!("composed frame stride overflows"))?;
        for region in &update.regions {
            let source_start = usize::try_from(region.payload_offset)
                .expect("validated payload offset fits usize");
            let region_stride =
                usize::try_from(region.stride).expect("validated region stride fits usize");
            let x_bytes = usize::try_from(region.x)
                .expect("validated region x fits usize")
                .checked_mul(usize::try_from(BYTES_PER_PIXEL).expect("constant fits"))
                .ok_or_else(|| eyre::eyre!("composed region x overflows"))?;
            for row in 0..region.height {
                let source = source_start
                    .checked_add(
                        usize::try_from(row)
                            .expect("validated row fits usize")
                            .checked_mul(region_stride)
                            .ok_or_else(|| eyre::eyre!("source row offset overflows"))?,
                    )
                    .ok_or_else(|| eyre::eyre!("source offset overflows"))?;
                let destination = usize::try_from(region.y + row)
                    .expect("validated destination row fits usize")
                    .checked_mul(frame_stride)
                    .and_then(|offset| offset.checked_add(x_bytes))
                    .ok_or_else(|| eyre::eyre!("destination offset overflows"))?;
                let source_end = source
                    .checked_add(region_stride)
                    .ok_or_else(|| eyre::eyre!("source end overflows"))?;
                let destination_end = destination
                    .checked_add(region_stride)
                    .ok_or_else(|| eyre::eyre!("destination end overflows"))?;
                let source_row = update
                    .bytes
                    .get(source..source_end)
                    .ok_or_else(|| eyre::eyre!("source row is out of bounds"))?;
                self.pixels
                    .get_mut(destination..destination_end)
                    .ok_or_else(|| eyre::eyre!("destination row is out of bounds"))?
                    .copy_from_slice(source_row);
            }
        }
        self.sequence = update.descriptor.sequence;
        Ok(())
    }
}

fn validate_rect_order_and_overlap(
    rects: &[RasterRect],
    frame_width: u32,
    frame_height: u32,
) -> Result<()> {
    for (index, rect) in rects.iter().enumerate() {
        let right = rect
            .x
            .checked_add(rect.width)
            .ok_or_else(|| eyre::eyre!("dirty region x extent overflows"))?;
        let bottom = rect
            .y
            .checked_add(rect.height)
            .ok_or_else(|| eyre::eyre!("dirty region y extent overflows"))?;
        if rect.width == 0 || rect.height == 0 || right > frame_width || bottom > frame_height {
            return Err(eyre::eyre!("dirty region is outside the complete frame"));
        }
        if let Some(previous) = index.checked_sub(1).and_then(|i| rects.get(i))
            && (rect.y, rect.x) <= (previous.y, previous.x)
        {
            return Err(eyre::eyre!(
                "dirty regions are not in canonical top-left order"
            ));
        }
        for previous in &rects[..index] {
            let previous_right = previous.x + previous.width;
            let previous_bottom = previous.y + previous.height;
            if rect.x < previous_right
                && right > previous.x
                && rect.y < previous_bottom
                && bottom > previous.y
            {
                return Err(eyre::eyre!("dirty regions overlap"));
            }
        }
    }
    Ok(())
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

    #[test]
    fn packed_dirty_regions_compose_to_the_exact_full_frame() {
        let base_pixels = vec![0_u8; checked_len(4, 3, BYTES_PER_PIXEL).expect("test fits")];
        let mut current_pixels = base_pixels.clone();
        current_pixels[4..12].copy_from_slice(&[1, 2, 3, 255, 4, 5, 6, 255]);
        current_pixels[32..36].copy_from_slice(&[7, 8, 9, 255]);
        let full = RasterFrameUpdate::full_rgba(1, 4, 3, &base_pixels, FrameCompression::Raw)
            .expect("full update");
        let dirty = RasterFrameUpdate::dirty_rgba(
            2,
            1,
            4,
            3,
            &current_pixels,
            &[
                RasterRect {
                    x: 1,
                    y: 0,
                    width: 2,
                    height: 1,
                },
                RasterRect {
                    x: 0,
                    y: 2,
                    width: 1,
                    height: 1,
                },
            ],
        )
        .expect("dirty update");
        assert_eq!(dirty.regions[0].payload_offset, 0);
        assert_eq!(dirty.regions[0].payload_length, 8);
        assert_eq!(dirty.regions[1].payload_offset, 8);
        assert_eq!(dirty.regions[1].payload_length, 4);
        assert_eq!(dirty.bytes.len(), 12);

        let mut composed = ComposedRasterFrame::from_full(&full).expect("compose full");
        composed.apply(&dirty).expect("compose dirty");
        assert_eq!(composed.sequence, 2);
        assert_eq!(composed.pixels, current_pixels);
    }

    #[test]
    fn prepacked_dirty_regions_match_complete_frame_cropping_without_full_source_input() {
        let current_pixels = pixels(4, 3);
        let rects = [
            RasterRect {
                x: 1,
                y: 0,
                width: 2,
                height: 1,
            },
            RasterRect {
                x: 0,
                y: 2,
                width: 1,
                height: 1,
            },
        ];
        let cropped = RasterFrameUpdate::dirty_rgba(2, 1, 4, 3, &current_pixels, &rects)
            .expect("complete-source dirty update");
        let first = &current_pixels[4..12];
        let second = &current_pixels[32..36];
        let packed = RasterFrameUpdate::dirty_rgba_packed(
            2,
            1,
            4,
            3,
            &[
                PackedRasterRegion {
                    rect: rects[0],
                    rgba: first,
                },
                PackedRasterRegion {
                    rect: rects[1],
                    rgba: second,
                },
            ],
        )
        .expect("prepacked dirty update");

        assert_eq!(packed, cropped);
    }

    #[test]
    fn prepacked_dirty_regions_reject_wrong_payload_length() {
        let result = RasterFrameUpdate::dirty_rgba_packed(
            2,
            1,
            4,
            3,
            &[PackedRasterRegion {
                rect: RasterRect {
                    x: 1,
                    y: 0,
                    width: 2,
                    height: 1,
                },
                rgba: &[0; 7],
            }],
        );

        let _ = result.expect_err("wrong packed payload length should fail");
    }

    #[test]
    fn dirty_regions_reject_noncanonical_order_overlap_and_wrong_base() {
        let source = pixels(4, 3);
        let reversed = RasterFrameUpdate::dirty_rgba(
            2,
            1,
            4,
            3,
            &source,
            &[
                RasterRect {
                    x: 2,
                    y: 1,
                    width: 1,
                    height: 1,
                },
                RasterRect {
                    x: 0,
                    y: 0,
                    width: 1,
                    height: 1,
                },
            ],
        );
        let _ = reversed.expect_err("noncanonical region order should fail");
        let overlapping = RasterFrameUpdate::dirty_rgba(
            2,
            1,
            4,
            3,
            &source,
            &[
                RasterRect {
                    x: 0,
                    y: 0,
                    width: 2,
                    height: 2,
                },
                RasterRect {
                    x: 1,
                    y: 1,
                    width: 2,
                    height: 1,
                },
            ],
        );
        let _ = overlapping.expect_err("overlapping regions should fail");

        let full = RasterFrameUpdate::full_rgba(4, 4, 3, &source, FrameCompression::Raw)
            .expect("full update");
        let dirty = RasterFrameUpdate::dirty_rgba(
            6,
            5,
            4,
            3,
            &source,
            &[RasterRect {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            }],
        )
        .expect("structurally valid dirty update");
        let mut composed = ComposedRasterFrame::from_full(&full).expect("compose full");
        assert!(composed.apply(&dirty).is_err());
        assert_eq!(composed.sequence, 4);
    }

    #[test]
    fn dirty_validation_rejects_malformed_packed_ranges() {
        let source = pixels(2, 1);
        let mut dirty = RasterFrameUpdate::dirty_rgba(
            2,
            1,
            2,
            1,
            &source,
            &[RasterRect {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            }],
        )
        .expect("valid dirty update");
        dirty.regions[0].payload_offset = 1;
        assert!(dirty.validate().is_err());
        dirty.regions[0].payload_offset = 0;
        dirty.regions[0].stride = 8;
        assert!(dirty.validate().is_err());
    }
}
