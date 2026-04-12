# logsplit

`logsplit` runs an interactive command in a live right-hand terminal pane, records the full terminal transcript to a logfile, and shows a reconstructed viewer for that same transcript on the left.

The split is rendered by the app itself, so it works inside another terminal multiplexer without depending on `tmux` panes or GNU Screen regions.

## Binaries

This repository builds two binaries:

- `logsplit`
  Runs a command with an embedded left-side transcript viewer and a right-side live terminal.
- `logsplit-rs`
  Opens a standalone viewer for an existing transcript log.

## What It Does

- Spawns your command in a PTY on the right side.
- Records the raw terminal transcript directly from the PTY stream.
- Replays that transcript into a virtual terminal for the left-side viewer.
- Lets you scroll, search, copy, and inspect the log independently from the live terminal.
- Preserves the caller's current working directory for the spawned command.

## Build

```bash
cargo build --release
```

The binaries will be written to:

```text
target/release/logsplit
target/release/logsplit-rs
```

## Usage

Run a command in split view:

```bash
target/release/logsplit claude --resume
```

Any shell command line works:

```bash
target/release/logsplit htop
target/release/logsplit python manage.py shell
target/release/logsplit tail -f /var/log/system.log
```

Open an existing logfile directly in the standalone viewer:

```bash
target/release/logsplit-rs --follow ~/.logsplit/some-logfile.log
```

The standalone viewer also supports offline text dumps:

```bash
target/release/logsplit-rs --dump --tail 200 ~/.logsplit/some-logfile.log
```

## Log Files

Logs are written under:

```text
~/.logsplit
```

The filename format is:

```text
<command-slug>-<screen-session>-w<window>-<unix-timestamp>.log
```

Example:

```text
claude-5317.ttys000.m1pro-w7-1775820282.log
```

Notes:

- `command-slug` is derived from the first token of the command line.
- `screen-session` comes from `$STY` when running inside GNU Screen, otherwise `nosession`.
- `window` comes from `$WINDOW` when running inside GNU Screen, otherwise `noscreen`.
- The log content is a raw terminal transcript, not a cleaned plain-text export.

## logsplit Controls

When the right pane has focus, normal input is forwarded to the child process.
`Ctrl-w` is reserved by `logsplit` for pane switching and is not forwarded to the child.
`Ctrl-g` is used for global helper actions that should work from either pane.

Global controls:

- `Ctrl-w h`
  Focus the left pane.
- `Ctrl-w l`
  Focus the right pane.
- `Ctrl-g q`
  Quit `logsplit`.
- `Ctrl-g v`
  Start characterwise visual selection in the currently focused pane.
- `Ctrl-g V`
  Start linewise visual selection in the currently focused pane.
- `Ctrl-g p`
  Paste clipboard contents into the right pane.

## Left Pane Viewer

The embedded left-side viewer has a persistent cursor and Vim-like navigation.

Cursor movement:

- `h`, `j`, `k`, `l`
  Move the left-pane cursor.
- `Left`, `Right`, `Up`, `Down`
  Move by one cell or row.
- `Enter`
  Move down one row.
- `Ctrl-n`
  Move down one row.
- `w`
  Move to the next word start.
- `e`
  Move to the end of the next word.
- `b`
  Move to the previous word start.
- `0`
  Move to column `0`.
- `$`
  Move to the end of the current row.
- `g`, `Home`
  Jump to the start of the file.
- `G`, `End`
  Jump to the end of the file.
- `H`
  Move to the top visible row.
- `M`
  Move to the middle visible row.
- `L`
  Move to the bottom visible row.

Scrolling and follow:

- `Ctrl-e`
  Scroll down one line while keeping the current file position when possible.
- `Ctrl-y`
  Scroll up one line while keeping the current file position when possible.
- `Space`, `f`, `PageDown`, `Ctrl-f`
  Page down.
- `Ctrl-b`, `PageUp`
  Page up.
- `d`, `Ctrl-d`
  Half-page down.
- `u`, `Ctrl-u`
  Half-page up.
- `F`
  Enable follow mode.
- `Ctrl-c`
  Stop follow mode.
- `r`
  Reload the left viewer from the logfile.

Search:

- `/`
  Search forward.
- `n`
  Repeat the previous search forward.
- `N`
  Repeat the previous search backward.

Selection and clipboard:

- `v`
  Start characterwise visual selection.
- `V`
  Start linewise visual selection.
- `y`, `Y`
  Copy the active selection to the system clipboard.
- `Esc`
  Clear the active selection.
- `p`, `P`
  Paste clipboard contents into the right pane.

Other:

- `?`
  Show left-pane help in the status line.

## logsplit-rs Viewer

`logsplit-rs` is the standalone transcript viewer. Its controls are currently simpler than the embedded `logsplit` left pane.

Navigation:

- `j`, `Down`, `Enter`, `Ctrl-n`, `Ctrl-e`
  Scroll down one line.
- `k`, `Up`, `Ctrl-y`
  Scroll up one line.
- `Space`, `f`, `PageDown`, `Ctrl-f`
  Page down.
- `b`, `PageUp`, `Ctrl-b`
  Page up.
- `d`, `Ctrl-d`
  Half-page down.
- `u`, `Ctrl-u`
  Half-page up.
- `g`, `Home`
  Jump to the top.
- `G`, `End`
  Jump to the end.
- `F`
  Enable follow mode.
- `Ctrl-c`
  Stop follow mode.
- `r`
  Reload the viewer.

Search:

- `/`
  Search forward.
- `n`
  Repeat the previous search forward.
- `N`
  Repeat the previous search backward.

Selection and clipboard:

- `v`
  Start characterwise visual selection.
- `V`
  Start linewise visual selection.
- `h`, `j`, `k`, `l`
  Move the selection cursor.
- `0`
  Move the selection cursor to column `0`.
- `$`
  Move the selection cursor to the end of the current row.
- `y`, `Y`
  Copy the selection to the system clipboard.
- `Esc`
  Clear the selection.

Quit:

- `q`
  Exit the viewer.

## Notes

- `logsplit` launches the child command directly as `/bin/zsh -lc <line>` inside a PTY and writes the PTY output into the logfile itself.
- The left-side viewer reconstructs terminal state by replaying escape sequences, so it is useful even for redraw-heavy CLI programs.
- The project is currently oriented toward Unix-like systems with PTY support and `/bin/zsh` available.

## Repository Layout

- `src/bin/logsplit.rs`
  Split-view command runner with embedded viewer.
- `src/main.rs`
  Standalone transcript viewer binary.
- `src/selection.rs`
  Shared selection and word-motion logic.
- `src/viewer.rs`
  Transcript replay and layout cache.
