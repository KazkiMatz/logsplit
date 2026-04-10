use std::cmp::max;
use std::env;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::Parser;
use crossterm::SynchronizedUpdate;
use crossterm::cursor::MoveTo;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::queue;
use crossterm::style::{Attribute, Print, ResetColor, SetAttribute};
use crossterm::terminal;
use logsplit_rs::{
    Cell, Style, TerminalGuard, ViewerCore, VirtualTerminal, clear_segment, draw_cells,
    overlay_cells,
};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

#[derive(Debug, Parser)]
#[command(about = "Run a command with live script logging and a side-by-side log viewer")]
struct Args {
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

#[derive(Debug)]
enum PaneMsg {
    Data(Vec<u8>),
    Eof,
}

struct Pane {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send>,
    rx: Receiver<PaneMsg>,
    term: VirtualTerminal,
    pending_utf8: Vec<u8>,
    alive: bool,
}

impl Pane {
    fn drain(&mut self) -> bool {
        let mut changed = false;
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                PaneMsg::Data(bytes) => {
                    let text =
                        logsplit_rs::decode_utf8_chunk(&mut self.pending_utf8, &bytes, false);
                    if !text.is_empty() {
                        self.term.feed(&text);
                        changed = true;
                    }
                }
                PaneMsg::Eof => {
                    let tail = logsplit_rs::decode_utf8_chunk(&mut self.pending_utf8, &[], true);
                    if !tail.is_empty() {
                        self.term.feed(&tail);
                        changed = true;
                    }
                    self.alive = false;
                }
            }
        }
        changed
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
    focus: Focus,
    prefix_pending: bool,
    logfile: PathBuf,
    previous_frame: Option<FrameSnapshot>,
}

impl App {
    fn new(line: String) -> io::Result<Self> {
        let dims = split_dims()?;
        let logfile = make_logfile_path(&line)?;
        if let Some(parent) = logfile.parent() {
            fs::create_dir_all(parent)?;
        }
        let _ = File::create(&logfile)?;

        let mut left = ViewerCore::new(logfile.clone(), dims.rows, dims.right_cols, true)?;
        left.jump_to_end(dims.rows, dims.left_cols)?;
        let right = spawn_logged_command(&line, &logfile, dims.rows, dims.right_cols)?;

        Ok(Self {
            left,
            right,
            focus: Focus::Right,
            prefix_pending: false,
            logfile,
            previous_frame: None,
        })
    }

    fn run(&mut self) -> io::Result<()> {
        let mut stdout = io::stdout();
        let _guard = TerminalGuard::enter(&mut stdout)?;
        let mut needs_redraw = true;

        loop {
            let dims = split_dims()?;
            let mut changed = false;
            changed |= self.left.poll(dims.rows, dims.left_cols)?;
            changed |= self.right.drain();

            if self.right.exited()? {
                break;
            }

            if changed {
                needs_redraw = true;
            }
            if needs_redraw {
                self.draw(&mut stdout)?;
                needs_redraw = false;
            }

            let has_event = match event::poll(Duration::from_millis(50)) {
                Ok(value) => value,
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => false,
                Err(err) => return Err(err),
            };
            if !has_event {
                continue;
            }
            let event = match event::read() {
                Ok(value) => value,
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => continue,
                Err(err) => return Err(err),
            };
            match event {
                Event::Resize(_, _) => {
                    let dims = split_dims()?;
                    self.left.resize_source(
                        dims.rows,
                        dims.right_cols,
                        dims.rows,
                        dims.left_cols,
                    )?;
                    if self.left.follow {
                        self.left.jump_to_end(dims.rows, dims.left_cols)?;
                    }
                    self.right.resize(dims.rows, dims.right_cols)?;
                    self.previous_frame = None;
                    needs_redraw = true;
                }
                Event::Key(key) => {
                    if self.handle_key(&mut stdout, key)? {
                        break;
                    }
                    needs_redraw = true;
                }
                _ => {}
            }
        }

        Ok(())
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
        let left_rows = self.left.visible_rows(dims.rows, dims.left_cols)?;
        let status_text = self
            .left
            .status_text(dims.rows, dims.left_cols, dims.left_cols)?;
        let left_status = reverse_status_cells(&status_text, dims.left_cols);
        let blank = Cell::blank(Style::default());
        let mut rows = Vec::with_capacity(dims.rows);

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
                overlay_cells(&mut row, dims.separator_col + 1, right_row, dims.right_cols);
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
            if self.prefix_pending {
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

    fn prompt_left(
        &mut self,
        stdout: &mut io::Stdout,
        prompt: &str,
        width: usize,
    ) -> io::Result<String> {
        let dims = split_dims()?;
        let mut buf = String::new();
        loop {
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
        self.left.status.clear();
        self.previous_frame = None;
        Ok(buf)
    }

    fn show_left_help(&mut self) {
        self.left.status = "j/k, C-e/C-n/C-y scroll, space/b/C-f/C-b page, d/u half-page, g/G home/end, / search, n/N repeat, F follow, Ctrl-g q quit".to_string();
    }

    fn handle_left_key(&mut self, stdout: &mut io::Stdout, key: KeyEvent) -> io::Result<()> {
        let dims = split_dims()?;
        match key {
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
            }
            | KeyEvent {
                code: KeyCode::Char('e'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => {
                self.left.scroll(1, dims.rows, dims.left_cols)?;
            }
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
            } => {
                self.left.scroll(-1, dims.rows, dims.left_cols)?;
            }
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
                self.left.page(1, dims.rows, dims.left_cols)?;
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
                self.left.page(-1, dims.rows, dims.left_cols)?;
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
                self.left.half_page(1, dims.rows, dims.left_cols)?;
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
                self.left.half_page(-1, dims.rows, dims.left_cols)?;
            }
            KeyEvent {
                code: KeyCode::Home,
                ..
            }
            | KeyEvent {
                code: KeyCode::Char('g'),
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                self.left.follow = false;
                self.left.top = 0;
                self.left.status.clear();
            }
            KeyEvent {
                code: KeyCode::End, ..
            }
            | KeyEvent {
                code: KeyCode::Char('G'),
                ..
            } => {
                self.left.follow = false;
                self.left.jump_to_end(dims.rows, dims.left_cols)?;
                self.left.status.clear();
            }
            KeyEvent {
                code: KeyCode::Char('F'),
                ..
            } => {
                self.left.follow = true;
                self.left.jump_to_end(dims.rows, dims.left_cols)?;
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
                self.left
                    .resize_source(dims.rows, dims.right_cols, dims.rows, dims.left_cols)?;
                if self.left.follow {
                    self.left.jump_to_end(dims.rows, dims.left_cols)?;
                }
                self.left.status = "Reloaded".to_string();
                self.previous_frame = None;
            }
            KeyEvent {
                code: KeyCode::Char('/'),
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                let term = self.prompt_left(stdout, "/", dims.left_cols)?;
                if !term.is_empty() {
                    self.left.search_term = Some(term.clone());
                    self.left.last_search_forward = true;
                    if !self.left.search(&term, true, dims.rows, dims.left_cols)? {
                        self.left.status = format!("Pattern not found: {}", term);
                    } else {
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
                self.left.repeat_search(
                    self.left.last_search_forward,
                    dims.rows,
                    dims.left_cols,
                )?;
            }
            KeyEvent {
                code: KeyCode::Char('N'),
                ..
            } => {
                self.left.repeat_search(
                    !self.left.last_search_forward,
                    dims.rows,
                    dims.left_cols,
                )?;
            }
            KeyEvent {
                code: KeyCode::Char('?'),
                ..
            }
            | KeyEvent {
                code: KeyCode::Char('h'),
                modifiers: KeyModifiers::NONE,
                ..
            } => self.show_left_help(),
            _ => {}
        }
        Ok(())
    }

    fn handle_key(&mut self, stdout: &mut io::Stdout, key: KeyEvent) -> io::Result<bool> {
        if self.prefix_pending {
            self.prefix_pending = false;
            return match key {
                KeyEvent {
                    code: KeyCode::Tab, ..
                } => {
                    self.focus = match self.focus {
                        Focus::Left => Focus::Right,
                        Focus::Right => Focus::Left,
                    };
                    Ok(false)
                }
                KeyEvent {
                    code: KeyCode::Char('h'),
                    ..
                } => {
                    self.focus = Focus::Left;
                    Ok(false)
                }
                KeyEvent {
                    code: KeyCode::Char('l'),
                    ..
                } => {
                    self.focus = Focus::Right;
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
                code: KeyCode::Char('g'),
                modifiers: KeyModifiers::CONTROL,
                ..
            }
        ) {
            self.prefix_pending = true;
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

fn spawn_reader(mut reader: Box<dyn Read + Send>, tx: mpsc::Sender<PaneMsg>) {
    thread::spawn(move || {
        let mut buf = [0u8; 65536];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => {
                    let _ = tx.send(PaneMsg::Eof);
                    break;
                }
                Ok(count) => {
                    if tx.send(PaneMsg::Data(buf[..count].to_vec())).is_err() {
                        break;
                    }
                }
                Err(_) => {
                    let _ = tx.send(PaneMsg::Eof);
                    break;
                }
            }
        }
    });
}

fn spawn_logged_command(line: &str, logfile: &Path, rows: usize, cols: usize) -> io::Result<Pane> {
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: rows as u16,
            cols: cols as u16,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(anyerr)?;
    let mut cmd = CommandBuilder::new("script");
    cmd.arg("-q");
    cmd.arg(logfile);
    cmd.arg("/bin/zsh");
    cmd.arg("-lc");
    cmd.arg(line);
    cmd.cwd(env::current_dir()?);
    cmd.env("TERM", "xterm-256color");
    let child = pair.slave.spawn_command(cmd).map_err(anyerr)?;
    let reader = pair.master.try_clone_reader().map_err(anyerr)?;
    let writer = pair.master.take_writer().map_err(anyerr)?;
    let (tx, rx) = mpsc::channel();
    spawn_reader(reader, tx);
    Ok(Pane {
        master: pair.master,
        writer,
        child,
        rx,
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

fn anyerr(err: anyhow::Error) -> io::Error {
    io::Error::other(err.to_string())
}

fn main() -> io::Result<()> {
    let args = Args::parse();
    let line = args.line.join(" ");
    let mut app = App::new(line)?;
    let logfile = app.logfile.clone();
    let result = app.run();
    eprintln!("logsplit: log saved to {}", logfile.display());
    result
}
