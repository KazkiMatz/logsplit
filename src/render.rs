use std::cmp::{max, min};
use std::io;

use crossterm::cursor::MoveTo;
use crossterm::queue;
use crossterm::style::{
    Attribute, Color, Print, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor,
};

use crate::terminal::{Cell, Style, char_width};

pub fn apply_style(stdout: &mut io::Stdout, style: Style) -> io::Result<()> {
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

pub fn draw_cells(
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

pub fn common_prefix_len(left: &[Cell], right: &[Cell]) -> usize {
    left.iter()
        .zip(right.iter())
        .take_while(|(a, b)| a == b)
        .count()
}

pub fn cell_prefix_width(cells: &[Cell], count: usize) -> usize {
    cells
        .iter()
        .take(count)
        .filter(|cell| !cell.wide_cont)
        .map(|cell| max(char_width(cell.ch), 1))
        .sum()
}

pub fn overlay_cells(dest: &mut [Cell], x_offset: usize, cells: &[Cell], max_width: usize) {
    let end = min(dest.len(), x_offset.saturating_add(max_width));
    let mut x = x_offset;
    for cell in cells {
        if cell.wide_cont {
            continue;
        }
        let width = max(char_width(cell.ch), 1);
        if x + width > end {
            break;
        }
        dest[x] = *cell;
        if width > 1 {
            for fill_x in x + 1..min(x + width, end) {
                dest[fill_x] = Cell {
                    ch: ' ',
                    style: cell.style,
                    wide_cont: true,
                };
            }
        }
        x += width;
    }
}

pub fn clear_segment(
    stdout: &mut io::Stdout,
    y: u16,
    x_offset: usize,
    width: usize,
) -> io::Result<()> {
    if width == 0 {
        return Ok(());
    }
    queue!(
        stdout,
        MoveTo(x_offset as u16, y),
        SetAttribute(Attribute::Reset),
        ResetColor,
        Print(" ".repeat(width))
    )?;
    Ok(())
}
