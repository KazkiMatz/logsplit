use std::io;

use crate::terminal::{Cell, Style, row_to_text};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct SelectionPoint {
    pub row: usize,
    pub col: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionMode {
    Character,
    Line,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub mode: SelectionMode,
    pub anchor: SelectionPoint,
    pub cursor: SelectionPoint,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WordMotion {
    ForwardStart,
    ForwardEnd,
    BackwardStart,
}

impl Selection {
    pub fn new(mode: SelectionMode, anchor: SelectionPoint) -> Self {
        Self {
            mode,
            anchor,
            cursor: anchor,
        }
    }

    pub fn ordered(self) -> (SelectionPoint, SelectionPoint) {
        if self.anchor <= self.cursor {
            (self.anchor, self.cursor)
        } else {
            (self.cursor, self.anchor)
        }
    }

    pub fn line_span(self) -> usize {
        let (start, end) = self.ordered();
        end.row.saturating_sub(start.row) + 1
    }
}

pub fn first_selectable_col(row: &[Cell]) -> usize {
    row.iter()
        .enumerate()
        .find(|(_, cell)| !cell.wide_cont && cell.ch != ' ')
        .or_else(|| row.iter().enumerate().find(|(_, cell)| !cell.wide_cont))
        .map(|(idx, _)| idx)
        .unwrap_or(0)
}

pub fn last_selectable_col(row: &[Cell]) -> usize {
    row.iter()
        .enumerate()
        .rev()
        .find(|(_, cell)| !cell.wide_cont)
        .map(|(idx, _)| idx)
        .unwrap_or(0)
}

pub fn normalize_col(row: &[Cell], col: usize) -> usize {
    if row.is_empty() {
        return 0;
    }
    let mut idx = col.min(last_selectable_col(row));
    while idx > 0 && row.get(idx).is_some_and(|cell| cell.wide_cont) {
        idx -= 1;
    }
    idx
}

pub fn previous_col(row: &[Cell], col: usize) -> usize {
    if row.is_empty() {
        return 0;
    }
    let mut idx = normalize_col(row, col);
    while idx > 0 {
        idx -= 1;
        if !row[idx].wide_cont {
            return idx;
        }
    }
    0
}

pub fn next_col(row: &[Cell], col: usize) -> usize {
    if row.is_empty() {
        return 0;
    }
    let mut idx = normalize_col(row, col);
    let width = cell_width(row, idx);
    idx = idx.saturating_add(width);
    while idx < row.len() && row[idx].wide_cont {
        idx += 1;
    }
    if idx >= row.len() {
        last_selectable_col(row)
    } else {
        idx
    }
}

pub fn move_word_point<F>(
    start: SelectionPoint,
    total_rows: usize,
    mut row_at: F,
    motion: WordMotion,
) -> io::Result<SelectionPoint>
where
    F: FnMut(usize) -> io::Result<Option<Vec<Cell>>>,
{
    let Some(mut current_class) = point_class(start, total_rows, &mut row_at)? else {
        return Ok(start);
    };
    match motion {
        WordMotion::ForwardStart => {
            if current_class == WordClass::Space {
                let Some(next) = next_non_space_point(start, total_rows, &mut row_at)? else {
                    return Ok(start);
                };
                return Ok(next);
            }

            let Some(mut current) = next_point(start, total_rows, &mut row_at)? else {
                return Ok(start);
            };
            loop {
                let next_class = point_class(current, total_rows, &mut row_at)?
                    .expect("next point should have a character");
                if current.row != start.row || next_class != current_class {
                    if next_class == WordClass::Space {
                        let Some(next) = next_non_space_point(current, total_rows, &mut row_at)?
                        else {
                            return Ok(start);
                        };
                        return Ok(next);
                    }
                    return Ok(current);
                }
                let Some(next) = next_point(current, total_rows, &mut row_at)? else {
                    return Ok(start);
                };
                current = next;
            }
        }
        WordMotion::ForwardEnd => {
            let mut current = start;
            if current_class == WordClass::Space {
                let Some(next) = next_non_space_point(current, total_rows, &mut row_at)? else {
                    return Ok(start);
                };
                current = next;
                current_class = point_class(current, total_rows, &mut row_at)?
                    .expect("point should have a character");
            }
            if let Some(next) = next_point(current, total_rows, &mut row_at)? {
                let next_class = point_class(next, total_rows, &mut row_at)?
                    .expect("next point should have a character");
                if next.row != current.row || next_class != current_class {
                    let Some(next_word) = next_non_space_point(current, total_rows, &mut row_at)?
                    else {
                        return Ok(start);
                    };
                    current = next_word;
                    current_class = point_class(current, total_rows, &mut row_at)?
                        .expect("point should have a character");
                }
            }
            run_end(current, current_class, total_rows, &mut row_at)
        }
        WordMotion::BackwardStart => {
            let mut current = start;
            if current_class == WordClass::Space {
                let Some(prev) = prev_non_space_point(current, total_rows, &mut row_at)? else {
                    return Ok(start);
                };
                current = prev;
                current_class = point_class(current, total_rows, &mut row_at)?
                    .expect("point should have a character");
            }
            if let Some(prev) = prev_point(current, total_rows, &mut row_at)? {
                let prev_class = point_class(prev, total_rows, &mut row_at)?
                    .expect("previous point should have a character");
                if prev.row != current.row || prev_class != current_class {
                    let Some(prev_word) = prev_non_space_point(current, total_rows, &mut row_at)?
                    else {
                        return Ok(start);
                    };
                    current = prev_word;
                    current_class = point_class(current, total_rows, &mut row_at)?
                        .expect("point should have a character");
                }
            }
            run_start(current, current_class, total_rows, &mut row_at)
        }
    }
}

pub fn apply_selection_highlight(row: &mut [Cell], row_index: usize, selection: &Selection) {
    let Some((start, end)) = selected_columns(selection, row_index, row) else {
        return;
    };
    for cell in row.iter_mut().take(end).skip(start) {
        cell.style = highlight_style(cell.style);
    }
}

pub fn selection_text<F>(selection: Selection, mut row_at: F) -> String
where
    F: FnMut(usize) -> Option<Vec<Cell>>,
{
    let (start, end) = selection.ordered();
    let mut parts = Vec::with_capacity(selection.line_span());
    for row_index in start.row..=end.row {
        let row = row_at(row_index).unwrap_or_default();
        match selection.mode {
            SelectionMode::Line => parts.push(row_to_text(&row)),
            SelectionMode::Character => {
                if row.is_empty() {
                    parts.push(String::new());
                    continue;
                }
                let row_start = if row_index == start.row {
                    normalize_col(&row, start.col)
                } else {
                    0
                };
                let row_end = if row_index == end.row {
                    selection_end_exclusive(&row, end.col)
                } else {
                    row.len()
                };
                let start_idx = row_start.min(row_end);
                parts.push(row_to_text(&row[start_idx..row_end]));
            }
        }
    }
    parts.join("\n")
}

fn selected_columns(
    selection: &Selection,
    row_index: usize,
    row: &[Cell],
) -> Option<(usize, usize)> {
    let (start, end) = selection.ordered();
    if row_index < start.row || row_index > end.row {
        return None;
    }
    if row.is_empty() {
        return None;
    }

    match selection.mode {
        SelectionMode::Line => Some((0, row.len())),
        SelectionMode::Character => {
            let row_start = if row_index == start.row {
                normalize_col(row, start.col)
            } else {
                0
            };
            let row_end = if row_index == end.row {
                selection_end_exclusive(row, end.col)
            } else {
                row.len()
            };
            if row_start >= row_end {
                None
            } else {
                Some((row_start, row_end))
            }
        }
    }
}

fn selection_end_exclusive(row: &[Cell], col: usize) -> usize {
    if row.is_empty() {
        return 0;
    }
    let idx = normalize_col(row, col);
    idx.saturating_add(cell_width(row, idx)).min(row.len())
}

fn point_class<F>(
    point: SelectionPoint,
    total_rows: usize,
    row_at: &mut F,
) -> io::Result<Option<WordClass>>
where
    F: FnMut(usize) -> io::Result<Option<Vec<Cell>>>,
{
    Ok(point_char(point, total_rows, row_at)?.map(classify_word))
}

fn point_char<F>(
    point: SelectionPoint,
    total_rows: usize,
    row_at: &mut F,
) -> io::Result<Option<char>>
where
    F: FnMut(usize) -> io::Result<Option<Vec<Cell>>>,
{
    if point.row >= total_rows {
        return Ok(None);
    }
    let Some(row) = row_at(point.row)? else {
        return Ok(None);
    };
    if row.is_empty() {
        return Ok(None);
    }
    let col = normalize_col(&row, point.col);
    Ok(row
        .get(col)
        .filter(|cell| !cell.wide_cont)
        .map(|cell| cell.ch))
}

fn next_point<F>(
    point: SelectionPoint,
    total_rows: usize,
    row_at: &mut F,
) -> io::Result<Option<SelectionPoint>>
where
    F: FnMut(usize) -> io::Result<Option<Vec<Cell>>>,
{
    if point.row >= total_rows {
        return Ok(None);
    }

    let Some(row) = row_at(point.row)? else {
        return Ok(None);
    };
    let positions = selectable_positions(&row);
    if !positions.is_empty() {
        let current_col = normalize_col(&row, point.col);
        if let Some(idx) = positions.iter().position(|(col, _)| *col == current_col) {
            if let Some((next_col, _)) = positions.get(idx + 1) {
                return Ok(Some(SelectionPoint {
                    row: point.row,
                    col: *next_col,
                }));
            }
        }
    }

    for row_index in point.row + 1..total_rows {
        let Some(row) = row_at(row_index)? else {
            continue;
        };
        if let Some((col, _)) = selectable_positions(&row).first() {
            return Ok(Some(SelectionPoint {
                row: row_index,
                col: *col,
            }));
        }
    }
    Ok(None)
}

fn prev_point<F>(
    point: SelectionPoint,
    total_rows: usize,
    row_at: &mut F,
) -> io::Result<Option<SelectionPoint>>
where
    F: FnMut(usize) -> io::Result<Option<Vec<Cell>>>,
{
    if point.row >= total_rows {
        return Ok(None);
    }

    let Some(row) = row_at(point.row)? else {
        return Ok(None);
    };
    let positions = selectable_positions(&row);
    if !positions.is_empty() {
        let current_col = normalize_col(&row, point.col);
        if let Some(idx) = positions.iter().position(|(col, _)| *col == current_col) {
            if idx > 0 {
                return Ok(Some(SelectionPoint {
                    row: point.row,
                    col: positions[idx - 1].0,
                }));
            }
        }
    }

    for row_index in (0..point.row).rev() {
        let Some(row) = row_at(row_index)? else {
            continue;
        };
        let positions = selectable_positions(&row);
        if let Some((col, _)) = positions.last() {
            return Ok(Some(SelectionPoint {
                row: row_index,
                col: *col,
            }));
        }
    }
    Ok(None)
}

fn selectable_positions(row: &[Cell]) -> Vec<(usize, char)> {
    row.iter()
        .enumerate()
        .filter(|(_, cell)| !cell.wide_cont)
        .map(|(idx, cell)| (idx, cell.ch))
        .collect()
}

fn next_non_space_point<F>(
    point: SelectionPoint,
    total_rows: usize,
    row_at: &mut F,
) -> io::Result<Option<SelectionPoint>>
where
    F: FnMut(usize) -> io::Result<Option<Vec<Cell>>>,
{
    let mut current = point;
    while let Some(next) = next_point(current, total_rows, row_at)? {
        let next_class =
            point_class(next, total_rows, row_at)?.expect("next point should have a character");
        if next_class != WordClass::Space {
            return Ok(Some(next));
        }
        current = next;
    }
    Ok(None)
}

fn prev_non_space_point<F>(
    point: SelectionPoint,
    total_rows: usize,
    row_at: &mut F,
) -> io::Result<Option<SelectionPoint>>
where
    F: FnMut(usize) -> io::Result<Option<Vec<Cell>>>,
{
    let mut current = point;
    while let Some(prev) = prev_point(current, total_rows, row_at)? {
        let prev_class =
            point_class(prev, total_rows, row_at)?.expect("previous point should have a character");
        if prev_class != WordClass::Space {
            return Ok(Some(prev));
        }
        current = prev;
    }
    Ok(None)
}

fn run_start<F>(
    mut point: SelectionPoint,
    class: WordClass,
    total_rows: usize,
    row_at: &mut F,
) -> io::Result<SelectionPoint>
where
    F: FnMut(usize) -> io::Result<Option<Vec<Cell>>>,
{
    while let Some(prev) = prev_point(point, total_rows, row_at)? {
        let prev_class =
            point_class(prev, total_rows, row_at)?.expect("previous point should have a character");
        if prev.row != point.row || prev_class != class {
            break;
        }
        point = prev;
    }
    Ok(point)
}

fn run_end<F>(
    mut point: SelectionPoint,
    class: WordClass,
    total_rows: usize,
    row_at: &mut F,
) -> io::Result<SelectionPoint>
where
    F: FnMut(usize) -> io::Result<Option<Vec<Cell>>>,
{
    while let Some(next) = next_point(point, total_rows, row_at)? {
        let next_class =
            point_class(next, total_rows, row_at)?.expect("next point should have a character");
        if next.row != point.row || next_class != class {
            break;
        }
        point = next;
    }
    Ok(point)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WordClass {
    Space,
    Word,
    Punct,
}

fn classify_word(ch: char) -> WordClass {
    if ch.is_whitespace() {
        WordClass::Space
    } else if ch.is_alphanumeric() || ch == '_' {
        WordClass::Word
    } else {
        WordClass::Punct
    }
}

fn cell_width(row: &[Cell], idx: usize) -> usize {
    if idx + 1 < row.len() && row[idx + 1].wide_cont {
        2
    } else {
        1
    }
}

fn highlight_style(style: Style) -> Style {
    Style {
        bg: Some(238),
        reverse: false,
        dim: false,
        ..style
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(text: &str) -> Vec<Cell> {
        text.chars()
            .map(|ch| Cell {
                ch,
                style: Style::default(),
                wide_cont: false,
            })
            .collect()
    }

    #[test]
    fn character_selection_spans_multiple_rows() {
        let selection = Selection {
            mode: SelectionMode::Character,
            anchor: SelectionPoint { row: 0, col: 1 },
            cursor: SelectionPoint { row: 2, col: 1 },
        };
        let rows = [row("alpha"), row("beta"), row("gamma")];
        let text = selection_text(selection, |idx| rows.get(idx).cloned());
        assert_eq!(text, "lpha\nbeta\nga");
    }

    #[test]
    fn line_selection_copies_whole_rows() {
        let selection = Selection {
            mode: SelectionMode::Line,
            anchor: SelectionPoint { row: 2, col: 3 },
            cursor: SelectionPoint { row: 0, col: 0 },
        };
        let rows = [row("one"), row("two"), row("three")];
        let text = selection_text(selection, |idx| rows.get(idx).cloned());
        assert_eq!(text, "one\ntwo\nthree");
    }

    #[test]
    fn word_motion_moves_across_rows() {
        let rows = [row("foo bar"), row("baz qux")];
        let start = SelectionPoint { row: 0, col: 0 };

        let next = move_word_point(
            start,
            rows.len(),
            |idx| Ok(rows.get(idx).cloned()),
            WordMotion::ForwardStart,
        )
        .expect("word motion should succeed");
        let end = move_word_point(
            next,
            rows.len(),
            |idx| Ok(rows.get(idx).cloned()),
            WordMotion::ForwardEnd,
        )
        .expect("word motion should succeed");
        let back = move_word_point(
            end,
            rows.len(),
            |idx| Ok(rows.get(idx).cloned()),
            WordMotion::BackwardStart,
        )
        .expect("word motion should succeed");

        assert_eq!(next, SelectionPoint { row: 0, col: 4 });
        assert_eq!(end, SelectionPoint { row: 0, col: 6 });
        assert_eq!(back, SelectionPoint { row: 0, col: 4 });
    }

    #[test]
    fn word_motion_treats_punctuation_as_separate_word() {
        let rows = [row("foo::bar")];
        let start = SelectionPoint { row: 0, col: 0 };

        let next = move_word_point(
            start,
            rows.len(),
            |idx| Ok(rows.get(idx).cloned()),
            WordMotion::ForwardStart,
        )
        .expect("word motion should succeed");
        let next2 = move_word_point(
            next,
            rows.len(),
            |idx| Ok(rows.get(idx).cloned()),
            WordMotion::ForwardStart,
        )
        .expect("word motion should succeed");

        assert_eq!(next, SelectionPoint { row: 0, col: 3 });
        assert_eq!(next2, SelectionPoint { row: 0, col: 5 });
    }

    #[test]
    fn forward_end_moves_to_next_word_when_already_at_run_end() {
        let rows = [row("foo bar")];
        let start = SelectionPoint { row: 0, col: 2 };

        let next = move_word_point(
            start,
            rows.len(),
            |idx| Ok(rows.get(idx).cloned()),
            WordMotion::ForwardEnd,
        )
        .expect("word motion should succeed");

        assert_eq!(next, SelectionPoint { row: 0, col: 6 });
    }

    #[test]
    fn backward_start_moves_to_previous_word_when_already_at_run_start() {
        let rows = [row("foo bar")];
        let start = SelectionPoint { row: 0, col: 4 };

        let prev = move_word_point(
            start,
            rows.len(),
            |idx| Ok(rows.get(idx).cloned()),
            WordMotion::BackwardStart,
        )
        .expect("word motion should succeed");

        assert_eq!(prev, SelectionPoint { row: 0, col: 0 });
    }
}
