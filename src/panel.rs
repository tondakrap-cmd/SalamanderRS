use std::cell::{Cell, RefCell};
use std::cmp::Ordering;
use std::collections::HashSet;
use std::env;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};

use gtk::gio;
use gtk::glib;
use gtk::prelude::*;

use crate::model::{
    format_modified, format_size, read_directory, DirectoryListing, EntryKind, EntryObject,
};

struct PanelState {
    dir: PathBuf,
    show_hidden: bool,
    generation: Cell<u64>,
    pending_cursor: Option<(PathBuf, PathBuf)>,
}

#[derive(Clone)]
pub struct Panel {
    pub root: gtk::Box,
    pub path_entry: gtk::Entry,
    view: gtk::ColumnView,
    store: gio::ListStore,
    sorted: gtk::SortListModel,
    selection: gtk::SingleSelection,
    state: Rc<RefCell<PanelState>>,
}

struct WeakPanel {
    root: glib::WeakRef<gtk::Box>,
    path_entry: glib::WeakRef<gtk::Entry>,
    view: glib::WeakRef<gtk::ColumnView>,
    store: glib::WeakRef<gio::ListStore>,
    sorted: glib::WeakRef<gtk::SortListModel>,
    selection: glib::WeakRef<gtk::SingleSelection>,
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
            state: self.state.upgrade()?,
        })
    }
}

#[derive(Clone, Copy)]
enum CellKind {
    Marked,
    Name,
    Size,
    Modified,
}

impl Panel {
    pub fn new(start_dir: &Path) -> Panel {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 6);
        root.set_margin_top(6);
        root.set_margin_bottom(6);
        root.set_margin_start(6);
        root.set_margin_end(6);

        let path_entry = gtk::Entry::new();
        path_entry.set_text(&start_dir.to_string_lossy());
        path_entry.set_tooltip_text(Some("Cesta k adresáři"));
        root.append(&path_entry);

        let store = gio::ListStore::new::<EntryObject>();
        let sorted = gtk::SortListModel::new(Some(store.clone()), None::<gtk::Sorter>);
        let section_sorter = gtk::CustomSorter::new(|left, right| {
            let left = left.downcast_ref::<EntryObject>().expect("entry object");
            let right = right.downcast_ref::<EntryObject>().expect("entry object");
            let rank = |entry: &EntryObject| {
                if entry.with_entry(|entry| entry.kind == EntryKind::Directory) {
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

        let marked_column =
            gtk::ColumnViewColumn::new(None, Some(make_factory(CellKind::Marked, &store)));
        marked_column.set_fixed_width(28);
        view.append_column(&marked_column);

        let name_column =
            gtk::ColumnViewColumn::new(Some("Název"), Some(make_factory(CellKind::Name, &store)));
        name_column.set_expand(true);
        name_column.set_sorter(Some(&gtk::CustomSorter::new(|left, right| {
            compare_by(left, right, compare_name)
        })));
        view.append_column(&name_column);

        let size_column = gtk::ColumnViewColumn::new(
            Some("Velikost"),
            Some(make_factory(CellKind::Size, &store)),
        );
        size_column.set_resizable(true);
        size_column.set_sorter(Some(&gtk::CustomSorter::new(|left, right| {
            compare_by(left, right, |left, right| {
                left.with_entry(|left| {
                    right.with_entry(|right| {
                        left.size
                            .cmp(&right.size)
                            .then_with(|| compare_name_entries(left, right))
                    })
                })
            })
        })));
        view.append_column(&size_column);

        let modified_column = gtk::ColumnViewColumn::new(
            Some("Změněno"),
            Some(make_factory(CellKind::Modified, &store)),
        );
        modified_column.set_resizable(true);
        modified_column.set_sorter(Some(&gtk::CustomSorter::new(|left, right| {
            compare_by(left, right, |left, right| {
                left.with_entry(|left| {
                    right.with_entry(|right| {
                        left.modified
                            .cmp(&right.modified)
                            .then_with(|| compare_name_entries(left, right))
                    })
                })
            })
        })));
        view.append_column(&modified_column);

        let scrolled = gtk::ScrolledWindow::new();
        scrolled.set_vexpand(true);
        scrolled.set_hexpand(true);
        scrolled.set_child(Some(&view));
        root.append(&scrolled);

        let panel = Panel {
            root,
            path_entry,
            view,
            store,
            sorted,
            selection,
            state: Rc::new(RefCell::new(PanelState {
                dir: start_dir.to_path_buf(),
                show_hidden: false,
                generation: Cell::new(0),
                pending_cursor: None,
            })),
        };

        let weak = panel.downgrade();
        panel.path_entry.connect_activate(move |_| {
            if let Some(panel) = weak.upgrade() {
                let requested = PathBuf::from(panel.path_entry.text());
                panel.navigate(&requested);
            }
        });

        let view_sorter = panel.view.sorter();
        panel.sorted.set_sorter(view_sorter.as_ref());
        panel
            .view
            .sort_by_column(Some(&name_column), gtk::SortType::Ascending);

        panel.reload();
        panel
    }

    pub fn view(&self) -> &gtk::ColumnView {
        &self.view
    }

    pub fn dir(&self) -> PathBuf {
        self.state.borrow().dir.clone()
    }

    pub fn resolve_path(&self, requested: &Path) -> PathBuf {
        let expanded = expand_tilde(requested);
        if expanded.is_absolute() {
            expanded
        } else {
            self.state.borrow().dir.join(expanded)
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

    pub fn navigate_parent(&self) {
        let parent = self.state.borrow().dir.parent().map(Path::to_path_buf);
        if let Some(parent) = parent {
            self.navigate(&parent);
        }
    }

    pub fn cursor_path(&self) -> Option<PathBuf> {
        self.selection
            .selected_item()
            .and_then(|item| item.downcast::<EntryObject>().ok())
            .map(|entry| entry.with_entry(|entry| entry.path.clone()))
    }

    pub fn selected_or_cursor(&self) -> Vec<PathBuf> {
        let marked: Vec<PathBuf> = self
            .store
            .iter::<EntryObject>()
            .flatten()
            .filter(|entry| entry.is_marked())
            .map(|entry| entry.with_entry(|entry| entry.path.clone()))
            .collect();
        if marked.is_empty() {
            self.cursor_path().into_iter().collect()
        } else {
            marked
        }
    }

    pub fn reload(&self) {
        let (dir, show_hidden) = {
            let state = self.state.borrow();
            (state.dir.clone(), state.show_hidden)
        };
        self.load(dir, show_hidden);
    }

    pub fn reload_and_focus(&self, path: &Path) {
        {
            let mut state = self.state.borrow_mut();
            state.pending_cursor = Some((state.dir.clone(), path.to_path_buf()));
        }
        self.reload();
    }

    pub fn navigate(&self, requested: &Path) {
        self.state.borrow_mut().pending_cursor = None;

        let expanded = expand_tilde(requested);
        let candidate = if expanded.is_absolute() {
            expanded
        } else {
            self.state.borrow().dir.join(expanded)
        };

        let candidate = match fs::canonicalize(&candidate) {
            Ok(candidate) if candidate.is_dir() => candidate,
            Ok(candidate) => {
                self.invalidate_loads();
                self.restore_path_entry();
                self.show_message(&format!(
                    "Nelze číst adresář {}: není adresář",
                    candidate.to_string_lossy()
                ));
                return;
            }
            Err(error) => {
                self.invalidate_loads();
                self.restore_path_entry();
                self.show_message(&format!(
                    "Nelze číst adresář {}: {error}",
                    candidate.to_string_lossy()
                ));
                return;
            }
        };

        let show_hidden = self.state.borrow().show_hidden;
        self.load(candidate, show_hidden);
    }

    pub fn clear_marks(&self) {
        for entry in self.store.iter::<EntryObject>().flatten() {
            entry.set_marked(false);
        }
        self.notify_all_changed();
    }

    pub(crate) fn mark_all(&self) {
        for entry in self.store.iter::<EntryObject>().flatten() {
            entry.set_marked(true);
        }
        self.notify_all_changed();
    }

    pub(crate) fn toggle_mark_and_advance(&self) {
        let Some(entry) = self
            .selection
            .selected_item()
            .and_then(|item| item.downcast::<EntryObject>().ok())
        else {
            return;
        };
        toggle_mark(&self.store, &entry);

        let position = self.selection.selected();
        let next = position.saturating_add(1);
        if position != gtk::INVALID_LIST_POSITION && next < self.sorted.n_items() {
            self.selection.set_selected(next);
            self.view.scroll_to(
                next,
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
            state: Rc::downgrade(&self.state),
        }
    }

    fn load(&self, dir: PathBuf, show_hidden: bool) {
        let generation = self.state.borrow().generation.get().wrapping_add(1);
        self.state.borrow().generation.set(generation);
        self.path_entry.set_text(&dir.to_string_lossy());

        let (sender, receiver) = async_channel::bounded(1);
        let worker_dir = dir.clone();
        std::thread::spawn(move || {
            let result = read_directory(&worker_dir, show_hidden);
            let _ = sender.send_blocking((generation, worker_dir, result));
        });

        let weak = self.downgrade();
        glib::spawn_future_local(async move {
            let Ok((generation, dir, result)) = receiver.recv().await else {
                return;
            };
            let Some(panel) = weak.upgrade() else {
                return;
            };
            if panel.state.borrow().generation.get() != generation {
                return;
            }
            match result {
                Ok(listing) => panel.apply_listing(dir, listing),
                Err(error) => {
                    panel.restore_path_entry();
                    panel.show_message(&format!(
                        "Nelze číst adresář {}: {error}",
                        dir.to_string_lossy()
                    ));
                }
            }
        });
    }

    fn apply_listing(&self, dir: PathBuf, listing: DirectoryListing) {
        let same_directory = self.state.borrow().dir == dir;
        let previous_cursor = same_directory.then(|| self.cursor_path()).flatten();
        let marked_paths: HashSet<PathBuf> = if same_directory {
            self.store
                .iter::<EntryObject>()
                .flatten()
                .filter(|entry| entry.is_marked())
                .map(|entry| entry.with_entry(|entry| entry.path.clone()))
                .collect()
        } else {
            HashSet::new()
        };

        self.store.remove_all();
        for entry in listing.entries {
            let path = entry.path.clone();
            let marked = same_directory && marked_paths.contains(&path);
            let entry = EntryObject::new(entry);
            entry.set_marked(marked);
            self.store.append(&entry);
        }

        self.state.borrow_mut().dir = dir.clone();
        self.path_entry.set_text(&dir.to_string_lossy());

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
        let position = desired_path
            .as_ref()
            .and_then(|path| self.find_sorted_path(path))
            .or_else(|| (self.sorted.n_items() > 0).then_some(0));
        if let Some(position) = position {
            self.selection.set_selected(position);
        }

        if !listing.warnings.is_empty() {
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

    fn find_sorted_path(&self, path: &Path) -> Option<u32> {
        (0..self.sorted.n_items()).find(|&position| {
            self.sorted
                .item(position)
                .and_then(|item| item.downcast::<EntryObject>().ok())
                .is_some_and(|entry| entry.with_entry(|entry| entry.path == path))
        })
    }

    fn notify_all_changed(&self) {
        let count = self.store.n_items();
        if count > 0 {
            self.store.items_changed(0, count, count);
        }
    }

    fn invalidate_loads(&self) {
        let generation = self.state.borrow().generation.get().wrapping_add(1);
        self.state.borrow().generation.set(generation);
    }

    fn restore_path_entry(&self) {
        let dir = self.state.borrow().dir.clone();
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

fn compare_name(left: &EntryObject, right: &EntryObject) -> Ordering {
    left.with_entry(|left| right.with_entry(|right| compare_name_entries(left, right)))
}

fn compare_name_entries(
    left: &crate::model::FileEntry,
    right: &crate::model::FileEntry,
) -> Ordering {
    left.folded_name.cmp(&right.folded_name).then_with(|| {
        left.name
            .as_os_str()
            .as_bytes()
            .cmp(right.name.as_os_str().as_bytes())
    })
}

fn compare_by(
    left: &glib::Object,
    right: &glib::Object,
    compare: impl FnOnce(&EntryObject, &EntryObject) -> Ordering,
) -> gtk::Ordering {
    let left = left.downcast_ref::<EntryObject>().expect("entry object");
    let right = right.downcast_ref::<EntryObject>().expect("entry object");
    compare(left, right).into()
}

fn toggle_mark(store: &gio::ListStore, entry: &EntryObject) {
    entry.set_marked(!entry.is_marked());
    if let Some(base_position) = store.find(entry) {
        store.items_changed(base_position, 1, 1);
    }
}

fn make_factory(kind: CellKind, store: &gio::ListStore) -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();
    let weak_store = store.downgrade();
    factory.connect_setup(move |_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().expect("list item");
        let label = gtk::Label::new(None);
        label.set_xalign(if matches!(kind, CellKind::Marked) {
            0.5
        } else {
            0.0
        });

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
            let Some(entry) = item
                .item()
                .and_then(|object| object.downcast::<EntryObject>().ok())
            else {
                return;
            };
            toggle_mark(&store, &entry);
        });
        label.add_controller(click);
        item.set_child(Some(&label));
    });

    factory.connect_bind(move |_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().expect("list item");
        let Some(label) = item.child().and_downcast::<gtk::Label>() else {
            return;
        };
        let Some(entry) = item
            .item()
            .and_then(|object| object.downcast::<EntryObject>().ok())
        else {
            label.set_text("");
            return;
        };
        let text = match kind {
            CellKind::Marked => if entry.is_marked() { "●" } else { "" }.to_owned(),
            CellKind::Name => entry.with_entry(|entry| entry.name.to_string_lossy().into_owned()),
            CellKind::Size => entry.with_entry(format_size),
            CellKind::Modified => entry.with_entry(|entry| format_modified(entry.modified)),
        };
        label.set_text(&text);
        if matches!(kind, CellKind::Marked) {
            let binding = entry
                .bind_property("marked", &label, "label")
                .transform_to(|_, marked: bool| {
                    Some(if marked { "●" } else { "" }.to_value())
                })
                .build();
            unsafe {
                label.set_data("salamander-rs-marked-binding", binding);
            }
        }
    });

    factory.connect_unbind(move |_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().expect("list item");
        if let Some(label) = item.child().and_downcast::<gtk::Label>() {
            if matches!(kind, CellKind::Marked) {
                unsafe {
                    label.steal_data::<glib::Binding>("salamander-rs-marked-binding");
                }
            }
            label.set_text("");
        }
    });

    factory
}
