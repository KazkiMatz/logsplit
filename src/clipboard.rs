use std::io::{self, Write};
use std::process::{Command, Stdio};

#[derive(Clone, Copy)]
struct ClipboardCommand {
    program: &'static str,
    args: &'static [&'static str],
}

const COPY_COMMANDS: &[ClipboardCommand] = &[
    ClipboardCommand {
        program: "pbcopy",
        args: &[],
    },
    ClipboardCommand {
        program: "wl-copy",
        args: &[],
    },
    ClipboardCommand {
        program: "xclip",
        args: &["-selection", "clipboard"],
    },
    ClipboardCommand {
        program: "xsel",
        args: &["--clipboard", "--input"],
    },
];

const PASTE_COMMANDS: &[ClipboardCommand] = &[
    ClipboardCommand {
        program: "pbpaste",
        args: &[],
    },
    ClipboardCommand {
        program: "wl-paste",
        args: &["-n"],
    },
    ClipboardCommand {
        program: "xclip",
        args: &["-selection", "clipboard", "-o"],
    },
    ClipboardCommand {
        program: "xsel",
        args: &["--clipboard", "--output"],
    },
];

pub fn copy_to_clipboard(text: &str) -> io::Result<()> {
    let mut last_err = None;
    for command in COPY_COMMANDS {
        let spawn = Command::new(command.program)
            .args(command.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        match spawn {
            Ok(mut child) => {
                if let Some(mut stdin) = child.stdin.take() {
                    stdin.write_all(text.as_bytes())?;
                }
                let status = child.wait()?;
                if status.success() {
                    return Ok(());
                }
                last_err = Some(io::Error::other(format!(
                    "{} exited with status {}",
                    command.program, status
                )));
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                last_err = Some(err);
            }
            Err(err) => return Err(err),
        }
    }

    Err(last_err.unwrap_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "no supported clipboard command found",
        )
    }))
}

pub fn paste_from_clipboard() -> io::Result<String> {
    let mut last_err = None;
    for command in PASTE_COMMANDS {
        match Command::new(command.program).args(command.args).output() {
            Ok(output) => {
                if output.status.success() {
                    return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
                }
                last_err = Some(io::Error::other(format!(
                    "{} exited with status {}",
                    command.program, output.status
                )));
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                last_err = Some(err);
            }
            Err(err) => return Err(err),
        }
    }

    Err(last_err.unwrap_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "no supported clipboard command found",
        )
    }))
}
