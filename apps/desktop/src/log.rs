use std::io::Write;
use std::path::PathBuf;
use std::sync::LazyLock;

static LOG_PATH: LazyLock<PathBuf> = LazyLock::new(pick_path);

fn candidates() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(value) = std::env::var_os("SISKIN_S3_LOG") {
        paths.push(PathBuf::from(value));
    }
    if let Ok(executable) = std::env::current_exe() {
        if let Some(directory) = executable.parent() {
            paths.push(directory.join("siskin-s3-probe.log"));
        }
    }
    paths.push(std::env::temp_dir().join("siskin-s3-probe.log"));
    paths
}

fn pick_path() -> PathBuf {
    for candidate in candidates() {
        if std::fs::write(&candidate, b"").is_ok() {
            return candidate;
        }
    }
    std::env::temp_dir().join("siskin-s3-probe.log")
}

/// Spike diagnostics land in a file because release builds use the Windows
/// subsystem and therefore have no console attached. The first writable
/// candidate wins: `SISKIN_S3_LOG`, next to the executable, then TEMP.
pub fn path() -> PathBuf {
    LOG_PATH.clone()
}

pub fn reset() {
    let _ = std::fs::write(path(), b"");
}

pub fn line(message: &str) {
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path())
    {
        let _ = writeln!(file, "{message}");
    }
}
