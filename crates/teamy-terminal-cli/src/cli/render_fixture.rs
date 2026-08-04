use crate::cli::output::CliOutput;
use arbitrary::Arbitrary;
use eyre::Context;
use facet::Facet;
use figue::{self as args};
use std::fs;
use std::path::PathBuf;
use std::time::Instant;
use teamy_terminal_core::CursorPosition;
use teamy_terminal_core::TerminalLimits;
use teamy_terminal_core::TerminalSession;
use teamy_terminal_font::CellRect;
use teamy_terminal_font::DEFAULT_PIXEL_SIZE;
use teamy_terminal_font::EMBEDDED_FONT_ID;
use teamy_terminal_font::EMBEDDED_FONT_SHA256;
use teamy_terminal_font::TerminalFont;
use teamy_terminal_font::TerminalFontCacheStats;
use teamy_terminal_font::TerminalFontCatalog;
use teamy_terminal_font::damage_regions;
use teamy_terminal_frame::ComposedRasterFrame;
use teamy_terminal_frame::FrameCompression;
use teamy_terminal_frame::PackedRasterRegion;
use teamy_terminal_frame::RasterFrameUpdate;
use teamy_terminal_frame::RasterRect;
use teamy_terminal_renderer::GPU_SLUG_SHADER_SOURCE_IDENTITY;
use teamy_terminal_renderer::GpuSlugCacheStats;
use teamy_terminal_renderer::GpuSlugRenderIdentity;
use teamy_terminal_renderer::GpuSlugRenderRequest;
use teamy_terminal_renderer::GpuSlugRenderStats;
use teamy_terminal_renderer::GpuSlugRenderer;
use teamy_terminal_renderer::GpuSlugRuntime;
use teamy_terminal_renderer::RUST_GPU_SLUG_RENDERER_ID;
use teamy_terminal_renderer::VulkanDeviceFacts;
use teamy_terminal_renderer::VulkanOffscreenCacheStats;
use teamy_terminal_renderer::VulkanOffscreenContext;

const CPU_RENDERER_ID: &str = "rust-cpu-fontdue";
const FIXTURE_ID: &str = "terminal-raster-evidence-v2";
const BASE_SEQUENCE: u64 = 1;
const DIRTY_SEQUENCE: u64 = 2;
const DEFAULT_SAMPLE_FRAMES: u32 = 1;
const MAX_TOTAL_REPEATED_FRAMES: u32 = 10_000;
const GPU_TRACY_STAGE_NAMES: [&str; 6] = [
    "gpu_slug_render",
    "gpu_slug_geometry",
    "gpu_slug_upload",
    "gpu_slug_raster_readback",
    "gpu_slug_payload_pack",
    "finished frame",
];

/// Stable rasterizer identity exercised by the render fixture.
#[derive(Arbitrary, Clone, Copy, Debug, Default, Facet, PartialEq, Eq)]
#[facet(rename_all = "kebab-case")]
#[repr(u8)]
pub enum RenderFixtureRenderer {
    /// CPU reference renderer backed by `fontdue`.
    #[default]
    RustCpuFontdue,
    /// True Slug outline coverage and compositing on a no-window Vulkan target.
    RustGpuSlug,
}

/// Output format for the deterministic render fixture.
#[derive(Arbitrary, Clone, Copy, Debug, Default, Facet, PartialEq, Eq)]
#[facet(rename_all = "kebab-case")]
#[repr(u8)]
pub enum RenderFixtureFormat {
    /// Lossless PNG archive/keyframe output.
    #[default]
    Png,
    /// Tight row-major RGBA8 bytes.
    Raw,
}

/// Stable transport identity exercised by the render fixture.
#[derive(Arbitrary, Clone, Copy, Debug, Default, Facet, PartialEq, Eq)]
#[facet(rename_all = "kebab-case")]
#[repr(u8)]
pub enum RenderFixtureTransport {
    /// Complete lossless PNG frame.
    #[default]
    FullPng,
    /// Complete tight row-major RGBA8 frame.
    FullRawRgba,
    /// Base-dependent packed RGBA8 dirty regions.
    DirtyRawRgba,
}

/// Arguments for `teamy-terminal render-fixture`.
#[derive(Arbitrary, Debug, Facet, PartialEq, Eq)]
#[facet(rename_all = "kebab-case")]
pub struct RenderFixtureArgs {
    /// Explicit rasterizer identity. Defaults to the CPU/fontdue reference.
    #[facet(args::named, default)]
    #[arbitrary(default)]
    pub renderer: RenderFixtureRenderer,

    /// Backwards-compatible artifact encoding. `png` resolves to `full-png`
    /// and `raw` resolves to `full-raw-rgba` when `transport` is omitted.
    #[facet(args::named, default)]
    #[arbitrary(default)]
    pub format: RenderFixtureFormat,

    /// Explicit stable transport identity. This takes precedence over
    /// `format` when both arguments are present.
    #[facet(args::named)]
    #[arbitrary(default)]
    pub transport: Option<RenderFixtureTransport>,

    /// Output path; defaults to a transport-specific file in the current dir.
    #[facet(args::named)]
    #[arbitrary(default)]
    pub output: Option<String>,

    /// Optional path for the machine-readable JSON evidence manifest.
    #[facet(args::named)]
    #[arbitrary(default)]
    pub manifest: Option<String>,

    /// Selected-tuple frames discarded before measured repeated samples.
    #[facet(args::named, default)]
    #[arbitrary(default)]
    pub warmup_frames: u32,

    /// Measured selected-tuple frames; defaults to one and must be non-zero.
    #[facet(args::named)]
    #[arbitrary(default)]
    pub sample_frames: Option<u32>,
}

#[derive(Arbitrary, Debug, Facet, PartialEq, Eq)]
struct RenderFixtureOutput {
    path: String,
    manifest_path: Option<String>,
    format: RenderFixtureFormat,
    transport_id: String,
    renderer_id: String,
    sequence: u64,
    base_sequence: u64,
    width: u32,
    height: u32,
    stride: u32,
    bytes: usize,
    regions: usize,
    exact_equivalence: bool,
}

#[derive(Clone, Debug, Default, Facet, PartialEq, Eq)]
#[expect(
    clippy::struct_field_names,
    reason = "the manifest keeps every duration unit explicit"
)]
struct RenderFixtureTimings {
    fixture_setup_us: u64,
    renderer_setup_us: u64,
    snapshot_a_us: u64,
    raster_a_us: u64,
    base_pack_us: u64,
    mutation_us: u64,
    snapshot_b_us: u64,
    damage_us: u64,
    dirty_raster_us: u64,
    proof_dirty_pack_us: u64,
    selected_pack_us: u64,
    compose_us: u64,
    direct_full_b_us: u64,
    total_us: u64,
}

#[derive(Clone, Debug, Facet, PartialEq, Eq)]
struct RenderFixtureRegionManifest {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    stride: u32,
    payload_offset: u32,
    payload_length: u32,
}

#[derive(Clone, Debug, Facet, PartialEq, Eq)]
struct RenderFixtureFontCacheStats {
    hits: u64,
    misses: u64,
    evictions: u64,
    cached_glyphs: usize,
    capacity: usize,
}

#[derive(Clone, Debug, Facet, PartialEq, Eq)]
struct RenderFixtureCpuCacheEvidence {
    process_font_catalog_loads: u64,
    after_base_render: RenderFixtureFontCacheStats,
    after_dirty_render: RenderFixtureFontCacheStats,
    after_direct_full_b_render: RenderFixtureFontCacheStats,
    after_repeated_workload: RenderFixtureFontCacheStats,
}

#[derive(Clone, Debug, Facet, PartialEq, Eq)]
struct RenderFixtureTargetCacheStats {
    capacity: usize,
    retained_targets: usize,
    allocations: u64,
    reuses: u64,
    evictions: u64,
    destructions: u64,
}

#[derive(Clone, Debug, Facet, PartialEq, Eq)]
struct RenderFixtureGpuCacheStats {
    geometry_hits: u64,
    geometry_misses: u64,
    geometry_evictions: u64,
    retained_glyphs: usize,
    glyph_capacity: usize,
    buffer_allocations: u64,
    targets: RenderFixtureTargetCacheStats,
}

#[derive(Clone, Debug, Facet, PartialEq, Eq)]
#[expect(
    clippy::struct_field_names,
    reason = "checkpoint prefixes make cache chronology unambiguous in JSON"
)]
struct RenderFixtureGpuCacheEvidence {
    after_base_render: RenderFixtureGpuCacheStats,
    after_dirty_render: RenderFixtureGpuCacheStats,
    after_direct_full_b_render: RenderFixtureGpuCacheStats,
    after_repeated_workload: RenderFixtureGpuCacheStats,
}

#[derive(Clone, Debug, Facet, PartialEq, Eq)]
struct RenderFixtureCacheEvidence {
    cache_kind: String,
    cpu: Option<RenderFixtureCpuCacheEvidence>,
    gpu: Option<RenderFixtureGpuCacheEvidence>,
}

#[derive(Clone, Debug, Facet, PartialEq, Eq)]
struct RenderFixtureGpuRenderStats {
    geometry_build_us: u64,
    outline_extraction_us: u64,
    directional_band_build_us: u64,
    upload_bytes: u64,
    upload_us: u64,
    command_record_us: u64,
    queue_submit_us: u64,
    gpu_completion_wait_us: u64,
    readback_map_copy_us: u64,
    payload_pack_us: u64,
    full_rgba_pack_us: Option<u64>,
    dirty_region_pack_us: Option<u64>,
    raw_packet_pack_us: Option<u64>,
    png_packet_pack_us: Option<u64>,
    total_us: u64,
    requested_pixels: u64,
    target_allocation_id: u64,
}

#[derive(Clone, Debug, Facet, PartialEq, Eq)]
struct RenderFixtureGpuStageEvidence {
    base_full_a: RenderFixtureGpuRenderStats,
    dirty_b: RenderFixtureGpuRenderStats,
    direct_full_b: RenderFixtureGpuRenderStats,
}

#[derive(Clone, Debug, Default, Facet, PartialEq, Eq)]
struct RenderFixtureDurationSummary {
    samples: u32,
    total_us: u64,
    min_us: u64,
    max_us: u64,
    mean_us: u64,
}

#[derive(Clone, Debug, Facet, PartialEq, Eq)]
struct RenderFixtureSamplingEvidence {
    warmup_frames_requested: u32,
    warmup_frames_completed: u32,
    sample_frames_requested: u32,
    sample_frames_completed: u32,
    maximum_total_repeated_frames: u32,
    first_repeated_frame_sequence: u64,
    last_repeated_frame_sequence: u64,
    presentation_generation: String,
    selected_workload: String,
    stable_stage_names: Vec<String>,
    renderer_total_us: RenderFixtureDurationSummary,
    packet_pack_us: RenderFixtureDurationSummary,
    gpu_stage_summaries: Option<RenderFixtureGpuStageSummaries>,
    expected_payload_fnv1a64: String,
    deterministic_payloads: bool,
}

#[derive(Clone, Debug, Facet, PartialEq, Eq)]
struct RenderFixtureGpuStageSummaries {
    geometry_build_us: RenderFixtureDurationSummary,
    outline_extraction_us: RenderFixtureDurationSummary,
    directional_band_build_us: RenderFixtureDurationSummary,
    upload_us: RenderFixtureDurationSummary,
    command_record_us: RenderFixtureDurationSummary,
    queue_submit_us: RenderFixtureDurationSummary,
    gpu_completion_wait_us: RenderFixtureDurationSummary,
    readback_map_copy_us: RenderFixtureDurationSummary,
    payload_pack_us: RenderFixtureDurationSummary,
    total_upload_bytes: u64,
    total_requested_pixels: u64,
}

#[derive(Clone, Debug, Facet, PartialEq, Eq)]
struct RenderFixtureDeviceEvidence {
    device_name: String,
    vendor_id: u32,
    device_id: u32,
    driver_version: u32,
    api_version: u32,
    queue_family_index: u32,
    queue_flags: String,
    max_image_dimension_2d: u32,
    enabled_instance_extensions: Vec<String>,
    enabled_device_extensions: Vec<String>,
}

#[derive(Clone, Debug, Facet, PartialEq, Eq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent no-window invariants must remain explicit evidence fields"
)]
struct RenderFixtureExecutionEvidence {
    rasterization_owner: String,
    execution_target: String,
    no_window: bool,
    uses_winit_window: bool,
    uses_vulkan_surface: bool,
    uses_swapchain: bool,
    device: Option<RenderFixtureDeviceEvidence>,
}

#[derive(Clone, Debug, Facet, PartialEq, Eq)]
struct RenderFixtureProvenance {
    fixture_id: String,
    package_version: String,
    repository: String,
    branch: String,
    revision: String,
    worktree_status: String,
    build_timestamp_unix: String,
    rustc_version: String,
    build_target: String,
    build_host: String,
    build_profile: String,
    tracy_feature_enabled: bool,
    extended_observability_feature_enabled: bool,
    target_os: String,
    target_arch: String,
    snapshot_hash_algorithm: String,
    snapshot_a_semantic_fnv1a64: String,
    snapshot_b_semantic_fnv1a64: String,
    shader_source_fnv1a64: Option<String>,
}

#[derive(Clone, Debug, Facet, PartialEq, Eq)]
struct RenderFixtureCellRegionManifest {
    column: u16,
    row: u16,
    columns: u16,
    rows: u16,
}

#[derive(Clone, Debug, Facet, PartialEq, Eq)]
struct RenderFixtureCompositionProof {
    base_sequence: u64,
    dirty_sequence: u64,
    base_rgba_bytes: usize,
    dirty_payload_bytes: usize,
    direct_full_b_rgba_bytes: usize,
    base_rgba_fnv1a64: String,
    dirty_payload_fnv1a64: String,
    direct_full_b_rgba_fnv1a64: String,
    composed_b_rgba_fnv1a64: String,
    dirty_regions: Vec<RenderFixtureRegionManifest>,
    exact_equivalence: bool,
}

#[derive(Clone, Debug, Facet, PartialEq, Eq)]
struct RenderFixtureVisualComparison {
    reference_renderer_id: String,
    candidate_renderer_id: String,
    width: u32,
    height: u32,
    pixel_count: u64,
    differing_pixels: u64,
    differing_pixels_basis_points: u32,
    alpha_differing_pixels: u64,
    max_channel_absolute_delta: u8,
    total_channel_absolute_delta: u64,
    mean_channel_absolute_delta_milli: u64,
    differences_present: bool,
    difference_min_x: u32,
    difference_min_y: u32,
    difference_max_x: u32,
    difference_max_y: u32,
    reference_raster_us: u64,
    comparison_us: u64,
}

#[derive(Clone, Debug, Facet, PartialEq, Eq)]
struct RenderFixtureManifest {
    manifest_version: u16,
    transport_id: String,
    renderer_id: String,
    damage_mode: String,
    encoding: String,
    frame_kind: String,
    contract_version: u16,
    sequence: u64,
    base_sequence: u64,
    width: u32,
    height: u32,
    logical_columns: u16,
    logical_rows: u16,
    frame_stride: u32,
    update_stride: u32,
    bytes_per_pixel: u8,
    origin: String,
    alpha_mode: String,
    color_space: String,
    font_id: String,
    font_sha256: String,
    font_pixel_size_milli: u32,
    cell_width: u32,
    cell_height: u32,
    payload_bytes: usize,
    payload_fnv1a64: String,
    region_count: usize,
    regions: Vec<RenderFixtureRegionManifest>,
    dirty_cell_regions: Vec<RenderFixtureCellRegionManifest>,
    exact_equivalence: bool,
    composition_proof: RenderFixtureCompositionProof,
    visual_comparison: Option<RenderFixtureVisualComparison>,
    cache: RenderFixtureCacheEvidence,
    timings: RenderFixtureTimings,
    gpu_stages: Option<RenderFixtureGpuStageEvidence>,
    provenance: RenderFixtureProvenance,
    execution: RenderFixtureExecutionEvidence,
    sampling: RenderFixtureSamplingEvidence,
}

struct RenderedFixture {
    renderer_id: &'static str,
    update: RasterFrameUpdate,
    logical_columns: u16,
    logical_rows: u16,
    cell_width: u32,
    cell_height: u32,
    font_pixel_size_milli: u32,
    dirty_cell_regions: Vec<RenderFixtureCellRegionManifest>,
    exact_equivalence: bool,
    composition_proof: RenderFixtureCompositionProof,
    visual_comparison: Option<RenderFixtureVisualComparison>,
    cache: RenderFixtureCacheEvidence,
    timings: RenderFixtureTimings,
    gpu_stages: Option<RenderFixtureGpuStageEvidence>,
    provenance: RenderFixtureProvenance,
    execution: RenderFixtureExecutionEvidence,
    sampling: RenderFixtureSamplingEvidence,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RenderFixtureSamplingRequest {
    warmup_frames: u32,
    sample_frames: u32,
}

#[derive(Debug)]
struct DurationAccumulator {
    samples: u32,
    total_us: u64,
    min_us: u64,
    max_us: u64,
}

#[derive(Debug)]
struct GpuStageAccumulators {
    geometry_build_us: DurationAccumulator,
    outline_extraction_us: DurationAccumulator,
    directional_band_build_us: DurationAccumulator,
    upload_us: DurationAccumulator,
    command_record_us: DurationAccumulator,
    queue_submit_us: DurationAccumulator,
    gpu_completion_wait_us: DurationAccumulator,
    readback_map_copy_us: DurationAccumulator,
    payload_pack_us: DurationAccumulator,
    total_upload_bytes: u64,
    total_requested_pixels: u64,
}

impl RenderFixtureArgs {
    /// Render and write the deterministic fixture.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid sampling bounds, renderer/Vulkan failure,
    /// non-deterministic raster results, malformed portable updates, artifact
    /// writes, or manifest serialization.
    pub fn invoke(self) -> eyre::Result<CliOutput> {
        let transport = resolve_transport(self.format, self.transport);
        let sampling = RenderFixtureSamplingRequest::new(self.warmup_frames, self.sample_frames)?;
        let rendered = render_transport_fixture(self.renderer, transport, sampling)?;
        let path = self.output.map_or_else(
            || PathBuf::from(transport.default_path(self.renderer)),
            PathBuf::from,
        );
        fs::write(&path, &rendered.update.bytes)
            .wrap_err_with(|| format!("failed to write render fixture at {}", path.display()))?;

        let manifest_path = self.manifest.map(PathBuf::from);
        if let Some(manifest_path) = manifest_path.as_ref() {
            let manifest = manifest(transport, &rendered);
            let json = facet_json::to_string_pretty(&manifest)
                .wrap_err("failed to serialize render fixture manifest")?;
            fs::write(manifest_path, json.as_bytes()).wrap_err_with(|| {
                format!(
                    "failed to write render fixture manifest at {}",
                    manifest_path.display()
                )
            })?;
        }

        let descriptor = rendered.update.descriptor;
        Ok(CliOutput::facet(RenderFixtureOutput {
            path: path.display().to_string(),
            manifest_path: manifest_path.map(|path| path.display().to_string()),
            format: transport.format(),
            transport_id: transport.id().to_string(),
            renderer_id: rendered.renderer_id.to_string(),
            sequence: descriptor.sequence,
            base_sequence: descriptor.base_sequence,
            width: descriptor.width,
            height: descriptor.height,
            stride: transport.wire_stride(descriptor.stride),
            bytes: rendered.update.bytes.len(),
            regions: rendered.update.regions.len(),
            exact_equivalence: rendered.exact_equivalence,
        }))
    }
}

fn render_transport_fixture(
    renderer: RenderFixtureRenderer,
    transport: RenderFixtureTransport,
    sampling: RenderFixtureSamplingRequest,
) -> eyre::Result<RenderedFixture> {
    let fixture = match renderer {
        RenderFixtureRenderer::RustCpuFontdue => render_cpu_transport_fixture(transport, sampling),
        RenderFixtureRenderer::RustGpuSlug => render_gpu_transport_fixture(transport, sampling),
    }?;
    debug_assert_eq!(fixture.renderer_id, renderer.id());
    Ok(fixture)
}

fn resolve_transport(
    format: RenderFixtureFormat,
    transport: Option<RenderFixtureTransport>,
) -> RenderFixtureTransport {
    transport.unwrap_or(match format {
        RenderFixtureFormat::Png => RenderFixtureTransport::FullPng,
        RenderFixtureFormat::Raw => RenderFixtureTransport::FullRawRgba,
    })
}

fn difficult_fixture_terminal() -> TerminalSession {
    let mut terminal = TerminalSession::new(TerminalLimits::default(), 40, 12);
    terminal.process_bytes(
        concat!(
            "\x1b[1;34mTeamy Terminal\x1b[0m\r\n",
            "\x1b[1;36mcyan\x1b[0m wide=界 combining=e\u{0301} fallback=\u{10ffff}\r\n",
            "box=┌─┐ powerline=\u{e0b0}\r\n",
            "$ echo ready"
        )
        .as_bytes(),
    );
    terminal.process_bytes(b"\x1b[?1049h\x1b[2Jalternate-screen-witness\x1b[?1049l");
    terminal.resize(32, 10);
    terminal.resize(40, 12);
    terminal.process_bytes(b"\x1b[8;1H\x1b[6 q");
    terminal.begin_selection(CursorPosition { column: 0, row: 1 });
    terminal.update_selection(CursorPosition { column: 12, row: 1 });
    terminal
}

#[expect(
    clippy::too_many_lines,
    reason = "one ordered fixture transaction keeps timing and proof checkpoints auditable"
)]
fn render_cpu_transport_fixture(
    transport: RenderFixtureTransport,
    sampling: RenderFixtureSamplingRequest,
) -> eyre::Result<RenderedFixture> {
    let total_started = Instant::now();
    let setup_started = Instant::now();
    let mut terminal = difficult_fixture_terminal();
    let fixture_setup_us = elapsed_us(setup_started);
    let renderer_setup_started = Instant::now();
    let mut font = TerminalFont::new(DEFAULT_PIXEL_SIZE)?;
    let renderer_setup_us = elapsed_us(renderer_setup_started);

    let base_snapshot_started = Instant::now();
    let snapshot_a = terminal.snapshot();
    let snapshot_a_us = elapsed_us(base_snapshot_started);
    let width = u32::from(snapshot_a.columns) * font.cell_width();
    let height = u32::from(snapshot_a.rows) * font.cell_height();

    let raster_a_started = Instant::now();
    let frame_a = font.render(&snapshot_a, width, height);
    let raster_a_us = elapsed_us(raster_a_started);
    let after_base_render = font.cache_stats().into();

    let mut timings = RenderFixtureTimings {
        fixture_setup_us,
        renderer_setup_us,
        snapshot_a_us,
        raster_a_us,
        ..RenderFixtureTimings::default()
    };

    let base_pack_started = Instant::now();
    let base = RasterFrameUpdate::full_rgba(
        BASE_SEQUENCE,
        width,
        height,
        &frame_a.pixels,
        FrameCompression::Raw,
    )?;
    timings.base_pack_us = elapsed_us(base_pack_started);

    let mutation_started = Instant::now();
    terminal.process_bytes(b"\r$ echo dirty");
    timings.mutation_us = elapsed_us(mutation_started);

    let changed_snapshot_started = Instant::now();
    let snapshot_b = terminal.snapshot();
    timings.snapshot_b_us = elapsed_us(changed_snapshot_started);

    let damage_started = Instant::now();
    let (cell_regions, full_refresh) = damage_regions(Some(&snapshot_a), &snapshot_b);
    timings.damage_us = elapsed_us(damage_started);
    if full_refresh || cell_regions.is_empty() {
        eyre::bail!("deterministic fixture unexpectedly required a full or empty refresh");
    }
    let dirty_cell_regions = cell_regions_manifest(&cell_regions);
    let raster_regions = cell_regions
        .iter()
        .map(|region| RasterRect {
            x: u32::from(region.column) * font.cell_width(),
            y: u32::from(region.row) * font.cell_height(),
            width: u32::from(region.columns) * font.cell_width(),
            height: u32::from(region.rows) * font.cell_height(),
        })
        .collect::<Vec<_>>();

    let dirty_raster_started = Instant::now();
    let mut dirty_render_b = frame_a.clone();
    font.render_cells(&snapshot_b, &mut dirty_render_b, &cell_regions);
    timings.dirty_raster_us = elapsed_us(dirty_raster_started);
    let after_dirty_render = font.cache_stats().into();

    let direct_full_b_started = Instant::now();
    let direct_full_b = font.render(&snapshot_b, width, height);
    timings.direct_full_b_us = elapsed_us(direct_full_b_started);
    let after_direct_full_b_render = font.cache_stats().into();

    let dirty_pack_started = Instant::now();
    let dirty = RasterFrameUpdate::dirty_rgba(
        DIRTY_SEQUENCE,
        BASE_SEQUENCE,
        width,
        height,
        &dirty_render_b.pixels,
        &raster_regions,
    )?;
    timings.proof_dirty_pack_us = elapsed_us(dirty_pack_started);

    let compose_started = Instant::now();
    let mut composed = ComposedRasterFrame::from_full(&base)?;
    composed.apply(&dirty)?;
    timings.compose_us = elapsed_us(compose_started);
    let exact_equivalence = dirty_render_b == direct_full_b
        && composed.sequence == DIRTY_SEQUENCE
        && composed.pixels == direct_full_b.pixels;
    if !exact_equivalence {
        eyre::bail!("dirty fixture composition differs from the direct full B render");
    }

    let composition_proof = RenderFixtureCompositionProof {
        base_sequence: BASE_SEQUENCE,
        dirty_sequence: DIRTY_SEQUENCE,
        base_rgba_bytes: base.bytes.len(),
        dirty_payload_bytes: dirty.bytes.len(),
        direct_full_b_rgba_bytes: direct_full_b.pixels.len(),
        base_rgba_fnv1a64: fnv1a64_hex(&base.bytes),
        dirty_payload_fnv1a64: fnv1a64_hex(&dirty.bytes),
        direct_full_b_rgba_fnv1a64: fnv1a64_hex(&direct_full_b.pixels),
        composed_b_rgba_fnv1a64: fnv1a64_hex(&composed.pixels),
        dirty_regions: regions_manifest(&dirty),
        exact_equivalence,
    };

    let selected_pack_started = Instant::now();
    let update = match transport {
        RenderFixtureTransport::FullPng | RenderFixtureTransport::FullRawRgba => {
            RasterFrameUpdate::full_rgba(
                DIRTY_SEQUENCE,
                width,
                height,
                &direct_full_b.pixels,
                transport.compression(),
            )?
        }
        RenderFixtureTransport::DirtyRawRgba => dirty,
    };
    timings.selected_pack_us = elapsed_us(selected_pack_started);
    update.validate()?;
    let sampling_evidence = run_cpu_repeated_workload(
        &mut font,
        &snapshot_b,
        &frame_a,
        &cell_regions,
        &raster_regions,
        transport,
        sampling,
        &update.bytes,
    )?;
    let after_repeated_workload = font.cache_stats().into();
    timings.total_us = elapsed_us(total_started);
    Ok(RenderedFixture {
        renderer_id: CPU_RENDERER_ID,
        update,
        logical_columns: snapshot_b.columns,
        logical_rows: snapshot_b.rows,
        cell_width: font.cell_width(),
        cell_height: font.cell_height(),
        font_pixel_size_milli: pixel_size_milli(DEFAULT_PIXEL_SIZE),
        dirty_cell_regions,
        exact_equivalence,
        composition_proof,
        visual_comparison: None,
        cache: RenderFixtureCacheEvidence {
            cache_kind: "fontdue-glyph-lru".to_string(),
            cpu: Some(RenderFixtureCpuCacheEvidence {
                process_font_catalog_loads: TerminalFontCatalog::shared_system_loads(),
                after_base_render,
                after_dirty_render,
                after_direct_full_b_render,
                after_repeated_workload,
            }),
            gpu: None,
        },
        timings,
        gpu_stages: None,
        provenance: provenance(&snapshot_a, &snapshot_b, None),
        execution: RenderFixtureExecutionEvidence {
            rasterization_owner: "server".to_string(),
            execution_target: "cpu-memory".to_string(),
            no_window: true,
            uses_winit_window: false,
            uses_vulkan_surface: false,
            uses_swapchain: false,
            device: None,
        },
        sampling: sampling_evidence,
    })
}

#[expect(
    clippy::too_many_arguments,
    reason = "the repeated evidence workload keeps the selected fixture inputs explicit"
)]
fn run_cpu_repeated_workload(
    font: &mut TerminalFont,
    snapshot: &teamy_terminal_core::TerminalSnapshot,
    base_frame: &teamy_terminal_font::RasterizedFrame,
    cell_regions: &[CellRect],
    raster_regions: &[RasterRect],
    transport: RenderFixtureTransport,
    sampling: RenderFixtureSamplingRequest,
    expected_payload: &[u8],
) -> eyre::Result<RenderFixtureSamplingEvidence> {
    let mut renderer_durations = DurationAccumulator::new();
    let mut packet_durations = DurationAccumulator::new();
    let repeated_frames = sampling
        .warmup_frames
        .checked_add(sampling.sample_frames)
        .expect("validated sampling count cannot overflow");
    for index in 0..repeated_frames {
        let raster_started = Instant::now();
        let frame = if transport == RenderFixtureTransport::DirtyRawRgba {
            let mut frame = base_frame.clone();
            font.render_cells(snapshot, &mut frame, cell_regions);
            frame
        } else {
            font.render(snapshot, base_frame.width, base_frame.height)
        };
        let renderer_us = elapsed_us(raster_started);

        let packet_started = Instant::now();
        let update = match transport {
            RenderFixtureTransport::FullPng | RenderFixtureTransport::FullRawRgba => {
                RasterFrameUpdate::full_rgba(
                    DIRTY_SEQUENCE,
                    frame.width,
                    frame.height,
                    &frame.pixels,
                    transport.compression(),
                )?
            }
            RenderFixtureTransport::DirtyRawRgba => RasterFrameUpdate::dirty_rgba(
                DIRTY_SEQUENCE,
                BASE_SEQUENCE,
                frame.width,
                frame.height,
                &frame.pixels,
                raster_regions,
            )?,
        };
        let packet_us = elapsed_us(packet_started);
        if update.bytes != expected_payload {
            eyre::bail!(
                "CPU repeated evidence payload {index} differed from the deterministic artifact"
            );
        }
        if index >= sampling.warmup_frames {
            renderer_durations.push(renderer_us);
            packet_durations.push(packet_us);
        }
    }
    Ok(RenderFixtureSamplingEvidence {
        warmup_frames_requested: sampling.warmup_frames,
        warmup_frames_completed: sampling.warmup_frames,
        sample_frames_requested: sampling.sample_frames,
        sample_frames_completed: sampling.sample_frames,
        maximum_total_repeated_frames: MAX_TOTAL_REPEATED_FRAMES,
        first_repeated_frame_sequence: DIRTY_SEQUENCE + 1,
        last_repeated_frame_sequence: DIRTY_SEQUENCE + u64::from(repeated_frames),
        presentation_generation: "render-fixture-v2".to_string(),
        selected_workload: format!("{}+{}", CPU_RENDERER_ID, transport.id()),
        stable_stage_names: vec![
            "cpu_fontdue_raster".to_string(),
            "portable_packet_pack".to_string(),
        ],
        renderer_total_us: renderer_durations.finish(),
        packet_pack_us: packet_durations.finish(),
        gpu_stage_summaries: None,
        expected_payload_fnv1a64: fnv1a64_hex(expected_payload),
        deterministic_payloads: true,
    })
}

#[expect(
    clippy::too_many_lines,
    reason = "one ordered GPU transaction keeps lifecycle, timing, and proof checkpoints auditable"
)]
fn render_gpu_transport_fixture(
    transport: RenderFixtureTransport,
    sampling: RenderFixtureSamplingRequest,
) -> eyre::Result<RenderedFixture> {
    let total_started = Instant::now();
    let setup_started = Instant::now();
    let mut terminal = difficult_fixture_terminal();
    let fixture_setup_us = elapsed_us(setup_started);

    let renderer_setup_started = Instant::now();
    let context = VulkanOffscreenContext::new()
        .wrap_err("failed to create the no-window Vulkan evidence context")?;
    let runtime = GpuSlugRuntime::new(context)
        .wrap_err("failed to create the retained GPU Slug evidence runtime")?;
    let device_facts = runtime.device_facts().clone();
    if !device_facts.enabled_instance_extensions.is_empty()
        || !device_facts.enabled_device_extensions.is_empty()
    {
        eyre::bail!("no-window GPU evidence unexpectedly enabled Vulkan window-system extensions");
    }
    let mut renderer = runtime
        .create_renderer()
        .wrap_err("failed to create the retained GPU Slug evidence session")?;
    let metrics = renderer
        .surface_metrics(DEFAULT_PIXEL_SIZE)
        .wrap_err("failed to derive native GPU Slug fixture metrics")?;
    let renderer_setup_us = elapsed_us(renderer_setup_started);

    let base_snapshot_started = Instant::now();
    let snapshot_a = terminal.snapshot();
    let snapshot_a_us = elapsed_us(base_snapshot_started);
    let width = metrics.native_width(snapshot_a.columns);
    let height = metrics.native_height(snapshot_a.rows);

    let raster_a_started = Instant::now();
    let base_result = renderer
        .render(GpuSlugRenderRequest::full(
            &snapshot_a,
            width,
            height,
            DEFAULT_PIXEL_SIZE,
            gpu_identity(BASE_SEQUENCE),
        ))
        .wrap_err("failed to render the GPU Slug base fixture")?;
    let raster_a_us = elapsed_us(raster_a_started);
    let base_rgba = base_result
        .full_rgba
        .as_deref()
        .ok_or_else(|| eyre::eyre!("full GPU Slug base render omitted complete RGBA pixels"))?;

    let mut timings = RenderFixtureTimings {
        fixture_setup_us,
        renderer_setup_us,
        snapshot_a_us,
        raster_a_us,
        ..RenderFixtureTimings::default()
    };

    let base_pack_started = Instant::now();
    let base = RasterFrameUpdate::full_rgba(
        BASE_SEQUENCE,
        width,
        height,
        base_rgba,
        FrameCompression::Raw,
    )?;
    timings.base_pack_us = elapsed_us(base_pack_started);

    let mutation_started = Instant::now();
    terminal.process_bytes(b"\r$ echo dirty");
    timings.mutation_us = elapsed_us(mutation_started);

    let changed_snapshot_started = Instant::now();
    let snapshot_b = terminal.snapshot();
    timings.snapshot_b_us = elapsed_us(changed_snapshot_started);

    let damage_started = Instant::now();
    let (cell_regions, full_refresh) = damage_regions(Some(&snapshot_a), &snapshot_b);
    timings.damage_us = elapsed_us(damage_started);
    if full_refresh || cell_regions.is_empty() {
        eyre::bail!("deterministic GPU fixture unexpectedly required a full or empty refresh");
    }
    let dirty_cell_regions = cell_regions_manifest(&cell_regions);

    let dirty_raster_started = Instant::now();
    let dirty_result = renderer
        .render(GpuSlugRenderRequest {
            snapshot: &snapshot_b,
            width,
            height,
            font_pixel_size: DEFAULT_PIXEL_SIZE,
            damage: &cell_regions,
            force_full_refresh: false,
            identity: gpu_identity(DIRTY_SEQUENCE),
            timeout: std::time::Duration::from_secs(5),
            cancellation: None,
        })
        .wrap_err("failed to render the GPU Slug dirty fixture")?;
    timings.dirty_raster_us = elapsed_us(dirty_raster_started);

    let packed_regions = dirty_result
        .dirty_regions
        .iter()
        .map(|region| PackedRasterRegion {
            rect: RasterRect {
                x: region.x,
                y: region.y,
                width: region.width,
                height: region.height,
            },
            rgba: &region.rgba,
        })
        .collect::<Vec<_>>();
    let dirty_pack_started = Instant::now();
    let dirty = RasterFrameUpdate::dirty_rgba_packed(
        DIRTY_SEQUENCE,
        BASE_SEQUENCE,
        width,
        height,
        &packed_regions,
    )?;
    timings.proof_dirty_pack_us = elapsed_us(dirty_pack_started);

    let direct_full_b_started = Instant::now();
    let direct_result = renderer
        .render(GpuSlugRenderRequest::full(
            &snapshot_b,
            width,
            height,
            DEFAULT_PIXEL_SIZE,
            gpu_identity(DIRTY_SEQUENCE),
        ))
        .wrap_err("failed to render the direct GPU Slug comparison fixture")?;
    timings.direct_full_b_us = elapsed_us(direct_full_b_started);
    let direct_full_b_rgba = direct_result.full_rgba.as_deref().ok_or_else(|| {
        eyre::eyre!("direct full GPU Slug comparison omitted complete RGBA pixels")
    })?;

    let compose_started = Instant::now();
    let mut composed = ComposedRasterFrame::from_full(&base)?;
    composed.apply(&dirty)?;
    timings.compose_us = elapsed_us(compose_started);
    let exact_equivalence =
        composed.sequence == DIRTY_SEQUENCE && composed.pixels.as_slice() == direct_full_b_rgba;
    if !exact_equivalence {
        eyre::bail!("GPU dirty fixture composition differs from the direct full B render");
    }

    let composition_proof = RenderFixtureCompositionProof {
        base_sequence: BASE_SEQUENCE,
        dirty_sequence: DIRTY_SEQUENCE,
        base_rgba_bytes: base.bytes.len(),
        dirty_payload_bytes: dirty.bytes.len(),
        direct_full_b_rgba_bytes: direct_full_b_rgba.len(),
        base_rgba_fnv1a64: fnv1a64_hex(&base.bytes),
        dirty_payload_fnv1a64: fnv1a64_hex(&dirty.bytes),
        direct_full_b_rgba_fnv1a64: fnv1a64_hex(direct_full_b_rgba),
        composed_b_rgba_fnv1a64: fnv1a64_hex(&composed.pixels),
        dirty_regions: regions_manifest(&dirty),
        exact_equivalence,
    };

    let selected_pack_started = Instant::now();
    let update = match transport {
        RenderFixtureTransport::FullPng | RenderFixtureTransport::FullRawRgba => {
            RasterFrameUpdate::full_rgba(
                DIRTY_SEQUENCE,
                width,
                height,
                direct_full_b_rgba,
                transport.compression(),
            )?
        }
        RenderFixtureTransport::DirtyRawRgba => dirty,
    };
    timings.selected_pack_us = elapsed_us(selected_pack_started);
    update.validate()?;
    let sampling_evidence = run_gpu_repeated_workload(
        &mut renderer,
        &snapshot_b,
        &cell_regions,
        width,
        height,
        transport,
        sampling,
        &update.bytes,
    )?;
    let after_repeated_workload = renderer.cache_stats().into();
    timings.total_us = elapsed_us(total_started);
    let reference_raster_started = Instant::now();
    let mut cpu_reference = TerminalFont::new(metrics.font_pixel_size)?;
    let cpu_reference_frame = cpu_reference.render(&snapshot_b, width, height);
    let reference_raster_us = elapsed_us(reference_raster_started);
    let comparison_started = Instant::now();
    let mut visual_comparison = compare_rgba(
        &cpu_reference_frame.pixels,
        direct_result
            .full_rgba
            .as_deref()
            .ok_or_else(|| eyre::eyre!("GPU direct-full comparison omitted RGBA pixels"))?,
        width,
        height,
    )?;
    visual_comparison.reference_raster_us = reference_raster_us;
    visual_comparison.comparison_us = elapsed_us(comparison_started);

    let mut base_stats = base_result.stats;
    base_stats.raw_packet_pack_us = Some(timings.base_pack_us);
    let mut dirty_stats = dirty_result.stats;
    dirty_stats.raw_packet_pack_us = Some(timings.proof_dirty_pack_us);
    let mut direct_stats = direct_result.stats;
    match transport {
        RenderFixtureTransport::FullPng => {
            direct_stats.png_packet_pack_us = Some(timings.selected_pack_us);
        }
        RenderFixtureTransport::FullRawRgba => {
            direct_stats.raw_packet_pack_us = Some(timings.selected_pack_us);
        }
        RenderFixtureTransport::DirtyRawRgba => {}
    }

    Ok(RenderedFixture {
        renderer_id: RUST_GPU_SLUG_RENDERER_ID,
        update,
        logical_columns: snapshot_b.columns,
        logical_rows: snapshot_b.rows,
        cell_width: metrics.cell_width,
        cell_height: metrics.cell_height,
        font_pixel_size_milli: pixel_size_milli(metrics.font_pixel_size),
        dirty_cell_regions,
        exact_equivalence,
        composition_proof,
        visual_comparison: Some(visual_comparison),
        cache: RenderFixtureCacheEvidence {
            cache_kind: "gpu-slug-geometry-target-buffer".to_string(),
            cpu: None,
            gpu: Some(RenderFixtureGpuCacheEvidence {
                after_base_render: base_result.cache.into(),
                after_dirty_render: dirty_result.cache.into(),
                after_direct_full_b_render: direct_result.cache.into(),
                after_repeated_workload,
            }),
        },
        timings,
        gpu_stages: Some(RenderFixtureGpuStageEvidence {
            base_full_a: base_stats.into(),
            dirty_b: dirty_stats.into(),
            direct_full_b: direct_stats.into(),
        }),
        provenance: provenance(
            &snapshot_a,
            &snapshot_b,
            Some(GPU_SLUG_SHADER_SOURCE_IDENTITY),
        ),
        execution: RenderFixtureExecutionEvidence {
            rasterization_owner: "server".to_string(),
            execution_target: "vulkan-offscreen-no-window".to_string(),
            no_window: true,
            uses_winit_window: false,
            uses_vulkan_surface: false,
            uses_swapchain: false,
            device: Some(device_facts.into()),
        },
        sampling: sampling_evidence,
    })
}

#[expect(
    clippy::too_many_arguments,
    reason = "the repeated evidence workload keeps the selected fixture inputs explicit"
)]
#[expect(
    clippy::too_many_lines,
    reason = "the selected GPU tuple is rendered, packed, and checked atomically"
)]
fn run_gpu_repeated_workload(
    renderer: &mut GpuSlugRenderer,
    snapshot: &teamy_terminal_core::TerminalSnapshot,
    cell_regions: &[CellRect],
    width: u32,
    height: u32,
    transport: RenderFixtureTransport,
    sampling: RenderFixtureSamplingRequest,
    expected_payload: &[u8],
) -> eyre::Result<RenderFixtureSamplingEvidence> {
    let mut renderer_durations = DurationAccumulator::new();
    let mut packet_durations = DurationAccumulator::new();
    let mut gpu_stages = GpuStageAccumulators::new();
    let repeated_frames = sampling
        .warmup_frames
        .checked_add(sampling.sample_frames)
        .expect("validated sampling count cannot overflow");
    for index in 0..repeated_frames {
        let frame_sequence = DIRTY_SEQUENCE
            .saturating_add(1)
            .saturating_add(u64::from(index));
        let result = if transport == RenderFixtureTransport::DirtyRawRgba {
            renderer
                .render(GpuSlugRenderRequest {
                    snapshot,
                    width,
                    height,
                    font_pixel_size: DEFAULT_PIXEL_SIZE,
                    damage: cell_regions,
                    force_full_refresh: false,
                    identity: gpu_identity(frame_sequence),
                    timeout: std::time::Duration::from_secs(5),
                    cancellation: None,
                })
                .wrap_err_with(|| format!("failed GPU dirty repeated evidence frame {index}"))?
        } else {
            renderer
                .render(GpuSlugRenderRequest::full(
                    snapshot,
                    width,
                    height,
                    DEFAULT_PIXEL_SIZE,
                    gpu_identity(frame_sequence),
                ))
                .wrap_err_with(|| format!("failed GPU full repeated evidence frame {index}"))?
        };
        let renderer_us = result.stats.total_us;

        let packet_started = Instant::now();
        let update = match transport {
            RenderFixtureTransport::FullPng | RenderFixtureTransport::FullRawRgba => {
                let rgba = result.full_rgba.as_deref().ok_or_else(|| {
                    eyre::eyre!("GPU full repeated evidence frame {index} omitted RGBA pixels")
                })?;
                RasterFrameUpdate::full_rgba(
                    frame_sequence,
                    width,
                    height,
                    rgba,
                    transport.compression(),
                )?
            }
            RenderFixtureTransport::DirtyRawRgba => {
                let packed = result
                    .dirty_regions
                    .iter()
                    .map(|region| PackedRasterRegion {
                        rect: RasterRect {
                            x: region.x,
                            y: region.y,
                            width: region.width,
                            height: region.height,
                        },
                        rgba: &region.rgba,
                    })
                    .collect::<Vec<_>>();
                RasterFrameUpdate::dirty_rgba_packed(
                    frame_sequence,
                    BASE_SEQUENCE,
                    width,
                    height,
                    &packed,
                )?
            }
        };
        let packet_us = elapsed_us(packet_started);
        if update.bytes != expected_payload {
            eyre::bail!(
                "GPU repeated evidence payload {index} differed from the deterministic artifact"
            );
        }
        if index >= sampling.warmup_frames {
            renderer_durations.push(renderer_us);
            packet_durations.push(packet_us);
            gpu_stages.push(result.stats);
        }
    }
    Ok(RenderFixtureSamplingEvidence {
        warmup_frames_requested: sampling.warmup_frames,
        warmup_frames_completed: sampling.warmup_frames,
        sample_frames_requested: sampling.sample_frames,
        sample_frames_completed: sampling.sample_frames,
        maximum_total_repeated_frames: MAX_TOTAL_REPEATED_FRAMES,
        first_repeated_frame_sequence: DIRTY_SEQUENCE + 1,
        last_repeated_frame_sequence: DIRTY_SEQUENCE + u64::from(repeated_frames),
        presentation_generation: "render-fixture-v2".to_string(),
        selected_workload: format!("{}+{}", RUST_GPU_SLUG_RENDERER_ID, transport.id()),
        stable_stage_names: GPU_TRACY_STAGE_NAMES
            .iter()
            .map(|name| (*name).to_string())
            .collect(),
        renderer_total_us: renderer_durations.finish(),
        packet_pack_us: packet_durations.finish(),
        gpu_stage_summaries: Some(gpu_stages.finish()),
        expected_payload_fnv1a64: fnv1a64_hex(expected_payload),
        deterministic_payloads: true,
    })
}

fn compare_rgba(
    reference: &[u8],
    candidate: &[u8],
    width: u32,
    height: u32,
) -> eyre::Result<RenderFixtureVisualComparison> {
    let pixel_count = u64::from(width).saturating_mul(u64::from(height));
    let expected_bytes = pixel_count.saturating_mul(4);
    let expected_bytes_usize = usize::try_from(expected_bytes)
        .wrap_err("cross-render comparison dimensions exceed addressable memory")?;
    if reference.len() != expected_bytes_usize || candidate.len() != expected_bytes_usize {
        eyre::bail!(
            "cross-render comparison expected {expected_bytes} RGBA bytes, got reference={} candidate={}",
            reference.len(),
            candidate.len()
        );
    }

    let mut differing_pixels = 0_u64;
    let mut alpha_differing_pixels = 0_u64;
    let mut max_channel_absolute_delta = 0_u8;
    let mut total_channel_absolute_delta = 0_u64;
    let mut min_x = width;
    let mut min_y = height;
    let mut max_x = 0_u32;
    let mut max_y = 0_u32;
    for (index, (reference_pixel, candidate_pixel)) in reference
        .chunks_exact(4)
        .zip(candidate.chunks_exact(4))
        .enumerate()
    {
        let mut differs = false;
        for channel in 0..4 {
            let delta = reference_pixel[channel].abs_diff(candidate_pixel[channel]);
            differs |= delta != 0;
            max_channel_absolute_delta = max_channel_absolute_delta.max(delta);
            total_channel_absolute_delta =
                total_channel_absolute_delta.saturating_add(u64::from(delta));
        }
        if reference_pixel[3] != candidate_pixel[3] {
            alpha_differing_pixels = alpha_differing_pixels.saturating_add(1);
        }
        if differs {
            differing_pixels = differing_pixels.saturating_add(1);
            let index = u64::try_from(index).unwrap_or(u64::MAX);
            let x = u32::try_from(index % u64::from(width.max(1))).unwrap_or(u32::MAX);
            let y = u32::try_from(index / u64::from(width.max(1))).unwrap_or(u32::MAX);
            min_x = min_x.min(x);
            min_y = min_y.min(y);
            max_x = max_x.max(x);
            max_y = max_y.max(y);
        }
    }
    let differences_present = differing_pixels != 0;
    if !differences_present {
        min_x = 0;
        min_y = 0;
    }
    Ok(RenderFixtureVisualComparison {
        reference_renderer_id: CPU_RENDERER_ID.to_string(),
        candidate_renderer_id: RUST_GPU_SLUG_RENDERER_ID.to_string(),
        width,
        height,
        pixel_count,
        differing_pixels,
        differing_pixels_basis_points: u32::try_from(
            differing_pixels.saturating_mul(10_000) / pixel_count.max(1),
        )
        .unwrap_or(10_000),
        alpha_differing_pixels,
        max_channel_absolute_delta,
        total_channel_absolute_delta,
        mean_channel_absolute_delta_milli: total_channel_absolute_delta.saturating_mul(1_000)
            / expected_bytes.max(1),
        differences_present,
        difference_min_x: min_x,
        difference_min_y: min_y,
        difference_max_x: max_x,
        difference_max_y: max_y,
        reference_raster_us: 0,
        comparison_us: 0,
    })
}

fn manifest(
    transport: RenderFixtureTransport,
    rendered: &RenderedFixture,
) -> RenderFixtureManifest {
    let descriptor = rendered.update.descriptor;
    RenderFixtureManifest {
        manifest_version: 2,
        transport_id: transport.id().to_string(),
        renderer_id: rendered.renderer_id.to_string(),
        damage_mode: transport.damage_mode().to_string(),
        encoding: transport.encoding_id().to_string(),
        frame_kind: transport.frame_kind_id().to_string(),
        contract_version: descriptor.contract_version,
        sequence: descriptor.sequence,
        base_sequence: descriptor.base_sequence,
        width: descriptor.width,
        height: descriptor.height,
        logical_columns: rendered.logical_columns,
        logical_rows: rendered.logical_rows,
        frame_stride: descriptor.width.saturating_mul(4),
        update_stride: transport.wire_stride(descriptor.stride),
        bytes_per_pixel: 4,
        origin: "top-left".to_string(),
        alpha_mode: "straight".to_string(),
        color_space: "srgb".to_string(),
        font_id: EMBEDDED_FONT_ID.to_string(),
        font_sha256: EMBEDDED_FONT_SHA256.to_string(),
        font_pixel_size_milli: rendered.font_pixel_size_milli,
        cell_width: rendered.cell_width,
        cell_height: rendered.cell_height,
        payload_bytes: rendered.update.bytes.len(),
        payload_fnv1a64: fnv1a64_hex(&rendered.update.bytes),
        region_count: rendered.update.regions.len(),
        regions: regions_manifest(&rendered.update),
        dirty_cell_regions: rendered.dirty_cell_regions.clone(),
        exact_equivalence: rendered.exact_equivalence,
        composition_proof: rendered.composition_proof.clone(),
        visual_comparison: rendered.visual_comparison.clone(),
        cache: rendered.cache.clone(),
        timings: rendered.timings.clone(),
        gpu_stages: rendered.gpu_stages.clone(),
        provenance: rendered.provenance.clone(),
        execution: rendered.execution.clone(),
        sampling: rendered.sampling.clone(),
    }
}

fn regions_manifest(update: &RasterFrameUpdate) -> Vec<RenderFixtureRegionManifest> {
    update
        .regions
        .iter()
        .map(|region| RenderFixtureRegionManifest {
            x: region.x,
            y: region.y,
            width: region.width,
            height: region.height,
            stride: region.stride,
            payload_offset: region.payload_offset,
            payload_length: region.payload_length,
        })
        .collect()
}

fn cell_regions_manifest(regions: &[CellRect]) -> Vec<RenderFixtureCellRegionManifest> {
    regions
        .iter()
        .map(|region| RenderFixtureCellRegionManifest {
            column: region.column,
            row: region.row,
            columns: region.columns,
            rows: region.rows,
        })
        .collect()
}

fn gpu_identity(frame_sequence: u64) -> GpuSlugRenderIdentity {
    GpuSlugRenderIdentity {
        session_sequence: 1,
        terminal_sequence: frame_sequence,
        frame_sequence,
        presentation_generation: "render-fixture-v2".to_string(),
        correlation_id: format!("render-fixture-v2-{frame_sequence}"),
    }
}

fn provenance(
    snapshot_a: &teamy_terminal_core::TerminalSnapshot,
    snapshot_b: &teamy_terminal_core::TerminalSnapshot,
    shader_source_identity: Option<u64>,
) -> RenderFixtureProvenance {
    RenderFixtureProvenance {
        fixture_id: FIXTURE_ID.to_string(),
        package_version: env!("CARGO_PKG_VERSION").to_string(),
        repository: env!("GIT_REPOSITORY_URL").to_string(),
        branch: env!("GIT_BRANCH").to_string(),
        revision: env!("GIT_REVISION").to_string(),
        worktree_status: env!("GIT_WORKTREE_STATUS").to_string(),
        build_timestamp_unix: option_env!("BUILD_TIMESTAMP_UNIX")
            .unwrap_or("unknown")
            .to_string(),
        rustc_version: env!("RUSTC_VERSION").to_string(),
        build_target: env!("BUILD_TARGET").to_string(),
        build_host: env!("BUILD_HOST").to_string(),
        build_profile: env!("BUILD_PROFILE").to_string(),
        tracy_feature_enabled: cfg!(feature = "tracy"),
        extended_observability_feature_enabled: cfg!(feature = "extended_observability"),
        target_os: std::env::consts::OS.to_string(),
        target_arch: std::env::consts::ARCH.to_string(),
        snapshot_hash_algorithm: "fnv1a64-rust-debug-v1".to_string(),
        snapshot_a_semantic_fnv1a64: fnv1a64_hex(format!("{snapshot_a:?}").as_bytes()),
        snapshot_b_semantic_fnv1a64: fnv1a64_hex(format!("{snapshot_b:?}").as_bytes()),
        shader_source_fnv1a64: shader_source_identity.map(|value| format!("{value:016x}")),
    }
}

#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "renderer pixel sizes are finite and bounded to 1..=64 before this conversion"
)]
fn pixel_size_milli(pixel_size: f32) -> u32 {
    (pixel_size * 1000.0).round() as u32
}

impl RenderFixtureSamplingRequest {
    fn new(warmup_frames: u32, sample_frames: Option<u32>) -> eyre::Result<Self> {
        let sample_frames = sample_frames.unwrap_or(DEFAULT_SAMPLE_FRAMES);
        if sample_frames == 0 {
            eyre::bail!("sample-frames must be at least one");
        }
        let total = warmup_frames
            .checked_add(sample_frames)
            .ok_or_else(|| eyre::eyre!("warmup/sample frame count overflows u32"))?;
        if total > MAX_TOTAL_REPEATED_FRAMES {
            eyre::bail!(
                "warmup-frames plus sample-frames must not exceed {MAX_TOTAL_REPEATED_FRAMES}"
            );
        }
        Ok(Self {
            warmup_frames,
            sample_frames,
        })
    }

    #[cfg(test)]
    const fn single() -> Self {
        Self {
            warmup_frames: 0,
            sample_frames: DEFAULT_SAMPLE_FRAMES,
        }
    }
}

impl DurationAccumulator {
    const fn new() -> Self {
        Self {
            samples: 0,
            total_us: 0,
            min_us: u64::MAX,
            max_us: 0,
        }
    }

    fn push(&mut self, duration_us: u64) {
        self.samples = self.samples.saturating_add(1);
        self.total_us = self.total_us.saturating_add(duration_us);
        self.min_us = self.min_us.min(duration_us);
        self.max_us = self.max_us.max(duration_us);
    }

    fn finish(self) -> RenderFixtureDurationSummary {
        let min_us = if self.samples == 0 { 0 } else { self.min_us };
        RenderFixtureDurationSummary {
            samples: self.samples,
            total_us: self.total_us,
            min_us,
            max_us: self.max_us,
            mean_us: self.total_us / u64::from(self.samples.max(1)),
        }
    }
}

impl GpuStageAccumulators {
    const fn new() -> Self {
        Self {
            geometry_build_us: DurationAccumulator::new(),
            outline_extraction_us: DurationAccumulator::new(),
            directional_band_build_us: DurationAccumulator::new(),
            upload_us: DurationAccumulator::new(),
            command_record_us: DurationAccumulator::new(),
            queue_submit_us: DurationAccumulator::new(),
            gpu_completion_wait_us: DurationAccumulator::new(),
            readback_map_copy_us: DurationAccumulator::new(),
            payload_pack_us: DurationAccumulator::new(),
            total_upload_bytes: 0,
            total_requested_pixels: 0,
        }
    }

    fn push(&mut self, stats: GpuSlugRenderStats) {
        self.geometry_build_us.push(stats.geometry_build_us);
        self.outline_extraction_us.push(stats.outline_extraction_us);
        self.directional_band_build_us
            .push(stats.directional_band_build_us);
        self.upload_us.push(stats.upload_us);
        self.command_record_us.push(stats.command_record_us);
        self.queue_submit_us.push(stats.queue_submit_us);
        self.gpu_completion_wait_us
            .push(stats.gpu_completion_wait_us);
        self.readback_map_copy_us.push(stats.readback_map_copy_us);
        self.payload_pack_us.push(stats.payload_pack_us);
        self.total_upload_bytes = self.total_upload_bytes.saturating_add(stats.upload_bytes);
        self.total_requested_pixels = self
            .total_requested_pixels
            .saturating_add(stats.requested_pixels);
    }

    fn finish(self) -> RenderFixtureGpuStageSummaries {
        RenderFixtureGpuStageSummaries {
            geometry_build_us: self.geometry_build_us.finish(),
            outline_extraction_us: self.outline_extraction_us.finish(),
            directional_band_build_us: self.directional_band_build_us.finish(),
            upload_us: self.upload_us.finish(),
            command_record_us: self.command_record_us.finish(),
            queue_submit_us: self.queue_submit_us.finish(),
            gpu_completion_wait_us: self.gpu_completion_wait_us.finish(),
            readback_map_copy_us: self.readback_map_copy_us.finish(),
            payload_pack_us: self.payload_pack_us.finish(),
            total_upload_bytes: self.total_upload_bytes,
            total_requested_pixels: self.total_requested_pixels,
        }
    }
}

fn fnv1a64_hex(bytes: &[u8]) -> String {
    let hash = bytes.iter().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
    });
    format!("{hash:016x}")
}

impl From<TerminalFontCacheStats> for RenderFixtureFontCacheStats {
    fn from(stats: TerminalFontCacheStats) -> Self {
        Self {
            hits: stats.hits,
            misses: stats.misses,
            evictions: stats.evictions,
            cached_glyphs: stats.cached_glyphs,
            capacity: stats.capacity,
        }
    }
}

impl From<VulkanOffscreenCacheStats> for RenderFixtureTargetCacheStats {
    fn from(stats: VulkanOffscreenCacheStats) -> Self {
        Self {
            capacity: stats.capacity,
            retained_targets: stats.retained_targets,
            allocations: stats.allocations,
            reuses: stats.reuses,
            evictions: stats.evictions,
            destructions: stats.destructions,
        }
    }
}

impl From<GpuSlugCacheStats> for RenderFixtureGpuCacheStats {
    fn from(stats: GpuSlugCacheStats) -> Self {
        Self {
            geometry_hits: stats.geometry_hits,
            geometry_misses: stats.geometry_misses,
            geometry_evictions: stats.geometry_evictions,
            retained_glyphs: stats.retained_glyphs,
            glyph_capacity: stats.glyph_capacity,
            buffer_allocations: stats.buffer_allocations,
            targets: stats.targets.into(),
        }
    }
}

impl From<GpuSlugRenderStats> for RenderFixtureGpuRenderStats {
    fn from(stats: GpuSlugRenderStats) -> Self {
        Self {
            geometry_build_us: stats.geometry_build_us,
            outline_extraction_us: stats.outline_extraction_us,
            directional_band_build_us: stats.directional_band_build_us,
            upload_bytes: stats.upload_bytes,
            upload_us: stats.upload_us,
            command_record_us: stats.command_record_us,
            queue_submit_us: stats.queue_submit_us,
            gpu_completion_wait_us: stats.gpu_completion_wait_us,
            readback_map_copy_us: stats.readback_map_copy_us,
            payload_pack_us: stats.payload_pack_us,
            full_rgba_pack_us: stats.full_rgba_pack_us,
            dirty_region_pack_us: stats.dirty_region_pack_us,
            raw_packet_pack_us: stats.raw_packet_pack_us,
            png_packet_pack_us: stats.png_packet_pack_us,
            total_us: stats.total_us,
            requested_pixels: stats.requested_pixels,
            target_allocation_id: stats.target_allocation_id,
        }
    }
}

impl From<VulkanDeviceFacts> for RenderFixtureDeviceEvidence {
    fn from(facts: VulkanDeviceFacts) -> Self {
        Self {
            device_name: facts.device_name,
            vendor_id: facts.vendor_id,
            device_id: facts.device_id,
            driver_version: facts.driver_version,
            api_version: facts.api_version,
            queue_family_index: facts.queue_family_index,
            queue_flags: format!("{:?}", facts.queue_flags),
            max_image_dimension_2d: facts.max_image_dimension_2d,
            enabled_instance_extensions: facts.enabled_instance_extensions,
            enabled_device_extensions: facts.enabled_device_extensions,
        }
    }
}

fn elapsed_us(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX)
}

impl RenderFixtureTransport {
    const fn id(self) -> &'static str {
        match self {
            Self::FullPng => "full-png",
            Self::FullRawRgba => "full-raw-rgba",
            Self::DirtyRawRgba => "dirty-raw-rgba",
        }
    }

    const fn damage_mode(self) -> &'static str {
        match self {
            Self::FullPng | Self::FullRawRgba => "full",
            Self::DirtyRawRgba => "dirty",
        }
    }

    const fn encoding_id(self) -> &'static str {
        match self {
            Self::FullPng => "png",
            Self::FullRawRgba | Self::DirtyRawRgba => "rgba8",
        }
    }

    const fn frame_kind_id(self) -> &'static str {
        match self {
            Self::FullPng | Self::FullRawRgba => "full",
            Self::DirtyRawRgba => "dirty-regions",
        }
    }

    const fn compression(self) -> FrameCompression {
        match self {
            Self::FullPng => FrameCompression::Png,
            Self::FullRawRgba | Self::DirtyRawRgba => FrameCompression::Raw,
        }
    }

    const fn format(self) -> RenderFixtureFormat {
        match self {
            Self::FullPng => RenderFixtureFormat::Png,
            Self::FullRawRgba | Self::DirtyRawRgba => RenderFixtureFormat::Raw,
        }
    }

    const fn wire_stride(self, rgba_stride: u32) -> u32 {
        match self {
            Self::FullPng | Self::DirtyRawRgba => 0,
            Self::FullRawRgba => rgba_stride,
        }
    }

    const fn default_path(self, renderer: RenderFixtureRenderer) -> &'static str {
        match (renderer, self) {
            (RenderFixtureRenderer::RustCpuFontdue, Self::FullPng) => "teamy-terminal-fixture.png",
            (RenderFixtureRenderer::RustCpuFontdue, Self::FullRawRgba) => {
                "teamy-terminal-fixture.rgba"
            }
            (RenderFixtureRenderer::RustCpuFontdue, Self::DirtyRawRgba) => {
                "teamy-terminal-fixture.dirty.rgba"
            }
            (RenderFixtureRenderer::RustGpuSlug, Self::FullPng) => {
                "teamy-terminal-gpu-slug-fixture.png"
            }
            (RenderFixtureRenderer::RustGpuSlug, Self::FullRawRgba) => {
                "teamy-terminal-gpu-slug-fixture.rgba"
            }
            (RenderFixtureRenderer::RustGpuSlug, Self::DirtyRawRgba) => {
                "teamy-terminal-gpu-slug-fixture.dirty.rgba"
            }
        }
    }
}

impl RenderFixtureRenderer {
    const fn id(self) -> &'static str {
        match self {
            Self::RustCpuFontdue => CPU_RENDERER_ID,
            Self::RustGpuSlug => RUST_GPU_SLUG_RENDERER_ID,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_formats_resolve_to_stable_full_transport_ids() {
        assert_eq!(
            resolve_transport(RenderFixtureFormat::Png, None),
            RenderFixtureTransport::FullPng
        );
        assert_eq!(
            resolve_transport(RenderFixtureFormat::Raw, None),
            RenderFixtureTransport::FullRawRgba
        );
        assert_eq!(
            resolve_transport(
                RenderFixtureFormat::Png,
                Some(RenderFixtureTransport::DirtyRawRgba)
            ),
            RenderFixtureTransport::DirtyRawRgba
        );
        assert_eq!(RenderFixtureTransport::FullPng.id(), "full-png");
        assert_eq!(RenderFixtureTransport::FullRawRgba.id(), "full-raw-rgba");
        assert_eq!(RenderFixtureTransport::DirtyRawRgba.id(), "dirty-raw-rgba");
        assert_eq!(RenderFixtureRenderer::default().id(), CPU_RENDERER_ID);
        assert_eq!(
            RenderFixtureRenderer::RustGpuSlug.id(),
            RUST_GPU_SLUG_RENDERER_ID
        );
        assert_eq!(
            RenderFixtureTransport::FullPng.default_path(RenderFixtureRenderer::default()),
            "teamy-terminal-fixture.png"
        );
    }

    #[test]
    fn fixture_state_covers_difficult_semantics_before_raster_comparison() {
        let snapshot = difficult_fixture_terminal().snapshot();
        let text = snapshot.visible_text();
        assert_eq!(snapshot.columns, 40);
        assert_eq!(snapshot.rows, 12);
        assert_eq!(
            snapshot.cursor_shape,
            teamy_terminal_core::CursorShape::SteadyBar
        );
        assert!(snapshot.selection.is_some());
        assert!(snapshot.is_selected(0, 1));
        assert!(text.contains("cyan"));
        assert!(text.contains('界'));
        assert!(text.contains("e\u{0301}"));
        assert!(text.contains('\u{10ffff}'));
        assert!(text.contains('┌'));
        assert!(text.contains('\u{e0b0}'));
        assert!(!text.contains("alternate-screen-witness"));
        assert!(snapshot.cells.iter().any(|cell| cell.combining_count() > 0));
    }

    #[test]
    fn repeated_evidence_counts_are_explicit_defaulted_and_bounded() {
        assert_eq!(
            RenderFixtureSamplingRequest::new(0, None).expect("default sampling"),
            RenderFixtureSamplingRequest::single()
        );
        let _error = RenderFixtureSamplingRequest::new(0, Some(0))
            .expect_err("zero measured samples must be rejected");
        let _error = RenderFixtureSamplingRequest::new(MAX_TOTAL_REPEATED_FRAMES, Some(1))
            .expect_err("sampling beyond the total frame bound must be rejected");
        assert_eq!(
            RenderFixtureSamplingRequest::new(25, Some(75)).expect("bounded sampling"),
            RenderFixtureSamplingRequest {
                warmup_frames: 25,
                sample_frames: 75,
            }
        );
    }

    #[test]
    fn dirty_transport_fixture_composes_exactly_to_direct_full_b() {
        let rendered = render_cpu_transport_fixture(
            RenderFixtureTransport::DirtyRawRgba,
            RenderFixtureSamplingRequest::single(),
        )
        .expect("dirty fixture should render and compose");

        assert!(rendered.exact_equivalence);
        assert_eq!(rendered.update.descriptor.sequence, DIRTY_SEQUENCE);
        assert_eq!(rendered.update.descriptor.base_sequence, BASE_SEQUENCE);
        assert!(!rendered.update.regions.is_empty());
        assert!(!rendered.update.bytes.is_empty());
        rendered
            .update
            .validate()
            .expect("dirty fixture update should validate");
    }

    #[test]
    fn manifests_expose_comparable_contract_font_timing_cache_and_proof_evidence() {
        let full_png = render_cpu_transport_fixture(
            RenderFixtureTransport::FullPng,
            RenderFixtureSamplingRequest::single(),
        )
        .expect("full PNG fixture");
        let full_raw = render_cpu_transport_fixture(
            RenderFixtureTransport::FullRawRgba,
            RenderFixtureSamplingRequest::single(),
        )
        .expect("full raw fixture");
        let dirty_raw = render_cpu_transport_fixture(
            RenderFixtureTransport::DirtyRawRgba,
            RenderFixtureSamplingRequest::single(),
        )
        .expect("dirty raw fixture");
        let manifests = [
            manifest(RenderFixtureTransport::FullPng, &full_png),
            manifest(RenderFixtureTransport::FullRawRgba, &full_raw),
            manifest(RenderFixtureTransport::DirtyRawRgba, &dirty_raw),
        ];

        for manifest in &manifests {
            assert_eq!(manifest.renderer_id, CPU_RENDERER_ID);
            assert_eq!(manifest.contract_version, 1);
            assert_eq!(manifest.logical_columns, 40);
            assert_eq!(manifest.logical_rows, 12);
            assert_eq!(manifest.font_id, EMBEDDED_FONT_ID);
            assert_eq!(manifest.font_sha256, EMBEDDED_FONT_SHA256);
            assert_eq!(manifest.bytes_per_pixel, 4);
            assert!(manifest.timings.total_us >= manifest.timings.damage_us);
            let cpu_cache = manifest.cache.cpu.as_ref().expect("CPU cache evidence");
            assert!(cpu_cache.process_font_catalog_loads >= 1);
            assert!(cpu_cache.after_dirty_render.hits >= cpu_cache.after_base_render.hits);
            assert!(cpu_cache.after_direct_full_b_render.hits >= cpu_cache.after_dirty_render.hits);
            assert!(manifest.cache.gpu.is_none());
            assert!(manifest.gpu_stages.is_none());
            assert!(manifest.visual_comparison.is_none());
            assert_eq!(manifest.provenance.fixture_id, FIXTURE_ID);
            assert!(manifest.execution.no_window);
            assert_eq!(manifest.sampling.warmup_frames_completed, 0);
            assert_eq!(manifest.sampling.sample_frames_completed, 1);
            assert!(manifest.sampling.deterministic_payloads);
            assert!(manifest.composition_proof.exact_equivalence);
            assert_eq!(
                manifest.composition_proof.composed_b_rgba_fnv1a64,
                manifest.composition_proof.direct_full_b_rgba_fnv1a64
            );
            assert!(!manifest.composition_proof.dirty_regions.is_empty());
        }
        assert_eq!(
            manifests[1].payload_fnv1a64,
            manifests[1].composition_proof.direct_full_b_rgba_fnv1a64
        );
        assert_eq!(manifests[0].update_stride, 0);
        assert_eq!(manifests[1].update_stride, manifests[1].frame_stride);
        assert_eq!(manifests[2].update_stride, 0);
        assert_eq!(
            manifests[2].payload_fnv1a64,
            manifests[2].composition_proof.dirty_payload_fnv1a64
        );
        let json = facet_json::to_string_pretty(&manifests[2])
            .expect("dirty manifest should serialize as machine-readable JSON");
        for evidence_key in [
            "transport_id",
            "font_sha256",
            "composition_proof",
            "exact_equivalence",
            "cache",
            "timings",
        ] {
            assert!(
                json.contains(evidence_key),
                "missing {evidence_key}: {json}"
            );
        }
    }

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "the test intentionally orchestrates and verifies the complete GPU evidence contract"
    )]
    fn gpu_fixture_uses_no_window_vulkan_and_proves_all_transports_when_available() {
        if !matches!(
            VulkanOffscreenContext::probe(),
            teamy_terminal_renderer::VulkanOffscreenAvailability::Available(_)
        ) {
            eprintln!("SKIP GPU render-fixture evidence proof: no usable Vulkan implementation");
            return;
        }

        for transport in [
            RenderFixtureTransport::FullPng,
            RenderFixtureTransport::FullRawRgba,
            RenderFixtureTransport::DirtyRawRgba,
        ] {
            let rendered = render_gpu_transport_fixture(
                transport,
                RenderFixtureSamplingRequest {
                    warmup_frames: 1,
                    sample_frames: 2,
                },
            )
            .expect("GPU fixture should render on the available no-window Vulkan device");
            let manifest = manifest(transport, &rendered);
            assert_eq!(manifest.renderer_id, RUST_GPU_SLUG_RENDERER_ID);
            assert_eq!(manifest.transport_id, transport.id());
            assert!(manifest.exact_equivalence);
            assert!(manifest.composition_proof.exact_equivalence);
            assert_eq!(
                manifest.composition_proof.composed_b_rgba_fnv1a64,
                manifest.composition_proof.direct_full_b_rgba_fnv1a64
            );
            assert!(manifest.cache.cpu.is_none());
            let gpu_cache = manifest.cache.gpu.as_ref().expect("GPU cache evidence");
            assert!(
                gpu_cache.after_direct_full_b_render.geometry_hits
                    > gpu_cache.after_base_render.geometry_hits
            );
            assert_eq!(
                gpu_cache.after_base_render.targets.allocations,
                gpu_cache.after_direct_full_b_render.targets.allocations
            );
            assert!(gpu_cache.after_direct_full_b_render.targets.reuses >= 2);
            let stages = manifest.gpu_stages.as_ref().expect("GPU stage evidence");
            assert!(stages.base_full_a.requested_pixels > 0);
            assert!(stages.dirty_b.requested_pixels < stages.base_full_a.requested_pixels);
            assert_eq!(
                stages.base_full_a.target_allocation_id,
                stages.direct_full_b.target_allocation_id
            );
            assert_eq!(
                manifest.execution.execution_target,
                "vulkan-offscreen-no-window"
            );
            assert!(manifest.execution.no_window);
            assert!(!manifest.execution.uses_winit_window);
            assert!(!manifest.execution.uses_vulkan_surface);
            assert!(!manifest.execution.uses_swapchain);
            let device = manifest
                .execution
                .device
                .as_ref()
                .expect("device provenance");
            assert!(device.enabled_instance_extensions.is_empty());
            assert!(device.enabled_device_extensions.is_empty());
            assert_eq!(manifest.sampling.warmup_frames_completed, 1);
            assert_eq!(manifest.sampling.sample_frames_completed, 2);
            assert_eq!(manifest.sampling.renderer_total_us.samples, 2);
            assert_eq!(manifest.sampling.packet_pack_us.samples, 2);
            let sampled_stages = manifest
                .sampling
                .gpu_stage_summaries
                .as_ref()
                .expect("sample-correlated GPU stage summaries");
            assert_eq!(sampled_stages.geometry_build_us.samples, 2);
            assert_eq!(sampled_stages.gpu_completion_wait_us.samples, 2);
            assert_eq!(sampled_stages.readback_map_copy_us.samples, 2);
            assert!(sampled_stages.total_requested_pixels > 0);
            assert!(manifest.sampling.deterministic_payloads);
            let comparison = manifest
                .visual_comparison
                .as_ref()
                .expect("CPU-versus-GPU visual comparison evidence");
            assert_eq!(comparison.reference_renderer_id, CPU_RENDERER_ID);
            assert_eq!(comparison.candidate_renderer_id, RUST_GPU_SLUG_RENDERER_ID);
            assert_eq!(comparison.width, manifest.width);
            assert_eq!(comparison.height, manifest.height);
            assert_eq!(
                comparison.pixel_count,
                u64::from(manifest.width) * u64::from(manifest.height)
            );
            assert!(comparison.differing_pixels <= comparison.pixel_count);
            assert!(comparison.differing_pixels_basis_points <= 10_000);
            assert!(comparison.reference_raster_us > 0);
            assert_eq!(
                manifest.sampling.stable_stage_names,
                GPU_TRACY_STAGE_NAMES
                    .iter()
                    .map(|name| (*name).to_string())
                    .collect::<Vec<_>>()
            );
            assert_eq!(
                manifest.provenance.shader_source_fnv1a64,
                Some(format!("{GPU_SLUG_SHADER_SOURCE_IDENTITY:016x}"))
            );
            let json = facet_json::to_string_pretty(&manifest)
                .expect("GPU manifest should serialize as machine-readable JSON");
            for evidence_key in [
                "gpu_stages",
                "geometry_build_us",
                "gpu_completion_wait_us",
                "target_allocation_id",
                "execution",
                "device_name",
                "snapshot_b_semantic_fnv1a64",
                "sample_frames_completed",
                "stable_stage_names",
                "visual_comparison",
                "differing_pixels_basis_points",
                "reference_raster_us",
            ] {
                assert!(
                    json.contains(evidence_key),
                    "missing {evidence_key}: {json}"
                );
            }
        }
    }

    #[test]
    fn rgba_comparison_reports_exact_and_bounded_pixel_differences() {
        let exact = compare_rgba(&[1, 2, 3, 4, 5, 6, 7, 8], &[1, 2, 3, 4, 5, 6, 7, 8], 2, 1)
            .expect("equal RGBA buffers should compare");
        assert!(!exact.differences_present);
        assert_eq!(exact.differing_pixels, 0);
        assert_eq!(exact.differing_pixels_basis_points, 0);
        assert_eq!(exact.total_channel_absolute_delta, 0);
        assert_eq!(exact.difference_min_x, 0);
        assert_eq!(exact.difference_min_y, 0);
        assert_eq!(exact.difference_max_x, 0);
        assert_eq!(exact.difference_max_y, 0);
        assert_eq!(exact.reference_raster_us, 0);

        let changed = compare_rgba(&[1, 2, 3, 4, 5, 6, 7, 8], &[1, 2, 3, 4, 5, 10, 7, 9], 2, 1)
            .expect("different RGBA buffers should compare");
        assert!(changed.differences_present);
        assert_eq!(changed.differing_pixels, 1);
        assert_eq!(changed.differing_pixels_basis_points, 5_000);
        assert_eq!(changed.alpha_differing_pixels, 1);
        assert_eq!(changed.max_channel_absolute_delta, 4);
        assert_eq!(changed.total_channel_absolute_delta, 5);
        assert_eq!(changed.mean_channel_absolute_delta_milli, 625);
        assert_eq!(changed.difference_min_x, 1);
        assert_eq!(changed.difference_min_y, 0);
        assert_eq!(changed.difference_max_x, 1);
        assert_eq!(changed.difference_max_y, 0);
    }

    #[test]
    fn rgba_comparison_rejects_buffers_that_do_not_match_dimensions() {
        let _error = compare_rgba(&[0; 4], &[0; 8], 2, 1)
            .expect_err("a short reference buffer must be rejected");
        let _error = compare_rgba(&[0; 8], &[0; 4], 2, 1)
            .expect_err("a short candidate buffer must be rejected");
    }
}
