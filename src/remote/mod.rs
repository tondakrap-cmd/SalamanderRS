//! Remote file systems over SSH (SFTP).
//!
//! Remote entries use virtual paths `sftp://<server id>/absolute/path`. They are relative
//! paths for the local file system, so a local operation applied by mistake fails instead
//! of touching a local file with the same name.

pub mod askpass;
pub mod conn;
pub mod log;
pub mod servers;
pub mod sftp;
pub mod transfer;

use std::ffi::OsString;
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use crate::model::{DirectoryListing, EntryKind, FileEntry};
use crate::sys::DiskSpace;

pub const SCHEME: &str = "sftp://";

pub fn virtual_path(id: &str, remote: &Path) -> PathBuf {
    let mut bytes = format!("{SCHEME}{id}").into_bytes();
    bytes.extend_from_slice(normalize(remote).as_os_str().as_bytes());
    PathBuf::from(OsString::from_vec(bytes))
}

/// Splits `sftp://<id>/path` into the server id and the absolute remote path.
pub fn split_virtual(path: &Path) -> Option<(String, PathBuf)> {
    let rest = path.as_os_str().as_bytes().strip_prefix(SCHEME.as_bytes())?;
    let slash = rest.iter().position(|byte| *byte == b'/').unwrap_or(rest.len());
    let id = String::from_utf8(rest[..slash].to_vec()).ok()?;
    if id.is_empty() {
        return None;
    }
    let remote = PathBuf::from(OsString::from_vec(rest[slash..].to_vec()));
    Some((id, normalize(&remote)))
}

/// Absolute, lexically normalized remote path ("/" for empty).
pub fn normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::from("/");
    for component in path.components() {
        match component {
            Component::Normal(part) => normalized.push(part),
            Component::ParentDir => {
                normalized.pop();
            }
            _ => {}
        }
    }
    normalized
}

/// Human readable location, e.g. `sftp://me@host/var/www`.
pub fn pretty(id: &str, remote: &Path) -> String {
    let label = conn::get(id).map(|connection| connection.label()).unwrap_or_else(|| id.to_owned());
    format!("{SCHEME}{label}{}", remote.to_string_lossy())
}

/// Finds the connected server for a typed `sftp://user@host[:port]/path` or id.
pub fn resolve_typed(text: &str) -> Option<(String, PathBuf)> {
    let rest = text.strip_prefix(SCHEME)?;
    let (authority, path) = match rest.find('/') {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, "/"),
    };
    let connection = conn::all().into_iter().find(|connection| {
        connection.server.id == authority
            || connection.label() == authority
            || connection.server.host == authority
            || connection.server.name == authority
    })?;
    Some((connection.server.id.clone(), normalize(Path::new(path))))
}

/// Lists a remote directory for the panel (blocking; called from the loader thread).
pub fn read_directory(id: &str, dir: &Path, show_hidden: bool) -> io::Result<DirectoryListing> {
    let connection = conn::get(id)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "server není připojen"))?;
    let entries = connection.browse(|sftp| sftp.read_dir(dir)).map_err(io::Error::other)?;
    log::log(&connection.label(), "LIST", &dir.to_string_lossy());
    let space = connection.browse(|sftp| sftp.statvfs(dir)).ok().flatten();
    let time = |seconds: Option<u32>| seconds.map(|seconds| UNIX_EPOCH + Duration::from_secs(u64::from(seconds)));
    let mut files = Vec::new();
    for entry in entries {
        if !entry.is_safe_name() {
            continue;
        }
        let folded_name = entry.name.to_string_lossy().to_lowercase();
        if !show_hidden && folded_name.starts_with('.') {
            continue;
        }
        let remote = dir.join(&entry.name);
        let attrs = &entry.attrs;
        let kind = if attrs.is_dir() {
            EntryKind::Directory
        } else if attrs.is_symlink() {
            EntryKind::Symlink
        } else if attrs.is_file() {
            EntryKind::File
        } else {
            EntryKind::Other
        };
        // Symlinks need two extra round trips, so only they pay for them.
        let (link_target, target_is_dir) = if kind == EntryKind::Symlink {
            let target = connection.browse(|sftp| sftp.readlink(&remote)).ok().map(PathBuf::from);
            let is_dir = connection.browse(|sftp| sftp.stat(&remote)).is_ok_and(|attrs| attrs.is_dir());
            (target, is_dir)
        } else {
            (None, false)
        };
        let (owner, group) = entry.owner_group().unwrap_or_else(|| {
            (
                attrs.uid.map(|uid| uid.to_string()).unwrap_or_default(),
                attrs.gid.map(|gid| gid.to_string()).unwrap_or_default(),
            )
        });
        files.push(FileEntry {
            path: virtual_path(id, &remote),
            name: entry.name.clone(),
            folded_name,
            kind,
            size: attrs.size.unwrap_or(0),
            modified: time(attrs.mtime),
            accessed: time(attrs.atime),
            changed: None,
            created: None,
            mode: attrs.permissions.unwrap_or(0),
            uid: attrs.uid.unwrap_or(0),
            gid: attrs.gid.unwrap_or(0),
            owner,
            group,
            nlink: 1,
            inode: 0,
            link_target,
            target_is_dir,
            has_acl: false,
        });
    }
    Ok(DirectoryListing {
        entries: files,
        warnings: Vec::new(),
        disk_space: space.map(|space| DiskSpace {
            available: space.available,
            total: space.total,
        }),
        note: Some(format!("SFTP: {}", connection.label())),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn virtual_paths_round_trip_and_normalize() {
        let path = virtual_path("abc123", Path::new("/home/me/../you/./x y"));
        assert_eq!(path, Path::new("sftp://abc123/home/you/x y"));
        assert!(!path.is_absolute());
        assert_eq!(split_virtual(&path), Some(("abc123".to_owned(), PathBuf::from("/home/you/x y"))));
        assert_eq!(split_virtual(Path::new("sftp://abc123")), Some(("abc123".to_owned(), PathBuf::from("/"))));
        assert_eq!(split_virtual(&virtual_path("abc123", Path::new("/"))), Some(("abc123".to_owned(), PathBuf::from("/"))));
        assert_eq!(split_virtual(Path::new("/home/me")), None);
        assert_eq!(split_virtual(Path::new("sftp:///x")), None);
        assert_eq!(normalize(Path::new("../../etc")), Path::new("/etc"));
    }
}
