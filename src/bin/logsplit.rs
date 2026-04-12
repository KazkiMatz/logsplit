use std::cmp::max;
use std::env;
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use clap::Parser;
use crossterm::SynchronizedUpdate;
use crossterm::cursor::MoveTo;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::queue;
use crossterm::style::{Attribute, Print, ResetColor, SetAttribute};
use crossterm::terminal;
use logsplit_rs::{
    Cell, Selection, SelectionMode, SelectionPoint, Style, TerminalGuard, ViewerCore,
    VirtualTerminal, WordMotion, apply_selection_highlight, clear_segment, copy_to_clipboard,
    debug_log, draw_cells, first_selectable_col, last_selectable_col, move_word_point, next_col,
    normalize_col, overlay_cells, paste_from_clipboard, previous_col, row_to_text, selection_text,
};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

#[derive(Debug, Parser)]
#[command(about = "Run a command with live PTY logging and a side-by-side log viewer")]
struct Args {
    #[arg(long, value_name = "PATH")]
    shell: Option<PathBuf>,

    #[arg(required = true, trailing_var_arg = true)]
    line: Vec<String>,
}

#[derive(Debug, Clone)]
struct FrameSnapshot {
    width: u16,
    height: u16,
    separator_col: usize,
    rows: Vec<Vec<Cell>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Left,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ActiveSelection {
    pane: Focus,
    selection: Selection,
}

#[derive(Debug)]
enum PaneMsg {
    Data(Vec<u8>),
    Eof,
}

#[derive(Debug)]
enum AppEvent {
    Terminal(Event),
    Pane(PaneMsg),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScreenPosition {
    Top,
    Middle,
    Bottom,
}

struct Pane {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send>,
    term: VirtualTerminal,
    pending_utf8: Vec<u8>,
    alive: bool,
}

impl Pane {
    fn handle_msg(&mut self, msg: PaneMsg) -> bool {
        match msg {
            PaneMsg::Data(bytes) => {
                let text = logsplit_rs::decode_utf8_chunk(&mut self.pending_utf8, &bytes, false);
                if !text.is_empty() {
                    self.term.feed(&text);
                    true
                } else {
                    false
                }
            }
            PaneMsg::Eof => {
                let tail = logsplit_rs::decode_utf8_chunk(&mut self.pending_utf8, &[], true);
                if !tail.is_empty() {
                    self.term.feed(&tail);
                }
                self.alive = false;
                !tail.is_empty()
            }
        }
    }

    fn resize(&mut self, rows: usize, cols: usize) -> io::Result<()> {
        let size = PtySize {
            rows: rows as u16,
            cols: cols as u16,
            pixel_width: 0,
            pixel_height: 0,
        };
        self.master.resize(size).map_err(anyerr)?;
        self.term.resize(rows, cols);
        Ok(())
    }

    fn write_input(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.writer.write_all(bytes)?;
        self.writer.flush()
    }

    fn exited(&mut self) -> io::Result<bool> {
        if !self.alive {
            return Ok(true);
        }
        match self.child.try_wait() {
            Ok(Some(_status)) => self.alive = false,
            Ok(None) => {}
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {}
            Err(err) => return Err(err),
        }
        Ok(!self.alive)
    }
}

struct App {
    left: ViewerCore,
    right: Pane,
    events_rx: Receiver<AppEvent>,
    events_tx: Sender<AppEvent>,
    focus: Focus,
    prefix_pending: bool,
    switch_prefix_pending: bool,
    left_cursor: SelectionPoint,
    selection: Option<ActiveSelection>,
    logfile: PathBuf,
    previous_frame: Option<FrameSnapshot>,
}

impl App {
    fn new(line: String, shell: PathBuf) -> io::Result<Self> {
        debug_log(&format!("App::new line={line} shell={}", shell.display()));
        let dims = split_dims()?;
        debug_log(&format!(
            "split_dims rows={} left_cols={} right_cols={}",
            dims.rows, dims.left_cols, dims.right_cols
        ));
        let logfile = make_logfile_path(&line)?;
        debug_log(&format!("logfile={}", logfile.display()));
        if let Some(parent) = logfile.parent() {
            fs::create_dir_all(parent)?;
            debug_log(&format!("created log dir {}", parent.display()));
        }
        let _ = File::create(&logfile)?;
        debug_log("created empty logfile");

        let mut left = ViewerCore::new(logfile.clone(), dims.rows, dims.right_cols, true)?;
        debug_log("ViewerCore::new ok");
        left.jump_to_end(dims.rows, dims.left_cols)?;
        debug_log("left.jump_to_end ok");
        let (events_tx, events_rx) = mpsc::channel();
        let right = spawn_logged_command(
            &line,
            &shell,
            &logfile,
            dims.rows,
            dims.right_cols,
            events_tx.clone(),
        )?;
        debug_log("spawn_logged_command ok");

        let mut app = Self {
            left,
            right,
            events_rx,
            events_tx,
            focus: Focus::Right,
            prefix_pending: false,
            switch_prefix_pending: false,
            left_cursor: SelectionPoint { row: 0, col: 0 },
            selection: None,
            logfile,
            previous_frame: None,
        };
        app.left_cursor = app.default_left_cursor(&dims)?;
        Ok(app)
    }

    fn run(&mut self) -> io::Result<()> {
        let mut stdout = io::stdout();
        debug_log("about to enter TerminalGuard");
        let _guard = TerminalGuard::enter(&mut stdout)?;
        debug_log("TerminalGuard entered");
        spawn_input_reader(self.events_tx.clone());
        let mut needs_redraw = true;

        loop {
            if needs_redraw {
                self.draw(&mut stdout)?;
                needs_redraw = false;
            }

            let mut pending = vec![self.next_app_event()?];
            while let Ok(app_event) = self.events_rx.try_recv() {
                pending.push(app_event);
            }

            let mut pane_changed = false;
            let mut saw_pane = false;
            let mut should_break = false;

            for app_event in pending {
                match app_event {
                    AppEvent::Pane(msg) => {
                        pane_changed |= self.right.handle_msg(msg);
                        saw_pane = true;
                    }
                    AppEvent::Terminal(event) => {
                        if saw_pane {
                            pane_changed |= self.sync_left_from_log()?;
                            saw_pane = false;
                        }
                        if pane_changed {
                            needs_redraw = true;
                            pane_changed = false;
                        }
                        if self.handle_terminal_event(&mut stdout, event)? {
                            should_break = true;
                            break;
                        }
                        needs_redraw = true;
                    }
                }
            }

            if saw_pane {
                pane_changed |= self.sync_left_from_log()?;
            }
            if pane_changed {
                needs_redraw = true;
            }

            if should_break || self.right.exited()? {
                debug_log("right pane exited; leaving run loop");
                break;
            }
        }

        Ok(())
    }

    fn next_app_event(&self) -> io::Result<AppEvent> {
        self.events_rx
            .recv()
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "app event channel closed"))
    }

    fn sync_left_from_log(&mut self) -> io::Result<bool> {
        let dims = split_dims()?;
        let changed = self.left.poll(dims.rows, dims.left_cols)?;
        if self.left.follow {
            self.snap_left_cursor_to_end(&dims)?;
        } else {
            self.normalize_left_cursor(&dims)?;
        }
        Ok(changed)
    }

    fn handle_resize(&mut self) -> io::Result<()> {
        let dims = split_dims()?;
        self.left
            .resize_source(dims.rows, dims.right_cols, dims.rows, dims.left_cols)?;
        if self.left.follow {
            self.left.jump_to_end(dims.rows, dims.left_cols)?;
            self.snap_left_cursor_to_end(&dims)?;
        } else {
            self.normalize_left_cursor(&dims)?;
            self.ensure_left_selection_visible(&dims, self.left_cursor.row);
        }
        self.right.resize(dims.rows, dims.right_cols)?;
        if self.selection.take().is_some() {
            self.left.status = "Selection cleared after resize".to_string();
        }
        self.previous_frame = None;
        Ok(())
    }

    fn handle_terminal_event(&mut self, stdout: &mut io::Stdout, event: Event) -> io::Result<bool> {
        match event {
            Event::Resize(_, _) => {
                self.handle_resize()?;
                Ok(false)
            }
            Event::Key(key) => self.handle_key(stdout, key),
            _ => Ok(false),
        }
    }

    fn handle_prompt_event(&mut self, app_event: AppEvent) -> io::Result<Option<KeyEvent>> {
        match app_event {
            AppEvent::Pane(msg) => {
                let _ = self.right.handle_msg(msg);
                let _ = self.sync_left_from_log()?;
                Ok(None)
            }
            AppEvent::Terminal(Event::Resize(_, _)) => {
                self.handle_resize()?;
                Ok(None)
            }
            AppEvent::Terminal(Event::Key(key)) => Ok(Some(key)),
            AppEvent::Terminal(_) => Ok(None),
        }
    }

    fn draw(&mut self, stdout: &mut io::Stdout) -> io::Result<()> {
        let dims = split_dims()?;
        let frame = self.build_frame(&dims)?;
        stdout.sync_update(|stdout| -> io::Result<()> {
            self.draw_frame_diff(stdout, &frame)?;
            Ok(())
        })??;
        self.previous_frame = Some(frame);
        Ok(())
    }

    fn build_frame(&mut self, dims: &SplitDims) -> io::Result<FrameSnapshot> {
        let total_width = dims.left_cols + 1 + dims.right_cols;
        let mut left_rows = self.left.visible_rows(dims.rows, dims.left_cols)?;
        if let Some(selection) = self
            .selection
            .filter(|selection| selection.pane == Focus::Left)
        {
            for (offset, row) in left_rows.iter_mut().enumerate() {
                apply_selection_highlight(row, self.left.top + offset, &selection.selection);
            }
        }
        let left_cursor = self.normalize_left_cursor(dims)?;
        if left_cursor.row >= self.left.top {
            let visible_offset = left_cursor.row - self.left.top;
            if let Some(row) = left_rows.get_mut(visible_offset) {
                apply_cursor_highlight(row, left_cursor.col, self.focus == Focus::Left);
            }
        }
        let status_override = self.status_override_text();
        let status_text = self.left.status_text_with_override(
            dims.rows,
            dims.left_cols,
            dims.left_cols,
            status_override.as_deref(),
        )?;
        let left_status = reverse_status_cells(&status_text, dims.left_cols);
        let blank = Cell::blank(Style::default());
        let mut rows = Vec::with_capacity(dims.rows);
        let active_right_selection = self
            .selection
            .filter(|selection| selection.pane == Focus::Right);

        for y in 0..dims.rows {
            let mut row = vec![blank; total_width];
            if let Some(left_row) = left_rows.get(y) {
                overlay_cells(&mut row, 0, left_row, dims.left_cols);
            }
            if y == dims.rows.saturating_sub(1) {
                overlay_cells(&mut row, 0, &left_status, dims.left_cols);
            }
            row[dims.separator_col] = self.separator_cell(y);
            if let Some(right_row) = self.right.term.screen_rows().get(y) {
                let mut rendered = right_row.clone();
                if let Some(selection) = active_right_selection {
                    apply_selection_highlight(&mut rendered, y, &selection.selection);
                }
                overlay_cells(&mut row, dims.separator_col + 1, &rendered, dims.right_cols);
            }
            rows.push(row);
        }

        Ok(FrameSnapshot {
            width: total_width as u16,
            height: dims.rows as u16,
            separator_col: dims.separator_col,
            rows,
        })
    }

    fn separator_cell(&self, y: usize) -> Cell {
        let ch = if y == 0 {
            if self.switch_prefix_pending {
                'w'
            } else if self.prefix_pending {
                '*'
            } else {
                match self.focus {
                    Focus::Left => '<',
                    Focus::Right => '>',
                }
            }
        } else {
            '|'
        };
        let fg = match self.focus {
            Focus::Left => 11,
            Focus::Right => 14,
        };
        Cell {
            ch,
            style: Style {
                fg: Some(fg),
                ..Style::default()
            },
            wide_cont: false,
        }
    }

    fn draw_frame_diff(&self, stdout: &mut io::Stdout, frame: &FrameSnapshot) -> io::Result<()> {
        let full_redraw = self.previous_frame.as_ref().is_none_or(|previous| {
            previous.width != frame.width
                || previous.height != frame.height
                || previous.separator_col != frame.separator_col
                || previous.rows.len() != frame.rows.len()
        });
        let separator_col = frame.separator_col;
        let right_offset = separator_col + 1;
        let right_width = frame.width as usize - right_offset;

        for (y, row) in frame.rows.iter().enumerate() {
            let previous_row = if full_redraw {
                None
            } else {
                self.previous_frame
                    .as_ref()
                    .and_then(|previous| previous.rows.get(y))
            };

            self.draw_segment_diff(
                stdout,
                y as u16,
                0,
                previous_row.map(|row| &row[..separator_col]),
                &row[..separator_col],
            )?;
            self.draw_segment_diff(
                stdout,
                y as u16,
                separator_col,
                previous_row.map(|row| &row[separator_col..separator_col + 1]),
                &row[separator_col..separator_col + 1],
            )?;
            self.draw_segment_diff(
                stdout,
                y as u16,
                right_offset,
                previous_row.map(|row| &row[right_offset..]),
                &row[right_offset..right_offset + right_width],
            )?;
        }
        Ok(())
    }

    fn draw_segment_diff(
        &self,
        stdout: &mut io::Stdout,
        y: u16,
        x_offset: usize,
        previous: Option<&[Cell]>,
        cells: &[Cell],
    ) -> io::Result<()> {
        if previous == Some(cells) {
            return Ok(());
        }
        clear_segment(stdout, y, x_offset, cells.len())?;
        draw_cells(stdout, y, x_offset, cells, x_offset + cells.len())?;
        Ok(())
    }

    fn prompt_left(&mut self, stdout: &mut io::Stdout, prompt: &str) -> io::Result<String> {
        let mut buf = String::new();
        loop {
            let dims = split_dims()?;
            let width = dims.left_cols;
            let mut line = format!("{}{}", prompt, buf);
            let line_len = line.chars().count();
            if line_len < width {
                line.push_str(&" ".repeat(width - line_len));
            }
            let line: String = line.chars().take(width).collect();
            queue!(
                stdout,
                MoveTo(0, dims.rows.saturating_sub(1) as u16),
                SetAttribute(Attribute::Reset),
                ResetColor,
                SetAttribute(Attribute::Reverse),
                Print(line.as_str()),
                SetAttribute(Attribute::Reset),
                ResetColor
            )?;
            stdout.flush()?;

            if let Some(key) = self.handle_prompt_event(self.next_app_event()?)? {
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
        self.left.status.clear();
        self.previous_frame = None;
        Ok(buf)
    }

    fn show_left_help(&mut self) {
        self.left.status = "h/j/k/l move cursor, w/e/b words, C-n and arrows move, C-e/C-y scroll view, space/f/C-f page down, C-b page up, d/u half-page, 0/$ line, g/G file, H/M/L screen, / search, n/N repeat, v/V visual select, Ctrl-w h/l switch pane, p paste to right, ? help, Ctrl-g v/V/p/q extra actions".to_string();
    }

    fn handle_left_key(&mut self, stdout: &mut io::Stdout, key: KeyEvent) -> io::Result<()> {
        match key {
            KeyEvent {
                code: KeyCode::Char('v'),
                modifiers: KeyModifiers::NONE,
                ..
            } => self.start_selection(SelectionMode::Character)?,
            KeyEvent {
                code: KeyCode::Char('V'),
                ..
            } => self.start_selection(SelectionMode::Line)?,
            KeyEvent {
                code: KeyCode::Char('p' | 'P'),
                modifiers: KeyModifiers::NONE,
                ..
            } => self.paste_right_from_clipboard(),
            KeyEvent {
                code: KeyCode::Char('q' | 'Q'),
                ..
            } => {
                self.left.status = "Use Ctrl-g q to quit logsplit".to_string();
            }
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
            } => self.move_left_cursor_row(1)?,
            KeyEvent {
                code: KeyCode::Char('e'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => self.scroll_left_view_preserve_cursor(1)?,
            KeyEvent {
                code: KeyCode::Up, ..
            }
            | KeyEvent {
                code: KeyCode::Char('k'),
                modifiers: KeyModifiers::NONE,
                ..
            } => self.move_left_cursor_row(-1)?,
            KeyEvent {
                code: KeyCode::Char('y'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => self.scroll_left_view_preserve_cursor(-1)?,
            KeyEvent {
                code: KeyCode::Left,
                ..
            }
            | KeyEvent {
                code: KeyCode::Char('h'),
                modifiers: KeyModifiers::NONE,
                ..
            } => self.move_left_cursor_col(false)?,
            KeyEvent {
                code: KeyCode::Right,
                ..
            }
            | KeyEvent {
                code: KeyCode::Char('l'),
                modifiers: KeyModifiers::NONE,
                ..
            } => self.move_left_cursor_col(true)?,
            KeyEvent {
                code: KeyCode::Char('w'),
                modifiers: KeyModifiers::NONE,
                ..
            } => self.move_left_cursor_word(WordMotion::ForwardStart)?,
            KeyEvent {
                code: KeyCode::Char('e'),
                modifiers: KeyModifiers::NONE,
                ..
            } => self.move_left_cursor_word(WordMotion::ForwardEnd)?,
            KeyEvent {
                code: KeyCode::Char('b'),
                modifiers: KeyModifiers::NONE,
                ..
            } => self.move_left_cursor_word(WordMotion::BackwardStart)?,
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
            } => self.move_left_cursor_page(1)?,
            KeyEvent {
                code: KeyCode::PageUp,
                ..
            }
            | KeyEvent {
                code: KeyCode::Char('b'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => self.move_left_cursor_page(-1)?,
            KeyEvent {
                code: KeyCode::Char('d'),
                modifiers: KeyModifiers::NONE,
                ..
            }
            | KeyEvent {
                code: KeyCode::Char('d'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => self.move_left_cursor_half_page(1)?,
            KeyEvent {
                code: KeyCode::Char('u'),
                modifiers: KeyModifiers::NONE,
                ..
            }
            | KeyEvent {
                code: KeyCode::Char('u'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => self.move_left_cursor_half_page(-1)?,
            KeyEvent {
                code: KeyCode::Char('0'),
                modifiers: KeyModifiers::NONE,
                ..
            } => self.move_left_cursor_line_edge(false)?,
            KeyEvent {
                code: KeyCode::Char('$'),
                ..
            } => self.move_left_cursor_line_edge(true)?,
            KeyEvent {
                code: KeyCode::Home,
                ..
            }
            | KeyEvent {
                code: KeyCode::Char('g'),
                modifiers: KeyModifiers::NONE,
                ..
            } => self.jump_left_cursor_file_edge(false)?,
            KeyEvent {
                code: KeyCode::End, ..
            }
            | KeyEvent {
                code: KeyCode::Char('G'),
                ..
            } => self.jump_left_cursor_file_edge(true)?,
            KeyEvent {
                code: KeyCode::Char('H'),
                ..
            } => self.move_left_cursor_screen_position(ScreenPosition::Top)?,
            KeyEvent {
                code: KeyCode::Char('M'),
                ..
            } => self.move_left_cursor_screen_position(ScreenPosition::Middle)?,
            KeyEvent {
                code: KeyCode::Char('L'),
                ..
            } => self.move_left_cursor_screen_position(ScreenPosition::Bottom)?,
            KeyEvent {
                code: KeyCode::Char('F'),
                ..
            } => {
                let dims = split_dims()?;
                self.left.follow = true;
                self.left.jump_to_end(dims.rows, dims.left_cols)?;
                self.snap_left_cursor_to_end(&dims)?;
                self.left.status = "Follow mode".to_string();
            }
            KeyEvent {
                code: KeyCode::Char('c'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => {
                self.left.follow = false;
                self.left.status = "Follow stopped".to_string();
            }
            KeyEvent {
                code: KeyCode::Char('r'),
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                let dims = split_dims()?;
                self.left
                    .resize_source(dims.rows, dims.right_cols, dims.rows, dims.left_cols)?;
                if self.left.follow {
                    self.left.jump_to_end(dims.rows, dims.left_cols)?;
                    self.snap_left_cursor_to_end(&dims)?;
                } else {
                    self.normalize_left_cursor(&dims)?;
                    self.ensure_left_selection_visible(&dims, self.left_cursor.row);
                }
                self.left.status = "Reloaded".to_string();
                self.previous_frame = None;
            }
            KeyEvent {
                code: KeyCode::Char('/'),
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                let dims = split_dims()?;
                let term = self.prompt_left(stdout, "/")?;
                if !term.is_empty() {
                    self.left.search_term = Some(term.clone());
                    self.left.last_search_forward = true;
                    if !self.left.search(&term, true, dims.rows, dims.left_cols)? {
                        self.left.status = format!("Pattern not found: {}", term);
                    } else {
                        self.left_cursor = SelectionPoint {
                            row: self.left.top,
                            col: 0,
                        };
                        self.normalize_left_cursor(&dims)?;
                        self.left.status = format!("/{}", term);
                    }
                } else {
                    self.left.status.clear();
                }
            }
            KeyEvent {
                code: KeyCode::Char('n'),
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                let dims = split_dims()?;
                self.left.repeat_search(
                    self.left.last_search_forward,
                    dims.rows,
                    dims.left_cols,
                )?;
                self.left_cursor = SelectionPoint {
                    row: self.left.top,
                    col: 0,
                };
                self.normalize_left_cursor(&dims)?;
            }
            KeyEvent {
                code: KeyCode::Char('N'),
                ..
            } => {
                let dims = split_dims()?;
                self.left.repeat_search(
                    !self.left.last_search_forward,
                    dims.rows,
                    dims.left_cols,
                )?;
                self.left_cursor = SelectionPoint {
                    row: self.left.top,
                    col: 0,
                };
                self.normalize_left_cursor(&dims)?;
            }
            KeyEvent {
                code: KeyCode::Char('?'),
                ..
            } => self.show_left_help(),
            _ => {}
        }
        Ok(())
    }

    fn start_selection(&mut self, mode: SelectionMode) -> io::Result<()> {
        let dims = split_dims()?;
        let point = match self.focus {
            Focus::Left => {
                self.left.follow = false;
                self.left_selection_start(&dims)?
            }
            Focus::Right => self.right_selection_start(),
        };
        self.selection = Some(ActiveSelection {
            pane: self.focus,
            selection: Selection::new(mode, point),
        });
        self.left.status.clear();
        Ok(())
    }

    fn default_left_cursor(&mut self, dims: &SplitDims) -> io::Result<SelectionPoint> {
        let total_rows = self.left.display_len(dims.left_cols)?;
        if total_rows == 0 {
            return Ok(SelectionPoint { row: 0, col: 0 });
        }
        let visible_height = ViewerCore::content_height(dims.rows);
        let visible_end = (self.left.top + visible_height).min(total_rows);
        let mut row_index = self.left.top.min(total_rows.saturating_sub(1));
        for candidate in (self.left.top..visible_end).rev() {
            if self
                .left
                .display_row(candidate, dims.left_cols)?
                .is_some_and(|row| !row.is_empty())
            {
                row_index = candidate;
                break;
            }
        }
        let row = self
            .left
            .display_row(row_index, dims.left_cols)?
            .unwrap_or_default();
        Ok(SelectionPoint {
            row: row_index,
            col: first_selectable_col(&row),
        })
    }

    fn normalize_left_cursor(&mut self, dims: &SplitDims) -> io::Result<SelectionPoint> {
        let total_rows = self.left.display_len(dims.left_cols)?;
        if total_rows == 0 {
            self.left_cursor = SelectionPoint { row: 0, col: 0 };
            return Ok(self.left_cursor);
        }
        self.left_cursor.row = self.left_cursor.row.min(total_rows.saturating_sub(1));
        let row = self
            .left
            .display_row(self.left_cursor.row, dims.left_cols)?
            .unwrap_or_default();
        self.left_cursor.col = if row.is_empty() {
            0
        } else {
            normalize_col(&row, self.left_cursor.col)
        };
        Ok(self.left_cursor)
    }

    fn left_selection_start(&mut self, dims: &SplitDims) -> io::Result<SelectionPoint> {
        self.normalize_left_cursor(dims)
    }

    fn right_selection_start(&self) -> SelectionPoint {
        let rows = self.right.term.screen_rows();
        let mut row_index = rows.len().saturating_sub(1);
        for candidate in (0..rows.len()).rev() {
            if !row_to_text(&rows[candidate]).is_empty() {
                row_index = candidate;
                break;
            }
        }
        let row = rows.get(row_index).cloned().unwrap_or_default();
        SelectionPoint {
            row: row_index,
            col: first_selectable_col(&row),
        }
    }

    fn ensure_left_selection_visible(&mut self, dims: &SplitDims, row_index: usize) {
        let visible_height = ViewerCore::content_height(dims.rows);
        if row_index < self.left.top {
            self.left.top = row_index;
        } else if visible_height > 0 && row_index >= self.left.top + visible_height {
            self.left.top = row_index.saturating_sub(visible_height.saturating_sub(1));
        }
    }

    fn snap_left_cursor_to_end(&mut self, dims: &SplitDims) -> io::Result<()> {
        let total_rows = self.left.display_len(dims.left_cols)?;
        if total_rows == 0 {
            self.left_cursor = SelectionPoint { row: 0, col: 0 };
            return Ok(());
        }
        self.left_cursor.row = total_rows - 1;
        let row = self
            .left
            .display_row(self.left_cursor.row, dims.left_cols)?
            .unwrap_or_default();
        self.left_cursor.col = if row.is_empty() {
            0
        } else {
            first_selectable_col(&row)
        };
        Ok(())
    }

    fn move_left_cursor_row(&mut self, amount: isize) -> io::Result<()> {
        let dims = split_dims()?;
        self.left.follow = false;
        let total_rows = self.left.display_len(dims.left_cols)?;
        if total_rows == 0 {
            self.left_cursor = SelectionPoint { row: 0, col: 0 };
            return Ok(());
        }
        self.normalize_left_cursor(&dims)?;
        let max_row = total_rows.saturating_sub(1) as isize;
        self.left_cursor.row = (self.left_cursor.row as isize + amount).clamp(0, max_row) as usize;
        let row = self
            .left
            .display_row(self.left_cursor.row, dims.left_cols)?
            .unwrap_or_default();
        self.left_cursor.col = if row.is_empty() {
            0
        } else {
            normalize_col(&row, self.left_cursor.col)
        };
        self.ensure_left_selection_visible(&dims, self.left_cursor.row);
        self.left.status.clear();
        Ok(())
    }

    fn move_left_cursor_col(&mut self, forward: bool) -> io::Result<()> {
        let dims = split_dims()?;
        self.left.follow = false;
        let row_index = self.normalize_left_cursor(&dims)?.row;
        let row = self
            .left
            .display_row(row_index, dims.left_cols)?
            .unwrap_or_default();
        self.left_cursor.col = if row.is_empty() {
            0
        } else if forward {
            next_col(&row, self.left_cursor.col)
        } else {
            previous_col(&row, self.left_cursor.col)
        };
        self.left.status.clear();
        Ok(())
    }

    fn move_left_cursor_page(&mut self, amount: isize) -> io::Result<()> {
        let dims = split_dims()?;
        let step = ViewerCore::content_height(dims.rows).max(1) as isize;
        self.move_left_cursor_row(amount * step)
    }

    fn move_left_cursor_half_page(&mut self, amount: isize) -> io::Result<()> {
        let dims = split_dims()?;
        let step = (ViewerCore::content_height(dims.rows) / 2).max(1) as isize;
        self.move_left_cursor_row(amount * step)
    }

    fn move_left_cursor_line_edge(&mut self, to_end: bool) -> io::Result<()> {
        let dims = split_dims()?;
        self.left.follow = false;
        let row_index = self.normalize_left_cursor(&dims)?.row;
        let row = self
            .left
            .display_row(row_index, dims.left_cols)?
            .unwrap_or_default();
        self.left_cursor.col = if row.is_empty() {
            0
        } else if to_end {
            last_selectable_col(&row)
        } else {
            0
        };
        self.left.status.clear();
        Ok(())
    }

    fn move_left_cursor_word(&mut self, motion: WordMotion) -> io::Result<()> {
        let dims = split_dims()?;
        self.left.follow = false;
        let total_rows = self.left.display_len(dims.left_cols)?;
        if total_rows == 0 {
            self.left_cursor = SelectionPoint { row: 0, col: 0 };
            return Ok(());
        }
        let start = self.normalize_left_cursor(&dims)?;
        self.left_cursor = move_word_point(
            start,
            total_rows,
            |row| self.left.display_row(row, dims.left_cols),
            motion,
        )?;
        self.ensure_left_selection_visible(&dims, self.left_cursor.row);
        self.left.status.clear();
        Ok(())
    }

    fn scroll_left_view_preserve_cursor(&mut self, amount: isize) -> io::Result<()> {
        let dims = split_dims()?;
        self.left.follow = false;
        let total_rows = self.left.display_len(dims.left_cols)?;
        if total_rows == 0 {
            self.left_cursor = SelectionPoint { row: 0, col: 0 };
            return Ok(());
        }
        self.normalize_left_cursor(&dims)?;
        let visible_height = ViewerCore::content_height(dims.rows);
        let max_top = total_rows.saturating_sub(visible_height);
        let old_top = self.left.top as isize;
        let new_top = (old_top + amount).clamp(0, max_top as isize) as usize;
        self.left.top = new_top;
        if visible_height > 0 {
            if self.left_cursor.row < self.left.top {
                self.left_cursor.row = self.left.top;
            } else {
                let visible_end = self.left.top + visible_height;
                if self.left_cursor.row >= visible_end {
                    self.left_cursor.row = visible_end.saturating_sub(1);
                }
            }
        }
        let row = self
            .left
            .display_row(self.left_cursor.row, dims.left_cols)?
            .unwrap_or_default();
        self.left_cursor.col = if row.is_empty() {
            0
        } else {
            normalize_col(&row, self.left_cursor.col)
        };
        self.left.status.clear();
        Ok(())
    }

    fn move_left_cursor_screen_position(&mut self, target: ScreenPosition) -> io::Result<()> {
        let dims = split_dims()?;
        self.left.follow = false;
        let total_rows = self.left.display_len(dims.left_cols)?;
        if total_rows == 0 {
            self.left_cursor = SelectionPoint { row: 0, col: 0 };
            return Ok(());
        }
        let visible_height = ViewerCore::content_height(dims.rows);
        let visible_end = (self.left.top + visible_height).min(total_rows);
        if visible_end <= self.left.top {
            return Ok(());
        }
        let visible_count = visible_end - self.left.top;
        self.left_cursor.row = match target {
            ScreenPosition::Top => self.left.top,
            ScreenPosition::Middle => self.left.top + visible_count.saturating_sub(1) / 2,
            ScreenPosition::Bottom => visible_end - 1,
        };
        let row = self
            .left
            .display_row(self.left_cursor.row, dims.left_cols)?
            .unwrap_or_default();
        self.left_cursor.col = if row.is_empty() {
            0
        } else {
            first_selectable_col(&row)
        };
        self.left.status.clear();
        Ok(())
    }

    fn jump_left_cursor_file_edge(&mut self, to_end: bool) -> io::Result<()> {
        let dims = split_dims()?;
        self.left.follow = false;
        let total_rows = self.left.display_len(dims.left_cols)?;
        if total_rows == 0 {
            self.left_cursor = SelectionPoint { row: 0, col: 0 };
            return Ok(());
        }
        self.left_cursor.row = if to_end { total_rows - 1 } else { 0 };
        let row = self
            .left
            .display_row(self.left_cursor.row, dims.left_cols)?
            .unwrap_or_default();
        self.left_cursor.col = if row.is_empty() {
            0
        } else if to_end {
            last_selectable_col(&row)
        } else {
            first_selectable_col(&row)
        };
        self.ensure_left_selection_visible(&dims, self.left_cursor.row);
        self.left.status.clear();
        Ok(())
    }

    fn clear_switch_prefix(&mut self) {
        self.switch_prefix_pending = false;
    }

    fn move_selection_row(&mut self, amount: isize) -> io::Result<()> {
        let dims = split_dims()?;
        let mut active = match self.selection {
            Some(selection) => selection,
            None => return Ok(()),
        };
        match active.pane {
            Focus::Left => {
                let total_rows = self.left.display_len(dims.left_cols)?;
                if total_rows == 0 {
                    return Ok(());
                }
                let max_row = total_rows.saturating_sub(1) as isize;
                let new_row =
                    (active.selection.cursor.row as isize + amount).clamp(0, max_row) as usize;
                let row = self
                    .left
                    .display_row(new_row, dims.left_cols)?
                    .unwrap_or_default();
                active.selection.cursor = SelectionPoint {
                    row: new_row,
                    col: if row.is_empty() {
                        0
                    } else {
                        normalize_col(&row, active.selection.cursor.col)
                    },
                };
                self.left_cursor = active.selection.cursor;
                self.ensure_left_selection_visible(&dims, new_row);
            }
            Focus::Right => {
                let rows = self.right.term.screen_rows();
                if rows.is_empty() {
                    return Ok(());
                }
                let max_row = rows.len().saturating_sub(1) as isize;
                let new_row =
                    (active.selection.cursor.row as isize + amount).clamp(0, max_row) as usize;
                let row = rows.get(new_row).cloned().unwrap_or_default();
                active.selection.cursor = SelectionPoint {
                    row: new_row,
                    col: if row.is_empty() {
                        0
                    } else {
                        normalize_col(&row, active.selection.cursor.col)
                    },
                };
            }
        }
        self.selection = Some(active);
        Ok(())
    }

    fn move_selection_col(&mut self, forward: bool) -> io::Result<()> {
        let dims = split_dims()?;
        let mut active = match self.selection {
            Some(selection) => selection,
            None => return Ok(()),
        };
        let row = match active.pane {
            Focus::Left => self
                .left
                .display_row(active.selection.cursor.row, dims.left_cols)?
                .unwrap_or_default(),
            Focus::Right => self
                .right
                .term
                .screen_rows()
                .get(active.selection.cursor.row)
                .cloned()
                .unwrap_or_default(),
        };
        active.selection.cursor.col = if row.is_empty() {
            0
        } else if forward {
            next_col(&row, active.selection.cursor.col)
        } else {
            previous_col(&row, active.selection.cursor.col)
        };
        if active.pane == Focus::Left {
            self.left_cursor = active.selection.cursor;
        }
        self.selection = Some(active);
        Ok(())
    }

    fn move_selection_line_edge(&mut self, to_end: bool) -> io::Result<()> {
        let dims = split_dims()?;
        let mut active = match self.selection {
            Some(selection) => selection,
            None => return Ok(()),
        };
        let row = match active.pane {
            Focus::Left => self
                .left
                .display_row(active.selection.cursor.row, dims.left_cols)?
                .unwrap_or_default(),
            Focus::Right => self
                .right
                .term
                .screen_rows()
                .get(active.selection.cursor.row)
                .cloned()
                .unwrap_or_default(),
        };
        active.selection.cursor.col = if row.is_empty() {
            0
        } else if to_end {
            last_selectable_col(&row)
        } else {
            0
        };
        if active.pane == Focus::Left {
            self.left_cursor = active.selection.cursor;
        }
        self.selection = Some(active);
        Ok(())
    }

    fn move_selection_word(&mut self, motion: WordMotion) -> io::Result<()> {
        let dims = split_dims()?;
        let mut active = match self.selection {
            Some(selection) => selection,
            None => return Ok(()),
        };
        match active.pane {
            Focus::Left => {
                let total_rows = self.left.display_len(dims.left_cols)?;
                if total_rows == 0 {
                    return Ok(());
                }
                active.selection.cursor = move_word_point(
                    active.selection.cursor,
                    total_rows,
                    |row| self.left.display_row(row, dims.left_cols),
                    motion,
                )?;
                self.left_cursor = active.selection.cursor;
                self.ensure_left_selection_visible(&dims, active.selection.cursor.row);
            }
            Focus::Right => {
                let total_rows = self.right.term.screen_rows().len();
                if total_rows == 0 {
                    return Ok(());
                }
                active.selection.cursor = move_word_point(
                    active.selection.cursor,
                    total_rows,
                    |row| Ok(self.right.term.screen_rows().get(row).cloned()),
                    motion,
                )?;
            }
        }
        self.selection = Some(active);
        Ok(())
    }

    fn jump_selection_file_edge(&mut self, to_end: bool) -> io::Result<()> {
        let dims = split_dims()?;
        let mut active = match self.selection {
            Some(selection) => selection,
            None => return Ok(()),
        };
        match active.pane {
            Focus::Left => {
                let total_rows = self.left.display_len(dims.left_cols)?;
                if total_rows == 0 {
                    return Ok(());
                }
                let target_row = if to_end { total_rows - 1 } else { 0 };
                let row = self
                    .left
                    .display_row(target_row, dims.left_cols)?
                    .unwrap_or_default();
                active.selection.cursor = SelectionPoint {
                    row: target_row,
                    col: if row.is_empty() {
                        0
                    } else if to_end {
                        last_selectable_col(&row)
                    } else {
                        first_selectable_col(&row)
                    },
                };
                self.left_cursor = active.selection.cursor;
                self.ensure_left_selection_visible(&dims, target_row);
            }
            Focus::Right => {
                let rows = self.right.term.screen_rows();
                if rows.is_empty() {
                    return Ok(());
                }
                let target_row = if to_end { rows.len() - 1 } else { 0 };
                let row = rows.get(target_row).cloned().unwrap_or_default();
                active.selection.cursor = SelectionPoint {
                    row: target_row,
                    col: if row.is_empty() {
                        0
                    } else if to_end {
                        last_selectable_col(&row)
                    } else {
                        first_selectable_col(&row)
                    },
                };
            }
        }
        self.selection = Some(active);
        Ok(())
    }

    fn move_selection_screen_position(&mut self, target: ScreenPosition) -> io::Result<()> {
        let dims = split_dims()?;
        let mut active = match self.selection {
            Some(selection) => selection,
            None => return Ok(()),
        };
        match active.pane {
            Focus::Left => {
                let total_rows = self.left.display_len(dims.left_cols)?;
                if total_rows == 0 {
                    return Ok(());
                }
                let visible_height = ViewerCore::content_height(dims.rows);
                let visible_end = (self.left.top + visible_height).min(total_rows);
                if visible_end <= self.left.top {
                    return Ok(());
                }
                let visible_count = visible_end - self.left.top;
                let target_row = match target {
                    ScreenPosition::Top => self.left.top,
                    ScreenPosition::Middle => self.left.top + visible_count.saturating_sub(1) / 2,
                    ScreenPosition::Bottom => visible_end - 1,
                };
                let row = self
                    .left
                    .display_row(target_row, dims.left_cols)?
                    .unwrap_or_default();
                active.selection.cursor = SelectionPoint {
                    row: target_row,
                    col: if row.is_empty() {
                        0
                    } else {
                        first_selectable_col(&row)
                    },
                };
                self.left_cursor = active.selection.cursor;
            }
            Focus::Right => {
                let rows = self.right.term.screen_rows();
                if rows.is_empty() {
                    return Ok(());
                }
                let target_row = match target {
                    ScreenPosition::Top => 0,
                    ScreenPosition::Middle => rows.len().saturating_sub(1) / 2,
                    ScreenPosition::Bottom => rows.len() - 1,
                };
                let row = rows.get(target_row).cloned().unwrap_or_default();
                active.selection.cursor = SelectionPoint {
                    row: target_row,
                    col: if row.is_empty() {
                        0
                    } else {
                        first_selectable_col(&row)
                    },
                };
            }
        }
        self.selection = Some(active);
        Ok(())
    }

    fn scroll_selection_preserve_cursor(&mut self, amount: isize) -> io::Result<()> {
        let Some(active) = self.selection else {
            return Ok(());
        };
        match active.pane {
            Focus::Left => {
                self.scroll_left_view_preserve_cursor(amount)?;
                if let Some(mut updated) = self.selection {
                    updated.selection.cursor = self.left_cursor;
                    self.selection = Some(updated);
                }
                Ok(())
            }
            Focus::Right => self.move_selection_row(amount),
        }
    }

    fn cancel_selection(&mut self, message: Option<&str>) {
        self.selection = None;
        if let Some(message) = message {
            self.left.status = message.to_string();
        } else {
            self.left.status.clear();
        }
    }

    fn copy_selection(&mut self) -> io::Result<()> {
        let dims = split_dims()?;
        let Some(active) = self.selection else {
            return Ok(());
        };
        let text = match active.pane {
            Focus::Left => selection_text(active.selection, |row| {
                self.left.display_row(row, dims.left_cols).ok().flatten()
            }),
            Focus::Right => {
                let rows = self.right.term.screen_rows();
                selection_text(active.selection, |row| rows.get(row).cloned())
            }
        };
        match copy_to_clipboard(&text) {
            Ok(()) => {
                self.selection = None;
                self.left.status = format!(
                    "Copied {} displayed line{} from {} pane",
                    active.selection.line_span(),
                    if active.selection.line_span() == 1 {
                        ""
                    } else {
                        "s"
                    },
                    match active.pane {
                        Focus::Left => "left",
                        Focus::Right => "right",
                    }
                );
            }
            Err(err) => {
                self.left.status = format!("Clipboard copy failed: {}", err);
            }
        }
        Ok(())
    }

    fn paste_right_from_clipboard(&mut self) {
        match paste_from_clipboard() {
            Ok(text) => {
                if text.is_empty() {
                    self.left.status = "Clipboard is empty".to_string();
                    return;
                }
                if !self.right.alive {
                    self.left.status = "Right pane is not running".to_string();
                    return;
                }
                match self.right.write_input(text.as_bytes()) {
                    Ok(()) => {
                        self.left.status = format!(
                            "Pasted {} byte{} into right pane",
                            text.len(),
                            if text.len() == 1 { "" } else { "s" }
                        );
                    }
                    Err(err) => {
                        self.left.status = format!("Paste failed: {}", err);
                    }
                }
            }
            Err(err) => {
                self.left.status = format!("Clipboard paste failed: {}", err);
            }
        }
    }

    fn handle_selection_key(&mut self, key: KeyEvent) -> io::Result<()> {
        match key {
            KeyEvent {
                code: KeyCode::Esc, ..
            } => self.cancel_selection(Some("Selection cleared")),
            KeyEvent {
                code: KeyCode::Char('y'),
                modifiers: KeyModifiers::NONE,
                ..
            }
            | KeyEvent {
                code: KeyCode::Char('Y'),
                ..
            } => {
                self.copy_selection()?;
            }
            KeyEvent {
                code: KeyCode::Down,
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
            } => self.move_selection_row(1)?,
            KeyEvent {
                code: KeyCode::Char('e'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => self.scroll_selection_preserve_cursor(1)?,
            KeyEvent {
                code: KeyCode::Up, ..
            }
            | KeyEvent {
                code: KeyCode::Char('k'),
                modifiers: KeyModifiers::NONE,
                ..
            } => self.move_selection_row(-1)?,
            KeyEvent {
                code: KeyCode::Char('y'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => self.scroll_selection_preserve_cursor(-1)?,
            KeyEvent {
                code: KeyCode::Left,
                ..
            }
            | KeyEvent {
                code: KeyCode::Char('h'),
                modifiers: KeyModifiers::NONE,
                ..
            } => self.move_selection_col(false)?,
            KeyEvent {
                code: KeyCode::Right,
                ..
            }
            | KeyEvent {
                code: KeyCode::Char('l'),
                modifiers: KeyModifiers::NONE,
                ..
            } => self.move_selection_col(true)?,
            KeyEvent {
                code: KeyCode::Char('w'),
                modifiers: KeyModifiers::NONE,
                ..
            } => self.move_selection_word(WordMotion::ForwardStart)?,
            KeyEvent {
                code: KeyCode::Char('e'),
                modifiers: KeyModifiers::NONE,
                ..
            } => self.move_selection_word(WordMotion::ForwardEnd)?,
            KeyEvent {
                code: KeyCode::Char('b'),
                modifiers: KeyModifiers::NONE,
                ..
            } => self.move_selection_word(WordMotion::BackwardStart)?,
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
            } => self.move_selection_row(ViewerCore::content_height(split_dims()?.rows) as isize)?,
            KeyEvent {
                code: KeyCode::PageUp,
                ..
            }
            | KeyEvent {
                code: KeyCode::Char('b'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => {
                self.move_selection_row(-(ViewerCore::content_height(split_dims()?.rows) as isize))?
            }
            KeyEvent {
                code: KeyCode::Char('d'),
                modifiers: KeyModifiers::NONE,
                ..
            }
            | KeyEvent {
                code: KeyCode::Char('d'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => self.move_selection_row(
                (ViewerCore::content_height(split_dims()?.rows) / 2).max(1) as isize,
            )?,
            KeyEvent {
                code: KeyCode::Char('u'),
                modifiers: KeyModifiers::NONE,
                ..
            }
            | KeyEvent {
                code: KeyCode::Char('u'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => self.move_selection_row(
                -((ViewerCore::content_height(split_dims()?.rows) / 2).max(1) as isize),
            )?,
            KeyEvent {
                code: KeyCode::Char('0'),
                modifiers: KeyModifiers::NONE,
                ..
            } => self.move_selection_line_edge(false)?,
            KeyEvent {
                code: KeyCode::Char('$'),
                ..
            } => self.move_selection_line_edge(true)?,
            KeyEvent {
                code: KeyCode::Home,
                ..
            }
            | KeyEvent {
                code: KeyCode::Char('g'),
                modifiers: KeyModifiers::NONE,
                ..
            } => self.jump_selection_file_edge(false)?,
            KeyEvent {
                code: KeyCode::End, ..
            }
            | KeyEvent {
                code: KeyCode::Char('G'),
                ..
            } => self.jump_selection_file_edge(true)?,
            KeyEvent {
                code: KeyCode::Char('H'),
                ..
            } => self.move_selection_screen_position(ScreenPosition::Top)?,
            KeyEvent {
                code: KeyCode::Char('M'),
                ..
            } => self.move_selection_screen_position(ScreenPosition::Middle)?,
            KeyEvent {
                code: KeyCode::Char('L'),
                ..
            } => self.move_selection_screen_position(ScreenPosition::Bottom)?,
            _ => {}
        }
        Ok(())
    }

    fn status_override_text(&self) -> Option<String> {
        self.selection.map(|active| {
            format!(
                "{} {} pane  h/j/k/l move  w/e/b word  H/M/L screen  y copy  Esc cancel",
                selection_label(active.selection.mode),
                match active.pane {
                    Focus::Left => "left",
                    Focus::Right => "right",
                }
            )
        })
    }

    fn handle_key(&mut self, stdout: &mut io::Stdout, key: KeyEvent) -> io::Result<bool> {
        if self.switch_prefix_pending {
            self.clear_switch_prefix();
            return match key {
                KeyEvent {
                    code: KeyCode::Char('h'),
                    modifiers: KeyModifiers::NONE,
                    ..
                } => {
                    self.selection = None;
                    self.focus = Focus::Left;
                    Ok(false)
                }
                KeyEvent {
                    code: KeyCode::Char('l'),
                    modifiers: KeyModifiers::NONE,
                    ..
                } => {
                    self.selection = None;
                    self.focus = Focus::Right;
                    Ok(false)
                }
                KeyEvent {
                    code: KeyCode::Esc, ..
                } => Ok(false),
                _ => {
                    self.left.status = "Ctrl-w h/l switches panes".to_string();
                    Ok(false)
                }
            };
        }

        if self.prefix_pending {
            self.prefix_pending = false;
            return match key {
                KeyEvent {
                    code: KeyCode::Char('v'),
                    modifiers: KeyModifiers::NONE,
                    ..
                } => {
                    self.start_selection(SelectionMode::Character)?;
                    Ok(false)
                }
                KeyEvent {
                    code: KeyCode::Char('V'),
                    ..
                } => {
                    self.start_selection(SelectionMode::Line)?;
                    Ok(false)
                }
                KeyEvent {
                    code: KeyCode::Char('p' | 'P'),
                    ..
                } => {
                    self.paste_right_from_clipboard();
                    Ok(false)
                }
                KeyEvent {
                    code: KeyCode::Char('q'),
                    ..
                } => Ok(true),
                _ => Ok(false),
            };
        }

        if matches!(
            key,
            KeyEvent {
                code: KeyCode::Char('w'),
                modifiers: KeyModifiers::CONTROL,
                ..
            }
        ) {
            self.switch_prefix_pending = true;
            return Ok(false);
        }

        if matches!(
            key,
            KeyEvent {
                code: KeyCode::Char('g'),
                modifiers: KeyModifiers::CONTROL,
                ..
            }
        ) {
            self.prefix_pending = true;
            return Ok(false);
        }

        if self.selection.is_some() {
            self.handle_selection_key(key)?;
            return Ok(false);
        }

        match self.focus {
            Focus::Left => self.handle_left_key(stdout, key)?,
            Focus::Right => {
                if let Some(bytes) = encode_key(key) {
                    if self.right.alive {
                        self.right.write_input(&bytes)?;
                    }
                }
            }
        }
        Ok(false)
    }
}

#[derive(Debug, Clone, Copy)]
struct SplitDims {
    rows: usize,
    left_cols: usize,
    right_cols: usize,
    separator_col: usize,
}

fn split_dims() -> io::Result<SplitDims> {
    let (width, height) = terminal::size()?;
    let width = max(width as usize, 3);
    let rows = max(height as usize, 1);
    let separator_col = width / 2;
    let left_cols = max(separator_col, 1);
    let right_cols = max(width.saturating_sub(left_cols + 1), 1);
    Ok(SplitDims {
        rows,
        left_cols,
        right_cols,
        separator_col: left_cols,
    })
}

fn spawn_reader(mut reader: Box<dyn Read + Send>, mut logfile: File, tx: Sender<AppEvent>) {
    thread::spawn(move || {
        let mut buf = [0u8; 65536];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => {
                    debug_log("spawn_reader: EOF");
                    let _ = tx.send(AppEvent::Pane(PaneMsg::Eof));
                    break;
                }
                Ok(count) => {
                    if let Err(err) = logfile.write_all(&buf[..count]) {
                        debug_log(&format!("spawn_reader: logfile write error: {err}"));
                    }
                    if tx
                        .send(AppEvent::Pane(PaneMsg::Data(buf[..count].to_vec())))
                        .is_err()
                    {
                        debug_log("spawn_reader: receiver dropped");
                        break;
                    }
                }
                Err(_) => {
                    debug_log("spawn_reader: read error");
                    let _ = tx.send(AppEvent::Pane(PaneMsg::Eof));
                    break;
                }
            }
        }
    });
}

fn spawn_input_reader(tx: Sender<AppEvent>) {
    thread::spawn(move || {
        loop {
            match event::read() {
                Ok(event @ Event::Key(_)) | Ok(event @ Event::Resize(_, _)) => {
                    if tx.send(AppEvent::Terminal(event)).is_err() {
                        debug_log("spawn_input_reader: receiver dropped");
                        break;
                    }
                }
                Ok(_) => {}
                Err(err) => {
                    debug_log(&format!("spawn_input_reader: read error: {err}"));
                    break;
                }
            }
        }
    });
}

fn spawn_logged_command(
    line: &str,
    shell: &Path,
    logfile: &Path,
    rows: usize,
    cols: usize,
    tx: Sender<AppEvent>,
) -> io::Result<Pane> {
    debug_log("spawn_logged_command: start");
    let pty_system = native_pty_system();
    debug_log("spawn_logged_command: got native_pty_system");
    let pair = pty_system
        .openpty(PtySize {
            rows: rows as u16,
            cols: cols as u16,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(anyerr)?;
    debug_log("spawn_logged_command: openpty ok");
    let shell_program = shell.to_string_lossy().into_owned();
    let mut cmd = CommandBuilder::new(&shell_program);
    cmd.arg("-lc");
    cmd.arg(line);
    cmd.cwd(env::current_dir()?);
    cmd.env("TERM", "xterm-256color");
    cmd.env("SCRIPT", logfile.as_os_str());
    debug_log("spawn_logged_command: command configured");
    let child = pair.slave.spawn_command(cmd).map_err(anyerr)?;
    debug_log("spawn_logged_command: spawn_command ok");
    let reader = pair.master.try_clone_reader().map_err(anyerr)?;
    debug_log("spawn_logged_command: try_clone_reader ok");
    let writer = pair.master.take_writer().map_err(anyerr)?;
    debug_log("spawn_logged_command: take_writer ok");
    let logfile_writer = File::options().append(true).open(logfile)?;
    spawn_reader(reader, logfile_writer, tx);
    debug_log("spawn_logged_command: reader thread spawned");
    Ok(Pane {
        master: pair.master,
        writer,
        child,
        term: VirtualTerminal::new(rows, cols),
        pending_utf8: Vec::new(),
        alive: true,
    })
}

fn sanitize_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        "cmd".to_string()
    } else {
        out
    }
}

fn command_slug(line: &str) -> String {
    let first = line.split_whitespace().next().unwrap_or("cmd");
    let first = Path::new(first)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or(first);
    sanitize_component(first)
}

fn make_logfile_path(line: &str) -> io::Result<PathBuf> {
    let home = env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is not set"))?;
    let logdir = home.join(".logsplit");
    let win = env::var("WINDOW").unwrap_or_else(|_| "noscreen".to_string());
    let sty = sanitize_component(&env::var("STY").unwrap_or_else(|_| "nosession".to_string()));
    let slug = command_slug(line);
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|err| io::Error::other(err.to_string()))?
        .as_secs();
    Ok(logdir.join(format!("{slug}-{sty}-w{win}-{ts}.log")))
}

fn encode_key(key: KeyEvent) -> Option<Vec<u8>> {
    match key.code {
        KeyCode::Char(ch) => {
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                if ch == ' ' {
                    return Some(vec![0]);
                }
                let lower = ch.to_ascii_lowercase() as u8;
                if lower.is_ascii_lowercase() {
                    return Some(vec![lower - b'a' + 1]);
                }
            }
            let mut out = Vec::new();
            if key.modifiers.contains(KeyModifiers::ALT) {
                out.push(0x1b);
            }
            let mut buf = [0u8; 4];
            out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
            Some(out)
        }
        KeyCode::Enter => Some(vec![b'\r']),
        KeyCode::Backspace => Some(vec![0x7f]),
        KeyCode::Tab => Some(vec![b'\t']),
        KeyCode::BackTab => Some(b"\x1b[Z".to_vec()),
        KeyCode::Esc => Some(vec![0x1b]),
        KeyCode::Left => Some(b"\x1b[D".to_vec()),
        KeyCode::Right => Some(b"\x1b[C".to_vec()),
        KeyCode::Up => Some(b"\x1b[A".to_vec()),
        KeyCode::Down => Some(b"\x1b[B".to_vec()),
        KeyCode::Home => Some(b"\x1b[H".to_vec()),
        KeyCode::End => Some(b"\x1b[F".to_vec()),
        KeyCode::PageUp => Some(b"\x1b[5~".to_vec()),
        KeyCode::PageDown => Some(b"\x1b[6~".to_vec()),
        KeyCode::Delete => Some(b"\x1b[3~".to_vec()),
        KeyCode::Insert => Some(b"\x1b[2~".to_vec()),
        _ => None,
    }
}

fn reverse_status_cells(text: &str, width: usize) -> Vec<Cell> {
    let status_style = Style {
        reverse: true,
        ..Style::default()
    };
    let mut cells = Vec::with_capacity(width);
    for ch in text.chars().take(width) {
        cells.push(Cell {
            ch,
            style: status_style,
            wide_cont: false,
        });
    }
    while cells.len() < width {
        cells.push(Cell::blank(status_style));
    }
    cells
}

fn apply_cursor_highlight(row: &mut Vec<Cell>, col: usize, focused: bool) {
    if row.is_empty() {
        row.push(Cell::blank(Style::default()));
    }
    let idx = normalize_col(row, col);
    let style = Style {
        bg: Some(if focused { 245 } else { 240 }),
        bold: focused,
        dim: false,
        reverse: false,
        ..row[idx].style
    };
    row[idx].style = style;
    if idx + 1 < row.len() && row[idx + 1].wide_cont {
        row[idx + 1].style = style;
    }
}

fn anyerr(err: anyhow::Error) -> io::Error {
    io::Error::other(err.to_string())
}

fn resolve_shell_path(args_shell: Option<&Path>) -> PathBuf {
    resolve_shell_path_from(
        args_shell,
        env::var_os("LOGSPLIT_SHELL").as_deref(),
        env::var_os("SHELL").as_deref(),
    )
}

fn resolve_shell_path_from(
    args_shell: Option<&Path>,
    logsplit_shell: Option<&OsStr>,
    shell: Option<&OsStr>,
) -> PathBuf {
    if let Some(path) = args_shell.filter(|path| !path.as_os_str().is_empty()) {
        return path.to_path_buf();
    }
    if let Some(value) = logsplit_shell.filter(|value| !value.is_empty()) {
        return PathBuf::from(value);
    }
    if let Some(value) = shell.filter(|value| !value.is_empty()) {
        return PathBuf::from(value);
    }
    PathBuf::from("/bin/sh")
}

fn main() -> io::Result<()> {
    let args = Args::parse();
    let shell = resolve_shell_path(args.shell.as_deref());
    let line = args.line.join(" ");
    debug_log(&format!(
        "main: starting line={line} shell={}",
        shell.display()
    ));
    let mut app = App::new(line, shell)?;
    let logfile = app.logfile.clone();
    let result = app.run();
    debug_log(&format!("main: run finished result={}", result.is_ok()));
    eprintln!("logsplit: log saved to {}", logfile.display());
    result
}

fn selection_label(mode: SelectionMode) -> &'static str {
    match mode {
        SelectionMode::Character => "VISUAL",
        SelectionMode::Line => "V-LINE",
    }
}

#[cfg(test)]
mod tests {
    use super::resolve_shell_path_from;
    use std::ffi::OsStr;
    use std::path::{Path, PathBuf};

    #[test]
    fn resolve_shell_prefers_explicit_arg() {
        let shell = resolve_shell_path_from(
            Some(Path::new("/bin/bash")),
            Some(OsStr::new("/bin/fish")),
            Some(OsStr::new("/bin/zsh")),
        );
        assert_eq!(shell, PathBuf::from("/bin/bash"));
    }

    #[test]
    fn resolve_shell_prefers_logsplit_env_over_shell_env() {
        let shell = resolve_shell_path_from(
            None,
            Some(OsStr::new("/bin/fish")),
            Some(OsStr::new("/bin/zsh")),
        );
        assert_eq!(shell, PathBuf::from("/bin/fish"));
    }

    #[test]
    fn resolve_shell_uses_shell_env_when_override_missing() {
        let shell = resolve_shell_path_from(None, None, Some(OsStr::new("/bin/zsh")));
        assert_eq!(shell, PathBuf::from("/bin/zsh"));
    }

    #[test]
    fn resolve_shell_falls_back_to_bin_sh() {
        let shell = resolve_shell_path_from(None, None, None);
        assert_eq!(shell, PathBuf::from("/bin/sh"));
    }
}
