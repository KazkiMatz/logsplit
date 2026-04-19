use std::io::Write;
use std::path::PathBuf;
use std::time::Instant;

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

pub fn debug_timing<F>(label: &str, start: Instant, details: F)
where
    F: FnOnce() -> String,
{
    if std::env::var_os("LOGSPLIT_DEBUG").is_none() {
        return;
    }
    let elapsed = start.elapsed();
    let threshold_ms = std::env::var("LOGSPLIT_DEBUG_TIMING_MS")
        .ok()
        .and_then(|value| value.parse::<u128>().ok())
        .unwrap_or(10);
    if elapsed.as_millis() < threshold_ms {
        return;
    }

    let detail = details();
    let mut message = format!("TIMING {label} {:.3}ms", elapsed.as_secs_f64() * 1000.0);
    if !detail.is_empty() {
        message.push(' ');
        message.push_str(&detail);
    }
    debug_log(&message);
}
