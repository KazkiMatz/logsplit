use std::cmp::max;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::Parser;
use crossterm::SynchronizedUpdate;
use crossterm::cursor::MoveTo;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::queue;
use crossterm::style::{Attribute, Print, ResetColor, SetAttribute};
use crossterm::terminal::{self, Clear, ClearType};
use logsplit_rs::{
    Cell, ReplayFile, TerminalGuard, ViewerCore, VirtualTerminal, cell_prefix_width,
    common_prefix_len, draw_cells,
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

struct ViewerApp {
    args: Args,
    viewer: ViewerCore,
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
        if viewer.follow {
            viewer.jump_to_end(dims.height as usize, dims.content_width)?;
        }
        Ok(Self {
            args,
            viewer,
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
        self.invalidate_frame();
        self.mark_dirty();
        Ok(())
    }

    fn draw(&mut self, stdout: &mut io::Stdout) -> io::Result<()> {
        let dims = self.viewport_dims()?;
        let content_rows = self
            .viewer
            .visible_rows(dims.height as usize, dims.content_width)?;
        let status = self.viewer.status_text(
            dims.height as usize,
            dims.content_width,
            dims.width as usize,
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
        let dims = self.viewport_dims()?;
        let mut buf = String::new();
        loop {
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
        self.viewer.status.clear();
        self.invalidate_frame();
        self.mark_dirty();
        Ok(buf)
    }

    fn show_help(&mut self) {
        self.viewer.status = "j/k, C-e/C-n/C-y scroll, space/b/C-f/C-b page, d/u half-page, g/G home/end, / search, n/N repeat, F follow, q quit".to_string();
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

        loop {
            let dims = self.viewport_dims()?;
            let changed = self.viewer.poll(dims.height as usize, dims.content_width)?;
            if changed {
                self.mark_dirty();
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
                    let dims = self.viewport_dims()?;
                    self.reload_for_viewport(dims)?;
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
