use std::ffi::OsString;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResizeEvent {
    pub offset: u64,
    pub rows: usize,
    pub cols: usize,
}

pub fn resize_events_path(log_path: &Path) -> PathBuf {
    let mut path = OsString::from(log_path.as_os_str());
    path.push(".resize");
    PathBuf::from(path)
}

pub fn load_resize_events(log_path: &Path) -> io::Result<Vec<ResizeEvent>> {
    let path = resize_events_path(log_path);
    let file = match File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err),
    };

    let mut events = Vec::new();
    for line in BufReader::new(file).lines() {
        let line = line?;
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let mut parts = line.split('\t');
        let Some(offset) = parts.next().and_then(|value| value.parse::<u64>().ok()) else {
            continue;
        };
        let Some(rows) = parts.next().and_then(|value| value.parse::<usize>().ok()) else {
            continue;
        };
        let Some(cols) = parts.next().and_then(|value| value.parse::<usize>().ok()) else {
            continue;
        };
        if rows == 0 || cols == 0 {
            continue;
        }
        events.push(ResizeEvent { offset, rows, cols });
    }

    Ok(events)
}

#[derive(Debug)]
pub struct TranscriptRecorder {
    logfile: File,
    resizefile: File,
    log_offset: u64,
    last_size: Option<(usize, usize)>,
}

impl TranscriptRecorder {
    pub fn create(log_path: &Path, rows: usize, cols: usize) -> io::Result<Self> {
        let logfile = File::options().append(true).open(log_path)?;
        let log_offset = logfile.metadata()?.len();
        let resizefile = File::create(resize_events_path(log_path))?;
        let mut recorder = Self {
            logfile,
            resizefile,
            log_offset,
            last_size: None,
        };
        recorder.record_resize(rows, cols)?;
        Ok(recorder)
    }

    pub fn append_bytes(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.logfile.write_all(bytes)?;
        self.log_offset += bytes.len() as u64;
        Ok(())
    }

    pub fn record_resize(&mut self, rows: usize, cols: usize) -> io::Result<()> {
        if rows == 0 || cols == 0 {
            return Ok(());
        }
        if self.last_size == Some((rows, cols)) {
            return Ok(());
        }
        writeln!(self.resizefile, "{}\t{}\t{}", self.log_offset, rows, cols)?;
        self.resizefile.flush()?;
        self.last_size = Some((rows, cols));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{ResizeEvent, TranscriptRecorder, load_resize_events, resize_events_path};
    use std::fs;
    use std::path::Path;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_log_path(name: &str) -> std::path::PathBuf {
        let mut path = std::env::temp_dir();
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        path.push(format!("logsplit-test-{name}-{nonce}.log"));
        path
    }

    #[test]
    fn resize_events_path_appends_sidecar_suffix() {
        let path = resize_events_path(Path::new("/tmp/example.log"));
        assert_eq!(path, Path::new("/tmp/example.log.resize"));
    }

    #[test]
    fn transcript_recorder_writes_initial_and_incremental_resize_events() {
        let log_path = temp_log_path("recorder");
        fs::write(&log_path, b"").unwrap();
        let mut recorder = TranscriptRecorder::create(&log_path, 24, 80).unwrap();
        recorder.append_bytes(b"hello").unwrap();
        recorder.record_resize(24, 80).unwrap();
        recorder.record_resize(40, 100).unwrap();

        let events = load_resize_events(&log_path).unwrap();
        assert_eq!(
            events,
            vec![
                ResizeEvent {
                    offset: 0,
                    rows: 24,
                    cols: 80,
                },
                ResizeEvent {
                    offset: 5,
                    rows: 40,
                    cols: 100,
                },
            ]
        );

        let _ = fs::remove_file(&log_path);
        let _ = fs::remove_file(resize_events_path(&log_path));
    }
}
