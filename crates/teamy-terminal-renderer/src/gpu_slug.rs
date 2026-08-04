//! Retained, true-GPU Slug terminal rasterization for no-window Vulkan targets.

use crate::VulkanOffscreenCacheStats;
use crate::VulkanOffscreenCancellation;
use crate::VulkanOffscreenContext;
use crate::VulkanOffscreenError;
use crate::VulkanOffscreenReadback;
use crate::VulkanOffscreenRegion;
use crate::VulkanOffscreenRegionalReadback;
use crate::VulkanOffscreenSession;
use crate::VulkanOffscreenStageTimings;
use crate::vulkan_offscreen::VulkanOffscreenRecording;
use ash::vk;
use num_traits::ToPrimitive;
use std::collections::HashMap;
use std::ffi::CString;
use std::fmt;
use std::ptr;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;
use teamy_terminal_core::Cell;
use teamy_terminal_core::Color;
use teamy_terminal_core::MAX_COMBINING_SCALARS_PER_CELL;
use teamy_terminal_core::TerminalSnapshot;
use teamy_terminal_font::CellRect;
use teamy_terminal_font::EMBEDDED_FONT_ID;
use teamy_terminal_font::EMBEDDED_FONT_SHA256;
use teamy_terminal_font::MAX_DAMAGE_REGIONS;
use teamy_terminal_font::slug::DEFAULT_BAND_SIZE_FONT_UNITS;
use teamy_terminal_font::slug::GPU_GLYPH_METADATA_WORDS;
use teamy_terminal_font::slug::SlugError;
use teamy_terminal_font::slug::SlugFont;
use teamy_terminal_font::slug::SlugFontMetrics;
use teamy_terminal_font::slug::build_directional_bands;
use teamy_terminal_font::slug::build_gpu_glyph_metadata;
#[cfg(feature = "tracy")]
use tracing::info_span;
use unicode_width::UnicodeWidthChar;

include!(concat!(env!("OUT_DIR"), "/gpu_slug_shader.rs"));

/// Stable identity of the renderer implemented by this module.
pub const RUST_GPU_SLUG_RENDERER_ID: &str = "rust-gpu-slug";
/// FNV-1a identity of the exact WGSL source compiled into this build.
pub const GPU_SLUG_SHADER_SOURCE_IDENTITY: u64 = GPU_SLUG_SHADER_SOURCE_FNV1A64;
/// Default em size shared with the CPU reference sizing policy.
pub const GPU_SLUG_DEFAULT_PIXEL_SIZE: f32 = teamy_terminal_font::DEFAULT_PIXEL_SIZE;
/// Lower fitting bound for remote panel targets. The standalone interactive
/// zoom control keeps its own larger usability floor.
pub const GPU_SLUG_MIN_PIXEL_SIZE: f32 = 1.0;
/// Maximum effective em size accepted by the current bridge sizing policy.
pub const GPU_SLUG_MAX_PIXEL_SIZE: f32 = 64.0;

const CELL_WORDS: usize = 8 + MAX_COMBINING_SCALARS_PER_CELL;
const CELL_COMBINING_START: usize = 8;
const _: [(); CELL_WORDS] = [(); GPU_SLUG_SHADER_CELL_WORDS];
const _: [(); MAX_COMBINING_SCALARS_PER_CELL] = [(); GPU_SLUG_SHADER_MAX_COMBINING_GLYPHS];
const FRAME_WORDS: usize = 16;
const REGION_WORDS: usize = 4;
const MAX_RETAINED_GLYPHS: usize = 1_024;
const MAX_GPU_BUFFER_BYTES: usize = 64 * 1024 * 1024;
const MIN_GPU_BUFFER_BYTES: usize = 16;
const DEFAULT_RENDER_TIMEOUT: Duration = Duration::from_secs(5);

/// Correlation fields copied unchanged through one GPU render.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct GpuSlugRenderIdentity {
    /// Terminal session sequence.
    pub session_sequence: u64,
    /// Authoritative terminal-state sequence.
    pub terminal_sequence: u64,
    /// Portable frame sequence.
    pub frame_sequence: u64,
    /// Atomic renderer/transport presentation generation.
    pub presentation_generation: String,
    /// Cross-process correlation identifier.
    pub correlation_id: String,
}

/// Native cell/font metrics selected for a GPU render.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GpuSlugSurfaceMetrics {
    /// Requested em size in physical pixels.
    pub font_pixel_size: f32,
    /// Monospace cell width in physical pixels.
    pub cell_width: u32,
    /// Line height in physical pixels.
    pub cell_height: u32,
}

impl GpuSlugSurfaceMetrics {
    /// Native raster width for a logical terminal grid.
    #[must_use]
    pub fn native_width(self, columns: u16) -> u32 {
        u32::from(columns).saturating_mul(self.cell_width)
    }

    /// Native raster height for a logical terminal grid.
    #[must_use]
    pub fn native_height(self, rows: u16) -> u32 {
        u32::from(rows).saturating_mul(self.cell_height)
    }
}

/// One canonical dirty region with tightly packed top-left RGBA8 bytes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GpuSlugDirtyRegion {
    /// Pixel-space left edge.
    pub x: u32,
    /// Pixel-space top edge.
    pub y: u32,
    /// Pixel width.
    pub width: u32,
    /// Pixel height.
    pub height: u32,
    /// Tightly packed RGBA8 rows.
    pub rgba: Vec<u8>,
}

/// Retained resource counters after one render.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GpuSlugCacheStats {
    /// Geometry lookups served by retained outline/band entries.
    pub geometry_hits: u64,
    /// Geometry entries extracted and appended.
    pub geometry_misses: u64,
    /// Geometry entries retired by bounded cache reset.
    pub geometry_evictions: u64,
    /// Currently retained resolved glyph entries.
    pub retained_glyphs: usize,
    /// Maximum retained glyph entries.
    pub glyph_capacity: usize,
    /// GPU buffer allocations/reallocations.
    pub buffer_allocations: u64,
    /// Target allocation/reuse/eviction counters from the no-window runtime.
    pub targets: VulkanOffscreenCacheStats,
}

/// Bounded stage evidence for one GPU render.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct GpuSlugRenderStats {
    /// Complete geometry-cache lookup/build stage.
    pub geometry_build_us: u64,
    /// Font outline extraction for cache misses in this frame.
    pub outline_extraction_us: u64,
    /// Directional-band construction for cache misses in this frame.
    pub directional_band_build_us: u64,
    /// Host bytes uploaded to cell/geometry/region/frame buffers.
    pub upload_bytes: u64,
    /// Host time spent mapping/copying GPU input buffers.
    pub upload_us: u64,
    /// Host time spent recording descriptor/dispatch commands.
    pub command_record_us: u64,
    /// CPU queue-lock plus `vkQueueSubmit` duration.
    pub queue_submit_us: u64,
    /// Measured completion-fence wait after submission.
    pub gpu_completion_wait_us: u64,
    /// Host memory map/invalidate/copy/unmap duration.
    pub readback_map_copy_us: u64,
    /// Host time adapting the full or regional readback into renderer payloads.
    pub payload_pack_us: u64,
    /// Full-RGBA wrapping/hash time, when a full request was performed.
    pub full_rgba_pack_us: Option<u64>,
    /// Tight dirty-region wrapping time, when a dirty request was performed.
    pub dirty_region_pack_us: Option<u64>,
    /// Raw protocol packetization time; `None` at this renderer-only boundary.
    pub raw_packet_pack_us: Option<u64>,
    /// PNG protocol packetization time; `None` at this renderer-only boundary.
    pub png_packet_pack_us: Option<u64>,
    /// Complete renderer call wall time.
    pub total_us: u64,
    /// Number of shader pixel invocations requested before workgroup rounding.
    pub requested_pixels: u64,
    /// GPU target allocation reused for this result.
    pub target_allocation_id: u64,
}

/// Renderer-neutral input for one no-window Slug GPU render.
#[derive(Clone, Debug)]
pub struct GpuSlugRenderRequest<'a> {
    /// Authoritative visible terminal state.
    ///
    /// Combining scalars and the requested cursor shape are consumed directly
    /// from the bounded renderer-neutral snapshot model.
    pub snapshot: &'a TerminalSnapshot,
    /// Native output width. Java must present without bitmap stretching.
    pub width: u32,
    /// Native output height. Java must present without bitmap stretching.
    pub height: u32,
    /// Requested em size in physical pixels.
    pub font_pixel_size: f32,
    /// Canonical cell damage. Ignored when `force_full_refresh` is true.
    pub damage: &'a [CellRect],
    /// Force complete image reconstruction.
    pub force_full_refresh: bool,
    /// Correlation fields preserved in the result.
    pub identity: GpuSlugRenderIdentity,
    /// Bounded completion deadline.
    pub timeout: Duration,
    /// Optional cooperative cancellation token.
    pub cancellation: Option<&'a VulkanOffscreenCancellation>,
}

impl<'a> GpuSlugRenderRequest<'a> {
    /// Construct a complete refresh with the standard GPU deadline.
    #[must_use]
    pub fn full(
        snapshot: &'a TerminalSnapshot,
        width: u32,
        height: u32,
        font_pixel_size: f32,
        identity: GpuSlugRenderIdentity,
    ) -> Self {
        Self {
            snapshot,
            width,
            height,
            font_pixel_size,
            damage: &[],
            force_full_refresh: true,
            identity,
            timeout: DEFAULT_RENDER_TIMEOUT,
            cancellation: None,
        }
    }
}

/// Deterministic no-window Vulkan readback and dirty packetization source.
#[derive(Clone, Debug, PartialEq)]
pub struct GpuSlugRenderResult {
    /// Stable renderer identity.
    pub renderer_id: &'static str,
    /// Exact embedded face identity.
    pub font_id: &'static str,
    /// Exact embedded face hash.
    pub font_sha256: &'static str,
    /// Native output width.
    pub width: u32,
    /// Native output height.
    pub height: u32,
    /// Tight full-frame RGBA8 bytes for full raw/PNG encoding.
    ///
    /// This is deliberately absent for dirty requests: the dirty path never
    /// copies the complete target from GPU memory merely to crop it on the CPU.
    pub full_rgba: Option<Vec<u8>>,
    /// Tight canonical regions for dirty raw packetization.
    pub dirty_regions: Vec<GpuSlugDirtyRegion>,
    /// Native font and cell metrics.
    pub metrics: GpuSlugSurfaceMetrics,
    /// Correlation fields copied from the request.
    pub identity: GpuSlugRenderIdentity,
    /// FNV-1a hash of the complete deterministic RGBA readback.
    ///
    /// A dirty result has no complete image and therefore no full visual hash.
    pub visual_hash: Option<u64>,
    /// Per-render stage evidence.
    pub stats: GpuSlugRenderStats,
    /// Retained resource counters after the render.
    pub cache: GpuSlugCacheStats,
}

/// Typed construction, validation, Vulkan, and shader failures.
#[derive(Debug)]
pub enum GpuSlugRendererError {
    /// The no-window Vulkan runtime rejected an operation.
    Vulkan(VulkanOffscreenError),
    /// Font outline or directional-band data was invalid.
    Slug(SlugError),
    /// A request violated the bounded renderer contract.
    InvalidRequest { detail: String },
    /// More distinct glyphs were requested than the bounded cache can represent.
    GlyphCapacity { requested: usize, capacity: usize },
    /// A retained GPU buffer would exceed its documented bound.
    BufferCapacity {
        resource: &'static str,
        requested: usize,
        maximum: usize,
    },
    /// Vulkan resource construction failed.
    Allocation {
        resource: &'static str,
        source: vk::Result,
    },
    /// Vulkan shader/pipeline/descriptor work failed.
    Operation {
        operation: &'static str,
        source: vk::Result,
    },
}

impl fmt::Display for GpuSlugRendererError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Vulkan(error) => write!(f, "rust-gpu-slug Vulkan failure: {error}"),
            Self::Slug(error) => write!(f, "rust-gpu-slug geometry failure: {error}"),
            Self::InvalidRequest { detail } => {
                write!(f, "invalid rust-gpu-slug render request: {detail}")
            }
            Self::GlyphCapacity {
                requested,
                capacity,
            } => write!(
                f,
                "rust-gpu-slug frame needs {requested} distinct glyphs; bounded capacity is {capacity}"
            ),
            Self::BufferCapacity {
                resource,
                requested,
                maximum,
            } => write!(
                f,
                "rust-gpu-slug {resource} needs {requested} bytes; bounded maximum is {maximum}"
            ),
            Self::Allocation { resource, source } => {
                write!(f, "failed to allocate rust-gpu-slug {resource}: {source:?}")
            }
            Self::Operation { operation, source } => {
                write!(f, "rust-gpu-slug Vulkan {operation} failed: {source:?}")
            }
        }
    }
}

impl std::error::Error for GpuSlugRendererError {}

impl From<VulkanOffscreenError> for GpuSlugRendererError {
    fn from(value: VulkanOffscreenError) -> Self {
        Self::Vulkan(value)
    }
}

impl From<SlugError> for GpuSlugRendererError {
    fn from(value: SlugError) -> Self {
        Self::Slug(value)
    }
}

/// Process-retained immutable Slug pipeline/font/runtime shared by independent sessions.
#[derive(Clone)]
pub struct GpuSlugRuntime {
    context: VulkanOffscreenContext,
    font: Arc<SlugFont<'static>>,
    shared: Arc<GpuSlugShared>,
}

impl fmt::Debug for GpuSlugRuntime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GpuSlugRuntime")
            .field("renderer_id", &RUST_GPU_SLUG_RENDERER_ID)
            .field("device", self.context.device_facts())
            .field("shared_pipeline_owners", &Arc::strong_count(&self.shared))
            .finish_non_exhaustive()
    }
}

/// One independent mutable render session created cheaply from [`GpuSlugRuntime`].
pub struct GpuSlugRenderer {
    runtime: GpuSlugRuntime,
    session: VulkanOffscreenSession,
    state: PipelineState,
}

impl fmt::Debug for GpuSlugRenderer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GpuSlugRenderer")
            .field("renderer_id", &RUST_GPU_SLUG_RENDERER_ID)
            .field("device", self.runtime.context.device_facts())
            .field("session_id", &self.session.id())
            .field("retained_glyphs", &self.state.glyphs.len())
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug)]
struct GlyphEntry {
    slot: u32,
    last_used: u64,
}

struct PipelineState {
    shared: Arc<GpuSlugShared>,
    descriptor_pool: vk::DescriptorPool,
    descriptor_set: vk::DescriptorSet,
    buffers: [GpuBuffer; 6],
    glyphs: HashMap<char, GlyphEntry>,
    metadata_words: Vec<u32>,
    curve_records: Vec<[f32; 4]>,
    band_words: Vec<u32>,
    uploaded_metadata_words: usize,
    uploaded_curve_records: usize,
    uploaded_band_words: usize,
    use_sequence: u64,
    geometry_hits: u64,
    geometry_misses: u64,
    geometry_evictions: u64,
    outline_extraction_us: u64,
    directional_band_build_us: u64,
    buffer_allocations: u64,
}

struct GpuSlugShared {
    context: VulkanOffscreenContext,
    device: ash::Device,
    memory_properties: vk::PhysicalDeviceMemoryProperties,
    shader_module: vk::ShaderModule,
    descriptor_set_layout: vk::DescriptorSetLayout,
    pipeline_layout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
}

#[derive(Clone, Copy)]
struct GpuBuffer {
    label: &'static str,
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    capacity: usize,
}

enum CompletedReadback {
    Full(VulkanOffscreenReadback),
    Regional(VulkanOffscreenRegionalReadback),
}

struct PreparedPayload {
    width: u32,
    height: u32,
    full_rgba: Option<Vec<u8>>,
    dirty_regions: Vec<GpuSlugDirtyRegion>,
    visual_hash: Option<u64>,
}

impl CompletedReadback {
    const fn timings(&self) -> VulkanOffscreenStageTimings {
        match self {
            Self::Full(readback) => readback.timings,
            Self::Regional(readback) => readback.timings,
        }
    }
}

impl GpuBuffer {
    const fn empty(label: &'static str) -> Self {
        Self {
            label,
            buffer: vk::Buffer::null(),
            memory: vk::DeviceMemory::null(),
            capacity: 0,
        }
    }
}

impl GpuSlugRuntime {
    /// Construct the process-retained shader/pipeline/layout and immutable font face once.
    ///
    /// # Errors
    ///
    /// Returns a typed font, Vulkan allocation, shader, or pipeline error.
    pub fn new(context: VulkanOffscreenContext) -> Result<Self, GpuSlugRendererError> {
        let _runtime_span = tracing::info_span!(
            "gpu_slug_runtime_create",
            renderer_id = RUST_GPU_SLUG_RENDERER_ID
        )
        .entered();
        let font = Arc::new(SlugFont::embedded()?);
        let shared = Arc::new(GpuSlugShared::new(context.clone())?);
        Ok(Self {
            context,
            font,
            shared,
        })
    }

    /// Create independent mutable descriptor/buffer/cache/target state without rebuilding shaders.
    ///
    /// # Errors
    ///
    /// Returns a typed descriptor or retained-buffer allocation error.
    pub fn create_renderer(&self) -> Result<GpuSlugRenderer, GpuSlugRendererError> {
        Ok(GpuSlugRenderer {
            runtime: self.clone(),
            session: self.context.create_session(),
            state: PipelineState::new(Arc::clone(&self.shared))?,
        })
    }

    /// Number of owners sharing the immutable process pipeline, useful for lifecycle evidence.
    #[must_use]
    pub fn shared_pipeline_owners(&self) -> usize {
        Arc::strong_count(&self.shared)
    }

    /// Device facts captured by the true no-window Vulkan context.
    #[must_use]
    pub fn device_facts(&self) -> &crate::VulkanDeviceFacts {
        self.context.device_facts()
    }

    /// Derive native cell metrics directly from the retained Slug outline face.
    ///
    /// This performs no `fontdue` construction, glyph rasterization, Vulkan
    /// allocation, or pipeline work and is suitable for Vox surface negotiation.
    ///
    /// # Errors
    ///
    /// Returns [`GpuSlugRendererError::InvalidRequest`] for an out-of-range
    /// size, or a typed Slug error if the embedded metric glyph is unavailable.
    pub fn surface_metrics(
        &self,
        font_pixel_size: f32,
    ) -> Result<GpuSlugSurfaceMetrics, GpuSlugRendererError> {
        surface_metrics(&self.font, font_pixel_size)
    }

    /// Metrics at [`GPU_SLUG_DEFAULT_PIXEL_SIZE`].
    ///
    /// # Errors
    ///
    /// Returns a typed Slug error if the embedded metric glyph is unavailable.
    pub fn default_surface_metrics(&self) -> Result<GpuSlugSurfaceMetrics, GpuSlugRendererError> {
        self.surface_metrics(GPU_SLUG_DEFAULT_PIXEL_SIZE)
    }

    /// Match the CPU bridge policy for fitting a logical grid into a panel.
    ///
    /// Zero panel dimensions select the default grid extent. The chosen em
    /// size is `default * min(width_scale, height_scale)` clamped to the shared
    /// bridge bounds; callers then use `metrics.native_width/height` so Java
    /// presents a crisp native raster rather than stretching the panel size.
    ///
    /// # Errors
    ///
    /// Returns a typed request error for an empty grid or a typed metric error.
    pub fn fit_surface_metrics(
        &self,
        columns: u16,
        rows: u16,
        panel_width: u32,
        panel_height: u32,
    ) -> Result<GpuSlugSurfaceMetrics, GpuSlugRendererError> {
        if columns == 0 || rows == 0 {
            return Err(GpuSlugRendererError::InvalidRequest {
                detail: "logical terminal grid must be non-zero".to_owned(),
            });
        }
        let defaults = self.default_surface_metrics()?;
        let default_width = defaults.native_width(columns).max(1);
        let default_height = defaults.native_height(rows).max(1);
        let target_width = if panel_width == 0 {
            default_width
        } else {
            panel_width
        };
        let target_height = if panel_height == 0 {
            default_height
        } else {
            panel_height
        };
        let width_scale = f64::from(target_width) / f64::from(default_width);
        let height_scale = f64::from(target_height) / f64::from(default_height);
        let mut pixel_size = (f64::from(GPU_SLUG_DEFAULT_PIXEL_SIZE)
            * width_scale.min(height_scale))
        .clamp(
            f64::from(GPU_SLUG_MIN_PIXEL_SIZE),
            f64::from(GPU_SLUG_MAX_PIXEL_SIZE),
        )
        .to_f32()
        .ok_or_else(|| GpuSlugRendererError::InvalidRequest {
            detail: "fitted font pixel size is not representable as f32".to_owned(),
        })?;
        for _ in 0..8 {
            let fitted = self.surface_metrics(pixel_size)?;
            let width = fitted.native_width(columns);
            let height = fitted.native_height(rows);
            if width <= target_width && height <= target_height {
                return Ok(fitted);
            }
            let shrink = (f64::from(target_width) / f64::from(width.max(1)))
                .min(f64::from(target_height) / f64::from(height.max(1)));
            let next = (f64::from(pixel_size) * shrink * 0.999)
                .max(f64::from(GPU_SLUG_MIN_PIXEL_SIZE))
                .to_f32()
                .ok_or_else(|| GpuSlugRendererError::InvalidRequest {
                    detail: "fitted font pixel size is not representable as f32".to_owned(),
                })?;
            if next >= pixel_size || (pixel_size - next).abs() < f32::EPSILON {
                break;
            }
            pixel_size = next;
        }
        Err(GpuSlugRendererError::InvalidRequest {
            detail: format!(
                "logical terminal grid {columns}x{rows} cannot fit panel {target_width}x{target_height}"
            ),
        })
    }
}

impl GpuSlugRenderer {
    /// Convenience constructor for a standalone one-session runtime.
    ///
    /// Long-lived services should construct one [`GpuSlugRuntime`] and call
    /// [`GpuSlugRuntime::create_renderer`] for each terminal session.
    ///
    /// # Errors
    ///
    /// Returns a typed runtime, pipeline, descriptor, or buffer error.
    pub fn new(context: VulkanOffscreenContext) -> Result<Self, GpuSlugRendererError> {
        GpuSlugRuntime::new(context)?.create_renderer()
    }

    /// Device facts captured by the true no-window Vulkan context.
    #[must_use]
    pub fn device_facts(&self) -> &crate::VulkanDeviceFacts {
        self.runtime.context.device_facts()
    }

    /// Delegate cheap outline-only surface negotiation to the shared runtime.
    ///
    /// # Errors
    ///
    /// Returns a typed size or embedded-font metric error.
    pub fn surface_metrics(
        &self,
        font_pixel_size: f32,
    ) -> Result<GpuSlugSurfaceMetrics, GpuSlugRendererError> {
        self.runtime.surface_metrics(font_pixel_size)
    }

    /// Current retained geometry/buffer/target counters.
    #[must_use]
    pub fn cache_stats(&self) -> GpuSlugCacheStats {
        GpuSlugCacheStats {
            geometry_hits: self.state.geometry_hits,
            geometry_misses: self.state.geometry_misses,
            geometry_evictions: self.state.geometry_evictions,
            retained_glyphs: self.state.glyphs.len(),
            glyph_capacity: MAX_RETAINED_GLYPHS,
            buffer_allocations: self.state.buffer_allocations,
            targets: self.runtime.context.cache_stats(),
        }
    }

    /// Render authoritative terminal cells entirely through the Slug compute shader.
    ///
    /// # Errors
    ///
    /// Returns a typed request, capacity, geometry, cancellation, timeout,
    /// Vulkan allocation, submission, device-loss, or readback error. No CPU
    /// raster fallback is attempted.
    #[expect(
        clippy::too_many_lines,
        reason = "the public render transaction keeps one ordered telemetry/ownership boundary"
    )]
    pub fn render(
        &mut self,
        request: GpuSlugRenderRequest<'_>,
    ) -> Result<GpuSlugRenderResult, GpuSlugRendererError> {
        let total_start = Instant::now();
        #[cfg(feature = "tracy")]
        let _frame_span = info_span!(
            "gpu_slug_render",
            renderer_id = RUST_GPU_SLUG_RENDERER_ID,
            width = request.width,
            height = request.height,
            columns = request.snapshot.columns,
            rows = request.snapshot.rows,
            frame_sequence = request.identity.frame_sequence,
            presentation_generation = %request.identity.presentation_generation,
        )
        .entered();
        validate_request(&request)?;
        let metrics = surface_metrics(&self.runtime.font, request.font_pixel_size)?;
        let pixel_regions = canonical_pixel_regions(&request, metrics)?;

        #[cfg(feature = "tracy")]
        let geometry_span = info_span!("gpu_slug_geometry").entered();
        let geometry_start = Instant::now();
        let outline_extraction_before = self.state.outline_extraction_us;
        let directional_band_before = self.state.directional_band_build_us;
        let unique = unique_render_characters(request.snapshot);
        if unique.len() > MAX_RETAINED_GLYPHS {
            return Err(GpuSlugRendererError::GlyphCapacity {
                requested: unique.len(),
                capacity: MAX_RETAINED_GLYPHS,
            });
        }
        if unique
            .iter()
            .any(|character| !self.state.glyphs.contains_key(character))
            && self.state.glyphs.len().saturating_add(unique.len()) > MAX_RETAINED_GLYPHS
        {
            self.state.reset_geometry_cache();
        }
        for character in unique {
            self.state.ensure_glyph(&self.runtime.font, character)?;
        }
        let geometry_build_us = elapsed_us(geometry_start);
        let outline_stage_us = self
            .state
            .outline_extraction_us
            .saturating_sub(outline_extraction_before);
        let band_stage_us = self
            .state
            .directional_band_build_us
            .saturating_sub(directional_band_before);
        #[cfg(feature = "tracy")]
        drop(geometry_span);

        let cells = build_cell_words(request.snapshot, &mut self.state)?;
        let regions = pixel_region_words(&pixel_regions);
        let frame = frame_words(
            request.snapshot,
            request.width,
            request.height,
            metrics,
            self.runtime.font.metrics(),
            pixel_regions.len(),
        );

        #[cfg(feature = "tracy")]
        let upload_span = info_span!("gpu_slug_upload").entered();
        let upload_start = Instant::now();
        let mut upload_bytes = 0_u64;
        upload_bytes =
            upload_bytes.saturating_add(self.state.upload_frame_data(&cells, &regions, &frame)?);
        upload_bytes = upload_bytes.saturating_add(self.state.upload_new_geometry()?);
        let upload_us = elapsed_us(upload_start);
        #[cfg(feature = "tracy")]
        drop(upload_span);

        let target = self.session.target(request.width, request.height)?;
        let target_allocation_id = target.allocation_id();
        let max_region_width = pixel_regions
            .iter()
            .map(|region| region.width)
            .max()
            .unwrap_or(1);
        let max_region_height = pixel_regions
            .iter()
            .map(|region| region.height)
            .max()
            .unwrap_or(1);
        let group_count_x = max_region_width.div_ceil(8);
        let group_count_y = max_region_height.div_ceil(8);
        let group_count_z = u32::try_from(pixel_regions.len()).map_err(|_conversion_error| {
            GpuSlugRendererError::InvalidRequest {
                detail: "damage region count does not fit a Vulkan dispatch".to_owned(),
            }
        })?;
        let requested_pixels = pixel_regions.iter().fold(0_u64, |total, region| {
            total.saturating_add(u64::from(region.width) * u64::from(region.height))
        });

        let readback_regions = pixel_regions
            .iter()
            .map(|region| VulkanOffscreenRegion {
                x: region.x,
                y: region.y,
                width: region.width,
                height: region.height,
            })
            .collect::<Vec<_>>();
        let full_readback = request.force_full_refresh || request.damage.is_empty();
        #[cfg(feature = "tracy")]
        let raster_span = info_span!("gpu_slug_raster_readback").entered();
        let pipeline_state = &self.state;
        let completed_readback = if full_readback {
            let readback = target.record_compute_and_readback(
                request.timeout,
                request.cancellation,
                |recording| {
                    record_slug_dispatch(
                        pipeline_state,
                        recording,
                        request.width,
                        request.height,
                        group_count_x,
                        group_count_y,
                        group_count_z,
                    )
                },
            )?;
            CompletedReadback::Full(readback)
        } else {
            let readback = target.record_compute_and_readback_regions(
                &readback_regions,
                request.timeout,
                request.cancellation,
                |recording| {
                    record_slug_dispatch(
                        pipeline_state,
                        recording,
                        request.width,
                        request.height,
                        group_count_x,
                        group_count_y,
                        group_count_z,
                    )
                },
            )?;
            CompletedReadback::Regional(readback)
        };
        #[cfg(feature = "tracy")]
        drop(raster_span);

        let operation_timings = completed_readback.timings();
        #[cfg(feature = "tracy")]
        let pack_span = info_span!("gpu_slug_payload_pack").entered();
        let payload_pack_start = Instant::now();
        let payload = prepare_payload(completed_readback);
        let payload_pack_us = elapsed_us(payload_pack_start);
        let (full_pack_stage, dirty_pack_stage) = if full_readback {
            (Some(payload_pack_us), None)
        } else {
            (None, Some(payload_pack_us))
        };
        #[cfg(feature = "tracy")]
        drop(pack_span);
        let render_stats = GpuSlugRenderStats {
            geometry_build_us,
            outline_extraction_us: outline_stage_us,
            directional_band_build_us: band_stage_us,
            upload_bytes,
            upload_us,
            command_record_us: operation_timings.command_record_us,
            queue_submit_us: operation_timings.queue_submit_us,
            gpu_completion_wait_us: operation_timings.gpu_completion_wait_us,
            readback_map_copy_us: operation_timings.readback_map_copy_us,
            payload_pack_us,
            full_rgba_pack_us: full_pack_stage,
            dirty_region_pack_us: dirty_pack_stage,
            raw_packet_pack_us: None,
            png_packet_pack_us: None,
            total_us: elapsed_us(total_start),
            requested_pixels,
            target_allocation_id,
        };
        #[cfg(feature = "extended_observability")]
        tracing::debug!(
            target: "teamy_terminal::gpu_slug",
            geometry_build_us = render_stats.geometry_build_us,
            outline_extraction_us = render_stats.outline_extraction_us,
            directional_band_build_us = render_stats.directional_band_build_us,
            upload_bytes = render_stats.upload_bytes,
            command_record_us = render_stats.command_record_us,
            queue_submit_us = render_stats.queue_submit_us,
            gpu_completion_wait_us = render_stats.gpu_completion_wait_us,
            readback_map_copy_us = render_stats.readback_map_copy_us,
            payload_pack_us = render_stats.payload_pack_us,
            full_rgba_pack_us = ?render_stats.full_rgba_pack_us,
            dirty_region_pack_us = ?render_stats.dirty_region_pack_us,
            raw_packet_pack_us = ?render_stats.raw_packet_pack_us,
            png_packet_pack_us = ?render_stats.png_packet_pack_us,
            requested_pixels = render_stats.requested_pixels,
            geometry_hits = self.state.geometry_hits,
            geometry_misses = self.state.geometry_misses,
            "completed retained GPU Slug raster/readback"
        );
        #[cfg(feature = "tracy")]
        tracing::info!(message = "finished frame", tracy.frame_mark = true);
        Ok(GpuSlugRenderResult {
            renderer_id: RUST_GPU_SLUG_RENDERER_ID,
            font_id: EMBEDDED_FONT_ID,
            font_sha256: EMBEDDED_FONT_SHA256,
            width: payload.width,
            height: payload.height,
            full_rgba: payload.full_rgba,
            dirty_regions: payload.dirty_regions,
            metrics,
            identity: request.identity,
            visual_hash: payload.visual_hash,
            stats: render_stats,
            cache: self.cache_stats(),
        })
    }
}

fn record_slug_dispatch(
    state: &PipelineState,
    recording: &VulkanOffscreenRecording<'_>,
    width: u32,
    height: u32,
    group_count_x: u32,
    group_count_y: u32,
    group_count_z: u32,
) -> Result<(), VulkanOffscreenError> {
    if recording.extent().width != width
        || recording.extent().height != height
        || recording.format() != vk::Format::R8G8B8A8_UNORM
    {
        return Err(VulkanOffscreenError::Operation {
            operation: "slug target ABI validation",
            source: vk::Result::ERROR_FORMAT_NOT_SUPPORTED,
        });
    }
    state.update_descriptors(recording);
    // SAFETY: The retained pipeline belongs to the same live device/command buffer.
    unsafe {
        recording.device().cmd_bind_pipeline(
            recording.command_buffer(),
            vk::PipelineBindPoint::COMPUTE,
            state.shared.pipeline,
        );
    };
    // SAFETY: The retained layout/set belong to this pipeline and all ranges are valid.
    unsafe {
        recording.device().cmd_bind_descriptor_sets(
            recording.command_buffer(),
            vk::PipelineBindPoint::COMPUTE,
            state.shared.pipeline_layout,
            0,
            &[state.descriptor_set],
            &[],
        );
    };
    // SAFETY: The bound pipeline/set and target remain live for this bounded dispatch.
    unsafe {
        recording.device().cmd_dispatch(
            recording.command_buffer(),
            group_count_x,
            group_count_y,
            group_count_z,
        );
    };
    Ok(())
}

fn prepare_payload(readback: CompletedReadback) -> PreparedPayload {
    match readback {
        CompletedReadback::Full(readback) => PreparedPayload {
            width: readback.width,
            height: readback.height,
            visual_hash: Some(fnv1a64(&readback.rgba)),
            full_rgba: Some(readback.rgba),
            dirty_regions: Vec::new(),
        },
        CompletedReadback::Regional(readback) => PreparedPayload {
            width: readback.frame_width,
            height: readback.frame_height,
            full_rgba: None,
            dirty_regions: readback
                .regions
                .into_iter()
                .map(|region| GpuSlugDirtyRegion {
                    x: region.region.x,
                    y: region.region.y,
                    width: region.region.width,
                    height: region.region.height,
                    rgba: region.rgba,
                })
                .collect(),
            visual_hash: None,
        },
    }
}

impl GpuSlugShared {
    fn new(context: VulkanOffscreenContext) -> Result<Self, GpuSlugRendererError> {
        let access = context.device_access();
        if access.queue_family_index() != context.device_facts().queue_family_index
            || access.target_format() != vk::Format::R8G8B8A8_UNORM
        {
            return Err(GpuSlugRendererError::InvalidRequest {
                detail: "retained Vulkan context does not expose the required compute/rgba8 ABI"
                    .to_owned(),
            });
        }
        let device = access.device().clone();
        // SAFETY: The physical device belongs to the retained live instance.
        let memory_properties = unsafe {
            access
                .instance()
                .get_physical_device_memory_properties(access.physical_device())
        };
        let shader_info = vk::ShaderModuleCreateInfo::default().code(GPU_SLUG_SHADER_WORDS);
        // SAFETY: Naga emitted and validated the embedded SPIR-V at build time.
        let shader_module =
            unsafe { device.create_shader_module(&shader_info, None) }.map_err(|source| {
                GpuSlugRendererError::Allocation {
                    resource: "shader module",
                    source,
                }
            })?;
        let bindings = [
            descriptor_binding(0, vk::DescriptorType::STORAGE_IMAGE),
            descriptor_binding(1, vk::DescriptorType::STORAGE_BUFFER),
            descriptor_binding(2, vk::DescriptorType::STORAGE_BUFFER),
            descriptor_binding(3, vk::DescriptorType::STORAGE_BUFFER),
            descriptor_binding(4, vk::DescriptorType::STORAGE_BUFFER),
            descriptor_binding(5, vk::DescriptorType::STORAGE_BUFFER),
            descriptor_binding(6, vk::DescriptorType::STORAGE_BUFFER),
        ];
        let layout_info = vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings);
        // SAFETY: The binding array remains alive during this call.
        let descriptor_set_layout =
            match unsafe { device.create_descriptor_set_layout(&layout_info, None) } {
                Ok(layout) => layout,
                Err(source) => {
                    // SAFETY: The shader module is live and has no dependents.
                    unsafe { device.destroy_shader_module(shader_module, None) };
                    return Err(GpuSlugRendererError::Allocation {
                        resource: "descriptor set layout",
                        source,
                    });
                }
            };
        let set_layouts = [descriptor_set_layout];
        let pipeline_layout_info =
            vk::PipelineLayoutCreateInfo::default().set_layouts(&set_layouts);
        // SAFETY: The descriptor layout is live and compatible with the shader ABI.
        let pipeline_layout = unsafe { device.create_pipeline_layout(&pipeline_layout_info, None) }
            .map_err(|source| GpuSlugRendererError::Allocation {
                resource: "pipeline layout",
                source,
            })?;
        let entry = CString::new(GPU_SLUG_SHADER_ENTRY).map_err(|_nul_error| {
            GpuSlugRendererError::InvalidRequest {
                detail: "generated shader entry point contains NUL".to_owned(),
            }
        })?;
        let stage = vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::COMPUTE)
            .module(shader_module)
            .name(&entry);
        let compute_info = [vk::ComputePipelineCreateInfo::default()
            .stage(stage)
            .layout(pipeline_layout)];
        // SAFETY: Shader module and pipeline layout are live and Naga validated the entry point.
        let pipeline = unsafe {
            device.create_compute_pipelines(vk::PipelineCache::null(), &compute_info, None)
        }
        .map_err(|(_, source)| GpuSlugRendererError::Operation {
            operation: "compute-pipeline creation",
            source,
        })?
        .into_iter()
        .next()
        .ok_or(GpuSlugRendererError::Operation {
            operation: "compute-pipeline creation returned no pipeline",
            source: vk::Result::ERROR_INITIALIZATION_FAILED,
        })?;
        Ok(Self {
            context,
            device,
            memory_properties,
            shader_module,
            descriptor_set_layout,
            pipeline_layout,
            pipeline,
        })
    }
}

impl PipelineState {
    fn new(shared: Arc<GpuSlugShared>) -> Result<Self, GpuSlugRendererError> {
        let pool_sizes = [
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::STORAGE_IMAGE)
                .descriptor_count(1),
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::STORAGE_BUFFER)
                .descriptor_count(6),
        ];
        let pool_info = vk::DescriptorPoolCreateInfo::default()
            .max_sets(1)
            .pool_sizes(&pool_sizes);
        // SAFETY: Pool sizes exactly cover the one retained descriptor set.
        let descriptor_pool = unsafe { shared.device.create_descriptor_pool(&pool_info, None) }
            .map_err(|source| GpuSlugRendererError::Allocation {
                resource: "descriptor pool",
                source,
            })?;
        let set_layouts = [shared.descriptor_set_layout];
        let allocate_info = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(descriptor_pool)
            .set_layouts(&set_layouts);
        // SAFETY: The pool and set layout are live.
        let descriptor_set = unsafe { shared.device.allocate_descriptor_sets(&allocate_info) }
            .map_err(|source| GpuSlugRendererError::Allocation {
                resource: "descriptor set",
                source,
            })?
            .into_iter()
            .next()
            .ok_or(GpuSlugRendererError::Allocation {
                resource: "descriptor set",
                source: vk::Result::ERROR_INITIALIZATION_FAILED,
            })?;
        let mut state = Self {
            shared,
            descriptor_pool,
            descriptor_set,
            buffers: [
                GpuBuffer::empty("cell buffer"),
                GpuBuffer::empty("glyph metadata buffer"),
                GpuBuffer::empty("curve buffer"),
                GpuBuffer::empty("band buffer"),
                GpuBuffer::empty("region buffer"),
                GpuBuffer::empty("frame buffer"),
            ],
            glyphs: HashMap::new(),
            metadata_words: Vec::new(),
            curve_records: Vec::new(),
            band_words: Vec::new(),
            uploaded_metadata_words: 0,
            uploaded_curve_records: 0,
            uploaded_band_words: 0,
            use_sequence: 0,
            geometry_hits: 0,
            geometry_misses: 0,
            geometry_evictions: 0,
            outline_extraction_us: 0,
            directional_band_build_us: 0,
            buffer_allocations: 0,
        };
        for index in 0..state.buffers.len() {
            let _ = state.ensure_buffer_capacity(index, MIN_GPU_BUFFER_BYTES)?;
        }
        Ok(state)
    }

    fn ensure_glyph(
        &mut self,
        font: &SlugFont<'_>,
        character: char,
    ) -> Result<u32, GpuSlugRendererError> {
        self.use_sequence = self.use_sequence.saturating_add(1);
        if let Some(entry) = self.glyphs.get_mut(&character) {
            entry.last_used = self.use_sequence;
            self.geometry_hits = self.geometry_hits.saturating_add(1);
            return Ok(entry.slot);
        }
        self.geometry_misses = self.geometry_misses.saturating_add(1);
        let outline_start = Instant::now();
        let geometry = font.glyph_geometry(character)?;
        self.outline_extraction_us = self
            .outline_extraction_us
            .saturating_add(elapsed_us(outline_start));
        let directional_band_start = Instant::now();
        let directional = build_directional_bands(
            &geometry.curves,
            geometry.bounds,
            DEFAULT_BAND_SIZE_FONT_UNITS,
        )?;
        self.directional_band_build_us = self
            .directional_band_build_us
            .saturating_add(elapsed_us(directional_band_start));
        let curve_start =
            u32::try_from(self.curve_records.len() / 2).map_err(|_conversion_error| {
                GpuSlugRendererError::BufferCapacity {
                    resource: "curve arena",
                    requested: self.curve_records.len(),
                    maximum: MAX_GPU_BUFFER_BYTES,
                }
            })?;
        let band_start = u32::try_from(self.band_words.len()).map_err(|_conversion_error| {
            GpuSlugRendererError::BufferCapacity {
                resource: "band arena",
                requested: self.band_words.len(),
                maximum: MAX_GPU_BUFFER_BYTES,
            }
        })?;
        let metadata = build_gpu_glyph_metadata(
            &geometry,
            &directional,
            font.metrics(),
            curve_start,
            band_start,
        )?;
        for curve in &geometry.curves {
            self.curve_records.extend(curve.gpu_vec4s());
        }
        self.band_words
            .extend(directional.packed_words(geometry.curves.len())?);
        let slot = u32::try_from(self.metadata_words.len() / GPU_GLYPH_METADATA_WORDS).map_err(
            |_conversion_error| GpuSlugRendererError::GlyphCapacity {
                requested: self.glyphs.len().saturating_add(1),
                capacity: MAX_RETAINED_GLYPHS,
            },
        )?;
        self.metadata_words.extend(metadata.into_words());
        let _ = self.glyphs.insert(
            character,
            GlyphEntry {
                slot,
                last_used: self.use_sequence,
            },
        );
        Ok(slot)
    }

    fn reset_geometry_cache(&mut self) {
        self.geometry_evictions = self
            .geometry_evictions
            .saturating_add(u64::try_from(self.glyphs.len()).unwrap_or(u64::MAX));
        self.glyphs.clear();
        self.metadata_words.clear();
        self.curve_records.clear();
        self.band_words.clear();
        self.uploaded_metadata_words = 0;
        self.uploaded_curve_records = 0;
        self.uploaded_band_words = 0;
    }

    fn upload_frame_data(
        &mut self,
        cells: &[u32],
        regions: &[u32],
        frame: &[u32; FRAME_WORDS],
    ) -> Result<u64, GpuSlugRendererError> {
        let mut bytes = 0_u64;
        bytes = bytes.saturating_add(self.replace_buffer_words(0, cells)?);
        bytes = bytes.saturating_add(self.replace_buffer_words(4, regions)?);
        bytes = bytes.saturating_add(self.replace_buffer_words(5, frame)?);
        Ok(bytes)
    }

    fn upload_new_geometry(&mut self) -> Result<u64, GpuSlugRendererError> {
        let mut bytes = 0_u64;
        let metadata_start = self.uploaded_metadata_words;
        let metadata = self.metadata_words[metadata_start..].to_vec();
        bytes = bytes.saturating_add(self.append_buffer_words(1, metadata_start, &metadata)?);
        self.uploaded_metadata_words = self.metadata_words.len();

        let curve_start = self.uploaded_curve_records;
        let curves = self.curve_records[curve_start..].to_vec();
        bytes = bytes.saturating_add(self.append_curve_records(2, curve_start, &curves)?);
        self.uploaded_curve_records = self.curve_records.len();

        let band_start = self.uploaded_band_words;
        let bands = self.band_words[band_start..].to_vec();
        bytes = bytes.saturating_add(self.append_buffer_words(3, band_start, &bands)?);
        self.uploaded_band_words = self.band_words.len();
        Ok(bytes)
    }

    fn replace_buffer_words(
        &mut self,
        index: usize,
        words: &[u32],
    ) -> Result<u64, GpuSlugRendererError> {
        let byte_len = words
            .len()
            .saturating_mul(size_of::<u32>())
            .max(MIN_GPU_BUFFER_BYTES);
        let reallocated = self.ensure_buffer_capacity(index, byte_len)?;
        if words.is_empty() {
            self.upload_bytes(index, 0, &[0; MIN_GPU_BUFFER_BYTES])?;
            return Ok(MIN_GPU_BUFFER_BYTES as u64);
        }
        let bytes = u32_bytes(words);
        self.upload_bytes(index, 0, bytes)?;
        let _ = reallocated;
        Ok(u64::try_from(bytes.len()).unwrap_or(u64::MAX))
    }

    fn append_buffer_words(
        &mut self,
        index: usize,
        word_offset: usize,
        words: &[u32],
    ) -> Result<u64, GpuSlugRendererError> {
        let total_words = word_offset.saturating_add(words.len());
        let total_bytes = total_words
            .saturating_mul(size_of::<u32>())
            .max(MIN_GPU_BUFFER_BYTES);
        let reallocated = self.ensure_buffer_capacity(index, total_bytes)?;
        if reallocated {
            let all = match index {
                1 => self.metadata_words.clone(),
                3 => self.band_words.clone(),
                _ => Vec::new(),
            };
            let bytes = if all.is_empty() {
                vec![0; MIN_GPU_BUFFER_BYTES]
            } else {
                u32_bytes(&all).to_vec()
            };
            self.upload_bytes(index, 0, &bytes)?;
            return Ok(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
        }
        if words.is_empty() {
            return Ok(0);
        }
        let bytes = u32_bytes(words);
        self.upload_bytes(index, word_offset * size_of::<u32>(), bytes)?;
        Ok(u64::try_from(bytes.len()).unwrap_or(u64::MAX))
    }

    fn append_curve_records(
        &mut self,
        index: usize,
        record_offset: usize,
        records: &[[f32; 4]],
    ) -> Result<u64, GpuSlugRendererError> {
        let record_bytes = size_of::<[f32; 4]>();
        let total_bytes = record_offset
            .saturating_add(records.len())
            .saturating_mul(record_bytes)
            .max(MIN_GPU_BUFFER_BYTES);
        let reallocated = self.ensure_buffer_capacity(index, total_bytes)?;
        if reallocated {
            let bytes = if self.curve_records.is_empty() {
                vec![0; MIN_GPU_BUFFER_BYTES]
            } else {
                curve_bytes(&self.curve_records).to_vec()
            };
            self.upload_bytes(index, 0, &bytes)?;
            return Ok(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
        }
        if records.is_empty() {
            return Ok(0);
        }
        let bytes = curve_bytes(records);
        self.upload_bytes(index, record_offset * record_bytes, bytes)?;
        Ok(u64::try_from(bytes.len()).unwrap_or(u64::MAX))
    }

    fn ensure_buffer_capacity(
        &mut self,
        index: usize,
        requested: usize,
    ) -> Result<bool, GpuSlugRendererError> {
        if requested > MAX_GPU_BUFFER_BYTES {
            return Err(GpuSlugRendererError::BufferCapacity {
                resource: self.buffers[index].label,
                requested,
                maximum: MAX_GPU_BUFFER_BYTES,
            });
        }
        if self.buffers[index].capacity >= requested {
            return Ok(false);
        }
        let capacity = requested
            .max(MIN_GPU_BUFFER_BYTES)
            .checked_next_power_of_two()
            .unwrap_or(MAX_GPU_BUFFER_BYTES)
            .min(MAX_GPU_BUFFER_BYTES);
        let created = create_host_buffer(
            &self.shared.device,
            &self.shared.memory_properties,
            self.buffers[index].label,
            capacity,
        )?;
        let old = std::mem::replace(&mut self.buffers[index], created);
        // SAFETY: Synchronous rendering guarantees the replaced buffer is idle.
        unsafe { destroy_buffer(&self.shared.device, old) };
        self.buffer_allocations = self.buffer_allocations.saturating_add(1);
        Ok(true)
    }

    fn upload_bytes(
        &self,
        index: usize,
        offset: usize,
        bytes: &[u8],
    ) -> Result<(), GpuSlugRendererError> {
        let buffer = self.buffers[index];
        if offset.saturating_add(bytes.len()) > buffer.capacity {
            return Err(GpuSlugRendererError::BufferCapacity {
                resource: buffer.label,
                requested: offset.saturating_add(bytes.len()),
                maximum: buffer.capacity,
            });
        }
        // SAFETY: The allocation is HOST_VISIBLE|HOST_COHERENT, and the mapped range
        // is bounded by the retained allocation capacity.
        let mapped = unsafe {
            self.shared.device.map_memory(
                buffer.memory,
                u64::try_from(offset).unwrap_or(u64::MAX),
                u64::try_from(bytes.len()).unwrap_or(u64::MAX),
                vk::MemoryMapFlags::empty(),
            )
        }
        .map_err(|source| GpuSlugRendererError::Operation {
            operation: "input-buffer map",
            source,
        })?;
        // SAFETY: The destination is a valid mapped range of at least bytes.len().
        unsafe { ptr::copy_nonoverlapping(bytes.as_ptr(), mapped.cast::<u8>(), bytes.len()) };
        // SAFETY: This allocation is mapped exactly once by this serialized renderer.
        unsafe { self.shared.device.unmap_memory(buffer.memory) };
        Ok(())
    }

    fn update_descriptors(
        &self,
        recording: &crate::vulkan_offscreen::VulkanOffscreenRecording<'_>,
    ) {
        let image_info = [vk::DescriptorImageInfo::default()
            .image_view(recording.image_view())
            .image_layout(vk::ImageLayout::GENERAL)];
        let buffer_infos = self.buffers.map(|buffer| {
            [vk::DescriptorBufferInfo::default()
                .buffer(buffer.buffer)
                .offset(0)
                .range(u64::try_from(buffer.capacity).unwrap_or(u64::MAX))]
        });
        let mut writes = Vec::with_capacity(7);
        writes.push(
            vk::WriteDescriptorSet::default()
                .dst_set(self.descriptor_set)
                .dst_binding(0)
                .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                .image_info(&image_info),
        );
        for (index, info) in buffer_infos.iter().enumerate() {
            writes.push(
                vk::WriteDescriptorSet::default()
                    .dst_set(self.descriptor_set)
                    .dst_binding(u32::try_from(index + 1).unwrap_or(u32::MAX))
                    .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                    .buffer_info(info),
            );
        }
        // SAFETY: Every descriptor points at a live retained buffer/image view and
        // this renderer serializes updates against synchronous submission.
        unsafe { recording.device().update_descriptor_sets(&writes, &[]) };
    }
}

impl Drop for PipelineState {
    fn drop(&mut self) {
        for buffer in self.buffers {
            // SAFETY: The buffer is idle and owned by this state.
            unsafe { destroy_buffer(&self.shared.device, buffer) };
        }
        // SAFETY: The pool is idle and owned by this state.
        unsafe {
            self.shared
                .device
                .destroy_descriptor_pool(self.descriptor_pool, None);
        }
    }
}

impl Drop for GpuSlugShared {
    fn drop(&mut self) {
        // SAFETY: This is the last immutable pipeline owner. Session states retain
        // their own Arc, so their descriptors/buffers are already destroyed.
        unsafe { self.device.destroy_pipeline(self.pipeline, None) };
        // SAFETY: The pipeline has been destroyed and this layout is no longer referenced.
        unsafe {
            self.device
                .destroy_pipeline_layout(self.pipeline_layout, None);
        };
        // SAFETY: No descriptor sets remain and this layout is no longer referenced.
        unsafe {
            self.device
                .destroy_descriptor_set_layout(self.descriptor_set_layout, None);
        };
        // SAFETY: Pipeline creation completed and no pipeline now references this module.
        unsafe { self.device.destroy_shader_module(self.shader_module, None) };
        let _ = &self.context;
    }
}

fn descriptor_binding(
    binding: u32,
    ty: vk::DescriptorType,
) -> vk::DescriptorSetLayoutBinding<'static> {
    vk::DescriptorSetLayoutBinding::default()
        .binding(binding)
        .descriptor_type(ty)
        .descriptor_count(1)
        .stage_flags(vk::ShaderStageFlags::COMPUTE)
}

fn create_host_buffer(
    device: &ash::Device,
    memory_properties: &vk::PhysicalDeviceMemoryProperties,
    resource: &'static str,
    capacity: usize,
) -> Result<GpuBuffer, GpuSlugRendererError> {
    let create_info = vk::BufferCreateInfo::default()
        .size(u64::try_from(capacity).unwrap_or(u64::MAX))
        .usage(vk::BufferUsageFlags::STORAGE_BUFFER)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);
    // SAFETY: The device is live and the bounded create-info is complete.
    let buffer = unsafe { device.create_buffer(&create_info, None) }
        .map_err(|source| GpuSlugRendererError::Allocation { resource, source })?;
    // SAFETY: The buffer was created by this device and is not yet bound.
    let requirements = unsafe { device.get_buffer_memory_requirements(buffer) };
    let required = vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
    let Some(memory_type_index) = memory_properties
        .memory_types
        .iter()
        .enumerate()
        .find(|(index, memory_type)| {
            (requirements.memory_type_bits & (1_u32 << index)) != 0
                && memory_type.property_flags.contains(required)
        })
        .and_then(|(index, _)| u32::try_from(index).ok())
    else {
        // SAFETY: No memory was bound and the buffer is idle.
        unsafe { device.destroy_buffer(buffer, None) };
        return Err(GpuSlugRendererError::InvalidRequest {
            detail: format!(
                "device exposes no HOST_VISIBLE|HOST_COHERENT memory for {resource}; a staging upload path is required"
            ),
        });
    };
    let allocate_info = vk::MemoryAllocateInfo::default()
        .allocation_size(requirements.size)
        .memory_type_index(memory_type_index);
    // SAFETY: The selected memory type is compatible with the buffer requirements.
    let memory = match unsafe { device.allocate_memory(&allocate_info, None) } {
        Ok(memory) => memory,
        Err(source) => {
            // SAFETY: The unbound buffer is idle.
            unsafe { device.destroy_buffer(buffer, None) };
            return Err(GpuSlugRendererError::Allocation { resource, source });
        }
    };
    // SAFETY: The allocation satisfies the queried requirements.
    if let Err(source) = unsafe { device.bind_buffer_memory(buffer, memory, 0) } {
        // SAFETY: Binding failed before any descriptor/submission used these handles.
        unsafe { device.free_memory(memory, None) };
        // SAFETY: Binding failed before any descriptor/submission used this buffer.
        unsafe { device.destroy_buffer(buffer, None) };
        return Err(GpuSlugRendererError::Operation {
            operation: "input-buffer memory bind",
            source,
        });
    }
    Ok(GpuBuffer {
        label: resource,
        buffer,
        memory,
        capacity,
    })
}

unsafe fn destroy_buffer(device: &ash::Device, buffer: GpuBuffer) {
    if buffer.buffer != vk::Buffer::null() {
        // SAFETY: Caller guarantees the device is live and the buffer is idle.
        unsafe { device.destroy_buffer(buffer.buffer, None) };
    }
    if buffer.memory != vk::DeviceMemory::null() {
        // SAFETY: Caller guarantees the device is live and the allocation is unmapped/idle.
        unsafe { device.free_memory(buffer.memory, None) };
    }
}

fn validate_request(request: &GpuSlugRenderRequest<'_>) -> Result<(), GpuSlugRendererError> {
    if request.snapshot.columns == 0 || request.snapshot.rows == 0 {
        return Err(GpuSlugRendererError::InvalidRequest {
            detail: "logical terminal grid must be non-zero".to_owned(),
        });
    }
    let expected_cells = usize::from(request.snapshot.columns)
        .checked_mul(usize::from(request.snapshot.rows))
        .ok_or_else(|| GpuSlugRendererError::InvalidRequest {
            detail: "terminal cell count overflows host address space".to_owned(),
        })?;
    if request.snapshot.cells.len() != expected_cells {
        return Err(GpuSlugRendererError::InvalidRequest {
            detail: format!(
                "snapshot contains {} cells but {}x{} requires {expected_cells}",
                request.snapshot.cells.len(),
                request.snapshot.columns,
                request.snapshot.rows,
            ),
        });
    }
    if request.width == 0 || request.height == 0 {
        return Err(GpuSlugRendererError::InvalidRequest {
            detail: "native target dimensions must be non-zero".to_owned(),
        });
    }
    if !request.font_pixel_size.is_finite()
        || !(GPU_SLUG_MIN_PIXEL_SIZE..=GPU_SLUG_MAX_PIXEL_SIZE).contains(&request.font_pixel_size)
    {
        return Err(GpuSlugRendererError::InvalidRequest {
            detail: format!(
                "font pixel size must be finite and within {GPU_SLUG_MIN_PIXEL_SIZE}..={GPU_SLUG_MAX_PIXEL_SIZE}"
            ),
        });
    }
    if !request.force_full_refresh && request.damage.len() > MAX_DAMAGE_REGIONS {
        return Err(GpuSlugRendererError::InvalidRequest {
            detail: format!(
                "{} damage regions exceed the canonical bound {MAX_DAMAGE_REGIONS}",
                request.damage.len()
            ),
        });
    }
    Ok(())
}

fn surface_metrics(
    font: &SlugFont<'_>,
    font_pixel_size: f32,
) -> Result<GpuSlugSurfaceMetrics, GpuSlugRendererError> {
    if !font_pixel_size.is_finite()
        || !(GPU_SLUG_MIN_PIXEL_SIZE..=GPU_SLUG_MAX_PIXEL_SIZE).contains(&font_pixel_size)
    {
        return Err(GpuSlugRendererError::InvalidRequest {
            detail: format!(
                "font pixel size must be finite and within {GPU_SLUG_MIN_PIXEL_SIZE}..={GPU_SLUG_MAX_PIXEL_SIZE}"
            ),
        });
    }
    let metrics = font.metrics();
    let scale = font_pixel_size / f32::from(metrics.units_per_em);
    let advance = font.glyph_geometry('M')?.advance;
    let cell_width = (advance * scale).ceil().max(1.0).to_u32().ok_or_else(|| {
        GpuSlugRendererError::InvalidRequest {
            detail: "Slug cell width is not representable as u32".to_owned(),
        }
    })?;
    let cell_height = ((metrics.ascender - metrics.descender) * scale)
        .ceil()
        .max(1.0)
        .to_u32()
        .ok_or_else(|| GpuSlugRendererError::InvalidRequest {
            detail: "Slug cell height is not representable as u32".to_owned(),
        })?;
    Ok(GpuSlugSurfaceMetrics {
        font_pixel_size,
        cell_width,
        cell_height,
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PixelRegion {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

fn canonical_pixel_regions(
    request: &GpuSlugRenderRequest<'_>,
    metrics: GpuSlugSurfaceMetrics,
) -> Result<Vec<PixelRegion>, GpuSlugRendererError> {
    if request.force_full_refresh || request.damage.is_empty() {
        return Ok(vec![PixelRegion {
            x: 0,
            y: 0,
            width: request.width,
            height: request.height,
        }]);
    }
    let mut output = Vec::with_capacity(request.damage.len());
    for region in request.damage {
        let cell_right = region.column.checked_add(region.columns);
        let cell_bottom = region.row.checked_add(region.rows);
        if region.columns == 0
            || region.rows == 0
            || cell_right.is_none_or(|right| right > request.snapshot.columns)
            || cell_bottom.is_none_or(|bottom| bottom > request.snapshot.rows)
        {
            return Err(GpuSlugRendererError::InvalidRequest {
                detail: format!(
                    "damage rectangle {}x{}+{}+{} exceeds logical grid {}x{}",
                    region.columns,
                    region.rows,
                    region.column,
                    region.row,
                    request.snapshot.columns,
                    request.snapshot.rows,
                ),
            });
        }
        let x = u32::from(region.column)
            .saturating_mul(metrics.cell_width)
            .min(request.width);
        let y = u32::from(region.row)
            .saturating_mul(metrics.cell_height)
            .min(request.height);
        let right = u32::from(cell_right.unwrap_or(request.snapshot.columns))
            .saturating_mul(metrics.cell_width)
            .min(request.width);
        let bottom = u32::from(cell_bottom.unwrap_or(request.snapshot.rows))
            .saturating_mul(metrics.cell_height)
            .min(request.height);
        if right > x && bottom > y {
            let current = PixelRegion {
                x,
                y,
                width: right - x,
                height: bottom - y,
            };
            if output.last().is_some_and(|previous: &PixelRegion| {
                (current.y, current.x) <= (previous.y, previous.x)
            }) || output
                .iter()
                .any(|previous| pixel_regions_overlap(*previous, current))
            {
                return Err(GpuSlugRendererError::InvalidRequest {
                    detail:
                        "damage rectangles must be canonical top-left ordered and non-overlapping"
                            .to_owned(),
                });
            }
            output.push(current);
        }
    }
    if output.is_empty() {
        return Err(GpuSlugRendererError::InvalidRequest {
            detail: "damage request has no pixels inside the native target".to_owned(),
        });
    }
    Ok(output)
}

fn pixel_regions_overlap(left: PixelRegion, right: PixelRegion) -> bool {
    left.x < right.x.saturating_add(right.width)
        && right.x < left.x.saturating_add(left.width)
        && left.y < right.y.saturating_add(right.height)
        && right.y < left.y.saturating_add(left.height)
}

fn unique_render_characters(snapshot: &TerminalSnapshot) -> Vec<char> {
    let mut characters = Vec::new();
    for cell in &snapshot.cells {
        if !characters.contains(&cell.character) {
            characters.push(cell.character);
        }
        for combining in cell.combining_scalars() {
            if !characters.contains(&combining) {
                characters.push(combining);
            }
        }
    }
    if !characters.contains(&' ') {
        characters.push(' ');
    }
    characters
}

fn build_cell_words(
    snapshot: &TerminalSnapshot,
    state: &mut PipelineState,
) -> Result<Vec<u32>, GpuSlugRendererError> {
    let mut output = Vec::with_capacity(snapshot.cells.len().saturating_mul(CELL_WORDS));
    for row in 0..snapshot.rows {
        for column in 0..snapshot.columns {
            let cell =
                snapshot
                    .cell(column, row)
                    .ok_or_else(|| GpuSlugRendererError::InvalidRequest {
                        detail: "snapshot cell disappeared during deterministic traversal"
                            .to_owned(),
                    })?;
            let continuation = column > 0
                && cell.character == ' '
                && cell.combining_count() == 0
                && snapshot.cell(column - 1, row).is_some_and(|previous| {
                    previous.character.width().is_some_and(|width| width >= 2)
                });
            let (render_cell, origin_column, glyph_width) = if continuation {
                let previous = snapshot.cell(column - 1, row).unwrap_or(cell);
                (previous, u32::from(column - 1), 2_u32)
            } else {
                (
                    cell,
                    u32::from(column),
                    u32::try_from(cell.character.width().unwrap_or(1).clamp(1, 2)).unwrap_or(1),
                )
            };
            let glyph = state.ensure_glyph_for_cell(render_cell.character)?;
            let combining_count = usize::from(render_cell.combining_count());
            let selected_column = u16::try_from(origin_column).unwrap_or(column);
            let (foreground, background) =
                cell_colors(render_cell, snapshot.is_selected(selected_column, row));
            let flags = u32::from(render_cell.underline && !continuation);
            output.extend([
                glyph,
                pack_rgba(foreground),
                pack_rgba(background),
                flags,
                origin_column,
                glyph_width,
                u32::from(render_cell.character),
                u32::from(render_cell.combining_count()),
            ]);
            debug_assert_eq!(output.len() % CELL_WORDS, CELL_COMBINING_START);
            for combining in render_cell.combining_scalars() {
                output.push(state.ensure_glyph_for_cell(combining)?);
            }
            output.resize(
                output
                    .len()
                    .saturating_add(MAX_COMBINING_SCALARS_PER_CELL)
                    .saturating_sub(combining_count),
                0,
            );
        }
    }
    debug_assert_eq!(
        output.len(),
        snapshot.cells.len().saturating_mul(CELL_WORDS)
    );
    Ok(output)
}

impl PipelineState {
    fn ensure_glyph_for_cell(&mut self, character: char) -> Result<u32, GpuSlugRendererError> {
        let Some(entry) = self.glyphs.get_mut(&character) else {
            return Err(GpuSlugRendererError::InvalidRequest {
                detail: format!("glyph {character:?} was not prepared before cell upload"),
            });
        };
        self.use_sequence = self.use_sequence.saturating_add(1);
        entry.last_used = self.use_sequence;
        self.geometry_hits = self.geometry_hits.saturating_add(1);
        Ok(entry.slot)
    }
}

fn cell_colors(cell: Cell, selected: bool) -> ([u8; 4], [u8; 4]) {
    let mut foreground = terminal_color(cell.foreground, false);
    let mut background = terminal_color(cell.background, true);
    if cell.bold {
        foreground[0] = foreground[0].saturating_add(24);
        foreground[1] = foreground[1].saturating_add(24);
        foreground[2] = foreground[2].saturating_add(24);
    }
    if cell.inverse {
        std::mem::swap(&mut foreground, &mut background);
    }
    if selected {
        ([255, 255, 255, 255], [70, 100, 170, 255])
    } else {
        (foreground, background)
    }
}

fn terminal_color(color: Color, background: bool) -> [u8; 4] {
    let [red, green, blue] = match color {
        Color::Default if background => [0, 0, 0],
        Color::Default => [212, 212, 212],
        Color::Rgb(red, green, blue) => [red, green, blue],
        Color::Indexed(index) => ansi_palette(index),
    };
    [red, green, blue, 255]
}

fn ansi_palette(index: u8) -> [u8; 3] {
    const BASIC: [[u8; 3]; 16] = [
        [0, 0, 0],
        [205, 49, 49],
        [13, 188, 121],
        [229, 229, 16],
        [36, 114, 200],
        [188, 63, 188],
        [17, 168, 205],
        [229, 229, 229],
        [102, 102, 102],
        [241, 76, 76],
        [35, 209, 139],
        [245, 245, 67],
        [59, 142, 234],
        [214, 112, 214],
        [41, 184, 219],
        [255, 255, 255],
    ];
    match index {
        0..=15 => BASIC[usize::from(index)],
        16..=231 => {
            let value = index - 16;
            [cube(value / 36), cube((value % 36) / 6), cube(value % 6)]
        }
        232..=255 => {
            let value = 8 + (index - 232) * 10;
            [value, value, value]
        }
    }
}

const fn cube(value: u8) -> u8 {
    if value == 0 { 0 } else { 55 + value * 40 }
}

fn pack_rgba(color: [u8; 4]) -> u32 {
    u32::from(color[0])
        | (u32::from(color[1]) << 8)
        | (u32::from(color[2]) << 16)
        | (u32::from(color[3]) << 24)
}

fn pixel_region_words(regions: &[PixelRegion]) -> Vec<u32> {
    let mut words = Vec::with_capacity(regions.len().saturating_mul(REGION_WORDS));
    for region in regions {
        words.extend([region.x, region.y, region.width, region.height]);
    }
    words
}

fn frame_words(
    snapshot: &TerminalSnapshot,
    width: u32,
    height: u32,
    surface: GpuSlugSurfaceMetrics,
    metrics: SlugFontMetrics,
    region_count: usize,
) -> [u32; FRAME_WORDS] {
    [
        width,
        height,
        u32::from(snapshot.columns),
        u32::from(snapshot.rows),
        surface.cell_width,
        surface.cell_height,
        u32::from(snapshot.cursor.column),
        u32::from(snapshot.cursor.row),
        u32::from(snapshot.cursor_visible),
        u32::try_from(GPU_GLYPH_METADATA_WORDS).unwrap_or(u32::MAX),
        u32::try_from(region_count).unwrap_or(u32::MAX),
        u32::from(snapshot.cursor_shape.renderer_code()),
        metrics.ascender.to_bits(),
        metrics.descender.to_bits(),
        surface.font_pixel_size.to_bits(),
        pack_rgba([30, 30, 30, 255]),
    ]
}

fn u32_bytes(words: &[u32]) -> &[u8] {
    // SAFETY: u32 has no padding and the resulting byte slice cannot outlive words.
    unsafe { std::slice::from_raw_parts(words.as_ptr().cast::<u8>(), std::mem::size_of_val(words)) }
}

fn curve_bytes(records: &[[f32; 4]]) -> &[u8] {
    // SAFETY: [f32; 4] is tightly packed and the byte slice cannot outlive records.
    unsafe {
        std::slice::from_raw_parts(
            records.as_ptr().cast::<u8>(),
            std::mem::size_of_val(records),
        )
    }
}

fn elapsed_us(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX)
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use teamy_terminal_core::CursorPosition;
    use teamy_terminal_core::CursorShape;
    use teamy_terminal_core::PromptMetadata;
    use teamy_terminal_core::SelectionRange;
    use teamy_terminal_core::TerminalLimits;
    use teamy_terminal_core::TerminalSession;
    use teamy_terminal_font::TerminalFont;
    use teamy_terminal_font::slug::Point;
    use teamy_terminal_font::slug::coverage_all_curves;

    fn snapshot(text: &str, columns: u16, rows: u16) -> TerminalSnapshot {
        let mut cells = vec![Cell::default(); usize::from(columns) * usize::from(rows)];
        for (index, character) in text.chars().take(cells.len()).enumerate() {
            cells[index].character = character;
        }
        TerminalSnapshot {
            columns,
            rows,
            cursor: CursorPosition::default(),
            cursor_visible: false,
            cursor_shape: CursorShape::default(),
            cells,
            scrollback_rows: 0,
            selection: None,
            prompt: PromptMetadata::default(),
        }
    }

    fn gpu_renderer() -> Option<GpuSlugRenderer> {
        let _guard = gpu_test_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let context = match VulkanOffscreenContext::with_target_cache_capacity(4) {
            Ok(context) => context,
            Err(error) => {
                eprintln!("SKIP GPU-only rust-gpu-slug proof: {error}");
                return None;
            }
        };
        match GpuSlugRenderer::new(context) {
            Ok(renderer) => Some(renderer),
            Err(error) => {
                eprintln!("SKIP GPU-only rust-gpu-slug pipeline proof: {error}");
                None
            }
        }
    }

    fn gpu_test_lock() -> &'static std::sync::Mutex<()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
    }

    fn full_rgba(result: &GpuSlugRenderResult) -> &[u8] {
        result
            .full_rgba
            .as_deref()
            .expect("full request must expose full RGBA")
    }

    #[test]
    fn shader_artifact_is_build_validated_and_source_identified() {
        let source = include_bytes!("../shaders/gpu_slug.wgsl");
        assert_eq!(fnv1a64(source), GPU_SLUG_SHADER_SOURCE_FNV1A64);
        assert_eq!(GPU_SLUG_SHADER_ENTRY, "gpu_slug_main");
        assert_eq!(GPU_SLUG_SHADER_WORDS.first(), Some(&0x0723_0203));
        assert!(GPU_SLUG_SHADER_WORDS.len() > 128);
        assert_eq!(CELL_WORDS, 12);
        assert_eq!(CELL_WORDS, GPU_SLUG_SHADER_CELL_WORDS);
        assert_eq!(
            MAX_COMBINING_SCALARS_PER_CELL,
            GPU_SLUG_SHADER_MAX_COMBINING_GLYPHS
        );
        assert_eq!(GPU_GLYPH_METADATA_WORDS, 16);
        let font = SlugFont::embedded().expect("embedded Slug font");
        assert!(matches!(
            surface_metrics(&font, f32::NAN),
            Err(GpuSlugRendererError::InvalidRequest { .. })
        ));
        assert!(matches!(
            surface_metrics(&font, GPU_SLUG_MAX_PIXEL_SIZE + 1.0),
            Err(GpuSlugRendererError::InvalidRequest { .. })
        ));
    }

    #[test]
    fn dirty_contract_rejects_noncanonical_or_out_of_grid_regions() {
        let snapshot = snapshot("abcd", 4, 1);
        let metrics = GpuSlugSurfaceMetrics {
            font_pixel_size: 16.0,
            cell_width: 8,
            cell_height: 16,
        };
        let overlapping = [CellRect::new(0, 0, 2, 1), CellRect::new(1, 0, 2, 1)];
        let request = GpuSlugRenderRequest {
            snapshot: &snapshot,
            width: 32,
            height: 16,
            font_pixel_size: 16.0,
            damage: &overlapping,
            force_full_refresh: false,
            identity: GpuSlugRenderIdentity::default(),
            timeout: DEFAULT_RENDER_TIMEOUT,
            cancellation: None,
        };
        assert!(matches!(
            canonical_pixel_regions(&request, metrics),
            Err(GpuSlugRendererError::InvalidRequest { .. })
        ));

        let outside = [CellRect::new(3, 0, 2, 1)];
        let request = GpuSlugRenderRequest {
            damage: &outside,
            ..request
        };
        assert!(matches!(
            canonical_pixel_regions(&request, metrics),
            Err(GpuSlugRendererError::InvalidRequest { .. })
        ));
    }

    #[test]
    fn hard_glyph_gpu_occupancy_matches_the_cpu_slug_oracle() {
        let Some(mut renderer) = gpu_renderer() else {
            return;
        };
        eprintln!("rust-gpu-slug device: {:?}", renderer.device_facts());
        for character in ['g', '/', 'b', 'r', '6'] {
            let snapshot = snapshot(&character.to_string(), 1, 1);
            let result = renderer
                .render(GpuSlugRenderRequest::full(
                    &snapshot,
                    64,
                    64,
                    48.0,
                    GpuSlugRenderIdentity::default(),
                ))
                .expect("GPU hard-glyph render should complete");
            let font = SlugFont::embedded().expect("font");
            let geometry = font.glyph_geometry(character).expect("geometry");
            let metrics = font.metrics();
            let scale = 48.0 / f32::from(metrics.units_per_em);
            let surface = surface_metrics(&font, 48.0).expect("surface metrics");
            let cell_width = surface
                .cell_width
                .to_f32()
                .expect("test cell width should be representable as f32");
            let offset = ((cell_width - geometry.advance * scale).max(0.0)) * 0.5;
            let rgba = full_rgba(&result);
            let mut oracle_occupied = 0_u32;
            let mut gpu_occupied = 0_u32;
            for y in 0..surface.cell_height.min(64) {
                for x in 0..surface.cell_width.min(64) {
                    let sample_x = x
                        .to_f32()
                        .expect("test x coordinate should be representable as f32");
                    let sample_y = y
                        .to_f32()
                        .expect("test y coordinate should be representable as f32");
                    let sample = Point::new(
                        (sample_x + 0.5 - offset) / scale,
                        metrics.ascender - (sample_y + 0.5) / scale,
                    );
                    let oracle =
                        coverage_all_curves(&geometry.curves, geometry.bounds, sample, scale)
                            .expect("oracle coverage");
                    oracle_occupied += u32::from(oracle >= 0.2);
                    let row = usize::try_from(y).expect("test row should fit usize");
                    let column = usize::try_from(x).expect("test column should fit usize");
                    let index = (row * 64 + column) * 4;
                    gpu_occupied += u32::from(rgba[index] >= 42);
                }
            }
            let difference = oracle_occupied.abs_diff(gpu_occupied);
            assert!(
                difference <= (oracle_occupied / 8).max(8),
                "{character:?} occupancy differs: oracle={oracle_occupied}, gpu={gpu_occupied}"
            );
        }
    }

    #[test]
    fn gpu_composites_color_style_selection_and_cursor() {
        let Some(mut renderer) = gpu_renderer() else {
            return;
        };
        let mut snapshot = snapshot("ABCD", 4, 1);
        snapshot.cells[0].foreground = Color::Rgb(1, 200, 33);
        snapshot.cells[0].background = Color::Rgb(7, 8, 9);
        snapshot.cells[1].foreground = Color::Indexed(1);
        snapshot.cells[1].bold = true;
        snapshot.cells[1].underline = true;
        snapshot.cells[2].foreground = Color::Rgb(3, 4, 5);
        snapshot.cells[2].background = Color::Rgb(99, 88, 77);
        snapshot.cells[2].inverse = true;
        snapshot.selection = Some(SelectionRange {
            anchor: CursorPosition { column: 3, row: 0 },
            focus: CursorPosition { column: 3, row: 0 },
        });
        snapshot.cursor = CursorPosition { column: 0, row: 0 };
        snapshot.cursor_visible = true;
        snapshot.cursor_shape = CursorShape::SteadyBar;
        let result = renderer
            .render(GpuSlugRenderRequest::full(
                &snapshot,
                160,
                48,
                32.0,
                GpuSlugRenderIdentity {
                    frame_sequence: 7,
                    presentation_generation: "generation:gpu/3".to_owned(),
                    correlation_id: "corr:not-a-number".to_owned(),
                    ..GpuSlugRenderIdentity::default()
                },
            ))
            .expect("styled GPU frame");
        let cell_width = result.metrics.cell_width;
        let cell_height = result.metrics.cell_height;
        let rgba = full_rgba(&result);
        let pixel = |x: u32, y: u32| {
            let index = (usize::try_from(y).unwrap() * 160 + usize::try_from(x).unwrap()) * 4;
            &rgba[index..index + 4]
        };
        assert_eq!(pixel(cell_width - 1, 0), [7, 8, 9, 255]);
        assert_eq!(
            pixel(cell_width + cell_width / 2, cell_height - 2),
            [229, 73, 73, 255]
        );
        assert_eq!(pixel(cell_width * 2, 0), [3, 4, 5, 255]);
        assert_eq!(pixel(cell_width * 3, 0), [70, 100, 170, 255]);
        assert_eq!(pixel(0, 0), [210, 210, 210, 190]);

        let mut cpu = TerminalFont::new(32.0).expect("CPU semantic oracle font");
        assert_eq!(cpu.cell_width(), cell_width);
        assert_eq!(cpu.cell_height(), cell_height);
        let cpu_frame = cpu.render(&snapshot, 160, 48);
        let cpu_pixel = |x: u32, y: u32| {
            let index = (usize::try_from(y).unwrap() * 160 + usize::try_from(x).unwrap()) * 4;
            &cpu_frame.pixels[index..index + 4]
        };
        for (x, y) in [
            (cell_width - 1, 0),
            (cell_width + cell_width / 2, cell_height - 2),
            (cell_width * 2, 0),
            (cell_width * 3, 0),
        ] {
            assert_eq!(pixel(x, y), cpu_pixel(x, y));
        }
        assert_eq!(pixel(0, 0), cpu_pixel(0, 0));
        assert_eq!(result.identity.frame_sequence, 7);
        assert_eq!(result.identity.presentation_generation, "generation:gpu/3");
        assert_eq!(result.identity.correlation_id, "corr:not-a-number");
    }

    #[test]
    fn every_cursor_shape_matches_cpu_geometry() {
        let Some(mut renderer) = gpu_renderer() else {
            return;
        };
        let metrics = renderer.surface_metrics(32.0).expect("GPU surface metrics");
        let mut cpu = TerminalFont::new(32.0).expect("CPU semantic oracle font");
        assert_eq!(cpu.cell_width(), metrics.cell_width);
        assert_eq!(cpu.cell_height(), metrics.cell_height);
        let mut frames = Vec::new();

        for shape in [
            CursorShape::BlinkingBlock,
            CursorShape::SteadyBlock,
            CursorShape::BlinkingUnderline,
            CursorShape::SteadyUnderline,
            CursorShape::BlinkingBar,
            CursorShape::SteadyBar,
        ] {
            let mut snapshot = snapshot(" ", 1, 1);
            snapshot.cursor_visible = true;
            snapshot.cursor_shape = shape;
            let gpu = renderer
                .render(GpuSlugRenderRequest::full(
                    &snapshot,
                    metrics.cell_width,
                    metrics.cell_height,
                    32.0,
                    GpuSlugRenderIdentity::default(),
                ))
                .expect("GPU cursor frame");
            let cpu_frame = cpu.render(&snapshot, metrics.cell_width, metrics.cell_height);
            assert_eq!(
                full_rgba(&gpu),
                cpu_frame.pixels,
                "cursor mismatch for {shape:?}"
            );
            frames.push(full_rgba(&gpu).to_vec());
        }

        assert_eq!(frames[0], frames[1]);
        assert_eq!(frames[2], frames[3]);
        assert_eq!(frames[4], frames[5]);
        assert_ne!(frames[0], frames[2]);
        assert_ne!(frames[2], frames[4]);
    }

    #[test]
    fn dirty_composition_matches_direct_full_and_reuses_a_b_a_targets() {
        let Some(mut renderer) = gpu_renderer() else {
            return;
        };
        let first = snapshot("g/r6", 4, 1);
        let mut second = first.clone();
        second.cells[1].character = 'b';
        let a = renderer
            .render(GpuSlugRenderRequest::full(
                &first,
                128,
                48,
                32.0,
                GpuSlugRenderIdentity::default(),
            ))
            .expect("first A frame");
        let damage = [CellRect::new(0, 0, 3, 1)];
        let dirty = renderer
            .render(GpuSlugRenderRequest {
                snapshot: &second,
                width: 128,
                height: 48,
                font_pixel_size: 32.0,
                damage: &damage,
                force_full_refresh: false,
                identity: GpuSlugRenderIdentity {
                    frame_sequence: 2,
                    ..GpuSlugRenderIdentity::default()
                },
                timeout: DEFAULT_RENDER_TIMEOUT,
                cancellation: None,
            })
            .expect("dirty A frame");
        let b = renderer
            .render(GpuSlugRenderRequest::full(
                &second,
                191,
                71,
                40.0,
                GpuSlugRenderIdentity::default(),
            ))
            .expect("B frame");
        let direct = renderer
            .render(GpuSlugRenderRequest::full(
                &second,
                128,
                48,
                32.0,
                GpuSlugRenderIdentity::default(),
            ))
            .expect("direct A frame");
        let mut composed = full_rgba(&a).to_vec();
        for region in &dirty.dirty_regions {
            let row_bytes = usize::try_from(region.width).unwrap() * 4;
            for row in 0..region.height {
                let destination = (usize::try_from(region.y + row).unwrap() * 128
                    + usize::try_from(region.x).unwrap())
                    * 4;
                let source = usize::try_from(row).unwrap() * row_bytes;
                composed[destination..destination + row_bytes]
                    .copy_from_slice(&region.rgba[source..source + row_bytes]);
            }
        }
        assert!(dirty.full_rgba.is_none());
        assert!(dirty.visual_hash.is_none());
        assert!(dirty.stats.full_rgba_pack_us.is_none());
        assert!(dirty.stats.dirty_region_pack_us.is_some());
        assert!(dirty.stats.raw_packet_pack_us.is_none());
        assert!(dirty.stats.png_packet_pack_us.is_none());
        assert_eq!(composed, full_rgba(&direct));
        assert_eq!(
            a.stats.target_allocation_id,
            dirty.stats.target_allocation_id
        );
        assert_eq!(
            a.stats.target_allocation_id,
            direct.stats.target_allocation_id
        );
        assert_ne!(a.stats.target_allocation_id, b.stats.target_allocation_id);
        assert_eq!(dirty.dirty_regions.len(), 1);
        assert_eq!(
            dirty.dirty_regions[0].rgba.len(),
            usize::try_from(dirty.dirty_regions[0].width).unwrap()
                * usize::try_from(dirty.dirty_regions[0].height).unwrap()
                * 4
        );
        assert!(direct.dirty_regions.is_empty());
        assert!(direct.stats.full_rgba_pack_us.is_some());
        assert!(direct.stats.dirty_region_pack_us.is_none());
        assert!(direct.cache.geometry_hits > a.cache.geometry_hits);
        assert!(direct.cache.targets.reuses >= 2);
    }

    fn assert_wide_glyph_contract(renderer: &mut GpuSlugRenderer) {
        let mut wide = snapshot("界 ", 2, 1);
        wide.cells[0].underline = true;
        wide.selection = Some(SelectionRange {
            anchor: CursorPosition { column: 0, row: 0 },
            focus: CursorPosition { column: 0, row: 0 },
        });
        let prepared = unique_render_characters(&wide);
        for character in prepared {
            renderer
                .state
                .ensure_glyph(&renderer.runtime.font, character)
                .expect("wide fixture glyph preparation");
        }
        let words = build_cell_words(&wide, &mut renderer.state).expect("wide cell words");
        assert_eq!(words[0], words[CELL_WORDS]);
        assert_eq!(words[1], words[CELL_WORDS + 1]);
        assert_eq!(words[2], words[CELL_WORDS + 2]);
        assert_eq!(words[2], pack_rgba([70, 100, 170, 255]));
        assert_eq!(words[3], 1);
        assert_eq!(words[CELL_WORDS + 3], 0);
        assert_eq!(words[4], 0);
        assert_eq!(words[CELL_WORDS + 4], 0);
        assert_eq!(words[5], 2);
        assert_eq!(words[CELL_WORDS + 5], 2);

        let wide_result = renderer
            .render(GpuSlugRenderRequest::full(
                &wide,
                96,
                48,
                32.0,
                GpuSlugRenderIdentity::default(),
            ))
            .expect("wide/continuation GPU frame");
        assert!(
            full_rgba(&wide_result)
                .chunks_exact(4)
                .any(|pixel| pixel[0] > 30)
        );
    }

    fn assert_combining_glyph_contract(renderer: &mut GpuSlugRenderer) {
        let mut combining_terminal = TerminalSession::new(TerminalLimits::default(), 2, 1);
        combining_terminal.process_bytes("\u{001b}[?25le\u{0301}".as_bytes());
        let combining = combining_terminal.snapshot();
        assert_eq!(combining.cursor, CursorPosition { column: 1, row: 0 });
        assert_eq!(
            combining
                .cell(0, 0)
                .map(|cell| cell.combining_scalars().collect::<Vec<_>>()),
            Some(vec!['\u{0301}'])
        );
        let prepared = unique_render_characters(&combining);
        for character in prepared {
            renderer
                .state
                .ensure_glyph(&renderer.runtime.font, character)
                .expect("combining fixture glyph preparation");
        }
        let words =
            build_cell_words(&combining, &mut renderer.state).expect("combining cell words");
        assert_eq!(words[7], 1);
        assert_eq!(
            words[CELL_COMBINING_START],
            renderer
                .state
                .glyphs
                .get(&'\u{0301}')
                .expect("combining glyph cache entry")
                .slot
        );
        assert_eq!(words[CELL_WORDS + 7], 0);
        let combining_result = renderer
            .render(GpuSlugRenderRequest::full(
                &combining,
                96,
                48,
                32.0,
                GpuSlugRenderIdentity::default(),
            ))
            .expect("combining-overlay fixture");
        let combining_rgba = full_rgba(&combining_result);
        let second_cell_start = usize::try_from(combining_result.metrics.cell_width).unwrap();
        let second_cell_has_ink = (0..combining_result.metrics.cell_height.min(48)).any(|y| {
            (second_cell_start..second_cell_start * 2).any(|x| {
                let index = (usize::try_from(y).unwrap() * 96 + x) * 4;
                combining_rgba[index] > 30
            })
        });
        assert!(
            !second_cell_has_ink,
            "combining scalar must overlay the base instead of consuming the next cell"
        );
    }

    fn assert_missing_glyph_contract(renderer: &mut GpuSlugRenderer) {
        let missing_character = char::from_u32(0x10_FFFF).expect("valid scalar");
        let missing = snapshot(&missing_character.to_string(), 1, 1);
        let question = snapshot("?", 1, 1);
        let missing_result = renderer
            .render(GpuSlugRenderRequest::full(
                &missing,
                64,
                48,
                32.0,
                GpuSlugRenderIdentity::default(),
            ))
            .expect("missing glyph fallback");
        let question_result = renderer
            .render(GpuSlugRenderRequest::full(
                &question,
                64,
                48,
                32.0,
                GpuSlugRenderIdentity::default(),
            ))
            .expect("question-mark glyph");
        assert_eq!(missing_result.visual_hash, question_result.visual_hash);
        assert_eq!(full_rgba(&missing_result), full_rgba(&question_result));
    }

    #[test]
    fn wide_combining_and_missing_glyph_contract_is_explicit() {
        let Some(mut renderer) = gpu_renderer() else {
            return;
        };
        assert_wide_glyph_contract(&mut renderer);
        assert_combining_glyph_contract(&mut renderer);
        assert_missing_glyph_contract(&mut renderer);
    }

    #[test]
    fn combining_damage_composes_exactly_and_retains_mark_geometry() {
        let Some(mut renderer) = gpu_renderer() else {
            return;
        };
        let mut terminal = TerminalSession::new(TerminalLimits::default(), 8, 1);
        terminal.process_bytes("\u{001b}[?25le\u{0301}X".as_bytes());
        let first = terminal.snapshot();
        let first_result = renderer
            .render(GpuSlugRenderRequest::full(
                &first,
                256,
                48,
                32.0,
                GpuSlugRenderIdentity::default(),
            ))
            .expect("first combining frame");

        terminal.process_bytes("\re\u{0327}".as_bytes());
        let second = terminal.snapshot();
        let (damage, full_refresh) = teamy_terminal_font::damage_regions(Some(&first), &second);
        assert!(!full_refresh);
        assert!(!damage.is_empty());
        let dirty = renderer
            .render(GpuSlugRenderRequest {
                snapshot: &second,
                width: 256,
                height: 48,
                font_pixel_size: 32.0,
                damage: &damage,
                force_full_refresh: false,
                identity: GpuSlugRenderIdentity::default(),
                timeout: DEFAULT_RENDER_TIMEOUT,
                cancellation: None,
            })
            .expect("dirty combining frame");
        let direct = renderer
            .render(GpuSlugRenderRequest::full(
                &second,
                256,
                48,
                32.0,
                GpuSlugRenderIdentity::default(),
            ))
            .expect("direct combining frame");

        let mut composed = full_rgba(&first_result).to_vec();
        for region in &dirty.dirty_regions {
            let row_bytes = usize::try_from(region.width).unwrap() * 4;
            for row in 0..region.height {
                let destination = (usize::try_from(region.y + row).unwrap() * 256
                    + usize::try_from(region.x).unwrap())
                    * 4;
                let source = usize::try_from(row).unwrap() * row_bytes;
                composed[destination..destination + row_bytes]
                    .copy_from_slice(&region.rgba[source..source + row_bytes]);
            }
        }

        assert_eq!(composed, full_rgba(&direct));
        assert_ne!(full_rgba(&first_result), full_rgba(&direct));
        assert!(dirty.cache.geometry_misses > first_result.cache.geometry_misses);
        assert_eq!(
            dirty.stats.target_allocation_id,
            first_result.stats.target_allocation_id
        );
    }

    #[test]
    fn native_clipping_scroll_alternate_screen_and_repeat_hash_are_deterministic() {
        let Some(mut renderer) = gpu_renderer() else {
            return;
        };
        let mut terminal = TerminalSession::new(TerminalLimits::default(), 8, 2);
        terminal.process_bytes(b"one\r\ntwo\r\nthree");
        let scrolled = terminal.snapshot();
        assert!(scrolled.scrollback_rows > 0);
        let clipped = renderer
            .render(GpuSlugRenderRequest::full(
                &scrolled,
                73,
                29,
                24.0,
                GpuSlugRenderIdentity::default(),
            ))
            .expect("clipped native target");
        let repeated = renderer
            .render(GpuSlugRenderRequest::full(
                &scrolled,
                73,
                29,
                24.0,
                GpuSlugRenderIdentity::default(),
            ))
            .expect("repeat clipped target");
        assert_eq!((clipped.width, clipped.height), (73, 29));
        assert_eq!(full_rgba(&clipped).len(), 73 * 29 * 4);
        assert!(clipped.dirty_regions.is_empty());
        assert!(clipped.visual_hash.is_some());
        assert_eq!(clipped.visual_hash, repeated.visual_hash);
        assert_eq!(full_rgba(&clipped), full_rgba(&repeated));

        terminal.process_bytes(b"\x1b[?1049h\x1b[?25lALT");
        let alternate = terminal.snapshot();
        assert!(alternate.visible_text().contains("ALT"));
        let alternate_result = renderer
            .render(GpuSlugRenderRequest::full(
                &alternate,
                73,
                29,
                24.0,
                GpuSlugRenderIdentity::default(),
            ))
            .expect("alternate-screen snapshot");
        assert_ne!(alternate_result.visual_hash, clipped.visual_hash);

        terminal.process_bytes(b"\x1b[?1049l");
        let restored = terminal.snapshot();
        assert_eq!(restored.visible_text(), scrolled.visible_text());
        let restored_result = renderer
            .render(GpuSlugRenderRequest::full(
                &restored,
                73,
                29,
                24.0,
                GpuSlugRenderIdentity::default(),
            ))
            .expect("restored main-screen snapshot");
        assert_eq!(restored_result.visual_hash, clipped.visual_hash);
    }

    #[test]
    fn process_runtime_shares_pipeline_but_sessions_render_independently() {
        let _guard = gpu_test_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let context = match VulkanOffscreenContext::with_target_cache_capacity(4) {
            Ok(context) => context,
            Err(error) => {
                eprintln!("SKIP GPU-only shared runtime proof: {error}");
                return;
            }
        };
        let runtime = GpuSlugRuntime::new(context).expect("process GPU slug runtime");
        let defaults = runtime
            .default_surface_metrics()
            .expect("default outline-only metrics");
        let fitted = runtime
            .fit_surface_metrics(80, 24, 1_280, 720)
            .expect("panel-fit outline-only metrics");
        assert!(defaults.native_width(80) > 0);
        assert!(defaults.native_height(24) > 0);
        assert!(fitted.native_width(80) <= 1_280);
        assert!(fitted.native_height(24) <= 720);
        let sfm_panel = runtime
            .fit_surface_metrics(70, 21, 425, 238)
            .expect("normal-scale SFM panel metrics");
        assert!(sfm_panel.native_width(70) <= 425);
        assert!(sfm_panel.native_height(21) <= 238);
        let mut first = runtime.create_renderer().expect("first session renderer");
        let mut second = runtime.create_renderer().expect("second session renderer");
        assert!(Arc::ptr_eq(&first.state.shared, &second.state.shared));
        assert_ne!(first.session.id(), second.session.id());
        let first_snapshot = snapshot("first", 5, 1);
        let second_snapshot = snapshot("second", 6, 1);
        let (first_result, second_result) = std::thread::scope(|scope| {
            let first_thread = scope.spawn(|| {
                first.render(GpuSlugRenderRequest::full(
                    &first_snapshot,
                    120,
                    48,
                    24.0,
                    GpuSlugRenderIdentity::default(),
                ))
            });
            let second_thread = scope.spawn(|| {
                second.render(GpuSlugRenderRequest::full(
                    &second_snapshot,
                    144,
                    48,
                    24.0,
                    GpuSlugRenderIdentity::default(),
                ))
            });
            (
                first_thread.join().expect("first session thread"),
                second_thread.join().expect("second session thread"),
            )
        });
        let first_result = first_result.expect("first independent render");
        let second_result = second_result.expect("second independent render");
        assert_ne!(
            first_result.stats.target_allocation_id,
            second_result.stats.target_allocation_id
        );
        assert_ne!(first_result.visual_hash, second_result.visual_hash);
        assert_eq!(first_result.cache.retained_glyphs, 6);
        assert_eq!(second_result.cache.retained_glyphs, 7);
    }
}
