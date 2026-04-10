# logsplit

`logsplit` is a terminal log splitter for long-running interactive commands.

It runs your command in the right pane under `script(1)`, writes the raw terminal transcript to a log file, and shows a less-like live viewer for that same log on the left. The split is rendered by the app itself, so it does not depend on `tmux` and does not need GNU Screen regions.

## What It Does

- Runs any command line in a PTY on the right side.
- Captures the command's terminal transcript to a log file.
- Shows an embedded log viewer on the left side with independent scrolling.
- Works well inside another terminal multiplexer because the split is handled internally.

## Binaries

This repository currently builds two binaries:

- `logsplit`
  Runs a command with a left-side embedded log viewer and a right-side live terminal.
- `logsplit-rs`
  A standalone less-like viewer for terminal transcript logs.

## Build

```bash
cargo build --release
```

The binaries will be available under `target/release/`.

## Usage

Run a command in the split view:

```bash
target/release/logsplit claude --resume
```

You can also pass any other command:

```bash
target/release/logsplit htop
target/release/logsplit "python manage.py shell"
target/release/logsplit tail -f /var/log/system.log
```

Use the standalone viewer directly:

```bash
target/release/logsplit-rs --follow ~/.logsplit/some-logfile.log
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

## logsplit Keybindings

`logsplit` uses `Ctrl-g` as its own prefix so it does not fight Screen's prefix.

- `Ctrl-g Tab`
  Toggle focus between left and right panes.
- `Ctrl-g h`
  Focus the left pane.
- `Ctrl-g l`
  Focus the right pane.
- `Ctrl-g q`
  Quit `logsplit`.

When the right pane has focus, normal input is forwarded to the command running there.

## Left Pane Viewer Keys

The embedded left-side viewer uses less-like navigation:

- `j`, `Down`, `Enter`, `Ctrl-n`, `Ctrl-e`
  Scroll down one line.
- `k`, `Up`, `Ctrl-y`
  Scroll up one line.
- `Space`, `PageDown`, `f`, `Ctrl-f`
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
  Follow mode.
- `Ctrl-c`
  Stop follow mode.
- `/`
  Search.
- `n`
  Repeat search forward.
- `N`
  Repeat search backward.
- `h`, `?`
  Show help text in the status line.

## Notes

- `logsplit` preserves the caller's current working directory for the spawned command.
- The log format is a raw terminal transcript, not a cleaned plain-text export.
- The embedded viewer reconstructs screen output from terminal control sequences, which makes it useful for full-screen and redraw-heavy terminal applications.
- The project is currently optimized for Unix-like systems with `script(1)` available.

## Repository Layout

- `src/bin/logsplit.rs`
  Main split-view command runner.
- `src/main.rs`
  Standalone transcript viewer binary.
