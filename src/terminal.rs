use std::cmp::{max, min};

use unicode_width::UnicodeWidthChar;

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
pub struct VirtualTerminal {
    rows: usize,
    cols: usize,
    history: Vec<Vec<Cell>>,
    current_style: Style,
    screen: Vec<Vec<Cell>>,
    cursor: Cursor,
    saved_cursor: Cursor,
    wrap_pending: bool,
    state: ParseState,
    csi: String,
    esc_other: String,
    osc: String,
    osc_escape: bool,
    sync_active: bool,
    line_dirty_min_col: Option<usize>,
}

impl VirtualTerminal {
    pub fn new(rows: usize, cols: usize) -> Self {
        let mut term = Self {
            rows: max(rows, 1),
            cols: max(cols, 1),
            history: Vec::new(),
            current_style: Style::default(),
            screen: Vec::new(),
            cursor: Cursor::default(),
            saved_cursor: Cursor::default(),
            wrap_pending: false,
            state: ParseState::Normal,
            csi: String::new(),
            esc_other: String::new(),
            osc: String::new(),
            osc_escape: false,
            sync_active: false,
            line_dirty_min_col: None,
        };
        term.reset(false);
        term
    }

    pub fn reset(&mut self, preserve_history: bool) {
        if !preserve_history {
            self.history.clear();
        }
        self.current_style = Style::default();
        self.screen = (0..self.rows).map(|_| self.blank_row()).collect();
        self.cursor = Cursor::default();
        self.saved_cursor = Cursor::default();
        self.wrap_pending = false;
        self.state = ParseState::Normal;
        self.csi.clear();
        self.esc_other.clear();
        self.osc.clear();
        self.osc_escape = false;
        self.sync_active = false;
        self.line_dirty_min_col = None;
    }

    pub fn reset_to_size(&mut self, rows: usize, cols: usize) {
        self.rows = max(rows, 1);
        self.cols = max(cols, 1);
        self.reset(false);
    }

    pub fn resize(&mut self, rows: usize, cols: usize) {
        let new_rows = max(rows, 1);
        let new_cols = max(cols, 1);

        if self.rows == new_rows && self.cols == new_cols {
            return;
        }

        for row in &mut self.history {
            normalize_row_width(row, new_cols, Style::default());
        }
        for row in &mut self.screen {
            normalize_row_width(row, new_cols, self.current_style);
        }

        if self.screen.len() > new_rows {
            let remove_count = self.screen.len() - new_rows;
            let removed = self.screen.drain(0..remove_count).collect::<Vec<_>>();
            self.history
                .extend(removed.into_iter().map(|row| trim_row(&row)));
        }

        self.rows = new_rows;
        self.cols = new_cols;

        while self.screen.len() < self.rows {
            self.screen.push(self.blank_row());
        }

        self.cursor.row = min(self.cursor.row, self.rows.saturating_sub(1));
        self.saved_cursor.row = min(self.saved_cursor.row, self.rows.saturating_sub(1));
        let max_col = self.cols.saturating_sub(1);
        let logical_cursor_col = if self.wrap_pending {
            self.cursor.col.saturating_add(1)
        } else {
            self.cursor.col
        };
        self.cursor.col = min(logical_cursor_col, max_col);
        self.saved_cursor.col = min(self.saved_cursor.col, max_col);
        self.wrap_pending = false;
        self.line_dirty_min_col = None;
    }

    pub fn feed(&mut self, text: &str) {
        for ch in text.chars() {
            self.feed_char(ch);
        }
    }

    pub fn rows(&self) -> usize {
        self.rows
    }

    pub fn cols(&self) -> usize {
        self.cols
    }

    pub fn history_len(&self) -> usize {
        self.history.len()
    }

    pub fn history_rows(&self) -> &[Vec<Cell>] {
        &self.history
    }

    pub fn screen_rows(&self) -> &[Vec<Cell>] {
        &self.screen
    }

    pub fn trimmed_screen_rows(&self) -> Vec<Vec<Cell>> {
        self.screen.iter().map(|row| trim_row(row)).collect()
    }

    pub fn rendered_rows(&self) -> Vec<Vec<Cell>> {
        let mut rows = Vec::with_capacity(self.history.len() + self.screen.len());
        rows.extend(self.history.iter().cloned());
        rows.extend(self.trimmed_screen_rows());
        rows
    }

    pub fn rendered_lines(&self) -> Vec<String> {
        let mut lines = Vec::with_capacity(self.history.len() + self.screen.len());
        lines.extend(self.history.iter().map(|row| row_to_text(row)));
        lines.extend(self.screen.iter().map(|row| row_to_text(row)));
        lines
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
            '\r' => {
                self.cancel_wrap_pending();
                self.cursor.col = 0;
            }
            '\n' => {
                self.cancel_wrap_pending();
                self.linefeed();
            }
            '\x08' => {
                self.cancel_wrap_pending();
                self.cursor.col = self.cursor.col.saturating_sub(1);
            }
            '\t' => {
                self.cancel_wrap_pending();
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
                self.cancel_wrap_pending();
                self.cursor = self.saved_cursor;
                self.state = ParseState::Normal;
            }
            'c' => {
                self.reset(true);
                self.state = ParseState::Normal;
            }
            'D' => {
                self.cancel_wrap_pending();
                self.linefeed();
                self.state = ParseState::Normal;
            }
            'E' => {
                self.cancel_wrap_pending();
                self.cursor.col = 0;
                self.linefeed();
                self.state = ParseState::Normal;
            }
            'M' => {
                self.cancel_wrap_pending();
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

        if should_cancel_wrap(final_char) {
            self.cancel_wrap_pending();
        }

        match final_char {
            'A' => self.cursor_up(param(&params, 0, 1)),
            'B' => self.cursor_down(param(&params, 0, 1), false),
            'C' => {
                self.cursor.col = min(
                    self.cols.saturating_sub(1),
                    self.cursor.col + param(&params, 0, 1),
                );
            }
            'D' => self.cursor.col = self.cursor.col.saturating_sub(param(&params, 0, 1)),
            'E' => {
                self.cursor_down(param(&params, 0, 1), false);
                self.cursor.col = 0;
            }
            'F' => {
                self.cursor_up(param(&params, 0, 1));
                self.cursor.col = 0;
            }
            'G' => {
                let col = param(&params, 0, 1).saturating_sub(1);
                self.cursor.col = min(self.cols.saturating_sub(1), col);
            }
            'H' | 'f' => {
                let row = param(&params, 0, 1).max(1) - 1;
                let col = param(&params, 1, 1).max(1) - 1;
                self.clear_line_dirty_if_row_changes(row);
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
            'h' | 'l' if private == '?' && params.iter().any(|param| *param == Some(2026)) => {
                self.sync_active = final_char == 'h';
            }
            'h' | 'l' => {}
            'n' if private == '?' => {}
            _ => {}
        }
    }

    fn blank_row(&self) -> Vec<Cell> {
        vec![Cell::blank(self.current_style); self.cols]
    }

    fn handle_sgr(&mut self, body: &str) {
        let parts = if body.is_empty() {
            vec!["0"]
        } else {
            body.split(';').collect::<Vec<_>>()
        };
        let mut fg = self.current_style.fg;
        let mut bg = self.current_style.bg;
        let mut bold = self.current_style.bold;
        let mut dim = self.current_style.dim;
        let mut reverse = self.current_style.reverse;
        let mut iter = parts.into_iter().peekable();
        while let Some(raw) = iter.next() {
            let Some(code) = parse_sgr_int(raw) else {
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
                30..=37 => fg = Some((code - 30) as u8),
                39 => fg = None,
                40..=47 => bg = Some((code - 40) as u8),
                49 => bg = None,
                90..=97 => fg = Some((code - 90 + 8) as u8),
                100..=107 => bg = Some((code - 100 + 8) as u8),
                38 | 48 => {
                    let target = if code == 38 { &mut fg } else { &mut bg };
                    match parse_sgr_int(iter.next().unwrap_or_default()) {
                        Some(5) => {
                            if let Some(value) =
                                iter.next().and_then(parse_sgr_int).map(|value| value as u8)
                            {
                                *target = Some(value);
                            }
                        }
                        Some(2) => {
                            let r = iter.next().and_then(parse_sgr_int);
                            let g = iter.next().and_then(parse_sgr_int);
                            let b = iter.next().and_then(parse_sgr_int);
                            if let (Some(r), Some(g), Some(b)) = (r, g, b) {
                                *target = Some(rgb_to_xterm256(r as u8, g as u8, b as u8));
                            }
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
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
        if self.wrap_pending {
            self.linefeed();
            self.cursor.col = 0;
            self.wrap_pending = false;
        }
        let width = max(char_width(ch), 1);
        if width > self.cols {
            return;
        }
        if self.cursor.col >= self.cols {
            self.cursor.col = self.cols.saturating_sub(1);
        }
        if width == 2 && self.cursor.col + width > self.cols {
            self.cursor.col = 0;
            self.linefeed();
        }
        if self.cursor.row >= self.rows {
            self.cursor.row = self.rows.saturating_sub(1);
        }
        self.mark_line_dirty(self.cursor.col);
        let row = &mut self.screen[self.cursor.row];
        row[self.cursor.col] = Cell {
            ch,
            style: self.current_style,
            wide_cont: false,
        };
        if width == 2 && self.cursor.col + 1 < self.cols {
            row[self.cursor.col + 1] = Cell {
                ch: ' ',
                style: self.current_style,
                wide_cont: true,
            };
        }
        let next_col = self.cursor.col + width;
        if next_col >= self.cols {
            self.cursor.col = self.cols.saturating_sub(1);
            self.wrap_pending = true;
        } else {
            self.cursor.col = next_col;
            self.wrap_pending = false;
        }
    }

    fn linefeed(&mut self) {
        if self.cursor.row + 1 < self.rows {
            self.cursor.row += 1;
        } else if self.should_use_sync_status_scroll_region() {
            self.scroll_sync_status_area_up();
        } else {
            self.scroll_up(1);
        }
        self.line_dirty_min_col = None;
    }

    fn reverse_index(&mut self) {
        self.line_dirty_min_col = None;
        if self.cursor.row > 0 {
            self.cursor.row -= 1;
        } else {
            self.scroll_down(1);
        }
    }

    fn cursor_up(&mut self, amount: usize) {
        if amount > 0 {
            self.line_dirty_min_col = None;
        }
        self.cursor.row = self.cursor.row.saturating_sub(amount);
    }

    fn cursor_down(&mut self, amount: usize, keep_col: bool) {
        if amount > 0 {
            self.line_dirty_min_col = None;
        }
        self.cursor.row = min(self.rows.saturating_sub(1), self.cursor.row + amount);
        if !keep_col {
            self.cursor.col = min(self.cursor.col, self.cols.saturating_sub(1));
        }
    }

    fn scroll_up(&mut self, amount: usize) {
        for _ in 0..amount {
            if !self.screen.is_empty() {
                let first = self.screen.remove(0);
                self.history.push(trim_row(&first));
                self.screen.push(self.blank_row());
            }
        }
    }

    fn scroll_down(&mut self, amount: usize) {
        for _ in 0..amount {
            if !self.screen.is_empty() {
                self.screen.pop();
                self.screen.insert(0, self.blank_row());
            }
        }
    }

    fn scroll_sync_status_area_up(&mut self) {
        // Claude's synchronized bottom-right status updates can issue LF from
        // the bottom row while expecting only the status area to move.
        let start = self.rows.saturating_sub(3);
        if start + 1 >= self.rows {
            return;
        }
        for row in start..self.rows - 1 {
            self.screen[row] = self.screen[row + 1].clone();
        }
        self.screen[self.rows - 1] = self.blank_row();
    }

    fn erase_in_line(&mut self, mode: usize) {
        let dirty_col = match mode {
            0 => self.cursor.col,
            1 | 2 => 0,
            _ => 0,
        };
        self.mark_line_dirty(dirty_col);
        let row = &mut self.screen[self.cursor.row];
        match mode {
            0 => {
                for cell in row.iter_mut().skip(self.cursor.col) {
                    *cell = Cell::blank(self.current_style);
                }
            }
            1 => {
                for cell in row.iter_mut().take(self.cursor.col + 1) {
                    *cell = Cell::blank(self.current_style);
                }
            }
            _ => {
                for cell in row.iter_mut() {
                    *cell = Cell::blank(self.current_style);
                }
            }
        }
    }

    fn erase_in_display(&mut self, mode: usize) {
        match mode {
            0 => {
                self.erase_in_line(0);
                for row in self.screen.iter_mut().skip(self.cursor.row + 1) {
                    for cell in row.iter_mut() {
                        *cell = Cell::blank(self.current_style);
                    }
                }
            }
            1 => {
                self.erase_in_line(1);
                for row in self.screen.iter_mut().take(self.cursor.row) {
                    for cell in row.iter_mut() {
                        *cell = Cell::blank(self.current_style);
                    }
                }
            }
            2 | 3 => {
                self.screen = (0..self.rows).map(|_| self.blank_row()).collect();
            }
            _ => {
                self.screen = (0..self.rows).map(|_| self.blank_row()).collect();
            }
        }
    }

    fn insert_lines(&mut self, amount: usize) {
        self.line_dirty_min_col = None;
        let amount = min(amount, self.rows.saturating_sub(self.cursor.row));
        for _ in 0..amount {
            self.screen.insert(self.cursor.row, self.blank_row());
            self.screen.pop();
        }
    }

    fn delete_lines(&mut self, amount: usize) {
        self.line_dirty_min_col = None;
        let amount = min(amount, self.rows.saturating_sub(self.cursor.row));
        for _ in 0..amount {
            self.screen.remove(self.cursor.row);
            self.screen.push(self.blank_row());
        }
    }

    fn insert_chars(&mut self, amount: usize) {
        self.mark_line_dirty(self.cursor.col);
        let row = &mut self.screen[self.cursor.row];
        for _ in 0..amount {
            row.insert(self.cursor.col, Cell::blank(self.current_style));
            row.pop();
        }
    }

    fn delete_chars(&mut self, amount: usize) {
        self.mark_line_dirty(self.cursor.col);
        let row = &mut self.screen[self.cursor.row];
        let amount = min(amount, self.cols.saturating_sub(self.cursor.col));
        for _ in 0..amount {
            row.remove(self.cursor.col);
            row.push(Cell::blank(self.current_style));
        }
    }

    fn erase_chars(&mut self, amount: usize) {
        self.mark_line_dirty(self.cursor.col);
        let end = min(self.cols, self.cursor.col + amount);
        let row = &mut self.screen[self.cursor.row];
        for cell in row.iter_mut().take(end).skip(self.cursor.col) {
            *cell = Cell::blank(self.current_style);
        }
    }

    fn cancel_wrap_pending(&mut self) {
        if self.wrap_pending {
            self.wrap_pending = false;
            self.cursor.col = min(self.cursor.col, self.cols.saturating_sub(1));
        }
    }

    fn mark_line_dirty(&mut self, col: usize) {
        self.line_dirty_min_col = Some(
            self.line_dirty_min_col
                .map_or(col, |current| min(current, col)),
        );
    }

    fn clear_line_dirty_if_row_changes(&mut self, row: usize) {
        if row != self.cursor.row {
            self.line_dirty_min_col = None;
        }
    }

    fn should_use_sync_status_scroll_region(&self) -> bool {
        let Some(min_col) = self.line_dirty_min_col else {
            return false;
        };
        self.sync_active && self.cursor.row + 1 >= self.rows && min_col >= self.cols / 2
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

fn normalize_row_width(row: &mut Vec<Cell>, cols: usize, blank_style: Style) {
    if row.len() > cols {
        row.truncate(cols);
    } else if row.len() < cols {
        row.resize(cols, Cell::blank(blank_style));
    }

    if row.is_empty() {
        return;
    }

    let last_idx = row.len() - 1;
    let last = row[last_idx];
    if last.wide_cont || (!last.wide_cont && char_width(last.ch) > 1) {
        row[last_idx] = Cell::blank(blank_style);
    }
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

fn should_cancel_wrap(final_char: char) -> bool {
    matches!(
        final_char,
        'A' | 'B'
            | 'C'
            | 'D'
            | 'E'
            | 'F'
            | 'G'
            | 'H'
            | 'f'
            | 'J'
            | 'K'
            | 'L'
            | 'M'
            | '@'
            | 'P'
            | 'X'
            | 'S'
            | 'T'
            | 's'
            | 'u'
    )
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
    fn synchronized_right_status_scroll_does_not_shift_main_content() {
        let mut term = VirtualTerminal::new(6, 20);
        term.feed("\x1b[1;1Htop");
        term.feed("\x1b[4;1Hseparator");
        term.feed("\x1b[5;1Haccept");
        term.feed("\x1b[3;3H");
        term.feed(
            "\x1b[?2026h\x1b[3B\r\x1b[10C\x1b[1Acheck\r\r\n\
             \x1b[10Cnew\r\r\n\x1b[2C\x1b[4A\x1b[?2026l",
        );

        let lines = term.rendered_lines();
        assert_eq!(lines[0], "top");
        assert_eq!(lines[3], "accept    check");
        assert_eq!(lines[4], "          new");
    }
}
