use teamy_terminal_font::EMBEDDED_FONT_SHA256;
use teamy_terminal_font::slug::DEFAULT_BAND_SIZE_FONT_UNITS;
use teamy_terminal_font::slug::DIRECTIONAL_BAND_HEADER_WORDS;
use teamy_terminal_font::slug::DirectionalBands;
use teamy_terminal_font::slug::GPU_FONT_METRICS_WORDS;
use teamy_terminal_font::slug::GPU_GLYPH_FLAG_FALLBACK;
use teamy_terminal_font::slug::GPU_GLYPH_METADATA_WORDS;
use teamy_terminal_font::slug::GlyphBounds;
use teamy_terminal_font::slug::GpuGlyphMetadata;
use teamy_terminal_font::slug::Point;
use teamy_terminal_font::slug::QuadraticCurve;
use teamy_terminal_font::slug::SlugError;
use teamy_terminal_font::slug::SlugFont;
use teamy_terminal_font::slug::build_directional_bands;
use teamy_terminal_font::slug::build_gpu_glyph_metadata;
use teamy_terminal_font::slug::coverage_all_curves;
use teamy_terminal_font::slug::coverage_banded;

const G_FALSE_LINE_REGRESSION: &str = include_str!("fixtures/slug-g-false-line-regression-v1.json");

fn embedded_geometry(
    character: char,
) -> (
    teamy_terminal_font::slug::SlugFontMetrics,
    teamy_terminal_font::slug::GlyphGeometry,
    DirectionalBands,
) {
    let font = SlugFont::embedded().expect("embedded Slug font should parse");
    let metrics = font.metrics();
    let geometry = font
        .glyph_geometry(character)
        .expect("fixture glyph should produce geometry");
    let bands = build_directional_bands(
        &geometry.curves,
        geometry.bounds,
        DEFAULT_BAND_SIZE_FONT_UNITS,
    )
    .expect("fixture glyph should build bands");
    (metrics, geometry, bands)
}

fn assert_grid_parity(character: char) {
    let (metrics, geometry, bands) = embedded_geometry(character);
    let scale = 256.0 / f32::from(metrics.units_per_em);
    let columns = 24_u32;
    let rows = 24_u32;
    for row in 0..rows {
        for column in 0..columns {
            let sample = Point::new(
                geometry.bounds.min_x
                    + (geometry.bounds.max_x - geometry.bounds.min_x) * (column as f32 + 0.5)
                        / columns as f32,
                geometry.bounds.min_y
                    + (geometry.bounds.max_y - geometry.bounds.min_y) * (row as f32 + 0.5)
                        / rows as f32,
            );
            let oracle = coverage_all_curves(&geometry.curves, geometry.bounds, sample, scale)
                .expect("all-curves coverage should evaluate");
            let banded = coverage_banded(&geometry.curves, &bands, sample, scale)
                .expect("banded coverage should evaluate");
            assert!(
                (banded - oracle).abs() <= 0.000_1,
                "{character:?} mismatch at ({column}, {row}): banded={banded} oracle={oracle}"
            );
        }
    }
}

#[test]
fn difficult_primary_glyphs_have_directional_bands_and_match_the_oracle() {
    for character in ['g', '/', 'b', 'r', '6'] {
        let (_, geometry, bands) = embedded_geometry(character);
        assert!(
            !geometry.used_fallback,
            "{character:?} unexpectedly fell back"
        );
        assert!(!geometry.curves.is_empty(), "{character:?} has no outline");
        assert!(
            bands
                .horizontal
                .iter()
                .chain(&bands.vertical)
                .any(|header| header.curve_count > 1),
            "{character:?} should exercise a multi-curve directional band"
        );
        assert_grid_parity(character);
    }
}

#[test]
fn ascii_box_powerline_and_unicode_fixtures_build() {
    let font = SlugFont::embedded().expect("embedded Slug font should parse");
    let fixtures = [
        "PS> cargo test --package teamy-terminal-font",
        "┌─┐│└┘═║╔╗╚╝",
        "\u{e0b0}\u{e0b1}\u{e0b2}\u{e0b3}",
        "éλ界█\u{0301}",
    ];
    for fixture in fixtures {
        for character in fixture.chars() {
            let geometry = font
                .glyph_geometry(character)
                .expect("text fixture should resolve or fall back");
            let bands = build_directional_bands(
                &geometry.curves,
                geometry.bounds,
                DEFAULT_BAND_SIZE_FONT_UNITS,
            )
            .expect("text fixture should build directional bands");
            bands
                .validate(geometry.curves.len())
                .expect("text fixture bands should validate");
        }
    }
}

#[test]
fn empty_and_missing_glyph_behavior_is_explicit() {
    let font = SlugFont::embedded().expect("embedded Slug font should parse");
    let space = font.glyph_geometry(' ').expect("space should resolve");
    assert!(!space.used_fallback);
    assert!(space.curves.is_empty());
    let space_bands =
        build_directional_bands(&space.curves, space.bounds, DEFAULT_BAND_SIZE_FONT_UNITS)
            .expect("empty glyph should retain valid empty bands");
    assert!(
        space_bands
            .horizontal
            .iter()
            .all(|header| header.curve_count == 0)
    );
    assert!(
        space_bands
            .vertical
            .iter()
            .all(|header| header.curve_count == 0)
    );

    let missing_character = char::from_u32(0x10_FFFF).expect("valid Unicode scalar");
    let missing = font
        .glyph_geometry(missing_character)
        .expect("missing glyph should use fallback");
    let question = font.glyph_geometry('?').expect("fallback should resolve");
    assert!(missing.used_fallback);
    assert_eq!(missing.resolved_character, '?');
    assert_eq!(missing.glyph_id, question.glyph_id);
    assert_eq!(missing.bounds, question.bounds);
    assert_eq!(missing.curves, question.curves);
}

#[test]
fn flat_segments_match_the_independent_oracle() {
    let bounds = GlyphBounds::new(0.0, 0.0, 10.0, 10.0).expect("square bounds");
    let curves = vec![
        QuadraticCurve::line(Point::new(0.0, 0.0), Point::new(10.0, 0.0)),
        QuadraticCurve::line(Point::new(10.0, 0.0), Point::new(10.0, 10.0)),
        QuadraticCurve::line(Point::new(10.0, 10.0), Point::new(0.0, 10.0)),
        QuadraticCurve::line(Point::new(0.0, 10.0), Point::new(0.0, 0.0)),
    ];
    let bands = build_directional_bands(&curves, bounds, DEFAULT_BAND_SIZE_FONT_UNITS)
        .expect("square bands");
    for y in -2..=12 {
        for x in -2..=12 {
            let sample = Point::new(x as f32 + 0.25, y as f32 + 0.25);
            let oracle =
                coverage_all_curves(&curves, bounds, sample, 1.0).expect("square oracle coverage");
            let banded =
                coverage_banded(&curves, &bands, sample, 1.0).expect("square banded coverage");
            assert!((banded - oracle).abs() <= 0.000_1);
        }
    }
}

#[test]
fn malformed_geometry_bounds_headers_and_offsets_are_rejected() {
    let invalid_curve = QuadraticCurve {
        p0: Point::new(f32::NAN, 0.0),
        p1: Point::new(0.0, 0.0),
        p2: Point::new(1.0, 1.0),
    };
    let bounds = GlyphBounds::new(0.0, 0.0, 1.0, 1.0).expect("valid bounds");
    assert_eq!(
        build_directional_bands(&[invalid_curve], bounds, 1.0),
        Err(SlugError::NonFiniteGeometry)
    );
    assert_eq!(
        GlyphBounds::new(2.0, 0.0, 1.0, 1.0),
        Err(SlugError::InvalidBounds)
    );
    assert_eq!(
        build_directional_bands(&[], bounds, 0.0),
        Err(SlugError::InvalidScale)
    );

    let curve = QuadraticCurve::line(Point::new(0.0, 0.0), Point::new(1.0, 1.0));
    let mut bands = build_directional_bands(&[curve], bounds, 1.0).expect("valid bands");
    bands.horizontal[0].descending_offset = u32::MAX;
    assert_eq!(
        bands.validate(1),
        Err(SlugError::MalformedBands("list range is out of bounds"))
    );

    let mut bands = build_directional_bands(&[curve], bounds, 1.0).expect("valid bands");
    bands.horizontal[0].split = f32::NAN;
    assert_eq!(
        bands.validate(1),
        Err(SlugError::MalformedBands("band split is not finite"))
    );

    let mut bands = build_directional_bands(&[curve], bounds, 1.0).expect("valid bands");
    let first_index = usize::try_from(bands.horizontal[0].descending_offset)
        .expect("test offset should fit usize");
    bands.curve_indices[first_index] = u32::MAX;
    assert_eq!(
        bands.validate(1),
        Err(SlugError::MalformedBands("curve index is out of bounds"))
    );

    let bands = build_directional_bands(&[curve], bounds, 1.0).expect("valid bands");
    assert_eq!(
        coverage_banded(&[curve], &bands, Point::new(0.5, 0.5), 0.0),
        Err(SlugError::InvalidScale)
    );
}

#[test]
fn curve_band_and_glyph_exports_have_a_stable_shader_word_contract() {
    let (metrics, geometry, bands) = embedded_geometry('g');
    let first_curve = geometry.curves[0];
    assert_eq!(
        first_curve.gpu_vec4s(),
        [
            [
                first_curve.p0.x,
                first_curve.p0.y,
                first_curve.p1.x,
                first_curve.p1.y
            ],
            [first_curve.p2.x, first_curve.p2.y, 0.0, 0.0],
        ]
    );

    let packed = bands
        .packed_words(geometry.curves.len())
        .expect("band words should pack");
    let table_words =
        (bands.horizontal.len() + bands.vertical.len()) * DIRECTIONAL_BAND_HEADER_WORDS;
    assert_eq!(packed[0], bands.horizontal[0].curve_count);
    assert!(usize::try_from(packed[1]).expect("offset should fit") >= table_words);
    assert!(usize::try_from(packed[2]).expect("offset should fit") >= table_words);
    assert_eq!(packed[3], bands.horizontal[0].split.to_bits());

    let metadata = build_gpu_glyph_metadata(&geometry, &bands, metrics, 17, 29)
        .expect("metadata should build");
    let words = metadata.words();
    assert_eq!(words.len(), GPU_GLYPH_METADATA_WORDS);
    assert_eq!(words[0], 17);
    assert_eq!(
        words[1],
        u32::try_from(geometry.curves.len()).expect("curve count")
    );
    assert_eq!(words[2], 29);
    assert_eq!(
        words[3],
        u32::try_from(bands.horizontal.len()).expect("horizontal count")
    );
    assert_eq!(
        words[4],
        u32::try_from(bands.vertical.len()).expect("vertical count")
    );
    assert_eq!(words[5], 0);
    assert_eq!(words[6], u32::from(metrics.units_per_em));
    assert_eq!(words[7], geometry.advance.to_bits());
    assert_eq!(words[8], geometry.bounds.min_x.to_bits());
    assert_eq!(words[15], bands.transform.y_offset.to_bits());
    assert_eq!(metrics.gpu_words().len(), GPU_FONT_METRICS_WORDS);
    assert_eq!(
        size_of::<GpuGlyphMetadata>(),
        GPU_GLYPH_METADATA_WORDS * size_of::<u32>()
    );
    assert_eq!(align_of::<GpuGlyphMetadata>(), align_of::<u32>());

    let font = SlugFont::embedded().expect("embedded Slug font should parse");
    let missing = font
        .glyph_geometry(char::from_u32(0x10_FFFF).expect("valid Unicode scalar"))
        .expect("missing glyph should fall back");
    let missing_bands = build_directional_bands(
        &missing.curves,
        missing.bounds,
        DEFAULT_BAND_SIZE_FONT_UNITS,
    )
    .expect("fallback bands");
    let missing_metadata = build_gpu_glyph_metadata(&missing, &missing_bands, metrics, 0, 0)
        .expect("fallback metadata");
    assert_eq!(missing_metadata.words()[5], GPU_GLYPH_FLAG_FALLBACK);
}

#[test]
fn g_false_line_regression_is_machine_readable_and_oracle_backed() {
    let (metrics, geometry, bands) = embedded_geometry('g');
    let width = 96_u32;
    let height = 96_u32;
    let scale = 256.0 / f32::from(metrics.units_per_em);
    let mut max_delta = 0.0_f32;
    let mut mismatch_pixels = 0_u32;
    let mut false_positive_pixels = 0_u32;
    let mut longest_false_positive_vertical_run = 0_u32;
    let mut column_runs = vec![0_u32; usize::try_from(width).expect("fixture width")];
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;

    for row in 0..height {
        for column in 0..width {
            let sample = Point::new(
                geometry.bounds.min_x
                    + (geometry.bounds.max_x - geometry.bounds.min_x) * (column as f32 + 0.5)
                        / width as f32,
                geometry.bounds.min_y
                    + (geometry.bounds.max_y - geometry.bounds.min_y) * (row as f32 + 0.5)
                        / height as f32,
            );
            let oracle = coverage_all_curves(&geometry.curves, geometry.bounds, sample, scale)
                .expect("g oracle should evaluate");
            let banded = coverage_banded(&geometry.curves, &bands, sample, scale)
                .expect("g bands should evaluate");
            let delta = (banded - oracle).abs();
            max_delta = max_delta.max(delta);
            mismatch_pixels += u32::from(delta > 0.01);
            let false_positive = banded >= 0.5 && oracle <= 0.05;
            false_positive_pixels += u32::from(false_positive);
            let run = &mut column_runs[usize::try_from(column).expect("column should fit")];
            if false_positive {
                *run += 1;
                longest_false_positive_vertical_run = longest_false_positive_vertical_run.max(*run);
            } else {
                *run = 0;
            }
            let quantized = (banded * 255.0).round().clamp(0.0, 255.0) as u8;
            hash ^= u64::from(quantized);
            hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
        }
    }

    let distinct_directional_lists = bands
        .horizontal
        .iter()
        .chain(&bands.vertical)
        .filter(|header| {
            let count = usize::try_from(header.curve_count).expect("header count");
            let descending = usize::try_from(header.descending_offset).expect("descending offset");
            let ascending = usize::try_from(header.ascending_offset).expect("ascending offset");
            bands.curve_indices[descending..descending + count]
                != bands.curve_indices[ascending..ascending + count]
        })
        .count();
    let document = format!(
        concat!(
            "{{\n",
            "  \"schema\": \"teamy-terminal.slug-g-false-line.v1\",\n",
            "  \"font_sha256\": \"{}\",\n",
            "  \"glyph\": \"g\",\n",
            "  \"grid_width\": {},\n",
            "  \"grid_height\": {},\n",
            "  \"font_size_pixels\": 256,\n",
            "  \"directional_header_words\": {},\n",
            "  \"distinct_directional_lists\": {},\n",
            "  \"max_oracle_delta_millionths\": {},\n",
            "  \"structural_mismatch_pixels\": {},\n",
            "  \"false_positive_pixels\": {},\n",
            "  \"longest_false_positive_vertical_run\": {},\n",
            "  \"coverage_fnv1a64\": \"{:016x}\"\n",
            "}}\n"
        ),
        EMBEDDED_FONT_SHA256,
        width,
        height,
        DIRECTIONAL_BAND_HEADER_WORDS,
        distinct_directional_lists,
        (max_delta * 1_000_000.0).ceil() as u32,
        mismatch_pixels,
        false_positive_pixels,
        longest_false_positive_vertical_run,
        hash,
    );
    assert_eq!(G_FALSE_LINE_REGRESSION, document);
}
