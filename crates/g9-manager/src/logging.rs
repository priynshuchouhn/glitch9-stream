//! Bounded manager logging for the long-running Windows service.

use anyhow::{Context, Result};
use std::fs::{File, OpenOptions};
use std::io::{self, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing_subscriber::fmt::MakeWriter;

const RETENTION: Duration = Duration::from_secs(3 * 24 * 60 * 60);

struct State {
    file: File,
    marker: PathBuf,
    last_truncated: SystemTime,
}

pub struct ThreeDayLog {
    state: Mutex<State>,
}

pub struct LogGuard<'a>(MutexGuard<'a, State>);

impl Write for LogGuard<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.truncate_if_due()?;
        self.0.file.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.file.flush()
    }
}

impl State {
    fn truncate_if_due(&mut self) -> io::Result<()> {
        let now = SystemTime::now();
        if now.duration_since(self.last_truncated).unwrap_or_default() < RETENTION {
            return Ok(());
        }
        self.file.set_len(0)?;
        self.file.seek(SeekFrom::Start(0))?;
        self.last_truncated = now;
        let _ = std::fs::write(
            &self.marker,
            now.duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
                .to_string(),
        );
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for ThreeDayLog {
    type Writer = LogGuard<'a>;

    fn make_writer(&'a self) -> Self::Writer {
        LogGuard(
            self.state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        )
    }
}

fn rotation_time(marker: &Path, log: &Path) -> SystemTime {
    std::fs::read_to_string(marker)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(|seconds| UNIX_EPOCH + Duration::from_secs(seconds))
        .or_else(|| std::fs::metadata(log).ok()?.created().ok())
        .unwrap_or_else(SystemTime::now)
}

pub fn init(log_dir: &str) -> Result<()> {
    let directory = Path::new(log_dir);
    std::fs::create_dir_all(directory)
        .with_context(|| format!("create manager log directory {}", directory.display()))?;
    let path = directory.join("mgr.log");
    let marker = directory.join("mgr.log.rotation");
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .read(true)
        .open(&path)
        .with_context(|| format!("open manager log {}", path.display()))?;
    let writer = ThreeDayLog {
        state: Mutex::new(State {
            file,
            last_truncated: rotation_time(&marker, &path),
            marker,
        }),
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .with_ansi(false)
        .with_writer(writer)
        .init();
    Ok(())
}
