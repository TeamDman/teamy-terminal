use crate::cli::output::CliOutput;
use arbitrary::Arbitrary;
use eyre::Context;
use facet::Facet;
use figue::{self as args};
use std::fs;
use std::path::PathBuf;
use std::time::Instant;
use teamy_terminal_core::TerminalLimits;
use teamy_terminal_core::TerminalSession;
use teamy_terminal_font::DEFAULT_PIXEL_SIZE;
use teamy_terminal_font::EMBEDDED_FONT_ID;
use teamy_terminal_font::EMBEDDED_FONT_SHA256;
use teamy_terminal_font::TerminalFont;
use teamy_terminal_font::TerminalFontCacheStats;
use teamy_terminal_font::TerminalFontCatalog;
use teamy_terminal_font::damage_regions;
use teamy_terminal_frame::ComposedRasterFrame;
use teamy_terminal_frame::FrameCompression;
use teamy_terminal_frame::RasterFrameUpdate;
use teamy_terminal_frame::RasterRect;

const RENDERER_ID: &str = "rust-cpu-fontdue";
const BASE_SEQUENCE: u64 = 1;
const DIRTY_SEQUENCE: u64 = 2;

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
struct RenderFixtureTimings {
    fixture_setup_us: u64,
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
struct RenderFixtureCacheEvidence {
    process_font_catalog_loads: u64,
    after_base_render: RenderFixtureFontCacheStats,
    after_dirty_render: RenderFixtureFontCacheStats,
    after_direct_full_b_render: RenderFixtureFontCacheStats,
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
    exact_equivalence: bool,
    composition_proof: RenderFixtureCompositionProof,
    cache: RenderFixtureCacheEvidence,
    timings: RenderFixtureTimings,
}

struct RenderedFixture {
    update: RasterFrameUpdate,
    logical_columns: u16,
    logical_rows: u16,
    cell_width: u32,
    cell_height: u32,
    exact_equivalence: bool,
    composition_proof: RenderFixtureCompositionProof,
    cache: RenderFixtureCacheEvidence,
    timings: RenderFixtureTimings,
}

impl RenderFixtureArgs {
    /// Render and write the deterministic fixture.
    pub fn invoke(self) -> eyre::Result<CliOutput> {
        let transport = resolve_transport(self.format, self.transport);
        let rendered = render_transport_fixture(transport)?;
        let path = self
            .output
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(transport.default_path()));
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
            renderer_id: RENDERER_ID.to_string(),
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

fn resolve_transport(
    format: RenderFixtureFormat,
    transport: Option<RenderFixtureTransport>,
) -> RenderFixtureTransport {
    transport.unwrap_or(match format {
        RenderFixtureFormat::Png => RenderFixtureTransport::FullPng,
        RenderFixtureFormat::Raw => RenderFixtureTransport::FullRawRgba,
    })
}

fn render_transport_fixture(transport: RenderFixtureTransport) -> eyre::Result<RenderedFixture> {
    let total_started = Instant::now();
    let setup_started = Instant::now();
    let mut terminal = TerminalSession::new(TerminalLimits::default(), 40, 12);
    terminal.process_bytes(
        b"\x1b[1;34mTeamy Terminal\x1b[0m\r\nportable frame fixture\r\n$ echo ready",
    );
    let mut font = TerminalFont::new(DEFAULT_PIXEL_SIZE)?;
    let fixture_setup_us = elapsed_us(setup_started);

    let snapshot_a_started = Instant::now();
    let snapshot_a = terminal.snapshot();
    let snapshot_a_us = elapsed_us(snapshot_a_started);
    let width = u32::from(snapshot_a.columns) * font.cell_width();
    let height = u32::from(snapshot_a.rows) * font.cell_height();

    let raster_a_started = Instant::now();
    let frame_a = font.render(&snapshot_a, width, height);
    let raster_a_us = elapsed_us(raster_a_started);
    let after_base_render = font.cache_stats().into();

    let mut timings = RenderFixtureTimings {
        fixture_setup_us,
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

    let snapshot_b_started = Instant::now();
    let snapshot_b = terminal.snapshot();
    timings.snapshot_b_us = elapsed_us(snapshot_b_started);

    let damage_started = Instant::now();
    let (cell_regions, full_refresh) = damage_regions(Some(&snapshot_a), &snapshot_b);
    timings.damage_us = elapsed_us(damage_started);
    if full_refresh || cell_regions.is_empty() {
        eyre::bail!("deterministic fixture unexpectedly required a full or empty refresh");
    }
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
    timings.total_us = elapsed_us(total_started);
    Ok(RenderedFixture {
        update,
        logical_columns: snapshot_b.columns,
        logical_rows: snapshot_b.rows,
        cell_width: font.cell_width(),
        cell_height: font.cell_height(),
        exact_equivalence,
        composition_proof,
        cache: RenderFixtureCacheEvidence {
            process_font_catalog_loads: TerminalFontCatalog::shared_system_loads(),
            after_base_render,
            after_dirty_render,
            after_direct_full_b_render,
        },
        timings,
    })
}

fn manifest(
    transport: RenderFixtureTransport,
    rendered: &RenderedFixture,
) -> RenderFixtureManifest {
    let descriptor = rendered.update.descriptor;
    RenderFixtureManifest {
        manifest_version: 1,
        transport_id: transport.id().to_string(),
        renderer_id: RENDERER_ID.to_string(),
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
        font_pixel_size_milli: (DEFAULT_PIXEL_SIZE * 1000.0).round() as u32,
        cell_width: rendered.cell_width,
        cell_height: rendered.cell_height,
        payload_bytes: rendered.update.bytes.len(),
        payload_fnv1a64: fnv1a64_hex(&rendered.update.bytes),
        region_count: rendered.update.regions.len(),
        regions: regions_manifest(&rendered.update),
        exact_equivalence: rendered.exact_equivalence,
        composition_proof: rendered.composition_proof.clone(),
        cache: rendered.cache.clone(),
        timings: rendered.timings.clone(),
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

    const fn default_path(self) -> &'static str {
        match self {
            Self::FullPng => "teamy-terminal-fixture.png",
            Self::FullRawRgba => "teamy-terminal-fixture.rgba",
            Self::DirtyRawRgba => "teamy-terminal-fixture.dirty.rgba",
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
    }

    #[test]
    fn dirty_transport_fixture_composes_exactly_to_direct_full_b() {
        let rendered = render_transport_fixture(RenderFixtureTransport::DirtyRawRgba)
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
        let full_png =
            render_transport_fixture(RenderFixtureTransport::FullPng).expect("full PNG fixture");
        let full_raw = render_transport_fixture(RenderFixtureTransport::FullRawRgba)
            .expect("full raw fixture");
        let dirty_raw = render_transport_fixture(RenderFixtureTransport::DirtyRawRgba)
            .expect("dirty raw fixture");
        let manifests = [
            manifest(RenderFixtureTransport::FullPng, &full_png),
            manifest(RenderFixtureTransport::FullRawRgba, &full_raw),
            manifest(RenderFixtureTransport::DirtyRawRgba, &dirty_raw),
        ];

        for manifest in &manifests {
            assert_eq!(manifest.renderer_id, RENDERER_ID);
            assert_eq!(manifest.contract_version, 1);
            assert_eq!(manifest.logical_columns, 40);
            assert_eq!(manifest.logical_rows, 12);
            assert_eq!(manifest.font_id, EMBEDDED_FONT_ID);
            assert_eq!(manifest.font_sha256, EMBEDDED_FONT_SHA256);
            assert_eq!(manifest.bytes_per_pixel, 4);
            assert!(manifest.timings.total_us >= manifest.timings.damage_us);
            assert!(manifest.cache.process_font_catalog_loads >= 1);
            assert!(
                manifest.cache.after_dirty_render.hits >= manifest.cache.after_base_render.hits
            );
            assert!(
                manifest.cache.after_direct_full_b_render.hits
                    >= manifest.cache.after_dirty_render.hits
            );
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
}
