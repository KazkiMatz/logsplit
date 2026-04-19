pub mod clipboard;
pub mod debug;
pub mod render;
pub mod selection;
pub mod terminal;
pub mod ui;
pub mod viewer;

pub use clipboard::{copy_to_clipboard, paste_from_clipboard};
pub use debug::{debug_log, debug_timing};
pub use render::{
    apply_style, cell_prefix_width, clear_segment, common_prefix_len, draw_cells, overlay_cells,
};
pub use selection::{
    Selection, SelectionMode, SelectionPoint, WordMotion, apply_selection_highlight,
    first_selectable_col, last_selectable_col, move_word_point, next_col, normalize_col,
    previous_col, selection_text,
};
pub use terminal::{
    Cell, Style, VirtualTerminal, char_width, clamp, clamp_signed, decode_utf8_chunk, row_to_text,
    trim_row, wrap_styled_line,
};
pub use ui::TerminalGuard;
pub use viewer::{ReplayFile, ViewerCore};
