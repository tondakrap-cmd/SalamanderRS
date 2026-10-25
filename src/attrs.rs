//! Changing Unix permissions, ownership and timestamps, optionally recursively.
//!
//! Pure Rust without GTK so it can be unit-tested; runs in a worker thread.

use std::fs;
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

use crate::model::OpError;
use crate::sys;

/// Permission bits to set and clear; bits in neither mask stay as they are.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ModeChange {
    pub set: u32,
    pub clear: u32,
    /// `chmod X` semantics: execute bits being set are applied only to directories and to
    /// files that already have at least one execute bit.
    pub exec_only_dirs: bool,
}

impl ModeChange {
    pub fn apply(&self, old: u32, is_dir: bool) -> u32 {
        let mut set = self.set & 0o7777;
        if self.exec_only_dirs && !is_dir && old & 0o111 == 0 {
            set &= !0o111;
        }
        (old & 0o7777 & !self.clear) | set
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Scope {
    #[default]
    All,
    FilesOnly,
    DirsOnly,
}

#[derive(Clone, Debug, Default)]
pub struct AttrChange {
    pub mode: Option<ModeChange>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub accessed: Option<SystemTime>,
    pub modified: Option<SystemTime>,
    pub recursive: bool,
    /// Which items are changed when `recursive` is set; selected items are always changed
    /// when not recursive.
    pub scope: Scope,
}

impl AttrChange {
    pub fn is_empty(&self) -> bool {
        self.mode
            .is_none_or(|mode| mode.set == 0 && mode.clear == 0)
            && self.uid.is_none()
            && self.gid.is_none()
            && self.accessed.is_none()
            && self.modified.is_none()
    }
}

#[derive(Debug, Default)]
pub struct AttrResult {
    pub changed: u64,
    pub errors: Vec<OpError>,
    pub cancelled: bool,
}

pub fn apply(
    paths: &[PathBuf],
    change: &AttrChange,
    cancel: &AtomicBool,
    mut progress: impl FnMut(&Path, u64),
) -> AttrResult {
    let mut result = AttrResult::default();
    for path in paths {
        if cancel.load(Ordering::Relaxed) {
            result.cancelled = true;
            break;
        }
        apply_one(path, change, cancel, &mut progress, &mut result);
    }
    result
}

fn in_scope(scope: Scope, recursive: bool, is_dir: bool) -> bool {
    !recursive
        || match scope {
            Scope::All => true,
            Scope::FilesOnly => !is_dir,
            Scope::DirsOnly => is_dir,
        }
}

fn record(result: &mut AttrResult, path: &Path, error: io::Error) {
    result.errors.push(OpError {
        source: path.to_path_buf(),
        target: None,
        error: error.to_string(),
    });
}

fn apply_one(
    path: &Path,
    change: &AttrChange,
    cancel: &AtomicBool,
    progress: &mut impl FnMut(&Path, u64),
    result: &mut AttrResult,
) {
    if cancel.load(Ordering::Relaxed) {
        result.cancelled = true;
        return;
    }
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) => return record(result, path, error),
    };
    let is_symlink = metadata.file_type().is_symlink();
    let is_dir = metadata.is_dir();
    let selected = in_scope(change.scope, change.recursive, is_dir);
    let descend = change.recursive && is_dir && !is_symlink;

    let new_mode = change
        .mode
        .filter(|_| selected && !is_symlink)
        .map(|mode| mode.apply(metadata.mode(), is_dir))
        .filter(|mode| *mode != metadata.mode() & 0o7777);

    // When a directory loses owner read/execute access, its children must be processed
    // before the directory itself or they become unreachable.
    let keeps_access = new_mode.is_none_or(|mode| mode & 0o500 == 0o500) || sys::effective_uid() == 0;
    if descend && !keeps_access {
        apply_children(path, change, cancel, progress, result);
    }

    if selected {
        let mut ok = true;
        if change.uid.is_some() || change.gid.is_some() {
            let uid = change.uid.filter(|uid| *uid != metadata.uid());
            let gid = change.gid.filter(|gid| *gid != metadata.gid());
            if uid.is_some() || gid.is_some() {
                if let Err(error) = std::os::unix::fs::lchown(path, uid, gid) {
                    record(result, path, error);
                    ok = false;
                }
            }
        }
        // chown clears setuid/setgid on files, so the mode is applied afterwards.
        if let Some(mode) = new_mode {
            if let Err(error) = fs::set_permissions(path, fs::Permissions::from_mode(mode)) {
                record(result, path, error);
                ok = false;
            }
        }
        if change.accessed.is_some() || change.modified.is_some() {
            if let Err(error) = sys::set_times(path, change.accessed, change.modified) {
                record(result, path, error);
                ok = false;
            }
        }
        if ok {
            result.changed += 1;
        }
        progress(path, result.changed);
    }

    if descend && keeps_access {
        apply_children(path, change, cancel, progress, result);
    }
}

fn apply_children(
    path: &Path,
    change: &AttrChange,
    cancel: &AtomicBool,
    progress: &mut impl FnMut(&Path, u64),
    result: &mut AttrResult,
) {
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) => return record(result, path, error),
    };
    // Collect first: changing times or modes while iterating is fine, but keeping the
    // directory handle open across a deep recursion would exhaust descriptors.
    let children: Vec<PathBuf> = entries
        .filter_map(|entry| match entry {
            Ok(entry) => Some(entry.path()),
            Err(error) => {
                record(result, path, error);
                None
            }
        })
        .collect();
    for child in children {
        if cancel.load(Ordering::Relaxed) {
            result.cancelled = true;
            return;
        }
        apply_one(&child, change, cancel, progress, result);
    }
}

/// Aggregated permission state of several items: bits set on all, and bits set on any.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ModeSummary {
    pub all: u32,
    pub any: u32,
}

impl ModeSummary {
    pub fn of(modes: impl IntoIterator<Item = u32>) -> Option<ModeSummary> {
        let mut iter = modes.into_iter().map(|mode| mode & 0o7777);
        let first = iter.next()?;
        Some(iter.fold(ModeSummary { all: first, any: first }, |acc, mode| ModeSummary {
            all: acc.all & mode,
            any: acc.any | mode,
        }))
    }

    /// `Some(true/false)` when consistent across all items, `None` when mixed.
    pub fn bit(&self, bit: u32) -> Option<bool> {
        match (self.all & bit != 0, self.any & bit != 0) {
            (true, _) => Some(true),
            (false, false) => Some(false),
            (false, true) => None,
        }
    }
}

pub fn parse_octal(text: &str) -> Option<u32> {
    let text = text.trim();
    if text.is_empty() || text.len() > 4 {
        return None;
    }
    u32::from_str_radix(text, 8).ok().filter(|mode| *mode <= 0o7777)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    fn mode_of(path: &Path) -> u32 {
        fs::symlink_metadata(path).unwrap().mode() & 0o7777
    }

    fn set_mode(path: &Path, mode: u32) {
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn mode_change_sets_and_clears_bits() {
        let change = ModeChange {
            set: 0o750,
            clear: 0o027,
            exec_only_dirs: false,
        };
        assert_eq!(change.apply(0o644, false), 0o750 | (0o644 & !0o027));
        let capital_x = ModeChange {
            set: 0o111,
            clear: 0,
            exec_only_dirs: true,
        };
        assert_eq!(capital_x.apply(0o644, false), 0o644);
        assert_eq!(capital_x.apply(0o744, false), 0o755);
        assert_eq!(capital_x.apply(0o644, true), 0o755);
    }

    #[test]
    fn summary_reports_mixed_bits() {
        let summary = ModeSummary::of([0o755, 0o644]).unwrap();
        assert_eq!(summary.bit(0o400), Some(true));
        assert_eq!(summary.bit(0o100), None);
        assert_eq!(summary.bit(0o4000), Some(false));
        assert!(ModeSummary::of([]).is_none());
    }

    #[test]
    fn parses_octal() {
        assert_eq!(parse_octal("755"), Some(0o755));
        assert_eq!(parse_octal("4755"), Some(0o4755));
        assert_eq!(parse_octal("8"), None);
        assert_eq!(parse_octal("17777"), None);
        assert_eq!(parse_octal(""), None);
    }

    #[test]
    fn recursive_files_only_with_capital_x() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("tree");
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("plain.txt"), "").unwrap();
        fs::write(root.join("sub/run.sh"), "").unwrap();
        set_mode(&root.join("plain.txt"), 0o600);
        set_mode(&root.join("sub/run.sh"), 0o700);
        set_mode(&root.join("sub"), 0o700);

        let change = AttrChange {
            mode: Some(ModeChange {
                set: 0o055,
                clear: 0,
                exec_only_dirs: true,
            }),
            recursive: true,
            scope: Scope::FilesOnly,
            ..AttrChange::default()
        };
        let result = apply(std::slice::from_ref(&root), &change, &AtomicBool::new(false), |_, _| {});
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert_eq!(mode_of(&root.join("plain.txt")), 0o644);
        assert_eq!(mode_of(&root.join("sub/run.sh")), 0o755);
        // Directories are outside the scope.
        assert_eq!(mode_of(&root.join("sub")), 0o700);
        assert_eq!(result.changed, 2);
    }

    #[test]
    fn removing_directory_access_processes_children_first() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("locked");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("file"), "").unwrap();
        set_mode(&root.join("file"), 0o644);

        let change = AttrChange {
            mode: Some(ModeChange {
                set: 0,
                clear: 0o577,
                exec_only_dirs: false,
            }),
            recursive: true,
            ..AttrChange::default()
        };
        let result = apply(std::slice::from_ref(&root), &change, &AtomicBool::new(false), |_, _| {});
        let root_mode = mode_of(&root);
        set_mode(&root, 0o700);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert_eq!(root_mode, 0o200);
        assert_eq!(mode_of(&root.join("file")), 0o200);
    }

    #[test]
    fn symlinks_keep_mode_but_get_times_without_following() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("target");
        let link = temp.path().join("link");
        fs::write(&target, "").unwrap();
        set_mode(&target, 0o600);
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let target_mtime = fs::metadata(&target).unwrap().mtime();

        let modified = UNIX_EPOCH + Duration::from_secs(1_500_000_000);
        let change = AttrChange {
            mode: Some(ModeChange {
                set: 0o777,
                clear: 0,
                exec_only_dirs: false,
            }),
            modified: Some(modified),
            ..AttrChange::default()
        };
        let result = apply(std::slice::from_ref(&link), &change, &AtomicBool::new(false), |_, _| {});
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert_eq!(mode_of(&target), 0o600);
        assert_eq!(fs::symlink_metadata(&link).unwrap().mtime(), 1_500_000_000);
        assert_eq!(fs::metadata(&target).unwrap().mtime(), target_mtime);
    }

    #[test]
    fn chown_to_own_ids_is_a_noop_success() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("file");
        fs::write(&path, "").unwrap();
        let metadata = fs::metadata(&path).unwrap();
        let change = AttrChange {
            uid: Some(metadata.uid()),
            gid: Some(metadata.gid()),
            ..AttrChange::default()
        };
        let result = apply(&[path], &change, &AtomicBool::new(false), |_, _| {});
        assert!(result.errors.is_empty());
        assert_eq!(result.changed, 1);
    }

    #[test]
    fn cancel_stops_before_first_item() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("file");
        fs::write(&path, "").unwrap();
        set_mode(&path, 0o600);
        let change = AttrChange {
            mode: Some(ModeChange {
                set: 0o077,
                clear: 0,
                exec_only_dirs: false,
            }),
            ..AttrChange::default()
        };
        let result = apply(std::slice::from_ref(&path), &change, &AtomicBool::new(true), |_, _| {});
        assert!(result.cancelled);
        assert_eq!(mode_of(&path), 0o600);
    }
}
