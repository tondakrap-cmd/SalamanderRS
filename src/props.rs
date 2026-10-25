//! Properties dialog (Alt+Enter) and Change attributes (Ctrl+F2): general information,
//! Unix permissions, owner/group and timestamps for one or more items.

use std::cell::Cell;
use std::fs::File;
use std::io::Read;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::AtomicBool;
use std::time::SystemTime;

use gtk::gio;
use gtk::glib;
use gtk::prelude::*;

use crate::attrs::{self, AttrChange, ModeChange, ModeSummary, Scope};
use crate::model::{
    self, format_bytes, format_bytes_exact, format_mode, format_octal, format_time, EntryKind,
    FileEntry,
};
use crate::ops;
use crate::sys;
use crate::xattr;

const TIME_FORMAT: &str = "%Y-%m-%d %H:%M:%S";
const UNCHANGED: &str = "(neměnit)";

#[derive(Clone, Copy, Eq, PartialEq)]
pub enum Page {
    General,
    Permissions,
}

struct BitCheck {
    bit: u32,
    check: gtk::CheckButton,
    touched: Cell<bool>,
}

struct PermissionsUi {
    bits: Vec<BitCheck>,
    octal: gtk::Entry,
    preview: gtk::Label,
    updating: Cell<bool>,
}

impl PermissionsUi {
    fn current_mode(&self) -> Option<u32> {
        let mut mode = 0;
        for bit in &self.bits {
            if bit.check.is_inconsistent() {
                return None;
            }
            if bit.check.is_active() {
                mode |= bit.bit;
            }
        }
        Some(mode)
    }

    fn sync_from_checks(&self) {
        self.updating.set(true);
        match self.current_mode() {
            Some(mode) => {
                self.octal.set_text(&format_octal(mode));
                self.preview.set_text(&format_mode(mode)[1..]);
            }
            None => {
                self.octal.set_text("");
                self.preview.set_text("(různá oprávnění)");
            }
        }
        self.updating.set(false);
    }

    fn apply_octal(&self) {
        if self.updating.get() {
            return;
        }
        let Some(mode) = attrs::parse_octal(&self.octal.text()) else {
            return;
        };
        self.updating.set(true);
        for bit in &self.bits {
            bit.check.set_inconsistent(false);
            bit.check.set_active(mode & bit.bit != 0);
            bit.touched.set(true);
        }
        self.preview.set_text(&format_mode(mode)[1..]);
        self.updating.set(false);
    }

    /// Only bits the user touched are changed, so a recursive change never copies the
    /// top-level mode onto every child.
    fn change(&self, exec_only_dirs: bool) -> Option<ModeChange> {
        let mut change = ModeChange {
            exec_only_dirs,
            ..ModeChange::default()
        };
        for bit in &self.bits {
            if !bit.touched.get() || bit.check.is_inconsistent() {
                continue;
            }
            if bit.check.is_active() {
                change.set |= bit.bit;
            } else {
                change.clear |= bit.bit;
            }
        }
        (change.set != 0 || change.clear != 0).then_some(change)
    }
}

fn info_grid() -> gtk::Grid {
    let grid = gtk::Grid::new();
    grid.set_row_spacing(6);
    grid.set_column_spacing(12);
    grid.set_margin_top(12);
    grid.set_margin_bottom(12);
    grid.set_margin_start(12);
    grid.set_margin_end(12);
    grid
}

fn add_info_row(grid: &gtk::Grid, row: &mut i32, label: &str, value: &str) -> gtk::Label {
    let key = gtk::Label::new(Some(label));
    key.set_xalign(1.0);
    key.set_valign(gtk::Align::Start);
    key.add_css_class("dim-label");
    let value_label = gtk::Label::new(Some(value));
    value_label.set_xalign(0.0);
    value_label.set_selectable(true);
    value_label.set_wrap(true);
    value_label.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    value_label.set_hexpand(true);
    grid.attach(&key, 0, *row, 1, 1);
    grid.attach(&value_label, 1, *row, 1, 1);
    *row += 1;
    value_label
}

fn describe_type(entry: &FileEntry) -> String {
    let content_type = match entry.kind {
        EntryKind::Directory => "inode/directory".to_owned(),
        EntryKind::Symlink => "inode/symlink".to_owned(),
        EntryKind::Other => match entry.mode & model::S_IFMT {
            0o010000 => return "Pojmenovaná roura (FIFO)".to_owned(),
            0o140000 => return "Socket".to_owned(),
            0o020000 => return "Znakové zařízení".to_owned(),
            0o060000 => return "Blokové zařízení".to_owned(),
            _ => return "Speciální soubor".to_owned(),
        },
        EntryKind::File => {
            // Sniff the first bytes so files without a telling name are recognised too.
            let mut head = vec![0u8; 4096];
            let read = File::open(&entry.path)
                .and_then(|mut file| file.read(&mut head))
                .unwrap_or(0);
            head.truncate(read);
            gio::content_type_guess(Some(&entry.path), Some(head.as_slice())).0.to_string()
        }
    };
    let description = gio::content_type_get_description(&content_type);
    let mime = gio::content_type_get_mime_type(&content_type)
        .map(|mime| mime.to_string())
        .unwrap_or(content_type);
    format!("{description} ({mime})")
}

fn string_dropdown(items: &[String], selected: u32) -> gtk::DropDown {
    let refs: Vec<&str> = items.iter().map(String::as_str).collect();
    let dropdown = gtk::DropDown::from_strings(&refs);
    dropdown.set_expression(Some(gtk::PropertyExpression::new(
        gtk::StringObject::static_type(),
        None::<&gtk::Expression>,
        "string",
    )));
    dropdown.set_enable_search(true);
    dropdown.set_selected(selected);
    dropdown.set_hexpand(true);
    dropdown
}

fn time_row(
    grid: &gtk::Grid,
    row: i32,
    label: &str,
    initial: Option<SystemTime>,
) -> (gtk::CheckButton, gtk::Entry) {
    let check = gtk::CheckButton::with_mnemonic(label);
    let entry = gtk::Entry::new();
    entry.set_text(&format_time(initial, TIME_FORMAT));
    entry.set_placeholder_text(Some("RRRR-MM-DD HH:MM:SS"));
    entry.set_hexpand(true);
    let weak_check = check.downgrade();
    entry.connect_changed(move |_| {
        if let Some(check) = weak_check.upgrade() {
            check.set_active(true);
        }
    });
    grid.attach(&check, 0, row, 1, 1);
    grid.attach(&entry, 1, row, 1, 1);
    (check, entry)
}

pub fn open(
    parent: &gtk::ApplicationWindow,
    paths: Vec<PathBuf>,
    page: Page,
    on_apply: impl FnOnce(Vec<PathBuf>, AttrChange) + 'static,
) {
    let mut entries = Vec::new();
    let mut errors = Vec::new();
    for path in &paths {
        match model::stat_entry(path) {
            Ok(entry) => entries.push(entry),
            Err(error) => errors.push(format!("{}: {error}", path.to_string_lossy())),
        }
    }
    if entries.is_empty() {
        gtk::AlertDialog::builder()
            .modal(true)
            .message(format!("Nelze načíst vlastnosti:\n{}", errors.join("\n")))
            .build()
            .show(Some(parent));
        return;
    }
    let single = entries.len() == 1;
    let title = if single {
        format!("Vlastnosti — {}", entries[0].name.to_string_lossy())
    } else {
        format!("Vlastnosti — {} položek", entries.len())
    };
    let window = gtk::Window::builder()
        .transient_for(parent)
        .modal(true)
        .title(title)
        .default_width(560)
        .build();
    let content = gtk::Box::new(gtk::Orientation::Vertical, 8);
    content.set_margin_top(12);
    content.set_margin_bottom(12);
    content.set_margin_start(12);
    content.set_margin_end(12);
    window.set_child(Some(&content));
    let notebook = gtk::Notebook::new();
    content.append(&notebook);

    // ---- General ----
    let general = info_grid();
    let mut row = 0;
    let size_label;
    let measure_targets: Vec<PathBuf>;
    if single {
        let entry = &entries[0];
        add_info_row(&general, &mut row, "Název:", &entry.name.to_string_lossy());
        add_info_row(
            &general,
            &mut row,
            "Umístění:",
            &entry
                .path
                .parent()
                .map(|parent| parent.to_string_lossy().into_owned())
                .unwrap_or_default(),
        );
        add_info_row(&general, &mut row, "Typ:", &describe_type(entry));
        if let Some(target) = &entry.link_target {
            let state = if entry.path.exists() { "" } else { " (neplatný odkaz)" };
            add_info_row(
                &general,
                &mut row,
                "Cíl odkazu:",
                &format!("{}{state}", target.to_string_lossy()),
            );
        }
        let size_text = if entry.kind == EntryKind::Directory {
            "Počítám…".to_owned()
        } else {
            format!("{} ({} B)", format_bytes(entry.size), format_bytes_exact(entry.size))
        };
        size_label = add_info_row(&general, &mut row, "Velikost:", &size_text);
        measure_targets = if entry.kind == EntryKind::Directory {
            vec![entry.path.clone()]
        } else {
            Vec::new()
        };
        add_info_row(
            &general,
            &mut row,
            "Vlastník:",
            &format!("{} ({}) : {} ({})", entry.owner, entry.uid, entry.group, entry.gid),
        );
        add_info_row(
            &general,
            &mut row,
            "Oprávnění:",
            &format!("{}  ({})", entry.mode_text(), format_octal(entry.mode)),
        );
        if entry.kind != EntryKind::Symlink {
            if let Ok(flags) = xattr::get_flags(&entry.path) {
                let letters = xattr::format_flags(flags);
                add_info_row(
                    &general,
                    &mut row,
                    "Atributy (lsattr):",
                    if letters.is_empty() { "—" } else { &letters },
                );
            }
            if entry.has_acl {
                add_info_row(&general, &mut row, "ACL:", "ano (viz záložka Rozšířené)");
            }
        }
        add_info_row(
            &general,
            &mut row,
            "I-uzel / odkazů:",
            &format!("{} / {}", entry.inode, entry.nlink),
        );
        add_info_row(&general, &mut row, "Vytvořeno:", &format_time(entry.created, TIME_FORMAT));
        add_info_row(&general, &mut row, "Změněno:", &format_time(entry.modified, TIME_FORMAT));
        add_info_row(&general, &mut row, "Přístup:", &format_time(entry.accessed, TIME_FORMAT));
        add_info_row(
            &general,
            &mut row,
            "Změna i-uzlu:",
            &format_time(entry.changed, TIME_FORMAT),
        );
        if let Some(space) = entry.path.parent().and_then(sys::disk_space) {
            add_info_row(
                &general,
                &mut row,
                "Svazek:",
                &format!(
                    "volno {} z {}",
                    format_bytes(space.available),
                    format_bytes(space.total)
                ),
            );
        }
    } else {
        let dirs = entries
            .iter()
            .filter(|entry| entry.kind == EntryKind::Directory)
            .count();
        add_info_row(
            &general,
            &mut row,
            "Položek:",
            &format!("{} ({} souborů, {dirs} adresářů)", entries.len(), entries.len() - dirs),
        );
        if let Some(parent) = entries[0].path.parent() {
            add_info_row(&general, &mut row, "Umístění:", &parent.to_string_lossy());
        }
        size_label = add_info_row(&general, &mut row, "Celková velikost:", "Počítám…");
        measure_targets = entries.iter().map(|entry| entry.path.clone()).collect();
    }
    if !errors.is_empty() {
        add_info_row(&general, &mut row, "Chyby:", &errors.join("\n"));
    }
    notebook.append_page(&general, Some(&gtk::Label::with_mnemonic("_Obecné")));

    if !measure_targets.is_empty() {
        let (sender, receiver) = async_channel::bounded(1);
        let cancel = std::sync::Arc::new(AtomicBool::new(false));
        let worker_cancel = std::sync::Arc::clone(&cancel);
        std::thread::spawn(move || {
            let mut total = ops::Measure::default();
            for path in measure_targets {
                total.add(ops::measure(&path, &worker_cancel));
            }
            let _ = sender.send_blocking(total);
        });
        window.connect_destroy(move |_| cancel.store(true, std::sync::atomic::Ordering::Relaxed));
        let weak_label = size_label.downgrade();
        glib::spawn_future_local(async move {
            if let (Ok(total), Some(label)) = (receiver.recv().await, weak_label.upgrade()) {
                let mut text = format!(
                    "{} ({} B) — {} souborů, {} adresářů",
                    format_bytes(total.bytes),
                    format_bytes_exact(total.bytes),
                    total.files,
                    total.dirs
                );
                if total.errors > 0 {
                    text.push_str(&format!(", nečitelných {}", total.errors));
                }
                label.set_text(&text);
            }
        });
    }

    // ---- Permissions ----
    let permissions_box = gtk::Box::new(gtk::Orientation::Vertical, 10);
    permissions_box.set_margin_top(12);
    permissions_box.set_margin_bottom(12);
    permissions_box.set_margin_start(12);
    permissions_box.set_margin_end(12);
    let any_symlink = entries.iter().any(|entry| entry.kind == EntryKind::Symlink);
    let modes: Vec<u32> = entries
        .iter()
        .filter(|entry| entry.kind != EntryKind::Symlink)
        .map(|entry| entry.mode)
        .collect();
    let summary = ModeSummary::of(modes.iter().copied()).unwrap_or(ModeSummary { all: 0o777, any: 0o777 });

    let grid = gtk::Grid::new();
    grid.set_row_spacing(6);
    grid.set_column_spacing(18);
    for (column, title) in ["Čtení", "Zápis", "Spouštění"].iter().enumerate() {
        let label = gtk::Label::new(Some(title));
        label.add_css_class("heading");
        grid.attach(&label, column as i32 + 1, 0, 1, 1);
    }
    let mut bits = Vec::new();
    for (row, (title, shift)) in [("Vlastník", 6), ("Skupina", 3), ("Ostatní", 0)].iter().enumerate() {
        let label = gtk::Label::new(Some(title));
        label.set_xalign(0.0);
        grid.attach(&label, 0, row as i32 + 1, 1, 1);
        for column in 0..3 {
            let bit = 0o4 >> column << shift;
            let check = gtk::CheckButton::new();
            check.set_halign(gtk::Align::Center);
            grid.attach(&check, column + 1, row as i32 + 1, 1, 1);
            bits.push((bit, check));
        }
    }
    let special_label = gtk::Label::new(Some("Speciální"));
    special_label.set_xalign(0.0);
    grid.attach(&special_label, 0, 4, 1, 1);
    for (column, (title, bit, tooltip)) in [
        ("SUID", 0o4000, "Spouštět s právy vlastníka souboru"),
        ("SGID", 0o2000, "Spouštět s právy skupiny; u adresáře dědit skupinu"),
        ("Sticky", 0o1000, "V adresáři mazat jen vlastní soubory (jako /tmp)"),
    ]
    .iter()
    .enumerate()
    {
        let check = gtk::CheckButton::with_label(title);
        check.set_tooltip_text(Some(tooltip));
        grid.attach(&check, column as i32 + 1, 4, 1, 1);
        bits.push((*bit, check));
    }
    permissions_box.append(&grid);

    let octal_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let octal_label = gtk::Label::with_mnemonic("_Osmičkově:");
    let octal = gtk::Entry::new();
    octal.set_max_length(4);
    octal.set_width_chars(6);
    octal_label.set_mnemonic_widget(Some(&octal));
    let preview = gtk::Label::new(None);
    preview.add_css_class("monospace");
    octal_row.append(&octal_label);
    octal_row.append(&octal);
    octal_row.append(&preview);
    permissions_box.append(&octal_row);

    let permissions = Rc::new(PermissionsUi {
        bits: bits
            .into_iter()
            .map(|(bit, check)| {
                match summary.bit(bit) {
                    Some(active) => check.set_active(active),
                    None => check.set_inconsistent(true),
                }
                BitCheck {
                    bit,
                    check,
                    touched: Cell::new(false),
                }
            })
            .collect(),
        octal,
        preview,
        updating: Cell::new(false),
    });
    permissions.sync_from_checks();
    for index in 0..permissions.bits.len() {
        let weak = Rc::downgrade(&permissions);
        permissions.bits[index].check.connect_toggled(move |check| {
            let Some(permissions) = weak.upgrade() else {
                return;
            };
            if permissions.updating.get() {
                return;
            }
            check.set_inconsistent(false);
            permissions.bits[index].touched.set(true);
            permissions.sync_from_checks();
        });
    }
    let weak = Rc::downgrade(&permissions);
    permissions.octal.connect_changed(move |_| {
        if let Some(permissions) = weak.upgrade() {
            permissions.apply_octal();
        }
    });
    if any_symlink {
        let note = gtk::Label::new(Some(
            "Symbolické odkazy nemají vlastní oprávnění; mění se jen vlastník a časy.",
        ));
        note.set_xalign(0.0);
        note.set_wrap(true);
        note.add_css_class("dim-label");
        permissions_box.append(&note);
    }

    // Owner and group.
    let owner_grid = gtk::Grid::new();
    owner_grid.set_row_spacing(6);
    owner_grid.set_column_spacing(12);
    let mut users = vec![UNCHANGED.to_owned()];
    users.extend(sys::all_users());
    let mut groups = vec![UNCHANGED.to_owned()];
    groups.extend(sys::all_groups());
    let common_uid = entries
        .iter()
        .all(|entry| entry.uid == entries[0].uid)
        .then_some(entries[0].owner.clone());
    let common_gid = entries
        .iter()
        .all(|entry| entry.gid == entries[0].gid)
        .then_some(entries[0].group.clone());
    let position = |items: &[String], name: Option<String>| {
        name.and_then(|name| items.iter().position(|item| *item == name))
            .unwrap_or(0) as u32
    };
    let initial_user = position(&users, common_uid);
    let initial_group = position(&groups, common_gid);
    let user_dropdown = string_dropdown(&users, initial_user);
    let group_dropdown = string_dropdown(&groups, initial_group);
    let owner_label = gtk::Label::with_mnemonic("_Vlastník:");
    owner_label.set_xalign(0.0);
    owner_label.set_mnemonic_widget(Some(&user_dropdown));
    let group_label = gtk::Label::with_mnemonic("Sk_upina:");
    group_label.set_xalign(0.0);
    group_label.set_mnemonic_widget(Some(&group_dropdown));
    owner_grid.attach(&owner_label, 0, 0, 1, 1);
    owner_grid.attach(&user_dropdown, 1, 0, 1, 1);
    owner_grid.attach(&group_label, 0, 1, 1, 1);
    owner_grid.attach(&group_dropdown, 1, 1, 1, 1);
    permissions_box.append(&owner_grid);
    if sys::effective_uid() != 0 {
        let note = gtk::Label::new(Some(
            "Vlastníka může měnit jen root; skupinu lze změnit jen na skupinu, jejímž jste členem.",
        ));
        note.set_xalign(0.0);
        note.set_wrap(true);
        note.add_css_class("dim-label");
        permissions_box.append(&note);
    }

    // Recursion.
    let has_dirs = entries.iter().any(|entry| entry.kind == EntryKind::Directory);
    let recursive = gtk::CheckButton::with_mnemonic("Použít _rekurzivně na obsah adresářů");
    let scope = gtk::DropDown::from_strings(&["soubory i adresáře", "jen soubory", "jen adresáře"]);
    let exec_only_dirs = gtk::CheckButton::with_mnemonic(
        "Spouštění _nastavit jen adresářům a již spustitelným souborům (chmod X)",
    );
    let recursion_box = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    recursion_box.append(&recursive);
    recursion_box.append(&scope);
    scope.set_sensitive(false);
    exec_only_dirs.set_sensitive(false);
    let weak_scope = scope.downgrade();
    let weak_exec = exec_only_dirs.downgrade();
    recursive.connect_toggled(move |check| {
        if let (Some(scope), Some(exec)) = (weak_scope.upgrade(), weak_exec.upgrade()) {
            scope.set_sensitive(check.is_active());
            exec.set_sensitive(check.is_active());
        }
    });
    if has_dirs {
        permissions_box.append(&recursion_box);
        permissions_box.append(&exec_only_dirs);
    }
    notebook.append_page(&permissions_box, Some(&gtk::Label::with_mnemonic("_Oprávnění")));

    // ---- Times ----
    let times = info_grid();
    let (modified_check, modified_entry) =
        time_row(&times, 0, "Čas _změny:", entries[0].modified);
    let (accessed_check, accessed_entry) =
        time_row(&times, 1, "Čas _přístupu:", entries[0].accessed);
    modified_check.set_active(false);
    accessed_check.set_active(false);
    let now_button = gtk::Button::with_mnemonic("Nastavit na _teď");
    now_button.set_halign(gtk::Align::Start);
    {
        let modified_entry = modified_entry.downgrade();
        let accessed_entry = accessed_entry.downgrade();
        now_button.connect_clicked(move |_| {
            let now = format_time(Some(SystemTime::now()), TIME_FORMAT);
            for entry in [&modified_entry, &accessed_entry] {
                if let Some(entry) = entry.upgrade() {
                    entry.set_text(&now);
                }
            }
        });
    }
    times.attach(&now_button, 1, 2, 1, 1);
    let ctime_note = gtk::Label::new(Some(
        "Čas změny i-uzlu (ctime) nastavuje jádro a nelze jej změnit. \
         Při rekurzi se časy nastaví všem položkám v rozsahu.",
    ));
    ctime_note.set_wrap(true);
    ctime_note.set_xalign(0.0);
    ctime_note.add_css_class("dim-label");
    times.attach(&ctime_note, 0, 3, 2, 1);
    notebook.append_page(&times, Some(&gtk::Label::with_mnemonic("Č_asy")));
    let extended = single.then(|| build_extended_page(&notebook, &window, &entries[0]));

    if page == Page::Permissions {
        notebook.set_current_page(Some(1));
    }

    // ---- Buttons ----
    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    buttons.set_halign(gtk::Align::End);
    let cancel = gtk::Button::with_mnemonic("_Zavřít");
    let apply = gtk::Button::with_mnemonic("_Použít");
    apply.add_css_class("suggested-action");
    buttons.append(&cancel);
    buttons.append(&apply);
    content.append(&buttons);
    window.set_default_widget(Some(&apply));

    let weak_window = window.downgrade();
    cancel.connect_clicked(move |_| {
        if let Some(window) = weak_window.upgrade() {
            window.close();
        }
    });
    let keys = gtk::EventControllerKey::new();
    let weak_window = window.downgrade();
    keys.connect_key_pressed(move |_, key, _, _| {
        if key == gtk::gdk::Key::Escape {
            if let Some(window) = weak_window.upgrade() {
                window.close();
            }
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    });
    window.add_controller(keys);

    let on_apply = Rc::new(Cell::new(Some(on_apply)));
    let paths: Vec<PathBuf> = entries.iter().map(|entry| entry.path.clone()).collect();
    let weak_window = window.downgrade();
    apply.connect_clicked(move |_| {
        let Some(window) = weak_window.upgrade() else {
            return;
        };
        let parse_time = |check: &gtk::CheckButton, entry: &gtk::Entry, what: &str| {
            if !check.is_active() {
                return Ok(None);
            }
            model::parse_local_time(&entry.text())
                .map(Some)
                .ok_or_else(|| format!("Neplatný {what}: očekáván formát RRRR-MM-DD HH:MM:SS"))
        };
        let times = parse_time(&modified_check, &modified_entry, "čas změny").and_then(|modified| {
            parse_time(&accessed_check, &accessed_entry, "čas přístupu")
                .map(|accessed| (modified, accessed))
        });
        let (modified, accessed) = match times {
            Ok(times) => times,
            Err(message) => {
                gtk::AlertDialog::builder()
                    .modal(true)
                    .message(message)
                    .build()
                    .show(Some(&window));
                return;
            }
        };
        let selected_name = |dropdown: &gtk::DropDown, initial: u32| -> Option<String> {
            let selected = dropdown.selected();
            (selected != 0 && selected != initial)
                .then(|| dropdown.selected_item().and_downcast::<gtk::StringObject>())
                .flatten()
                .map(|item| item.string().to_string())
        };
        let uid = selected_name(&user_dropdown, initial_user).and_then(|name| sys::user_id(&name));
        let gid = selected_name(&group_dropdown, initial_group).and_then(|name| sys::group_id(&name));
        let change = AttrChange {
            mode: permissions.change(exec_only_dirs.is_active() && recursive.is_active()),
            uid,
            gid,
            accessed,
            modified,
            recursive: recursive.is_active() && has_dirs,
            scope: match scope.selected() {
                1 => Scope::FilesOnly,
                2 => Scope::DirsOnly,
                _ => Scope::All,
            },
        };
        // Flags and ACLs of a single item are quick and applied right here.
        let extended_errors = extended.as_ref().map(|page| page.apply()).unwrap_or_default();
        let parent = window.transient_for();
        window.close();
        if !extended_errors.is_empty() {
            gtk::AlertDialog::builder()
                .modal(true)
                .message(format!("Rozšířené atributy:\n{}", extended_errors.join("\n")))
                .build()
                .show(parent.as_ref());
        }
        if change.is_empty() {
            return;
        }
        if let Some(on_apply) = on_apply.take() {
            on_apply(paths.clone(), change);
        }
    });

    window.present();
}

/// Short human-readable summary of a change for the progress window title.
pub fn describe_change(change: &AttrChange) -> String {
    let mut parts = Vec::new();
    if let Some(mode) = change.mode {
        let mut text = String::from("chmod");
        if mode.set != 0 {
            text.push_str(&format!(" +{:04o}", mode.set));
        }
        if mode.clear != 0 {
            text.push_str(&format!(" -{:04o}", mode.clear));
        }
        parts.push(text);
    }
    if change.uid.is_some() || change.gid.is_some() {
        parts.push("chown".to_owned());
    }
    if change.modified.is_some() || change.accessed.is_some() {
        parts.push("časy".to_owned());
    }
    if change.recursive {
        parts.push("rekurzivně".to_owned());
    }
    parts.join(", ")
}

struct ExtendedPage {
    path: PathBuf,
    is_dir: bool,
    /// Initial flags and the editable check boxes; `None` when unsupported.
    flags: Option<(u32, Vec<(u32, gtk::CheckButton)>)>,
    acl: Option<(gtk::TextView, String)>,
}

impl ExtendedPage {
    fn apply(&self) -> Vec<String> {
        let mut errors = Vec::new();
        if let Some((initial, checks)) = &self.flags {
            let mut flags = *initial;
            for (bit, check) in checks {
                if check.is_active() {
                    flags |= bit;
                } else {
                    flags &= !bit;
                }
            }
            if flags != *initial {
                if let Err(error) = xattr::set_flags(&self.path, flags) {
                    let hint = if error.raw_os_error() == Some(libc::EPERM) {
                        " (příznaky i a a smí měnit jen root)"
                    } else {
                        ""
                    };
                    errors.push(format!("chattr: {error}{hint}"));
                }
            }
        }
        if let Some((view, initial)) = &self.acl {
            let buffer = view.buffer();
            let text = buffer.text(&buffer.start_iter(), &buffer.end_iter(), false);
            if text.trim() != initial.trim() {
                if let Err(error) = xattr::write_acl_text(&self.path, &text, self.is_dir) {
                    errors.push(format!("ACL: {error}"));
                }
            }
        }
        errors
    }
}

fn section_heading(text: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_xalign(0.0);
    label.add_css_class("heading");
    label
}

fn note(text: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_xalign(0.0);
    label.set_wrap(true);
    label.add_css_class("dim-label");
    label
}

/// "Extended" tab: filesystem flags (chattr), POSIX ACL and extended attributes.
fn build_extended_page(notebook: &gtk::Notebook, window: &gtk::Window, entry: &FileEntry) -> Rc<ExtendedPage> {
    let page = gtk::Box::new(gtk::Orientation::Vertical, 8);
    page.set_margin_top(12);
    page.set_margin_bottom(12);
    page.set_margin_start(12);
    page.set_margin_end(12);
    let scrolled = gtk::ScrolledWindow::new();
    scrolled.set_child(Some(&page));
    scrolled.set_propagate_natural_height(true);
    scrolled.set_max_content_height(520);
    scrolled.set_hscrollbar_policy(gtk::PolicyType::Never);
    notebook.append_page(&scrolled, Some(&gtk::Label::with_mnemonic("_Rozšířené")));

    let is_symlink = entry.kind == EntryKind::Symlink;
    let is_dir = entry.kind == EntryKind::Directory;
    if is_symlink {
        page.append(&note(
            "Symbolické odkazy nemají příznaky souborového systému, ACL ani uživatelské \
             rozšířené atributy. Otevřete vlastnosti cíle odkazu.",
        ));
        return Rc::new(ExtendedPage {
            path: entry.path.clone(),
            is_dir,
            flags: None,
            acl: None,
        });
    }

    // ---- chattr ----
    page.append(&section_heading("Atributy souborového systému (chattr)"));
    let flags = match xattr::get_flags(&entry.path) {
        Ok(current) => {
            let grid = gtk::Grid::new();
            grid.set_row_spacing(4);
            grid.set_column_spacing(18);
            let mut checks = Vec::new();
            let mut position = 0;
            for flag in xattr::FLAGS {
                // Kernel-managed flags are only shown when set.
                if flag.read_only && current & flag.bit == 0 {
                    continue;
                }
                let check = gtk::CheckButton::with_label(flag.label);
                check.set_tooltip_text(Some(flag.tooltip));
                check.set_active(current & flag.bit != 0);
                check.set_sensitive(!flag.read_only);
                grid.attach(&check, position % 2, position / 2, 1, 1);
                position += 1;
                if !flag.read_only {
                    checks.push((flag.bit, check));
                }
            }
            page.append(&grid);
            Some((current, checks))
        }
        Err(error) => {
            page.append(&note(&format!("Souborový systém příznaky nepodporuje ({error}).")));
            None
        }
    };

    // ---- ACL ----
    page.append(&section_heading("Seznam řízení přístupu (POSIX ACL)"));
    let acl = match xattr::read_acl_text(&entry.path) {
        Ok(text) => {
            let view = gtk::TextView::new();
            view.set_monospace(true);
            view.set_top_margin(4);
            view.set_bottom_margin(4);
            view.set_left_margin(4);
            view.buffer().set_text(&text);
            let frame = gtk::Frame::new(None);
            let acl_scroll = gtk::ScrolledWindow::new();
            acl_scroll.set_min_content_height(110);
            acl_scroll.set_child(Some(&view));
            frame.set_child(Some(&acl_scroll));
            page.append(&frame);
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            let base = gtk::Button::with_mnemonic("Vložit _základ z oprávnění");
            let mode = entry.mode;
            let weak_view = view.downgrade();
            base.connect_clicked(move |_| {
                if let Some(view) = weak_view.upgrade() {
                    let rwx = |shift: u32| format_mode(mode)[1 + shift as usize..4 + shift as usize].replace(['s', 't'], "x").replace(['S', 'T'], "-");
                    let mut text = format!("user::{}\ngroup::{}\nother::{}\n", rwx(0), rwx(3), rwx(6));
                    if is_dir {
                        text.push_str(&format!(
                            "default:user::{}\ndefault:group::{}\ndefault:other::{}\n",
                            rwx(0),
                            rwx(3),
                            rwx(6)
                        ));
                    }
                    view.buffer().set_text(&text);
                }
            });
            let clear = gtk::Button::with_mnemonic("_Odstranit ACL");
            let weak_view = view.downgrade();
            clear.connect_clicked(move |_| {
                if let Some(view) = weak_view.upgrade() {
                    view.buffer().set_text("");
                }
            });
            row.append(&base);
            row.append(&clear);
            page.append(&row);
            page.append(&note(
                "Jeden záznam na řádek: user::rwx, user:jméno:r-x, group::r-x, group:jméno:rw-, \
                 mask::rwx, other::r--. Výchozí ACL adresáře začínají „default:“. Chybějící \
                 mask se dopočítá. Prázdný text ACL odstraní. Uplatní se tlačítkem Použít.",
            ));
            Some((view, text))
        }
        Err(error) => {
            page.append(&note(&format!("ACL nelze načíst ({error}).")));
            None
        }
    };

    // ---- xattr ----
    page.append(&section_heading("Rozšířené atributy (xattr)"));
    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::Single);
    let list_frame = gtk::Frame::new(None);
    let list_scroll = gtk::ScrolledWindow::new();
    list_scroll.set_min_content_height(110);
    list_scroll.set_child(Some(&list));
    list_frame.set_child(Some(&list_scroll));
    page.append(&list_frame);
    let status = note("");
    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let add = gtk::Button::with_mnemonic("_Přidat…");
    let edit = gtk::Button::with_mnemonic("_Upravit…");
    let remove = gtk::Button::with_mnemonic("_Smazat");
    buttons.append(&add);
    buttons.append(&edit);
    buttons.append(&remove);
    page.append(&buttons);
    page.append(&status);
    page.append(&note(
        "Změny xattr se uloží ihned. Běžní uživatelé mohou měnit jmenný prostor user.*; \
         security.* a trusted.* vyžadují oprávnění roota.",
    ));

    let path = entry.path.clone();
    let refresh: Rc<dyn Fn()> = {
        let list = list.downgrade();
        let status = status.downgrade();
        let path = path.clone();
        Rc::new(move || {
            let (Some(list), Some(status)) = (list.upgrade(), status.upgrade()) else {
                return;
            };
            list.remove_all();
            match xattr::list(&path) {
                Ok(names) => {
                    let names: Vec<String> = names
                        .into_iter()
                        .filter(|name| name != xattr::ACL_ACCESS && name != xattr::ACL_DEFAULT)
                        .collect();
                    for name in &names {
                        let value = xattr::get(&path, name)
                            .map(|value| value.map(|value| xattr::format_value(&value)).unwrap_or_default())
                            .unwrap_or_else(|error| format!("<{error}>"));
                        let label = gtk::Label::new(Some(&format!("{name} = {value}")));
                        label.set_xalign(0.0);
                        label.set_ellipsize(gtk::pango::EllipsizeMode::End);
                        label.set_margin_start(4);
                        label.set_widget_name(name);
                        list.append(&label);
                    }
                    status.set_text(if names.is_empty() { "Žádné rozšířené atributy." } else { "" });
                }
                Err(error) => status.set_text(&format!("Nelze načíst: {error}")),
            }
        })
    };
    refresh();

    let selected_name = {
        let list = list.downgrade();
        move || -> Option<String> {
            list.upgrade()?
                .selected_row()?
                .child()
                .map(|child| child.widget_name().to_string())
        }
    };
    let selected_name = Rc::new(selected_name);

    let open_editor = {
        let window = window.downgrade();
        let path = path.clone();
        let refresh = Rc::clone(&refresh);
        Rc::new(move |name: Option<String>| {
            let Some(window) = window.upgrade() else {
                return;
            };
            let value = name
                .as_deref()
                .and_then(|name| xattr::get(&path, name).ok().flatten())
                .map(|value| xattr::format_value(&value))
                .unwrap_or_default();
            let path = path.clone();
            let refresh = Rc::clone(&refresh);
            let shown_name = name.clone();
            xattr_editor(&window, shown_name.as_deref(), &value, move |editor, new_name, new_value| {
                let result = xattr::parse_value(&new_value).and_then(|bytes| {
                    xattr::set(&path, &new_name, &bytes).map_err(|error| {
                        if error.raw_os_error() == Some(libc::ENOTSUP) {
                            format!("{error} — použijte jmenný prostor user., např. user.{new_name}")
                        } else {
                            error.to_string()
                        }
                    })
                });
                // Renaming an attribute: remove the old one after the new one is stored.
                let result = result.and_then(|()| match &name {
                    Some(old) if *old != new_name => xattr::remove(&path, old).map_err(|error| error.to_string()),
                    _ => Ok(()),
                });
                match result {
                    Ok(()) => {
                        editor.close();
                        refresh();
                    }
                    Err(error) => gtk::AlertDialog::builder()
                        .modal(true)
                        .message(format!("Nelze uložit {new_name}: {error}"))
                        .build()
                        .show(Some(editor)),
                }
            });
        })
    };
    {
        let open_editor = Rc::clone(&open_editor);
        add.connect_clicked(move |_| open_editor(None));
    }
    {
        let edit_editor = Rc::clone(&open_editor);
        let edit_selected = Rc::clone(&selected_name);
        edit.connect_clicked(move |_| {
            if let Some(name) = edit_selected() {
                edit_editor(Some(name));
            }
        });
        let open_editor = Rc::clone(&open_editor);
        let selected_name = Rc::clone(&selected_name);
        list.connect_row_activated(move |_, _| {
            if let Some(name) = selected_name() {
                open_editor(Some(name));
            }
        });
    }
    {
        let path = path.clone();
        let refresh = Rc::clone(&refresh);
        let status = status.downgrade();
        remove.connect_clicked(move |_| {
            let Some(name) = selected_name() else {
                return;
            };
            match xattr::remove(&path, &name) {
                Ok(()) => refresh(),
                Err(error) => {
                    if let Some(status) = status.upgrade() {
                        status.set_text(&format!("Nelze smazat {name}: {error}"));
                    }
                }
            }
        });
    }

    Rc::new(ExtendedPage {
        path,
        is_dir,
        flags,
        acl,
    })
}

/// Small editor for one extended attribute. `on_save` receives the editor window so it can
/// stay open when saving fails.
fn xattr_editor(
    parent: &gtk::Window,
    name: Option<&str>,
    value: &str,
    on_save: impl Fn(&gtk::Window, String, String) + 'static,
) {
    let window = gtk::Window::builder()
        .transient_for(parent)
        .modal(true)
        .title(if name.is_some() { "Upravit atribut" } else { "Přidat atribut" })
        .default_width(480)
        .resizable(false)
        .build();
    let content = gtk::Box::new(gtk::Orientation::Vertical, 8);
    content.set_margin_top(18);
    content.set_margin_bottom(18);
    content.set_margin_start(18);
    content.set_margin_end(18);
    window.set_child(Some(&content));
    let grid = gtk::Grid::new();
    grid.set_row_spacing(6);
    grid.set_column_spacing(8);
    let name_entry = gtk::Entry::new();
    name_entry.set_text(name.unwrap_or("user."));
    name_entry.set_hexpand(true);
    let value_entry = gtk::Entry::new();
    value_entry.set_text(value);
    value_entry.set_placeholder_text(Some("text, nebo 0x… pro binární hodnotu"));
    value_entry.set_activates_default(true);
    for (row, (label, entry)) in [("_Jméno:", &name_entry), ("_Hodnota:", &value_entry)].into_iter().enumerate() {
        let label = gtk::Label::with_mnemonic(label);
        label.set_xalign(0.0);
        label.set_mnemonic_widget(Some(entry));
        grid.attach(&label, 0, row as i32, 1, 1);
        grid.attach(entry, 1, row as i32, 1, 1);
    }
    content.append(&grid);
    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    buttons.set_halign(gtk::Align::End);
    let cancel = gtk::Button::with_mnemonic("_Zrušit");
    let save = gtk::Button::with_mnemonic("_Uložit");
    save.add_css_class("suggested-action");
    buttons.append(&cancel);
    buttons.append(&save);
    content.append(&buttons);
    window.set_default_widget(Some(&save));
    let weak = window.downgrade();
    cancel.connect_clicked(move |_| {
        if let Some(window) = weak.upgrade() {
            window.close();
        }
    });
    let weak = window.downgrade();
    let (name_input, value_input) = (name_entry.clone(), value_entry.clone());
    save.connect_clicked(move |_| {
        let Some(window) = weak.upgrade() else {
            return;
        };
        let name_entry = &name_input;
        let value_entry = &value_input;
        let name = name_entry.text().trim().to_owned();
        if name.is_empty() || !name.contains('.') {
            gtk::AlertDialog::builder()
                .modal(true)
                .message("Jméno musí obsahovat jmenný prostor, např. user.komentar")
                .build()
                .show(Some(&window));
            return;
        }
        on_save(&window, name, value_entry.text().to_string());
    });
    window.present();
    if name.is_some() {
        value_entry.grab_focus();
    } else {
        name_entry.grab_focus();
        name_entry.set_position(-1);
    }
}
