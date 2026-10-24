//! Thin safe wrappers around the few libc calls std does not expose.

use std::ffi::{CStr, CString};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_BUFFER: usize = 1 << 20;

fn c_path(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "cesta obsahuje NUL"))
}

/// Calls a reentrant `get*_r` function with a growing buffer.
fn with_buffer<T>(mut call: impl FnMut(&mut [libc::c_char]) -> (libc::c_int, Option<T>)) -> Option<T> {
    let mut buffer = vec![0 as libc::c_char; 4096];
    loop {
        let (rc, value) = call(&mut buffer);
        if rc == libc::ERANGE && buffer.len() < MAX_BUFFER {
            buffer.resize(buffer.len() * 2, 0);
            continue;
        }
        return if rc == 0 { value } else { None };
    }
}

pub fn user_name(uid: u32) -> Option<String> {
    with_buffer(|buffer| {
        let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
        let mut result: *mut libc::passwd = std::ptr::null_mut();
        let rc = unsafe {
            libc::getpwuid_r(uid, &mut entry, buffer.as_mut_ptr(), buffer.len(), &mut result)
        };
        let name = (!result.is_null())
            .then(|| unsafe { CStr::from_ptr(entry.pw_name) }.to_string_lossy().into_owned());
        (rc, name)
    })
}

pub fn group_name(gid: u32) -> Option<String> {
    with_buffer(|buffer| {
        let mut entry: libc::group = unsafe { std::mem::zeroed() };
        let mut result: *mut libc::group = std::ptr::null_mut();
        let rc = unsafe {
            libc::getgrgid_r(gid, &mut entry, buffer.as_mut_ptr(), buffer.len(), &mut result)
        };
        let name = (!result.is_null())
            .then(|| unsafe { CStr::from_ptr(entry.gr_name) }.to_string_lossy().into_owned());
        (rc, name)
    })
}

pub fn user_id(name: &str) -> Option<u32> {
    if let Ok(uid) = name.parse() {
        return Some(uid);
    }
    let name = CString::new(name).ok()?;
    with_buffer(|buffer| {
        let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
        let mut result: *mut libc::passwd = std::ptr::null_mut();
        let rc = unsafe {
            libc::getpwnam_r(
                name.as_ptr(),
                &mut entry,
                buffer.as_mut_ptr(),
                buffer.len(),
                &mut result,
            )
        };
        (rc, (!result.is_null()).then_some(entry.pw_uid))
    })
}

pub fn group_id(name: &str) -> Option<u32> {
    if let Ok(gid) = name.parse() {
        return Some(gid);
    }
    let name = CString::new(name).ok()?;
    with_buffer(|buffer| {
        let mut entry: libc::group = unsafe { std::mem::zeroed() };
        let mut result: *mut libc::group = std::ptr::null_mut();
        let rc = unsafe {
            libc::getgrnam_r(
                name.as_ptr(),
                &mut entry,
                buffer.as_mut_ptr(),
                buffer.len(),
                &mut result,
            )
        };
        (rc, (!result.is_null()).then_some(entry.gr_gid))
    })
}

pub fn effective_uid() -> u32 {
    unsafe { libc::geteuid() }
}

/// All user names known to NSS, sorted. Uses the non-reentrant `getpwent`, so it must only be
/// called from the GTK main thread.
pub fn all_users() -> Vec<String> {
    let mut names = Vec::new();
    unsafe {
        libc::setpwent();
        loop {
            let entry = libc::getpwent();
            if entry.is_null() {
                break;
            }
            names.push(CStr::from_ptr((*entry).pw_name).to_string_lossy().into_owned());
        }
        libc::endpwent();
    }
    names.sort();
    names.dedup();
    names
}

/// All group names known to NSS, sorted. Main thread only, see [`all_users`].
pub fn all_groups() -> Vec<String> {
    let mut names = Vec::new();
    unsafe {
        libc::setgrent();
        loop {
            let entry = libc::getgrent();
            if entry.is_null() {
                break;
            }
            names.push(CStr::from_ptr((*entry).gr_name).to_string_lossy().into_owned());
        }
        libc::endgrent();
    }
    names.sort();
    names.dedup();
    names
}

fn to_timespec(time: Option<SystemTime>) -> libc::timespec {
    let mut spec: libc::timespec = unsafe { std::mem::zeroed() };
    match time {
        None => spec.tv_nsec = libc::UTIME_OMIT,
        Some(time) => match time.duration_since(UNIX_EPOCH) {
            Ok(duration) => {
                spec.tv_sec = duration.as_secs() as libc::time_t;
                spec.tv_nsec = duration.subsec_nanos() as _;
            }
            Err(error) => {
                let duration = error.duration();
                let mut seconds = -(duration.as_secs() as i64);
                let mut nanos = i64::from(duration.subsec_nanos());
                if nanos > 0 {
                    seconds -= 1;
                    nanos = 1_000_000_000 - nanos;
                }
                spec.tv_sec = seconds as libc::time_t;
                spec.tv_nsec = nanos as _;
            }
        },
    }
    spec
}

/// Sets access and/or modification time without following a final symlink.
pub fn set_times(path: &Path, accessed: Option<SystemTime>, modified: Option<SystemTime>) -> io::Result<()> {
    if accessed.is_none() && modified.is_none() {
        return Ok(());
    }
    let path = c_path(path)?;
    let times = [to_timespec(accessed), to_timespec(modified)];
    let rc = unsafe {
        libc::utimensat(
            libc::AT_FDCWD,
            path.as_ptr(),
            times.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DiskSpace {
    pub available: u64,
    pub total: u64,
}

pub fn disk_space(path: &Path) -> Option<DiskSpace> {
    let path = c_path(path).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(path.as_ptr(), &mut stat) } != 0 {
        return None;
    }
    let fragment = stat.f_frsize as u64;
    Some(DiskSpace {
        available: (stat.f_bavail as u64).saturating_mul(fragment),
        total: (stat.f_blocks as u64).saturating_mul(fragment),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::MetadataExt;
    use std::time::Duration;

    #[test]
    fn resolves_root_user_and_group() {
        assert_eq!(user_name(0).as_deref(), Some("root"));
        assert_eq!(group_name(0).as_deref(), Some("root"));
        assert_eq!(user_id("root"), Some(0));
        assert_eq!(group_id("root"), Some(0));
        assert_eq!(user_id("1234"), Some(1234));
    }

    #[test]
    fn set_times_changes_only_requested_time() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("file");
        fs::write(&path, "x").unwrap();
        let before = fs::metadata(&path).unwrap();
        let modified = UNIX_EPOCH + Duration::from_secs(1_000_000_000);
        set_times(&path, None, Some(modified)).unwrap();
        let after = fs::metadata(&path).unwrap();
        assert_eq!(after.mtime(), 1_000_000_000);
        assert_eq!(after.atime(), before.atime());
    }

    #[test]
    fn disk_space_reports_nonzero_total() {
        let space = disk_space(Path::new("/")).unwrap();
        assert!(space.total > 0);
        assert!(space.available <= space.total);
    }
}
