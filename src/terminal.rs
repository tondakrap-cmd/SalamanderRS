//! Starting a terminal emulator in a directory, optionally running a command (Ctrl+T).
//!
//! Terminals disagree on how a command is passed: `--` (GNOME), `-e` followed by the
//! arguments, `-e` followed by one shell string, or `-x`. Each known terminal is described
//! here so a command with spaces and quotes (the ssh command line) arrives intact.

use std::env;
use std::io;
use std::path::Path;
use std::process::{Command, Stdio};

#[derive(Clone, Copy)]
enum Style {
    /// `term -- command args…`
    DoubleDash,
    /// `term -e command args…` (Debian's x-terminal-emulator contract, xterm, konsole)
    DashE,
    /// `term -e "command args…"` as a single shell string
    DashEString,
    /// `term -x command args…` (xfce4-terminal)
    DashX,
    /// `term command args…` (kitty)
    Direct,
}

/// `(program, command style, arguments selecting the working directory)`; `{dir}` is
/// replaced by the directory. All of them also start in the process' current directory.
const TERMINALS: &[(&str, Style, &[&str])] = &[
    ("x-terminal-emulator", Style::DashE, &[]),
    ("ptyxis", Style::DoubleDash, &["--new-window", "--working-directory={dir}"]),
    ("gnome-terminal", Style::DoubleDash, &["--working-directory={dir}"]),
    ("kgx", Style::DoubleDash, &["--working-directory={dir}"]),
    ("konsole", Style::DashE, &["--workdir", "{dir}"]),
    ("xfce4-terminal", Style::DashX, &["--working-directory={dir}"]),
    ("qterminal", Style::DashEString, &["--workdir", "{dir}"]),
    ("lxterminal", Style::DashEString, &["--working-directory={dir}"]),
    ("tilix", Style::DashEString, &["--working-directory={dir}"]),
    ("kitty", Style::Direct, &["--directory", "{dir}"]),
    ("alacritty", Style::DashE, &["--working-directory", "{dir}"]),
    ("foot", Style::Direct, &["--working-directory={dir}"]),
    ("xterm", Style::DashE, &[]),
];

/// Starts a terminal in `dir`, running `command` when given. `$TERMINAL` is tried first
/// (described like the known terminal of the same name, otherwise with `-e`), then the
/// known terminals in order.
pub fn launch(dir: &Path, command: Option<&[String]>) -> Result<(), String> {
    let custom = env::var("TERMINAL").ok().filter(|value| !value.trim().is_empty());
    let candidates = custom
        .iter()
        .map(|program| {
            let name = Path::new(program).file_name().and_then(|name| name.to_str()).unwrap_or_default();
            match TERMINALS.iter().find(|(known, _, _)| *known == name) {
                Some(&(_, style, dir_args)) => (program.as_str(), style, dir_args),
                None => (program.as_str(), Style::DashE, &[][..]),
            }
        })
        .chain(TERMINALS.iter().copied());
    for (program, style, dir_args) in candidates {
        let mut process = Command::new(program);
        process.current_dir(dir).stdin(Stdio::null());
        let dir_text = dir.to_string_lossy();
        process.args(dir_args.iter().map(|arg| arg.replace("{dir}", &dir_text)));
        let mut script = None;
        if let Some(command) = command {
            match style {
                Style::DoubleDash => process.arg("--").args(command),
                Style::DashE => process.arg("-e").args(command),
                // Their own command-line splitting breaks on shell quoting (qterminal turns
                // 'it'\''s' into three words), so they get the path of a one-shot script.
                Style::DashEString => process.arg("-e").arg(script.insert(command_script(command)?).as_path()),
                Style::DashX => process.arg("-x").args(command),
                Style::Direct => process.args(command),
            };
        }
        let spawned = process.spawn();
        if let (Err(_), Some(script)) = (&spawned, &script) {
            let _ = std::fs::remove_file(script);
        }
        match spawned {
            Ok(mut child) => {
                // Reaps the terminal (or its launcher) when it exits.
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
                return Ok(());
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(format!("Nelze spustit {program}: {error}")),
        }
    }
    Err("Nenalezen žádný emulátor terminálu (nastavte proměnnou TERMINAL)".to_owned())
}

/// Writes `command` into a private script that removes itself and then runs the command.
fn command_script(command: &[String]) -> Result<std::path::PathBuf, String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let dir = crate::remote::askpass::runtime_dir().map_err(|error| format!("Nelze připravit terminál: {error}"))?;
    let path = dir.join(format!("term-{}.sh", crate::sodium::random_hex(8)));
    let script = format!("#!/bin/sh\nrm -f -- \"$0\"\nexec {}\n", shell_join(command));
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o700)
        .open(&path)
        .and_then(|mut file| file.write_all(script.as_bytes()))
        .map_err(|error| format!("Nelze připravit terminál: {error}"))?;
    Ok(path)
}

/// Joins arguments into one POSIX shell command line, quoting where needed.
pub fn shell_join(args: &[String]) -> String {
    args.iter().map(|arg| shell_quote(arg)).collect::<Vec<_>>().join(" ")
}

pub fn shell_quote(arg: &str) -> String {
    let safe = !arg.is_empty()
        && arg.bytes().all(|byte| byte.is_ascii_alphanumeric() || b"-_./=:@,+%".contains(&byte));
    if safe {
        arg.to_owned()
    } else {
        format!("'{}'", arg.replace('\'', "'\\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_join_quotes_only_when_needed() {
        let args = ["ssh", "-o", "ControlPath=/run/x y", "--", "host", "cd '/a b' && exec \"$SHELL\" -l"]
            .map(String::from);
        assert_eq!(
            shell_join(&args),
            r#"ssh -o 'ControlPath=/run/x y' -- host 'cd '\''/a b'\'' && exec "$SHELL" -l'"#
        );
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn shell_join_round_trips_through_sh() {
        let args = ["printf", "%s|", "a b", "it's", "$HOME", "\"q\"", ""].map(String::from);
        let output = Command::new("sh").arg("-c").arg(shell_join(&args)).output().unwrap();
        assert_eq!(String::from_utf8(output.stdout).unwrap(), "a b|it's|$HOME|\"q\"||");
    }
}
