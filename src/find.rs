//! Find files (Alt+F7): recursive search by name mask and optional content.

use std::cell::{Cell, RefCell};
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use gtk::glib;
use gtk::prelude::*;

use crate::model::Mask;

#[derive(Clone, Debug)]
pub struct Query {
    pub root: PathBuf,
    pub mask: Mask,
    /// Content to search for; empty means name-only search.
    pub content: Vec<u8>,
    pub case_sensitive: bool,
    pub include_hidden: bool,
    pub include_dirs: bool,
}

pub enum FindEvent {
    Directory(PathBuf),
    Found(PathBuf),
    Finished { found: u64, errors: u64, cancelled: bool },
}

/// Walks the tree without following symlinks. `on_event` returning false stops the search.
pub fn search(query: &Query, cancel: &AtomicBool, mut on_event: impl FnMut(FindEvent) -> bool) {
    let needle = if query.case_sensitive {
        query.content.clone()
    } else {
        query.content.to_ascii_lowercase()
    };
    let mut buffer = vec![0u8; 256 * 1024];
    let mut stack = vec![query.root.clone()];
    let (mut found, mut errors) = (0u64, 0u64);
    let mut stopped = false;

    'walk: while let Some(dir) = stack.pop() {
        if cancel.load(Ordering::Relaxed) || !on_event(FindEvent::Directory(dir.clone())) {
            stopped = true;
            break;
        }
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => {
                errors += 1;
                continue;
            }
        };
        let mut subdirs = Vec::new();
        for entry in entries {
            if cancel.load(Ordering::Relaxed) {
                stopped = true;
                break 'walk;
            }
            let Ok(entry) = entry else {
                errors += 1;
                continue;
            };
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !query.include_hidden && name.starts_with('.') {
                continue;
            }
            let Ok(file_type) = entry.file_type() else {
                errors += 1;
                continue;
            };
            let path = entry.path();
            if file_type.is_dir() {
                subdirs.push(path.clone());
            }
            if !query.mask.matches(&name) {
                continue;
            }
            let matched = if file_type.is_dir() {
                query.include_dirs && needle.is_empty()
            } else if needle.is_empty() {
                true
            } else if file_type.is_file() {
                match contains(&path, &needle, query.case_sensitive, &mut buffer, cancel) {
                    Ok(found) => found,
                    Err(_) => {
                        errors += 1;
                        false
                    }
                }
            } else {
                false
            };
            if matched {
                found += 1;
                if !on_event(FindEvent::Found(path)) {
                    stopped = true;
                    break 'walk;
                }
            }
        }
        // Reverse so that the stack pops subdirectories in directory order.
        subdirs.reverse();
        stack.extend(subdirs);
    }
    let cancelled = stopped || cancel.load(Ordering::Relaxed);
    on_event(FindEvent::Finished {
        found,
        errors,
        cancelled,
    });
}

/// Streams the file looking for `needle` (already lower-cased when case-insensitive).
fn contains(
    path: &Path,
    needle: &[u8],
    case_sensitive: bool,
    buffer: &mut [u8],
    cancel: &AtomicBool,
) -> io::Result<bool> {
    let mut file = File::open(path)?;
    let overlap = needle.len().saturating_sub(1);
    let mut window: Vec<u8> = Vec::with_capacity(buffer.len() + overlap);
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Ok(false);
        }
        let read = file.read(buffer)?;
        if read == 0 {
            return Ok(false);
        }
        let chunk = &mut buffer[..read];
        if !case_sensitive {
            chunk.make_ascii_lowercase();
        }
        window.extend_from_slice(chunk);
        if window.windows(needle.len()).any(|candidate| candidate == needle) {
            return Ok(true);
        }
        let keep = window.len().min(overlap);
        window.drain(..window.len() - keep);
    }
}

struct FindUi {
    window: gtk::Window,
    root_entry: gtk::Entry,
    mask_entry: gtk::Entry,
    content_entry: gtk::Entry,
    case_check: gtk::CheckButton,
    hidden_check: gtk::CheckButton,
    dirs_check: gtk::CheckButton,
    start_button: gtk::Button,
    stop_button: gtk::Button,
    status: gtk::Label,
    results: gtk::StringList,
    selection: gtk::SingleSelection,
    cancel: RefCell<Option<Arc<AtomicBool>>>,
    running: Cell<bool>,
}

/// Opens the find window. `on_goto` focuses a result in the active panel, `on_view` opens it
/// in the viewer.
pub fn open(
    parent: &gtk::ApplicationWindow,
    start_dir: &Path,
    on_goto: impl Fn(PathBuf) + 'static,
    on_view: impl Fn(PathBuf) + 'static,
) {
    let window = gtk::Window::builder()
        .transient_for(parent)
        .title("Hledat soubory")
        .default_width(760)
        .default_height(560)
        .build();
    let content = gtk::Box::new(gtk::Orientation::Vertical, 8);
    content.set_margin_top(12);
    content.set_margin_bottom(12);
    content.set_margin_start(12);
    content.set_margin_end(12);
    window.set_child(Some(&content));

    let grid = gtk::Grid::new();
    grid.set_row_spacing(6);
    grid.set_column_spacing(8);
    let add_row = |row: i32, label: &str, entry: &gtk::Entry| {
        let label = gtk::Label::with_mnemonic(label);
        label.set_xalign(0.0);
        label.set_mnemonic_widget(Some(entry));
        entry.set_hexpand(true);
        grid.attach(&label, 0, row, 1, 1);
        grid.attach(entry, 1, row, 1, 1);
    };
    let root_entry = gtk::Entry::new();
    root_entry.set_text(&start_dir.to_string_lossy());
    let mask_entry = gtk::Entry::new();
    mask_entry.set_text("*");
    mask_entry.set_tooltip_text(Some(
        "Masky oddělené ; — např. *.rs;*.toml, vyloučení za | (např. *|*.o)",
    ));
    let content_entry = gtk::Entry::new();
    content_entry.set_placeholder_text(Some("(nepovinné)"));
    add_row(0, "_Maska jména:", &mask_entry);
    add_row(1, "_Obsahuje text:", &content_entry);
    add_row(2, "Hledat _v:", &root_entry);
    content.append(&grid);

    let options = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    let case_check = gtk::CheckButton::with_mnemonic("_Rozlišovat velikost písmen");
    let hidden_check = gtk::CheckButton::with_mnemonic("Včetně _skrytých");
    let dirs_check = gtk::CheckButton::with_mnemonic("Včetně _adresářů");
    dirs_check.set_active(true);
    options.append(&case_check);
    options.append(&hidden_check);
    options.append(&dirs_check);
    content.append(&options);

    let results = gtk::StringList::new(&[]);
    let selection = gtk::SingleSelection::new(Some(results.clone()));
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(|_, item| {
        let label = gtk::Label::new(None);
        label.set_xalign(0.0);
        label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        item.downcast_ref::<gtk::ListItem>()
            .expect("list item")
            .set_child(Some(&label));
    });
    factory.connect_bind(|_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().expect("list item");
        if let (Some(label), Some(text)) = (
            item.child().and_downcast::<gtk::Label>(),
            item.item().and_downcast::<gtk::StringObject>(),
        ) {
            label.set_text(&text.string());
        }
    });
    let list = gtk::ListView::new(Some(selection.clone()), Some(factory));
    let scrolled = gtk::ScrolledWindow::new();
    scrolled.set_vexpand(true);
    scrolled.set_child(Some(&list));
    content.append(&scrolled);

    let status = gtk::Label::new(Some("Zadejte kritéria a stiskněte Hledat."));
    status.set_xalign(0.0);
    status.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    content.append(&status);

    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    buttons.set_halign(gtk::Align::End);
    let view_button = gtk::Button::with_mnemonic("_Zobrazit (F3)");
    let goto_button = gtk::Button::with_mnemonic("_Přejít na soubor");
    let stop_button = gtk::Button::with_mnemonic("Zas_tavit");
    stop_button.set_sensitive(false);
    let start_button = gtk::Button::with_mnemonic("_Hledat");
    start_button.add_css_class("suggested-action");
    let close_button = gtk::Button::with_mnemonic("Zavří_t");
    for button in [&view_button, &goto_button, &stop_button, &start_button, &close_button] {
        buttons.append(button);
    }
    content.append(&buttons);
    window.set_default_widget(Some(&start_button));
    for entry in [&root_entry, &mask_entry, &content_entry] {
        entry.set_activates_default(true);
    }

    let ui = Rc::new(FindUi {
        window,
        root_entry,
        mask_entry,
        content_entry,
        case_check,
        hidden_check,
        dirs_check,
        start_button,
        stop_button,
        status,
        results,
        selection,
        cancel: RefCell::new(None),
        running: Cell::new(false),
    });

    let selected_path = {
        let weak = Rc::downgrade(&ui);
        move || -> Option<PathBuf> {
            let ui = weak.upgrade()?;
            ui.selection
                .selected_item()
                .and_downcast::<gtk::StringObject>()
                .map(|item| PathBuf::from(item.string().as_str()))
        }
    };
    let selected_path = Rc::new(selected_path);
    let on_goto = Rc::new(on_goto);
    let on_view = Rc::new(on_view);

    {
        let selected_path = Rc::clone(&selected_path);
        let on_goto = Rc::clone(&on_goto);
        let weak = Rc::downgrade(&ui);
        let goto = move || {
            if let Some(path) = selected_path() {
                if let Some(ui) = weak.upgrade() {
                    ui.window.close();
                }
                on_goto(path);
            }
        };
        let goto = Rc::new(goto);
        let goto_clone = Rc::clone(&goto);
        goto_button.connect_clicked(move |_| goto_clone());
        list.connect_activate(move |_, _| goto());
    }
    {
        let selected_path = Rc::clone(&selected_path);
        let on_view = Rc::clone(&on_view);
        view_button.connect_clicked(move |_| {
            if let Some(path) = selected_path() {
                on_view(path);
            }
        });
    }

    let keys = gtk::EventControllerKey::new();
    {
        let weak = Rc::downgrade(&ui);
        let selected_path = Rc::clone(&selected_path);
        let on_view = Rc::clone(&on_view);
        keys.connect_key_pressed(move |_, key, _, _| match key {
            gtk::gdk::Key::Escape => {
                if let Some(ui) = weak.upgrade() {
                    if ui.running.get() {
                        ui.stop();
                    } else {
                        ui.window.close();
                    }
                }
                glib::Propagation::Stop
            }
            gtk::gdk::Key::F3 => {
                if let Some(path) = selected_path() {
                    on_view(path);
                }
                glib::Propagation::Stop
            }
            _ => glib::Propagation::Proceed,
        });
    }
    ui.window.add_controller(keys);

    let weak = Rc::downgrade(&ui);
    ui.start_button.connect_clicked(move |_| {
        if let Some(ui) = weak.upgrade() {
            ui.start();
        }
    });
    let weak = Rc::downgrade(&ui);
    ui.stop_button.connect_clicked(move |_| {
        if let Some(ui) = weak.upgrade() {
            ui.stop();
        }
    });
    let weak = Rc::downgrade(&ui);
    close_button.connect_clicked(move |_| {
        if let Some(ui) = weak.upgrade() {
            ui.window.close();
        }
    });
    // The window keeps the UI state alive; stop a running search when it closes.
    let owned = Rc::clone(&ui);
    ui.window.connect_close_request(move |_| {
        owned.stop();
        glib::Propagation::Proceed
    });

    ui.window.present();
    ui.mask_entry.grab_focus();
}

impl FindUi {
    fn start(self: &Rc<Self>) {
        if self.running.get() {
            return;
        }
        let root = PathBuf::from(self.root_entry.text().as_str());
        if !root.is_dir() {
            self.status
                .set_text(&format!("{} není adresář", root.to_string_lossy()));
            return;
        }
        let query = Query {
            root,
            mask: Mask::parse(&self.mask_entry.text()),
            content: self.content_entry.text().as_bytes().to_vec(),
            case_sensitive: self.case_check.is_active(),
            include_hidden: self.hidden_check.is_active(),
            include_dirs: self.dirs_check.is_active(),
        };
        self.results.splice(0, self.results.n_items(), &[] as &[&str]);
        let cancel = Arc::new(AtomicBool::new(false));
        *self.cancel.borrow_mut() = Some(Arc::clone(&cancel));
        self.running.set(true);
        self.start_button.set_sensitive(false);
        self.stop_button.set_sensitive(true);

        let (sender, receiver) = async_channel::bounded::<Vec<FindEvent>>(16);
        std::thread::spawn(move || {
            // Batch events so a fast name search does not flood the main loop.
            let mut batch = Vec::new();
            let mut last_flush = std::time::Instant::now();
            search(&query, &cancel, |event| {
                let finished = matches!(event, FindEvent::Finished { .. });
                batch.push(event);
                if finished || batch.len() >= 200 || last_flush.elapsed().as_millis() >= 100 {
                    last_flush = std::time::Instant::now();
                    return sender.send_blocking(std::mem::take(&mut batch)).is_ok();
                }
                true
            });
        });

        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            while let Ok(events) = receiver.recv().await {
                let Some(ui) = weak.upgrade() else {
                    return;
                };
                let mut found: Vec<String> = Vec::new();
                for event in events {
                    match event {
                        FindEvent::Directory(dir) => {
                            ui.status
                                .set_text(&format!("Prohledávám {}", dir.to_string_lossy()));
                        }
                        FindEvent::Found(path) => found.push(path.to_string_lossy().into_owned()),
                        FindEvent::Finished {
                            found: count,
                            errors,
                            cancelled,
                        } => {
                            let mut text = format!(
                                "{} — nalezeno {count}",
                                if cancelled { "Zastaveno" } else { "Hotovo" }
                            );
                            if errors > 0 {
                                text.push_str(&format!(", nečitelných položek: {errors}"));
                            }
                            ui.status.set_text(&text);
                            ui.running.set(false);
                            ui.start_button.set_sensitive(true);
                            ui.stop_button.set_sensitive(false);
                        }
                    }
                }
                if !found.is_empty() {
                    let refs: Vec<&str> = found.iter().map(String::as_str).collect();
                    ui.results.splice(ui.results.n_items(), 0, &refs);
                }
            }
        });
    }

    fn stop(&self) {
        if let Some(cancel) = self.cancel.borrow().as_ref() {
            cancel.store(true, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(query: &Query) -> Vec<PathBuf> {
        let mut found = Vec::new();
        search(query, &AtomicBool::new(false), |event| {
            if let FindEvent::Found(path) = event {
                found.push(path);
            }
            true
        });
        found.sort();
        found
    }

    fn query(root: &Path, mask: &str, content: &str) -> Query {
        Query {
            root: root.to_path_buf(),
            mask: Mask::parse(mask),
            content: content.as_bytes().to_vec(),
            case_sensitive: false,
            include_hidden: false,
            include_dirs: true,
        }
    }

    #[test]
    fn finds_by_mask_recursively_skipping_hidden() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(root.join("a/one.rs"), "").unwrap();
        fs::write(root.join("a/b/two.RS"), "").unwrap();
        fs::write(root.join(".git/three.rs"), "").unwrap();
        fs::write(root.join("a/readme.md"), "").unwrap();

        assert_eq!(
            run(&query(root, "*.rs", "")),
            vec![root.join("a/b/two.RS"), root.join("a/one.rs")]
        );
        let mut with_hidden = query(root, "*.rs", "");
        with_hidden.include_hidden = true;
        assert_eq!(run(&with_hidden).len(), 3);
    }

    #[test]
    fn content_search_handles_case_and_chunk_boundaries() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let mut big = vec![b'x'; 256 * 1024 - 3];
        big.extend_from_slice(b"NEEDLE");
        fs::write(root.join("big.bin"), &big).unwrap();
        fs::write(root.join("small.txt"), "a needle here").unwrap();
        fs::write(root.join("none.txt"), "nothing").unwrap();

        assert_eq!(
            run(&query(root, "*", "needle")),
            vec![root.join("big.bin"), root.join("small.txt")]
        );
        let mut sensitive = query(root, "*", "needle");
        sensitive.case_sensitive = true;
        assert_eq!(run(&sensitive), vec![root.join("small.txt")]);
    }

    #[test]
    fn reports_finish_once_and_honours_stop() {
        let temp = tempfile::tempdir().unwrap();
        for index in 0..5 {
            fs::write(temp.path().join(format!("f{index}")), "").unwrap();
        }
        let mut found = 0;
        let mut finished = 0;
        search(
            &query(temp.path(), "*", ""),
            &AtomicBool::new(false),
            |event| match event {
                FindEvent::Found(_) => {
                    found += 1;
                    found < 2
                }
                FindEvent::Finished { cancelled, .. } => {
                    assert!(cancelled);
                    finished += 1;
                    true
                }
                FindEvent::Directory(_) => true,
            },
        );
        assert_eq!(found, 2);
        assert_eq!(finished, 1);
    }
}
