//! Renderer-neutral outline geometry and analytic Slug-style coverage.
//!
//! This module deliberately stops before GPU resource or shader ownership. It
//! turns font outlines into quadratic curves, constructs the directional band
//! contract consumed by raster backends, and provides an independent CPU
//! oracle plus a banded evaluator for conformance tests.

use std::error::Error;
use std::fmt;

use ttf_parser::Face;
use ttf_parser::GlyphId;
use ttf_parser::OutlineBuilder;

use super::EMBEDDED_FONT;

/// Default band span in font units.
pub const DEFAULT_BAND_SIZE_FONT_UNITS: f32 = 64.0;
/// Maximum horizontal or vertical bands in one glyph.
pub const MAX_BAND_COUNT: usize = 255;
/// Number of `u32` fields in the canonical directional band header.
pub const DIRECTIONAL_BAND_HEADER_WORDS: usize = 4;
/// Number of shader-friendly `vec4<f32>` records per quadratic curve.
pub const GPU_CURVE_VEC4_COUNT: usize = 2;
/// Number of words in [`GpuGlyphMetadata`].
pub const GPU_GLYPH_METADATA_WORDS: usize = 16;
/// Number of words in [`SlugFontMetrics::gpu_words`].
pub const GPU_FONT_METRICS_WORDS: usize = 4;
/// [`GpuGlyphMetadata`] flag indicating fallback geometry.
pub const GPU_GLYPH_FLAG_FALLBACK: u32 = 1;

const COVERAGE_EPSILON: f32 = 1.0 / 65_536.0;
const DEFAULT_CUBIC_TOLERANCE: f32 = 0.25;
const MAX_CUBIC_SUBDIVISION_DEPTH: u32 = 8;

/// A validation or conversion failure in the renderer-neutral Slug contract.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SlugError {
    /// The supplied font face could not be parsed.
    InvalidFont,
    /// The requested fallback character is absent from the font face.
    MissingFallbackGlyph(char),
    /// Geometry contains a non-finite coordinate.
    NonFiniteGeometry,
    /// Glyph bounds are non-finite or inverted.
    InvalidBounds,
    /// A band size or coverage scale was non-finite or non-positive.
    InvalidScale,
    /// A collection cannot be represented by the canonical `u32` contract.
    CountOverflow(&'static str),
    /// Directional band headers or indices fail structural validation.
    MalformedBands(&'static str),
}

impl fmt::Display for SlugError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidFont => formatter.write_str("font face could not be parsed"),
            Self::MissingFallbackGlyph(character) => {
                write!(
                    formatter,
                    "font face does not contain fallback glyph {character:?}"
                )
            }
            Self::NonFiniteGeometry => formatter.write_str("outline geometry is not finite"),
            Self::InvalidBounds => formatter.write_str("glyph bounds are non-finite or inverted"),
            Self::InvalidScale => formatter.write_str("scale must be finite and positive"),
            Self::CountOverflow(label) => {
                write!(formatter, "{label} does not fit the canonical u32 contract")
            }
            Self::MalformedBands(reason) => {
                write!(formatter, "directional band data is malformed: {reason}")
            }
        }
    }
}

impl Error for SlugError {}

/// A point in font-design units.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Point {
    /// Horizontal coordinate.
    pub x: f32,
    /// Vertical coordinate.
    pub y: f32,
}

impl Point {
    /// Construct a point.
    #[must_use]
    pub const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }

    fn is_finite(self) -> bool {
        self.x.is_finite() && self.y.is_finite()
    }
}

/// One quadratic Bézier segment in font-design units.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct QuadraticCurve {
    /// Segment start.
    pub p0: Point,
    /// Quadratic control point.
    pub p1: Point,
    /// Segment end.
    pub p2: Point,
}

impl QuadraticCurve {
    /// Represent a line as a degenerate quadratic.
    #[must_use]
    pub const fn line(from: Point, to: Point) -> Self {
        Self {
            p0: from,
            p1: Point::new((from.x + to.x) * 0.5, (from.y + to.y) * 0.5),
            p2: to,
        }
    }

    /// Export the curve as two tightly specified shader `vec4<f32>` records.
    ///
    /// Record zero is `(p0.x, p0.y, p1.x, p1.y)` and record one is
    /// `(p2.x, p2.y, 0, 0)`. This avoids relying on Rust struct padding in GPU
    /// buffers and matches a storage-buffer `array<vec4<f32>>` contract.
    #[must_use]
    pub const fn gpu_vec4s(self) -> [[f32; 4]; GPU_CURVE_VEC4_COUNT] {
        [
            [self.p0.x, self.p0.y, self.p1.x, self.p1.y],
            [self.p2.x, self.p2.y, 0.0, 0.0],
        ]
    }

    fn is_finite(self) -> bool {
        self.p0.is_finite() && self.p1.is_finite() && self.p2.is_finite()
    }
}

/// One cubic Bézier segment accepted by the outline conversion seam.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CubicCurve {
    /// Segment start.
    pub p0: Point,
    /// First cubic control point.
    pub p1: Point,
    /// Second cubic control point.
    pub p2: Point,
    /// Segment end.
    pub p3: Point,
}

impl CubicCurve {
    fn is_finite(self) -> bool {
        self.p0.is_finite() && self.p1.is_finite() && self.p2.is_finite() && self.p3.is_finite()
    }
}

/// Axis-aligned glyph bounds in font-design units.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct GlyphBounds {
    /// Minimum horizontal coordinate.
    pub min_x: f32,
    /// Minimum vertical coordinate.
    pub min_y: f32,
    /// Maximum horizontal coordinate.
    pub max_x: f32,
    /// Maximum vertical coordinate.
    pub max_y: f32,
}

impl GlyphBounds {
    /// Construct validated bounds.
    pub fn new(min_x: f32, min_y: f32, max_x: f32, max_y: f32) -> Result<Self, SlugError> {
        let bounds = Self {
            min_x,
            min_y,
            max_x,
            max_y,
        };
        bounds.validate()?;
        Ok(bounds)
    }

    /// Horizontal midpoint.
    #[must_use]
    pub fn midpoint_x(self) -> f32 {
        (self.min_x + self.max_x) * 0.5
    }

    /// Vertical midpoint.
    #[must_use]
    pub fn midpoint_y(self) -> f32 {
        (self.min_y + self.max_y) * 0.5
    }

    fn validate(self) -> Result<(), SlugError> {
        if !self.min_x.is_finite()
            || !self.min_y.is_finite()
            || !self.max_x.is_finite()
            || !self.max_y.is_finite()
            || self.min_x > self.max_x
            || self.min_y > self.max_y
        {
            return Err(SlugError::InvalidBounds);
        }
        Ok(())
    }
}

/// Font-wide metrics needed to map outline units to terminal cells.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SlugFontMetrics {
    /// Units per em from the font face.
    pub units_per_em: u16,
    /// Font ascender in design units.
    pub ascender: f32,
    /// Font descender in design units.
    pub descender: f32,
}

impl SlugFontMetrics {
    /// Export stable shader words without depending on a graphics API crate.
    ///
    /// The fields are `units_per_em` as an integer word followed by the bit
    /// representations of `ascender`, `descender`, and their vertical span.
    #[must_use]
    pub fn gpu_words(self) -> [u32; GPU_FONT_METRICS_WORDS] {
        [
            u32::from(self.units_per_em),
            self.ascender.to_bits(),
            self.descender.to_bits(),
            (self.ascender - self.descender).to_bits(),
        ]
    }
}

/// Reusable renderer-neutral geometry for one resolved glyph.
#[derive(Clone, Debug, PartialEq)]
pub struct GlyphGeometry {
    /// Character requested by the terminal snapshot.
    pub requested_character: char,
    /// Character whose outline was actually extracted.
    pub resolved_character: char,
    /// True when `resolved_character` is the configured fallback.
    pub used_fallback: bool,
    /// Face-local glyph identifier.
    pub glyph_id: u16,
    /// Horizontal advance in design units.
    pub advance: f32,
    /// Validated outline/cell bounds.
    pub bounds: GlyphBounds,
    /// Contours represented entirely as quadratic segments.
    pub curves: Vec<QuadraticCurve>,
}

/// Fixed-size shader metadata for one glyph's curve and band buffers.
///
/// The transparent word array is the stable ABI; consumers must not infer a
/// layout from Rust field padding. Word positions are:
///
/// 0 curve start, 1 curve count, 2 band-block start, 3 horizontal-band count,
/// 4 vertical-band count, 5 flags, 6 units per em, 7 advance bits,
/// 8..=11 bounds bits `(min_x, min_y, max_x, max_y)`, and
/// 12..=15 band-transform bits `(x_scale, y_scale, x_offset, y_offset)`.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GpuGlyphMetadata {
    words: [u32; GPU_GLYPH_METADATA_WORDS],
}

impl GpuGlyphMetadata {
    /// Borrow the canonical shader words.
    #[must_use]
    pub const fn words(&self) -> &[u32; GPU_GLYPH_METADATA_WORDS] {
        &self.words
    }

    /// Consume the metadata into canonical shader words.
    #[must_use]
    pub const fn into_words(self) -> [u32; GPU_GLYPH_METADATA_WORDS] {
        self.words
    }
}

/// Build fixed shader metadata after a renderer has assigned buffer offsets.
pub fn build_gpu_glyph_metadata(
    geometry: &GlyphGeometry,
    bands: &DirectionalBands,
    font_metrics: SlugFontMetrics,
    curve_start: u32,
    band_word_start: u32,
) -> Result<GpuGlyphMetadata, SlugError> {
    bands.validate(geometry.curves.len())?;
    if geometry.bounds != bands.bounds {
        return Err(SlugError::MalformedBands(
            "glyph and directional-band bounds differ",
        ));
    }
    let curve_count = u32::try_from(geometry.curves.len())
        .map_err(|_| SlugError::CountOverflow("glyph curve count"))?;
    let horizontal_count = u32::try_from(bands.horizontal.len())
        .map_err(|_| SlugError::CountOverflow("horizontal band count"))?;
    let vertical_count = u32::try_from(bands.vertical.len())
        .map_err(|_| SlugError::CountOverflow("vertical band count"))?;
    let flags = if geometry.used_fallback {
        GPU_GLYPH_FLAG_FALLBACK
    } else {
        0
    };
    Ok(GpuGlyphMetadata {
        words: [
            curve_start,
            curve_count,
            band_word_start,
            horizontal_count,
            vertical_count,
            flags,
            u32::from(font_metrics.units_per_em),
            geometry.advance.to_bits(),
            geometry.bounds.min_x.to_bits(),
            geometry.bounds.min_y.to_bits(),
            geometry.bounds.max_x.to_bits(),
            geometry.bounds.max_y.to_bits(),
            bands.transform.x_scale.to_bits(),
            bands.transform.y_scale.to_bits(),
            bands.transform.x_offset.to_bits(),
            bands.transform.y_offset.to_bits(),
        ],
    })
}

/// A parsed font face that can produce deterministic Slug geometry.
pub struct SlugFont<'a> {
    face: Face<'a>,
    fallback_character: char,
    fallback_glyph: GlyphId,
}

impl fmt::Debug for SlugFont<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SlugFont")
            .field("fallback_character", &self.fallback_character)
            .field("fallback_glyph", &self.fallback_glyph.0)
            .field("metrics", &self.metrics())
            .finish()
    }
}

impl SlugFont<'static> {
    /// Parse the deterministic embedded Caskaydia Cove Nerd Font Mono face.
    pub fn embedded() -> Result<Self, SlugError> {
        Self::parse(EMBEDDED_FONT, 0, '?')
    }
}

impl<'a> SlugFont<'a> {
    /// Parse a font face and bind an explicit missing-glyph fallback.
    pub fn parse(
        bytes: &'a [u8],
        face_index: u32,
        fallback_character: char,
    ) -> Result<Self, SlugError> {
        let face = Face::parse(bytes, face_index).map_err(|_| SlugError::InvalidFont)?;
        let fallback_glyph = face
            .glyph_index(fallback_character)
            .ok_or(SlugError::MissingFallbackGlyph(fallback_character))?;
        Ok(Self {
            face,
            fallback_character,
            fallback_glyph,
        })
    }

    /// Return the font metrics used by extracted glyph geometry.
    #[must_use]
    pub fn metrics(&self) -> SlugFontMetrics {
        SlugFontMetrics {
            units_per_em: self.face.units_per_em(),
            ascender: f32::from(self.face.ascender()),
            descender: f32::from(self.face.descender()),
        }
    }

    /// Extract one glyph, falling back to the configured character when absent.
    pub fn glyph_geometry(&self, character: char) -> Result<GlyphGeometry, SlugError> {
        let (glyph_id, resolved_character, used_fallback) =
            self.face.glyph_index(character).map_or(
                (self.fallback_glyph, self.fallback_character, true),
                |glyph_id| (glyph_id, character, false),
            );
        let advance = self.face.glyph_hor_advance(glyph_id).map_or(0.0, f32::from);
        if !advance.is_finite() {
            return Err(SlugError::NonFiniteGeometry);
        }

        let mut builder = QuadraticOutlineBuilder::default();
        let outline_bounds = self.face.outline_glyph(glyph_id, &mut builder);
        if builder.invalid_geometry {
            return Err(SlugError::NonFiniteGeometry);
        }
        let bounds = outline_bounds.map_or_else(
            || {
                GlyphBounds::new(
                    0.0,
                    f32::from(self.face.descender()),
                    advance.max(0.0),
                    f32::from(self.face.ascender()),
                )
            },
            |rect| {
                GlyphBounds::new(
                    f32::from(rect.x_min),
                    f32::from(rect.y_min),
                    f32::from(rect.x_max),
                    f32::from(rect.y_max),
                )
            },
        )?;
        validate_curves(&builder.curves)?;

        Ok(GlyphGeometry {
            requested_character: character,
            resolved_character,
            used_fallback,
            glyph_id: glyph_id.0,
            advance,
            bounds,
            curves: builder.curves,
        })
    }
}

/// Approximate a cubic segment with bounded quadratic segments.
pub fn approximate_cubic(
    cubic: CubicCurve,
    tolerance: f32,
) -> Result<Vec<QuadraticCurve>, SlugError> {
    if !cubic.is_finite() {
        return Err(SlugError::NonFiniteGeometry);
    }
    if !tolerance.is_finite() || tolerance <= 0.0 {
        return Err(SlugError::InvalidScale);
    }
    let mut curves = Vec::new();
    append_cubic_as_quadratics(
        &mut curves,
        cubic,
        tolerance,
        0,
        MAX_CUBIC_SUBDIVISION_DEPTH,
    );
    Ok(curves)
}

#[derive(Default)]
struct QuadraticOutlineBuilder {
    curves: Vec<QuadraticCurve>,
    contour_start: Option<Point>,
    current: Option<Point>,
    invalid_geometry: bool,
}

impl QuadraticOutlineBuilder {
    fn push_line(&mut self, to: Point) {
        if let Some(from) = self.current {
            self.curves.push(QuadraticCurve::line(from, to));
            self.current = Some(to);
        }
    }

    fn push_quadratic(&mut self, control: Point, to: Point) {
        if let Some(from) = self.current {
            self.curves.push(QuadraticCurve {
                p0: from,
                p1: control,
                p2: to,
            });
            self.current = Some(to);
        }
    }
}

impl OutlineBuilder for QuadraticOutlineBuilder {
    fn move_to(&mut self, x: f32, y: f32) {
        let point = Point::new(x, y);
        self.invalid_geometry |= !point.is_finite();
        self.contour_start = Some(point);
        self.current = Some(point);
    }

    fn line_to(&mut self, x: f32, y: f32) {
        let point = Point::new(x, y);
        self.invalid_geometry |= !point.is_finite();
        self.push_line(point);
    }

    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        let control = Point::new(x1, y1);
        let to = Point::new(x, y);
        self.invalid_geometry |= !control.is_finite() || !to.is_finite();
        self.push_quadratic(control, to);
    }

    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        let Some(from) = self.current else {
            return;
        };
        let cubic = CubicCurve {
            p0: from,
            p1: Point::new(x1, y1),
            p2: Point::new(x2, y2),
            p3: Point::new(x, y),
        };
        if !cubic.is_finite() {
            self.invalid_geometry = true;
            return;
        }
        append_cubic_as_quadratics(
            &mut self.curves,
            cubic,
            DEFAULT_CUBIC_TOLERANCE,
            0,
            MAX_CUBIC_SUBDIVISION_DEPTH,
        );
        self.current = Some(cubic.p3);
    }

    fn close(&mut self) {
        if let (Some(current), Some(start)) = (self.current, self.contour_start)
            && current != start
        {
            self.push_line(start);
        }
        self.current = self.contour_start;
    }
}

fn append_cubic_as_quadratics(
    output: &mut Vec<QuadraticCurve>,
    cubic: CubicCurve,
    tolerance: f32,
    depth: u32,
    max_depth: u32,
) {
    let q1_from_p1 = Point::new(
        ((3.0 * cubic.p1.x) - cubic.p0.x) * 0.5,
        ((3.0 * cubic.p1.y) - cubic.p0.y) * 0.5,
    );
    let q1_from_p2 = Point::new(
        ((3.0 * cubic.p2.x) - cubic.p3.x) * 0.5,
        ((3.0 * cubic.p2.y) - cubic.p3.y) * 0.5,
    );
    let error = (q1_from_p1.x - q1_from_p2.x)
        .abs()
        .max((q1_from_p1.y - q1_from_p2.y).abs());
    if error <= tolerance || depth >= max_depth {
        output.push(QuadraticCurve {
            p0: cubic.p0,
            p1: Point::new(
                (q1_from_p1.x + q1_from_p2.x) * 0.5,
                (q1_from_p1.y + q1_from_p2.y) * 0.5,
            ),
            p2: cubic.p3,
        });
        return;
    }

    let p01 = midpoint(cubic.p0, cubic.p1);
    let p12 = midpoint(cubic.p1, cubic.p2);
    let p23 = midpoint(cubic.p2, cubic.p3);
    let p01_12 = midpoint(p01, p12);
    let p12_23 = midpoint(p12, p23);
    let center = midpoint(p01_12, p12_23);
    append_cubic_as_quadratics(
        output,
        CubicCurve {
            p0: cubic.p0,
            p1: p01,
            p2: p01_12,
            p3: center,
        },
        tolerance,
        depth + 1,
        max_depth,
    );
    append_cubic_as_quadratics(
        output,
        CubicCurve {
            p0: center,
            p1: p12_23,
            p2: p23,
            p3: cubic.p3,
        },
        tolerance,
        depth + 1,
        max_depth,
    );
}

fn midpoint(lhs: Point, rhs: Point) -> Point {
    Point::new((lhs.x + rhs.x) * 0.5, (lhs.y + rhs.y) * 0.5)
}

/// Four-field directional band header.
///
/// Offsets address [`DirectionalBands::curve_indices`]. The descending list is
/// sorted by the relevant curve maximum; the ascending list is sorted by its
/// minimum. Both lists contain `curve_count` indices.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct DirectionalBandHeader {
    /// Number of curves in each directional list.
    pub curve_count: u32,
    /// Start of the maximum-descending list.
    pub descending_offset: u32,
    /// Start of the minimum-ascending list.
    pub ascending_offset: u32,
    /// Sample coordinate below which the ascending/left ray is cheaper.
    pub split: f32,
}

impl DirectionalBandHeader {
    /// Encode the four semantic fields as canonical `u32` words.
    #[must_use]
    pub fn words(self) -> [u32; DIRECTIONAL_BAND_HEADER_WORDS] {
        [
            self.curve_count,
            self.descending_offset,
            self.ascending_offset,
            self.split.to_bits(),
        ]
    }
}

/// Linear transforms from design coordinates to clamped band indices.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct BandTransform {
    /// Horizontal-coordinate scale for vertical bands.
    pub x_scale: f32,
    /// Vertical-coordinate scale for horizontal bands.
    pub y_scale: f32,
    /// Horizontal-coordinate offset for vertical bands.
    pub x_offset: f32,
    /// Vertical-coordinate offset for horizontal bands.
    pub y_offset: f32,
}

/// Directional band metadata independent of any graphics API.
#[derive(Clone, Debug, PartialEq)]
pub struct DirectionalBands {
    /// Bounds from which the transforms and split fallbacks were derived.
    pub bounds: GlyphBounds,
    /// Horizontal bands selected by sample `y` and traversed along `x`.
    pub horizontal: Vec<DirectionalBandHeader>,
    /// Vertical bands selected by sample `x` and traversed along `y`.
    pub vertical: Vec<DirectionalBandHeader>,
    /// Concatenated descending and ascending curve-index lists.
    pub curve_indices: Vec<u32>,
    /// Coordinate-to-band transforms.
    pub transform: BandTransform,
}

impl DirectionalBands {
    /// Validate offsets, counts, splits, transforms, and curve indices.
    pub fn validate(&self, curve_count: usize) -> Result<(), SlugError> {
        self.bounds.validate()?;
        if self.horizontal.is_empty() || self.vertical.is_empty() {
            return Err(SlugError::MalformedBands(
                "both axes require at least one band",
            ));
        }
        if self.horizontal.len() > MAX_BAND_COUNT || self.vertical.len() > MAX_BAND_COUNT {
            return Err(SlugError::MalformedBands(
                "band count exceeds the canonical bound",
            ));
        }
        if !self.transform.x_scale.is_finite()
            || !self.transform.y_scale.is_finite()
            || !self.transform.x_offset.is_finite()
            || !self.transform.y_offset.is_finite()
            || self.transform.x_scale <= 0.0
            || self.transform.y_scale <= 0.0
        {
            return Err(SlugError::MalformedBands("band transform is invalid"));
        }
        for header in self.horizontal.iter().chain(&self.vertical) {
            validate_header(header, &self.curve_indices, curve_count)?;
        }
        Ok(())
    }

    /// Pack headers followed by index lists for direct GPU-buffer upload.
    ///
    /// Packed offsets are absolute word offsets into the returned buffer.
    pub fn packed_words(&self, curve_count: usize) -> Result<Vec<u32>, SlugError> {
        self.validate(curve_count)?;
        let header_count = self
            .horizontal
            .len()
            .checked_add(self.vertical.len())
            .ok_or(SlugError::CountOverflow("band header count"))?;
        let table_words = header_count
            .checked_mul(DIRECTIONAL_BAND_HEADER_WORDS)
            .ok_or(SlugError::CountOverflow("band header words"))?;
        let table_words_u32 = u32::try_from(table_words)
            .map_err(|_| SlugError::CountOverflow("band header words"))?;
        let total_words = table_words
            .checked_add(self.curve_indices.len())
            .ok_or(SlugError::CountOverflow("packed band words"))?;
        let mut words = Vec::with_capacity(total_words);
        for header in self.horizontal.iter().chain(&self.vertical) {
            let descending_offset = table_words_u32
                .checked_add(header.descending_offset)
                .ok_or(SlugError::CountOverflow("descending packed offset"))?;
            let ascending_offset = table_words_u32
                .checked_add(header.ascending_offset)
                .ok_or(SlugError::CountOverflow("ascending packed offset"))?;
            words.extend([
                header.curve_count,
                descending_offset,
                ascending_offset,
                header.split.to_bits(),
            ]);
        }
        words.extend_from_slice(&self.curve_indices);
        Ok(words)
    }
}

/// Build directional horizontal/vertical bands for one glyph.
pub fn build_directional_bands(
    curves: &[QuadraticCurve],
    bounds: GlyphBounds,
    band_size_font_units: f32,
) -> Result<DirectionalBands, SlugError> {
    validate_curves(curves)?;
    bounds.validate()?;
    if !band_size_font_units.is_finite() || band_size_font_units <= 0.0 {
        return Err(SlugError::InvalidScale);
    }

    let vertical_count = compute_band_count(bounds.max_x - bounds.min_x, band_size_font_units);
    let horizontal_count = compute_band_count(bounds.max_y - bounds.min_y, band_size_font_units);
    let transform = BandTransform {
        x_scale: compute_band_scale(bounds.min_x, bounds.max_x, vertical_count),
        y_scale: compute_band_scale(bounds.min_y, bounds.max_y, horizontal_count),
        x_offset: compute_band_offset(bounds.min_x, bounds.max_x, vertical_count),
        y_offset: compute_band_offset(bounds.min_y, bounds.max_y, horizontal_count),
    };
    let extents: Vec<_> = curves.iter().copied().map(curve_extents).collect();
    let mut horizontal_members = vec![Vec::new(); horizontal_count];
    let mut vertical_members = vec![Vec::new(); vertical_count];

    for (curve_index, extent) in extents.iter().enumerate() {
        let first_horizontal = band_index(
            extent.min_y,
            transform.y_scale,
            transform.y_offset,
            horizontal_count,
        );
        let last_horizontal = band_index(
            extent.max_y,
            transform.y_scale,
            transform.y_offset,
            horizontal_count,
        );
        for members in &mut horizontal_members[first_horizontal..=last_horizontal] {
            members.push(curve_index);
        }

        let first_vertical = band_index(
            extent.min_x,
            transform.x_scale,
            transform.x_offset,
            vertical_count,
        );
        let last_vertical = band_index(
            extent.max_x,
            transform.x_scale,
            transform.x_offset,
            vertical_count,
        );
        for members in &mut vertical_members[first_vertical..=last_vertical] {
            members.push(curve_index);
        }
    }

    let mut curve_indices = Vec::new();
    let horizontal = build_axis_headers(
        &horizontal_members,
        &extents,
        Axis::Horizontal,
        bounds.min_x,
        bounds.max_x,
        &mut curve_indices,
    )?;
    let vertical = build_axis_headers(
        &vertical_members,
        &extents,
        Axis::Vertical,
        bounds.min_y,
        bounds.max_y,
        &mut curve_indices,
    )?;
    let bands = DirectionalBands {
        bounds,
        horizontal,
        vertical,
        curve_indices,
        transform,
    };
    bands.validate(curves.len())?;
    Ok(bands)
}

#[derive(Clone, Copy)]
enum Axis {
    Horizontal,
    Vertical,
}

fn build_axis_headers(
    members_by_band: &[Vec<usize>],
    extents: &[CurveExtents],
    axis: Axis,
    fallback_min: f32,
    fallback_max: f32,
    indices: &mut Vec<u32>,
) -> Result<Vec<DirectionalBandHeader>, SlugError> {
    let mut headers = Vec::with_capacity(members_by_band.len());
    for members in members_by_band {
        let mut descending = members.clone();
        descending.sort_by(|lhs, rhs| {
            descending_value(extents[*rhs], axis).total_cmp(&descending_value(extents[*lhs], axis))
        });
        let mut ascending = members.clone();
        ascending.sort_by(|lhs, rhs| {
            ascending_value(extents[*lhs], axis).total_cmp(&ascending_value(extents[*rhs], axis))
        });
        let split = choose_band_split(
            &descending,
            &ascending,
            extents,
            axis,
            fallback_min,
            fallback_max,
        );
        let curve_count = u32::try_from(members.len())
            .map_err(|_| SlugError::CountOverflow("band curve count"))?;
        let descending_offset = u32::try_from(indices.len())
            .map_err(|_| SlugError::CountOverflow("descending index offset"))?;
        append_indices(indices, &descending)?;
        let ascending_offset = u32::try_from(indices.len())
            .map_err(|_| SlugError::CountOverflow("ascending index offset"))?;
        append_indices(indices, &ascending)?;
        headers.push(DirectionalBandHeader {
            curve_count,
            descending_offset,
            ascending_offset,
            split,
        });
    }
    Ok(headers)
}

fn append_indices(output: &mut Vec<u32>, indices: &[usize]) -> Result<(), SlugError> {
    for index in indices {
        output.push(u32::try_from(*index).map_err(|_| SlugError::CountOverflow("curve index"))?);
    }
    Ok(())
}

fn choose_band_split(
    descending: &[usize],
    ascending: &[usize],
    extents: &[CurveExtents],
    axis: Axis,
    fallback_min: f32,
    fallback_max: f32,
) -> f32 {
    let count = descending.len();
    if count == 0 {
        return (fallback_min + fallback_max) * 0.5;
    }
    let mut best_worst = count;
    let mut best_split = (fallback_min + fallback_max) * 0.5;
    let mut left_count = count;
    for (curve_offset, curve_index) in descending.iter().enumerate() {
        let split = descending_value(extents[*curve_index], axis);
        let right_count = curve_offset + 1;
        while left_count > 0 && ascending_value(extents[ascending[left_count - 1]], axis) > split {
            left_count -= 1;
        }
        let worst = right_count.max(left_count);
        if worst < best_worst {
            best_worst = worst;
            best_split = split;
        }
    }
    best_split
}

fn validate_header(
    header: &DirectionalBandHeader,
    indices: &[u32],
    curve_count: usize,
) -> Result<(), SlugError> {
    if !header.split.is_finite() {
        return Err(SlugError::MalformedBands("band split is not finite"));
    }
    let count = usize::try_from(header.curve_count)
        .map_err(|_| SlugError::MalformedBands("curve count does not fit usize"))?;
    for offset in [header.descending_offset, header.ascending_offset] {
        let start = usize::try_from(offset)
            .map_err(|_| SlugError::MalformedBands("list offset does not fit usize"))?;
        let end = start
            .checked_add(count)
            .ok_or(SlugError::MalformedBands("list range overflow"))?;
        let list = indices
            .get(start..end)
            .ok_or(SlugError::MalformedBands("list range is out of bounds"))?;
        if list
            .iter()
            .any(|index| usize::try_from(*index).map_or(true, |index| index >= curve_count))
        {
            return Err(SlugError::MalformedBands("curve index is out of bounds"));
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct CurveExtents {
    min_x: f32,
    max_x: f32,
    min_y: f32,
    max_y: f32,
}

fn curve_extents(curve: QuadraticCurve) -> CurveExtents {
    CurveExtents {
        min_x: curve.p0.x.min(curve.p1.x).min(curve.p2.x),
        max_x: curve.p0.x.max(curve.p1.x).max(curve.p2.x),
        min_y: curve.p0.y.min(curve.p1.y).min(curve.p2.y),
        max_y: curve.p0.y.max(curve.p1.y).max(curve.p2.y),
    }
}

fn descending_value(extents: CurveExtents, axis: Axis) -> f32 {
    match axis {
        Axis::Horizontal => extents.max_x,
        Axis::Vertical => extents.max_y,
    }
}

fn ascending_value(extents: CurveExtents, axis: Axis) -> f32 {
    match axis {
        Axis::Horizontal => extents.min_x,
        Axis::Vertical => extents.min_y,
    }
}

fn compute_band_count(span: f32, band_size: f32) -> usize {
    let count = (span.max(1.0) / band_size).ceil();
    usize::try_from(count as u64)
        .unwrap_or(MAX_BAND_COUNT)
        .clamp(1, MAX_BAND_COUNT)
}

fn compute_band_scale(minimum: f32, maximum: f32, count: usize) -> f32 {
    let _ = minimum;
    count as f32 / (maximum - minimum).max(1.0)
}

fn compute_band_offset(minimum: f32, maximum: f32, count: usize) -> f32 {
    -(minimum * compute_band_scale(minimum, maximum, count))
}

fn band_index(value: f32, scale: f32, offset: f32, count: usize) -> usize {
    let upper = count.saturating_sub(1) as f32;
    ((value * scale) + offset).trunc().clamp(0.0, upper) as usize
}

fn validate_curves(curves: &[QuadraticCurve]) -> Result<(), SlugError> {
    if curves.iter().copied().any(|curve| !curve.is_finite()) {
        return Err(SlugError::NonFiniteGeometry);
    }
    Ok(())
}

#[derive(Clone, Copy, Debug)]
enum RayDirection {
    Left,
    Right,
}

/// Evaluate supersampled coverage by walking every curve without band pruning.
///
/// This oracle never consumes caller-supplied or serialized band data. It
/// derives ray splits once from validated curves, then walks every curve;
/// offsets, index lists, packed words, and early-exit pruning cannot influence
/// the reference result.
pub fn coverage_all_curves(
    curves: &[QuadraticCurve],
    bounds: GlyphBounds,
    sample: Point,
    pixels_per_font_unit: f32,
) -> Result<f32, SlugError> {
    validate_coverage_inputs(curves, bounds, sample, pixels_per_font_unit)?;
    let oracle_splits = build_directional_bands(curves, bounds, DEFAULT_BAND_SIZE_FONT_UNITS)?;
    supersampled(sample, pixels_per_font_unit, |offset_sample| {
        Ok(coverage_all_curves_single_sample(
            curves,
            &oracle_splits,
            offset_sample,
            pixels_per_font_unit,
        ))
    })
}

/// Evaluate supersampled coverage through directional band lists and splits.
pub fn coverage_banded(
    curves: &[QuadraticCurve],
    bands: &DirectionalBands,
    sample: Point,
    pixels_per_font_unit: f32,
) -> Result<f32, SlugError> {
    validate_coverage_inputs(curves, bands.bounds, sample, pixels_per_font_unit)?;
    bands.validate(curves.len())?;
    supersampled(sample, pixels_per_font_unit, |offset_sample| {
        coverage_banded_single_sample(curves, bands, offset_sample, pixels_per_font_unit)
    })
}

fn supersampled(
    sample: Point,
    pixels_per_font_unit: f32,
    mut evaluate: impl FnMut(Point) -> Result<f32, SlugError>,
) -> Result<f32, SlugError> {
    let step = 0.25 / pixels_per_font_unit.max(COVERAGE_EPSILON);
    let offsets = [(-step, -step), (step, -step), (-step, step), (step, step)];
    let mut coverage = 0.0;
    for (x, y) in offsets {
        coverage += evaluate(Point::new(sample.x + x, sample.y + y))?;
    }
    Ok(coverage * 0.25)
}

fn validate_coverage_inputs(
    curves: &[QuadraticCurve],
    bounds: GlyphBounds,
    sample: Point,
    pixels_per_font_unit: f32,
) -> Result<(), SlugError> {
    validate_curves(curves)?;
    bounds.validate()?;
    if !sample.is_finite() {
        return Err(SlugError::NonFiniteGeometry);
    }
    if !pixels_per_font_unit.is_finite() || pixels_per_font_unit <= 0.0 {
        return Err(SlugError::InvalidScale);
    }
    Ok(())
}

fn coverage_all_curves_single_sample(
    curves: &[QuadraticCurve],
    oracle_splits: &DirectionalBands,
    sample: Point,
    pixels_per_font_unit: f32,
) -> f32 {
    if curves.is_empty() {
        return 0.0;
    }
    let horizontal_band = band_index(
        sample.y,
        oracle_splits.transform.y_scale,
        oracle_splits.transform.y_offset,
        oracle_splits.horizontal.len(),
    );
    let horizontal_direction = if sample.x < oracle_splits.horizontal[horizontal_band].split {
        RayDirection::Left
    } else {
        RayDirection::Right
    };
    let vertical_band = band_index(
        sample.x,
        oracle_splits.transform.x_scale,
        oracle_splits.transform.x_offset,
        oracle_splits.vertical.len(),
    );
    let vertical_direction = if sample.y < oracle_splits.vertical[vertical_band].split {
        RayDirection::Left
    } else {
        RayDirection::Right
    };
    coverage_all_curves_with_directions(
        curves,
        sample,
        pixels_per_font_unit,
        horizontal_direction,
        vertical_direction,
    )
}

fn coverage_all_curves_with_directions(
    curves: &[QuadraticCurve],
    sample: Point,
    pixels_per_font_unit: f32,
    horizontal_direction: RayDirection,
    vertical_direction: RayDirection,
) -> f32 {
    all_curves_accumulator(
        curves,
        sample,
        pixels_per_font_unit,
        horizontal_direction,
        vertical_direction,
    )
    .finish()
}

fn all_curves_accumulator(
    curves: &[QuadraticCurve],
    sample: Point,
    pixels_per_font_unit: f32,
    horizontal_direction: RayDirection,
    vertical_direction: RayDirection,
) -> CoverageAccumulator {
    let mut accumulator = CoverageAccumulator::default();
    for curve in curves {
        accumulate_horizontal(
            *curve,
            sample,
            pixels_per_font_unit,
            horizontal_direction,
            &mut accumulator,
        );
        accumulate_vertical(
            *curve,
            sample,
            pixels_per_font_unit,
            vertical_direction,
            &mut accumulator,
        );
    }
    accumulator
}

fn coverage_banded_single_sample(
    curves: &[QuadraticCurve],
    bands: &DirectionalBands,
    sample: Point,
    pixels_per_font_unit: f32,
) -> Result<f32, SlugError> {
    if curves.is_empty() {
        return Ok(0.0);
    }
    let horizontal_index = band_index(
        sample.y,
        bands.transform.y_scale,
        bands.transform.y_offset,
        bands.horizontal.len(),
    );
    let horizontal = bands.horizontal[horizontal_index];
    let horizontal_direction = if sample.x < horizontal.split {
        RayDirection::Left
    } else {
        RayDirection::Right
    };
    let mut accumulator = CoverageAccumulator::default();
    for curve_index in indices_for_header(bands, horizontal, horizontal_direction)? {
        let curve_index = usize::try_from(*curve_index)
            .map_err(|_| SlugError::MalformedBands("curve index does not fit usize"))?;
        let curve = curves
            .get(curve_index)
            .ok_or(SlugError::MalformedBands("curve index is out of bounds"))?;
        let extents = curve_extents(*curve);
        if should_stop(
            extents,
            sample,
            pixels_per_font_unit,
            Axis::Horizontal,
            horizontal_direction,
        ) {
            break;
        }
        accumulate_horizontal(
            *curve,
            sample,
            pixels_per_font_unit,
            horizontal_direction,
            &mut accumulator,
        );
    }

    let vertical_index = band_index(
        sample.x,
        bands.transform.x_scale,
        bands.transform.x_offset,
        bands.vertical.len(),
    );
    let vertical = bands.vertical[vertical_index];
    let vertical_direction = if sample.y < vertical.split {
        RayDirection::Left
    } else {
        RayDirection::Right
    };
    for curve_index in indices_for_header(bands, vertical, vertical_direction)? {
        let curve_index = usize::try_from(*curve_index)
            .map_err(|_| SlugError::MalformedBands("curve index does not fit usize"))?;
        let curve = curves
            .get(curve_index)
            .ok_or(SlugError::MalformedBands("curve index is out of bounds"))?;
        let extents = curve_extents(*curve);
        if should_stop(
            extents,
            sample,
            pixels_per_font_unit,
            Axis::Vertical,
            vertical_direction,
        ) {
            break;
        }
        accumulate_vertical(
            *curve,
            sample,
            pixels_per_font_unit,
            vertical_direction,
            &mut accumulator,
        );
    }
    Ok(accumulator.finish())
}

fn indices_for_header(
    bands: &DirectionalBands,
    header: DirectionalBandHeader,
    direction: RayDirection,
) -> Result<&[u32], SlugError> {
    let offset = match direction {
        RayDirection::Left => header.ascending_offset,
        RayDirection::Right => header.descending_offset,
    };
    let start = usize::try_from(offset)
        .map_err(|_| SlugError::MalformedBands("list offset does not fit usize"))?;
    let count = usize::try_from(header.curve_count)
        .map_err(|_| SlugError::MalformedBands("curve count does not fit usize"))?;
    let end = start
        .checked_add(count)
        .ok_or(SlugError::MalformedBands("list range overflow"))?;
    bands
        .curve_indices
        .get(start..end)
        .ok_or(SlugError::MalformedBands("list range is out of bounds"))
}

fn should_stop(
    extents: CurveExtents,
    sample: Point,
    pixels_per_font_unit: f32,
    axis: Axis,
    direction: RayDirection,
) -> bool {
    let (minimum, maximum, coordinate) = match axis {
        Axis::Horizontal => (extents.min_x, extents.max_x, sample.x),
        Axis::Vertical => (extents.min_y, extents.max_y, sample.y),
    };
    match direction {
        RayDirection::Left => (minimum - coordinate) * pixels_per_font_unit > 0.5,
        RayDirection::Right => (maximum - coordinate) * pixels_per_font_unit < -0.5,
    }
}

#[derive(Debug, Default)]
struct CoverageAccumulator {
    x_coverage: f32,
    y_coverage: f32,
    x_weight: f32,
    y_weight: f32,
}

impl CoverageAccumulator {
    fn finish(self) -> f32 {
        ((self.x_coverage * self.x_weight + self.y_coverage * self.y_weight).abs()
            / (self.x_weight + self.y_weight).max(COVERAGE_EPSILON))
        .max(self.x_coverage.abs().min(self.y_coverage.abs()))
        .clamp(0.0, 1.0)
    }
}

fn accumulate_horizontal(
    curve: QuadraticCurve,
    sample: Point,
    pixels_per_font_unit: f32,
    direction: RayDirection,
    accumulator: &mut CoverageAccumulator,
) {
    let p0 = Point::new(
        curve.p0.x - sample.x,
        curve.p0.y - sample.y + COVERAGE_EPSILON,
    );
    let p1 = Point::new(
        curve.p1.x - sample.x,
        curve.p1.y - sample.y + COVERAGE_EPSILON,
    );
    let p2 = Point::new(
        curve.p2.x - sample.x,
        curve.p2.y - sample.y + COVERAGE_EPSILON,
    );
    for root in quadratic_axis_roots(p0.y, p1.y, p2.y).iter() {
        let derivative = quadratic_derivative(p0.y, p1.y, p2.y, root);
        if derivative.abs() <= COVERAGE_EPSILON {
            continue;
        }
        add_horizontal_sample(
            evaluate_quadratic(p0.x, p1.x, p2.x, root),
            pixels_per_font_unit,
            direction,
            derivative.signum(),
            accumulator,
        );
    }
}

fn accumulate_vertical(
    curve: QuadraticCurve,
    sample: Point,
    pixels_per_font_unit: f32,
    direction: RayDirection,
    accumulator: &mut CoverageAccumulator,
) {
    let p0 = Point::new(curve.p0.x - sample.x, curve.p0.y - sample.y);
    let p1 = Point::new(curve.p1.x - sample.x, curve.p1.y - sample.y);
    let p2 = Point::new(curve.p2.x - sample.x, curve.p2.y - sample.y);
    for root in quadratic_axis_roots(p0.x, p1.x, p2.x).iter() {
        let derivative = quadratic_derivative(p0.x, p1.x, p2.x, root);
        if derivative.abs() <= COVERAGE_EPSILON {
            continue;
        }
        add_vertical_sample(
            evaluate_quadratic(p0.y, p1.y, p2.y, root),
            pixels_per_font_unit,
            direction,
            derivative.signum(),
            accumulator,
        );
    }
}

fn add_horizontal_sample(
    distance: f32,
    pixels_per_font_unit: f32,
    direction: RayDirection,
    sign: f32,
    accumulator: &mut CoverageAccumulator,
) {
    let signed_distance = distance * pixels_per_font_unit;
    let directional_sign = match direction {
        RayDirection::Left => -sign,
        RayDirection::Right => sign,
    };
    accumulator.x_coverage += directional_sign * directional_sample(signed_distance, direction);
    accumulator.x_weight = accumulator
        .x_weight
        .max(saturate(1.0 - signed_distance.abs() * 2.0));
}

fn add_vertical_sample(
    distance: f32,
    pixels_per_font_unit: f32,
    direction: RayDirection,
    sign: f32,
    accumulator: &mut CoverageAccumulator,
) {
    let signed_distance = distance * pixels_per_font_unit;
    let directional_sign = match direction {
        RayDirection::Left => -sign,
        RayDirection::Right => sign,
    };
    accumulator.y_coverage += directional_sign * directional_sample(signed_distance, direction);
    accumulator.y_weight = accumulator
        .y_weight
        .max(saturate(1.0 - signed_distance.abs() * 2.0));
}

fn directional_sample(signed_distance: f32, direction: RayDirection) -> f32 {
    match direction {
        RayDirection::Left => saturate(0.5 - signed_distance),
        RayDirection::Right => saturate(signed_distance + 0.5),
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct QuadraticRoots {
    values: [f32; 2],
    count: usize,
}

impl QuadraticRoots {
    fn iter(self) -> impl Iterator<Item = f32> {
        self.values.into_iter().take(self.count)
    }
}

fn quadratic_axis_roots(first: f32, control: f32, last: f32) -> QuadraticRoots {
    let a = first - (2.0 * control) + last;
    let b = 2.0 * (control - first);
    let c = first;
    if a.abs() <= COVERAGE_EPSILON {
        if b.abs() <= COVERAGE_EPSILON {
            return QuadraticRoots::default();
        }
        return roots_in_half_open_unit_interval([-c / b, f32::NAN]);
    }
    let discriminant = b.mul_add(b, -4.0 * a * c);
    if discriminant < 0.0 {
        return QuadraticRoots::default();
    }
    let square_root = discriminant.sqrt();
    roots_in_half_open_unit_interval([
        (-b - square_root) / (2.0 * a),
        (-b + square_root) / (2.0 * a),
    ])
}

fn roots_in_half_open_unit_interval(mut candidates: [f32; 2]) -> QuadraticRoots {
    candidates.sort_by(f32::total_cmp);
    let mut roots = QuadraticRoots::default();
    for candidate in candidates {
        if !candidate.is_finite() || !(0.0..1.0).contains(&candidate) {
            continue;
        }
        if roots.count > 0 && (roots.values[roots.count - 1] - candidate).abs() <= COVERAGE_EPSILON
        {
            continue;
        }
        roots.values[roots.count] = candidate;
        roots.count += 1;
    }
    roots
}

fn quadratic_derivative(first: f32, control: f32, last: f32, t: f32) -> f32 {
    2.0 * ((1.0 - t) * (control - first) + t * (last - control))
}

fn evaluate_quadratic(first: f32, control: f32, last: f32, t: f32) -> f32 {
    let inverse = 1.0 - t;
    inverse * inverse * first + 2.0 * inverse * t * control + t * t * last
}

fn saturate(value: f32) -> f32 {
    value.clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cubic_approximation_preserves_endpoints_and_subdivides_curvature() {
        let cubic = CubicCurve {
            p0: Point::new(0.0, 0.0),
            p1: Point::new(0.0, 100.0),
            p2: Point::new(100.0, 100.0),
            p3: Point::new(100.0, 0.0),
        };
        let curves = approximate_cubic(cubic, 0.25).expect("cubic should approximate");
        assert!(curves.len() > 1);
        assert_eq!(curves.first().map(|curve| curve.p0), Some(cubic.p0));
        assert_eq!(curves.last().map(|curve| curve.p2), Some(cubic.p3));
        for pair in curves.windows(2) {
            assert_eq!(pair[0].p2, pair[1].p0);
        }
    }

    #[test]
    fn split_minimizes_the_worse_directional_list() {
        let extents = vec![
            CurveExtents {
                min_x: 0.0,
                max_x: 4.0,
                min_y: 0.0,
                max_y: 0.0,
            },
            CurveExtents {
                min_x: 2.0,
                max_x: 6.0,
                min_y: 0.0,
                max_y: 0.0,
            },
            CurveExtents {
                min_x: 5.0,
                max_x: 8.0,
                min_y: 0.0,
                max_y: 0.0,
            },
        ];
        let descending = vec![2, 1, 0];
        let ascending = vec![0, 1, 2];
        let split = choose_band_split(
            &descending,
            &ascending,
            &extents,
            Axis::Horizontal,
            0.0,
            8.0,
        );
        let chosen_left = extents
            .iter()
            .filter(|extent| extent.min_x <= split)
            .count();
        let chosen_right = extents
            .iter()
            .filter(|extent| extent.max_x >= split)
            .count();
        let chosen_worst = chosen_left.max(chosen_right);
        let brute_worst = extents
            .iter()
            .map(|candidate| candidate.max_x)
            .map(|candidate| {
                let left = extents
                    .iter()
                    .filter(|extent| extent.min_x <= candidate)
                    .count();
                let right = extents
                    .iter()
                    .filter(|extent| extent.max_x >= candidate)
                    .count();
                left.max(right)
            })
            .min()
            .expect("fixture has candidates");
        assert_eq!(chosen_worst, brute_worst);
    }
}
