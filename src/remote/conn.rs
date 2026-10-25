//! SSH connections: the system `ssh` runs the SFTP subsystem, authentication prompts go
//! through the askpass bridge, and an ssh ControlMaster lets further sessions (transfers)
//! reuse the authenticated connection without asking again.

use std::collections::HashMap;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::askpass::{self, AskpassServer, PromptKind};
use super::log;
use super::servers::{AuthMethod, Server};
use super::sftp::{Sftp, SftpError};
use crate::sodium::Secret;

pub type PromptHandler = Box<dyn Fn(PromptKind, &str) -> Option<Secret> + Send + Sync>;

pub struct Connection {
    pub server: Server,
    pub home: PathBuf,
    control_path: PathBuf,
    askpass: AskpassServer,
    browse: Mutex<Option<Sftp>>,
}

fn registry() -> &'static Mutex<HashMap<String, Arc<Connection>>> {
    static REGISTRY: OnceLock<Mutex<HashMap<String, Arc<Connection>>>> = OnceLock::new();
    REGISTRY.get_or_init(Default::default)
}

pub fn get(id: &str) -> Option<Arc<Connection>> {
    registry().lock().unwrap().get(id).cloned()
}

pub fn all() -> Vec<Arc<Connection>> {
    registry().lock().unwrap().values().cloned().collect()
}

fn ssh_args(server: &Server, control_path: &Path) -> Vec<String> {
    let mut args: Vec<String> = [
        "-o", "ControlMaster=auto",
        "-o", &format!("ControlPath={}", control_path.to_string_lossy()),
        "-o", "ControlPersist=300",
        "-o", "ServerAliveInterval=30",
        "-o", "ServerAliveCountMax=3",
        "-o", "ConnectTimeout=20",
        "-o", "StrictHostKeyChecking=ask",
        "-o", "BatchMode=no",
        "-o", "NumberOfPasswordPrompts=3",
        "-p", &server.port.to_string(),
    ]
    .iter()
    .map(|arg| arg.to_string())
    .collect();
    if !server.user.is_empty() {
        args.extend(["-l".to_owned(), server.user.clone()]);
    }
    match server.auth {
        AuthMethod::Password => args.extend(
            ["-o", "PreferredAuthentications=password,keyboard-interactive", "-o", "PubkeyAuthentication=no"]
                .map(String::from),
        ),
        AuthMethod::Key => args.extend(["-o", "PreferredAuthentications=publickey"].map(String::from)),
        AuthMethod::Auto => {}
    }
    if let Some(identity) = &server.identity {
        args.extend(["-i".to_owned(), identity.to_string_lossy().into_owned()]);
        if server.auth == AuthMethod::Key {
            args.extend(["-o".to_owned(), "IdentitiesOnly=yes".to_owned()]);
        }
    }
    args.extend(server.extra_args());
    args
}

/// Last meaningful ssh error lines, without the usual informational warnings.
fn describe_failure(error: &SftpError, stderr: &str) -> String {
    let lines: Vec<&str> = stderr
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with("Warning: Permanently added"))
        .collect();
    if lines.is_empty() {
        error.to_string()
    } else {
        lines[lines.len().saturating_sub(4)..].join("\n")
    }
}

fn spawn_sftp(server: &Server, control_path: &Path, askpass: &AskpassServer) -> Result<Sftp, String> {
    let mut command = Command::new("ssh");
    command
        .args(ssh_args(server, control_path))
        .arg("-s")
        .arg("--")
        .arg(&server.host)
        .arg("sftp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in askpass.environment() {
        command.env(key, value);
    }
    // Without a controlling terminal ssh never reads secrets from a tty behind our back.
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = command.spawn().map_err(|error| format!("Nelze spustit ssh: {error}"))?;
    let stderr_text = Arc::new(Mutex::new(String::new()));
    let reader = child.stderr.take().map(|mut stderr| {
        let text = Arc::clone(&stderr_text);
        std::thread::spawn(move || {
            let mut buffer = [0u8; 4096];
            while let Ok(read) = stderr.read(&mut buffer) {
                if read == 0 {
                    break;
                }
                let mut text = text.lock().unwrap();
                text.push_str(&String::from_utf8_lossy(&buffer[..read]));
                if text.len() > 16 * 1024 {
                    let mut cut = text.len() - 16 * 1024;
                    while !text.is_char_boundary(cut) {
                        cut += 1;
                    }
                    text.drain(..cut);
                }
            }
        })
    });
    Sftp::from_child(child, Some(Arc::clone(&stderr_text))).map_err(|error| {
        // ssh has exited; give the reader a moment to collect its last words.
        if let Some(reader) = reader {
            let started = Instant::now();
            while !reader.is_finished() && started.elapsed() < Duration::from_secs(2) {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        let text = stderr_text.lock().unwrap().clone();
        describe_failure(&error, &text)
    })
}

impl Connection {
    /// Connects (blocking; run in a worker thread). `handler` answers ssh prompts.
    pub fn connect(server: Server, handler: PromptHandler) -> Result<Arc<Connection>, String> {
        server.validate()?;
        let label = server.label();
        let dir = askpass::runtime_dir().map_err(|error| format!("Nelze vytvořit runtime adresář: {error}"))?;
        let askpass = AskpassServer::start(handler).map_err(|error| format!("Nelze spustit askpass: {error}"))?;
        let control_path = dir.join(format!("cm-{}", server.id));
        log::log(&label, "CONNECT", &format!("ověření: {}", server.auth.label()));
        let result = spawn_sftp(&server, &control_path, &askpass).and_then(|mut sftp| {
            let home = sftp
                .realpath(Path::new("."))
                .map(PathBuf::from)
                .map_err(|error| error.to_string())?;
            Ok((sftp, home))
        });
        let (sftp, home) = match result {
            Ok(value) => value,
            Err(error) => {
                log::log(&label, "ERROR", &format!("připojení selhalo: {error}"));
                return Err(error);
            }
        };
        log::log(&label, "CONNECTED", &format!("SFTP v{}, domov {}", sftp.version, home.to_string_lossy()));
        let connection = Arc::new(Connection {
            server,
            home,
            control_path,
            askpass,
            browse: Mutex::new(Some(sftp)),
        });
        registry()
            .lock()
            .unwrap()
            .insert(connection.server.id.clone(), Arc::clone(&connection));
        Ok(connection)
    }

    pub fn label(&self) -> String {
        self.server.label()
    }

    /// A separate SFTP session for a long transfer, multiplexed over the ControlMaster.
    pub fn session(&self) -> Result<Sftp, String> {
        spawn_sftp(&self.server, &self.control_path, &self.askpass)
    }

    /// Runs `operation` on the shared browsing session, reconnecting once if it died.
    pub fn browse<R>(&self, operation: impl Fn(&mut Sftp) -> Result<R, SftpError>) -> Result<R, String> {
        let mut session = self.browse.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        for attempt in 0..2 {
            if session.is_none() {
                *session = Some(self.session()?);
            }
            match operation(session.as_mut().expect("session present")) {
                Ok(value) => return Ok(value),
                Err(error) if error.is_connection() && attempt == 0 => *session = None,
                Err(error) => return Err(error.to_string()),
            }
        }
        unreachable!("the loop returns on the second attempt")
    }

    /// Command line for an interactive shell in `dir`, reusing the ControlMaster.
    pub fn terminal_command(&self, dir: &Path) -> Vec<String> {
        let mut command = vec!["ssh".to_owned()];
        command.extend(ssh_args(&self.server, &self.control_path));
        let quoted = format!("'{}'", dir.to_string_lossy().replace('\'', "'\\''"));
        command.extend([
            "-t".to_owned(),
            "--".to_owned(),
            self.server.host.clone(),
            format!("cd {quoted} && exec \"$SHELL\" -l"),
        ]);
        command
    }

    /// Closes the sessions and the ControlMaster.
    pub fn disconnect(&self) {
        *self.browse.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        let _ = Command::new("ssh")
            .args([
                "-o".to_owned(),
                format!("ControlPath={}", self.control_path.to_string_lossy()),
                "-O".to_owned(),
                "exit".to_owned(),
                "-p".to_owned(),
                self.server.port.to_string(),
            ])
            .args(if self.server.user.is_empty() { vec![] } else { vec!["-l".to_owned(), self.server.user.clone()] })
            .arg("--")
            .arg(&self.server.host)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        registry().lock().unwrap().remove(&self.server.id);
        log::log(&self.label(), "DISCONNECT", "");
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// Test server set up by the developer (see README): 127.0.0.1:2222 with a key.
    pub fn test_server(known_hosts: &Path) -> Option<Server> {
        let home = PathBuf::from(std::env::var_os("HOME")?);
        let key = home.join("srs-sshtest/client_ed25519");
        if !key.is_file() || std::net::TcpStream::connect("127.0.0.1:2222").is_err() {
            eprintln!("test sshd on 127.0.0.1:2222 not available, skipping");
            return None;
        }
        let mut server = Server::new();
        server.id = format!("test{}", crate::sodium::random_hex(4));
        server.host = "127.0.0.1".to_owned();
        server.port = 2222;
        server.auth = AuthMethod::Key;
        server.identity = Some(key);
        server.options = format!("-o UserKnownHostsFile={}", known_hosts.to_string_lossy());
        Some(server)
    }

    #[test]
    fn ssh_arguments_never_put_host_before_options() {
        let mut server = Server::new();
        server.host = "example.com".to_owned();
        server.user = "me".to_owned();
        server.auth = AuthMethod::Password;
        let args = ssh_args(&server, Path::new("/run/x/cm-1"));
        assert!(args.contains(&"ControlPath=/run/x/cm-1".to_owned()));
        assert!(args.contains(&"PubkeyAuthentication=no".to_owned()));
        assert!(!args.contains(&"example.com".to_owned()));
    }

    #[test]
    fn connects_through_real_ssh_with_host_key_prompt() {
        let temp = tempfile::tempdir().unwrap();
        let known_hosts = temp.path().join("known_hosts");
        let Some(server) = test_server(&known_hosts) else { return };
        // The askpass helper is the application binary; tests run as a different binary,
        // so this test exercises everything except the helper by pre-trusting the key.
        let status = Command::new("ssh-keyscan")
            .args(["-p", "2222", "-t", "ed25519", "127.0.0.1"])
            .output()
            .unwrap();
        std::fs::write(&known_hosts, &status.stdout).unwrap();
        let connection = Connection::connect(server, Box::new(|_, _| None)).unwrap();
        assert!(connection.home.is_absolute());
        let listing = connection.browse(|sftp| sftp.read_dir(Path::new("/etc"))).unwrap();
        assert!(listing.iter().any(|entry| entry.name == "hostname"));
        // A second session reuses the ControlMaster.
        let mut session = connection.session().unwrap();
        assert!(session.stat(Path::new("/etc/hostname")).unwrap().is_file());
        drop(session);
        connection.disconnect();
        assert!(get(&connection.server.id).is_none());
    }

    #[test]
    fn unknown_host_key_is_refused_without_confirmation() {
        let temp = tempfile::tempdir().unwrap();
        let known_hosts = temp.path().join("known_hosts");
        let Some(server) = test_server(&known_hosts) else { return };
        let error = Connection::connect(server, Box::new(|_, _| None)).err().unwrap();
        assert!(error.to_lowercase().contains("host key verification failed"), "{error}");
    }
}
