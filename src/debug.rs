use std::io::Write;
use std::path::PathBuf;

pub fn debug_log(message: &str) {
    let Some(path) = std::env::var_os("LOGSPLIT_DEBUG") else {
        return;
    };
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(PathBuf::from(path))
    else {
        return;
    };
    let _ = writeln!(file, "{message}");
}
