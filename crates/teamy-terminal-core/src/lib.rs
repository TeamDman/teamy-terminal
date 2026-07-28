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

/// A visible-grid text selection owned by the terminal session.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SelectionRange {
    /// The cell where the selection started.
    pub anchor: CursorPosition,
    /// The cell currently extended by the selection.
    pub focus: CursorPosition,
}

impl SelectionRange {
    /// Return the ordered inclusive endpoints of this selection.
    #[must_use]
    pub const fn ordered(self) -> (CursorPosition, CursorPosition) {
        if self.anchor.row < self.focus.row
            || (self.anchor.row == self.focus.row
                && self.anchor.column <= self.focus.column)
        {
            (self.anchor, self.focus)
        } else {
            (self.focus, self.anchor)
        }
    }

    /// Return whether a visible cell is included in this linear selection.
    #[must_use]
    pub const fn contains(self, position: CursorPosition) -> bool {
        let (start, end) = self.ordered();
        (position.row > start.row
            || (position.row == start.row && position.column >= start.column))
            && (position.row < end.row
                || (position.row == end.row && position.column <= end.column))
    }
}

/// An inclusive-start, exclusive-end range in the visible terminal grid.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TerminalRange {
    /// The first cell in the range.
    pub start: CursorPosition,
    /// The cell immediately after the range.
    pub end: CursorPosition,
}

/// Shell-integration metadata associated with the latest prompt and command.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PromptMetadata {
    /// The latest OSC 133 prompt range, when a shell reported one.
    pub prompt_range: Option<TerminalRange>,
    /// The latest OSC 133 command/input-and-output range, when completed.
    pub command_range: Option<TerminalRange>,
    /// The exit status reported by the latest OSC 133 D marker.
    pub command_status: Option<i32>,
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
    /// The active visible-grid selection, if any.
    pub selection: Option<SelectionRange>,
    /// The latest shell prompt and command markers.
    pub prompt: PromptMetadata,
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

    /// Return whether a visible cell is selected.
    #[must_use]
    pub fn is_selected(&self, column: u16, row: u16) -> bool {
        self.selection.is_some_and(|selection| {
            selection.contains(CursorPosition { column, row })
        })
    }

    /// Return the visible grid as newline-separated text without row padding.
    #[must_use]
    pub fn visible_text(&self) -> String {
        let mut rows = Vec::with_capacity(usize::from(self.rows));
        for row in 0..self.rows {
            let start = usize::from(row) * usize::from(self.columns);
            let end = start + usize::from(self.columns);
            let mut text = self.cells[start..end]
                .iter()
                .map(|cell| cell.character)
                .collect::<String>();
            while text.ends_with(' ') {
                let _ = text.pop();
            }
            rows.push(text);
        }
        rows.join("\n")
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

#[derive(Clone, Debug)]
struct TerminalRow {
    cells: Vec<Cell>,
    /// Whether this row is a continuation of the logical row above it.
    wrapped: bool,
}

#[derive(Clone, Debug)]
struct SavedScreen {
    columns: u16,
    rows: u16,
    cursor: CursorPosition,
    saved_cursor: CursorPosition,
    cursor_visible: bool,
    wrap_pending: bool,
    style: Style,
    cells: Vec<Cell>,
    row_wrapped: Vec<bool>,
    scrollback: VecDeque<TerminalRow>,
}

#[derive(Clone, Copy, Debug)]
struct SavedCursor {
    cursor: CursorPosition,
    cursor_visible: bool,
    style: Style,
}

#[derive(Clone, Copy, Debug)]
struct ReflowUnit {
    cell: Cell,
    width: usize,
    continuation: Option<Cell>,
}

#[derive(Clone, Debug, Default)]
struct LogicalLine {
    units: Vec<ReflowUnit>,
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
    row_wrapped: Vec<bool>,
    scrollback: VecDeque<TerminalRow>,
    selection: Option<SelectionRange>,
    prompt: PromptMetadata,
    prompt_start: Option<CursorPosition>,
    command_start: Option<CursorPosition>,
    responses: VecDeque<Vec<u8>>,
    win32_input_mode: bool,
    mouse_tracking_mode: u8,
    sgr_mouse_mode: bool,
    alternate_screen: Option<SavedScreen>,
    saved_private_cursor: Option<SavedCursor>,
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
            row_wrapped: vec![false; usize::from(rows)],
            scrollback: VecDeque::new(),
            selection: None,
            prompt: PromptMetadata::default(),
            prompt_start: None,
            command_start: None,
            responses: VecDeque::new(),
            win32_input_mode: false,
            mouse_tracking_mode: 0,
            sgr_mouse_mode: false,
            alternate_screen: None,
            saved_private_cursor: None,
        }
    }

    fn resize(&mut self, columns: u16, rows: u16) {
        let columns = columns.clamp(1, self.limits.max_columns.max(1));
        let rows = rows.clamp(1, self.limits.max_rows.max(1));
        if columns == self.columns && rows == self.rows {
            return;
        }
        self.selection = None;
        self.clear_prompt_metadata();

        let old_columns = self.columns;
        let old_rows = self.rows;
        let old_scrollback_len = self.scrollback.len();
        let source_cursor = (
            old_scrollback_len + usize::from(self.cursor.row),
            usize::from(self.cursor.column),
        );
        let mut source_rows = self.scrollback.iter().cloned().collect::<Vec<_>>();
        source_rows.extend((0..usize::from(old_rows)).map(|row| {
            TerminalRow {
                cells: self.cells
                    [row * usize::from(old_columns)..(row + 1) * usize::from(old_columns)]
                    .to_vec(),
                wrapped: self.row_wrapped[row],
            }
        }));

        let (mut reflowed, cursor) = reflow_rows(&source_rows, columns, source_cursor);
        let target_rows = usize::from(rows);
        while reflowed.len() < target_rows {
            reflowed.push(TerminalRow {
                cells: Vec::new(),
                wrapped: false,
            });
        }
        let maximum_rows = self.limits.max_scrollback_rows.saturating_add(target_rows);
        let dropped_rows = reflowed.len().saturating_sub(maximum_rows);
        if dropped_rows > 0 {
            reflowed.drain(..dropped_rows);
        }

        self.columns = columns;
        self.rows = rows;
        self.scrollback.clear();
        let cursor_global_row = cursor.map_or_else(
            || reflowed.len().saturating_sub(1),
            |(row, _)| row.saturating_sub(dropped_rows),
        );
        let visible_start = cursor_global_row
            .saturating_sub(target_rows.saturating_sub(1))
            .min(reflowed.len().saturating_sub(target_rows));
        self.scrollback
            .extend(reflowed[..visible_start].iter().cloned());
        self.cells = vec![Cell::default(); target_rows * usize::from(columns)];
        self.row_wrapped = vec![false; target_rows];
        for (row, source) in reflowed[visible_start..visible_start + target_rows]
            .iter()
            .enumerate()
        {
            let destination =
                &mut self.cells[row * usize::from(columns)..(row + 1) * usize::from(columns)];
            destination[..source.cells.len()].copy_from_slice(&source.cells);
            self.row_wrapped[row] = source.wrapped;
        }

        let cursor_row = cursor
            .map_or_else(
                || target_rows.saturating_sub(1),
                |(row, _)| {
                    row.saturating_sub(dropped_rows)
                        .saturating_sub(visible_start)
                },
            )
            .min(target_rows.saturating_sub(1));
        let cursor_column = cursor
            .map_or(0, |(_, column)| column)
            .min(usize::from(columns).saturating_sub(1));
        self.cursor = CursorPosition {
            column: u16::try_from(cursor_column).expect("resized cursor column fits in u16"),
            row: u16::try_from(cursor_row).expect("resized cursor row fits in u16"),
        };
        self.wrap_pending = false;
    }

    fn clear(&mut self) {
        self.cells.fill(Cell::default());
        self.cursor = CursorPosition::default();
        self.saved_cursor = CursorPosition::default();
        self.style = Style::default();
        self.wrap_pending = false;
        self.row_wrapped.fill(false);
        self.scrollback.clear();
        self.selection = None;
        self.clear_prompt_metadata();
        self.saved_private_cursor = None;
        self.win32_input_mode = false;
        self.mouse_tracking_mode = 0;
        self.sgr_mouse_mode = false;
    }

    fn save_private_cursor(&mut self) {
        self.saved_private_cursor = Some(SavedCursor {
            cursor: self.cursor,
            cursor_visible: self.cursor_visible,
            style: self.style,
        });
    }

    fn restore_private_cursor(&mut self) {
        let Some(saved_cursor) = self.saved_private_cursor.take() else {
            return;
        };
        self.cursor = CursorPosition {
            column: saved_cursor.cursor.column.min(self.columns - 1),
            row: saved_cursor.cursor.row.min(self.rows - 1),
        };
        self.cursor_visible = saved_cursor.cursor_visible;
        self.style = saved_cursor.style;
        self.wrap_pending = false;
    }

    fn enter_alternate_screen(&mut self) {
        if self.alternate_screen.is_some() {
            return;
        }

        self.alternate_screen = Some(SavedScreen {
            columns: self.columns,
            rows: self.rows,
            cursor: self.cursor,
            saved_cursor: self.saved_cursor,
            cursor_visible: self.cursor_visible,
            wrap_pending: self.wrap_pending,
            style: self.style,
            cells: self.cells.clone(),
            row_wrapped: self.row_wrapped.clone(),
            scrollback: self.scrollback.clone(),
        });
        self.selection = None;
        self.clear_prompt_metadata();
        self.cells = vec![Cell::default(); usize::from(self.columns) * usize::from(self.rows)];
        self.row_wrapped.fill(false);
        self.scrollback.clear();
        self.cursor = CursorPosition::default();
        self.cursor_visible = true;
        self.style = Style::default();
        self.wrap_pending = false;
    }

    fn exit_alternate_screen(&mut self) {
        let Some(saved_screen) = self.alternate_screen.take() else {
            return;
        };
        let columns = self.columns;
        let rows = self.rows;
        self.selection = None;
        self.clear_prompt_metadata();

        self.columns = saved_screen.columns;
        self.rows = saved_screen.rows;
        self.cursor = saved_screen.cursor;
        self.saved_cursor = saved_screen.saved_cursor;
        self.cursor_visible = saved_screen.cursor_visible;
        self.wrap_pending = saved_screen.wrap_pending;
        self.style = saved_screen.style;
        self.cells = saved_screen.cells;
        self.row_wrapped = saved_screen.row_wrapped;
        self.scrollback = saved_screen.scrollback;

        if self.columns != columns || self.rows != rows {
            self.resize(columns, rows);
        }
    }

    fn clamp_position(&self, position: CursorPosition) -> CursorPosition {
        CursorPosition {
            column: position.column.min(self.columns.saturating_sub(1)),
            row: position.row.min(self.rows.saturating_sub(1)),
        }
    }

    fn selected_text(&self) -> Option<String> {
        let selection = self.selection?;
        let (start, end) = selection.ordered();
        let mut text = String::new();
        for row in start.row..=end.row {
            let first_column = if row == start.row { start.column } else { 0 };
            let last_column = if row == end.row {
                end.column
            } else {
                self.columns.saturating_sub(1)
            };
            let row_start = usize::from(row) * usize::from(self.columns);
            let row_end = row_start + usize::from(self.columns);
            let cells = &self.cells[row_start..row_end];
            let mut line = cells[usize::from(first_column)..=usize::from(last_column)]
                .iter()
                .map(|cell| cell.character)
                .collect::<String>();
            while line.ends_with(' ') {
                let _ = line.pop();
            }
            text.push_str(&line);
            if row != end.row {
                text.push('\n');
            }
        }
        Some(text)
    }

    fn clear_prompt_metadata(&mut self) {
        self.prompt = PromptMetadata::default();
        self.prompt_start = None;
        self.command_start = None;
    }

    fn prompt_marker_start(&mut self) {
        self.prompt = PromptMetadata::default();
        self.prompt_start = Some(self.cursor);
        self.command_start = None;
    }

    fn prompt_marker_end(&mut self) {
        if let Some(start) = self.prompt_start.take() {
            self.prompt.prompt_range = Some(TerminalRange {
                start,
                end: self.cursor,
            });
            self.command_start = Some(self.cursor);
        }
    }

    fn command_marker_start(&mut self) {
        self.command_start = Some(self.cursor);
    }

    fn command_marker_end(&mut self, status: Option<i32>) {
        if let Some(start) = self.command_start.take() {
            self.prompt.command_range = Some(TerminalRange {
                start,
                end: self.cursor,
            });
        }
        self.prompt.command_status = status;
    }

    fn cell_index(&self, column: u16, row: u16) -> usize {
        usize::from(row) * usize::from(self.columns) + usize::from(column)
    }

    fn line_feed(&mut self, wrapped: bool) {
        self.wrap_pending = false;
        if self.cursor.row + 1 >= self.rows {
            self.clear_prompt_metadata();
            let line = TerminalRow {
                cells: self.cells[..usize::from(self.columns)].to_vec(),
                wrapped: self.row_wrapped[0],
            };
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
            self.row_wrapped.copy_within(1.., 0);
            let last_row = self.row_wrapped.len() - 1;
            self.row_wrapped[last_row] = wrapped;
        } else {
            self.cursor.row += 1;
            self.row_wrapped[usize::from(self.cursor.row)] = wrapped;
        }
    }

    fn reverse_index(&mut self) {
        if self.cursor.row == 0 {
            let columns = usize::from(self.columns);
            let mut line = vec![Cell::default(); columns];
            line.copy_from_slice(&self.cells[self.cells.len() - columns..]);
            let last_wrapped = *self.row_wrapped.last().unwrap_or(&false);
            let before_last_row = self.cells.len() - columns;
            self.cells.copy_within(..before_last_row, columns);
            self.cells[..columns].copy_from_slice(&line);
            let row_count = self.row_wrapped.len();
            self.row_wrapped.copy_within(..row_count - 1, 1);
            self.row_wrapped[0] = last_wrapped;
        } else {
            self.cursor.row -= 1;
        }
    }

    fn put_char(&mut self, character: char) {
        if self.wrap_pending {
            self.cursor.column = 0;
            self.line_feed(true);
        }
        let width = u16::try_from(character.width().unwrap_or(1).clamp(1, 2))
            .expect("clamped Unicode width fits in u16");
        if width == 2 && self.cursor.column + 1 >= self.columns {
            self.cursor.column = 0;
            self.line_feed(true);
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
                self.row_wrapped.fill(false);
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
            2 => {
                self.cells[row_start..row_end].fill(Cell::default());
                self.row_wrapped[usize::from(self.cursor.row)] = false;
            }
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

    fn osc133(&mut self, marker: &[u8], status: Option<i32>) {
        match marker {
            b"A" => self.prompt_marker_start(),
            b"B" => self.prompt_marker_end(),
            b"C" => self.command_marker_start(),
            b"D" => self.command_marker_end(status),
            _ => {}
        }
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
            b'\n' | 0x0b | 0x0c => self.state.line_feed(false),
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
                    let enabled = action == 'h';
                    match parameter {
                        25 => self.state.cursor_visible = enabled,
                        1047 => {
                            if enabled {
                                self.state.enter_alternate_screen();
                            } else {
                                self.state.exit_alternate_screen();
                            }
                        }
                        1048 => {
                            if enabled {
                                self.state.save_private_cursor();
                            } else {
                                self.state.restore_private_cursor();
                            }
                        }
                        1049 => {
                            if enabled {
                                self.state.save_private_cursor();
                                self.state.enter_alternate_screen();
                            } else {
                                self.state.exit_alternate_screen();
                                self.state.restore_private_cursor();
                            }
                        }
                        9001 => self.state.win32_input_mode = enabled,
                        1000 => self.state.mouse_tracking_mode = if enabled { 1 } else { 0 },
                        1002 => self.state.mouse_tracking_mode = if enabled { 2 } else { 0 },
                        1003 => self.state.mouse_tracking_mode = if enabled { 3 } else { 0 },
                        1006 => self.state.sgr_mouse_mode = enabled,
                        _ => {}
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

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        if params.first().copied() != Some(b"133") {
            return;
        }
        let Some(marker) = params.get(1).copied() else {
            return;
        };
        let status = params
            .get(2)
            .and_then(|value| std::str::from_utf8(value).ok())
            .and_then(|value| value.parse::<i32>().ok());
        self.state.osc133(marker, status);
    }

    fn esc_dispatch(&mut self, _intermediates: &[u8], _ignore: bool, byte: u8) {
        match byte {
            b'7' => self.state.saved_cursor = self.state.cursor,
            b'8' => self
                .state
                .move_cursor(self.state.saved_cursor.column, self.state.saved_cursor.row),
            b'c' => self.state.clear(),
            b'D' | b'E' => {
                self.state.line_feed(false);
                if byte == b'E' {
                    self.state.cursor.column = 0;
                }
            }
            b'M' => self.state.reverse_index(),
            _ => {}
        }
    }
}

fn reflow_rows(
    source_rows: &[TerminalRow],
    new_columns: u16,
    source_cursor: (usize, usize),
) -> (Vec<TerminalRow>, Option<(usize, usize)>) {
    let columns = usize::from(new_columns.max(1));
    let mut logical_lines = Vec::<LogicalLine>::new();
    let mut cursor_anchor = None;

    for (row_index, row) in source_rows.iter().enumerate() {
        if !row.wrapped || logical_lines.is_empty() {
            logical_lines.push(LogicalLine::default());
        }
        let line_index = logical_lines.len() - 1;
        let line = logical_lines
            .last_mut()
            .expect("a logical line was just created");
        let line_width = line.units.iter().map(|unit| unit.width).sum::<usize>();
        let last_content = if row.wrapped {
            row.cells.len()
        } else {
            row.cells
                .iter()
                .rposition(|cell| *cell != Cell::default())
                .map_or(0, |index| index + 1)
        };
        let mut row_width = 0;

        for column in 0..last_content {
            let cell = row.cells[column];
            if column > 0 && is_wide_continuation(row.cells[column - 1], cell) {
                continue;
            }
            let width = cell.character.width().unwrap_or(1).clamp(1, 2);
            if source_cursor.0 == row_index && source_cursor.1 == column {
                cursor_anchor = Some((line_index, line_width + row_width));
            }
            let continuation = if width == 2 {
                row.cells.get(column + 1).copied()
            } else {
                None
            };
            line.units.push(ReflowUnit {
                cell,
                width,
                continuation,
            });
            row_width += width;
            if source_cursor.0 == row_index && source_cursor.1 == column + 1 && width == 2 {
                cursor_anchor = Some((line_index, line_width + row_width));
            }
        }

        if source_cursor.0 == row_index && cursor_anchor.is_none() {
            cursor_anchor = Some((line_index, line_width + row_width));
        }
    }

    let mut output = Vec::new();
    let mut destination_cursor = None;
    for (line_index, line) in logical_lines.iter().enumerate() {
        let line_width = line.units.iter().map(|unit| unit.width).sum::<usize>();
        let output_start = output.len();
        let mut output_cells = Vec::new();
        let mut output_row = 0;
        for unit in &line.units {
            let unit_width = unit.width.min(columns);
            if !output_cells.is_empty() && output_cells.len() + unit_width > columns {
                output.push(TerminalRow {
                    cells: std::mem::take(&mut output_cells),
                    wrapped: output_row > 0,
                });
                output_row += 1;
            }
            output_cells.push(unit.cell);
            if unit_width == 2 {
                let continuation = unit.continuation.unwrap_or_else(|| {
                    let mut continuation = unit.cell;
                    continuation.character = ' ';
                    continuation
                });
                output_cells.push(continuation);
            }
        }
        output.push(TerminalRow {
            cells: std::mem::take(&mut output_cells),
            wrapped: output_row > 0,
        });

        if let Some((cursor_line, cursor_offset)) = cursor_anchor
            && cursor_line == line_index
        {
            let offset = cursor_offset.min(line_width);
            let (row_offset, column) =
                if line_width > 0 && offset == line_width && offset % columns == 0 {
                    (output_row, columns - 1)
                } else {
                    (offset / columns, offset % columns)
                };
            destination_cursor = Some((output_start + row_offset, column));
        }
    }

    (output, destination_cursor)
}

fn is_wide_continuation(previous: Cell, current: Cell) -> bool {
    previous.character.width().is_some_and(|width| width >= 2)
        && current.character == ' '
        && current.foreground == previous.foreground
        && current.background == previous.background
        && current.bold == previous.bold
        && current.underline == previous.underline
        && current.inverse == previous.inverse
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

    /// Return whether the shell requested Windows `KEY_EVENT_RECORD` input.
    #[must_use]
    pub const fn win32_input_mode_enabled(&self) -> bool {
        self.state.win32_input_mode
    }

    /// Return whether the child requested terminal mouse reports.
    #[must_use]
    pub const fn mouse_reporting_enabled(&self) -> bool {
        self.state.mouse_tracking_mode != 0
    }

    /// Return whether mouse reports should use SGR (1006) encoding.
    #[must_use]
    pub const fn sgr_mouse_mode_enabled(&self) -> bool {
        self.state.sgr_mouse_mode
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

    /// Begin a visible-grid selection at a bounded cell position.
    pub fn begin_selection(&mut self, anchor: CursorPosition) {
        let anchor = self.state.clamp_position(anchor);
        self.state.selection = Some(SelectionRange {
            anchor,
            focus: anchor,
        });
    }

    /// Extend the active selection to a bounded cell position.
    pub fn update_selection(&mut self, focus: CursorPosition) {
        let focus = self.state.clamp_position(focus);
        let Some(selection) = self.state.selection.as_mut() else {
            return;
        };
        selection.focus = focus;
    }

    /// Clear any active selection.
    pub fn clear_selection(&mut self) {
        self.state.selection = None;
    }

    /// Return the current visible-grid selection.
    #[must_use]
    pub const fn selection(&self) -> Option<SelectionRange> {
        self.state.selection
    }

    /// Return selected visible text with trailing row padding removed.
    #[must_use]
    pub fn selected_text(&self) -> Option<String> {
        self.state.selected_text()
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
            selection: self.state.selection,
            prompt: self.state.prompt,
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
            TerminalRow {
                cells: vec![Cell::default(); usize::from(self.state.columns)],
                wrapped: false,
            },
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
    fn tracks_conpty_win32_input_mode_requests() {
        let mut terminal = session();
        assert!(!terminal.win32_input_mode_enabled());

        terminal.process_bytes(b"\x1b[?9001h");
        assert!(terminal.win32_input_mode_enabled());

        terminal.process_bytes(b"\x1b[?9001l");
        assert!(!terminal.win32_input_mode_enabled());
    }

    #[test]
    fn tracks_xterm_mouse_reporting_requests() {
        let mut terminal = session();
        assert!(!terminal.mouse_reporting_enabled());
        assert!(!terminal.sgr_mouse_mode_enabled());

        terminal.process_bytes(b"\x1b[?1002;1006h");
        assert!(terminal.mouse_reporting_enabled());
        assert!(terminal.sgr_mouse_mode_enabled());

        terminal.process_bytes(b"\x1b[?1002;1006l");
        assert!(!terminal.mouse_reporting_enabled());
        assert!(!terminal.sgr_mouse_mode_enabled());
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

    #[test]
    fn resize_reflows_and_restores_content_hidden_by_a_narrow_viewport() {
        let mut terminal = TerminalSession::new(
            TerminalLimits {
                max_columns: 20,
                max_rows: 10,
                max_scrollback_rows: 128,
            },
            10,
            4,
        );
        terminal.process_bytes(b"abcdefghij");

        terminal.resize(5, 4);
        let narrow = terminal.snapshot();
        assert_eq!(narrow.cell(0, 0).map(|cell| cell.character), Some('a'));
        assert_eq!(narrow.cell(0, 1).map(|cell| cell.character), Some('f'));

        terminal.resize(10, 4);
        let wide = terminal.snapshot();
        let text: String = (0..10)
            .filter_map(|column| wide.cell(column, 0).map(|cell| cell.character))
            .collect();
        assert_eq!(text, "abcdefghij");
    }

    #[test]
    fn visible_selection_clamps_and_extracts_text_without_padding() {
        let mut terminal = TerminalSession::new(
            TerminalLimits {
                max_columns: 20,
                max_rows: 4,
                max_scrollback_rows: 8,
            },
            8,
            2,
        );
        terminal.process_bytes(b"alpha\r\nbeta");
        assert_eq!(terminal.snapshot().visible_text(), "alpha\nbeta");
        terminal.begin_selection(CursorPosition { column: 1, row: 0 });
        terminal.update_selection(CursorPosition {
            column: 99,
            row: 99,
        });

        let selection = terminal.selection().expect("selection should be active");
        assert_eq!(selection.anchor, CursorPosition { column: 1, row: 0 });
        assert_eq!(selection.focus, CursorPosition { column: 7, row: 1 });
        assert!(selection.contains(CursorPosition { column: 2, row: 0 }));
        assert!(selection.contains(CursorPosition { column: 7, row: 1 }));
        assert!(!selection.contains(CursorPosition { column: 0, row: 0 }));
        assert_eq!(terminal.selected_text().as_deref(), Some("lpha\nbeta"));
        assert_eq!(terminal.snapshot().selection, Some(selection));

        terminal.clear_selection();
        assert_eq!(terminal.selected_text(), None);
        assert_eq!(terminal.snapshot().selection, None);
    }

    #[test]
    fn resize_and_alternate_screen_clear_visible_selection() {
        let mut terminal = session();
        terminal.process_bytes(b"selection");
        terminal.begin_selection(CursorPosition::default());
        terminal.resize(40, 12);
        assert_eq!(terminal.selection(), None);

        terminal.begin_selection(CursorPosition::default());
        terminal.process_bytes(b"\x1b[?1049halt\x1b[?1049l");
        assert_eq!(terminal.selection(), None);
    }

    #[test]
    fn osc133_markers_publish_prompt_command_range_and_status() {
        let mut terminal = session();
        terminal.process_bytes(
            b"\x1b]133;A\x07PS \x1b]133;B\x07Get-Process\x1b]133;C\x07output\x1b]133;D;7\x07",
        );

        assert_eq!(
            terminal.snapshot().prompt,
            PromptMetadata {
                prompt_range: Some(TerminalRange {
                    start: CursorPosition { column: 0, row: 0 },
                    end: CursorPosition { column: 3, row: 0 },
                }),
                command_range: Some(TerminalRange {
                    start: CursorPosition { column: 14, row: 0 },
                    end: CursorPosition { column: 20, row: 0 },
                }),
                command_status: Some(7),
            }
        );
    }

    #[test]
    fn osc133_power_shell_markers_infer_command_start_after_prompt() {
        let mut terminal = session();
        terminal.process_bytes(b"\x1b]133;A\x07PS \x1b]133;B\x07echo ok\x1b]133;D;0\x07");

        assert_eq!(
            terminal.snapshot().prompt.command_range,
            Some(TerminalRange {
                start: CursorPosition { column: 3, row: 0 },
                end: CursorPosition { column: 10, row: 0 },
            })
        );
        assert_eq!(terminal.snapshot().prompt.command_status, Some(0));
    }

    #[test]
    fn alternate_screen_restores_primary_screen_and_scrollback() {
        let mut terminal = TerminalSession::new(
            TerminalLimits {
                max_columns: 20,
                max_rows: 2,
                max_scrollback_rows: 8,
            },
            20,
            2,
        );
        terminal.process_bytes(b"normal\r\nsecond\r\nthird");
        let primary = terminal.snapshot();

        terminal.process_bytes(b"\x1b[?1049halternate\x1b[?25l");
        let alternate = terminal.snapshot();
        assert_eq!(alternate.cell(0, 0).map(|cell| cell.character), Some('a'));
        assert_eq!(alternate.scrollback_rows, 0);
        assert!(!alternate.cursor_visible);

        terminal.process_bytes(b"\x1b[?1049l");
        assert_eq!(terminal.snapshot(), primary);
    }

    #[test]
    fn alternate_screen_resize_restores_primary_at_new_size() {
        let mut terminal = TerminalSession::new(
            TerminalLimits {
                max_columns: 20,
                max_rows: 8,
                max_scrollback_rows: 8,
            },
            10,
            4,
        );
        terminal.process_bytes(b"primary text");
        terminal.process_bytes(b"\x1b[?1049halt");
        terminal.resize(6, 3);
        terminal.process_bytes(b"\x1b[?1049l");

        let snapshot = terminal.snapshot();
        assert_eq!((snapshot.columns, snapshot.rows), (6, 3));
        let text: String = (0..6)
            .filter_map(|column| snapshot.cell(column, 0).map(|cell| cell.character))
            .collect();
        assert_eq!(text, "primar");
    }

    #[test]
    fn alternate_screen_1047_restores_the_primary_cursor() {
        let mut terminal = session();
        terminal.process_bytes(b"primary\x1b[1;4H");
        let primary_cursor = terminal.snapshot().cursor;

        terminal.process_bytes(b"\x1b[?1047halt\x1b[10;10H\x1b[?1047l");
        assert_eq!(terminal.snapshot().cursor, primary_cursor);
    }

    #[test]
    fn private_cursor_mode_restores_cursor_and_style() {
        let mut terminal = session();
        terminal.process_bytes(b"\x1b[3;5H\x1b[31m\x1b[?1048h\x1b[1;1H\x1b[0m\x1b[?1048lX");

        let snapshot = terminal.snapshot();
        assert_eq!(snapshot.cursor, CursorPosition { column: 5, row: 2 });
        assert_eq!(
            snapshot.cell(4, 2).map(|cell| cell.foreground),
            Some(Color::Indexed(1))
        );
    }
}
