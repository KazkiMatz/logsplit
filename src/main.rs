use std::cmp::{max, min};
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::Parser;
use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::style::{
    Attribute, Color, Print, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor,
};
use crossterm::terminal::{
    self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
    enable_raw_mode,
};
use crossterm::{SynchronizedUpdate, execute, queue};
use unicode_width::UnicodeWidthChar;

#[derive(Debug, Parser, Clone)]
#[command(about = "less-like viewer for Claude/script logs")]
struct Args {
    path: PathBuf,
    #[arg(long)]
    rows: Option<usize>,
    #[arg(long)]
    cols: Option<usize>,
    #[arg(long)]
    follow: bool,
    #[arg(long)]
    dump: bool,
    #[arg(long)]
    tail: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
struct Style {
    fg: Option<u8>,
    bg: Option<u8>,
    bold: bool,
    dim: bool,
    reverse: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Cell {
    ch: char,
    style: Style,
    wide_cont: bool,
}

impl Cell {
    fn blank(style: Style) -> Self {
        Self {
            ch: ' ',
            style,
            wide_cont: false,
        }
    }
}

#[derive(Debug, Clone)]
struct FrameSnapshot {
    width: u16,
    height: u16,
    rows: Vec<Vec<Cell>>,
    status: String,
}

#[derive(Debug, Clone, Copy, Default)]
struct Cursor {
    row: usize,
    col: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParseState {
    Normal,
    Esc,
    Csi,
    Osc,
    EscOther,
    Charset,
}

#[derive(Debug)]
struct VirtualTerminal {
    rows: usize,
    cols: usize,
    history: Vec<Vec<Cell>>,
    current_style: Style,
    screen: Vec<Vec<Cell>>,
    cursor: Cursor,
    saved_cursor: Cursor,
    state: ParseState,
    csi: String,
    esc_other: String,
    osc: String,
    osc_escape: bool,
}

impl VirtualTerminal {
    fn new(rows: usize, cols: usize) -> Self {
        let mut term = Self {
            rows: max(rows, 1),
            cols: max(cols, 1),
            history: Vec::new(),
            current_style: Style::default(),
            screen: Vec::new(),
            cursor: Cursor::default(),
            saved_cursor: Cursor::default(),
            state: ParseState::Normal,
            csi: String::new(),
            esc_other: String::new(),
            osc: String::new(),
            osc_escape: false,
        };
        term.reset(false);
        term
    }

    fn reset(&mut self, preserve_history: bool) {
        if !preserve_history {
            self.history.clear();
        }
        self.current_style = Style::default();
        self.screen = (0..self.rows).map(|_| self.blank_row()).collect();
        self.cursor = Cursor::default();
        self.saved_cursor = Cursor::default();
        self.state = ParseState::Normal;
        self.csi.clear();
        self.esc_other.clear();
        self.osc.clear();
        self.osc_escape = false;
    }

    fn resize(&mut self, rows: usize, cols: usize) {
        self.rows = max(rows, 1);
        self.cols = max(cols, 1);
        self.reset(false);
    }

    fn feed(&mut self, text: &str) {
        for ch in text.chars() {
            self.feed_char(ch);
        }
    }

    fn feed_char(&mut self, ch: char) {
        match self.state {
            ParseState::Normal => self.feed_normal(ch),
            ParseState::Esc => self.feed_esc(ch),
            ParseState::Csi => self.feed_csi(ch),
            ParseState::Osc => self.feed_osc(ch),
            ParseState::EscOther => self.feed_esc_other(ch),
            ParseState::Charset => self.state = ParseState::Normal,
        }
    }

    fn feed_normal(&mut self, ch: char) {
        let ord = ch as u32;
        match ch {
            '\x1b' => self.state = ParseState::Esc,
            '\r' => self.cursor.col = 0,
            '\n' => self.linefeed(),
            '\x08' => self.cursor.col = self.cursor.col.saturating_sub(1),
            '\t' => {
                let next_tab = ((self.cursor.col / 8) + 1) * 8;
                self.cursor.col = min(next_tab, self.cols.saturating_sub(1));
            }
            '\x07' | '\x0e' | '\x0f' | '\x11' | '\x13' => {}
            _ if ord < 32 || ord == 127 => {}
            _ => self.put_char(ch),
        }
    }

    fn feed_esc(&mut self, ch: char) {
        match ch {
            '[' => {
                self.csi.clear();
                self.state = ParseState::Csi;
            }
            ']' => {
                self.osc.clear();
                self.osc_escape = false;
                self.state = ParseState::Osc;
            }
            '(' | ')' | '*' | '+' => self.state = ParseState::Charset,
            '7' => {
                self.saved_cursor = self.cursor;
                self.state = ParseState::Normal;
            }
            '8' => {
                self.cursor = self.saved_cursor;
                self.state = ParseState::Normal;
            }
            'c' => {
                self.reset(true);
                self.state = ParseState::Normal;
            }
            'D' => {
                self.linefeed();
                self.state = ParseState::Normal;
            }
            'E' => {
                self.cursor.col = 0;
                self.linefeed();
                self.state = ParseState::Normal;
            }
            'M' => {
                self.reverse_index();
                self.state = ParseState::Normal;
            }
            _ if ('@'..='~').contains(&ch) => self.state = ParseState::Normal,
            _ => {
                self.esc_other.clear();
                self.esc_other.push(ch);
                self.state = ParseState::EscOther;
            }
        }
    }

    fn feed_esc_other(&mut self, ch: char) {
        self.esc_other.push(ch);
        if ('@'..='~').contains(&ch) {
            self.state = ParseState::Normal;
        }
    }

    fn feed_osc(&mut self, ch: char) {
        if self.osc_escape {
            self.osc_escape = false;
            if ch == '\\' {
                self.state = ParseState::Normal;
            } else {
                self.osc.push('\x1b');
                self.osc.push(ch);
            }
            return;
        }
        match ch {
            '\x07' => self.state = ParseState::Normal,
            '\x1b' => self.osc_escape = true,
            _ => self.osc.push(ch),
        }
    }

    fn feed_csi(&mut self, ch: char) {
        self.csi.push(ch);
        if ('@'..='~').contains(&ch) {
            let seq = self.csi.clone();
            self.csi.clear();
            self.state = ParseState::Normal;
            self.handle_csi(&seq);
        }
    }

    fn handle_csi(&mut self, seq: &str) {
        if seq.is_empty() || !valid_csi(seq) {
            return;
        }

        let final_char = seq.chars().last().unwrap_or_default();
        let mut body = &seq[..seq.len().saturating_sub(1)];
        let mut private = '\0';
        if let Some(first) = body.chars().next() {
            if matches!(first, '<' | '=' | '>' | '?') {
                private = first;
                body = &body[first.len_utf8()..];
            }
        }
        let params = if body.is_empty() {
            Vec::new()
        } else {
            body.split(';').map(parse_param).collect::<Vec<_>>()
        };

        match final_char {
            'A' => {
                self.cursor.row = self.cursor.row.saturating_sub(param(&params, 0, 1));
            }
            'B' => self.cursor_down(param(&params, 0, 1), false),
            'C' => {
                self.cursor.col = min(
                    self.cols.saturating_sub(1),
                    self.cursor.col + param(&params, 0, 1),
                );
            }
            'D' => {
                self.cursor.col = self.cursor.col.saturating_sub(param(&params, 0, 1));
            }
            'E' => {
                self.cursor_down(param(&params, 0, 1), false);
                self.cursor.col = 0;
            }
            'F' => {
                self.cursor.row = self.cursor.row.saturating_sub(param(&params, 0, 1));
                self.cursor.col = 0;
            }
            'G' => {
                let col = param(&params, 0, 1).saturating_sub(1);
                self.cursor.col = min(self.cols.saturating_sub(1), col);
            }
            'H' | 'f' => {
                let row = param(&params, 0, 1).max(1) - 1;
                let col = param(&params, 1, 1).max(1) - 1;
                self.cursor.row = min(self.rows.saturating_sub(1), row);
                self.cursor.col = min(self.cols.saturating_sub(1), col);
            }
            'J' => self.erase_in_display(param(&params, 0, 0)),
            'K' => self.erase_in_line(param(&params, 0, 0)),
            'L' => self.insert_lines(param(&params, 0, 1)),
            'M' => self.delete_lines(param(&params, 0, 1)),
            '@' => self.insert_chars(param(&params, 0, 1)),
            'P' => self.delete_chars(param(&params, 0, 1)),
            'X' => self.erase_chars(param(&params, 0, 1)),
            'S' => self.scroll_up(param(&params, 0, 1)),
            'T' => self.scroll_down(param(&params, 0, 1)),
            'm' => self.handle_sgr(body),
            's' => self.saved_cursor = self.cursor,
            'u' => self.cursor = self.saved_cursor,
            'h' | 'l' => {}
            'n' if private == '?' => {}
            _ => {}
        }
    }

    fn blank_row(&self) -> Vec<Cell> {
        vec![Cell::blank(self.current_style); self.cols]
    }

    fn handle_sgr(&mut self, body: &str) {
        let params = if body.is_empty() {
            vec!["0"]
        } else {
            body.split(';').collect::<Vec<_>>()
        };

        let mut fg = self.current_style.fg;
        let mut bg = self.current_style.bg;
        let mut bold = self.current_style.bold;
        let mut dim = self.current_style.dim;
        let mut reverse = self.current_style.reverse;

        let mut idx = 0usize;
        while idx < params.len() {
            let code = parse_sgr_int(params[idx]);
            let Some(code) = code else {
                idx += 1;
                continue;
            };
            match code {
                0 => {
                    fg = None;
                    bg = None;
                    bold = false;
                    dim = false;
                    reverse = false;
                }
                1 => bold = true,
                2 => dim = true,
                22 => {
                    bold = false;
                    dim = false;
                }
                7 => reverse = true,
                27 => reverse = false,
                39 => fg = None,
                49 => bg = None,
                30..=37 => fg = Some((code - 30) as u8),
                40..=47 => bg = Some((code - 40) as u8),
                90..=97 => fg = Some((code - 90 + 8) as u8),
                100..=107 => bg = Some((code - 100 + 8) as u8),
                38 | 48 => {
                    let is_fg = code == 38;
                    let mode = params.get(idx + 1).and_then(|value| parse_sgr_int(value));
                    if mode == Some(5) && idx + 2 < params.len() {
                        if let Some(value) = parse_sgr_int(params[idx + 2]) {
                            if is_fg {
                                fg = Some(value as u8);
                            } else {
                                bg = Some(value as u8);
                            }
                        }
                        idx += 2;
                    } else if mode == Some(2) && idx + 4 < params.len() {
                        let parts = (2..=4)
                            .map(|offset| {
                                params
                                    .get(idx + offset)
                                    .and_then(|value| parse_sgr_int(value))
                            })
                            .collect::<Vec<_>>();
                        if parts.iter().all(|value| value.is_some()) {
                            let color = rgb_to_xterm256(
                                parts[0].unwrap_or_default() as u8,
                                parts[1].unwrap_or_default() as u8,
                                parts[2].unwrap_or_default() as u8,
                            );
                            if is_fg {
                                fg = Some(color);
                            } else {
                                bg = Some(color);
                            }
                        }
                        idx += 4;
                    }
                }
                _ => {}
            }
            idx += 1;
        }

        self.current_style = Style {
            fg,
            bg,
            bold,
            dim,
            reverse,
        };
    }

    fn put_char(&mut self, ch: char) {
        let width = max(char_width(ch), 1);
        if self.cursor.col >= self.cols {
            self.cursor.col = 0;
            self.linefeed();
        }
        if width == 2 && self.cursor.col == self.cols.saturating_sub(1) {
            self.cursor.col = 0;
            self.linefeed();
        }
        self.screen[self.cursor.row][self.cursor.col] = Cell {
            ch,
            style: self.current_style,
            wide_cont: false,
        };
        if width == 2 && self.cursor.col + 1 < self.cols {
            self.screen[self.cursor.row][self.cursor.col + 1] = Cell {
                ch: ' ',
                style: self.current_style,
                wide_cont: true,
            };
        }
        self.cursor.col += width;
        if self.cursor.col >= self.cols {
            self.cursor.col = self.cols;
        }
    }

    fn linefeed(&mut self) {
        if self.cursor.row == self.rows.saturating_sub(1) {
            self.scroll_up(1);
        } else {
            self.cursor.row += 1;
        }
    }

    fn reverse_index(&mut self) {
        if self.cursor.row == 0 {
            self.screen.insert(0, self.blank_row());
            self.screen.pop();
        } else {
            self.cursor.row -= 1;
        }
    }

    fn cursor_down(&mut self, amount: usize, allow_scroll: bool) {
        for _ in 0..amount {
            if self.cursor.row == self.rows.saturating_sub(1) {
                if allow_scroll {
                    self.scroll_up(1);
                }
            } else {
                self.cursor.row += 1;
            }
        }
    }

    fn scroll_up(&mut self, amount: usize) {
        for _ in 0..amount {
            let first = trim_row(&self.screen[0]);
            self.history.push(first);
            self.screen.remove(0);
            self.screen.push(self.blank_row());
        }
    }

    fn scroll_down(&mut self, amount: usize) {
        for _ in 0..amount {
            self.screen.insert(0, self.blank_row());
            self.screen.pop();
        }
    }

    fn erase_in_line(&mut self, mode: usize) {
        let (start, end) = match mode {
            0 => (self.cursor.col, self.cols),
            1 => (0, self.cursor.col + 1),
            _ => (0, self.cols),
        };
        let row = &mut self.screen[self.cursor.row];
        for cell in row.iter_mut().take(min(end, self.cols)).skip(start) {
            *cell = Cell::blank(self.current_style);
        }
    }

    fn erase_in_display(&mut self, mode: usize) {
        match mode {
            0 => {
                self.erase_in_line(0);
                for row in self.cursor.row + 1..self.rows {
                    self.screen[row] = self.blank_row();
                }
            }
            1 => {
                self.erase_in_line(1);
                for row in 0..self.cursor.row {
                    self.screen[row] = self.blank_row();
                }
            }
            _ => {
                self.screen = (0..self.rows).map(|_| self.blank_row()).collect();
            }
        }
    }

    fn insert_lines(&mut self, amount: usize) {
        for _ in 0..amount {
            self.screen.insert(self.cursor.row, self.blank_row());
            self.screen.pop();
        }
    }

    fn delete_lines(&mut self, amount: usize) {
        for _ in 0..amount {
            if self.cursor.row < self.screen.len() {
                self.screen.remove(self.cursor.row);
                self.screen.push(self.blank_row());
            }
        }
    }

    fn insert_chars(&mut self, amount: usize) {
        let row = &mut self.screen[self.cursor.row];
        for _ in 0..amount {
            row.insert(self.cursor.col, Cell::blank(self.current_style));
            row.pop();
        }
    }

    fn delete_chars(&mut self, amount: usize) {
        let row = &mut self.screen[self.cursor.row];
        for _ in 0..amount {
            if self.cursor.col < row.len() {
                row.remove(self.cursor.col);
                row.push(Cell::blank(self.current_style));
            }
        }
    }

    fn erase_chars(&mut self, amount: usize) {
        let end = min(self.cols, self.cursor.col + amount);
        let row = &mut self.screen[self.cursor.row];
        for cell in row.iter_mut().take(end).skip(self.cursor.col) {
            *cell = Cell::blank(self.current_style);
        }
    }

    fn rendered_rows(&self) -> Vec<Vec<Cell>> {
        let mut rows = self.history.clone();
        rows.extend(self.screen.iter().map(|row| trim_row(row)));
        rows
    }

    fn rendered_lines(&self) -> Vec<String> {
        self.rendered_rows()
            .into_iter()
            .map(|row| row_to_text(&row))
            .collect()
    }
}

#[derive(Debug)]
struct ReplayFile {
    path: PathBuf,
    offset: u64,
    pending_utf8: Vec<u8>,
}

impl ReplayFile {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            offset: 0,
            pending_utf8: Vec::new(),
        }
    }

    fn replay_all(&mut self, term: &mut VirtualTerminal) -> io::Result<()> {
        term.reset(false);
        self.offset = 0;
        self.pending_utf8.clear();

        let mut fh = File::open(&self.path)?;
        let mut buf = [0u8; 65536];
        loop {
            let count = fh.read(&mut buf)?;
            if count == 0 {
                break;
            }
            self.offset += count as u64;
            let text = decode_utf8_chunk(&mut self.pending_utf8, &buf[..count], false);
            term.feed(&text);
        }
        let remainder = decode_utf8_chunk(&mut self.pending_utf8, &[], true);
        if !remainder.is_empty() {
            term.feed(&remainder);
        }
        Ok(())
    }

    fn poll(&mut self, term: &mut VirtualTerminal) -> io::Result<bool> {
        let size = std::fs::metadata(&self.path)?.len();
        if size < self.offset {
            self.replay_all(term)?;
            return Ok(true);
        }
        if size == self.offset {
            return Ok(false);
        }

        let mut fh = File::open(&self.path)?;
        fh.seek(SeekFrom::Start(self.offset))?;

        let mut changed = false;
        let mut buf = [0u8; 65536];
        loop {
            let count = fh.read(&mut buf)?;
            if count == 0 {
                break;
            }
            changed = true;
            self.offset += count as u64;
            let text = decode_utf8_chunk(&mut self.pending_utf8, &buf[..count], false);
            term.feed(&text);
        }
        Ok(changed)
    }
}

#[derive(Debug, Clone)]
struct LayoutCache {
    content_width: usize,
    history_count: usize,
    display: Vec<(usize, Vec<Cell>)>,
    first_display_by_logical: Vec<usize>,
}

struct ViewerApp {
    path: PathBuf,
    args: Args,
    status: String,
    follow: bool,
    search_term: Option<String>,
    last_search_forward: bool,
    top: usize,
    term: VirtualTerminal,
    replay: ReplayFile,
    layout_cache: Option<LayoutCache>,
    layout_stale: bool,
    needs_redraw: bool,
    previous_frame: Option<FrameSnapshot>,
}

impl ViewerApp {
    fn new(path: PathBuf, args: Args) -> io::Result<Self> {
        let mut app = Self {
            path: path.clone(),
            args,
            status: String::new(),
            follow: false,
            search_term: None,
            last_search_forward: true,
            top: 0,
            term: VirtualTerminal::new(1, 1),
            replay: ReplayFile::new(path),
            layout_cache: None,
            layout_stale: true,
            needs_redraw: true,
            previous_frame: None,
        };
        app.follow = app.args.follow;
        app.rebuild_from_screen_size()?;
        app.replay.replay_all(&mut app.term)?;
        Ok(app)
    }

    fn viewport_dims(&self) -> io::Result<(usize, usize)> {
        let (width, height) = terminal::size()?;
        let content_rows = max(height as usize, 1).saturating_sub(1).max(1);
        let term_rows = self.args.rows.unwrap_or(max(content_rows, 40));
        let term_cols = self.args.cols.unwrap_or(max(width as usize, 120));
        Ok((max(term_rows, 1), max(term_cols, 1)))
    }

    fn rebuild_from_screen_size(&mut self) -> io::Result<()> {
        let (rows, cols) = self.viewport_dims()?;
        self.term.resize(rows, cols);
        self.drop_layout_cache();
        Ok(())
    }

    fn invalidate_layout(&mut self) {
        self.layout_stale = true;
    }

    fn drop_layout_cache(&mut self) {
        self.layout_cache = None;
        self.layout_stale = true;
    }

    fn invalidate_frame(&mut self) {
        self.previous_frame = None;
    }

    fn mark_dirty(&mut self) {
        self.needs_redraw = true;
    }

    fn ensure_layout(&mut self) -> io::Result<()> {
        let (width, _height) = terminal::size()?;
        let content_width = max(width as usize, 1).saturating_sub(1).max(1);
        let current_history_count = self.term.history.len();

        if let Some(cache) = &self.layout_cache {
            if !self.layout_stale
                && cache.content_width == content_width
                && cache.history_count == current_history_count
            {
                return Ok(());
            }
        }

        let screen_rows = self
            .term
            .screen
            .iter()
            .map(|row| trim_row(row))
            .collect::<Vec<_>>();

        let (display, first_display_by_logical) = if let Some(cache) = &self.layout_cache {
            if cache.content_width == content_width && cache.history_count <= current_history_count
            {
                let cached_history_count = cache.history_count;
                let prefix_display_count = cache
                    .first_display_by_logical
                    .get(cached_history_count)
                    .copied()
                    .unwrap_or_else(|| cache.display.len());
                let mut display = cache.display[..prefix_display_count].to_vec();
                let mut first = cache.first_display_by_logical[..cached_history_count].to_vec();

                for (idx, row) in self
                    .term
                    .history
                    .iter()
                    .enumerate()
                    .skip(cached_history_count)
                {
                    first.push(display.len());
                    for segment in wrap_styled_line(row, content_width) {
                        display.push((idx, segment));
                    }
                }

                for (offset, row) in screen_rows.iter().enumerate() {
                    let logical_idx = current_history_count + offset;
                    first.push(display.len());
                    for segment in wrap_styled_line(row, content_width) {
                        display.push((logical_idx, segment));
                    }
                }

                (display, first)
            } else {
                full_layout(&self.term.history, &screen_rows, content_width)
            }
        } else {
            full_layout(&self.term.history, &screen_rows, content_width)
        };

        self.layout_cache = Some(LayoutCache {
            content_width,
            history_count: current_history_count,
            display,
            first_display_by_logical,
        });
        self.layout_stale = false;
        Ok(())
    }

    fn layout_cache(&mut self) -> io::Result<&LayoutCache> {
        self.ensure_layout()?;
        Ok(self
            .layout_cache
            .as_ref()
            .expect("layout cache should exist after ensure_layout"))
    }

    fn lines(&self) -> Vec<String> {
        self.term.rendered_lines()
    }

    fn content_height(&self) -> io::Result<usize> {
        let (_w, h) = terminal::size()?;
        Ok(max(h as usize, 1).saturating_sub(1).max(1))
    }

    fn max_top(&mut self) -> io::Result<usize> {
        let len = self.layout_cache()?.display.len();
        Ok(len.saturating_sub(self.content_height()?))
    }

    fn jump_to_end(&mut self) -> io::Result<()> {
        self.top = self.max_top()?;
        self.mark_dirty();
        Ok(())
    }

    fn draw(&mut self, stdout: &mut io::Stdout) -> io::Result<()> {
        let (width, height) = terminal::size()?;
        let content_height = self.content_height()?;
        self.ensure_layout()?;
        let line_count = self
            .layout_cache
            .as_ref()
            .expect("layout cache should exist after ensure_layout")
            .display
            .len();
        let max_top = line_count.saturating_sub(content_height);
        let top = clamp(self.top, 0, max_top);
        self.top = top;

        let mode = if self.follow { "FOLLOW" } else { "PAUSED" };
        let percent = if line_count == 0 {
            100
        } else {
            min(100, ((top + content_height) * 100) / line_count)
        };
        let default_status = format!(
            "{}  {}  {}/{}  {}%",
            mode,
            self.path
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or("viewer"),
            top + 1,
            max(line_count, 1),
            percent
        );
        let status = if self.status.is_empty() {
            default_status
        } else {
            self.status.clone()
        };
        let mut padded = status;
        let width_usize = width as usize;
        if padded.len() < width_usize {
            padded.push_str(&" ".repeat(width_usize - padded.len()));
        }
        padded.truncate(width_usize.saturating_sub(1));

        let frame = {
            let cache = self
                .layout_cache
                .as_ref()
                .expect("layout cache should exist after ensure_layout");
            let visible_end = min(top + content_height, cache.display.len());
            let visible = &cache.display[top..visible_end];
            let mut rows = Vec::with_capacity(content_height);
            for y in 0..content_height {
                if let Some((_logical_idx, cells)) = visible.get(y) {
                    rows.push(cells.clone());
                } else {
                    rows.push(Vec::new());
                }
            }
            FrameSnapshot {
                width,
                height,
                rows,
                status: padded,
            }
        };

        stdout.sync_update(|stdout| -> io::Result<()> {
            self.draw_frame_diff(stdout, &frame)?;
            Ok(())
        })??;
        self.previous_frame = Some(frame);
        Ok(())
    }

    fn draw_frame_diff(&self, stdout: &mut io::Stdout, frame: &FrameSnapshot) -> io::Result<()> {
        let full_redraw = self.previous_frame.as_ref().is_none_or(|previous| {
            previous.width != frame.width
                || previous.height != frame.height
                || previous.rows.len() != frame.rows.len()
        });

        for (y, row) in frame.rows.iter().enumerate() {
            let previous_row = if full_redraw {
                None
            } else {
                self.previous_frame
                    .as_ref()
                    .and_then(|previous| previous.rows.get(y))
                    .map(Vec::as_slice)
            };
            self.draw_row_diff(
                stdout,
                y as u16,
                previous_row,
                row,
                frame.width.saturating_sub(1) as usize,
            )?;
        }

        let status_changed = full_redraw
            || self
                .previous_frame
                .as_ref()
                .is_none_or(|previous| previous.status != frame.status);
        if status_changed {
            queue!(
                stdout,
                MoveTo(0, frame.height.saturating_sub(1)),
                Clear(ClearType::CurrentLine),
                SetAttribute(Attribute::Reset),
                ResetColor,
                SetAttribute(Attribute::Reverse),
                Print(frame.status.as_str()),
                SetAttribute(Attribute::Reset),
                ResetColor
            )?;
        }
        Ok(())
    }

    fn draw_row_diff(
        &self,
        stdout: &mut io::Stdout,
        y: u16,
        previous: Option<&[Cell]>,
        cells: &[Cell],
        max_width: usize,
    ) -> io::Result<()> {
        if previous == Some(cells) {
            return Ok(());
        }

        let prefix_cells = previous
            .map(|previous| common_prefix_len(previous, cells))
            .unwrap_or(0);
        let start_x = cell_prefix_width(cells, prefix_cells);
        if start_x >= max_width {
            return Ok(());
        }

        queue!(
            stdout,
            MoveTo(start_x as u16, y),
            Clear(ClearType::UntilNewLine)
        )?;
        self.draw_cells(stdout, y, start_x, &cells[prefix_cells..], max_width)?;
        Ok(())
    }

    fn draw_cells(
        &self,
        stdout: &mut io::Stdout,
        y: u16,
        start_x: usize,
        cells: &[Cell],
        max_width: usize,
    ) -> io::Result<()> {
        let mut x = start_x;
        let mut run_chars = String::new();
        let mut run_style: Option<Style> = None;
        let mut run_start = start_x;

        let flush = |stdout: &mut io::Stdout,
                     y: u16,
                     run_start: usize,
                     run_chars: &mut String,
                     run_style: &mut Option<Style>|
         -> io::Result<()> {
            let Some(style) = *run_style else {
                return Ok(());
            };
            if run_chars.is_empty() {
                return Ok(());
            }
            apply_style(stdout, style)?;
            queue!(
                stdout,
                MoveTo(run_start as u16, y),
                Print(run_chars.as_str())
            )?;
            run_chars.clear();
            *run_style = None;
            Ok(())
        };

        for cell in cells {
            if cell.wide_cont {
                continue;
            }
            let width = max(char_width(cell.ch), 1);
            if x + width > max_width {
                break;
            }
            if run_style != Some(cell.style) {
                flush(stdout, y, run_start, &mut run_chars, &mut run_style)?;
                run_style = Some(cell.style);
                run_start = x;
            }
            run_chars.push(cell.ch);
            x += width;
        }
        flush(stdout, y, run_start, &mut run_chars, &mut run_style)?;
        Ok(())
    }

    fn scroll(&mut self, amount: isize) -> io::Result<()> {
        let had_status = !self.status.is_empty();
        if amount != 0 {
            self.follow = false;
        }
        let max_top = self.max_top()?;
        let new_top = clamp_signed(self.top as isize + amount, 0, max_top as isize) as usize;
        if new_top != self.top {
            self.top = new_top;
            self.mark_dirty();
        }
        self.status.clear();
        if had_status {
            self.mark_dirty();
        }
        Ok(())
    }

    fn page(&mut self, amount: isize) -> io::Result<()> {
        let step = self.content_height()? as isize;
        self.scroll(amount * step)
    }

    fn half_page(&mut self, amount: isize) -> io::Result<()> {
        let step = max(1, self.content_height()? / 2) as isize;
        self.scroll(amount * step)
    }

    fn prompt(&mut self, stdout: &mut io::Stdout, prompt: &str) -> io::Result<String> {
        let (_width, height) = terminal::size()?;
        let mut buf = String::new();
        loop {
            let line = format!("{}{}", prompt, buf);
            queue!(
                stdout,
                MoveTo(0, height.saturating_sub(1)),
                SetAttribute(Attribute::Reverse),
                Clear(ClearType::CurrentLine),
                Print(line),
                SetAttribute(Attribute::Reset),
                ResetColor
            )?;
            stdout.flush()?;

            let event = event::read()?;
            if let Event::Key(key) = event {
                match key {
                    KeyEvent {
                        code: KeyCode::Enter,
                        ..
                    } => break,
                    KeyEvent {
                        code: KeyCode::Esc, ..
                    } => {
                        buf.clear();
                        break;
                    }
                    KeyEvent {
                        code: KeyCode::Backspace,
                        ..
                    } => {
                        buf.pop();
                    }
                    KeyEvent {
                        code: KeyCode::Char(ch),
                        modifiers,
                        ..
                    } if modifiers.is_empty() || modifiers == KeyModifiers::SHIFT => {
                        if !ch.is_control() {
                            buf.push(ch);
                        }
                    }
                    _ => {}
                }
            }
        }
        self.status.clear();
        self.invalidate_frame();
        self.mark_dirty();
        Ok(buf)
    }

    fn search(&mut self, term: &str, forward: bool) -> io::Result<bool> {
        let lines = self.lines();
        self.ensure_layout()?;
        let current_logical = {
            let cache = self
                .layout_cache
                .as_ref()
                .expect("layout cache should exist after ensure_layout");
            if cache.display.is_empty() {
                0
            } else {
                cache.display[min(self.top, cache.display.len() - 1)].0
            }
        };
        let target = term.to_lowercase();
        let max_top = self.max_top()?;

        if forward {
            for idx in min(current_logical + 1, lines.len())..lines.len() {
                if lines[idx].to_lowercase().contains(&target) {
                    let mapped = self
                        .layout_cache
                        .as_ref()
                        .expect("layout cache should exist after ensure_layout")
                        .first_display_by_logical
                        .get(idx)
                        .copied()
                        .unwrap_or(0);
                    self.top = clamp(mapped, 0, max_top);
                    self.follow = false;
                    return Ok(true);
                }
            }
        } else {
            for idx in (0..min(current_logical, lines.len())).rev() {
                if lines[idx].to_lowercase().contains(&target) {
                    let mapped = self
                        .layout_cache
                        .as_ref()
                        .expect("layout cache should exist after ensure_layout")
                        .first_display_by_logical
                        .get(idx)
                        .copied()
                        .unwrap_or(0);
                    self.top = clamp(mapped, 0, max_top);
                    self.follow = false;
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    fn repeat_search(&mut self, forward: bool) -> io::Result<()> {
        let Some(term) = self.search_term.clone() else {
            self.status = "No previous search".to_string();
            self.mark_dirty();
            return Ok(());
        };
        self.last_search_forward = forward;
        if !self.search(&term, forward)? {
            self.status = format!("Pattern not found: {}", term);
        } else {
            self.status = format!("/{}", term);
        }
        self.mark_dirty();
        Ok(())
    }

    fn show_help(&mut self) {
        self.status = "j/k, C-e/C-n/C-y scroll, space/b/C-f/C-b page, d/u half-page, g/G home/end, / search, n/N repeat, F follow, q quit".to_string();
        self.mark_dirty();
    }

    fn handle_key(&mut self, stdout: &mut io::Stdout, key: KeyEvent) -> io::Result<bool> {
        match key {
            KeyEvent {
                code: KeyCode::Char('q' | 'Q'),
                ..
            } => return Ok(true),
            KeyEvent {
                code: KeyCode::Down | KeyCode::Enter,
                ..
            }
            | KeyEvent {
                code: KeyCode::Char('j'),
                modifiers: KeyModifiers::NONE,
                ..
            }
            | KeyEvent {
                code: KeyCode::Char('n'),
                modifiers: KeyModifiers::CONTROL,
                ..
            }
            | KeyEvent {
                code: KeyCode::Char('e'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => self.scroll(1)?,
            KeyEvent {
                code: KeyCode::Up, ..
            }
            | KeyEvent {
                code: KeyCode::Char('k'),
                modifiers: KeyModifiers::NONE,
                ..
            }
            | KeyEvent {
                code: KeyCode::Char('y'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => self.scroll(-1)?,
            KeyEvent {
                code: KeyCode::PageDown,
                ..
            }
            | KeyEvent {
                code: KeyCode::Char(' ' | 'f'),
                modifiers: KeyModifiers::NONE,
                ..
            }
            | KeyEvent {
                code: KeyCode::Char('f'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => self.page(1)?,
            KeyEvent {
                code: KeyCode::PageUp,
                ..
            }
            | KeyEvent {
                code: KeyCode::Char('b'),
                modifiers: KeyModifiers::NONE,
                ..
            }
            | KeyEvent {
                code: KeyCode::Char('b'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => self.page(-1)?,
            KeyEvent {
                code: KeyCode::Char('d'),
                modifiers: KeyModifiers::NONE,
                ..
            }
            | KeyEvent {
                code: KeyCode::Char('d'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => self.half_page(1)?,
            KeyEvent {
                code: KeyCode::Char('u'),
                modifiers: KeyModifiers::NONE,
                ..
            }
            | KeyEvent {
                code: KeyCode::Char('u'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => self.half_page(-1)?,
            KeyEvent {
                code: KeyCode::Home,
                ..
            }
            | KeyEvent {
                code: KeyCode::Char('g'),
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                self.follow = false;
                self.top = 0;
                self.status.clear();
                self.mark_dirty();
            }
            KeyEvent {
                code: KeyCode::End, ..
            }
            | KeyEvent {
                code: KeyCode::Char('G'),
                ..
            } => {
                self.follow = false;
                self.jump_to_end()?;
                self.status.clear();
            }
            KeyEvent {
                code: KeyCode::Char('F'),
                ..
            } => {
                self.follow = true;
                self.jump_to_end()?;
                self.status = "Follow mode".to_string();
            }
            KeyEvent {
                code: KeyCode::Char('c'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => {
                self.follow = false;
                self.status = "Follow stopped".to_string();
                self.mark_dirty();
            }
            KeyEvent {
                code: KeyCode::Char('r'),
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                self.replay.replay_all(&mut self.term)?;
                self.invalidate_layout();
                if self.follow {
                    self.jump_to_end()?;
                }
                self.status = "Reloaded".to_string();
                self.mark_dirty();
            }
            KeyEvent {
                code: KeyCode::Char('/'),
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                let term = self.prompt(stdout, "/")?;
                if !term.is_empty() {
                    self.search_term = Some(term.clone());
                    self.last_search_forward = true;
                    if !self.search(&term, true)? {
                        self.status = format!("Pattern not found: {}", term);
                    } else {
                        self.status = format!("/{}", term);
                    }
                } else {
                    self.status.clear();
                }
                self.mark_dirty();
            }
            KeyEvent {
                code: KeyCode::Char('n'),
                modifiers: KeyModifiers::NONE,
                ..
            } => self.repeat_search(self.last_search_forward)?,
            KeyEvent {
                code: KeyCode::Char('N'),
                ..
            } => self.repeat_search(!self.last_search_forward)?,
            KeyEvent {
                code: KeyCode::Char('?'),
                ..
            }
            | KeyEvent {
                code: KeyCode::Char('h'),
                modifiers: KeyModifiers::NONE,
                ..
            } => self.show_help(),
            _ => {}
        }
        Ok(false)
    }

    fn run(&mut self) -> io::Result<()> {
        let mut stdout = io::stdout();
        let _guard = TerminalGuard::enter(&mut stdout)?;

        loop {
            let changed = self.replay.poll(&mut self.term)?;
            if changed {
                self.invalidate_layout();
                self.mark_dirty();
            }
            if changed && self.follow {
                self.jump_to_end()?;
            }
            if self.needs_redraw {
                self.draw(&mut stdout)?;
                self.needs_redraw = false;
            }
            if !event::poll(Duration::from_millis(200))? {
                continue;
            }
            match event::read()? {
                Event::Resize(_, _) => {
                    self.rebuild_from_screen_size()?;
                    self.replay.replay_all(&mut self.term)?;
                    self.drop_layout_cache();
                    self.invalidate_frame();
                    if self.follow {
                        self.jump_to_end()?;
                    }
                    self.mark_dirty();
                }
                Event::Key(key) => {
                    if self.handle_key(&mut stdout, key)? {
                        break;
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }
}

struct TerminalGuard;

impl TerminalGuard {
    fn enter(stdout: &mut io::Stdout) -> io::Result<Self> {
        enable_raw_mode()?;
        execute!(stdout, EnterAlternateScreen, Hide)?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let mut stdout = io::stdout();
        let _ = execute!(
            stdout,
            Show,
            LeaveAlternateScreen,
            ResetColor,
            SetAttribute(Attribute::Reset)
        );
    }
}

fn full_layout(
    history: &[Vec<Cell>],
    screen_rows: &[Vec<Cell>],
    content_width: usize,
) -> (Vec<(usize, Vec<Cell>)>, Vec<usize>) {
    let logical = history
        .iter()
        .cloned()
        .chain(screen_rows.iter().cloned())
        .collect::<Vec<_>>();
    let mut display = Vec::new();
    let mut first_display_by_logical = Vec::new();
    for (idx, row) in logical.iter().enumerate() {
        first_display_by_logical.push(display.len());
        for segment in wrap_styled_line(row, content_width) {
            display.push((idx, segment));
        }
    }
    (display, first_display_by_logical)
}

fn apply_style(stdout: &mut io::Stdout, style: Style) -> io::Result<()> {
    queue!(stdout, SetAttribute(Attribute::Reset), ResetColor)?;
    if let Some(fg) = style.fg {
        queue!(stdout, SetForegroundColor(Color::AnsiValue(fg)))?;
    }
    if let Some(bg) = style.bg {
        queue!(stdout, SetBackgroundColor(Color::AnsiValue(bg)))?;
    }
    if style.bold {
        queue!(stdout, SetAttribute(Attribute::Bold))?;
    }
    if style.dim {
        queue!(stdout, SetAttribute(Attribute::Dim))?;
    }
    if style.reverse {
        queue!(stdout, SetAttribute(Attribute::Reverse))?;
    }
    Ok(())
}

fn clamp(value: usize, lower: usize, upper: usize) -> usize {
    value.max(lower).min(upper)
}

fn clamp_signed(value: isize, lower: isize, upper: isize) -> isize {
    value.max(lower).min(upper)
}

fn common_prefix_len(left: &[Cell], right: &[Cell]) -> usize {
    left.iter()
        .zip(right.iter())
        .take_while(|(a, b)| a == b)
        .count()
}

fn cell_prefix_width(cells: &[Cell], count: usize) -> usize {
    cells
        .iter()
        .take(count)
        .filter(|cell| !cell.wide_cont)
        .map(|cell| max(char_width(cell.ch), 1))
        .sum()
}

fn char_width(ch: char) -> usize {
    UnicodeWidthChar::width(ch).unwrap_or(0)
}

fn trim_row(row: &[Cell]) -> Vec<Cell> {
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

fn row_to_text(row: &[Cell]) -> String {
    trim_row(row)
        .iter()
        .filter(|cell| !cell.wide_cont)
        .map(|cell| cell.ch)
        .collect()
}

fn wrap_styled_line(line: &[Cell], width: usize) -> Vec<Vec<Cell>> {
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

fn valid_csi(seq: &str) -> bool {
    if seq.is_empty() {
        return false;
    }
    let bytes = seq.as_bytes();
    let final_byte = *bytes.last().unwrap_or(&0);
    if !(0x40..=0x7e).contains(&final_byte) {
        return false;
    }
    bytes[..bytes.len() - 1]
        .iter()
        .all(|byte| matches!(*byte, b'0'..=b'9' | b':' | b';' | b'<' | b'=' | b'>' | b'?' | b' ' ..= b'/'))
}

fn parse_param(value: &str) -> Option<usize> {
    let part = value.split(':').next().unwrap_or_default();
    if part.is_empty() {
        None
    } else {
        part.parse::<usize>().ok()
    }
}

fn param(params: &[Option<usize>], index: usize, default: usize) -> usize {
    match params.get(index).copied().flatten() {
        Some(0) | None => default,
        Some(value) => value,
    }
}

fn parse_sgr_int(value: &str) -> Option<u16> {
    let part = value.split(':').next().unwrap_or_default();
    if part.is_empty() {
        Some(0)
    } else {
        part.parse::<u16>().ok()
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

fn decode_utf8_chunk(pending: &mut Vec<u8>, bytes: &[u8], final_flush: bool) -> String {
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

fn dump_render(path: &Path, rows: usize, cols: usize, tail: Option<usize>) -> io::Result<()> {
    let mut term = VirtualTerminal::new(rows, cols);
    let mut replay = ReplayFile::new(path.to_path_buf());
    replay.replay_all(&mut term)?;
    let mut lines = term.rendered_lines();
    if let Some(tail) = tail {
        if tail < lines.len() {
            lines = lines.split_off(lines.len() - tail);
        }
    }
    for line in lines {
        println!("{}", line);
    }
    Ok(())
}

fn main() -> io::Result<()> {
    let args = Args::parse();
    if args.dump {
        let rows = args.rows.unwrap_or(200);
        let cols = args.cols.unwrap_or(120);
        return dump_render(&args.path, rows, cols, args.tail);
    }

    let mut app = ViewerApp::new(args.path.clone(), args)?;
    app.run()
}
