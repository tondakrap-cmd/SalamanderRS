use std::cell::Cell;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::{DateTime, Local};
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
}

pub fn read_directory(path: &Path, show_hidden: bool) -> io::Result<DirectoryListing> {
    let mut entries = Vec::new();
    let mut warnings = Vec::new();

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
        entries.push(FileEntry {
            path: item_path,
            folded_name: name.to_string_lossy().to_lowercase(),
            name,
            kind,
            size: metadata.len(),
            modified: metadata.modified().ok(),
        });
    }

    Ok(DirectoryListing { entries, warnings })
}

pub fn format_size(entry: &FileEntry) -> String {
    match entry.kind {
        EntryKind::Directory => "<DIR>".to_owned(),
        EntryKind::Symlink => "<LINK>".to_owned(),
        EntryKind::Other => "<SPECIAL>".to_owned(),
        EntryKind::File => format_bytes(entry.size),
    }
}

pub fn format_bytes(size: u64) -> String {
    if size < 1024 {
        return format!("{size} B");
    }
    const UNITS: [&str; 4] = ["KiB", "MiB", "GiB", "TiB"];
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

pub fn format_modified(modified: Option<SystemTime>) -> String {
    modified.map_or_else(
        || "-".to_owned(),
        |time| {
            DateTime::<Local>::from(time)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        },
    )
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
            let object: Self = glib::Object::new();
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
        let all = read_directory(temp.path(), true).unwrap();
        assert!(all.entries.iter().any(|entry| entry.name == ".hidden"));
        assert!(all.entries.iter().any(|entry| entry.name == non_utf8));
        let link = all
            .entries
            .iter()
            .find(|entry| entry.name == "link")
            .unwrap();
        assert_eq!(link.kind, EntryKind::Symlink);
        assert_eq!(format_size(link), "<LINK>");
    }

    #[test]
    fn formats_sizes_and_missing_time() {
        assert_eq!(format_bytes(1023), "1023 B");
        assert_eq!(format_bytes(1536), "1.5 KiB");
        assert_eq!(format_bytes(1024 * 1024), "1.0 MiB");
        assert_eq!(format_modified(None), "-");
    }
}
