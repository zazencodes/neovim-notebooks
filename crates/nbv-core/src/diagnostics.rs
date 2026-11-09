//! Bounded process diagnostics. Payloads (source, prompts, environment and RPC arguments)
//! must never be written to the lifecycle log.
use std::collections::VecDeque;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncRead, AsyncReadExt};

pub const STDERR_BYTES: usize = 16 * 1024;
const LOG_BYTES: u64 = 64 * 1024;
static LOG_LOCK: Mutex<()> = Mutex::new(());
pub type StderrTail = Arc<Mutex<VecDeque<u8>>>;

pub fn stderr_tail() -> StderrTail {
    Arc::new(Mutex::new(VecDeque::with_capacity(STDERR_BYTES)))
}

pub fn tail_text(tail: &StderrTail) -> String {
    String::from_utf8_lossy(&tail.lock().expect("stderr lock poisoned").iter().copied().collect::<Vec<_>>())
        .into_owned()
}

/// Fixed-size reads also bound memory for streams containing no newline.
pub async fn read_stderr(mut reader: impl AsyncRead + Unpin, tail: StderrTail) -> std::io::Result<()> {
    let mut chunk = [0; 2048];
    loop {
        let n = reader.read(&mut chunk).await?;
        if n == 0 {
            return Ok(());
        }
        let mut bytes = tail.lock().expect("stderr lock poisoned");
        let discard = (bytes.len() + n).saturating_sub(STDERR_BYTES);
        bytes.drain(..discard);
        bytes.extend(&chunk[..n]);
    }
}

pub fn exit_status(status: &std::io::Result<std::process::ExitStatus>) -> String {
    match status {
        Ok(status) => status.to_string(),
        Err(error) => format!("cannot wait for process: {error}"),
    }
}

pub fn log_path() -> std::io::Result<PathBuf> {
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
        .ok_or_else(|| std::io::Error::other("neither XDG_STATE_HOME nor HOME is set"))?;
    Ok(state.join("nbv/diagnostics.log"))
}

/// Records only caller-supplied lifecycle labels and numeric status, never raw errors.
pub fn record(component: &str, event: &str, generation: u64) -> std::io::Result<()> {
    let _lock = LOG_LOCK.lock().expect("diagnostics lock poisoned");
    let path = log_path()?;
    record_at(&path, component, event, generation)
}

fn record_at(path: &std::path::Path, component: &str, event: &str, generation: u64) -> std::io::Result<()> {
    let time = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
    let line = format!("{time} {component} generation={generation} {event}\n");
    if line.len() as u64 > LOG_BYTES {
        return Err(std::io::Error::other("diagnostic record exceeds log size"));
    }
    std::fs::create_dir_all(path.parent().expect("log parent"))?;
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    // Separate processes can run different notebooks concurrently. Serialize rotation
    // with a stable, empty lock file, whose lifetime is independent of either log file.
    let lock = options.open(path.with_extension("lock"))?;
    lock.lock()?;
    if std::fs::metadata(path).is_ok_and(|m| m.len() + line.len() as u64 > LOG_BYTES) {
        std::fs::rename(path, path.with_extension("log.1"))?;
    }
    let mut file = options.open(path)?;
    file.write_all(line.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn lifecycle_log_keeps_only_two_bounded_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("diagnostics.log");
        for _ in 0..5000 {
            record_at(&path, "kernel", "shell read failed", 7).unwrap();
        }
        for file in [&path, &path.with_extension("log.1")] {
            assert!(std::fs::metadata(file).unwrap().len() <= LOG_BYTES);
            let text = std::fs::read_to_string(file).unwrap();
            assert!(text.lines().all(|line| line.ends_with("kernel generation=7 shell read failed")));
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(std::fs::metadata(file).unwrap().permissions().mode() & 0o777, 0o600);
            }
        }
        assert_eq!(std::fs::metadata(path.with_extension("lock")).unwrap().len(), 0);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 3);
    }

    #[tokio::test]
    async fn huge_unterminated_stderr_is_byte_bounded() {
        let mut data = vec![b'x'; STDERR_BYTES * 16];
        data.extend_from_slice(b"final error");
        let tail = stderr_tail();
        read_stderr(data.as_slice(), tail.clone()).await.unwrap();
        assert_eq!(tail.lock().unwrap().len(), STDERR_BYTES);
        assert!(tail_text(&tail).ends_with("final error"));
    }
}
