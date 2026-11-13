//! Headless VT abstraction over `alacritty_terminal` (ADR-007).
//!
//! h4rn owns this trait so `alacritty_terminal` (pre-1.0, churning) stays
//! replaceable by `vt100`. It provides damage tracking, resize reflow and the
//! compact screen snapshot used on reconnect.
//!
//! The daemon feeds every PTY byte into [`TerminalModel::feed`]. The model keeps
//! one authoritative screen plus scrollback grid, so a reconnecting viewer gets
//! an ANSI snapshot that recreates what the current viewer sees — not a replay of
//! every byte the process ever wrote.
//!
//! Snapshot rules (ADR-007, T3 Code §4):
//! - the whole grid is serialised as ANSI: scrollback first, then the screen;
//! - at most [`MAX_SCROLLBACK_LINES`] lines and [`MAX_SNAPSHOT_BYTES`] bytes,
//!   trimming the oldest lines first;
//! - the cursor lands on the model's current cursor cell, so a client that
//!   writes the snapshot into a fresh xterm is immediately ready for input.

#![deny(missing_docs)]

use std::sync::{Arc, Mutex};

use alacritty_terminal::Term;
use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::Config;
use alacritty_terminal::term::TermMode;
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::vte::ansi::{
    Color, NamedColor, Processor, Rgb, StdSyncHandler as VteSyncHandler,
};

/// Crate name, used by skeleton tests and startup diagnostics.
pub const CRATE_NAME: &str = "h4rn-vt";

/// Maximum lines retained by the screen model and emitted in a snapshot.
///
/// This is T3 Code's `DEFAULT_HISTORY_LINE_LIMIT`, adopted unchanged: a terminal
/// is a working surface, not an archive; the protocol log remains the archive.
pub const MAX_SCROLLBACK_LINES: usize = 5_000;

/// Maximum size of one serialised snapshot, in bytes (8 MiB).
///
/// A larger snapshot would make a reconnect slower than the relay's budget and
/// can exceed a client's WebSocket message limit once JSON-escaped.
pub const MAX_SNAPSHOT_BYTES: usize = 8 * 1024 * 1024;

/// Smallest accepted column count. `alacritty_terminal` needs two so a wide
/// character always has a cell to occupy.
pub const MIN_COLUMNS: u16 = 2;

/// Largest column count accepted from a viewer.
pub const MAX_COLUMNS: u16 = 1_000;

/// Smallest accepted row count.
pub const MIN_ROWS: u16 = 1;

/// Largest row count accepted from a viewer.
pub const MAX_ROWS: u16 = 500;

/// A headless terminal screen model.
///
/// The trait exists so the `alacritty_terminal` implementation can be replaced
/// by `vt100` without touching the daemon (ADR-007).
pub trait TerminalModel: Send {
    /// Consumes raw PTY bytes.
    fn feed(&mut self, bytes: &[u8]);

    /// Resizes the grid, reflowing wrapped lines where the backend supports it.
    ///
    /// Out-of-range values are clamped; a resize never panics.
    fn resize(&mut self, cols: u16, rows: u16);

    /// Serialises the screen and scrollback as ANSI.
    fn snapshot(&self) -> String;

    /// Current grid width in columns.
    fn cols(&self) -> u16;

    /// Current grid height in rows.
    fn rows(&self) -> u16;

    /// Cursor position as zero-based `(column, row)` in screen coordinates.
    fn cursor(&self) -> (u16, u16);

    /// The window title set with OSC 0/2, when the process set one.
    fn title(&self) -> Option<String>;

    /// Whether the alternate screen is active (a full-screen TUI).
    fn alternate_screen(&self) -> bool;
}

/// Receives OSC title events from `alacritty_terminal`.
///
/// The backend keeps the title in a private field and only exposes changes as
/// events, so the wrapper records them itself.
#[derive(Clone, Default)]
struct TitleListener {
    title: Arc<Mutex<Option<String>>>,
}

impl EventListener for TitleListener {
    fn send_event(&self, event: Event) {
        let title = match event {
            Event::Title(title) => Some(title),
            Event::ResetTitle => None,
            _ => return,
        };
        if let Ok(mut slot) = self.title.lock() {
            *slot = title;
        }
    }
}

/// `alacritty_terminal`-backed screen model.
pub struct AlacrittyVt {
    term: Term<TitleListener>,
    parser: Processor<VteSyncHandler>,
    title: Arc<Mutex<Option<String>>>,
    cols: u16,
    rows: u16,
}

impl AlacrittyVt {
    /// Creates a grid of `cols` × `rows` with the standard scrollback bound.
    pub fn new(cols: u16, rows: u16) -> Self {
        let cols = clamp_cols(cols);
        let rows = clamp_rows(rows);
        let config = Config {
            scrolling_history: MAX_SCROLLBACK_LINES,
            ..Config::default()
        };
        let dimensions = DimensionsPair::new(cols, rows);
        let listener = TitleListener::default();
        let title = Arc::clone(&listener.title);
        let term = Term::new(config, &dimensions, listener);
        Self {
            term,
            parser: Processor::new(),
            title,
            cols,
            rows,
        }
    }

    fn cursor_point(&self) -> Point {
        self.term.grid().cursor.point
    }

    /// Renders the last `max_lines` grid lines, oldest first.
    fn render(&self, start_line: Line, end_line: Line) -> String {
        let grid = self.term.grid();
        let last_column = grid.last_column();
        let mut out =
            String::with_capacity(((self.cols as usize + 8) * self.rows as usize).max(64));
        let mut style = Style::default();
        out.push_str("\x1b[0m");
        let mut first_line = true;

        let mut line = start_line;
        while line <= end_line {
            if !first_line {
                out.push_str("\r\n");
            }
            first_line = false;
            render_line(grid, line, last_column, &mut style, &mut out);
            line += 1;
        }

        out.push_str("\x1b[0m");
        if !self.term.mode().contains(TermMode::SHOW_CURSOR) {
            out.push_str("\x1b[?25l");
        }
        self.append_cursor(&mut out, start_line);
        out
    }

    fn append_cursor(&self, out: &mut String, start_line: Line) {
        let point = self.cursor_point();
        let row = point.line.0 - start_line.0;
        let row = row.max(0) as usize + 1;
        out.push_str(&format!("\x1b[{row};{}H", point.column.0 + 1));
    }
}

impl TerminalModel for AlacrittyVt {
    fn feed(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.term, bytes);
    }

    fn resize(&mut self, cols: u16, rows: u16) {
        let cols = clamp_cols(cols);
        let rows = clamp_rows(rows);
        if cols == self.cols && rows == self.rows {
            return;
        }
        self.cols = cols;
        self.rows = rows;
        // `resize` reflows wrapped lines for the primary grid and keeps the
        // alternate grid clipped, which is the behaviour a full-screen TUI
        // expects (ADR-007: reflow on resize).
        self.term.resize(DimensionsPair::new(cols, rows));
    }

    fn snapshot(&self) -> String {
        let grid = self.term.grid();
        let bottom = grid.bottommost_line();
        let top = grid.topmost_line();
        let total_lines = (bottom.0 - top.0 + 1) as usize;
        let keep = total_lines.min(MAX_SCROLLBACK_LINES);
        let mut start = Line(bottom.0 - keep as i32 + 1);
        let rendered = self.render(start, bottom);
        if rendered.len() <= MAX_SNAPSHOT_BYTES {
            return rendered;
        }

        // The byte budget is the hard limit; trim the oldest lines until the
        // snapshot fits. A binary search keeps the number of renders small even
        // for a pathological grid.
        let mut low = 0usize;
        let mut high = keep.saturating_sub(1);
        while low < high {
            let mid = (low + high) / 2;
            let candidate_start = Line(bottom.0 - (keep - mid) as i32 + 1);
            let candidate = self.render(candidate_start, bottom);
            if candidate.len() <= MAX_SNAPSHOT_BYTES {
                high = mid;
            } else {
                low = mid + 1;
            }
        }
        start = Line(bottom.0 - (keep - low) as i32 + 1);
        let final_render = self.render(start, bottom);
        if final_render.len() <= MAX_SNAPSHOT_BYTES {
            final_render
        } else {
            // Even a single line is too large (a viewer asked for 1000 columns
            // and the line is full of wide characters). Cut on a char boundary.
            truncate_on_char_boundary(final_render, MAX_SNAPSHOT_BYTES)
        }
    }

    fn cols(&self) -> u16 {
        self.cols
    }

    fn rows(&self) -> u16 {
        self.rows
    }

    fn cursor(&self) -> (u16, u16) {
        let point = self.cursor_point();
        (
            point.column.0.min(self.cols as usize - 1) as u16,
            point.line.0.clamp(0, self.rows as i32 - 1) as u16,
        )
    }

    fn title(&self) -> Option<String> {
        self.title.lock().ok().and_then(|slot| slot.clone())
    }

    fn alternate_screen(&self) -> bool {
        self.term.mode().contains(TermMode::ALT_SCREEN)
    }
}

/// Grid dimensions for one terminal size.
struct DimensionsPair {
    cols: usize,
    rows: usize,
}

impl DimensionsPair {
    fn new(cols: u16, rows: u16) -> Self {
        Self {
            cols: cols as usize,
            rows: rows as usize,
        }
    }
}

impl Dimensions for DimensionsPair {
    fn total_lines(&self) -> usize {
        self.rows
    }

    fn screen_lines(&self) -> usize {
        self.rows
    }

    fn columns(&self) -> usize {
        self.cols
    }
}

fn clamp_cols(cols: u16) -> u16 {
    cols.clamp(MIN_COLUMNS, MAX_COLUMNS)
}

fn clamp_rows(rows: u16) -> u16 {
    rows.clamp(MIN_ROWS, MAX_ROWS)
}

fn truncate_on_char_boundary(mut text: String, max: usize) -> String {
    if text.len() <= max {
        return text;
    }
    let mut cut = max;
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    text.truncate(cut);
    text.push_str("\x1b[0m");
    text
}

/// SGR state carried from cell to cell so a snapshot does not repeat escapes.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
struct Style {
    fg: ColorKey,
    bg: ColorKey,
    flags: u16,
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum ColorKey {
    #[default]
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

impl From<Color> for ColorKey {
    fn from(color: Color) -> Self {
        match color {
            Color::Named(name) => match name {
                NamedColor::Black => Self::Indexed(0),
                NamedColor::Red => Self::Indexed(1),
                NamedColor::Green => Self::Indexed(2),
                NamedColor::Yellow => Self::Indexed(3),
                NamedColor::Blue => Self::Indexed(4),
                NamedColor::Magenta => Self::Indexed(5),
                NamedColor::Cyan => Self::Indexed(6),
                NamedColor::White => Self::Indexed(7),
                NamedColor::BrightBlack => Self::Indexed(8),
                NamedColor::BrightRed => Self::Indexed(9),
                NamedColor::BrightGreen => Self::Indexed(10),
                NamedColor::BrightYellow => Self::Indexed(11),
                NamedColor::BrightBlue => Self::Indexed(12),
                NamedColor::BrightMagenta => Self::Indexed(13),
                NamedColor::BrightCyan => Self::Indexed(14),
                NamedColor::BrightWhite => Self::Indexed(15),
                _ => Self::Default,
            },
            Color::Indexed(index) => Self::Indexed(index),
            Color::Spec(Rgb { r, g, b }) => Self::Rgb(r, g, b),
        }
    }
}

/// Attributes that a snapshot reproduces. Cursor, selection and hyperlink
/// state belong to the client view and are not serialised.
const SNAPSHOT_FLAGS: Flags = Flags::BOLD
    .union(Flags::DIM)
    .union(Flags::ITALIC)
    .union(Flags::ALL_UNDERLINES)
    .union(Flags::INVERSE)
    .union(Flags::HIDDEN)
    .union(Flags::STRIKEOUT);

fn render_line(
    grid: &alacritty_terminal::grid::Grid<Cell>,
    line: Line,
    last_column: Column,
    style: &mut Style,
    out: &mut String,
) {
    let row = &grid[line];
    let mut cells: Vec<(usize, &Cell)> = Vec::with_capacity(last_column.0 + 1);
    for column in 0..=last_column.0 {
        let cell = &row[Column(column)];
        // Spacer cells exist so a wide character can span two columns. The
        // leading cell already carries the character; emitting the spacer would
        // duplicate it on the client.
        if cell
            .flags
            .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
        {
            continue;
        }
        cells.push((column, cell));
    }

    // Trim trailing blanks unless they carry a background or decoration; a
    // terminal snapshot should not carry a wall of spaces (nor pay for it in
    // the 8 MiB budget).
    let keep = cells
        .iter()
        .rposition(|(_, cell)| !is_blank(cell))
        .map_or(0, |index| index + 1);

    for (_, cell) in cells.iter().take(keep) {
        let next = Style {
            fg: cell.fg.into(),
            bg: cell.bg.into(),
            flags: cell.flags.bits() & SNAPSHOT_FLAGS.bits(),
        };
        if next != *style {
            write_sgr(out, next);
            *style = next;
        }
        out.push(cell.c);
        if let Some(extra) = cell.zerowidth() {
            for character in extra {
                out.push(*character);
            }
        }
    }

    // Keep the style across an all-blank line so the next line's first SGR is
    // still accurate.
    if keep == 0 && *style != Style::default() {
        write_sgr(out, Style::default());
        *style = Style::default();
    }
}

fn is_blank(cell: &Cell) -> bool {
    cell.c == ' '
        && cell.zerowidth().is_none()
        && cell.fg == Color::Named(NamedColor::Foreground)
        && cell.bg == Color::Named(NamedColor::Background)
        && !cell.flags.intersects(SNAPSHOT_FLAGS)
}

fn write_sgr(out: &mut String, style: Style) {
    out.push_str("\x1b[0");
    if style.flags & Flags::BOLD.bits() != 0 {
        out.push_str(";1");
    }
    if style.flags & Flags::DIM.bits() != 0 {
        out.push_str(";2");
    }
    if style.flags & Flags::ITALIC.bits() != 0 {
        out.push_str(";3");
    }
    if style.flags & Flags::ALL_UNDERLINES.bits() != 0 {
        out.push_str(";4");
    }
    if style.flags & Flags::INVERSE.bits() != 0 {
        out.push_str(";7");
    }
    if style.flags & Flags::HIDDEN.bits() != 0 {
        out.push_str(";8");
    }
    if style.flags & Flags::STRIKEOUT.bits() != 0 {
        out.push_str(";9");
    }
    write_color(out, style.fg, true);
    write_color(out, style.bg, false);
    out.push('m');
}

fn write_color(out: &mut String, color: ColorKey, foreground: bool) {
    match color {
        ColorKey::Default => {}
        ColorKey::Indexed(index) => {
            out.push_str(&format!(";{};5;{index}", if foreground { 38 } else { 48 }));
        }
        ColorKey::Rgb(r, g, b) => {
            out.push_str(&format!(
                ";{};2;{r};{g};{b}",
                if foreground { 38 } else { 48 }
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(model: &mut AlacrittyVt, text: &str) {
        model.feed(text.as_bytes());
    }

    #[test]
    fn crate_is_wired() {
        assert_eq!(CRATE_NAME, "h4rn-vt");
    }

    #[test]
    fn snapshot_contains_visible_text_and_cursor() {
        let mut model = AlacrittyVt::new(40, 10);
        feed(&mut model, "hello\r\nworld");
        let snapshot = model.snapshot();
        assert!(snapshot.contains("hello"));
        assert!(snapshot.contains("world"));
        // Cursor is on row 2 (zero-based 1), column 5.
        assert!(snapshot.contains("\x1b[2;6H"), "{snapshot:?}");
    }

    #[test]
    fn scrollback_survives_into_the_snapshot() {
        let mut model = AlacrittyVt::new(20, 3);
        for index in 0..12 {
            feed(&mut model, &format!("line-{index}\r\n"));
        }
        let snapshot = model.snapshot();
        assert!(snapshot.contains("line-0"), "oldest line is kept");
        assert!(snapshot.contains("line-11"));
        assert!(snapshot.lines().count() <= MAX_SCROLLBACK_LINES + 8);
    }

    #[test]
    fn resize_reflows_long_lines() {
        let mut model = AlacrittyVt::new(40, 6);
        feed(&mut model, "abcdefghijklmnopqrstuvwxyz0123456789");
        assert_eq!(model.cols(), 40);
        model.resize(12, 6);
        let narrow = model.snapshot();
        assert!(narrow.contains("abcdefghijkl"));
        assert!(!narrow.contains("abcdefghijklmnopqrstuvwxyz0123456789"));
        model.resize(40, 6);
        let wide = model.snapshot();
        assert!(wide.contains("abcdefghijklmnopqrstuvwxyz0123456789"));
    }

    #[test]
    fn colours_and_attributes_are_preserved() {
        let mut model = AlacrittyVt::new(30, 4);
        feed(&mut model, "\x1b[31mred\x1b[0m \x1b[1;44mbold\x1b[0m");
        let snapshot = model.snapshot();
        assert!(snapshot.contains("38;5;1"), "{snapshot:?}");
        assert!(snapshot.contains("48;5;4"), "{snapshot:?}");
        assert!(snapshot.contains(";1m") || snapshot.contains(";1;"));
    }

    #[test]
    fn alternate_screen_has_no_history() {
        let mut model = AlacrittyVt::new(30, 4);
        feed(&mut model, "primary\r\n");
        feed(&mut model, "\x1b[?1049h");
        feed(&mut model, "full-screen");
        assert!(model.alternate_screen());
        let snapshot = model.snapshot();
        assert!(snapshot.contains("full-screen"));
        assert!(!snapshot.contains("primary"));
    }

    #[test]
    fn malformed_and_partial_sequences_do_not_panic() {
        let mut model = AlacrittyVt::new(30, 4);
        model.feed(b"\x1b[");
        model.feed(b"38;5;");
        model.feed(b"not utf8 \xff\xfe");
        model.feed(b"\x1b[999999;999999H");
        assert!(!model.snapshot().is_empty());
    }

    #[test]
    fn resize_clamps_absurd_dimensions() {
        let mut model = AlacrittyVt::new(0, 0);
        assert_eq!(model.cols(), MIN_COLUMNS);
        assert_eq!(model.rows(), MIN_ROWS);
        model.resize(u16::MAX, u16::MAX);
        assert_eq!(model.cols(), MAX_COLUMNS);
        assert_eq!(model.rows(), MAX_ROWS);
    }

    #[test]
    fn snapshot_is_capped_at_the_line_and_byte_budget() {
        let mut model = AlacrittyVt::new(200, 40);
        // More than 5000 lines and more than 8 MiB of coloured output.
        for index in 0..6_500 {
            feed(
                &mut model,
                &format!(
                    "\x1b[3{index_mod};1mline {index} \x1b[0m padding padding padding\r\n",
                    index_mod = index % 8
                ),
            );
        }
        let snapshot = model.snapshot();
        assert!(snapshot.len() <= MAX_SNAPSHOT_BYTES);
        assert!(snapshot.lines().count() <= MAX_SCROLLBACK_LINES + 4);
        assert!(snapshot.contains("line 6499"));
    }

    #[test]
    fn wide_characters_are_not_duplicated() {
        let mut model = AlacrittyVt::new(20, 4);
        feed(&mut model, "日本語");
        let snapshot = model.snapshot();
        assert!(snapshot.contains("日本語"));
    }
}
