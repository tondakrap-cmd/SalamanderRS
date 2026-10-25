//! Recursive download, upload and delete over SFTP with progress, cancel and audit logging.

use std::fs::{self, File};
use std::io;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, UNIX_EPOCH};

use super::conn::Connection;
use super::log;
use super::sftp::{self, Attrs, Sftp, SftpError};
use crate::archive::JobResult;
use crate::model::format_bytes;

struct Job<'a> {
    sftp: &'a mut Sftp,
    label: String,
    overwrite: bool,
    cancel: &'a AtomicBool,
    progress: &'a mut dyn FnMut(&Path),
    result: JobResult,
}

impl Job<'_> {
    fn cancelled(&mut self) -> bool {
        if self.cancel.load(Ordering::Relaxed) {
            self.result.cancelled = true;
        }
        self.result.cancelled
    }

    fn fail(&mut self, source: &Path, target: Option<&Path>, error: impl ToString) {
        let error = error.to_string();
        log::log(
            &self.label,
            "ERROR",
            &format!(
                "{}{}: {error}",
                source.to_string_lossy(),
                target.map(|target| format!(" -> {}", target.to_string_lossy())).unwrap_or_default()
            ),
        );
        self.result.errors.push(crate::model::OpError {
            source: source.to_path_buf(),
            target: target.map(Path::to_path_buf),
            error,
        });
    }

    fn download(&mut self, remote: &Path, local: &Path) {
        if self.cancelled() {
            return;
        }
        (self.progress)(remote);
        let attrs = match self.sftp.lstat(remote) {
            Ok(attrs) => attrs,
            Err(error) => return self.fail(remote, Some(local), error),
        };
        if attrs.is_dir() {
            match fs::symlink_metadata(local) {
                Ok(metadata) if metadata.is_dir() => {}
                Ok(_) => return self.fail(remote, Some(local), "cíl existuje a není adresář"),
                Err(_) => {
                    if let Err(error) = fs::create_dir(local) {
                        return self.fail(remote, Some(local), error);
                    }
                }
            }
            let entries = match self.sftp.read_dir(remote) {
                Ok(entries) => entries,
                Err(error) => return self.fail(remote, Some(local), error),
            };
            for entry in entries {
                if !entry.is_safe_name() {
                    self.fail(remote, Some(local), format!("server poslal neplatné jméno „{}“ — přeskočeno", entry.name.to_string_lossy()));
                    continue;
                }
                self.download(&remote.join(&entry.name), &local.join(&entry.name));
                if self.result.cancelled {
                    return;
                }
            }
            if let Some(permissions) = attrs.permissions {
                let _ = fs::set_permissions(local, fs::Permissions::from_mode((permissions & 0o1777) | 0o700));
            }
            set_local_times(local, &attrs);
            self.result.done += 1;
        } else if attrs.is_symlink() {
            let target = match self.sftp.readlink(remote) {
                Ok(target) => target,
                Err(error) => return self.fail(remote, Some(local), error),
            };
            if let Err(error) = prepare_local_target(local, self.overwrite) {
                return self.fail(remote, Some(local), error);
            }
            match std::os::unix::fs::symlink(&target, local) {
                Ok(()) => {
                    self.result.done += 1;
                    log::log(&self.label, "DOWNLOAD", &format!("{} -> {} (symlink)", remote.to_string_lossy(), local.to_string_lossy()));
                }
                Err(error) => self.fail(remote, Some(local), error),
            }
        } else if attrs.is_file() {
            if let Err(error) = prepare_local_target(local, self.overwrite) {
                return self.fail(remote, Some(local), error);
            }
            let file = match fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(local) {
                Ok(file) => file,
                Err(error) => return self.fail(remote, Some(local), error),
            };
            let progress = &mut self.progress;
            let mut last = std::time::Instant::now();
            let outcome = self.sftp.download(remote, &file, self.cancel, &mut |_| {
                if last.elapsed() > Duration::from_millis(200) {
                    last = std::time::Instant::now();
                    progress(remote);
                }
            });
            drop(file);
            match outcome {
                Ok(bytes) => {
                    // Never recreate setuid/setgid bits from a remote server.
                    let mode = attrs.permissions.unwrap_or(0o644) & 0o1777;
                    let _ = fs::set_permissions(local, fs::Permissions::from_mode(mode));
                    set_local_times(local, &attrs);
                    self.result.done += 1;
                    log::log(&self.label, "DOWNLOAD", &format!("{} -> {} ({})", remote.to_string_lossy(), local.to_string_lossy(), format_bytes(bytes)));
                }
                Err(error) => {
                    let _ = fs::remove_file(local);
                    if matches!(error, SftpError::Cancelled) {
                        self.result.cancelled = true;
                        log::log(&self.label, "CANCEL", &format!("stahování {}", remote.to_string_lossy()));
                    } else {
                        self.fail(remote, Some(local), error);
                    }
                }
            }
        } else {
            self.fail(remote, Some(local), "nepodporovaný typ souboru");
        }
    }

    fn upload(&mut self, local: &Path, remote: &Path) {
        if self.cancelled() {
            return;
        }
        (self.progress)(local);
        let metadata = match fs::symlink_metadata(local) {
            Ok(metadata) => metadata,
            Err(error) => return self.fail(local, Some(remote), error),
        };
        let existing = match self.sftp.try_lstat(remote) {
            Ok(existing) => existing,
            Err(error) => return self.fail(local, Some(remote), error),
        };
        if metadata.is_dir() {
            match existing {
                Some(attrs) if attrs.is_dir() => {}
                Some(_) => return self.fail(local, Some(remote), "cíl existuje a není adresář"),
                None => {
                    if let Err(error) = self.sftp.mkdir(remote, metadata.mode() & 0o7777) {
                        return self.fail(local, Some(remote), error);
                    }
                }
            }
            let mut children: Vec<_> = match fs::read_dir(local) {
                Ok(entries) => entries.filter_map(|entry| entry.ok().map(|entry| entry.file_name())).collect(),
                Err(error) => return self.fail(local, Some(remote), error),
            };
            children.sort();
            for child in children {
                self.upload(&local.join(&child), &remote.join(&child));
                if self.result.cancelled {
                    return;
                }
            }
            let _ = self.sftp.setstat(remote, &times_of(&metadata));
            self.result.done += 1;
            log::log(&self.label, "MKDIR", &remote.to_string_lossy());
        } else if metadata.file_type().is_symlink() {
            if existing.is_some() {
                if !self.overwrite {
                    return self.fail(local, Some(remote), "cíl již existuje");
                }
                if let Err(error) = self.sftp.remove(remote) {
                    return self.fail(local, Some(remote), error);
                }
            }
            let target = match fs::read_link(local) {
                Ok(target) => target,
                Err(error) => return self.fail(local, Some(remote), error),
            };
            match self.sftp.symlink(&target, remote) {
                Ok(()) => {
                    self.result.done += 1;
                    log::log(&self.label, "UPLOAD", &format!("{} -> {} (symlink)", local.to_string_lossy(), remote.to_string_lossy()));
                }
                Err(error) => self.fail(local, Some(remote), error),
            }
        } else if metadata.is_file() {
            let fresh = match existing {
                Some(attrs) if attrs.is_dir() => return self.fail(local, Some(remote), "cíl je adresář"),
                Some(_) if !self.overwrite => return self.fail(local, Some(remote), "cíl již existuje"),
                Some(_) => false,
                None => true,
            };
            let mut file = match File::open(local) {
                Ok(file) => file,
                Err(error) => return self.fail(local, Some(remote), error),
            };
            let flags = sftp::OPEN_WRITE
                | sftp::OPEN_CREATE
                | if fresh { sftp::OPEN_EXCLUSIVE } else { sftp::OPEN_TRUNCATE };
            let attrs = Attrs {
                permissions: Some(metadata.mode() & 0o777),
                ..Attrs::default()
            };
            let progress = &mut self.progress;
            let mut last = std::time::Instant::now();
            let outcome = self.sftp.upload(&mut file, remote, flags, &attrs, self.cancel, &mut |_| {
                if last.elapsed() > Duration::from_millis(200) {
                    last = std::time::Instant::now();
                    progress(local);
                }
            });
            match outcome {
                Ok(bytes) => {
                    // Like `scp -p`: keep modification and access times.
                    let _ = self.sftp.setstat(remote, &times_of(&metadata));
                    self.result.done += 1;
                    log::log(&self.label, "UPLOAD", &format!("{} -> {} ({})", local.to_string_lossy(), remote.to_string_lossy(), format_bytes(bytes)));
                }
                Err(error) => {
                    if fresh {
                        let _ = self.sftp.remove(remote);
                    }
                    if matches!(error, SftpError::Cancelled) {
                        self.result.cancelled = true;
                        log::log(&self.label, "CANCEL", &format!("nahrávání {}", local.to_string_lossy()));
                    } else {
                        self.fail(local, Some(remote), error);
                    }
                }
            }
        } else {
            self.fail(local, Some(remote), "nepodporovaný typ souboru");
        }
    }

    fn delete(&mut self, remote: &Path) {
        if self.cancelled() {
            return;
        }
        (self.progress)(remote);
        let attrs = match self.sftp.lstat(remote) {
            Ok(attrs) => attrs,
            Err(error) => return self.fail(remote, None, error),
        };
        if attrs.is_dir() {
            let entries = match self.sftp.read_dir(remote) {
                Ok(entries) => entries,
                Err(error) => return self.fail(remote, None, error),
            };
            for entry in entries {
                if !entry.is_safe_name() {
                    self.fail(remote, None, format!("server poslal neplatné jméno „{}“ — přeskočeno", entry.name.to_string_lossy()));
                    continue;
                }
                self.delete(&remote.join(&entry.name));
                if self.result.cancelled {
                    return;
                }
            }
            match self.sftp.rmdir(remote) {
                Ok(()) => {
                    self.result.done += 1;
                    log::log(&self.label, "DELETE", &format!("{}/", remote.to_string_lossy()));
                }
                Err(error) => self.fail(remote, None, error),
            }
        } else {
            match self.sftp.remove(remote) {
                Ok(()) => {
                    self.result.done += 1;
                    log::log(&self.label, "DELETE", &remote.to_string_lossy());
                }
                Err(error) => self.fail(remote, None, error),
            }
        }
    }
}

fn prepare_local_target(local: &Path, overwrite: bool) -> Result<(), String> {
    match fs::symlink_metadata(local) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.to_string()),
        Ok(_) if !overwrite => Err("cíl již existuje".to_owned()),
        Ok(metadata) if metadata.is_dir() => Err("cíl je adresář".to_owned()),
        Ok(_) => fs::remove_file(local).map_err(|error| error.to_string()),
    }
}

fn set_local_times(local: &Path, attrs: &Attrs) {
    let time = |seconds: Option<u32>| seconds.map(|seconds| UNIX_EPOCH + Duration::from_secs(u64::from(seconds)));
    let _ = crate::sys::set_times(local, time(attrs.atime), time(attrs.mtime));
}

fn times_of(metadata: &fs::Metadata) -> Attrs {
    Attrs {
        atime: u32::try_from(metadata.atime()).ok(),
        mtime: u32::try_from(metadata.mtime()).ok(),
        ..Attrs::default()
    }
}

fn run(
    sftp: &mut Sftp,
    label: String,
    overwrite: bool,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(&Path),
    body: impl FnOnce(&mut Job),
) -> JobResult {
    let mut job = Job {
        sftp,
        label,
        overwrite,
        cancel,
        progress,
        result: JobResult::default(),
    };
    body(&mut job);
    job.result
}

fn with_session(connection: &Connection, body: impl FnOnce(&mut Sftp) -> JobResult) -> JobResult {
    match connection.session() {
        Ok(mut sftp) => body(&mut sftp),
        Err(error) => {
            let mut result = JobResult::default();
            result.errors.push(crate::model::OpError {
                source: PathBuf::from(connection.label()),
                target: None,
                error,
            });
            result
        }
    }
}

/// Downloads remote files or directory trees into local directory `dest`.
pub fn download(
    connection: &Connection,
    items: &[PathBuf],
    dest: &Path,
    overwrite: bool,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(&Path),
) -> JobResult {
    with_session(connection, |sftp| download_with(sftp, connection.label(), items, dest, overwrite, cancel, progress))
}

pub fn download_with(
    sftp: &mut Sftp,
    label: String,
    items: &[PathBuf],
    dest: &Path,
    overwrite: bool,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(&Path),
) -> JobResult {
    run(sftp, label, overwrite, cancel, progress, |job| {
        for item in items {
            let Some(name) = item.file_name() else { continue };
            job.download(item, &dest.join(name));
        }
    })
}

/// Uploads local files or directory trees into remote directory `remote_dir`.
pub fn upload(
    connection: &Connection,
    sources: &[PathBuf],
    remote_dir: &Path,
    overwrite: bool,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(&Path),
) -> JobResult {
    with_session(connection, |sftp| upload_with(sftp, connection.label(), sources, remote_dir, overwrite, cancel, progress))
}

pub fn upload_with(
    sftp: &mut Sftp,
    label: String,
    sources: &[PathBuf],
    remote_dir: &Path,
    overwrite: bool,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(&Path),
) -> JobResult {
    run(sftp, label, overwrite, cancel, progress, |job| {
        for source in sources {
            let Some(name) = source.file_name() else { continue };
            job.upload(source, &remote_dir.join(name));
        }
    })
}

/// Deletes remote files and directory trees.
pub fn delete(connection: &Connection, items: &[PathBuf], cancel: &AtomicBool, progress: &mut dyn FnMut(&Path)) -> JobResult {
    with_session(connection, |sftp| delete_with(sftp, connection.label(), items, cancel, progress))
}

pub fn delete_with(sftp: &mut Sftp, label: String, items: &[PathBuf], cancel: &AtomicBool, progress: &mut dyn FnMut(&Path)) -> JobResult {
    run(sftp, label, false, cancel, progress, |job| {
        for item in items {
            job.delete(item);
        }
    })
}

/// Downloads one file into a private temporary directory for viewing or opening.
pub fn download_to_temp(connection: &Connection, remote: &Path) -> Result<PathBuf, String> {
    let root = std::env::temp_dir().join(format!("salamander-rs-{}", crate::sys::effective_uid()));
    fs::create_dir_all(&root).map_err(|error| error.to_string())?;
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).map_err(|error| error.to_string())?;
    let dest = root.join(format!("{}-remote-{}", std::process::id(), crate::sodium::random_hex(4)));
    fs::create_dir(&dest).map_err(|error| error.to_string())?;
    let result = download(connection, &[remote.to_path_buf()], &dest, true, &AtomicBool::new(false), &mut |_| {});
    if let Some(error) = result.errors.first() {
        return Err(error.error.clone());
    }
    Ok(dest.join(remote.file_name().unwrap_or_default()))
}

/// Single quick operations on the browsing session, each logged.
pub fn mkdir(connection: &Connection, path: &Path) -> Result<(), String> {
    let result = connection.browse(|sftp| sftp.mkdir(path, 0o755));
    log_result(connection, "MKDIR", &path.to_string_lossy(), &result);
    result
}

pub fn rename(connection: &Connection, from: &Path, to: &Path) -> Result<(), String> {
    let result = connection.browse(|sftp| {
        if sftp.try_lstat(to)?.is_some() {
            return Err(SftpError::Status {
                code: sftp::STATUS_FAILURE,
                message: "cíl již existuje".to_owned(),
            });
        }
        sftp.rename(from, to)
    });
    log_result(connection, "RENAME", &format!("{} -> {}", from.to_string_lossy(), to.to_string_lossy()), &result);
    result
}

pub fn chmod(connection: &Connection, path: &Path, mode: u32) -> Result<(), String> {
    let attrs = Attrs {
        permissions: Some(mode & 0o7777),
        ..Attrs::default()
    };
    let result = connection.browse(|sftp| sftp.setstat(path, &attrs));
    log_result(connection, "CHMOD", &format!("{:04o} {}", mode & 0o7777, path.to_string_lossy()), &result);
    result
}

pub fn create_file(connection: &Connection, path: &Path) -> Result<(), String> {
    let result = connection.browse(|sftp| {
        let handle = sftp.open(path, sftp::OPEN_WRITE | sftp::OPEN_CREATE | sftp::OPEN_EXCLUSIVE, &Attrs { permissions: Some(0o644), ..Attrs::default() })?;
        sftp.close(&handle)
    });
    log_result(connection, "CREATE", &path.to_string_lossy(), &result);
    result
}

fn log_result(connection: &Connection, action: &str, detail: &str, result: &Result<(), String>) {
    match result {
        Ok(()) => log::log(&connection.label(), action, detail),
        Err(error) => log::log(&connection.label(), "ERROR", &format!("{action} {detail}: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::sftp::tests::local_session;

    fn tree(root: &Path) {
        fs::create_dir_all(root.join("dir/sub")).unwrap();
        fs::write(root.join("dir/a.txt"), "alpha").unwrap();
        fs::write(root.join("dir/sub/b.bin"), vec![7u8; 100_000]).unwrap();
        std::os::unix::fs::symlink("a.txt", root.join("dir/link")).unwrap();
        crate::sys::set_times(&root.join("dir/a.txt"), None, Some(UNIX_EPOCH + Duration::from_secs(1_100_000_000))).unwrap();
    }

    #[test]
    fn upload_download_and_delete_trees() {
        let Some(mut sftp) = local_session() else { return };
        let temp = tempfile::tempdir().unwrap();
        let local = temp.path().join("local");
        let remote = temp.path().join("remote");
        let back = temp.path().join("back");
        for dir in [&local, &remote, &back] {
            fs::create_dir(dir).unwrap();
        }
        tree(&local);
        let no = AtomicBool::new(false);

        let up = upload_with(&mut sftp, "test".to_owned(), &[local.join("dir")], &remote, false, &no, &mut |_| {});
        assert!(up.errors.is_empty(), "{:?}", up.errors);
        assert_eq!(fs::read(remote.join("dir/sub/b.bin")).unwrap().len(), 100_000);
        assert_eq!(fs::read_link(remote.join("dir/link")).unwrap(), Path::new("a.txt"));
        assert_eq!(fs::metadata(remote.join("dir/a.txt")).unwrap().mtime(), 1_100_000_000);

        // Without overwrite existing files are reported, with overwrite replaced.
        fs::write(local.join("dir/a.txt"), "changed").unwrap();
        let again = upload_with(&mut sftp, "test".to_owned(), &[local.join("dir/a.txt")], &remote.join("dir"), false, &no, &mut |_| {});
        assert_eq!(again.errors.len(), 1);
        assert_eq!(fs::read_to_string(remote.join("dir/a.txt")).unwrap(), "alpha");
        let forced = upload_with(&mut sftp, "test".to_owned(), &[local.join("dir/a.txt")], &remote.join("dir"), true, &no, &mut |_| {});
        assert!(forced.errors.is_empty(), "{:?}", forced.errors);
        assert_eq!(fs::read_to_string(remote.join("dir/a.txt")).unwrap(), "changed");

        let down = download_with(&mut sftp, "test".to_owned(), &[remote.join("dir")], &back, false, &no, &mut |_| {});
        assert!(down.errors.is_empty(), "{:?}", down.errors);
        assert_eq!(fs::read_to_string(back.join("dir/a.txt")).unwrap(), "changed");
        assert_eq!(fs::read(back.join("dir/sub/b.bin")).unwrap(), vec![7u8; 100_000]);
        assert_eq!(fs::read_link(back.join("dir/link")).unwrap(), Path::new("a.txt"));

        let deleted = delete_with(&mut sftp, "test".to_owned(), &[remote.join("dir")], &no, &mut |_| {});
        assert!(deleted.errors.is_empty(), "{:?}", deleted.errors);
        assert!(!remote.join("dir").exists());
        assert!(back.join("dir/sub/b.bin").exists());
    }
}
