use std::cmp::max;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, SystemTime};

use clap::Parser;
use crossterm::SynchronizedUpdate;
use crossterm::cursor::MoveTo;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::queue;
use crossterm::style::{Attribute, Print, ResetColor, SetAttribute};
use crossterm::terminal::{self, Clear, ClearType};
use logsplit_rs::{
    Cell, ReplayFile, Selection, SelectionMode, SelectionPoint, TerminalGuard, ViewerCore,
    VirtualTerminal, apply_selection_highlight, cell_prefix_width, common_prefix_len,
    copy_to_clipboard, draw_cells, first_selectable_col, last_selectable_col, next_col,
    normalize_col, previous_col, resize_events_path, selection_text,
};

#[derive(Debug, Parser, Clone)]
#[command(about = "less-like viewer for terminal transcript logs")]
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

#[derive(Debug, Clone)]
struct FrameSnapshot {
    width: u16,
    height: u16,
    rows: Vec<Vec<Cell>>,
    status: String,
}

#[derive(Debug, Clone, Copy)]
struct ViewportDims {
    width: u16,
    height: u16,
    source_rows: usize,
    source_cols: usize,
    content_width: usize,
}

#[derive(Debug)]
enum ViewerEvent {
    Terminal(Event),
    FileChanged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileSignature {
    len: u64,
    modified: Option<SystemTime>,
}

struct ViewerApp {
    args: Args,
    viewer: ViewerCore,
    events_rx: Receiver<ViewerEvent>,
    events_tx: Sender<ViewerEvent>,
    selection: Option<Selection>,
    needs_redraw: bool,
    previous_frame: Option<FrameSnapshot>,
}

impl ViewerApp {
    fn new(args: Args) -> io::Result<Self> {
        let dims = Self::viewport_dims_for(&args)?;
        let mut viewer = ViewerCore::new(
            args.path.clone(),
            dims.source_rows,
            dims.source_cols,
            args.follow,
        )?;
        let (events_tx, events_rx) = mpsc::channel();
        spawn_file_watcher(args.path.clone(), events_tx.clone());
        if viewer.follow {
            viewer.jump_to_end(dims.height as usize, dims.content_width)?;
        }
        Ok(Self {
            args,
            viewer,
            events_rx,
            events_tx,
            selection: None,
            needs_redraw: true,
            previous_frame: None,
        })
    }

    fn viewport_dims(&self) -> io::Result<ViewportDims> {
        Self::viewport_dims_for(&self.args)
    }

    fn viewport_dims_for(args: &Args) -> io::Result<ViewportDims> {
        let (width, height) = terminal::size()?;
        let content_rows = max(height as usize, 1).saturating_sub(1).max(1);
        let source_rows = args.rows.unwrap_or(max(content_rows, 40));
        let source_cols = args.cols.unwrap_or(max(width as usize, 120));
        let content_width = max(width as usize, 1).saturating_sub(1).max(1);
        Ok(ViewportDims {
            width,
            height,
            source_rows: max(source_rows, 1),
            source_cols: max(source_cols, 1),
            content_width,
        })
    }

    fn mark_dirty(&mut self) {
        self.needs_redraw = true;
    }

    fn invalidate_frame(&mut self) {
        self.previous_frame = None;
    }

    fn reload_for_viewport(&mut self, dims: ViewportDims) -> io::Result<()> {
        self.viewer.resize_source(
            dims.source_rows,
            dims.source_cols,
            dims.height as usize,
            dims.content_width,
        )?;
        self.selection = None;
        self.invalidate_frame();
        self.mark_dirty();
        Ok(())
    }

    fn draw(&mut self, stdout: &mut io::Stdout) -> io::Result<()> {
        let dims = self.viewport_dims()?;
        let mut content_rows = self
            .viewer
            .visible_rows(dims.height as usize, dims.content_width)?;
        if let Some(selection) = self.selection {
            for (offset, row) in content_rows.iter_mut().enumerate() {
                apply_selection_highlight(row, self.viewer.top + offset, &selection);
            }
        }
        let status_override = self.selection.map(|selection| {
            format!(
                "{}  h/j/k/l move  0/$ line  g/G file  y copy  Esc cancel",
                selection_label(selection.mode)
            )
        });
        let status = self.viewer.status_text_with_override(
            dims.height as usize,
            dims.content_width,
            dims.width as usize,
            status_override.as_deref(),
        )?;
        let frame = FrameSnapshot {
            width: dims.width,
            height: dims.height,
            rows: content_rows,
            status,
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
        draw_cells(stdout, y, start_x, &cells[prefix_cells..], max_width)?;
        Ok(())
    }

    fn scroll(&mut self, amount: isize) -> io::Result<()> {
        let dims = self.viewport_dims()?;
        let had_status = !self.viewer.status.is_empty();
        let old_top = self.viewer.top;
        self.viewer
            .scroll(amount, dims.height as usize, dims.content_width)?;
        if self.viewer.top != old_top {
            self.mark_dirty();
        }
        if had_status {
            self.mark_dirty();
        }
        Ok(())
    }

    fn page(&mut self, amount: isize) -> io::Result<()> {
        let dims = self.viewport_dims()?;
        let had_status = !self.viewer.status.is_empty();
        let old_top = self.viewer.top;
        self.viewer
            .page(amount, dims.height as usize, dims.content_width)?;
        if self.viewer.top != old_top {
            self.mark_dirty();
        }
        if had_status {
            self.mark_dirty();
        }
        Ok(())
    }

    fn half_page(&mut self, amount: isize) -> io::Result<()> {
        let dims = self.viewport_dims()?;
        let had_status = !self.viewer.status.is_empty();
        let old_top = self.viewer.top;
        self.viewer
            .half_page(amount, dims.height as usize, dims.content_width)?;
        if self.viewer.top != old_top {
            self.mark_dirty();
        }
        if had_status {
            self.mark_dirty();
        }
        Ok(())
    }

    fn prompt(&mut self, stdout: &mut io::Stdout, prompt: &str) -> io::Result<String> {
        let mut buf = String::new();
        loop {
            let dims = self.viewport_dims()?;
            let line = format!("{}{}", prompt, buf);
            queue!(
                stdout,
                MoveTo(0, dims.height.saturating_sub(1)),
                SetAttribute(Attribute::Reverse),
                Clear(ClearType::CurrentLine),
                Print(line),
                SetAttribute(Attribute::Reset),
                ResetColor
            )?;
            stdout.flush()?;

            if let Some(key) = self.handle_prompt_event(self.next_viewer_event()?)? {
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
        self.viewer.status.clear();
        self.invalidate_frame();
        self.mark_dirty();
        Ok(buf)
    }

    fn next_viewer_event(&self) -> io::Result<ViewerEvent> {
        self.events_rx
            .recv()
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "viewer event channel closed"))
    }

    fn poll_viewer_change(&mut self) -> io::Result<bool> {
        let dims = self.viewport_dims()?;
        self.viewer.poll(dims.height as usize, dims.content_width)
    }

    fn handle_terminal_event(&mut self, stdout: &mut io::Stdout, event: Event) -> io::Result<bool> {
        match event {
            Event::Resize(_, _) => {
                let dims = self.viewport_dims()?;
                self.reload_for_viewport(dims)?;
                Ok(false)
            }
            Event::Key(key) => self.handle_key(stdout, key),
            _ => Ok(false),
        }
    }

    fn handle_prompt_event(&mut self, event: ViewerEvent) -> io::Result<Option<KeyEvent>> {
        match event {
            ViewerEvent::Terminal(Event::Resize(_, _)) => {
                let dims = self.viewport_dims()?;
                self.reload_for_viewport(dims)?;
                Ok(None)
            }
            ViewerEvent::Terminal(Event::Key(key)) => Ok(Some(key)),
            ViewerEvent::Terminal(_) => Ok(None),
            ViewerEvent::FileChanged => {
                if self.poll_viewer_change()? {
                    self.mark_dirty();
                }
                Ok(None)
            }
        }
    }

    fn show_help(&mut self) {
        self.viewer.status = "j/k, C-e/C-n/C-y scroll, space/b/C-f/C-b page, d/u half-page, g/G home/end, / search, n/N repeat, v/V visual select, F follow, q quit".to_string();
        self.mark_dirty();
    }

    fn start_selection(&mut self, mode: SelectionMode) -> io::Result<()> {
        let dims = self.viewport_dims()?;
        self.viewer.follow = false;
        let point = self.selection_start(dims)?;
        self.selection = Some(Selection::new(mode, point));
        self.viewer.status.clear();
        self.mark_dirty();
        Ok(())
    }

    fn selection_start(&mut self, dims: ViewportDims) -> io::Result<SelectionPoint> {
        let total_rows = self.viewer.display_len(dims.content_width)?;
        if total_rows == 0 {
            return Ok(SelectionPoint { row: 0, col: 0 });
        }
        let visible_height = ViewerCore::content_height(dims.height as usize);
        let visible_end = (self.viewer.top + visible_height).min(total_rows);
        let mut row_index = self.viewer.top.min(total_rows.saturating_sub(1));
        for candidate in self.viewer.top..visible_end {
            if self
                .viewer
                .display_row(candidate, dims.content_width)?
                .is_some_and(|row| !row.is_empty())
            {
                row_index = candidate;
                break;
            }
        }
        let row = self
            .viewer
            .display_row(row_index, dims.content_width)?
            .unwrap_or_default();
        Ok(SelectionPoint {
            row: row_index,
            col: first_selectable_col(&row),
        })
    }

    fn ensure_selection_visible(&mut self, dims: ViewportDims, row_index: usize) {
        let visible_height = ViewerCore::content_height(dims.height as usize);
        if row_index < self.viewer.top {
            self.viewer.top = row_index;
        } else if visible_height > 0 && row_index >= self.viewer.top + visible_height {
            self.viewer.top = row_index.saturating_sub(visible_height.saturating_sub(1));
        }
    }

    fn move_selection_row(&mut self, amount: isize) -> io::Result<()> {
        let dims = self.viewport_dims()?;
        let total_rows = self.viewer.display_len(dims.content_width)?;
        if total_rows == 0 {
            return Ok(());
        }
        let mut selection = match self.selection {
            Some(selection) => selection,
            None => return Ok(()),
        };
        let max_row = total_rows.saturating_sub(1) as isize;
        let new_row = (selection.cursor.row as isize + amount).clamp(0, max_row) as usize;
        let row = self
            .viewer
            .display_row(new_row, dims.content_width)?
            .unwrap_or_default();
        let new_col = if row.is_empty() {
            0
        } else {
            normalize_col(&row, selection.cursor.col)
        };
        selection.cursor = SelectionPoint {
            row: new_row,
            col: new_col,
        };
        self.selection = Some(selection);
        self.ensure_selection_visible(dims, new_row);
        self.mark_dirty();
        Ok(())
    }

    fn move_selection_col(&mut self, forward: bool) -> io::Result<()> {
        let dims = self.viewport_dims()?;
        let mut selection = match self.selection {
            Some(selection) => selection,
            None => return Ok(()),
        };
        let row = self
            .viewer
            .display_row(selection.cursor.row, dims.content_width)?
            .unwrap_or_default();
        selection.cursor.col = if row.is_empty() {
            0
        } else if forward {
            next_col(&row, selection.cursor.col)
        } else {
            previous_col(&row, selection.cursor.col)
        };
        self.selection = Some(selection);
        self.mark_dirty();
        Ok(())
    }

    fn move_selection_line_edge(&mut self, to_end: bool) -> io::Result<()> {
        let dims = self.viewport_dims()?;
        let mut selection = match self.selection {
            Some(selection) => selection,
            None => return Ok(()),
        };
        let row = self
            .viewer
            .display_row(selection.cursor.row, dims.content_width)?
            .unwrap_or_default();
        selection.cursor.col = if row.is_empty() {
            0
        } else if to_end {
            last_selectable_col(&row)
        } else {
            0
        };
        self.selection = Some(selection);
        self.mark_dirty();
        Ok(())
    }

    fn jump_selection_file_edge(&mut self, to_end: bool) -> io::Result<()> {
        let dims = self.viewport_dims()?;
        let total_rows = self.viewer.display_len(dims.content_width)?;
        if total_rows == 0 {
            return Ok(());
        }
        let target_row = if to_end { total_rows - 1 } else { 0 };
        let row = self
            .viewer
            .display_row(target_row, dims.content_width)?
            .unwrap_or_default();
        let mut selection = match self.selection {
            Some(selection) => selection,
            None => return Ok(()),
        };
        selection.cursor = SelectionPoint {
            row: target_row,
            col: if row.is_empty() {
                0
            } else if to_end {
                last_selectable_col(&row)
            } else {
                first_selectable_col(&row)
            },
        };
        self.selection = Some(selection);
        self.ensure_selection_visible(dims, target_row);
        self.mark_dirty();
        Ok(())
    }

    fn copy_selection(&mut self) -> io::Result<()> {
        let dims = self.viewport_dims()?;
        let Some(selection) = self.selection else {
            return Ok(());
        };
        let text = selection_text(selection, |row| {
            self.viewer
                .display_row(row, dims.content_width)
                .ok()
                .flatten()
        });
        match copy_to_clipboard(&text) {
            Ok(()) => {
                self.viewer.status = format!(
                    "Copied {} displayed line{}",
                    selection.line_span(),
                    if selection.line_span() == 1 { "" } else { "s" }
                );
                self.selection = None;
            }
            Err(err) => {
                self.viewer.status = format!("Clipboard copy failed: {}", err);
            }
        }
        self.mark_dirty();
        Ok(())
    }

    fn cancel_selection(&mut self, message: Option<&str>) {
        self.selection = None;
        if let Some(message) = message {
            self.viewer.status = message.to_string();
        } else {
            self.viewer.status.clear();
        }
        self.mark_dirty();
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
            }
            | KeyEvent {
                code: KeyCode::Char('e'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => self.move_selection_row(1)?,
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
            } => self.move_selection_row(-1)?,
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
            } => {
                let step =
                    ViewerCore::content_height(self.viewport_dims()?.height as usize) as isize;
                self.move_selection_row(step.max(1))?;
            }
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
            } => {
                let step =
                    ViewerCore::content_height(self.viewport_dims()?.height as usize) as isize;
                self.move_selection_row(-(step.max(1)))?;
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
            } => {
                let step = (ViewerCore::content_height(self.viewport_dims()?.height as usize) / 2)
                    .max(1) as isize;
                self.move_selection_row(step)?;
            }
            KeyEvent {
                code: KeyCode::Char('u'),
                modifiers: KeyModifiers::NONE,
                ..
            }
            | KeyEvent {
                code: KeyCode::Char('u'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => {
                let step = (ViewerCore::content_height(self.viewport_dims()?.height as usize) / 2)
                    .max(1) as isize;
                self.move_selection_row(-step)?;
            }
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
            _ => {}
        }
        Ok(())
    }

    fn handle_key(&mut self, stdout: &mut io::Stdout, key: KeyEvent) -> io::Result<bool> {
        if self.selection.is_some() {
            self.handle_selection_key(key)?;
            return Ok(false);
        }

        match key {
            KeyEvent {
                code: KeyCode::Char('q' | 'Q'),
                ..
            } => return Ok(true),
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
                self.viewer.follow = false;
                self.viewer.top = 0;
                self.viewer.status.clear();
                self.mark_dirty();
            }
            KeyEvent {
                code: KeyCode::End, ..
            }
            | KeyEvent {
                code: KeyCode::Char('G'),
                ..
            } => {
                let dims = self.viewport_dims()?;
                self.viewer.follow = false;
                self.viewer
                    .jump_to_end(dims.height as usize, dims.content_width)?;
                self.viewer.status.clear();
                self.mark_dirty();
            }
            KeyEvent {
                code: KeyCode::Char('F'),
                ..
            } => {
                let dims = self.viewport_dims()?;
                self.viewer.follow = true;
                self.viewer
                    .jump_to_end(dims.height as usize, dims.content_width)?;
                self.viewer.status = "Follow mode".to_string();
                self.mark_dirty();
            }
            KeyEvent {
                code: KeyCode::Char('c'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => {
                self.viewer.follow = false;
                self.viewer.status = "Follow stopped".to_string();
                self.mark_dirty();
            }
            KeyEvent {
                code: KeyCode::Char('r'),
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                let dims = self.viewport_dims()?;
                self.reload_for_viewport(dims)?;
                self.viewer.status = "Reloaded".to_string();
                self.mark_dirty();
            }
            KeyEvent {
                code: KeyCode::Char('/'),
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                let term = self.prompt(stdout, "/")?;
                if !term.is_empty() {
                    let dims = self.viewport_dims()?;
                    self.viewer.search_term = Some(term.clone());
                    self.viewer.last_search_forward = true;
                    if !self
                        .viewer
                        .search(&term, true, dims.height as usize, dims.content_width)?
                    {
                        self.viewer.status = format!("Pattern not found: {}", term);
                    } else {
                        self.viewer.status = format!("/{}", term);
                    }
                } else {
                    self.viewer.status.clear();
                }
                self.mark_dirty();
            }
            KeyEvent {
                code: KeyCode::Char('n'),
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                let dims = self.viewport_dims()?;
                self.viewer.repeat_search(
                    self.viewer.last_search_forward,
                    dims.height as usize,
                    dims.content_width,
                )?;
                self.mark_dirty();
            }
            KeyEvent {
                code: KeyCode::Char('N'),
                ..
            } => {
                let dims = self.viewport_dims()?;
                self.viewer.repeat_search(
                    !self.viewer.last_search_forward,
                    dims.height as usize,
                    dims.content_width,
                )?;
                self.mark_dirty();
            }
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
        spawn_input_reader(self.events_tx.clone());

        loop {
            if self.needs_redraw {
                self.draw(&mut stdout)?;
                self.needs_redraw = false;
            }

            let mut pending = vec![self.next_viewer_event()?];
            while let Ok(event) = self.events_rx.try_recv() {
                pending.push(event);
            }

            let mut file_changed = false;
            let mut should_break = false;
            for event in pending {
                match event {
                    ViewerEvent::FileChanged => {
                        file_changed = true;
                    }
                    ViewerEvent::Terminal(event) => {
                        if file_changed {
                            if self.poll_viewer_change()? {
                                self.mark_dirty();
                            }
                            file_changed = false;
                        }
                        if self.handle_terminal_event(&mut stdout, event)? {
                            should_break = true;
                            break;
                        }
                        self.mark_dirty();
                    }
                }
            }
            if file_changed && self.poll_viewer_change()? {
                self.mark_dirty();
            }
            if should_break {
                break;
            }
        }
        Ok(())
    }
}

fn current_file_signature(path: &Path) -> Option<FileSignature> {
    let meta = fs::metadata(path).ok()?;
    Some(FileSignature {
        len: meta.len(),
        modified: meta.modified().ok(),
    })
}

fn spawn_input_reader(tx: Sender<ViewerEvent>) {
    thread::spawn(move || {
        loop {
            match event::read() {
                Ok(event @ Event::Key(_)) | Ok(event @ Event::Resize(_, _)) => {
                    if tx.send(ViewerEvent::Terminal(event)).is_err() {
                        break;
                    }
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
    });
}

fn spawn_file_watcher(path: PathBuf, tx: Sender<ViewerEvent>) {
    thread::spawn(move || {
        let resize_path = resize_events_path(&path);
        let mut signature = (
            current_file_signature(&path),
            current_file_signature(&resize_path),
        );
        loop {
            thread::sleep(Duration::from_millis(20));
            let next = (
                current_file_signature(&path),
                current_file_signature(&resize_path),
            );
            if next != signature {
                signature = next;
                if tx.send(ViewerEvent::FileChanged).is_err() {
                    break;
                }
            }
        }
    });
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

    let mut app = ViewerApp::new(args)?;
    app.run()
}

fn selection_label(mode: SelectionMode) -> &'static str {
    match mode {
        SelectionMode::Character => "VISUAL",
        SelectionMode::Line => "V-LINE",
    }
}
