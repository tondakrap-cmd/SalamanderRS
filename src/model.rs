use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Local};

use crate::sys::{self, DiskSpace};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EntryKind {
    Directory,
    File,
    Symlink,
    Other,
}

#[derive(Debug)]
pub struct FileEntry {
    pub path: PathBuf,
    pub name: OsString,
    pub folded_name: String,
    pub kind: EntryKind,
    pub size: u64,
    pub modified: Option<SystemTime>,
    pub accessed: Option<SystemTime>,
    /// Inode change time (ctime); it cannot be set by userspace.
    pub changed: Option<SystemTime>,
    pub created: Option<SystemTime>,
    /// Full `st_mode` including the file type bits.
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub owner: String,
    pub group: String,
    pub nlink: u64,
    pub inode: u64,
    pub link_target: Option<PathBuf>,
    /// For symlinks: whether the (followed) target is a directory.
    pub target_is_dir: bool,
}

impl FileEntry {
    pub fn is_dir_like(&self) -> bool {
        self.kind == EntryKind::Directory || (self.kind == EntryKind::Symlink && self.target_is_dir)
    }

    pub fn is_hidden(&self) -> bool {
        self.folded_name.starts_with('.')
    }

    /// Lower-cased extension without the dot; directories and dot-files without a further
    /// dot have none.
    pub fn extension(&self) -> &str {
        if self.is_dir_like() {
            return "";
        }
        extension_of(&self.folded_name)
    }
}

pub fn extension_of(name: &str) -> &str {
    match name.rfind('.') {
        Some(0) | None => "",
        Some(index) => &name[index + 1..],
    }
}

/// Caches uid/gid → name lookups for one directory read.
#[derive(Default)]
pub struct NameCache {
    users: HashMap<u32, String>,
    groups: HashMap<u32, String>,
}

impl NameCache {
    pub fn user(&mut self, uid: u32) -> String {
        self.users
            .entry(uid)
            .or_insert_with(|| sys::user_name(uid).unwrap_or_else(|| uid.to_string()))
            .clone()
    }

    pub fn group(&mut self, gid: u32) -> String {
        self.groups
            .entry(gid)
            .or_insert_with(|| sys::group_name(gid).unwrap_or_else(|| gid.to_string()))
            .clone()
    }
}

fn ctime(metadata: &fs::Metadata) -> Option<SystemTime> {
    let seconds = u64::try_from(metadata.ctime()).ok()?;
    let nanos = u32::try_from(metadata.ctime_nsec()).unwrap_or(0);
    Some(UNIX_EPOCH + Duration::new(seconds, nanos))
}

pub fn entry_from_metadata(
    path: PathBuf,
    name: OsString,
    metadata: &fs::Metadata,
    names: &mut NameCache,
) -> FileEntry {
    let file_type = metadata.file_type();
    let kind = if file_type.is_symlink() {
        EntryKind::Symlink
    } else if file_type.is_dir() {
        EntryKind::Directory
    } else if file_type.is_file() {
        EntryKind::File
    } else {
        EntryKind::Other
    };
    let (link_target, target_is_dir) = if kind == EntryKind::Symlink {
        (
            fs::read_link(&path).ok(),
            fs::metadata(&path).is_ok_and(|target| target.is_dir()),
        )
    } else {
        (None, false)
    };
    FileEntry {
        folded_name: name.to_string_lossy().to_lowercase(),
        name,
        kind,
        size: metadata.len(),
        modified: metadata.modified().ok(),
        accessed: metadata.accessed().ok(),
        changed: ctime(metadata),
        created: metadata.created().ok(),
        mode: metadata.mode(),
        uid: metadata.uid(),
        gid: metadata.gid(),
        owner: names.user(metadata.uid()),
        group: names.group(metadata.gid()),
        nlink: metadata.nlink(),
        inode: metadata.ino(),
        link_target,
        target_is_dir,
        path,
    }
}

/// Reads one path without following a final symlink.
pub fn stat_entry(path: &Path) -> io::Result<FileEntry> {
    let metadata = fs::symlink_metadata(path)?;
    let name = path
        .file_name()
        .map(OsString::from)
        .unwrap_or_else(|| path.as_os_str().to_owned());
    Ok(entry_from_metadata(
        path.to_path_buf(),
        name,
        &metadata,
        &mut NameCache::default(),
    ))
}

#[derive(Debug)]
pub struct OpError {
    pub source: PathBuf,
    pub target: Option<PathBuf>,
    pub error: String,
}

#[derive(Debug)]
pub struct DirectoryListing {
    pub entries: Vec<FileEntry>,
    pub warnings: Vec<OpError>,
    pub disk_space: Option<DiskSpace>,
}

pub fn read_directory(path: &Path, show_hidden: bool) -> io::Result<DirectoryListing> {
    let mut entries = Vec::new();
    let mut warnings = Vec::new();
    let mut names = NameCache::default();

    for item in fs::read_dir(path)? {
        let item = match item {
            Ok(item) => item,
            Err(error) => {
                warnings.push(OpError {
                    source: path.to_path_buf(),
                    target: None,
                    error: error.to_string(),
                });
                continue;
            }
        };
        let name = item.file_name();
        if !show_hidden && name.to_string_lossy().starts_with('.') {
            continue;
        }
        let item_path = item.path();
        let metadata = match fs::symlink_metadata(&item_path) {
            Ok(metadata) => metadata,
            Err(error) => {
                warnings.push(OpError {
                    source: item_path,
                    target: None,
                    error: error.to_string(),
                });
                continue;
            }
        };
        entries.push(entry_from_metadata(item_path, name, &metadata, &mut names));
    }

    Ok(DirectoryListing {
        entries,
        warnings,
        disk_space: sys::disk_space(path),
    })
}

pub fn format_size(entry: &FileEntry) -> String {
    match entry.kind {
        EntryKind::Directory => "<DIR>".to_owned(),
        EntryKind::Symlink if entry.target_is_dir => "<LINK→DIR>".to_owned(),
        EntryKind::Symlink => "<LINK>".to_owned(),
        EntryKind::Other => "<SPECIAL>".to_owned(),
        EntryKind::File => format_bytes(entry.size),
    }
}

pub fn format_bytes(size: u64) -> String {
    if size < 1024 {
        return format!("{size} B");
    }
    const UNITS: [&str; 5] = ["KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut value = size as f64;
    let mut unit = "B";
    for candidate in UNITS {
        value /= 1024.0;
        unit = candidate;
        if value < 1024.0 {
            break;
        }
    }
    format!("{value:.1} {unit}")
}

/// Exact byte count with thin-space thousands separators, e.g. "1 234 567".
pub fn format_bytes_exact(size: u64) -> String {
    let digits = size.to_string();
    let mut output = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            output.push('\u{2009}');
        }
        output.push(digit);
    }
    output
}

pub fn format_modified(modified: Option<SystemTime>) -> String {
    format_time(modified, "%Y-%m-%d %H:%M")
}

pub fn format_time(time: Option<SystemTime>, format: &str) -> String {
    time.map_or_else(
        || "-".to_owned(),
        |time| DateTime::<Local>::from(time).format(format).to_string(),
    )
}

/// Parses "YYYY-MM-DD HH:MM[:SS]" in local time.
pub fn parse_local_time(text: &str) -> Option<SystemTime> {
    let text = text.trim();
    let naive = chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S")
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M"))
        .ok()?;
    let local = naive.and_local_timezone(Local).earliest()?;
    Some(SystemTime::from(local))
}

pub const S_IFMT: u32 = 0o170000;
pub const S_IFDIR: u32 = 0o040000;
pub const S_IFLNK: u32 = 0o120000;

/// `ls -l` style mode string, e.g. "drwxr-sr-t".
pub fn format_mode(mode: u32) -> String {
    let kind = match mode & S_IFMT {
        S_IFDIR => 'd',
        S_IFLNK => 'l',
        0o010000 => 'p',
        0o140000 => 's',
        0o020000 => 'c',
        0o060000 => 'b',
        _ => '-',
    };
    let bit = |mask: u32, ch: char| if mode & mask != 0 { ch } else { '-' };
    let exec = |mask: u32, special: u32, ch: char| match (mode & special != 0, mode & mask != 0) {
        (true, true) => ch,
        (true, false) => ch.to_ascii_uppercase(),
        (false, true) => 'x',
        (false, false) => '-',
    };
    [
        kind,
        bit(0o400, 'r'),
        bit(0o200, 'w'),
        exec(0o100, 0o4000, 's'),
        bit(0o040, 'r'),
        bit(0o020, 'w'),
        exec(0o010, 0o2000, 's'),
        bit(0o004, 'r'),
        bit(0o002, 'w'),
        exec(0o001, 0o1000, 't'),
    ]
    .into_iter()
    .collect()
}

pub fn format_octal(mode: u32) -> String {
    format!("{:04o}", mode & 0o7777)
}

/// File selection mask: `*.rs;*.toml|target*` — patterns before `|` include, after exclude.
/// Patterns are case-insensitive, support `*` and `?`, and `*.*` matches every name.
#[derive(Clone, Debug)]
pub struct Mask {
    include: Vec<String>,
    exclude: Vec<String>,
}

impl Mask {
    pub fn parse(text: &str) -> Mask {
        let (include, exclude) = text.split_once('|').unwrap_or((text, ""));
        let split = |part: &str| -> Vec<String> {
            part.split([';', ','])
                .map(str::trim)
                .filter(|pattern| !pattern.is_empty())
                .map(|pattern| {
                    if pattern == "*.*" {
                        "*".to_owned()
                    } else {
                        pattern.to_lowercase()
                    }
                })
                .collect()
        };
        let mut include = split(include);
        if include.is_empty() {
            include.push("*".to_owned());
        }
        Mask {
            include,
            exclude: split(exclude),
        }
    }

    pub fn matches(&self, name: &str) -> bool {
        let name = name.to_lowercase();
        self.include
            .iter()
            .any(|pattern| wildcard_match(pattern, &name))
            && !self
                .exclude
                .iter()
                .any(|pattern| wildcard_match(pattern, &name))
    }
}

pub fn wildcard_match(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    let (mut p, mut t) = (0, 0);
    let mut star: Option<usize> = None;
    let mut resume = 0;
    while t < text.len() {
        if p < pattern.len() && (pattern[p] == '?' || pattern[p] == text[t]) {
            p += 1;
            t += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            star = Some(p);
            resume = t;
            p += 1;
        } else if let Some(star) = star {
            p = star + 1;
            resume += 1;
            t = resume;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == '*' {
        p += 1;
    }
    p == pattern.len()
}

#[derive(Clone, Debug)]
pub struct CompareItem {
    pub name: OsString,
    pub size: u64,
    pub modified: Option<SystemTime>,
}

/// Returns names to mark on the left and right side: files missing on the other side,
/// newer files, and files with the same time but a different size. Times within two
/// seconds are treated as equal (FAT/exFAT resolution).
pub fn compare_listings(
    left: &[CompareItem],
    right: &[CompareItem],
) -> (HashSet<OsString>, HashSet<OsString>) {
    const TOLERANCE: Duration = Duration::from_secs(2);
    let right_by_name: HashMap<&OsString, &CompareItem> =
        right.iter().map(|item| (&item.name, item)).collect();
    let left_names: HashSet<&OsString> = left.iter().map(|item| &item.name).collect();
    let mut left_marks = HashSet::new();
    let mut right_marks = HashSet::new();

    for item in left {
        let Some(other) = right_by_name.get(&item.name) else {
            left_marks.insert(item.name.clone());
            continue;
        };
        let ordering = match (item.modified, other.modified) {
            (Some(a), Some(b)) if a > b + TOLERANCE => std::cmp::Ordering::Greater,
            (Some(a), Some(b)) if b > a + TOLERANCE => std::cmp::Ordering::Less,
            _ => std::cmp::Ordering::Equal,
        };
        match ordering {
            std::cmp::Ordering::Greater => {
                left_marks.insert(item.name.clone());
            }
            std::cmp::Ordering::Less => {
                right_marks.insert(item.name.clone());
            }
            std::cmp::Ordering::Equal if item.size != other.size => {
                left_marks.insert(item.name.clone());
                right_marks.insert(item.name.clone());
            }
            std::cmp::Ordering::Equal => {}
        }
    }
    for item in right {
        if !left_names.contains(&item.name) {
            right_marks.insert(item.name.clone());
        }
    }
    (left_marks, right_marks)
}

mod entry_object {
    use super::{Cell, FileEntry};
    use gtk::glib;
    use gtk::glib::object::ObjectExt;
    use gtk::subclass::prelude::*;

    mod imp {
        use super::*;

        #[derive(glib::Properties, Default)]
        #[properties(wrapper_type = super::EntryObject)]
        pub struct EntryObject {
            pub entry: std::cell::OnceCell<FileEntry>,
            #[property(get, set)]
            pub marked: Cell<bool>,
            /// Computed recursive size of a directory; -1 while unknown.
            #[property(get, set)]
            pub dir_size: Cell<i64>,
        }

        #[glib::object_subclass]
        impl ObjectSubclass for EntryObject {
            const NAME: &'static str = "SalamanderRsEntryObject";
            type Type = super::EntryObject;
        }

        #[glib::derived_properties]
        impl ObjectImpl for EntryObject {}
    }

    glib::wrapper! {
        pub struct EntryObject(ObjectSubclass<imp::EntryObject>);
    }

    impl EntryObject {
        pub fn new(entry: FileEntry) -> Self {
            let object: Self = glib::Object::builder().property("dir-size", -1i64).build();
            object
                .imp()
                .entry
                .set(entry)
                .expect("entry initialized once");
            object
        }

        pub fn with_entry<R>(&self, f: impl FnOnce(&FileEntry) -> R) -> R {
            f(self.imp().entry.get().expect("entry initialized"))
        }

        pub fn is_marked(&self) -> bool {
            self.imp().marked.get()
        }

        pub fn known_dir_size(&self) -> Option<u64> {
            u64::try_from(self.imp().dir_size.get()).ok()
        }

        /// Size used for sorting and totals: file size, or the computed directory size.
        pub fn effective_size(&self) -> u64 {
            self.with_entry(|entry| match entry.kind {
                super::EntryKind::File => entry.size,
                super::EntryKind::Directory => self.known_dir_size().unwrap_or(0),
                _ => 0,
            })
        }

        pub fn display_size(&self) -> String {
            match self.known_dir_size() {
                Some(size) if self.with_entry(|entry| entry.kind == super::EntryKind::Directory) => {
                    super::format_bytes(size)
                }
                _ => self.with_entry(super::format_size),
            }
        }
    }
}

pub use entry_object::EntryObject;

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStringExt;

    #[test]
    fn directory_preserves_names_hidden_flag_and_symlink_kind() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("target")).unwrap();
        fs::write(temp.path().join(".hidden"), b"hidden").unwrap();
        let non_utf8 = OsString::from_vec(b"bad\xff".to_vec());
        fs::write(temp.path().join(&non_utf8), b"data").unwrap();
        std::os::unix::fs::symlink("target", temp.path().join("link")).unwrap();

        let visible = read_directory(temp.path(), false).unwrap();
        assert!(!visible.entries.iter().any(|entry| entry.name == ".hidden"));
        assert!(visible.warnings.is_empty());
        assert!(visible.disk_space.is_some());
        let all = read_directory(temp.path(), true).unwrap();
        assert!(all.entries.iter().any(|entry| entry.name == ".hidden"));
        assert!(all.entries.iter().any(|entry| entry.name == non_utf8));
        let link = all
            .entries
            .iter()
            .find(|entry| entry.name == "link")
            .unwrap();
        assert_eq!(link.kind, EntryKind::Symlink);
        assert!(link.target_is_dir);
        assert!(link.is_dir_like());
        assert_eq!(link.link_target.as_deref(), Some(Path::new("target")));
        assert_eq!(format_size(link), "<LINK→DIR>");
    }

    #[test]
    fn entries_carry_unix_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("script.sh");
        fs::write(&path, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o750)).unwrap();
        let entry = stat_entry(&path).unwrap();
        assert_eq!(entry.mode & 0o7777, 0o750);
        assert_eq!(format_mode(entry.mode), "-rwxr-x---");
        assert_eq!(entry.uid, sys::effective_uid());
        assert!(!entry.owner.is_empty());
        assert_eq!(entry.extension(), "sh");
        assert_eq!(entry.nlink, 1);
    }

    #[test]
    fn formats_sizes_and_missing_time() {
        assert_eq!(format_bytes(1023), "1023 B");
        assert_eq!(format_bytes(1536), "1.5 KiB");
        assert_eq!(format_bytes(1024 * 1024), "1.0 MiB");
        assert_eq!(format_modified(None), "-");
        assert_eq!(format_bytes_exact(1234567), "1\u{2009}234\u{2009}567");
        assert_eq!(format_bytes_exact(999), "999");
    }

    #[test]
    fn formats_mode_with_special_bits() {
        assert_eq!(format_mode(S_IFDIR | 0o755), "drwxr-xr-x");
        assert_eq!(format_mode(0o100000 | 0o4755), "-rwsr-xr-x");
        assert_eq!(format_mode(0o100000 | 0o2644), "-rw-r-Sr--");
        assert_eq!(format_mode(S_IFDIR | 0o1777), "drwxrwxrwt");
        assert_eq!(format_mode(S_IFDIR | 0o1776), "drwxrwxrwT");
        assert_eq!(format_mode(S_IFLNK | 0o777), "lrwxrwxrwx");
        assert_eq!(format_octal(S_IFDIR | 0o1755), "1755");
    }

    #[test]
    fn extensions() {
        assert_eq!(extension_of("archive.tar.gz"), "gz");
        assert_eq!(extension_of(".bashrc"), "");
        assert_eq!(extension_of("readme"), "");
        assert_eq!(extension_of(".config.toml"), "toml");
    }

    #[test]
    fn wildcard_and_mask_matching() {
        assert!(wildcard_match("*.txt", "notes.txt"));
        assert!(!wildcard_match("*.txt", "notes.txt.bak"));
        assert!(wildcard_match("a?c*", "abcdef"));
        assert!(wildcard_match("*", ""));
        assert!(!wildcard_match("?", ""));
        assert!(wildcard_match("*a*b*", "xxaxxbxx"));

        let mask = Mask::parse("*.RS; *.toml | main*");
        assert!(mask.matches("lib.rs"));
        assert!(mask.matches("Cargo.TOML"));
        assert!(!mask.matches("main.rs"));
        assert!(!mask.matches("readme.md"));
        assert!(Mask::parse("*.*").matches("Makefile"));
        assert!(Mask::parse("").matches("anything"));
    }

    #[test]
    fn parses_local_time() {
        let time = parse_local_time("2024-02-29 13:45:10").unwrap();
        assert_eq!(format_time(Some(time), "%Y-%m-%d %H:%M:%S"), "2024-02-29 13:45:10");
        assert!(parse_local_time("2024-02-29 13:45").is_some());
        assert!(parse_local_time("nonsense").is_none());
    }

    #[test]
    fn compares_listings_by_presence_time_and_size() {
        let t = |seconds| Some(UNIX_EPOCH + Duration::from_secs(seconds));
        let item = |name: &str, size, modified| CompareItem {
            name: name.into(),
            size,
            modified,
        };
        let left = [
            item("same", 1, t(100)),
            item("newer-left", 1, t(200)),
            item("older-left", 1, t(100)),
            item("size", 1, t(100)),
            item("only-left", 1, t(100)),
            item("fat-time", 1, t(101)),
        ];
        let right = [
            item("same", 1, t(100)),
            item("newer-left", 1, t(100)),
            item("older-left", 1, t(200)),
            item("size", 2, t(100)),
            item("only-right", 1, t(100)),
            item("fat-time", 1, t(100)),
        ];
        let (l, r) = compare_listings(&left, &right);
        let set = |names: &[&str]| names.iter().map(OsString::from).collect::<HashSet<_>>();
        assert_eq!(l, set(&["newer-left", "size", "only-left"]));
        assert_eq!(r, set(&["older-left", "size", "only-right"]));
    }
}
