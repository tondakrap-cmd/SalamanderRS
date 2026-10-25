use std::cell::{Cell, RefCell};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::env;
use std::ffi::OsString;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use gtk::gio;
use gtk::glib;
use gtk::prelude::*;

use crate::archive;
use crate::model::{
    format_bytes, format_modified, read_archive_directory, read_directory, wildcard_match,
    CompareItem, DirectoryListing, EntryKind, EntryObject, FileEntry, Mask,
};
use crate::ops;
use crate::sys::DiskSpace;

const HISTORY_LIMIT: usize = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SortKey {
    Name,
    Extension,
    Time,
    Size,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OptionalColumn {
    Extension,
    Permissions,
    Owner,
}

/// What a panel shows: a real directory, or a directory inside an archive file.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Location {
    Dir(PathBuf),
    Archive { archive: PathBuf, inner: PathBuf },
}

impl Location {
    /// Path shown in the path entry and history; inside archives it is virtual
    /// (`/data/a.zip/dir`).
    pub fn display(&self) -> PathBuf {
        match self {
            Location::Dir(dir) => dir.clone(),
            Location::Archive { archive, inner } if inner.as_os_str().is_empty() => archive.clone(),
            Location::Archive { archive, inner } => archive.join(inner),
        }
    }

    /// The real directory: the archive's parent when inside an archive.
    pub fn fs_dir(&self) -> PathBuf {
        match self {
            Location::Dir(dir) => dir.clone(),
            Location::Archive { archive, .. } => archive
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("/")),
        }
    }
}

/// Drops `.` and resolves `..` lexically inside an archive.
fn normalize_inner(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::Normal(part) => normalized.push(part),
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            _ => {}
        }
    }
    normalized
}

struct PanelState {
    location: Location,
    /// Target of the load in flight, so repeated Backspace climbs from where it is going.
    loading: Option<Location>,
    note: Option<String>,
    show_hidden: bool,
    generation: Cell<u64>,
    pending_cursor: Option<(PathBuf, PathBuf)>,
    history: Vec<PathBuf>,
    history_pos: usize,
    pending_history: Option<usize>,
    quick_search: String,
    monitor: Option<gio::FileMonitor>,
    monitored_dir: Option<PathBuf>,
    refresh_pending: bool,
    disk_space: Option<DiskSpace>,
}

struct Columns {
    name: gtk::ColumnViewColumn,
    extension: gtk::ColumnViewColumn,
    size: gtk::ColumnViewColumn,
    modified: gtk::ColumnViewColumn,
    permissions: gtk::ColumnViewColumn,
    owner: gtk::ColumnViewColumn,
}

#[derive(Clone)]
pub struct Panel {
    pub root: gtk::Box,
    pub path_entry: gtk::Entry,
    view: gtk::ColumnView,
    store: gio::ListStore,
    sorted: gtk::SortListModel,
    selection: gtk::SingleSelection,
    info_label: gtk::Label,
    status_label: gtk::Label,
    columns: Rc<Columns>,
    state: Rc<RefCell<PanelState>>,
}

struct WeakPanel {
    root: glib::WeakRef<gtk::Box>,
    path_entry: glib::WeakRef<gtk::Entry>,
    view: glib::WeakRef<gtk::ColumnView>,
    store: glib::WeakRef<gio::ListStore>,
    sorted: glib::WeakRef<gtk::SortListModel>,
    selection: glib::WeakRef<gtk::SingleSelection>,
    info_label: glib::WeakRef<gtk::Label>,
    status_label: glib::WeakRef<gtk::Label>,
    columns: Weak<Columns>,
    state: Weak<RefCell<PanelState>>,
}

impl WeakPanel {
    fn upgrade(&self) -> Option<Panel> {
        Some(Panel {
            root: self.root.upgrade()?,
            path_entry: self.path_entry.upgrade()?,
            view: self.view.upgrade()?,
            store: self.store.upgrade()?,
            sorted: self.sorted.upgrade()?,
            selection: self.selection.upgrade()?,
            info_label: self.info_label.upgrade()?,
            status_label: self.status_label.upgrade()?,
            columns: self.columns.upgrade()?,
            state: self.state.upgrade()?,
        })
    }
}

#[derive(Clone, Copy)]
enum CellKind {
    Marked,
    Name,
    Extension,
    Size,
    Modified,
    Permissions,
    Owner,
}

/// Sizes and counts shown in the status line.
#[derive(Default)]
struct Totals {
    files: u64,
    dirs: u64,
    bytes: u64,
    marked_files: u64,
    marked_dirs: u64,
    marked_bytes: u64,
}

impl Panel {
    pub fn new(start_dir: &Path, show_hidden: bool) -> Panel {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 4);
        root.set_margin_top(6);
        root.set_margin_bottom(4);
        root.set_margin_start(6);
        root.set_margin_end(6);

        let path_entry = gtk::Entry::new();
        path_entry.set_text(&start_dir.to_string_lossy());
        path_entry.set_tooltip_text(Some("Cesta k adresáři (Ctrl+L)"));
        root.append(&path_entry);

        let store = gio::ListStore::new::<EntryObject>();
        let sorted = gtk::SortListModel::new(Some(store.clone()), None::<gtk::Sorter>);
        let section_sorter = gtk::CustomSorter::new(|left, right| {
            let rank = |object: &glib::Object| {
                let entry = object.downcast_ref::<EntryObject>().expect("entry object");
                if entry.with_entry(FileEntry::is_dir_like) {
                    0
                } else {
                    1
                }
            };
            rank(left).cmp(&rank(right)).into()
        });
        sorted.set_section_sorter(Some(&section_sorter));

        let selection = gtk::SingleSelection::new(Some(sorted.clone()));
        selection.set_autoselect(true);
        selection.set_can_unselect(false);

        let view = gtk::ColumnView::new(Some(selection.clone()));
        view.set_vexpand(true);
        view.set_hexpand(true);
        view.add_css_class("file-list");

        let marked_column =
            gtk::ColumnViewColumn::new(None, Some(make_factory(CellKind::Marked, &store)));
        marked_column.set_fixed_width(24);
        view.append_column(&marked_column);

        let column = |title: &str, kind: CellKind, sorter: gtk::CustomSorter| {
            let column = gtk::ColumnViewColumn::new(Some(title), Some(make_factory(kind, &store)));
            column.set_resizable(true);
            column.set_sorter(Some(&sorter));
            view.append_column(&column);
            column
        };
        let name = column(
            "Název",
            CellKind::Name,
            entry_sorter(|left, _, right, _| compare_name_entries(left, right)),
        );
        name.set_expand(true);
        let extension = column(
            "Přípona",
            CellKind::Extension,
            entry_sorter(|left, _, right, _| {
                left.extension()
                    .cmp(right.extension())
                    .then_with(|| compare_name_entries(left, right))
            }),
        );
        let size = column(
            "Velikost",
            CellKind::Size,
            entry_sorter(|left, left_object, right, right_object| {
                left_object
                    .effective_size()
                    .cmp(&right_object.effective_size())
                    .then_with(|| compare_name_entries(left, right))
            }),
        );
        let modified = column(
            "Změněno",
            CellKind::Modified,
            entry_sorter(|left, _, right, _| {
                left.modified
                    .cmp(&right.modified)
                    .then_with(|| compare_name_entries(left, right))
            }),
        );
        let permissions = column(
            "Práva",
            CellKind::Permissions,
            entry_sorter(|left, _, right, _| {
                (left.mode & 0o7777)
                    .cmp(&(right.mode & 0o7777))
                    .then_with(|| compare_name_entries(left, right))
            }),
        );
        let owner = column(
            "Vlastník",
            CellKind::Owner,
            entry_sorter(|left, _, right, _| {
                (&left.owner, &left.group)
                    .cmp(&(&right.owner, &right.group))
                    .then_with(|| compare_name_entries(left, right))
            }),
        );
        extension.set_visible(false);
        // Fixed widths for the narrow columns so the name column receives the remaining space;
        // all of them stay user-resizable.
        for (column, width) in [
            (&extension, 70),
            (&size, 95),
            (&modified, 135),
            (&permissions, 100),
            (&owner, 140),
        ] {
            column.set_fixed_width(width);
        }

        let scrolled = gtk::ScrolledWindow::new();
        scrolled.set_vexpand(true);
        scrolled.set_hexpand(true);
        scrolled.set_child(Some(&view));
        root.append(&scrolled);

        let info_label = gtk::Label::new(None);
        info_label.set_xalign(0.0);
        info_label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        info_label.add_css_class("panel-info");
        root.append(&info_label);
        let status_label = gtk::Label::new(None);
        status_label.set_xalign(0.0);
        status_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        status_label.add_css_class("dim-label");
        status_label.add_css_class("panel-status");
        root.append(&status_label);

        let panel = Panel {
            root,
            path_entry,
            view,
            store,
            sorted,
            selection,
            info_label,
            status_label,
            columns: Rc::new(Columns {
                name,
                extension,
                size,
                modified,
                permissions,
                owner,
            }),
            state: Rc::new(RefCell::new(PanelState {
                location: Location::Dir(start_dir.to_path_buf()),
                loading: None,
                note: None,
                show_hidden,
                generation: Cell::new(0),
                pending_cursor: None,
                history: Vec::new(),
                history_pos: 0,
                pending_history: None,
                quick_search: String::new(),
                monitor: None,
                monitored_dir: None,
                refresh_pending: false,
                disk_space: None,
            })),
        };

        let weak = panel.downgrade();
        panel.path_entry.connect_activate(move |_| {
            if let Some(panel) = weak.upgrade() {
                let requested = PathBuf::from(panel.path_entry.text());
                panel.navigate(&requested);
                panel.view.grab_focus();
            }
        });

        let weak = panel.downgrade();
        panel.selection.connect_selected_item_notify(move |_| {
            if let Some(panel) = weak.upgrade() {
                panel.update_info();
            }
        });
        let weak = panel.downgrade();
        panel.store.connect_items_changed(move |_, _, _, _| {
            if let Some(panel) = weak.upgrade() {
                panel.update_status();
            }
        });

        let view_sorter = panel.view.sorter();
        panel.sorted.set_sorter(view_sorter.as_ref());
        panel
            .view
            .sort_by_column(Some(&panel.columns.name), gtk::SortType::Ascending);

        panel.load(Location::Dir(start_dir.to_path_buf()), show_hidden, false);
        panel
    }

    pub fn view(&self) -> &gtk::ColumnView {
        &self.view
    }

    /// The real directory shown, or the directory containing the open archive.
    pub fn dir(&self) -> PathBuf {
        self.state.borrow().location.fs_dir()
    }

    pub fn location(&self) -> Location {
        self.state.borrow().location.clone()
    }

    /// `(archive file, inner directory)` when the panel shows an archive.
    pub fn archive(&self) -> Option<(PathBuf, PathBuf)> {
        match &self.state.borrow().location {
            Location::Archive { archive, inner } => Some((archive.clone(), inner.clone())),
            Location::Dir(_) => None,
        }
    }

    /// Inner paths of the selected entries when the panel shows an archive.
    pub fn archive_selection(&self) -> Vec<PathBuf> {
        let Some((archive, _)) = self.archive() else {
            return Vec::new();
        };
        self.selected_or_cursor()
            .into_iter()
            .filter_map(|path| path.strip_prefix(&archive).ok().map(Path::to_path_buf))
            .collect()
    }

    pub fn show_hidden(&self) -> bool {
        self.state.borrow().show_hidden
    }

    pub fn history(&self) -> Vec<PathBuf> {
        self.state.borrow().history.clone()
    }

    pub fn resolve_path(&self, requested: &Path) -> PathBuf {
        let expanded = expand_tilde(requested);
        if expanded.is_absolute() {
            expanded
        } else {
            self.state.borrow().location.display().join(expanded)
        }
    }

    pub fn set_cursor_position(&self, position: u32) {
        if position < self.sorted.n_items() {
            self.selection.set_selected(position);
        }
    }

    pub fn toggle_hidden(&self) {
        {
            let mut state = self.state.borrow_mut();
            state.show_hidden = !state.show_hidden;
        }
        self.reload();
    }

    pub fn set_column_visible(&self, column: OptionalColumn, visible: bool) {
        let column = match column {
            OptionalColumn::Extension => &self.columns.extension,
            OptionalColumn::Permissions => &self.columns.permissions,
            OptionalColumn::Owner => &self.columns.owner,
        };
        column.set_visible(visible);
    }

    /// Sorts by the given key; repeating the same key reverses the order.
    pub fn sort_by(&self, key: SortKey) {
        let column = match key {
            SortKey::Name => &self.columns.name,
            SortKey::Extension => &self.columns.extension,
            SortKey::Time => &self.columns.modified,
            SortKey::Size => &self.columns.size,
        };
        let order = self
            .view
            .sorter()
            .and_downcast::<gtk::ColumnViewSorter>()
            .filter(|sorter| sorter.primary_sort_column().as_ref() == Some(column))
            .map(|sorter| match sorter.primary_sort_order() {
                gtk::SortType::Ascending => gtk::SortType::Descending,
                _ => gtk::SortType::Ascending,
            })
            .unwrap_or(gtk::SortType::Ascending);
        let cursor = self.cursor_path();
        self.view.sort_by_column(Some(column), order);
        if let Some(position) = cursor.and_then(|path| self.find_sorted_path(&path)) {
            self.move_cursor(position);
        }
    }

    pub fn navigate_parent(&self) {
        // Inside an archive the parent of the archive root is the directory containing the
        // archive file, with the cursor on it — the same as for ordinary directories.
        let current = {
            let state = self.state.borrow();
            state.loading.as_ref().unwrap_or(&state.location).display()
        };
        if let Some(parent) = current.parent() {
            self.navigate_and_focus(parent, Some(&current));
        }
    }

    pub fn navigate_home(&self) {
        if let Some(home) = env::var_os("HOME") {
            self.navigate(Path::new(&home));
        }
    }

    pub fn history_back(&self) {
        self.history_step(-1);
    }

    pub fn history_forward(&self) {
        self.history_step(1);
    }

    fn history_step(&self, delta: isize) {
        let target = {
            let state = self.state.borrow();
            let position = state.history_pos as isize + delta;
            if position < 0 || position as usize >= state.history.len() {
                return;
            }
            (position as usize, state.history[position as usize].clone())
        };
        let (position, dir) = target;
        // When stepping back, focus the directory we came from (like Backspace does).
        let focus = (delta < 0)
            .then(|| self.state.borrow().history.get(position + 1).cloned())
            .flatten()
            .filter(|next| next.parent() == Some(dir.as_path()));
        self.state.borrow_mut().pending_history = Some(position);
        self.navigate_and_focus(&dir, focus.as_deref());
    }

    pub fn cursor_path(&self) -> Option<PathBuf> {
        self.cursor_entry()
            .map(|entry| entry.with_entry(|entry| entry.path.clone()))
    }

    pub fn cursor_entry(&self) -> Option<EntryObject> {
        self.selection
            .selected_item()
            .and_then(|item| item.downcast::<EntryObject>().ok())
    }

    pub fn selected_or_cursor(&self) -> Vec<PathBuf> {
        self.selected_entries()
            .iter()
            .map(|entry| entry.with_entry(|entry| entry.path.clone()))
            .collect()
    }

    pub fn selected_entries(&self) -> Vec<EntryObject> {
        let marked: Vec<EntryObject> = self
            .store
            .iter::<EntryObject>()
            .flatten()
            .filter(EntryObject::is_marked)
            .collect();
        if marked.is_empty() {
            self.cursor_entry().into_iter().collect()
        } else {
            marked
        }
    }

    pub fn reload(&self) {
        let (location, show_hidden) = {
            let state = self.state.borrow();
            (state.location.clone(), state.show_hidden)
        };
        self.load(location, show_hidden, false);
    }

    pub fn reload_and_focus(&self, path: &Path) {
        {
            let mut state = self.state.borrow_mut();
            state.pending_cursor = Some((state.location.display(), path.to_path_buf()));
        }
        self.reload();
    }

    pub fn navigate(&self, requested: &Path) {
        self.navigate_and_focus(requested, None);
    }

    /// Opens `requested` and places the cursor on `focus` (a path inside it) when present.
    pub fn navigate_and_focus(&self, requested: &Path, focus: Option<&Path>) {
        self.state.borrow_mut().pending_cursor = None;
        self.quick_search_end();

        let candidate = self.resolve_path(requested);
        let location = match fs::canonicalize(&candidate) {
            Ok(path) if path.is_dir() => Location::Dir(path),
            Ok(path) if path.is_file() && archive::looks_like_archive(&path) => Location::Archive {
                archive: path,
                inner: PathBuf::new(),
            },
            Ok(path) => {
                self.fail_navigation(&format!(
                    "Nelze číst adresář {}: není adresář",
                    path.to_string_lossy()
                ));
                return;
            }
            Err(error) => match archive::split_virtual(&candidate) {
                // A path inside an archive, e.g. typed into the path entry or from history.
                Some((file, inner)) => Location::Archive {
                    archive: fs::canonicalize(&file).unwrap_or(file),
                    inner: normalize_inner(&inner),
                },
                None => {
                    self.fail_navigation(&format!(
                        "Nelze číst adresář {}: {error}",
                        candidate.to_string_lossy()
                    ));
                    return;
                }
            },
        };

        if let Some(name) = focus.and_then(Path::file_name) {
            let display = location.display();
            self.state.borrow_mut().pending_cursor = Some((display.clone(), display.join(name)));
        }
        let show_hidden = self.state.borrow().show_hidden;
        self.load(location, show_hidden, false);
    }

    fn fail_navigation(&self, message: &str) {
        self.state.borrow_mut().pending_history = None;
        self.invalidate_loads();
        self.restore_path_entry();
        self.show_message(message);
    }

    pub fn clear_marks(&self) {
        self.set_marks(|_| false);
    }

    pub(crate) fn mark_all(&self) {
        self.set_marks(|_| true);
    }

    pub fn invert_marks(&self) {
        self.set_marks(|entry| !entry.is_marked());
    }

    /// Marks (or unmarks) entries matching the mask; directories only when `include_dirs`.
    pub fn mark_matching(&self, mask: &Mask, include_dirs: bool, mark: bool) {
        self.set_marks(|entry| {
            let matches = entry.with_entry(|item| {
                (include_dirs || !item.is_dir_like()) && mask.matches(&item.name.to_string_lossy())
            });
            if matches {
                mark
            } else {
                entry.is_marked()
            }
        });
    }

    /// Marks exactly the entries whose names are in `names`.
    pub fn mark_names(&self, names: &HashSet<OsString>) {
        self.set_marks(|entry| entry.with_entry(|item| names.contains(&item.name)));
    }

    fn set_marks(&self, decide: impl Fn(&EntryObject) -> bool) {
        for entry in self.store.iter::<EntryObject>().flatten() {
            let marked = decide(&entry);
            if marked != entry.is_marked() {
                entry.set_marked(marked);
            }
        }
        self.update_status();
    }

    /// Regular files of the listing, used by Compare directories.
    pub fn compare_items(&self) -> Vec<CompareItem> {
        self.store
            .iter::<EntryObject>()
            .flatten()
            .filter_map(|entry| {
                entry.with_entry(|item| {
                    (item.kind == EntryKind::File).then(|| CompareItem {
                        name: item.name.clone(),
                        size: item.size,
                        modified: item.modified,
                    })
                })
            })
            .collect()
    }

    pub(crate) fn toggle_mark_and_advance(&self) {
        self.mark_and_move(1);
    }

    /// Toggles the mark under the cursor and moves the cursor by `delta` rows.
    pub fn mark_and_move(&self, delta: i32) {
        let Some(entry) = self.cursor_entry() else {
            return;
        };
        entry.set_marked(!entry.is_marked());
        self.update_status();
        let position = self.selection.selected();
        if position == gtk::INVALID_LIST_POSITION {
            return;
        }
        let next = position as i64 + i64::from(delta);
        if next >= 0 && (next as u32) < self.sorted.n_items() {
            self.move_cursor(next as u32);
        }
    }

    /// Calculates recursive sizes of the given directories in the background, shows them in
    /// the Size column and reports the total through `on_done`.
    pub fn calculate_sizes(
        &self,
        entries: Vec<EntryObject>,
        on_done: impl FnOnce(ops::Measure) + 'static,
    ) {
        let paths: Vec<(PathBuf, bool)> = entries
            .iter()
            .map(|entry| entry.with_entry(|item| (item.path.clone(), item.kind == EntryKind::Directory)))
            .collect();
        self.status_label.set_text("Počítám obsazené místo…");
        let (sender, receiver) = async_channel::unbounded();
        std::thread::spawn(move || {
            let cancel = AtomicBool::new(false);
            for (path, is_dir) in paths {
                let measured = if is_dir {
                    ops::measure(&path, &cancel)
                } else {
                    // Files and symlinks: their own size without following links.
                    let mut single = ops::Measure::default();
                    if let Ok(metadata) = fs::symlink_metadata(&path) {
                        single.files = 1;
                        if metadata.is_file() {
                            single.bytes = metadata.len();
                        }
                    }
                    single
                };
                if sender.send_blocking((path, is_dir, measured)).is_err() {
                    return;
                }
            }
        });

        let weak = self.downgrade();
        glib::spawn_future_local(async move {
            let mut total = ops::Measure::default();
            while let Ok((path, is_dir, measured)) = receiver.recv().await {
                total.add(measured);
                if is_dir {
                    total.dirs += 1;
                }
                if let Some(panel) = weak.upgrade() {
                    if is_dir {
                        if let Some(entry) = panel.find_entry(&path) {
                            entry.set_dir_size(measured.bytes as i64);
                        }
                    }
                }
            }
            if let Some(panel) = weak.upgrade() {
                panel.update_status();
            }
            on_done(total);
        });
    }

    fn find_entry(&self, path: &Path) -> Option<EntryObject> {
        self.store
            .iter::<EntryObject>()
            .flatten()
            .find(|entry| entry.with_entry(|item| item.path == path))
    }

    // ---- Quick search (typing a name jumps to it) ----

    pub fn quick_search_active(&self) -> bool {
        !self.state.borrow().quick_search.is_empty()
    }

    /// Extends the quick-search text; returns false (and keeps the old text) when no entry
    /// matches the extended text.
    pub fn quick_search_push(&self, ch: char) -> bool {
        let mut query = self.state.borrow().quick_search.clone();
        query.push(ch);
        match self.find_quick_match(&query, 0, true) {
            Some(position) => {
                self.state.borrow_mut().quick_search = query;
                self.move_cursor(position);
                self.update_info();
                true
            }
            None => {
                self.view.error_bell();
                false
            }
        }
    }

    pub fn quick_search_pop(&self) {
        let query = {
            let mut state = self.state.borrow_mut();
            state.quick_search.pop();
            state.quick_search.clone()
        };
        if !query.is_empty() {
            if let Some(position) = self.find_quick_match(&query, 0, true) {
                self.move_cursor(position);
            }
        }
        self.update_info();
    }

    pub fn quick_search_next(&self, forward: bool) {
        let query = self.state.borrow().quick_search.clone();
        let current = self.selection.selected();
        if query.is_empty() || current == gtk::INVALID_LIST_POSITION {
            return;
        }
        let start = if forward {
            current + 1
        } else if current == 0 {
            return;
        } else {
            current - 1
        };
        if let Some(position) = self.find_quick_match(&query, start, forward) {
            self.move_cursor(position);
        }
    }

    pub fn quick_search_end(&self) {
        let was_active = !std::mem::take(&mut self.state.borrow_mut().quick_search).is_empty();
        if was_active {
            self.update_info();
        }
    }

    fn find_quick_match(&self, query: &str, start: u32, forward: bool) -> Option<u32> {
        let pattern = format!("{}*", query.to_lowercase());
        let wildcard = query.contains(['*', '?']);
        let matches = |position: u32| {
            self.sorted
                .item(position)
                .and_downcast::<EntryObject>()
                .is_some_and(|entry| {
                    entry.with_entry(|item| {
                        if wildcard {
                            wildcard_match(&pattern, &item.folded_name)
                        } else {
                            item.folded_name.starts_with(&pattern[..pattern.len() - 1])
                        }
                    })
                })
        };
        let count = self.sorted.n_items();
        if forward {
            (start..count).find(|&position| matches(position))
        } else {
            (0..=start.min(count.saturating_sub(1)))
                .rev()
                .find(|&position| matches(position))
        }
    }

    // ---- Cursor and focus ----

    fn has_focus(&self) -> bool {
        let view: &gtk::Widget = self.view.upcast_ref();
        self.view
            .root()
            .and_then(|root| root.focus())
            .is_some_and(|focus| &focus == view || focus.is_ancestor(view))
    }

    /// Moves the cursor (selection and keyboard focus) to `position` and scrolls to it.
    pub fn move_cursor(&self, position: u32) {
        self.move_cursor_focus(position, self.has_focus());
    }

    fn move_cursor_focus(&self, position: u32, focus: bool) {
        if position >= self.sorted.n_items() {
            return;
        }
        if focus {
            self.view.scroll_to(
                position,
                None::<&gtk::ColumnViewColumn>,
                gtk::ListScrollFlags::FOCUS | gtk::ListScrollFlags::SELECT,
                None,
            );
        } else {
            self.selection.set_selected(position);
            self.view.scroll_to(
                position,
                None::<&gtk::ColumnViewColumn>,
                gtk::ListScrollFlags::empty(),
                None,
            );
        }
    }

    fn downgrade(&self) -> WeakPanel {
        WeakPanel {
            root: self.root.downgrade(),
            path_entry: self.path_entry.downgrade(),
            view: self.view.downgrade(),
            store: self.store.downgrade(),
            sorted: self.sorted.downgrade(),
            selection: self.selection.downgrade(),
            info_label: self.info_label.downgrade(),
            status_label: self.status_label.downgrade(),
            columns: Rc::downgrade(&self.columns),
            state: Rc::downgrade(&self.state),
        }
    }

    fn load(&self, location: Location, show_hidden: bool, quiet: bool) {
        let generation = self.state.borrow().generation.get().wrapping_add(1);
        self.state.borrow().generation.set(generation);
        self.state.borrow_mut().loading = Some(location.clone());
        self.path_entry.set_text(&location.display().to_string_lossy());

        let (sender, receiver) = async_channel::bounded(1);
        let worker_location = location.clone();
        std::thread::spawn(move || {
            let result = match &worker_location {
                Location::Dir(dir) => read_directory(dir, show_hidden),
                Location::Archive { archive, inner } => {
                    read_archive_directory(archive, inner, show_hidden)
                }
            };
            let _ = sender.send_blocking((generation, worker_location, result));
        });

        let weak = self.downgrade();
        glib::spawn_future_local(async move {
            let Ok((generation, location, result)) = receiver.recv().await else {
                return;
            };
            let Some(panel) = weak.upgrade() else {
                return;
            };
            if panel.state.borrow().generation.get() != generation {
                return;
            }
            panel.state.borrow_mut().loading = None;
            match result {
                Ok(listing) => panel.apply_listing(location, listing, quiet),
                Err(error) => {
                    panel.state.borrow_mut().pending_history = None;
                    panel.restore_path_entry();
                    if !quiet {
                        panel.show_message(&format!(
                            "Nelze číst adresář {}: {error}",
                            location.display().to_string_lossy()
                        ));
                    }
                }
            }
        });
    }

    fn apply_listing(&self, location: Location, listing: DirectoryListing, quiet: bool) {
        let same_directory = self.state.borrow().location == location;
        let dir = location.display();
        // Replacing the rows destroys the focused row widget, so remember whether the list
        // had keyboard focus and give it back afterwards.
        let had_focus = self.has_focus();
        let previous_cursor = same_directory.then(|| self.cursor_path()).flatten();
        let previous_position = self.selection.selected();
        let mut marked_paths = HashSet::new();
        let mut dir_sizes = HashMap::new();
        if same_directory {
            for entry in self.store.iter::<EntryObject>().flatten() {
                entry.with_entry(|item| {
                    if entry.is_marked() {
                        marked_paths.insert(item.path.clone());
                    }
                    if let Some(size) = entry.known_dir_size() {
                        dir_sizes.insert(item.path.clone(), size);
                    }
                });
            }
        }

        let items: Vec<EntryObject> = listing
            .entries
            .into_iter()
            .map(|entry| {
                let marked = marked_paths.contains(&entry.path);
                let size = dir_sizes.get(&entry.path).copied();
                let object = EntryObject::new(entry);
                object.set_marked(marked);
                if let Some(size) = size {
                    object.set_dir_size(size as i64);
                }
                object
            })
            .collect();
        self.store.splice(0, self.store.n_items(), &items);

        {
            let mut state = self.state.borrow_mut();
            state.location = location.clone();
            state.disk_space = listing.disk_space;
            state.note = listing.note.clone();
            match state.pending_history.take() {
                Some(position) => state.history_pos = position,
                None => {
                    if state.history.get(state.history_pos) != Some(&dir) {
                        let keep = (state.history_pos + 1).min(state.history.len());
                        state.history.truncate(keep);
                        state.history.push(dir.clone());
                        if state.history.len() > HISTORY_LIMIT {
                            state.history.remove(0);
                        }
                        state.history_pos = state.history.len() - 1;
                    }
                }
            }
        }
        self.path_entry.set_text(&dir.to_string_lossy());
        match &location {
            Location::Dir(real) => self.watch_directory(real),
            Location::Archive { .. } => self.unwatch(),
        }

        let pending_path = {
            let mut state = self.state.borrow_mut();
            if state
                .pending_cursor
                .as_ref()
                .is_some_and(|(pending_dir, _)| pending_dir == &dir)
            {
                state.pending_cursor.take().map(|(_, path)| path)
            } else {
                None
            }
        };

        let desired_path = pending_path.or(previous_cursor);
        let count = self.sorted.n_items();
        let position = desired_path
            .as_ref()
            .and_then(|path| self.find_sorted_path(path))
            // A deleted cursor item: stay at the same row rather than jumping to the top.
            .or_else(|| {
                (same_directory && previous_position != gtk::INVALID_LIST_POSITION && count > 0)
                    .then(|| previous_position.min(count - 1))
            })
            .or_else(|| (count > 0).then_some(0));
        if let Some(position) = position {
            self.move_cursor_focus(position, had_focus);
        }
        if had_focus && !self.has_focus() {
            self.view.grab_focus();
        }
        self.update_info();
        self.update_status();

        if !listing.warnings.is_empty() && !quiet {
            let mut message = format!("Varování při čtení adresáře {}:", dir.to_string_lossy());
            for warning in listing.warnings.iter().take(10) {
                message.push_str(&format!(
                    "\n{}: {}",
                    warning.source.to_string_lossy(),
                    warning.error
                ));
            }
            if listing.warnings.len() > 10 {
                message.push_str(&format!("\n… a dalších {}", listing.warnings.len() - 10));
            }
            self.show_message(&message);
        }
    }

    /// Watches the current directory with inotify (via GIO) and refreshes the listing shortly
    /// after changes, so external modifications show up without Ctrl+R.
    fn watch_directory(&self, dir: &Path) {
        if self.state.borrow().monitored_dir.as_deref() == Some(dir) {
            return;
        }
        let monitor = gio::File::for_path(dir)
            .monitor_directory(gio::FileMonitorFlags::WATCH_MOVES, gio::Cancellable::NONE)
            .ok();
        if let Some(monitor) = &monitor {
            monitor.set_rate_limit(500);
            let weak = self.downgrade();
            monitor.connect_changed(move |_, _, _, event| {
                if matches!(
                    event,
                    gio::FileMonitorEvent::ChangesDoneHint | gio::FileMonitorEvent::PreUnmount
                ) {
                    return;
                }
                if let Some(panel) = weak.upgrade() {
                    panel.schedule_refresh();
                }
            });
        }
        let mut state = self.state.borrow_mut();
        if let Some(old) = state.monitor.take() {
            old.cancel();
        }
        state.monitor = monitor;
        state.monitored_dir = Some(dir.to_path_buf());
    }

    fn unwatch(&self) {
        let mut state = self.state.borrow_mut();
        if let Some(old) = state.monitor.take() {
            old.cancel();
        }
        state.monitored_dir = None;
    }

    fn schedule_refresh(&self) {
        if std::mem::replace(&mut self.state.borrow_mut().refresh_pending, true) {
            return;
        }
        let weak = self.downgrade();
        glib::timeout_add_local_once(Duration::from_millis(300), move || {
            if let Some(panel) = weak.upgrade() {
                let (location, show_hidden) = {
                    let mut state = panel.state.borrow_mut();
                    state.refresh_pending = false;
                    (state.location.clone(), state.show_hidden)
                };
                panel.load(location, show_hidden, true);
            }
        });
    }

    fn update_info(&self) {
        let query = self.state.borrow().quick_search.clone();
        if !query.is_empty() {
            self.info_label
                .set_markup(&format!("Rychlé hledání: <b>{}</b>", glib::markup_escape_text(&query)));
            return;
        }
        let text = self
            .cursor_entry()
            .map(|entry| {
                let size = entry.display_size();
                entry.with_entry(|item| {
                    let mut text = item.name.to_string_lossy().into_owned();
                    if let Some(target) = &item.link_target {
                        text.push_str(" → ");
                        text.push_str(&target.to_string_lossy());
                    }
                    format!(
                        "{text}   {size}   {}   {}   {}:{}",
                        format_modified(item.modified),
                        item.mode_text(),
                        item.owner,
                        item.group
                    )
                })
            })
            .unwrap_or_default();
        self.info_label.set_text(&text);
    }

    fn update_status(&self) {
        let mut totals = Totals::default();
        for entry in self.store.iter::<EntryObject>().flatten() {
            let is_dir = entry.with_entry(FileEntry::is_dir_like);
            let size = entry.effective_size();
            if is_dir {
                totals.dirs += 1;
            } else {
                totals.files += 1;
            }
            totals.bytes += size;
            if entry.is_marked() {
                if is_dir {
                    totals.marked_dirs += 1;
                } else {
                    totals.marked_files += 1;
                }
                totals.marked_bytes += size;
            }
        }
        let mut text = if totals.marked_files + totals.marked_dirs > 0 {
            format!(
                "Vybráno {} z {} souborů, {} z {} adresářů — {} z {}",
                totals.marked_files,
                totals.files,
                totals.marked_dirs,
                totals.dirs,
                format_bytes(totals.marked_bytes),
                format_bytes(totals.bytes)
            )
        } else {
            format!(
                "{} souborů, {} adresářů — {}",
                totals.files,
                totals.dirs,
                format_bytes(totals.bytes)
            )
        };
        if let Some(note) = &self.state.borrow().note {
            text.insert_str(0, &format!("{note}   |   "));
        }
        if let Some(space) = self.state.borrow().disk_space {
            text.push_str(&format!(
                "   |   Volno {} z {}",
                format_bytes(space.available),
                format_bytes(space.total)
            ));
        }
        self.status_label.set_text(&text);
    }

    fn find_sorted_path(&self, path: &Path) -> Option<u32> {
        (0..self.sorted.n_items()).find(|&position| {
            self.sorted
                .item(position)
                .and_then(|item| item.downcast::<EntryObject>().ok())
                .is_some_and(|entry| entry.with_entry(|entry| entry.path == path))
        })
    }

    fn invalidate_loads(&self) {
        let generation = self.state.borrow().generation.get().wrapping_add(1);
        self.state.borrow().generation.set(generation);
        self.state.borrow_mut().loading = None;
    }

    fn restore_path_entry(&self) {
        let dir = self.state.borrow().location.display();
        self.path_entry.set_text(&dir.to_string_lossy());
    }

    fn show_message(&self, message: &str) {
        let parent = self.root.root().and_downcast::<gtk::Window>();
        let dialog = gtk::AlertDialog::builder()
            .modal(true)
            .message(message)
            .build();
        dialog.show(parent.as_ref());
    }
}

fn expand_tilde(path: &Path) -> PathBuf {
    let Ok(rest) = path.strip_prefix("~") else {
        return path.to_path_buf();
    };
    let Some(home) = env::var_os("HOME").map(PathBuf::from) else {
        return path.to_path_buf();
    };
    home.join(rest)
}

fn compare_name_entries(left: &FileEntry, right: &FileEntry) -> Ordering {
    left.folded_name.cmp(&right.folded_name).then_with(|| {
        left.name
            .as_os_str()
            .as_bytes()
            .cmp(right.name.as_os_str().as_bytes())
    })
}

fn entry_sorter(
    compare: impl Fn(&FileEntry, &EntryObject, &FileEntry, &EntryObject) -> Ordering + 'static,
) -> gtk::CustomSorter {
    gtk::CustomSorter::new(move |left, right| {
        let left = left.downcast_ref::<EntryObject>().expect("entry object");
        let right = right.downcast_ref::<EntryObject>().expect("entry object");
        left.with_entry(|l| right.with_entry(|r| compare(l, left, r, right)))
            .into()
    })
}

const NOTIFY_KEY: &str = "salamander-rs-notify";

fn make_factory(kind: CellKind, store: &gio::ListStore) -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();
    let weak_store = store.downgrade();
    factory.connect_setup(move |_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().expect("list item");
        let child: gtk::Widget = match kind {
            CellKind::Name => {
                let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
                row.append(&gtk::Image::new());
                let label = gtk::Label::new(None);
                label.set_xalign(0.0);
                label.set_ellipsize(gtk::pango::EllipsizeMode::End);
                row.append(&label);
                row.upcast()
            }
            _ => {
                let label = gtk::Label::new(None);
                label.set_xalign(match kind {
                    CellKind::Marked => 0.5,
                    CellKind::Size => 1.0,
                    _ => 0.0,
                });
                if matches!(kind, CellKind::Permissions) {
                    label.add_css_class("monospace");
                }
                if matches!(kind, CellKind::Owner | CellKind::Extension) {
                    label.set_ellipsize(gtk::pango::EllipsizeMode::End);
                }
                label.upcast()
            }
        };

        // Ctrl+click toggles the mark like in Salamander.
        let click = gtk::GestureClick::new();
        let weak_item = item.downgrade();
        let weak_store = weak_store.clone();
        click.connect_pressed(move |gesture, _, _, _| {
            if !gesture
                .current_event_state()
                .contains(gtk::gdk::ModifierType::CONTROL_MASK)
            {
                return;
            }
            let (Some(item), Some(store)) = (weak_item.upgrade(), weak_store.upgrade()) else {
                return;
            };
            if let Some(entry) = item.item().and_downcast::<EntryObject>() {
                entry.set_marked(!entry.is_marked());
                // Lets the panel refresh its status line (rows are kept for the same item).
                if let Some(position) = store.find(&entry) {
                    store.items_changed(position, 1, 1);
                }
            }
        });
        child.add_controller(click);
        item.set_child(Some(&child));
    });

    factory.connect_bind(move |_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().expect("list item");
        let Some(child) = item.child() else {
            return;
        };
        let Some(entry) = item.item().and_downcast::<EntryObject>() else {
            return;
        };
        update_cell(kind, &child, &entry);
        // Marks and computed sizes change without the item being replaced in the model, so
        // the cell follows property notifications.
        let weak_child = child.downgrade();
        let handler = entry.connect_notify_local(None, move |entry, _| {
            if let Some(child) = weak_child.upgrade() {
                update_cell(kind, &child, entry);
            }
        });
        unsafe {
            child.set_data(NOTIFY_KEY, (entry.downgrade(), handler));
        }
    });

    factory.connect_unbind(move |_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().expect("list item");
        if let Some(child) = item.child() {
            let data = unsafe {
                child.steal_data::<(glib::WeakRef<EntryObject>, glib::SignalHandlerId)>(NOTIFY_KEY)
            };
            if let Some((entry, handler)) = data {
                if let Some(entry) = entry.upgrade() {
                    entry.disconnect(handler);
                }
            }
        }
    });

    factory
}

fn icon_for(entry: &FileEntry) -> gio::Icon {
    let content_type = if entry.is_dir_like() {
        glib::GString::from("inode/directory")
    } else if entry.kind == EntryKind::Symlink {
        glib::GString::from("inode/symlink")
    } else if entry.kind == EntryKind::Other {
        glib::GString::from("inode/x-generic")
    } else if entry.mode & 0o111 != 0 && entry.extension().is_empty() {
        glib::GString::from("application/x-executable")
    } else {
        gio::content_type_guess(Some(Path::new(&entry.name)), None).0
    };
    gio::content_type_get_icon(&content_type)
}

fn update_cell(kind: CellKind, child: &gtk::Widget, entry: &EntryObject) {
    if entry.is_marked() {
        child.add_css_class("marked");
    } else {
        child.remove_css_class("marked");
    }
    let label = match kind {
        CellKind::Name => child
            .last_child()
            .and_downcast::<gtk::Label>()
            .expect("name label"),
        _ => child.clone().downcast::<gtk::Label>().expect("cell label"),
    };
    let text = match kind {
        CellKind::Marked => if entry.is_marked() { "●" } else { "" }.to_owned(),
        CellKind::Name => entry.with_entry(|item| {
            if let Some(image) = child.first_child().and_downcast::<gtk::Image>() {
                image.set_from_gicon(&icon_for(item));
            }
            if item.is_hidden() {
                label.add_css_class("dim-label");
            } else {
                label.remove_css_class("dim-label");
            }
            if item.kind == EntryKind::Symlink {
                label.add_css_class("symlink");
            } else {
                label.remove_css_class("symlink");
            }
            item.name.to_string_lossy().into_owned()
        }),
        CellKind::Extension => entry.with_entry(|item| item.extension().to_owned()),
        CellKind::Size => entry.display_size(),
        CellKind::Modified => entry.with_entry(|item| format_modified(item.modified)),
        CellKind::Permissions => entry.with_entry(FileEntry::mode_text),
        CellKind::Owner => entry.with_entry(|item| format!("{}:{}", item.owner, item.group)),
    };
    label.set_text(&text);
}
