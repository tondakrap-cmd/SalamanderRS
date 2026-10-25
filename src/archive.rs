//! Archives through libarchive: browsing as directories, extracting, creating and
//! modifying (add/delete by rewriting into a temporary archive).
//!
//! Supports reading everything libarchive knows (zip, tar.*, 7z, rar, iso, cpio, deb, …)
//! and writing tar, zip, 7z and cpio with the common compression filters.

use std::collections::{BTreeMap, HashMap};
use std::ffi::{CStr, CString, OsStr, OsString};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::raw::{c_char, c_int, c_long, c_uint, c_void};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::model::{EntryKind, OpError};
use crate::sys;

#[repr(C)]
struct RawArchive {
    _private: [u8; 0],
}

#[repr(C)]
struct RawEntry {
    _private: [u8; 0],
}

const ARCHIVE_EOF: c_int = 1;
const ARCHIVE_OK: c_int = 0;
const ARCHIVE_WARN: c_int = -20;

const AE_IFMT: u32 = 0o170000;
const AE_IFREG: u32 = 0o100000;
const AE_IFDIR: u32 = 0o040000;
const AE_IFLNK: u32 = 0o120000;

const FORMAT_FAMILY_MASK: c_int = 0xff0000;
const FORMAT_CPIO: c_int = 0x10000;
const FORMAT_TAR: c_int = 0x30000;
const FORMAT_TAR_PAX_RESTRICTED: c_int = 0x30003;
const FORMAT_ZIP: c_int = 0x50000;
const FORMAT_7ZIP: c_int = 0xE0000;

const FILTER_NONE: c_int = 0;
const FILTER_GZIP: c_int = 1;
const FILTER_BZIP2: c_int = 2;
const FILTER_LZMA: c_int = 5;
const FILTER_XZ: c_int = 6;
const FILTER_LZIP: c_int = 9;
const FILTER_LZ4: c_int = 13;
const FILTER_ZSTD: c_int = 14;

extern "C" {
    fn archive_read_new() -> *mut RawArchive;
    fn archive_read_support_filter_all(archive: *mut RawArchive) -> c_int;
    fn archive_read_support_format_all(archive: *mut RawArchive) -> c_int;
    fn archive_read_open_filename(
        archive: *mut RawArchive,
        filename: *const c_char,
        block_size: usize,
    ) -> c_int;
    fn archive_read_next_header(archive: *mut RawArchive, entry: *mut *mut RawEntry) -> c_int;
    fn archive_read_data(archive: *mut RawArchive, buffer: *mut c_void, size: usize) -> isize;
    fn archive_read_free(archive: *mut RawArchive) -> c_int;
    fn archive_format(archive: *mut RawArchive) -> c_int;
    fn archive_format_name(archive: *mut RawArchive) -> *const c_char;
    fn archive_filter_count(archive: *mut RawArchive) -> c_int;
    fn archive_filter_code(archive: *mut RawArchive, index: c_int) -> c_int;
    fn archive_filter_name(archive: *mut RawArchive, index: c_int) -> *const c_char;
    fn archive_error_string(archive: *mut RawArchive) -> *const c_char;

    fn archive_write_new() -> *mut RawArchive;
    fn archive_write_set_format(archive: *mut RawArchive, code: c_int) -> c_int;
    fn archive_write_add_filter(archive: *mut RawArchive, code: c_int) -> c_int;
    fn archive_write_open_filename(archive: *mut RawArchive, filename: *const c_char) -> c_int;
    fn archive_write_header(archive: *mut RawArchive, entry: *mut RawEntry) -> c_int;
    fn archive_write_data(archive: *mut RawArchive, buffer: *const c_void, size: usize) -> isize;
    fn archive_write_finish_entry(archive: *mut RawArchive) -> c_int;
    fn archive_write_close(archive: *mut RawArchive) -> c_int;
    fn archive_write_free(archive: *mut RawArchive) -> c_int;

    fn archive_entry_new() -> *mut RawEntry;
    fn archive_entry_free(entry: *mut RawEntry);
    fn archive_entry_pathname(entry: *mut RawEntry) -> *const c_char;
    fn archive_entry_size(entry: *mut RawEntry) -> i64;
    fn archive_entry_mtime(entry: *mut RawEntry) -> libc::time_t;
    fn archive_entry_mtime_nsec(entry: *mut RawEntry) -> c_long;
    fn archive_entry_mtime_is_set(entry: *mut RawEntry) -> c_int;
    fn archive_entry_filetype(entry: *mut RawEntry) -> libc::mode_t;
    fn archive_entry_perm(entry: *mut RawEntry) -> libc::mode_t;
    fn archive_entry_uname(entry: *mut RawEntry) -> *const c_char;
    fn archive_entry_gname(entry: *mut RawEntry) -> *const c_char;
    fn archive_entry_uid(entry: *mut RawEntry) -> i64;
    fn archive_entry_gid(entry: *mut RawEntry) -> i64;
    fn archive_entry_symlink(entry: *mut RawEntry) -> *const c_char;
    fn archive_entry_hardlink(entry: *mut RawEntry) -> *const c_char;
    fn archive_entry_is_encrypted(entry: *mut RawEntry) -> c_int;
    fn archive_entry_set_pathname(entry: *mut RawEntry, path: *const c_char);
    fn archive_entry_set_size(entry: *mut RawEntry, size: i64);
    fn archive_entry_set_filetype(entry: *mut RawEntry, kind: c_uint);
    fn archive_entry_set_perm(entry: *mut RawEntry, perm: libc::mode_t);
    fn archive_entry_set_mtime(entry: *mut RawEntry, seconds: libc::time_t, nanos: c_long);
    fn archive_entry_set_symlink(entry: *mut RawEntry, target: *const c_char);
    fn archive_entry_set_uid(entry: *mut RawEntry, uid: i64);
    fn archive_entry_set_gid(entry: *mut RawEntry, gid: i64);
    fn archive_entry_set_uname(entry: *mut RawEntry, name: *const c_char);
    fn archive_entry_set_gname(entry: *mut RawEntry, name: *const c_char);
}

fn c_string(bytes: &[u8]) -> Result<CString, String> {
    CString::new(bytes).map_err(|_| "cesta obsahuje NUL".to_owned())
}

/// Copies a possibly-null C string.
unsafe fn opt_bytes(pointer: *const c_char) -> Option<Vec<u8>> {
    (!pointer.is_null()).then(|| CStr::from_ptr(pointer).to_bytes().to_vec())
}

fn error_of(archive: *mut RawArchive) -> String {
    unsafe { opt_bytes(archive_error_string(archive)) }
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_else(|| "neznámá chyba libarchive".to_owned())
}

struct Reader(*mut RawArchive);

impl Reader {
    fn open(path: &Path) -> Result<Reader, String> {
        let name = c_string(path.as_os_str().as_bytes())?;
        let reader = Reader(unsafe { archive_read_new() });
        if reader.0.is_null() {
            return Err("libarchive: nedostatek paměti".to_owned());
        }
        unsafe {
            archive_read_support_filter_all(reader.0);
            archive_read_support_format_all(reader.0);
            if archive_read_open_filename(reader.0, name.as_ptr(), 64 * 1024) != ARCHIVE_OK {
                return Err(error_of(reader.0));
            }
        }
        Ok(reader)
    }

    /// Next entry header; the pointer stays valid until the next call.
    fn next(&mut self) -> Result<Option<*mut RawEntry>, String> {
        let mut entry = std::ptr::null_mut();
        match unsafe { archive_read_next_header(self.0, &mut entry) } {
            ARCHIVE_EOF => Ok(None),
            ARCHIVE_OK | ARCHIVE_WARN => Ok(Some(entry)),
            _ => Err(error_of(self.0)),
        }
    }

    fn read(&mut self, buffer: &mut [u8]) -> Result<usize, String> {
        let read = unsafe { archive_read_data(self.0, buffer.as_mut_ptr().cast(), buffer.len()) };
        if read >= 0 {
            Ok(read as usize)
        } else {
            Err(error_of(self.0))
        }
    }

    fn format_description(&self) -> String {
        let mut text = unsafe { opt_bytes(archive_format_name(self.0)) }
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_default();
        for index in 0..unsafe { archive_filter_count(self.0) } {
            if unsafe { archive_filter_code(self.0, index) } == FILTER_NONE {
                continue;
            }
            if let Some(name) = unsafe { opt_bytes(archive_filter_name(self.0, index)) } {
                text.push_str(" + ");
                text.push_str(&String::from_utf8_lossy(&name));
            }
        }
        text
    }

    fn write_format(&self) -> Option<WriteFormat> {
        let format = unsafe { archive_format(self.0) };
        let format = match format & FORMAT_FAMILY_MASK {
            FORMAT_TAR | FORMAT_CPIO => format,
            FORMAT_ZIP => FORMAT_ZIP,
            FORMAT_7ZIP => FORMAT_7ZIP,
            _ => return None,
        };
        let mut filters = Vec::new();
        for index in 0..unsafe { archive_filter_count(self.0) } {
            match unsafe { archive_filter_code(self.0, index) } {
                FILTER_NONE => {}
                code @ (FILTER_GZIP | FILTER_BZIP2 | FILTER_LZMA | FILTER_XZ | FILTER_LZIP
                | FILTER_LZ4 | FILTER_ZSTD) => filters.push(code),
                _ => return None,
            }
        }
        Some(WriteFormat { format, filters })
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        unsafe {
            archive_read_free(self.0);
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WriteFormat {
    format: c_int,
    filters: Vec<c_int>,
}

struct Writer {
    raw: *mut RawArchive,
    closed: bool,
}

impl Writer {
    fn create(path: &Path, format: &WriteFormat) -> Result<Writer, String> {
        let name = c_string(path.as_os_str().as_bytes())?;
        let writer = Writer {
            raw: unsafe { archive_write_new() },
            closed: false,
        };
        if writer.raw.is_null() {
            return Err("libarchive: nedostatek paměti".to_owned());
        }
        unsafe {
            if archive_write_set_format(writer.raw, format.format) != ARCHIVE_OK {
                return Err(error_of(writer.raw));
            }
            for filter in &format.filters {
                if archive_write_add_filter(writer.raw, *filter) != ARCHIVE_OK {
                    return Err(error_of(writer.raw));
                }
            }
            if archive_write_open_filename(writer.raw, name.as_ptr()) != ARCHIVE_OK {
                return Err(error_of(writer.raw));
            }
        }
        Ok(writer)
    }

    fn header(&mut self, entry: *mut RawEntry) -> Result<(), String> {
        match unsafe { archive_write_header(self.raw, entry) } {
            ARCHIVE_OK | ARCHIVE_WARN => Ok(()),
            _ => Err(error_of(self.raw)),
        }
    }

    fn data(&mut self, data: &[u8]) -> Result<(), String> {
        let mut rest = data;
        while !rest.is_empty() {
            let written = unsafe { archive_write_data(self.raw, rest.as_ptr().cast(), rest.len()) };
            if written <= 0 {
                return Err(error_of(self.raw));
            }
            rest = &rest[written as usize..];
        }
        Ok(())
    }

    fn finish_entry(&mut self) -> Result<(), String> {
        match unsafe { archive_write_finish_entry(self.raw) } {
            ARCHIVE_OK | ARCHIVE_WARN => Ok(()),
            _ => Err(error_of(self.raw)),
        }
    }

    fn close(mut self) -> Result<(), String> {
        self.closed = true;
        match unsafe { archive_write_close(self.raw) } {
            ARCHIVE_OK | ARCHIVE_WARN => Ok(()),
            _ => Err(error_of(self.raw)),
        }
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        unsafe {
            if !self.closed {
                archive_write_close(self.raw);
            }
            archive_write_free(self.raw);
        }
    }
}

/// Owned `archive_entry` for writing new members.
struct NewEntry(*mut RawEntry);

impl NewEntry {
    fn new() -> NewEntry {
        NewEntry(unsafe { archive_entry_new() })
    }
}

impl Drop for NewEntry {
    fn drop(&mut self) {
        unsafe { archive_entry_free(self.0) }
    }
}

#[derive(Clone, Debug)]
pub struct ArchEntry {
    /// Normalized inner path without leading `/` or `./`.
    pub path: PathBuf,
    pub kind: EntryKind,
    pub size: u64,
    pub modified: Option<SystemTime>,
    /// File type and permission bits.
    pub mode: u32,
    pub owner: String,
    pub group: String,
    pub link_target: Option<PathBuf>,
    pub encrypted: bool,
    /// The stored path contained `..` and must never be extracted.
    pub unsafe_path: bool,
}

/// Turns a stored member name into a relative path of normal components.
fn normalize(bytes: &[u8]) -> (PathBuf, bool) {
    let raw = PathBuf::from(OsString::from_vec(bytes.to_vec()));
    let mut path = PathBuf::new();
    let mut unsafe_path = false;
    for component in raw.components() {
        match component {
            Component::Normal(part) => path.push(part),
            Component::ParentDir => unsafe_path = true,
            Component::RootDir | Component::CurDir | Component::Prefix(_) => {}
        }
    }
    (path, unsafe_path)
}

fn entry_info(entry: *mut RawEntry) -> ArchEntry {
    unsafe {
        let (path, unsafe_path) = normalize(&opt_bytes(archive_entry_pathname(entry)).unwrap_or_default());
        let file_type = archive_entry_filetype(entry) & AE_IFMT;
        let hardlink = opt_bytes(archive_entry_hardlink(entry));
        let kind = match file_type {
            AE_IFDIR => EntryKind::Directory,
            AE_IFLNK => EntryKind::Symlink,
            AE_IFREG => EntryKind::File,
            0 if hardlink.is_some() => EntryKind::File,
            _ => EntryKind::Other,
        };
        let modified = (archive_entry_mtime_is_set(entry) != 0).then(|| {
            let seconds = archive_entry_mtime(entry);
            let nanos = archive_entry_mtime_nsec(entry).clamp(0, 999_999_999) as u32;
            if seconds >= 0 {
                UNIX_EPOCH + Duration::new(seconds as u64, nanos)
            } else {
                UNIX_EPOCH - Duration::from_secs(seconds.unsigned_abs())
            }
        });
        let name = |pointer: *const c_char, id: i64| {
            opt_bytes(pointer)
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                .unwrap_or_else(|| id.to_string())
        };
        ArchEntry {
            kind,
            size: u64::try_from(archive_entry_size(entry)).unwrap_or(0),
            modified,
            mode: (if file_type == 0 { AE_IFREG } else { file_type }) | (archive_entry_perm(entry) & 0o7777),
            owner: name(archive_entry_uname(entry), archive_entry_uid(entry)),
            group: name(archive_entry_gname(entry), archive_entry_gid(entry)),
            link_target: opt_bytes(archive_entry_symlink(entry))
                .map(|bytes| PathBuf::from(OsString::from_vec(bytes))),
            encrypted: archive_entry_is_encrypted(entry) != 0,
            unsafe_path,
            path,
        }
    }
}

pub struct Index {
    pub entries: Vec<ArchEntry>,
    /// Human readable format, e.g. "POSIX pax interchange format + gzip".
    pub format: String,
    /// `None` for formats libarchive cannot write (rar, iso, …).
    pub write_format: Option<WriteFormat>,
}

impl Index {
    pub fn is_writable(&self) -> bool {
        self.write_format.is_some()
    }

    /// Direct children of `inner`, synthesizing directories that only exist implicitly
    /// as path prefixes. `None` when `inner` is not a directory in the archive.
    pub fn children(&self, inner: &Path) -> Option<Vec<ArchEntry>> {
        let mut children: BTreeMap<OsString, ArchEntry> = BTreeMap::new();
        let mut exists = inner.as_os_str().is_empty();
        for entry in &self.entries {
            if entry.path == inner {
                exists |= entry.kind == EntryKind::Directory;
                continue;
            }
            let Ok(rest) = entry.path.strip_prefix(inner) else {
                continue;
            };
            let mut components = rest.components();
            let Some(first) = components.next() else {
                continue;
            };
            exists = true;
            let name = first.as_os_str().to_owned();
            if components.next().is_none() {
                children.insert(name, entry.clone());
            } else {
                children.entry(name.clone()).or_insert_with(|| ArchEntry {
                    path: inner.join(&name),
                    kind: EntryKind::Directory,
                    size: 0,
                    modified: None,
                    mode: AE_IFDIR | 0o755,
                    owner: String::new(),
                    group: String::new(),
                    link_target: None,
                    encrypted: false,
                    unsafe_path: false,
                });
            }
        }
        exists.then(|| children.into_values().collect())
    }

    pub fn contains(&self, inner: &Path) -> bool {
        self.entries
            .iter()
            .any(|entry| entry.path == inner || entry.path.starts_with(inner))
    }
}

type CacheEntry = (SystemTime, u64, Arc<Index>);

fn cache() -> &'static Mutex<HashMap<PathBuf, CacheEntry>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, CacheEntry>>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// Reads (or returns the cached) list of members; the cache is keyed by mtime and size.
pub fn index(path: &Path) -> Result<Arc<Index>, String> {
    let metadata = fs::metadata(path).map_err(|error| error.to_string())?;
    let stamp = (metadata.modified().unwrap_or(UNIX_EPOCH), metadata.len());
    if let Some((modified, size, index)) = cache().lock().unwrap().get(path) {
        if (*modified, *size) == stamp {
            return Ok(Arc::clone(index));
        }
    }
    let mut reader = Reader::open(path)?;
    let mut entries = Vec::new();
    let mut description = None;
    while let Some(entry) = reader.next()? {
        if description.is_none() {
            // Format and filters are only known after the first header.
            description = Some((reader.format_description(), reader.write_format()));
        }
        let info = entry_info(entry);
        if !info.path.as_os_str().is_empty() {
            entries.push(info);
        }
    }
    let (format, write_format) = description
        .unwrap_or_else(|| (reader.format_description(), reader.write_format()));
    let index = Arc::new(Index {
        entries,
        format,
        write_format,
    });
    cache()
        .lock()
        .unwrap()
        .insert(path.to_path_buf(), (stamp.0, stamp.1, Arc::clone(&index)));
    Ok(index)
}

const EXTENSIONS: &[&str] = &[
    ".zip", ".jar", ".war", ".apk", ".7z", ".tar", ".tgz", ".tbz", ".tbz2", ".txz", ".tzst",
    ".tlz", ".tar.gz", ".tar.bz2", ".tar.xz", ".tar.zst", ".tar.lz", ".tar.lzma", ".tar.lz4",
    ".rar", ".iso", ".cpio", ".deb", ".rpm", ".cab", ".xar",
];

pub fn looks_like_archive(path: &Path) -> bool {
    path.file_name()
        .map(|name| name.to_string_lossy().to_lowercase())
        .is_some_and(|name| EXTENSIONS.iter().any(|extension| name.ends_with(extension)))
}

/// Splits a virtual path like `/data/a.zip/dir/file` into the archive file and the inner
/// path, when one of its ancestors is an existing archive file.
pub fn split_virtual(path: &Path) -> Option<(PathBuf, PathBuf)> {
    let mut candidate = path.to_path_buf();
    loop {
        if candidate.is_file() {
            return looks_like_archive(&candidate).then(|| {
                let inner = path.strip_prefix(&candidate).unwrap_or(Path::new("")).to_path_buf();
                (candidate, inner)
            });
        }
        if candidate.exists() || !candidate.pop() {
            return None;
        }
    }
}

fn selected(path: &Path, items: &[PathBuf]) -> bool {
    items.is_empty() || items.iter().any(|item| path == item || path.starts_with(item))
}

/// Rejects writes through symlinks or files placed by earlier members ("zip slip").
fn check_parents(dest: &Path, relative: &Path) -> Result<(), String> {
    let mut current = dest.to_path_buf();
    if let Some(parent) = relative.parent() {
        for component in parent.components() {
            current.push(component);
            match fs::symlink_metadata(&current) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    return Err("cesta vede přes symbolický odkaz".to_owned());
                }
                Ok(metadata) if !metadata.is_dir() => {
                    return Err(format!("{} není adresář", current.to_string_lossy()));
                }
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => break,
                Err(error) => return Err(error.to_string()),
            }
        }
    }
    Ok(())
}

pub struct ExtractRequest<'a> {
    pub archive: &'a Path,
    /// Inner paths to extract (files or directory subtrees); empty means everything.
    pub items: &'a [PathBuf],
    /// Inner directory stripped from member paths, i.e. the panel's current directory.
    pub base: &'a Path,
    pub dest: &'a Path,
    pub overwrite: bool,
}

#[derive(Debug, Default)]
pub struct JobResult {
    pub done: u64,
    pub errors: Vec<OpError>,
    pub cancelled: bool,
}

impl JobResult {
    fn fail(&mut self, source: impl Into<PathBuf>, target: Option<PathBuf>, error: impl Into<String>) {
        self.errors.push(OpError {
            source: source.into(),
            target,
            error: error.into(),
        });
    }
}

fn remove_existing(path: &Path, overwrite: bool) -> Result<bool, String> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(error.to_string()),
        Ok(_) if !overwrite => Err("cíl již existuje".to_owned()),
        Ok(metadata) if metadata.is_dir() => Err("cíl je adresář".to_owned()),
        Ok(_) => fs::remove_file(path).map(|()| true).map_err(|error| error.to_string()),
    }
}

pub fn extract(
    request: &ExtractRequest,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(&Path),
) -> JobResult {
    let mut result = JobResult::default();
    let mut reader = match Reader::open(request.archive) {
        Ok(reader) => reader,
        Err(error) => {
            result.fail(request.archive, None, error);
            return result;
        }
    };
    let mut buffer = vec![0u8; 256 * 1024];
    let mut directories: Vec<(PathBuf, u32, Option<SystemTime>)> = Vec::new();
    loop {
        if cancel.load(Ordering::Relaxed) {
            result.cancelled = true;
            break;
        }
        let entry = match reader.next() {
            Ok(Some(entry)) => entry,
            Ok(None) => break,
            Err(error) => {
                result.fail(request.archive, None, error);
                break;
            }
        };
        let info = entry_info(entry);
        if info.path.as_os_str().is_empty() || !selected(&info.path, request.items) {
            continue;
        }
        let source = request.archive.join(&info.path);
        if info.unsafe_path {
            result.fail(source, None, "cesta v archivu obsahuje „..“ — přeskočeno");
            continue;
        }
        let Ok(relative) = info.path.strip_prefix(request.base) else {
            continue;
        };
        if relative.as_os_str().is_empty() {
            continue;
        }
        let target = request.dest.join(relative);
        if let Err(error) = check_parents(request.dest, relative) {
            result.fail(source, Some(target), error);
            continue;
        }
        progress(&info.path);
        if let Some(parent) = target.parent() {
            if let Err(error) = fs::create_dir_all(parent) {
                result.fail(source, Some(target), error.to_string());
                continue;
            }
        }
        let hardlink = unsafe { opt_bytes(archive_entry_hardlink(entry)) };
        let outcome: Result<(), String> = match info.kind {
            EntryKind::Directory => match fs::symlink_metadata(&target) {
                Ok(metadata) if metadata.is_dir() => Ok(()),
                Ok(_) => Err("cíl existuje a není adresář".to_owned()),
                Err(_) => fs::create_dir(&target).map_err(|error| error.to_string()),
            }
            .map(|()| directories.push((target.clone(), info.mode, info.modified))),
            EntryKind::Symlink => remove_existing(&target, request.overwrite).and_then(|_| {
                let link = info.link_target.clone().unwrap_or_default();
                std::os::unix::fs::symlink(link, &target).map_err(|error| error.to_string())
            }),
            EntryKind::File if hardlink.is_some() => {
                let (link, _) = normalize(&hardlink.unwrap_or_default());
                match link.strip_prefix(request.base) {
                    Ok(link_relative) if !link_relative.as_os_str().is_empty() => {
                        remove_existing(&target, request.overwrite).and_then(|_| {
                            fs::hard_link(request.dest.join(link_relative), &target)
                                .map_err(|error| format!("pevný odkaz: {error}"))
                        })
                    }
                    _ => Err("pevný odkaz míří mimo rozbalovanou část".to_owned()),
                }
            }
            EntryKind::File if info.encrypted => {
                Err("položka je zašifrovaná (heslo není podporováno)".to_owned())
            }
            EntryKind::File => remove_existing(&target, request.overwrite).and_then(|_| {
                let mut output = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&target)
                    .map_err(|error| error.to_string())?;
                let copied = loop {
                    if cancel.load(Ordering::Relaxed) {
                        break Err("zrušeno".to_owned());
                    }
                    match reader.read(&mut buffer) {
                        Ok(0) => break Ok(()),
                        Ok(read) => {
                            if let Err(error) = output.write_all(&buffer[..read]) {
                                break Err(error.to_string());
                            }
                        }
                        Err(error) => break Err(error),
                    }
                };
                drop(output);
                if let Err(error) = copied {
                    let _ = fs::remove_file(&target);
                    return Err(error);
                }
                // Never recreate setuid/setgid bits from untrusted archives.
                fs::set_permissions(&target, fs::Permissions::from_mode(info.mode & 0o1777))
                    .map_err(|error| error.to_string())?;
                sys::set_times(&target, None, info.modified).map_err(|error| error.to_string())
            }),
            EntryKind::Other | EntryKind::Parent => Err("nepodporovaný typ položky".to_owned()),
        };
        match outcome {
            Ok(()) => result.done += 1,
            Err(error) => result.fail(source, Some(target), error),
        }
    }
    // Directory permissions and times last, deepest first, so writing children cannot fail
    // on a read-only directory or bump its mtime.
    directories.sort_by(|a, b| b.0.components().count().cmp(&a.0.components().count()));
    for (directory, mode, modified) in directories {
        let _ = fs::set_permissions(&directory, fs::Permissions::from_mode((mode & 0o1777) | 0o700));
        let _ = sys::set_times(&directory, None, modified);
    }
    result
}

fn temp_root() -> PathBuf {
    std::env::temp_dir().join(format!("salamander-rs-{}", sys::effective_uid()))
}

/// Extracts one member into a private temporary directory (for viewing or opening).
pub fn extract_to_temp(archive: &Path, inner: &Path) -> Result<PathBuf, String> {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let root = temp_root();
    fs::create_dir_all(&root).map_err(|error| error.to_string())?;
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).map_err(|error| error.to_string())?;
    let dest = root.join(format!(
        "{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&dest).map_err(|error| error.to_string())?;
    let base = inner.parent().unwrap_or(Path::new(""));
    let result = extract(
        &ExtractRequest {
            archive,
            items: &[inner.to_path_buf()],
            base,
            dest: &dest,
            overwrite: true,
        },
        &AtomicBool::new(false),
        &mut |_| {},
    );
    if let Some(error) = result.errors.first() {
        return Err(error.error.clone());
    }
    let path = dest.join(inner.file_name().unwrap_or_default());
    if path.exists() {
        Ok(path)
    } else {
        Err("položka v archivu nenalezena".to_owned())
    }
}

/// Removes temporary extractions left by earlier runs of this program.
pub fn cleanup_temp() {
    let Ok(entries) = fs::read_dir(temp_root()) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        // Directories are named "<pid>-<n>"; keep those of instances that are still running.
        let pid = name.split('-').next().unwrap_or_default();
        if pid.is_empty() || !Path::new("/proc").join(pid).exists() {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PackFormat {
    Zip,
    TarGz,
    TarXz,
    TarZst,
    TarBz2,
    Tar,
    SevenZip,
}

impl PackFormat {
    pub const ALL: [PackFormat; 7] = [
        PackFormat::Zip,
        PackFormat::TarGz,
        PackFormat::TarXz,
        PackFormat::TarZst,
        PackFormat::TarBz2,
        PackFormat::Tar,
        PackFormat::SevenZip,
    ];

    pub fn extension(self) -> &'static str {
        match self {
            PackFormat::Zip => ".zip",
            PackFormat::TarGz => ".tar.gz",
            PackFormat::TarXz => ".tar.xz",
            PackFormat::TarZst => ".tar.zst",
            PackFormat::TarBz2 => ".tar.bz2",
            PackFormat::Tar => ".tar",
            PackFormat::SevenZip => ".7z",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            PackFormat::Zip => "ZIP (.zip)",
            PackFormat::TarGz => "tar + gzip (.tar.gz)",
            PackFormat::TarXz => "tar + xz (.tar.xz)",
            PackFormat::TarZst => "tar + zstd (.tar.zst)",
            PackFormat::TarBz2 => "tar + bzip2 (.tar.bz2)",
            PackFormat::Tar => "tar bez komprese (.tar)",
            PackFormat::SevenZip => "7-Zip (.7z)",
        }
    }

    fn write_format(self) -> WriteFormat {
        let (format, filters) = match self {
            PackFormat::Zip => (FORMAT_ZIP, vec![]),
            PackFormat::TarGz => (FORMAT_TAR_PAX_RESTRICTED, vec![FILTER_GZIP]),
            PackFormat::TarXz => (FORMAT_TAR_PAX_RESTRICTED, vec![FILTER_XZ]),
            PackFormat::TarZst => (FORMAT_TAR_PAX_RESTRICTED, vec![FILTER_ZSTD]),
            PackFormat::TarBz2 => (FORMAT_TAR_PAX_RESTRICTED, vec![FILTER_BZIP2]),
            PackFormat::Tar => (FORMAT_TAR_PAX_RESTRICTED, vec![]),
            PackFormat::SevenZip => (FORMAT_7ZIP, vec![]),
        };
        WriteFormat { format, filters }
    }
}

fn to_time_parts(time: SystemTime) -> (libc::time_t, c_long) {
    match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => (duration.as_secs() as libc::time_t, duration.subsec_nanos() as c_long),
        Err(error) => (-(error.duration().as_secs() as i64) as libc::time_t, 0),
    }
}

/// Writes `source` (recursively, without following symlinks) as member `inner`.
fn add_tree(
    writer: &mut Writer,
    source: &Path,
    inner: &Path,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(&Path),
    result: &mut JobResult,
    buffer: &mut [u8],
) -> Result<(), String> {
    if cancel.load(Ordering::Relaxed) {
        result.cancelled = true;
        return Ok(());
    }
    let metadata = match fs::symlink_metadata(source) {
        Ok(metadata) => metadata,
        Err(error) => {
            result.fail(source, None, error.to_string());
            return Ok(());
        }
    };
    let file_type = metadata.file_type();
    let kind = if file_type.is_dir() {
        AE_IFDIR
    } else if file_type.is_symlink() {
        AE_IFLNK
    } else if file_type.is_file() {
        AE_IFREG
    } else {
        result.fail(source, None, "nepodporovaný typ souboru");
        return Ok(());
    };
    // Open the file before writing the header so an unreadable file is skipped cleanly.
    let mut input = if kind == AE_IFREG {
        match File::open(source) {
            Ok(file) => Some(file),
            Err(error) => {
                result.fail(source, None, error.to_string());
                return Ok(());
            }
        }
    } else {
        None
    };
    progress(source);
    let entry = NewEntry::new();
    let name = c_string(inner.as_os_str().as_bytes())?;
    let (seconds, nanos) = to_time_parts(metadata.modified().unwrap_or(UNIX_EPOCH));
    let owner = sys::user_name(metadata.uid()).and_then(|name| CString::new(name).ok());
    let group = sys::group_name(metadata.gid()).and_then(|name| CString::new(name).ok());
    let link = if kind == AE_IFLNK {
        Some(c_string(fs::read_link(source).map_err(|error| error.to_string())?.as_os_str().as_bytes())?)
    } else {
        None
    };
    unsafe {
        archive_entry_set_pathname(entry.0, name.as_ptr());
        archive_entry_set_filetype(entry.0, kind);
        archive_entry_set_perm(entry.0, metadata.mode() & 0o7777);
        archive_entry_set_mtime(entry.0, seconds, nanos);
        archive_entry_set_uid(entry.0, i64::from(metadata.uid()));
        archive_entry_set_gid(entry.0, i64::from(metadata.gid()));
        if let Some(owner) = &owner {
            archive_entry_set_uname(entry.0, owner.as_ptr());
        }
        if let Some(group) = &group {
            archive_entry_set_gname(entry.0, group.as_ptr());
        }
        archive_entry_set_size(entry.0, if kind == AE_IFREG { metadata.len() as i64 } else { 0 });
        if let Some(link) = &link {
            archive_entry_set_symlink(entry.0, link.as_ptr());
        }
    }
    writer.header(entry.0)?;
    if let Some(input) = input.as_mut() {
        // Write exactly the announced size: tar and zip headers already contain it.
        let mut remaining = metadata.len();
        while remaining > 0 {
            if cancel.load(Ordering::Relaxed) {
                result.cancelled = true;
                return Ok(());
            }
            let wanted = buffer.len().min(remaining as usize);
            let read = input.read(&mut buffer[..wanted]).map_err(|error| {
                format!("{}: {error}", source.to_string_lossy())
            })?;
            if read == 0 {
                return Err(format!("{}: soubor se během čtení zkrátil", source.to_string_lossy()));
            }
            writer.data(&buffer[..read])?;
            remaining -= read as u64;
        }
    }
    writer.finish_entry()?;
    result.done += 1;

    if kind == AE_IFDIR {
        let mut children: Vec<OsString> = match fs::read_dir(source) {
            Ok(entries) => entries.filter_map(|entry| entry.ok().map(|entry| entry.file_name())).collect(),
            Err(error) => {
                result.fail(source, None, error.to_string());
                return Ok(());
            }
        };
        children.sort();
        for child in children {
            add_tree(writer, &source.join(&child), &inner.join(&child), cancel, progress, result, buffer)?;
            if result.cancelled {
                return Ok(());
            }
        }
    }
    Ok(())
}

fn temporary_sibling(path: &Path) -> PathBuf {
    let name = path.file_name().unwrap_or(OsStr::new("archive"));
    let mut temporary = OsString::from(".");
    temporary.push(name);
    temporary.push(format!(".salamander-{}.tmp", std::process::id()));
    path.with_file_name(temporary)
}

/// Finishes writing into `temporary` and moves it over `target`, or cleans up.
fn commit(writer: Writer, temporary: &Path, target: &Path, mode: Option<u32>, result: &mut JobResult) {
    if result.cancelled {
        drop(writer);
        let _ = fs::remove_file(temporary);
        return;
    }
    if let Err(error) = writer.close() {
        result.fail(target, None, error);
        let _ = fs::remove_file(temporary);
        return;
    }
    if let Some(mode) = mode {
        let _ = fs::set_permissions(temporary, fs::Permissions::from_mode(mode & 0o7777));
    }
    if let Err(error) = fs::rename(temporary, target) {
        result.fail(target, None, error.to_string());
        let _ = fs::remove_file(temporary);
    }
}

/// Creates a new archive from `sources`, each stored under its own file name.
pub fn create(
    target: &Path,
    format: PackFormat,
    sources: &[PathBuf],
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(&Path),
) -> JobResult {
    let mut result = JobResult::default();
    if target.exists() {
        result.fail(target, None, "archiv již existuje");
        return result;
    }
    let temporary = temporary_sibling(target);
    let mut writer = match Writer::create(&temporary, &format.write_format()) {
        Ok(writer) => writer,
        Err(error) => {
            result.fail(target, None, error);
            let _ = fs::remove_file(&temporary);
            return result;
        }
    };
    let mut buffer = vec![0u8; 256 * 1024];
    for source in sources {
        let Some(name) = source.file_name() else {
            result.fail(source, None, "zdroj nemá jméno");
            continue;
        };
        if let Err(error) = add_tree(&mut writer, source, Path::new(name), cancel, progress, &mut result, &mut buffer) {
            result.fail(source, Some(target.to_path_buf()), error);
            drop(writer);
            let _ = fs::remove_file(&temporary);
            return result;
        }
        if result.cancelled {
            break;
        }
    }
    commit(writer, &temporary, target, None, &mut result);
    result
}

/// Rewrites `archive` without the `remove` members and with `add` (filesystem paths)
/// stored inside directory `add_dir`. Added names replace existing members.
pub fn modify(
    archive: &Path,
    remove: &[PathBuf],
    add: &[PathBuf],
    add_dir: &Path,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(&Path),
) -> JobResult {
    let mut result = JobResult::default();
    let format = match index(archive) {
        Ok(index) => match index.write_format.clone() {
            Some(format) => format,
            None => {
                result.fail(archive, None, format!("formát {} nelze upravovat", index.format));
                return result;
            }
        },
        Err(error) => {
            result.fail(archive, None, error);
            return result;
        }
    };
    let replaced: Vec<PathBuf> = add
        .iter()
        .filter_map(|source| source.file_name().map(|name| add_dir.join(name)))
        .collect();
    let original_mode = fs::metadata(archive).ok().map(|metadata| metadata.mode());
    let temporary = temporary_sibling(archive);
    let mut reader = match Reader::open(archive) {
        Ok(reader) => reader,
        Err(error) => {
            result.fail(archive, None, error);
            return result;
        }
    };
    let mut writer = match Writer::create(&temporary, &format) {
        Ok(writer) => writer,
        Err(error) => {
            result.fail(archive, None, error);
            let _ = fs::remove_file(&temporary);
            return result;
        }
    };
    let mut buffer = vec![0u8; 256 * 1024];
    let abort = |writer: Writer, result: &mut JobResult, error: String| {
        result.fail(archive, None, error);
        drop(writer);
        let _ = fs::remove_file(&temporary);
    };

    loop {
        if cancel.load(Ordering::Relaxed) {
            result.cancelled = true;
            break;
        }
        let entry = match reader.next() {
            Ok(Some(entry)) => entry,
            Ok(None) => break,
            Err(error) => {
                abort(writer, &mut result, error);
                return result;
            }
        };
        let info = entry_info(entry);
        let dropped = remove.iter().chain(&replaced).any(|item| info.path == *item || info.path.starts_with(item));
        if dropped {
            continue;
        }
        progress(&info.path);
        if let Err(error) = writer.header(entry) {
            abort(writer, &mut result, error);
            return result;
        }
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => {
                    if let Err(error) = writer.data(&buffer[..read]) {
                        abort(writer, &mut result, error);
                        return result;
                    }
                }
                Err(error) => {
                    abort(writer, &mut result, error);
                    return result;
                }
            }
        }
        if let Err(error) = writer.finish_entry() {
            abort(writer, &mut result, error);
            return result;
        }
    }
    drop(reader);

    for source in add {
        if result.cancelled {
            break;
        }
        let Some(name) = source.file_name() else {
            continue;
        };
        if let Err(error) = add_tree(&mut writer, source, &add_dir.join(name), cancel, progress, &mut result, &mut buffer) {
            abort(writer, &mut result, error);
            return result;
        }
    }
    commit(writer, &temporary, archive, original_mode, &mut result);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(root: &Path) {
        fs::create_dir_all(root.join("dir/sub")).unwrap();
        fs::write(root.join("dir/a.txt"), "alpha").unwrap();
        fs::write(root.join("dir/sub/b.txt"), "beta").unwrap();
        std::os::unix::fs::symlink("a.txt", root.join("dir/link")).unwrap();
        fs::set_permissions(root.join("dir/a.txt"), fs::Permissions::from_mode(0o640)).unwrap();
        sys::set_times(&root.join("dir/a.txt"), None, Some(UNIX_EPOCH + Duration::from_secs(1_000_000_000))).unwrap();
    }

    fn pack(temp: &Path, format: PackFormat) -> PathBuf {
        tree(&temp.join("src"));
        let archive = temp.join(format!("test{}", format.extension()));
        let result = create(&archive, format, &[temp.join("src/dir")], &AtomicBool::new(false), &mut |_| {});
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        archive
    }

    #[test]
    fn every_pack_format_round_trips() {
        for format in PackFormat::ALL {
            let temp = tempfile::tempdir().unwrap();
            let archive = pack(temp.path(), format);
            let index = index(&archive).unwrap();
            assert!(index.is_writable(), "{format:?}");
            let names: Vec<String> = index
                .children(Path::new("dir"))
                .unwrap()
                .iter()
                .map(|entry| entry.path.to_string_lossy().into_owned())
                .collect();
            assert_eq!(names, ["dir/a.txt", "dir/link", "dir/sub"], "{format:?}");

            let dest = temp.path().join("out");
            fs::create_dir(&dest).unwrap();
            let result = extract(
                &ExtractRequest {
                    archive: &archive,
                    items: &[],
                    base: Path::new(""),
                    dest: &dest,
                    overwrite: false,
                },
                &AtomicBool::new(false),
                &mut |_| {},
            );
            assert!(result.errors.is_empty(), "{format:?}: {:?}", result.errors);
            assert_eq!(fs::read_to_string(dest.join("dir/sub/b.txt")).unwrap(), "beta");
            assert_eq!(fs::read_link(dest.join("dir/link")).unwrap(), Path::new("a.txt"));
            let metadata = fs::metadata(dest.join("dir/a.txt")).unwrap();
            assert_eq!(metadata.mtime(), 1_000_000_000, "{format:?}");
            if format != PackFormat::SevenZip {
                assert_eq!(metadata.mode() & 0o777, 0o640, "{format:?}");
            }
        }
    }

    #[test]
    fn extracts_subtree_relative_to_base_and_respects_overwrite() {
        let temp = tempfile::tempdir().unwrap();
        let archive = pack(temp.path(), PackFormat::TarGz);
        let dest = temp.path().join("out");
        fs::create_dir(&dest).unwrap();
        fs::write(dest.join("a.txt"), "keep").unwrap();
        let items = [PathBuf::from("dir/sub"), PathBuf::from("dir/a.txt")];
        let request = |overwrite| ExtractRequest {
            archive: &archive,
            items: &items,
            base: Path::new("dir"),
            dest: &dest,
            overwrite,
        };
        let result = extract(&request(false), &AtomicBool::new(false), &mut |_| {});
        assert_eq!(result.errors.len(), 1);
        assert_eq!(fs::read_to_string(dest.join("a.txt")).unwrap(), "keep");
        assert_eq!(fs::read_to_string(dest.join("sub/b.txt")).unwrap(), "beta");
        assert!(!dest.join("link").exists());

        let result = extract(&request(true), &AtomicBool::new(false), &mut |_| {});
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert_eq!(fs::read_to_string(dest.join("a.txt")).unwrap(), "alpha");
    }

    fn raw_archive(path: &Path, members: &[(&str, u32, &str)]) {
        let mut writer = Writer::create(path, &PackFormat::Tar.write_format()).unwrap();
        for (name, kind, content) in members {
            let entry = NewEntry::new();
            let name = CString::new(*name).unwrap();
            let link = CString::new(*content).unwrap();
            unsafe {
                archive_entry_set_pathname(entry.0, name.as_ptr());
                archive_entry_set_filetype(entry.0, *kind);
                archive_entry_set_perm(entry.0, 0o644);
                if *kind == AE_IFLNK {
                    archive_entry_set_symlink(entry.0, link.as_ptr());
                } else {
                    archive_entry_set_size(entry.0, content.len() as i64);
                }
            }
            writer.header(entry.0).unwrap();
            if *kind == AE_IFREG {
                writer.data(content.as_bytes()).unwrap();
            }
            writer.finish_entry().unwrap();
        }
        writer.close().unwrap();
    }

    #[test]
    fn refuses_parent_traversal_and_symlinked_parents() {
        let temp = tempfile::tempdir().unwrap();
        let archive = temp.path().join("evil.tar");
        let outside = temp.path().join("outside");
        fs::create_dir(&outside).unwrap();
        raw_archive(
            &archive,
            &[
                ("../escape.txt", AE_IFREG, "x"),
                ("link", AE_IFLNK, outside.to_str().unwrap()),
                ("link/planted.txt", AE_IFREG, "x"),
                ("ok.txt", AE_IFREG, "fine"),
            ],
        );
        let dest = temp.path().join("dest");
        fs::create_dir(&dest).unwrap();
        let result = extract(
            &ExtractRequest {
                archive: &archive,
                items: &[],
                base: Path::new(""),
                dest: &dest,
                overwrite: false,
            },
            &AtomicBool::new(false),
            &mut |_| {},
        );
        assert_eq!(result.errors.len(), 2, "{:?}", result.errors);
        assert!(!temp.path().join("escape.txt").exists());
        assert!(!outside.join("planted.txt").exists());
        assert_eq!(fs::read_to_string(dest.join("ok.txt")).unwrap(), "fine");
    }

    #[test]
    fn modify_adds_replaces_and_removes_members() {
        let temp = tempfile::tempdir().unwrap();
        let archive = pack(temp.path(), PackFormat::Zip);
        let new_file = temp.path().join("a.txt");
        fs::write(&new_file, "replaced").unwrap();
        let extra = temp.path().join("extra.txt");
        fs::write(&extra, "extra").unwrap();

        let result = modify(
            &archive,
            &[PathBuf::from("dir/sub")],
            &[new_file, extra],
            Path::new("dir"),
            &AtomicBool::new(false),
            &mut |_| {},
        );
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let index = index(&archive).unwrap();
        let names: Vec<String> = index
            .children(Path::new("dir"))
            .unwrap()
            .iter()
            .map(|entry| entry.path.to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["dir/a.txt", "dir/extra.txt", "dir/link"]);
        let path = extract_to_temp(&archive, Path::new("dir/a.txt")).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "replaced");
        assert!(!temporary_sibling(&archive).exists());
    }

    #[test]
    fn implicit_directories_are_listed() {
        let temp = tempfile::tempdir().unwrap();
        let archive = temp.path().join("implicit.tar");
        raw_archive(&archive, &[("a/b/c.txt", AE_IFREG, "c"), ("./top.txt", AE_IFREG, "t")]);
        let index = index(&archive).unwrap();
        let root: Vec<_> = index.children(Path::new("")).unwrap();
        assert_eq!(root.len(), 2);
        assert_eq!(root[0].path, Path::new("a"));
        assert_eq!(root[0].kind, EntryKind::Directory);
        assert_eq!(root[1].path, Path::new("top.txt"));
        assert_eq!(index.children(Path::new("a/b")).unwrap()[0].size, 1);
        assert!(index.children(Path::new("missing")).is_none());
        assert!(index.children(Path::new("top.txt")).is_none());
    }

    #[test]
    fn splits_virtual_paths() {
        let temp = tempfile::tempdir().unwrap();
        let archive = pack(temp.path(), PackFormat::Tar);
        assert_eq!(
            split_virtual(&archive.join("dir/sub")),
            Some((archive.clone(), PathBuf::from("dir/sub")))
        );
        assert_eq!(split_virtual(&archive), Some((archive.clone(), PathBuf::new())));
        assert_eq!(split_virtual(&temp.path().join("src/dir")), None);
        assert!(looks_like_archive(Path::new("x.TAR.GZ")));
        assert!(!looks_like_archive(Path::new("x.txt")));
    }
}
