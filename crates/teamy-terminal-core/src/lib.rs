//! Renderer-free terminal state, VT parsing, and bounded snapshots.
//!
//! The core owns terminal semantics but has no process, window, clipboard, or
//! graphics dependencies. A PTY adapter feeds bytes into [`TerminalSession`]
//! and a renderer consumes [`TerminalSnapshot`].

use std::collections::VecDeque;
use unicode_width::UnicodeWidthChar;
use vte::Params;
use vte::Parser;
use vte::Perform;

/// Conservative limits used by the terminal session.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TerminalLimits {
    /// Maximum number of visible columns.
    pub max_columns: u16,
    /// Maximum number of visible rows.
    pub max_rows: u16,
    /// Maximum number of retained scrollback rows.
    pub max_scrollback_rows: usize,
}

impl Default for TerminalLimits {
    fn default() -> Self {
        Self {
            max_columns: 240,
            max_rows: 120,
            max_scrollback_rows: 10_000,
        }
    }
}

/// A bounded terminal grid position.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CursorPosition {
    /// Zero-based column.
    pub column: u16,
    /// Zero-based row.
    pub row: u16,
}

/// The colour carried by a terminal cell.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Color {
    /// The terminal's configured default colour.
    #[default]
    Default,
    /// An ANSI 256-colour palette index.
    Indexed(u8),
    /// A true-colour RGB value.
    Rgb(u8, u8, u8),
}

/// The style and contents of one visible cell.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Cell {
    /// The character displayed by the cell.
    pub character: char,
    /// Foreground colour.
    pub foreground: Color,
    /// Background colour.
    pub background: Color,
    /// Whether bold intensity is enabled.
    pub bold: bool,
    /// Whether underline is enabled.
    pub underline: bool,
    /// Whether foreground and background are visually inverted.
    pub inverse: bool,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            character: ' ',
            foreground: Color::Default,
            background: Color::Default,
            bold: false,
            underline: false,
            inverse: false,
        }
    }
}

/// A deterministic copy of the visible terminal state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TerminalSnapshot {
    /// Visible column count.
    pub columns: u16,
    /// Visible row count.
    pub rows: u16,
    /// Cursor position at snapshot time.
    pub cursor: CursorPosition,
    /// Whether the cursor should be painted.
    pub cursor_visible: bool,
    /// Visible cells in row-major order.
    pub cells: Vec<Cell>,
    /// Number of retained scrollback rows.
    pub scrollback_rows: usize,
}

impl TerminalSnapshot {
    /// Get one visible cell by zero-based coordinates.
    #[must_use]
    pub fn cell(&self, column: u16, row: u16) -> Option<Cell> {
        if column >= self.columns || row >= self.rows {
            return None;
        }
        let index = usize::from(row) * usize::from(self.columns) + usize::from(column);
        self.cells.get(index).copied()
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct Style {
    foreground: Color,
    background: Color,
    bold: bool,
    underline: bool,
    inverse: bool,
}

impl Style {
    const fn cell(self, character: char) -> Cell {
        Cell {
            character,
            foreground: self.foreground,
            background: self.background,
            bold: self.bold,
            underline: self.underline,
            inverse: self.inverse,
        }
    }
}

struct TerminalState {
    limits: TerminalLimits,
    columns: u16,
    rows: u16,
    cursor: CursorPosition,
    saved_cursor: CursorPosition,
    cursor_visible: bool,
    wrap_pending: bool,
    style: Style,
    cells: Vec<Cell>,
    scrollback: VecDeque<Vec<Cell>>,
    responses: VecDeque<Vec<u8>>,
}

impl TerminalState {
    fn new(limits: TerminalLimits, columns: u16, rows: u16) -> Self {
        let columns = columns.clamp(1, limits.max_columns.max(1));
        let rows = rows.clamp(1, limits.max_rows.max(1));
        Self {
            limits,
            columns,
            rows,
            cursor: CursorPosition::default(),
            saved_cursor: CursorPosition::default(),
            cursor_visible: true,
            wrap_pending: false,
            style: Style::default(),
            cells: vec![Cell::default(); usize::from(columns) * usize::from(rows)],
            scrollback: VecDeque::new(),
            responses: VecDeque::new(),
        }
    }

    fn resize(&mut self, columns: u16, rows: u16) {
        let columns = columns.clamp(1, self.limits.max_columns.max(1));
        let rows = rows.clamp(1, self.limits.max_rows.max(1));
        if columns == self.columns && rows == self.rows {
            return;
        }

        let old_cells = std::mem::take(&mut self.cells);
        let old_columns = self.columns;
        let old_rows = self.rows;
        self.columns = columns;
        self.rows = rows;
        self.cells = vec![Cell::default(); usize::from(columns) * usize::from(rows)];
        for row in 0..usize::from(rows.min(old_rows)) {
            for column in 0..usize::from(columns.min(old_columns)) {
                let old_index = row * usize::from(old_columns) + column;
                let new_index = row * usize::from(columns) + column;
                self.cells[new_index] = old_cells[old_index];
            }
        }
        self.cursor.column = self.cursor.column.min(columns - 1);
        self.cursor.row = self.cursor.row.min(rows - 1);
        self.wrap_pending = false;
    }

    fn clear(&mut self) {
        self.cells.fill(Cell::default());
        self.cursor = CursorPosition::default();
        self.saved_cursor = CursorPosition::default();
        self.style = Style::default();
        self.wrap_pending = false;
        self.scrollback.clear();
    }

    fn cell_index(&self, column: u16, row: u16) -> usize {
        usize::from(row) * usize::from(self.columns) + usize::from(column)
    }

    fn line_feed(&mut self) {
        self.wrap_pending = false;
        if self.cursor.row + 1 >= self.rows {
            let line = self.cells[..usize::from(self.columns)].to_vec();
            if self.limits.max_scrollback_rows > 0 {
                self.scrollback.push_back(line);
                while self.scrollback.len() > self.limits.max_scrollback_rows {
                    let _ = self.scrollback.pop_front();
                }
            }
            let columns = usize::from(self.columns);
            self.cells.copy_within(columns.., 0);
            let last_row = self.cells.len() - columns;
            self.cells[last_row..].fill(Cell::default());
        } else {
            self.cursor.row += 1;
        }
    }

    fn reverse_index(&mut self) {
        if self.cursor.row == 0 {
            let columns = usize::from(self.columns);
            let mut line = vec![Cell::default(); columns];
            line.copy_from_slice(&self.cells[self.cells.len() - columns..]);
            let before_last_row = self.cells.len() - columns;
            self.cells.copy_within(..before_last_row, columns);
            self.cells[..columns].copy_from_slice(&line);
        } else {
            self.cursor.row -= 1;
        }
    }

    fn put_char(&mut self, character: char) {
        if self.wrap_pending {
            self.cursor.column = 0;
            self.line_feed();
        }
        let width = u16::try_from(character.width().unwrap_or(1).clamp(1, 2))
            .expect("clamped Unicode width fits in u16");
        if width == 2 && self.cursor.column + 1 >= self.columns {
            self.cursor.column = 0;
            self.line_feed();
        }
        let index = self.cell_index(self.cursor.column, self.cursor.row);
        self.cells[index] = self.style.cell(character);
        if width == 2 && self.cursor.column + 1 < self.columns {
            self.cells[index + 1] = self.style.cell(' ');
        }
        if self.cursor.column + width >= self.columns {
            self.cursor.column = self.columns - 1;
            self.wrap_pending = true;
        } else {
            self.cursor.column += width;
        }
    }

    fn clear_screen(&mut self, mode: u16) {
        let cursor = self.cell_index(self.cursor.column, self.cursor.row);
        match mode {
            0 => self.cells[cursor..].fill(Cell::default()),
            1 => self.cells[..=cursor].fill(Cell::default()),
            2 | 3 => {
                self.cells.fill(Cell::default());
                if mode == 3 {
                    self.scrollback.clear();
                }
            }
            _ => {}
        }
    }

    fn clear_line(&mut self, mode: u16) {
        let row_start = usize::from(self.cursor.row) * usize::from(self.columns);
        let row_end = row_start + usize::from(self.columns);
        match mode {
            0 => self.cells[usize::from(self.cursor.column) + row_start..row_end]
                .fill(Cell::default()),
            1 => self.cells[row_start..=row_start + usize::from(self.cursor.column)]
                .fill(Cell::default()),
            2 => self.cells[row_start..row_end].fill(Cell::default()),
            _ => {}
        }
    }

    fn move_cursor(&mut self, column: u16, row: u16) {
        self.cursor = CursorPosition {
            column: column.min(self.columns - 1),
            row: row.min(self.rows - 1),
        };
        self.wrap_pending = false;
    }

    fn response(&mut self, bytes: Vec<u8>) {
        self.responses.push_back(bytes);
    }
}

struct Performer<'a> {
    state: &'a mut TerminalState,
}

impl Performer<'_> {
    fn parameter(params: &Params, index: usize, default: u16) -> u16 {
        params
            .iter()
            .nth(index)
            .and_then(|values| values.first().copied())
            .filter(|value| *value != 0)
            .unwrap_or(default)
    }

    fn parameter_or_zero(params: &Params, index: usize) -> u16 {
        params
            .iter()
            .nth(index)
            .and_then(|values| values.first().copied())
            .unwrap_or(0)
    }

    fn indexed_color(index: u16) -> Color {
        Color::Indexed(
            u8::try_from(index.min(u16::from(u8::MAX))).expect("clamped colour index fits in u8"),
        )
    }

    fn apply_sgr(&mut self, params: &Params) {
        let mut values = params
            .iter()
            .flat_map(|values| values.iter().copied())
            .peekable();
        if values.peek().is_none() {
            self.state.style = Style::default();
            return;
        }
        while let Some(value) = values.next() {
            match value {
                0 => self.state.style = Style::default(),
                1 => self.state.style.bold = true,
                22 => self.state.style.bold = false,
                4 => self.state.style.underline = true,
                24 => self.state.style.underline = false,
                7 => self.state.style.inverse = true,
                27 => self.state.style.inverse = false,
                30..=37 => self.state.style.foreground = Self::indexed_color(value - 30),
                39 => self.state.style.foreground = Color::Default,
                40..=47 => self.state.style.background = Self::indexed_color(value - 40),
                49 => self.state.style.background = Color::Default,
                90..=97 => self.state.style.foreground = Self::indexed_color(value - 90 + 8),
                100..=107 => self.state.style.background = Self::indexed_color(value - 100 + 8),
                38 | 48 => {
                    let is_foreground = value == 38;
                    match values.next() {
                        Some(5) => {
                            if let Some(index) = values.next() {
                                let color = Self::indexed_color(index);
                                if is_foreground {
                                    self.state.style.foreground = color;
                                } else {
                                    self.state.style.background = color;
                                }
                            }
                        }
                        Some(2) => {
                            let red = values.next().unwrap_or(0).min(255) as u8;
                            let green = values.next().unwrap_or(0).min(255) as u8;
                            let blue = values.next().unwrap_or(0).min(255) as u8;
                            let color = Color::Rgb(red, green, blue);
                            if is_foreground {
                                self.state.style.foreground = color;
                            } else {
                                self.state.style.background = color;
                            }
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
    }
}

impl Perform for Performer<'_> {
    fn print(&mut self, character: char) {
        self.state.put_char(character);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            b'\n' | 0x0b | 0x0c => self.state.line_feed(),
            b'\r' => {
                self.state.cursor.column = 0;
                self.state.wrap_pending = false;
            }
            0x08 => {
                self.state.cursor.column = self.state.cursor.column.saturating_sub(1);
                self.state.wrap_pending = false;
            }
            b'\t' => {
                let next = ((self.state.cursor.column / 8) + 1) * 8;
                self.state.cursor.column = next.min(self.state.columns - 1);
                self.state.wrap_pending = false;
            }
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], _ignore: bool, action: char) {
        match action {
            'A' => {
                self.state.cursor.row = self
                    .state
                    .cursor
                    .row
                    .saturating_sub(Self::parameter(params, 0, 1));
            }
            'B' | 'e' => {
                self.state.cursor.row = self
                    .state
                    .cursor
                    .row
                    .saturating_add(Self::parameter(params, 0, 1))
                    .min(self.state.rows - 1);
                self.state.wrap_pending = false;
            }
            'C' | 'a' => {
                self.state.cursor.column = self
                    .state
                    .cursor
                    .column
                    .saturating_add(Self::parameter(params, 0, 1))
                    .min(self.state.columns - 1);
                self.state.wrap_pending = false;
            }
            'D' => {
                self.state.cursor.column = self
                    .state
                    .cursor
                    .column
                    .saturating_sub(Self::parameter(params, 0, 1));
                self.state.wrap_pending = false;
            }
            'E' => {
                self.state.cursor.row = self
                    .state
                    .cursor
                    .row
                    .saturating_add(Self::parameter(params, 0, 1))
                    .min(self.state.rows - 1);
                self.state.cursor.column = 0;
                self.state.wrap_pending = false;
            }
            'F' => {
                self.state.cursor.row = self
                    .state
                    .cursor
                    .row
                    .saturating_sub(Self::parameter(params, 0, 1));
                self.state.cursor.column = 0;
                self.state.wrap_pending = false;
            }
            'G' | '`' => {
                self.state.cursor.column = Self::parameter(params, 0, 1)
                    .saturating_sub(1)
                    .min(self.state.columns - 1);
                self.state.wrap_pending = false;
            }
            'd' => {
                self.state.cursor.row = Self::parameter(params, 0, 1)
                    .saturating_sub(1)
                    .min(self.state.rows - 1);
                self.state.wrap_pending = false;
            }
            'H' | 'f' => {
                let row = Self::parameter(params, 0, 1).saturating_sub(1);
                let column = Self::parameter(params, 1, 1).saturating_sub(1);
                self.state.move_cursor(column, row);
            }
            'J' => self.state.clear_screen(Self::parameter_or_zero(params, 0)),
            'K' => self.state.clear_line(Self::parameter_or_zero(params, 0)),
            'm' => Self::apply_sgr(self, params),
            'h' | 'l' if intermediates == [b'?'] => {
                for parameter in params.iter().filter_map(|values| values.first().copied()) {
                    if parameter == 25 {
                        self.state.cursor_visible = action == 'h';
                    }
                }
            }
            'n' if Self::parameter_or_zero(params, 0) == 6 => {
                self.state.response(
                    format!(
                        "\x1b[{};{}R",
                        self.state.cursor.row + 1,
                        self.state.cursor.column + 1
                    )
                    .into_bytes(),
                );
            }
            'c' => self.state.response(b"\x1b[?1;2c".to_vec()),
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, _intermediates: &[u8], _ignore: bool, byte: u8) {
        match byte {
            b'7' => self.state.saved_cursor = self.state.cursor,
            b'8' => self
                .state
                .move_cursor(self.state.saved_cursor.column, self.state.saved_cursor.row),
            b'c' => self.state.clear(),
            b'D' | b'E' => {
                self.state.line_feed();
                if byte == b'E' {
                    self.state.cursor.column = 0;
                }
            }
            b'M' => self.state.reverse_index(),
            _ => {}
        }
    }
}

/// A renderer-free, bounded terminal session.
pub struct TerminalSession {
    state: TerminalState,
    parser: Parser,
}

impl std::fmt::Debug for TerminalSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TerminalSession")
            .field("snapshot", &self.snapshot())
            .finish()
    }
}

impl TerminalSession {
    /// Create a session with dimensions clamped to the supplied limits.
    #[must_use]
    pub fn new(limits: TerminalLimits, columns: u16, rows: u16) -> Self {
        Self {
            state: TerminalState::new(limits, columns, rows),
            parser: Parser::new(),
        }
    }

    /// Return the session's immutable limits.
    #[must_use]
    pub const fn limits(&self) -> TerminalLimits {
        self.state.limits
    }

    /// Return the current visible column count.
    #[must_use]
    pub const fn columns(&self) -> u16 {
        self.state.columns
    }

    /// Return the current visible row count.
    #[must_use]
    pub const fn rows(&self) -> u16 {
        self.state.rows
    }

    /// Feed raw PTY bytes through the VT parser.
    pub fn process_bytes(&mut self, bytes: &[u8]) {
        let mut performer = Performer {
            state: &mut self.state,
        };
        self.parser.advance(&mut performer, bytes);
    }

    /// Resize the visible grid while preserving the overlapping cells.
    pub fn resize(&mut self, columns: u16, rows: u16) {
        self.state.resize(columns, rows);
    }

    /// Return a deterministic copy of the current visible state.
    #[must_use]
    pub fn snapshot(&self) -> TerminalSnapshot {
        TerminalSnapshot {
            columns: self.state.columns,
            rows: self.state.rows,
            cursor: self.state.cursor,
            cursor_visible: self.state.cursor_visible,
            cells: self.state.cells.clone(),
            scrollback_rows: self.state.scrollback.len(),
        }
    }

    /// Drain responses requested by terminal programs, such as cursor reports.
    pub fn drain_responses(&mut self) -> impl Iterator<Item = Vec<u8>> + '_ {
        std::iter::from_fn(|| self.state.responses.pop_front())
    }

    /// Compatibility helper for deterministic fixture tests.
    pub fn set_scrollback_rows(&mut self, rows: usize) {
        self.state.scrollback.clear();
        let count = rows.min(self.state.limits.max_scrollback_rows);
        self.state.scrollback.resize(
            count,
            vec![Cell::default(); usize::from(self.state.columns)],
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> TerminalSession {
        TerminalSession::new(
            TerminalLimits {
                max_columns: 80,
                max_rows: 24,
                max_scrollback_rows: 128,
            },
            80,
            24,
        )
    }

    #[test]
    fn bounded_session_snapshot_is_deterministic() {
        let limits = TerminalLimits {
            max_columns: 80,
            max_rows: 24,
            max_scrollback_rows: 128,
        };
        let mut terminal = TerminalSession::new(limits, 200, 100);
        terminal.set_scrollback_rows(1_000);
        terminal.resize(100, 50);

        let snapshot = terminal.snapshot();
        assert_eq!(snapshot.columns, 80);
        assert_eq!(snapshot.rows, 24);
        assert_eq!(snapshot.cursor, CursorPosition::default());
        assert_eq!(snapshot.scrollback_rows, 128);
    }

    #[test]
    fn parses_text_and_basic_ansi_styles() {
        let mut terminal = session();
        terminal.process_bytes(b"hello\x1b[31mred\x1b[0m!");
        let snapshot = terminal.snapshot();
        assert_eq!(snapshot.cell(0, 0).map(|cell| cell.character), Some('h'));
        assert_eq!(snapshot.cell(5, 0).map(|cell| cell.character), Some('r'));
        assert_eq!(
            snapshot.cell(5, 0).map(|cell| cell.foreground),
            Some(Color::Indexed(1))
        );
        assert_eq!(snapshot.cell(8, 0).map(|cell| cell.character), Some('!'));
    }

    #[test]
    fn scrollback_is_bounded_and_cursor_reports_are_replyable() {
        let mut terminal = TerminalSession::new(
            TerminalLimits {
                max_columns: 20,
                max_rows: 2,
                max_scrollback_rows: 2,
            },
            20,
            2,
        );
        terminal.process_bytes(b"one\ntwo\nthree\n\x1b[6n");
        assert_eq!(terminal.snapshot().scrollback_rows, 2);
        assert_eq!(
            terminal.drain_responses().next(),
            Some(b"\x1b[2;12R".to_vec())
        );
    }

    #[test]
    fn resize_preserves_the_overlapping_grid() {
        let mut terminal = session();
        terminal.process_bytes(b"abc");
        terminal.resize(3, 2);
        let snapshot = terminal.snapshot();
        assert_eq!(snapshot.cell(0, 0).map(|cell| cell.character), Some('a'));
        assert_eq!(snapshot.cell(2, 0).map(|cell| cell.character), Some('c'));
    }
}
