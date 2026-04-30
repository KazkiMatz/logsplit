use std::cmp::{max, min};
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};

use crate::debug::debug_timing;
use crate::terminal::{
    Cell, VirtualTerminal, clamp, clamp_signed, decode_utf8_chunk, wrap_styled_line,
};
use crate::transcript::{ResizeEvent, load_resize_events, resize_events_path};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileSignature {
    len: u64,
    modified: Option<SystemTime>,
}

fn current_file_signature(path: &Path) -> Option<FileSignature> {
    let meta = fs::metadata(path).ok()?;
    Some(FileSignature {
        len: meta.len(),
        modified: meta.modified().ok(),
    })
}

#[derive(Debug)]
pub struct ReplayFile {
    path: PathBuf,
    resize_path: PathBuf,
    offset: u64,
    pending_utf8: Vec<u8>,
    resize_events: Vec<ResizeEvent>,
    next_resize_index: usize,
    resize_signature: Option<FileSignature>,
}

impl ReplayFile {
    pub fn new(path: PathBuf) -> Self {
        let resize_path = resize_events_path(&path);
        Self {
            path,
            resize_path,
            offset: 0,
            pending_utf8: Vec::new(),
            resize_events: Vec::new(),
            next_resize_index: 0,
            resize_signature: None,
        }
    }

    pub fn replay_all(&mut self, term: &mut VirtualTerminal) -> io::Result<()> {
        let start = Instant::now();
        self.offset = 0;
        self.pending_utf8.clear();
        self.reload_resize_events()?;
        self.reset_term_for_replay(term);

        let mut fh = File::open(&self.path)?;
        let mut buf = [0u8; 65536];
        let mut chunk_count = 0usize;
        loop {
            let count = fh.read(&mut buf)?;
            if count == 0 {
                break;
            }
            chunk_count += 1;
            self.feed_bytes(term, &buf[..count]);
        }
        let remainder = decode_utf8_chunk(&mut self.pending_utf8, &[], true);
        if !remainder.is_empty() {
            term.feed(&remainder);
        }
        self.apply_resize_events_at_current_offset(term);
        debug_timing("ReplayFile::replay_all", start, || {
            format!(
                "path={} bytes={} chunks={} history_rows={} resize_events={}",
                self.path.display(),
                self.offset,
                chunk_count,
                term.history_len(),
                self.resize_events.len()
            )
        });
        Ok(())
    }

    pub fn poll(&mut self, term: &mut VirtualTerminal) -> io::Result<bool> {
        let start = Instant::now();
        let resize_signature = current_file_signature(&self.resize_path);
        if resize_signature != self.resize_signature {
            self.replay_all(term)?;
            return Ok(true);
        }

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
        let mut bytes_read = 0usize;
        let mut chunk_count = 0usize;
        loop {
            let count = fh.read(&mut buf)?;
            if count == 0 {
                break;
            }
            changed = true;
            bytes_read += count;
            chunk_count += 1;
            self.feed_bytes(term, &buf[..count]);
        }
        self.apply_resize_events_at_current_offset(term);
        if changed {
            debug_timing("ReplayFile::poll", start, || {
                format!(
                    "path={} bytes={} chunks={} offset={} history_rows={} resize_events={}",
                    self.path.display(),
                    bytes_read,
                    chunk_count,
                    self.offset,
                    term.history_len(),
                    self.resize_events.len()
                )
            });
        }
        Ok(changed)
    }

    fn reload_resize_events(&mut self) -> io::Result<()> {
        self.resize_events = load_resize_events(&self.path)?;
        self.next_resize_index = 0;
        self.resize_signature = current_file_signature(&self.resize_path);
        Ok(())
    }

    fn reset_term_for_replay(&mut self, term: &mut VirtualTerminal) {
        let mut initial_rows = term.rows();
        let mut initial_cols = term.cols();
        while let Some(event) = self.resize_events.get(self.next_resize_index).copied() {
            if event.offset != 0 {
                break;
            }
            initial_rows = event.rows;
            initial_cols = event.cols;
            self.next_resize_index += 1;
        }

        term.reset_to_size(initial_rows, initial_cols);
    }

    fn feed_bytes(&mut self, term: &mut VirtualTerminal, bytes: &[u8]) {
        self.apply_resize_events_at_current_offset(term);
        let mut start = 0usize;
        while start < bytes.len() {
            let mut end = bytes.len();
            if let Some(event) = self.resize_events.get(self.next_resize_index) {
                if event.offset > self.offset {
                    let delta = (event.offset - self.offset) as usize;
                    if delta < end - start {
                        end = start + delta;
                    }
                }
            }

            if end == start {
                self.apply_resize_events_at_current_offset(term);
                continue;
            }

            let text = decode_utf8_chunk(&mut self.pending_utf8, &bytes[start..end], false);
            if !text.is_empty() {
                term.feed(&text);
            }
            self.offset += (end - start) as u64;
            start = end;
            self.apply_resize_events_at_current_offset(term);
        }
    }

    fn apply_resize_events_at_current_offset(&mut self, term: &mut VirtualTerminal) {
        while let Some(event) = self.resize_events.get(self.next_resize_index).copied() {
            if event.offset > self.offset {
                break;
            }
            if term.rows() != event.rows || term.cols() != event.cols {
                term.resize(event.rows, event.cols);
            }
            self.next_resize_index += 1;
        }
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
        let start = Instant::now();
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
        let visible_count = visible.len();
        debug_timing("ViewerCore::visible_rows", start, || {
            format!(
                "path={} top={} height={} visible={} display={}",
                self.path.display(),
                self.top,
                content_height,
                visible_count,
                cache.display.len()
            )
        });
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
        let start = Instant::now();
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
        let screen_row_count = screen_rows.len();

        if let Some(cache) = self.layout_cache.as_mut() {
            if cache.content_width == content_width && cache.history_count <= current_history_count
            {
                let cached_history_count = cache.history_count;
                let prefix_display_count = cache
                    .first_display_by_logical
                    .get(cached_history_count)
                    .copied()
                    .unwrap_or(cache.display.len());
                cache.display.truncate(prefix_display_count);
                cache
                    .first_display_by_logical
                    .truncate(cached_history_count);

                let target_logical_rows = current_history_count + screen_row_count;
                cache.first_display_by_logical.reserve(
                    target_logical_rows.saturating_sub(cache.first_display_by_logical.len()),
                );

                for (offset, row) in self
                    .term
                    .history_rows()
                    .iter()
                    .skip(cached_history_count)
                    .enumerate()
                {
                    let logical_idx = cached_history_count + offset;
                    append_wrapped_row(
                        &mut cache.display,
                        &mut cache.first_display_by_logical,
                        logical_idx,
                        row,
                        content_width,
                    );
                }

                for (offset, row) in screen_rows.iter().enumerate() {
                    let logical_idx = current_history_count + offset;
                    append_wrapped_row(
                        &mut cache.display,
                        &mut cache.first_display_by_logical,
                        logical_idx,
                        row,
                        content_width,
                    );
                }

                cache.content_width = content_width;
                cache.history_count = current_history_count;
                self.layout_stale = false;
                let display_len = cache.display.len();
                debug_timing("ViewerCore::ensure_layout", start, || {
                    format!(
                        "path={} mode={} width={} history_rows={} screen_rows={} display_rows={}",
                        self.path.display(),
                        "append-in-place",
                        content_width,
                        current_history_count,
                        screen_row_count,
                        display_len
                    )
                });
                return Ok(());
            }
        }

        let mode = if self.layout_cache.is_some() {
            "full-rebuild"
        } else {
            "full-initial"
        };
        let (display, first_display_by_logical) =
            full_layout(self.term.history_rows(), &screen_rows, content_width);
        let display_len = display.len();

        self.layout_cache = Some(LayoutCache {
            content_width,
            history_count: current_history_count,
            display,
            first_display_by_logical,
        });
        self.layout_stale = false;
        debug_timing("ViewerCore::ensure_layout", start, || {
            format!(
                "path={} mode={} width={} history_rows={} screen_rows={} display_rows={}",
                self.path.display(),
                mode,
                content_width,
                current_history_count,
                screen_row_count,
                display_len
            )
        });
        Ok(())
    }
}

fn append_wrapped_row(
    display: &mut Vec<(usize, Vec<Cell>)>,
    first_display_by_logical: &mut Vec<usize>,
    logical_idx: usize,
    row: &[Cell],
    content_width: usize,
) {
    first_display_by_logical.push(display.len());
    for segment in wrap_styled_line(row, content_width) {
        display.push((logical_idx, segment));
    }
}

fn full_layout(
    history_rows: &[Vec<Cell>],
    screen_rows: &[Vec<Cell>],
    content_width: usize,
) -> (Vec<(usize, Vec<Cell>)>, Vec<usize>) {
    let mut display = Vec::new();
    let mut first_display_by_logical = Vec::with_capacity(history_rows.len() + screen_rows.len());
    for (idx, row) in history_rows.iter().enumerate() {
        append_wrapped_row(
            &mut display,
            &mut first_display_by_logical,
            idx,
            row,
            content_width,
        );
    }
    for (offset, row) in screen_rows.iter().enumerate() {
        let logical_idx = history_rows.len() + offset;
        append_wrapped_row(
            &mut display,
            &mut first_display_by_logical,
            logical_idx,
            row,
            content_width,
        );
    }
    (display, first_display_by_logical)
}

#[cfg(test)]
mod tests {
    use super::ReplayFile;
    use crate::terminal::VirtualTerminal;
    use crate::transcript::resize_events_path;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_log_path(name: &str) -> std::path::PathBuf {
        let mut path = std::env::temp_dir();
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        path.push(format!("logsplit-viewer-test-{name}-{nonce}.log"));
        path
    }

    #[test]
    fn replay_applies_resize_history_by_byte_offset() {
        let path = temp_log_path("resize-replay");
        fs::write(&path, b"abcdef").unwrap();
        fs::write(resize_events_path(&path), b"0\t1\t3\n3\t1\t6\n").unwrap();

        let mut term = VirtualTerminal::new(10, 10);
        let mut replay = ReplayFile::new(path.clone());
        replay.replay_all(&mut term).unwrap();

        assert_eq!(term.rendered_lines(), vec!["abcdef"]);

        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(resize_events_path(&path));
    }
}
