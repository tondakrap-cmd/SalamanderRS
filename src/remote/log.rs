//! Audit log of remote work in `~/.local/state/salamander-rs/remote.log` (mode 0600).
//! Secrets are never written; the log rotates at 5 MiB into `remote.log.1`.

use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::sync::Mutex;

const MAX_SIZE: u64 = 5 * 1024 * 1024;

static LOCK: Mutex<()> = Mutex::new(());

pub fn log_path() -> PathBuf {
    // Tests must never write into the user's real audit log.
    if cfg!(test) {
        return std::env::temp_dir().join(format!("salamander-rs-test-{}", std::process::id())).join("remote.log");
    }
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
                .join(".local")
                .join("state")
        });
    state.join("salamander-rs").join("remote.log")
}

fn sanitize(text: &str) -> String {
    text.chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect()
}

/// Appends one line: time, server, action and detail.
pub fn log(server: &str, action: &str, detail: &str) {
    write_line(&log_path(), server, action, detail);
}

fn write_line(path: &std::path::Path, server: &str, action: &str, detail: &str) {
    let _guard = LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if fs::metadata(path).is_ok_and(|metadata| metadata.len() > MAX_SIZE) {
        let _ = fs::rename(path, path.with_extension("log.1"));
    }
    let line = format!(
        "{}  {:<28}  {:<10}  {}\n",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S%z"),
        sanitize(server),
        sanitize(action),
        sanitize(detail)
    );
    if let Ok(mut file) = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
    {
        let _ = file.write_all(line.as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn writes_sanitized_lines_with_private_permissions() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state/remote.log");
        write_line(&path, "me@host", "UPLOAD", "/a\n/b -> /c");
        write_line(&path, "me@host", "DELETE", "/c");
        let text = fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("me@host") && lines[0].contains("UPLOAD") && lines[0].contains("/a /b -> /c"));
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    }
}
