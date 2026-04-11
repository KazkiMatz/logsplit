use std::io;

use crossterm::cursor::{Hide, Show};
use crossterm::execute;
use crossterm::style::{Attribute, ResetColor, SetAttribute};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};

use crate::debug_log;

pub struct TerminalGuard;

impl TerminalGuard {
    pub fn enter(stdout: &mut io::Stdout) -> io::Result<Self> {
        debug_log("TerminalGuard::enter enable_raw_mode");
        enable_raw_mode()?;
        debug_log("TerminalGuard::enter execute EnterAlternateScreen");
        execute!(stdout, EnterAlternateScreen, Hide)?;
        debug_log("TerminalGuard::enter done");
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let mut stdout = io::stdout();
        let _ = execute!(
            stdout,
            Show,
            LeaveAlternateScreen,
            ResetColor,
            SetAttribute(Attribute::Reset)
        );
    }
}
