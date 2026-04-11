use std::cmp::{max, min};
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::terminal::{
    Cell, VirtualTerminal, clamp, clamp_signed, decode_utf8_chunk, wrap_styled_line,
};

#[derive(Debug)]
pub struct ReplayFile {
    path: PathBuf,
    offset: u64,
    pending_utf8: Vec<u8>,
}

impl ReplayFile {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            offset: 0,
            pending_utf8: Vec::new(),
        }
    }

    pub fn replay_all(&mut self, term: &mut VirtualTerminal) -> io::Result<()> {
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

    pub fn poll(&mut self, term: &mut VirtualTerminal) -> io::Result<bool> {
        let size = fs::metadata(&self.path)?.len();
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

pub struct ViewerCore {
    path: PathBuf,
    pub status: String,
    pub follow: bool,
    pub search_term: Option<String>,
    pub last_search_forward: bool,
    pub top: usize,
    term: VirtualTerminal,
    replay: ReplayFile,
    layout_cache: Option<LayoutCache>,
    layout_stale: bool,
}

impl ViewerCore {
    pub fn new(
        path: PathBuf,
        source_rows: usize,
        source_cols: usize,
        follow: bool,
    ) -> io::Result<Self> {
        let mut core = Self {
            path: path.clone(),
            status: String::new(),
            follow,
            search_term: None,
            last_search_forward: true,
            top: 0,
            term: VirtualTerminal::new(source_rows, source_cols),
            replay: ReplayFile::new(path),
            layout_cache: None,
            layout_stale: true,
        };
        core.replay.replay_all(&mut core.term)?;
        Ok(core)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn term(&self) -> &VirtualTerminal {
        &self.term
    }

    pub fn lines(&self) -> Vec<String> {
        self.term.rendered_lines()
    }

    pub fn content_height(total_rows: usize) -> usize {
        max(total_rows, 1).saturating_sub(1).max(1)
    }

    pub fn drop_layout_cache(&mut self) {
        self.layout_cache = None;
        self.layout_stale = true;
    }

    pub fn invalidate_layout(&mut self) {
        self.layout_stale = true;
    }

    pub fn resize_source(
        &mut self,
        source_rows: usize,
        source_cols: usize,
        total_rows: usize,
        content_width: usize,
    ) -> io::Result<()> {
        self.term.resize(source_rows, source_cols);
        self.replay.replay_all(&mut self.term)?;
        self.drop_layout_cache();
        if self.follow {
            self.jump_to_end(total_rows, content_width)?;
        } else {
            self.top = clamp(self.top, 0, self.max_top(total_rows, content_width)?);
        }
        Ok(())
    }

    pub fn poll(&mut self, total_rows: usize, content_width: usize) -> io::Result<bool> {
        let changed = self.replay.poll(&mut self.term)?;
        if changed {
            self.invalidate_layout();
            if self.follow {
                self.jump_to_end(total_rows, content_width)?;
            }
        }
        Ok(changed)
    }

    pub fn max_top(&mut self, total_rows: usize, content_width: usize) -> io::Result<usize> {
        self.ensure_layout(content_width)?;
        let len = self
            .layout_cache
            .as_ref()
            .expect("layout cache should exist after ensure_layout")
            .display
            .len();
        Ok(len.saturating_sub(Self::content_height(total_rows)))
    }

    pub fn jump_to_end(&mut self, total_rows: usize, content_width: usize) -> io::Result<()> {
        self.top = self.max_top(total_rows, content_width)?;
        Ok(())
    }

    pub fn visible_rows(
        &mut self,
        total_rows: usize,
        content_width: usize,
    ) -> io::Result<Vec<Vec<Cell>>> {
        let content_height = Self::content_height(total_rows);
        self.ensure_layout(content_width)?;
        let cache = self
            .layout_cache
            .as_ref()
            .expect("layout cache should exist after ensure_layout");
        let max_top = cache.display.len().saturating_sub(content_height);
        self.top = clamp(self.top, 0, max_top);

        let visible_end = min(self.top + content_height, cache.display.len());
        let visible = &cache.display[self.top..visible_end];
        let mut rows = Vec::with_capacity(content_height);
        for y in 0..content_height {
            if let Some((_logical_idx, cells)) = visible.get(y) {
                rows.push(cells.clone());
            } else {
                rows.push(Vec::new());
            }
        }
        Ok(rows)
    }

    pub fn display_len(&mut self, content_width: usize) -> io::Result<usize> {
        self.ensure_layout(content_width)?;
        Ok(self
            .layout_cache
            .as_ref()
            .expect("layout cache should exist after ensure_layout")
            .display
            .len())
    }

    pub fn display_row(
        &mut self,
        index: usize,
        content_width: usize,
    ) -> io::Result<Option<Vec<Cell>>> {
        self.ensure_layout(content_width)?;
        Ok(self
            .layout_cache
            .as_ref()
            .expect("layout cache should exist after ensure_layout")
            .display
            .get(index)
            .map(|(_, cells)| cells.clone()))
    }

    pub fn status_text(
        &mut self,
        total_rows: usize,
        content_width: usize,
        status_width: usize,
    ) -> io::Result<String> {
        self.status_text_with_override(total_rows, content_width, status_width, None)
    }

    pub fn status_text_with_override(
        &mut self,
        total_rows: usize,
        content_width: usize,
        status_width: usize,
        override_text: Option<&str>,
    ) -> io::Result<String> {
        self.ensure_layout(content_width)?;
        let cache = self
            .layout_cache
            .as_ref()
            .expect("layout cache should exist after ensure_layout");
        let content_height = Self::content_height(total_rows);
        let line_count = cache.display.len();
        let max_top = line_count.saturating_sub(content_height);
        self.top = clamp(self.top, 0, max_top);

        let mode = if self.follow { "FOLLOW" } else { "PAUSED" };
        let percent = if line_count == 0 {
            100
        } else {
            min(100, ((self.top + content_height) * 100) / line_count)
        };
        let default_status = format!(
            "{}  {}  {}/{}  {}%",
            mode,
            self.path
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or("viewer"),
            self.top + 1,
            max(line_count, 1),
            percent
        );
        let mut text = if let Some(text) = override_text {
            text.to_string()
        } else if self.status.is_empty() {
            default_status
        } else {
            self.status.clone()
        };
        let text_len = text.chars().count();
        if text_len < status_width {
            text.push_str(&" ".repeat(status_width - text_len));
        }
        Ok(text.chars().take(status_width).collect())
    }

    pub fn scroll(
        &mut self,
        amount: isize,
        total_rows: usize,
        content_width: usize,
    ) -> io::Result<()> {
        if amount != 0 {
            self.follow = false;
        }
        let max_top = self.max_top(total_rows, content_width)?;
        self.top = clamp_signed(self.top as isize + amount, 0, max_top as isize) as usize;
        self.status.clear();
        Ok(())
    }

    pub fn page(
        &mut self,
        amount: isize,
        total_rows: usize,
        content_width: usize,
    ) -> io::Result<()> {
        let step = Self::content_height(total_rows) as isize;
        self.scroll(amount * step, total_rows, content_width)
    }

    pub fn half_page(
        &mut self,
        amount: isize,
        total_rows: usize,
        content_width: usize,
    ) -> io::Result<()> {
        let step = max(1, Self::content_height(total_rows) / 2) as isize;
        self.scroll(amount * step, total_rows, content_width)
    }

    pub fn search(
        &mut self,
        term: &str,
        forward: bool,
        total_rows: usize,
        content_width: usize,
    ) -> io::Result<bool> {
        let lines = self.lines();
        self.ensure_layout(content_width)?;
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
        let max_top = self.max_top(total_rows, content_width)?;

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

    pub fn repeat_search(
        &mut self,
        forward: bool,
        total_rows: usize,
        content_width: usize,
    ) -> io::Result<bool> {
        let Some(term) = self.search_term.clone() else {
            self.status = "No previous search".to_string();
            return Ok(false);
        };
        self.last_search_forward = forward;
        if !self.search(&term, forward, total_rows, content_width)? {
            self.status = format!("Pattern not found: {}", term);
            Ok(false)
        } else {
            self.status = format!("/{}", term);
            Ok(true)
        }
    }

    fn ensure_layout(&mut self, content_width: usize) -> io::Result<()> {
        let content_width = max(content_width, 1);
        let current_history_count = self.term.history_len();

        if let Some(cache) = &self.layout_cache {
            if !self.layout_stale
                && cache.content_width == content_width
                && cache.history_count == current_history_count
            {
                return Ok(());
            }
        }

        let screen_rows = self.term.trimmed_screen_rows();
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
                    .rendered_rows()
                    .into_iter()
                    .enumerate()
                    .skip(cached_history_count)
                    .take(current_history_count.saturating_sub(cached_history_count))
                {
                    first.push(display.len());
                    for segment in wrap_styled_line(&row, content_width) {
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
                full_layout(
                    &self.term.rendered_rows()[..current_history_count],
                    &screen_rows,
                    content_width,
                )
            }
        } else {
            full_layout(
                &self.term.rendered_rows()[..current_history_count],
                &screen_rows,
                content_width,
            )
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
}

fn full_layout(
    history_rows: &[Vec<Cell>],
    screen_rows: &[Vec<Cell>],
    content_width: usize,
) -> (Vec<(usize, Vec<Cell>)>, Vec<usize>) {
    let logical = history_rows
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
