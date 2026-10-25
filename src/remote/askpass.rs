//! SSH_ASKPASS bridge: ssh runs this very program as its askpass helper, which forwards the
//! prompt over a private Unix socket to the running application and prints the answer.
//!
//! The socket lives in a 0700 runtime directory and every request must carry a random
//! one-time token, so other users cannot query it and the password never appears on a
//! command line, in a file or in the environment.

use std::io::{self, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use crate::sodium::{self, Secret};

pub const SOCKET_ENV: &str = "SALAMANDER_ASKPASS_SOCKET";
pub const TOKEN_ENV: &str = "SALAMANDER_ASKPASS_TOKEN";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PromptKind {
    Password,
    Passphrase,
    /// "Are you sure you want to continue connecting (yes/no/[fingerprint])?"
    HostKey,
    Other,
}

pub fn classify(prompt: &str) -> PromptKind {
    let lower = prompt.to_lowercase();
    if lower.contains("(yes/no") || lower.contains("continue connecting") {
        PromptKind::HostKey
    } else if lower.contains("passphrase") {
        PromptKind::Passphrase
    } else if lower.contains("password") {
        PromptKind::Password
    } else {
        PromptKind::Other
    }
}

/// Private directory for sockets (askpass and ssh ControlMaster), mode 0700.
pub fn runtime_dir() -> io::Result<PathBuf> {
    // Without XDG_RUNTIME_DIR a directory of its own in /tmp: the temporary-file directory
    // `salamander-rs-<uid>` is swept at startup and must not hold sockets.
    let dir = match std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).filter(|path| path.is_dir()) {
        Some(base) => base.join("salamander-rs"),
        None => std::env::temp_dir().join(format!("salamander-rs-run-{}", crate::sys::effective_uid())),
    };
    std::fs::create_dir_all(&dir)?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::symlink_metadata(&dir)?;
    if metadata.uid() != crate::sys::effective_uid() || metadata.file_type().is_symlink() {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "runtime adresář nepatří uživateli"));
    }
    Ok(dir)
}

/// Removes askpass sockets of instances that are no longer running (killed or crashed) and
/// terminal scripts that were never started.
pub fn cleanup_stale() {
    let Ok(dir) = runtime_dir() else { return };
    let Ok(entries) = std::fs::read_dir(&dir) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let stale = match name.strip_prefix("askpass-") {
            Some(rest) => {
                let pid = rest.split('-').next().unwrap_or_default();
                pid.is_empty() || !pid.bytes().all(|byte| byte.is_ascii_digit()) || !Path::new("/proc").join(pid).exists()
            }
            // Another instance may be about to start one, so only old scripts are removed.
            None => {
                name.starts_with("term-")
                    && name.ends_with(".sh")
                    && entry
                        .metadata()
                        .and_then(|metadata| metadata.modified())
                        .is_ok_and(|modified| modified.elapsed().unwrap_or_default() > Duration::from_secs(60))
            }
        };
        if stale {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

type Handler = dyn Fn(PromptKind, &str) -> Option<Secret> + Send + Sync;

pub struct AskpassServer {
    pub socket: PathBuf,
    pub token: String,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl AskpassServer {
    pub fn start(handler: Box<Handler>) -> io::Result<AskpassServer> {
        let socket = runtime_dir()?.join(format!("askpass-{}-{}", std::process::id(), sodium::random_hex(8)));
        let listener = UnixListener::bind(&socket)?;
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        let token = sodium::random_hex(32);
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let thread_token = token.clone();
        let handler: Arc<Handler> = Arc::from(handler);
        let thread = std::thread::spawn(move || {
            while !thread_stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        // Each prompt may wait for the user, so it gets its own thread.
                        let handler = Arc::clone(&handler);
                        let token = thread_token.clone();
                        std::thread::spawn(move || {
                            let _ = serve(stream, &token, handler.as_ref());
                        });
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    Err(_) => break,
                }
            }
        });
        Ok(AskpassServer {
            socket,
            token,
            stop,
            thread: Some(thread),
        })
    }

    /// Environment for the ssh process.
    pub fn environment(&self) -> Vec<(String, String)> {
        let program = std::env::current_exe()
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "salamander-rs".to_owned());
        vec![
            ("SSH_ASKPASS".to_owned(), program),
            ("SSH_ASKPASS_REQUIRE".to_owned(), "force".to_owned()),
            (SOCKET_ENV.to_owned(), self.socket.to_string_lossy().into_owned()),
            (TOKEN_ENV.to_owned(), self.token.clone()),
        ]
    }
}

impl Drop for AskpassServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let _ = std::fs::remove_file(&self.socket);
    }
}

fn serve(mut stream: UnixStream, token: &str, handler: &Handler) -> io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut request = Vec::new();
    Read::by_ref(&mut stream).take(64 * 1024).read_to_end(&mut request)?;
    let text = String::from_utf8_lossy(&request);
    let (given, prompt) = text.split_once('\n').unwrap_or((&text, ""));
    if !sodium::equal(given.as_bytes(), token.as_bytes()) {
        stream.write_all(b"0")?;
        return Ok(());
    }
    match handler(classify(prompt), prompt.trim_end()) {
        Some(answer) => {
            // Built with exact capacity so no unwiped copy is left by a reallocation.
            let mut bytes = Vec::with_capacity(answer.as_bytes().len() + 1);
            bytes.push(b'1');
            bytes.extend_from_slice(answer.as_bytes());
            let reply = Secret::new(bytes);
            stream.write_all(reply.as_bytes())?;
        }
        None => stream.write_all(b"0")?,
    }
    Ok(())
}

/// Client side, used by the askpass helper process.
pub fn request(socket: &Path, token: &str, prompt: &str) -> io::Result<Option<Secret>> {
    let mut stream = UnixStream::connect(socket)?;
    stream.write_all(format!("{token}\n{prompt}").as_bytes())?;
    stream.shutdown(std::net::Shutdown::Write)?;
    let mut reply = Vec::new();
    stream.read_to_end(&mut reply)?;
    let reply = Secret::new(reply);
    match reply.as_bytes().split_first() {
        Some((b'1', answer)) => Ok(Some(Secret::new(answer.to_vec()))),
        _ => Ok(None),
    }
}

/// When started by ssh as SSH_ASKPASS, answers the prompt and returns the exit code.
pub fn askpass_main() -> Option<i32> {
    let socket = std::env::var_os(SOCKET_ENV)?;
    let token = std::env::var(TOKEN_ENV).ok()?;
    let prompt = std::env::args().nth(1).unwrap_or_default();
    Some(match request(Path::new(&socket), &token, &prompt) {
        Ok(Some(answer)) => {
            let mut stdout = io::stdout().lock();
            let ok = stdout.write_all(answer.as_bytes()).and_then(|()| stdout.write_all(b"\n"));
            if ok.is_ok() { 0 } else { 1 }
        }
        _ => 1,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_ssh_prompts() {
        assert_eq!(classify("me@host's password: "), PromptKind::Password);
        assert_eq!(classify("Enter passphrase for key '/home/me/.ssh/id_ed25519': "), PromptKind::Passphrase);
        assert_eq!(
            classify("The authenticity of host 'x' can't be established.\nAre you sure you want to continue connecting (yes/no/[fingerprint])? "),
            PromptKind::HostKey
        );
        assert_eq!(classify("Verification code: "), PromptKind::Other);
    }

    #[test]
    fn server_answers_only_with_correct_token() {
        let server = AskpassServer::start(Box::new(|kind, prompt| {
            assert_eq!(kind, PromptKind::Password);
            assert!(prompt.contains("password"));
            Some(Secret::from_str("tajne"))
        }))
        .unwrap();
        let mode = std::fs::metadata(&server.socket).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0);
        let answer = request(&server.socket, &server.token, "me@host's password: ").unwrap();
        assert_eq!(answer.unwrap().as_str(), Some("tajne"));
        assert!(request(&server.socket, "wrong-token", "me@host's password: ").unwrap().is_none());
        let socket = server.socket.clone();
        drop(server);
        assert!(!socket.exists());
    }
}
