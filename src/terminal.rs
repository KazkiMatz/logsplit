use std::cmp::max;

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::{Cell as AlacrittyCell, Flags};
use alacritty_terminal::term::{Config as TermConfig, Term};
use alacritty_terminal::vte::ansi::{Color, NamedColor, Processor};
use unicode_width::UnicodeWidthChar;

const SCROLLBACK_HISTORY: usize = 1_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Style {
    pub fg: Option<u8>,
    pub bg: Option<u8>,
    pub bold: bool,
    pub dim: bool,
    pub reverse: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cell {
    pub ch: char,
    pub style: Style,
    pub wide_cont: bool,
}

impl Cell {
    pub fn blank(style: Style) -> Self {
        Self {
            ch: ' ',
            style,
            wide_cont: false,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct TerminalSize {
    rows: usize,
    cols: usize,
}

impl Dimensions for TerminalSize {
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

pub struct VirtualTerminal {
    rows: usize,
    cols: usize,
    term: Term<VoidListener>,
    parser: Processor,
    history: Vec<Vec<Cell>>,
    screen: Vec<Vec<Cell>>,
    history_valid: bool,
}

impl VirtualTerminal {
    pub fn new(rows: usize, cols: usize) -> Self {
        let rows = max(rows, 1);
        let cols = max(cols, 1);
        let mut terminal = Self {
            rows,
            cols,
            term: Self::new_term(rows, cols),
            parser: Processor::new(),
            history: Vec::new(),
            screen: Vec::new(),
            history_valid: false,
        };
        terminal.refresh_screen_cache();
        terminal
    }

    fn new_term(rows: usize, cols: usize) -> Term<VoidListener> {
        let config = TermConfig {
            scrolling_history: SCROLLBACK_HISTORY,
            ..Default::default()
        };
        Term::new(config, &TerminalSize { rows, cols }, VoidListener)
    }

    pub fn reset(&mut self, _preserve_history: bool) {
        self.term = Self::new_term(self.rows, self.cols);
        self.parser = Processor::new();
        self.history.clear();
        self.history_valid = true;
        self.refresh_screen_cache();
    }

    pub fn reset_to_size(&mut self, rows: usize, cols: usize) {
        self.rows = max(rows, 1);
        self.cols = max(cols, 1);
        self.reset(false);
    }

    pub fn resize(&mut self, rows: usize, cols: usize) {
        let rows = max(rows, 1);
        let cols = max(cols, 1);
        if self.rows == rows && self.cols == cols {
            return;
        }
        self.rows = rows;
        self.cols = cols;
        self.term.resize(TerminalSize { rows, cols });
        self.history_valid = false;
        self.refresh_screen_cache();
    }

    pub fn feed(&mut self, text: &str) {
        self.parser.advance(&mut self.term, text.as_bytes());
        self.refresh_screen_cache();
    }

    pub fn rows(&self) -> usize {
        self.rows
    }

    pub fn cols(&self) -> usize {
        self.cols
    }

    pub fn history_len(&self) -> usize {
        self.term.grid().history_size()
    }

    pub fn history_rows(&mut self) -> &[Vec<Cell>] {
        self.refresh_history_cache();
        &self.history
    }

    pub fn screen_rows(&self) -> &[Vec<Cell>] {
        &self.screen
    }

    pub fn trimmed_screen_rows(&self) -> Vec<Vec<Cell>> {
        self.screen.iter().map(|row| trim_row(row)).collect()
    }

    pub fn rendered_rows(&mut self) -> Vec<Vec<Cell>> {
        let screen_rows = self.trimmed_screen_rows();
        let history_rows = self.history_rows();
        let mut rows = Vec::with_capacity(history_rows.len() + screen_rows.len());
        rows.extend(history_rows.iter().cloned());
        rows.extend(screen_rows);
        rows
    }

    pub fn rendered_lines(&mut self) -> Vec<String> {
        self.rendered_rows()
            .into_iter()
            .map(|row| row_to_text(&row))
            .collect()
    }

    fn refresh_history_cache(&mut self) {
        if self.history_valid && self.history.len() == self.term.grid().history_size() {
            return;
        }

        let grid = self.term.grid();
        let history_size = grid.history_size();
        if self.history_valid && self.history.len() < history_size {
            let added = history_size - self.history.len();
            for line in -(added as i32)..0 {
                self.history.push(collect_grid_line(grid, Line(line)));
            }
        } else {
            self.history.clear();
            for line in grid.topmost_line().0..0 {
                self.history.push(collect_grid_line(grid, Line(line)));
            }
        }
        self.history_valid = true;
    }

    fn refresh_screen_cache(&mut self) {
        let grid = self.term.grid();
        self.screen.clear();
        self.screen.reserve(self.rows);
        for line in 0..self.rows as i32 {
            self.screen.push(collect_grid_line(grid, Line(line)));
        }
    }
}

pub fn clamp(value: usize, lower: usize, upper: usize) -> usize {
    value.max(lower).min(upper)
}

pub fn clamp_signed(value: isize, lower: isize, upper: isize) -> isize {
    value.max(lower).min(upper)
}

pub fn char_width(ch: char) -> usize {
    UnicodeWidthChar::width(ch).unwrap_or(0)
}

pub fn trim_row(row: &[Cell]) -> Vec<Cell> {
    let mut end = row.len();
    while end > 0 {
        let cell = row[end - 1];
        if cell.wide_cont || cell.ch != ' ' {
            break;
        }
        end -= 1;
    }
    row[..end].to_vec()
}

pub fn row_to_text(row: &[Cell]) -> String {
    trim_row(row)
        .iter()
        .filter(|cell| !cell.wide_cont)
        .map(|cell| cell.ch)
        .collect()
}

pub fn wrap_styled_line(line: &[Cell], width: usize) -> Vec<Vec<Cell>> {
    let width = max(width, 1);
    let trimmed = trim_row(line);
    if trimmed.is_empty() {
        return vec![Vec::new()];
    }

    let mut out = Vec::new();
    let mut buf = Vec::new();
    let mut used = 0usize;

    for cell in trimmed {
        if cell.wide_cont {
            continue;
        }
        let w = max(char_width(cell.ch), 1);
        if used > 0 && used + w > width {
            out.push(buf);
            buf = vec![cell];
            used = w;
            continue;
        }
        buf.push(cell);
        used += w;
        if used >= width {
            out.push(buf);
            buf = Vec::new();
            used = 0;
        }
    }

    if !buf.is_empty() || out.is_empty() {
        out.push(buf);
    }
    out
}

pub fn decode_utf8_chunk(pending: &mut Vec<u8>, bytes: &[u8], final_flush: bool) -> String {
    let mut buf = Vec::with_capacity(pending.len() + bytes.len());
    buf.extend_from_slice(pending);
    buf.extend_from_slice(bytes);
    pending.clear();

    let mut out = String::new();
    let mut idx = 0usize;
    while idx < buf.len() {
        match std::str::from_utf8(&buf[idx..]) {
            Ok(text) => {
                out.push_str(text);
                idx = buf.len();
            }
            Err(err) => {
                let valid_end = idx + err.valid_up_to();
                if valid_end > idx {
                    if let Ok(text) = std::str::from_utf8(&buf[idx..valid_end]) {
                        out.push_str(text);
                    }
                }
                if let Some(err_len) = err.error_len() {
                    out.push('\u{fffd}');
                    idx = valid_end + err_len;
                } else {
                    pending.extend_from_slice(&buf[valid_end..]);
                    idx = buf.len();
                }
            }
        }
    }

    if final_flush && !pending.is_empty() {
        out.push('\u{fffd}');
        pending.clear();
    }
    out
}

fn collect_grid_line(
    grid: &alacritty_terminal::grid::Grid<AlacrittyCell>,
    line: Line,
) -> Vec<Cell> {
    let mut row = Vec::with_capacity(grid.columns());
    for col in 0..grid.columns() {
        row.push(convert_cell(&grid[line][Column(col)]));
    }
    row
}

fn convert_cell(cell: &AlacrittyCell) -> Cell {
    let hidden = cell.flags.contains(Flags::HIDDEN);
    Cell {
        ch: if hidden { ' ' } else { cell.c },
        style: Style {
            fg: color_to_xterm256(cell.fg),
            bg: color_to_xterm256(cell.bg),
            bold: cell.flags.contains(Flags::BOLD),
            dim: cell.flags.contains(Flags::DIM),
            reverse: cell.flags.contains(Flags::INVERSE),
        },
        wide_cont: cell
            .flags
            .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER),
    }
}

fn color_to_xterm256(color: Color) -> Option<u8> {
    match color {
        Color::Named(named) => named_color_to_xterm256(named),
        Color::Indexed(index) => Some(index),
        Color::Spec(rgb) => Some(rgb_to_xterm256(rgb.r, rgb.g, rgb.b)),
    }
}

fn named_color_to_xterm256(color: NamedColor) -> Option<u8> {
    match color {
        NamedColor::Black | NamedColor::DimBlack => Some(0),
        NamedColor::Red | NamedColor::DimRed => Some(1),
        NamedColor::Green | NamedColor::DimGreen => Some(2),
        NamedColor::Yellow | NamedColor::DimYellow => Some(3),
        NamedColor::Blue | NamedColor::DimBlue => Some(4),
        NamedColor::Magenta | NamedColor::DimMagenta => Some(5),
        NamedColor::Cyan | NamedColor::DimCyan => Some(6),
        NamedColor::White | NamedColor::DimWhite => Some(7),
        NamedColor::BrightBlack => Some(8),
        NamedColor::BrightRed => Some(9),
        NamedColor::BrightGreen => Some(10),
        NamedColor::BrightYellow => Some(11),
        NamedColor::BrightBlue => Some(12),
        NamedColor::BrightMagenta => Some(13),
        NamedColor::BrightCyan => Some(14),
        NamedColor::BrightWhite => Some(15),
        _ => None,
    }
}

fn rgb_to_xterm256(r: u8, g: u8, b: u8) -> u8 {
    if r == g && g == b {
        if r < 8 {
            return 16;
        }
        if r > 248 {
            return 231;
        }
        return (((r as f32 - 8.0) / 247.0) * 24.0).round() as u8 + 232;
    }
    16 + 36 * ((r as f32 / 255.0) * 5.0).round() as u8
        + 6 * ((g as f32 / 255.0) * 5.0).round() as u8
        + ((b as f32 / 255.0) * 5.0).round() as u8
}

#[cfg(test)]
mod tests {
    use super::VirtualTerminal;

    #[test]
    fn resize_preserves_existing_row_content() {
        let mut term = VirtualTerminal::new(1, 3);
        term.feed("abc");
        term.resize(1, 6);
        term.feed("def");

        assert_eq!(term.rendered_lines(), vec!["abcdef"]);
    }

    #[test]
    fn cursor_motion_after_last_column_cancels_wrap_pending() {
        let mut term = VirtualTerminal::new(2, 5);
        term.feed("abcde\x1b[AZ");

        assert_eq!(term.rendered_lines(), vec!["abcdZ", ""]);
    }

    #[test]
    fn sgr_after_last_column_keeps_wrap_pending() {
        let mut term = VirtualTerminal::new(2, 5);
        term.feed("abcde\x1b[31mZ");

        assert_eq!(term.rendered_lines(), vec!["abcde", "Z"]);
    }

    #[test]
    fn scrollback_rows_are_exposed_before_screen_rows() {
        let mut term = VirtualTerminal::new(2, 10);
        term.feed("one\r\ntwo\r\nthree");

        assert_eq!(term.history_len(), 1);
        assert_eq!(term.rendered_lines(), vec!["one", "two", "three"]);
    }

    #[test]
    fn cursor_forward_preserves_existing_cells_like_a_terminal() {
        let mut term = VirtualTerminal::new(1, 20);
        term.feed("abcde");
        term.feed("\rX\x1b[2CZ");

        assert_eq!(term.rendered_lines(), vec!["XbcZe"]);
    }
}
