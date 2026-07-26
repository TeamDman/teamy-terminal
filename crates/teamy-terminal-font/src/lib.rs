//! Caskaydia Cove glyph rasterization for terminal snapshots.
//!
//! The embedded Caskaydia face is always available as the deterministic base
//! font. On systems that provide them, additional installed faces are used as
//! per-glyph fallbacks for symbols and emoji that Caskaydia does not contain.
//! The crate produces ordinary RGBA frames and knows nothing about Windows or
//! Vulkan; the renderer is responsible for presenting those frames through the
//! graphics API.

use eyre::Result;
use fontdb::Database;
use fontdb::Family;
use fontdb::Query;
use fontdb::Source as FontSource;
use fontdue::Font;
use fontdue::FontSettings;
use num_traits::ToPrimitive;
use std::collections::HashMap;
use swash::FontRef;
use swash::scale::Render;
use swash::scale::ScaleContext;
use swash::scale::Source as SwashSource;
use swash::scale::StrikeWith;
use swash::scale::image::Content;
use teamy_terminal_core::Cell;
use teamy_terminal_core::Color;
use teamy_terminal_core::TerminalSnapshot;
use unicode_width::UnicodeWidthChar;

const EMBEDDED_FONT: &[u8] = include_bytes!("../assets/CaskaydiaCoveNerdFontMono-Regular.ttf");
const PRIMARY_FONT_FAMILY: &str = "CaskaydiaCove Nerd Font Mono";
const FALLBACK_FONT_FAMILIES: &[&str] = &[
    "Segoe UI Emoji",
    "Segoe UI Symbol",
    "Noto Sans Symbols 2",
    "Noto Color Emoji",
];

/// The default embedded font size in physical pixels.
pub const DEFAULT_PIXEL_SIZE: f32 = 16.0;

/// The RGBA output of a font render pass.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RasterizedFrame {
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// Premultiplied-independent RGBA8 pixels in row-major order.
    pub pixels: Vec<u8>,
}

/// A rectangular range of terminal cells to refresh in a cached frame.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CellRect {
    /// Zero-based left column.
    pub column: u16,
    /// Zero-based top row.
    pub row: u16,
    /// Number of columns covered by the rectangle.
    pub columns: u16,
    /// Number of rows covered by the rectangle.
    pub rows: u16,
}

impl CellRect {
    /// Construct a cell rectangle.
    #[must_use]
    pub const fn new(column: u16, row: u16, columns: u16, rows: u16) -> Self {
        Self {
            column,
            row,
            columns,
            rows,
        }
    }

    /// Cover the complete visible terminal grid.
    #[must_use]
    pub const fn full(columns: u16, rows: u16) -> Self {
        Self::new(0, 0, columns, rows)
    }

    fn contains(self, column: u16, row: u16) -> bool {
        u32::from(column) >= u32::from(self.column)
            && u32::from(row) >= u32::from(self.row)
            && u32::from(column) < u32::from(self.column) + u32::from(self.columns)
            && u32::from(row) < u32::from(self.row) + u32::from(self.rows)
    }
}

#[derive(Clone, Debug)]
struct Glyph {
    metrics: fontdue::Metrics,
    bitmap: Vec<u8>,
    color_bitmap: Option<ColorBitmap>,
}

#[derive(Clone, Debug)]
struct ColorBitmap {
    left: i32,
    top: i32,
    width: u32,
    height: u32,
    pixels: Vec<u8>,
}

struct FontFace {
    font: Font,
    data: Vec<u8>,
    index: u32,
}

/// A terminal font backed by Caskaydia Cove and installed fallback faces.
pub struct TerminalFont {
    fonts: Vec<FontFace>,
    swash_context: ScaleContext,
    pixel_size: f32,
    cell_width: u32,
    cell_height: u32,
    baseline: i32,
    glyphs: HashMap<char, Glyph>,
}

impl std::fmt::Debug for TerminalFont {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TerminalFont")
            .field("pixel_size", &self.pixel_size)
            .field("cell_width", &self.cell_width)
            .field("cell_height", &self.cell_height)
            .field("baseline", &self.baseline)
            .field("font_count", &self.fonts.len())
            .field("cached_glyphs", &self.glyphs.len())
            .finish_non_exhaustive()
    }
}

impl TerminalFont {
    /// Load the embedded Caskaydia face at the requested pixel size.
    ///
    /// # Errors
    ///
    /// Returns an error if the embedded font bytes cannot be parsed.
    pub fn new(pixel_size: f32) -> Result<Self> {
        let pixel_size = pixel_size.max(8.0);
        let embedded_font = Font::from_bytes(EMBEDDED_FONT, FontSettings::default())
            .map_err(|error| eyre::eyre!("embedded Caskaydia font is invalid: {error}"))?;
        let embedded_face = FontFace {
            font: embedded_font,
            data: EMBEDDED_FONT.to_vec(),
            index: 0,
        };

        let mut database = Database::new();
        database.load_system_fonts();
        let mut fonts = Vec::new();
        if let Some(font) = load_system_font(&database, PRIMARY_FONT_FAMILY) {
            fonts.push(font);
        }
        fonts.push(embedded_face);
        fonts.extend(
            FALLBACK_FONT_FAMILIES
                .iter()
                .filter_map(|family_name| load_system_font(&database, family_name)),
        );
        let Some(primary_font) = fonts.first() else {
            return Err(eyre::eyre!("no terminal font was loaded"));
        };
        let metrics = primary_font.font.metrics('M', pixel_size);
        let line_metrics = primary_font.font.horizontal_line_metrics(pixel_size);
        let cell_width = rounded_u32(metrics.advance_width, 1);
        let (cell_height, baseline) = line_metrics.map_or_else(
            || (rounded_u32(pixel_size, 1), rounded_i32(pixel_size, 1)),
            |line| {
                (
                    rounded_u32(line.new_line_size, 1),
                    rounded_i32(line.ascent, 1),
                )
            },
        );
        Ok(Self {
            fonts,
            swash_context: ScaleContext::new(),
            pixel_size,
            cell_width,
            cell_height,
            baseline,
            glyphs: HashMap::new(),
        })
    }

    /// Load the standard-size embedded face.
    ///
    /// # Errors
    ///
    /// Returns an error if the embedded font bytes cannot be parsed.
    pub fn embedded() -> Result<Self> {
        Self::new(DEFAULT_PIXEL_SIZE)
    }

    /// Width of one monospace terminal cell.
    #[must_use]
    pub const fn cell_width(&self) -> u32 {
        self.cell_width
    }

    /// Height of one terminal cell.
    #[must_use]
    pub const fn cell_height(&self) -> u32 {
        self.cell_height
    }

    /// Render a terminal snapshot into a frame of the supplied pixel size.
    ///
    /// # Panics
    ///
    /// Panics only if the bounded terminal dimensions or embedded glyph
    /// metrics violate their internal `u16`/`i32` representation invariants.
    pub fn render(
        &mut self,
        snapshot: &TerminalSnapshot,
        width: u32,
        height: u32,
    ) -> RasterizedFrame {
        let pixel_count = usize::try_from(width)
            .unwrap_or(0)
            .saturating_mul(usize::try_from(height).unwrap_or(0));
        let mut pixels = vec![0_u8; pixel_count.saturating_mul(4)];
        for pixel in pixels.chunks_exact_mut(4) {
            pixel.copy_from_slice(&[30, 30, 30, 255]);
        }
        let mut frame = RasterizedFrame {
            width,
            height,
            pixels,
        };
        self.render_cells(
            snapshot,
            &mut frame,
            &[CellRect::full(snapshot.columns, snapshot.rows)],
        );
        frame
    }

    /// Refresh selected cell rectangles in an existing cached frame.
    ///
    /// The caller must expand damage around wide glyphs and cursor movement
    /// when necessary. Pixels outside `regions` are preserved, which makes the
    /// method suitable for a cached-frame dirty-rectangle prototype.
    pub fn render_cells(
        &mut self,
        snapshot: &TerminalSnapshot,
        frame: &mut RasterizedFrame,
        regions: &[CellRect],
    ) {
        let max_columns = (frame.width / self.cell_width).min(u32::from(snapshot.columns));
        let max_rows = (frame.height / self.cell_height).min(u32::from(snapshot.rows));
        for region in regions.iter().copied() {
            let first_column = u32::from(region.column).min(max_columns);
            let first_row = u32::from(region.row).min(max_rows);
            let last_column = first_column
                .saturating_add(u32::from(region.columns))
                .min(max_columns);
            let last_row = first_row
                .saturating_add(u32::from(region.rows))
                .min(max_rows);
            if first_column >= last_column || first_row >= last_row {
                continue;
            }

            let pixel_left = first_column.saturating_mul(self.cell_width);
            let pixel_top = first_row.saturating_mul(self.cell_height);
            let pixel_right = last_column.saturating_mul(self.cell_width).min(frame.width);
            let pixel_bottom = last_row.saturating_mul(self.cell_height).min(frame.height);
            for y in pixel_top..pixel_bottom {
                let row_start = (usize::try_from(y).unwrap_or(0)
                    * usize::try_from(frame.width).unwrap_or(0)
                    + usize::try_from(pixel_left).unwrap_or(0))
                    * 4;
                let row_end = (usize::try_from(y).unwrap_or(0)
                    * usize::try_from(frame.width).unwrap_or(0)
                    + usize::try_from(pixel_right).unwrap_or(0))
                    * 4;
                if let Some(row) = frame.pixels.get_mut(row_start..row_end) {
                    for pixel in row.chunks_exact_mut(4) {
                        pixel.copy_from_slice(&[30, 30, 30, 255]);
                    }
                }
            }

            for row in first_row..last_row {
                for column in first_column..last_column {
                    let column = u16::try_from(column).expect("rendered column fits in u16");
                    let row = u16::try_from(row).expect("rendered row fits in u16");
                    let Some(cell) = snapshot.cell(column, row) else {
                        continue;
                    };
                    if is_wide_character_continuation(snapshot, column, row, cell) {
                        continue;
                    }
                    self.draw_cell(
                        &mut frame.pixels,
                        frame.width,
                        frame.height,
                        u32::from(column),
                        u32::from(row),
                        cell,
                        snapshot.is_selected(column, row),
                    );
                }
            }

            if snapshot.cursor_visible
                && region.contains(snapshot.cursor.column, snapshot.cursor.row)
                && u32::from(snapshot.cursor.column) < max_columns
                && u32::from(snapshot.cursor.row) < max_rows
            {
                let x = u32::from(snapshot.cursor.column) * self.cell_width;
                let y = u32::from(snapshot.cursor.row) * self.cell_height;
                let cursor_color = [210, 210, 210, 190];
                for cursor_y in y..(y + self.cell_height).min(frame.height) {
                    for cursor_x in x..(x + 2).min(frame.width) {
                        put_pixel(
                            &mut frame.pixels,
                            frame.width,
                            cursor_x,
                            cursor_y,
                            cursor_color,
                        );
                    }
                }
            }
        }
    }

    fn draw_cell(
        &mut self,
        pixels: &mut [u8],
        frame_width: u32,
        frame_height: u32,
        column: u32,
        row: u32,
        cell: Cell,
        selected: bool,
    ) {
        let mut foreground = color(cell.foreground, false);
        let mut background = color(cell.background, true);
        if cell.bold {
            foreground = brighten(foreground);
        }
        if cell.inverse {
            std::mem::swap(&mut foreground, &mut background);
        }
        if selected {
            background = [70, 100, 170, 255];
            foreground = [255, 255, 255, 255];
        }
        let x = column * self.cell_width;
        let y = row * self.cell_height;
        let character_width = u32::try_from(cell.character.width().unwrap_or(1).clamp(1, 2))
            .expect("clamped Unicode width fits in u32");
        let glyph_cell_width_px = self.cell_width.saturating_mul(character_width);
        for cell_y in y..(y + self.cell_height).min(frame_height) {
            for cell_x in x..x.saturating_add(glyph_cell_width_px).min(frame_width) {
                put_pixel(pixels, frame_width, cell_x, cell_y, background);
            }
        }

        if cell.character != ' ' {
            let glyph = self.glyph(cell.character);
            let glyph_cell_width = glyph_cell_width_px
                .to_f32()
                .expect("terminal cell width is representable as f32");
            let glyph_origin_x = i32::try_from(x).expect("frame x coordinate fits in i32")
                + rounded_i32(
                    ((glyph_cell_width - glyph.metrics.advance_width).max(0.0) / 2.0).round(),
                    0,
                );
            let glyph_right = x.saturating_add(glyph_cell_width_px).min(frame_width);
            let mut target = GlyphRenderTarget {
                pixels,
                frame_width,
                frame_height,
            };
            draw_glyph(
                &mut target,
                &glyph,
                GlyphPlacement {
                    origin_x: glyph_origin_x,
                    cell_y: y,
                    baseline: self.baseline,
                    right: glyph_right,
                    foreground,
                },
            );
        }
        if cell.underline {
            let underline_y =
                (y + self.cell_height.saturating_sub(2)).min(frame_height.saturating_sub(1));
            for underline_x in x..(x + self.cell_width).min(frame_width) {
                put_pixel(pixels, frame_width, underline_x, underline_y, foreground);
            }
        }
    }

    fn glyph(&mut self, character: char) -> Glyph {
        if let Some(glyph) = self.glyphs.get(&character) {
            return glyph.clone();
        }

        let font_index = self.font_index_for(character).unwrap_or_else(|| {
            self.font_index_for('?')
                .expect("the embedded Caskaydia font must contain the fallback glyph")
        });
        let rendered_character = if self.fonts[font_index].font.has_glyph(character) {
            character
        } else {
            '?'
        };
        let (metrics, bitmap) = self.fonts[font_index]
            .font
            .rasterize(rendered_character, self.pixel_size);
        let color_bitmap = render_color_glyph(
            &mut self.swash_context,
            &self.fonts[font_index],
            rendered_character,
            self.pixel_size,
        );
        let glyph = Glyph {
            metrics,
            bitmap,
            color_bitmap,
        };
        let _ = self.glyphs.insert(character, glyph.clone());
        glyph
    }

    fn font_index_for(&self, character: char) -> Option<usize> {
        self.fonts
            .iter()
            .position(|font| font.font.has_glyph(character))
    }
}

struct GlyphRenderTarget<'a> {
    pixels: &'a mut [u8],
    frame_width: u32,
    frame_height: u32,
}

#[derive(Copy, Clone)]
struct GlyphPlacement {
    origin_x: i32,
    cell_y: u32,
    baseline: i32,
    right: u32,
    foreground: [u8; 4],
}

fn draw_glyph(target: &mut GlyphRenderTarget<'_>, glyph: &Glyph, placement: GlyphPlacement) {
    if let Some(color_bitmap) = glyph.color_bitmap.as_ref() {
        let color_origin_y = i32::try_from(placement.cell_y)
            .expect("frame y coordinate fits in i32")
            + placement.baseline
            - color_bitmap.top;
        for glyph_row in 0..color_bitmap.height {
            for glyph_column in 0..color_bitmap.width {
                let pixel_index = (usize::try_from(glyph_row).expect("glyph row fits in usize")
                    * usize::try_from(color_bitmap.width).expect("glyph width fits in usize")
                    + usize::try_from(glyph_column).expect("glyph column fits in usize"))
                    * 4;
                let Some(pixel) = color_bitmap.pixels.get(pixel_index..pixel_index + 4) else {
                    continue;
                };
                if pixel[3] == 0 {
                    continue;
                }
                let target_x = placement.origin_x
                    + color_bitmap.left
                    + i32::try_from(glyph_column).expect("glyph column fits in i32");
                let target_y =
                    color_origin_y + i32::try_from(glyph_row).expect("glyph row fits in i32");
                if let (Ok(target_x), Ok(target_y)) =
                    (u32::try_from(target_x), u32::try_from(target_y))
                    && target_x < placement.right
                    && target_y < target.frame_height
                {
                    put_pixel_blended(
                        target.pixels,
                        target.frame_width,
                        target_x,
                        target_y,
                        [pixel[0], pixel[1], pixel[2], 255],
                        pixel[3],
                    );
                }
            }
        }
        return;
    }

    let glyph_x = placement.origin_x + glyph.metrics.xmin;
    let glyph_y = i32::try_from(placement.cell_y).expect("frame y coordinate fits in i32")
        + placement.baseline
        - glyph.metrics.ymin
        - i32::try_from(glyph.metrics.height).expect("glyph height fits in i32");
    for glyph_row in 0..glyph.metrics.height {
        for glyph_column in 0..glyph.metrics.width {
            let alpha = glyph.bitmap[glyph_row * glyph.metrics.width + glyph_column];
            if alpha == 0 {
                continue;
            }
            let target_x = glyph_x + i32::try_from(glyph_column).expect("glyph column fits in i32");
            let target_y = glyph_y + i32::try_from(glyph_row).expect("glyph row fits in i32");
            if let (Ok(target_x), Ok(target_y)) = (u32::try_from(target_x), u32::try_from(target_y))
                && target_x < placement.right
                && target_y < target.frame_height
            {
                put_pixel_blended(
                    target.pixels,
                    target.frame_width,
                    target_x,
                    target_y,
                    placement.foreground,
                    alpha,
                );
            }
        }
    }
}

fn is_wide_character_continuation(
    snapshot: &TerminalSnapshot,
    column: u16,
    row: u16,
    cell: Cell,
) -> bool {
    if cell.character != ' ' || column == 0 {
        return false;
    }
    snapshot
        .cell(column - 1, row)
        .is_some_and(|previous| previous.character.width().is_some_and(|width| width >= 2))
}

fn load_system_font(database: &Database, family_name: &str) -> Option<FontFace> {
    let query = Query {
        families: &[Family::Name(family_name)],
        ..Query::default()
    };
    let font_id = database.query(&query)?;
    let face_info = database.face(font_id)?;
    let font_bytes = read_font_source(&face_info.source).ok()?;
    let settings = FontSettings {
        collection_index: face_info.index,
        ..FontSettings::default()
    };
    let font = Font::from_bytes(font_bytes.clone(), settings).ok()?;
    Some(FontFace {
        font,
        data: font_bytes,
        index: face_info.index,
    })
}

fn read_font_source(source: &FontSource) -> Result<Vec<u8>> {
    match source {
        FontSource::File(path) => std::fs::read(path)
            .map_err(|error| eyre::eyre!("failed to read system font {}: {error}", path.display())),
        FontSource::Binary(data) => Ok(data.as_ref().as_ref().to_vec()),
        FontSource::SharedFile(path, _) => std::fs::read(path)
            .map_err(|error| eyre::eyre!("failed to read system font {}: {error}", path.display())),
    }
}

fn render_color_glyph(
    context: &mut ScaleContext,
    face: &FontFace,
    character: char,
    pixel_size: f32,
) -> Option<ColorBitmap> {
    let font = FontRef::from_index(&face.data, usize::try_from(face.index).ok()?)?;
    let glyph_id = font.charmap().map(character);
    if glyph_id == 0 {
        return None;
    }
    let mut scaler = context.builder(font).size(pixel_size).hint(true).build();
    let image = Render::new(&[
        SwashSource::ColorOutline(0),
        SwashSource::ColorBitmap(StrikeWith::BestFit),
        SwashSource::Outline,
    ])
    .render(&mut scaler, glyph_id)?;
    if image.content != Content::Color {
        return None;
    }
    Some(ColorBitmap {
        left: image.placement.left,
        top: image.placement.top,
        width: image.placement.width,
        height: image.placement.height,
        pixels: image.data,
    })
}

fn color(color: Color, is_background: bool) -> [u8; 4] {
    let [red, green, blue] = match color {
        Color::Default if is_background => [0, 0, 0],
        Color::Default => [212, 212, 212],
        Color::Rgb(red, green, blue) => [red, green, blue],
        Color::Indexed(index) => palette(index),
    };
    [red, green, blue, 255]
}

fn palette(index: u8) -> [u8; 3] {
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
            let red = value / 36;
            let green = (value % 36) / 6;
            let blue = value % 6;
            [cube(red), cube(green), cube(blue)]
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

fn brighten(mut color: [u8; 4]) -> [u8; 4] {
    color[0] = color[0].saturating_add(24);
    color[1] = color[1].saturating_add(24);
    color[2] = color[2].saturating_add(24);
    color
}

fn put_pixel(pixels: &mut [u8], width: u32, x: u32, y: u32, color: [u8; 4]) {
    let index = (usize::try_from(y).unwrap_or(0) * usize::try_from(width).unwrap_or(0)
        + usize::try_from(x).unwrap_or(0))
        * 4;
    if let Some(pixel) = pixels.get_mut(index..index + 4) {
        pixel.copy_from_slice(&color);
    }
}

fn put_pixel_blended(pixels: &mut [u8], width: u32, x: u32, y: u32, color: [u8; 4], alpha: u8) {
    let index = (usize::try_from(y).unwrap_or(0) * usize::try_from(width).unwrap_or(0)
        + usize::try_from(x).unwrap_or(0))
        * 4;
    let Some(pixel) = pixels.get_mut(index..index + 4) else {
        return;
    };
    let alpha = u16::from(alpha);
    let inverse = 255_u16 - alpha;
    for channel in 0..3 {
        pixel[channel] = u8::try_from(
            (u16::from(color[channel]) * alpha + u16::from(pixel[channel]) * inverse) / 255,
        )
        .expect("blended colour channel fits in u8");
    }
    pixel[3] = 255;
}

fn rounded_u32(value: f32, fallback: u32) -> u32 {
    value.ceil().max(1.0).to_u32().unwrap_or(fallback).max(1)
}

fn rounded_i32(value: f32, fallback: i32) -> i32 {
    value.to_i32().unwrap_or(fallback)
}

#[cfg(test)]
mod tests {
    use super::*;
    use teamy_terminal_core::TerminalLimits;
    use teamy_terminal_core::TerminalSession;

    #[test]
    fn embedded_font_has_monospace_metrics_and_renders_pixels() {
        let mut font = TerminalFont::embedded().expect("embedded font should parse");
        assert!(font.cell_width() > 0);
        assert!(font.cell_height() > 0);
        let mut terminal = TerminalSession::new(TerminalLimits::default(), 2, 1);
        terminal.process_bytes(b"A");
        let snapshot = terminal.snapshot();
        let frame = font.render(&snapshot, 64, 32);
        let repeat = font.render(&snapshot, 64, 32);
        assert_eq!(frame.pixels.len(), 64 * 32 * 4);
        assert_eq!(frame, repeat);
        assert!(frame.pixels.chunks_exact(4).any(|pixel| pixel[0] > 30));
    }

    #[test]
    fn dirty_cell_render_matches_a_full_render() {
        let mut font = TerminalFont::embedded().expect("embedded font should parse");
        let mut terminal = TerminalSession::new(TerminalLimits::default(), 4, 1);
        terminal.process_bytes(b"\x1b[?25lABCD");
        let first = terminal.snapshot();
        let mut cached = font.render(&first, 64, 32);

        terminal.process_bytes(b"\rZ");
        let second = terminal.snapshot();
        let expected = font.render(&second, 64, 32);
        font.render_cells(&second, &mut cached, &[CellRect::new(0, 0, 2, 1)]);

        assert_eq!(cached, expected);
    }

    #[test]
    fn selected_cells_receive_a_visible_highlight() {
        let mut font = TerminalFont::embedded().expect("embedded font should parse");
        let mut terminal = TerminalSession::new(TerminalLimits::default(), 2, 1);
        terminal.process_bytes(b"A");
        let unselected = font.render(&terminal.snapshot(), 64, 32);

        terminal.begin_selection(teamy_terminal_core::CursorPosition::default());
        let selected = font.render(&terminal.snapshot(), 64, 32);

        assert_ne!(selected, unselected);
        assert!(selected
            .pixels
            .chunks_exact(4)
            .any(|pixel| pixel == [70, 100, 170, 255]));
    }

    #[test]
    fn unsupported_glyphs_use_the_visible_question_mark_fallback() {
        let mut font = TerminalFont::embedded().expect("embedded font should parse");
        let unsupported = char::from_u32(0x10_FFFF).expect("Unicode scalar should be valid");
        let missing = font.glyph(unsupported);
        let question_mark = font.glyph('?');
        assert_eq!(missing.metrics, question_mark.metrics);
        assert_eq!(missing.bitmap, question_mark.bitmap);
    }

    #[test]
    fn default_background_is_black_and_gray_is_distinct_from_white() {
        assert_eq!(color(Color::Default, true), [0, 0, 0, 255]);
        assert_eq!(color(Color::Indexed(8), false), [102, 102, 102, 255]);
        assert_eq!(color(Color::Indexed(15), false), [255, 255, 255, 255]);
    }
}
