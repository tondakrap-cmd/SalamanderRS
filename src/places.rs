//! "Go to" dialog (Alt+F1 / Alt+F2) — the Linux counterpart of Salamander's drive menu:
//! home and XDG folders, mounted and mountable volumes, GTK bookmarks, hot paths and history.

use std::cell::RefCell;
use std::env;
use std::fs;
use std::path::PathBuf;
use std::rc::Rc;

use gtk::gio;
use gtk::glib;
use gtk::prelude::*;

use crate::model::format_bytes;
use crate::remote::servers::Server;
use crate::sys;

enum Target {
    Path(PathBuf),
    Volume(gio::Volume),
    Server(Server),
}

struct Place {
    icon: &'static str,
    label: String,
    detail: String,
    target: Target,
}

fn path_place(icon: &'static str, label: impl Into<String>, path: PathBuf) -> Place {
    let mut detail = path.to_string_lossy().into_owned();
    if let Some(space) = sys::disk_space(&path) {
        detail.push_str(&format!("   ·   volno {}", format_bytes(space.available)));
    }
    Place {
        icon,
        label: label.into(),
        detail,
        target: Target::Path(path),
    }
}

/// Bookmarks shared with Nautilus/GTK file choosers: `file:///path optional label` lines.
fn gtk_bookmarks() -> Vec<(String, PathBuf)> {
    let file = glib::user_config_dir().join("gtk-3.0").join("bookmarks");
    let Ok(text) = fs::read_to_string(file) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| {
            let (uri, label) = line.split_once(' ').unwrap_or((line, ""));
            let path = gio::File::for_uri(uri).path()?;
            let label = if label.is_empty() {
                path.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.to_string_lossy().into_owned())
            } else {
                label.to_owned()
            };
            Some((label, path))
        })
        .collect()
}

fn collect_places(hotpaths: &[Option<PathBuf>], history: &[PathBuf], servers: Vec<Server>) -> Vec<(String, Vec<Place>)> {
    let mut sections = Vec::new();

    let mut basic = Vec::new();
    if let Some(home) = env::var_os("HOME").map(PathBuf::from) {
        basic.push(path_place("user-home-symbolic", "Domovská složka", home));
    }
    for (directory, icon) in [
        (glib::UserDirectory::Desktop, "user-desktop-symbolic"),
        (glib::UserDirectory::Documents, "folder-documents-symbolic"),
        (glib::UserDirectory::Downloads, "folder-download-symbolic"),
        (glib::UserDirectory::Pictures, "folder-pictures-symbolic"),
    ] {
        if let Some(path) = glib::user_special_dir(directory).filter(|path| path.is_dir()) {
            let label = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            basic.push(path_place(icon, label, path));
        }
    }
    basic.push(path_place("drive-harddisk-symbolic", "Kořen systému /", PathBuf::from("/")));
    let trash = glib::user_data_dir().join("Trash").join("files");
    if trash.is_dir() {
        basic.push(path_place("user-trash-symbolic", "Koš", trash));
    }
    sections.push(("Místa".to_owned(), basic));

    let monitor = gio::VolumeMonitor::get();
    let mut volumes = Vec::new();
    let mut seen = Vec::new();
    for mount in monitor.mounts() {
        if mount.is_shadowed() {
            continue;
        }
        let Some(path) = mount.root().path() else {
            continue;
        };
        seen.push(path.clone());
        volumes.push(path_place("drive-removable-media-symbolic", mount.name(), path));
    }
    for volume in monitor.volumes() {
        if volume.get_mount().is_some() || !volume.can_mount() {
            continue;
        }
        volumes.push(Place {
            icon: "drive-removable-media-symbolic",
            label: volume.name().to_string(),
            detail: "nepřipojeno — Enter připojí".to_owned(),
            target: Target::Volume(volume),
        });
    }
    if !volumes.is_empty() {
        sections.push(("Svazky".to_owned(), volumes));
    }

    let servers: Vec<Place> = servers
        .into_iter()
        .map(|server| {
            let connected = crate::remote::conn::get(&server.id).is_some();
            Place {
                icon: "network-server-symbolic",
                label: server.display_name(),
                detail: format!("sftp://{}{}", server.label(), if connected { "   ·   připojeno" } else { "" }),
                target: Target::Server(server),
            }
        })
        .collect();
    if !servers.is_empty() {
        sections.push(("Servery SSH (Ctrl+K spravuje)".to_owned(), servers));
    }

    let bookmarks: Vec<Place> = gtk_bookmarks()
        .into_iter()
        .filter(|(_, path)| !seen.contains(path))
        .map(|(label, path)| path_place("user-bookmarks-symbolic", label, path))
        .collect();
    if !bookmarks.is_empty() {
        sections.push(("Záložky".to_owned(), bookmarks));
    }

    let hot: Vec<Place> = hotpaths
        .iter()
        .enumerate()
        .filter_map(|(index, path)| {
            path.as_ref()
                .map(|path| path_place("starred-symbolic", format!("Ctrl+{index}"), path.clone()))
        })
        .collect();
    if !hot.is_empty() {
        sections.push(("Klávesové cesty (Ctrl+Shift+číslo uloží)".to_owned(), hot));
    }

    let mut recent: Vec<PathBuf> = Vec::new();
    for path in history.iter().rev() {
        if !recent.contains(path) {
            recent.push(path.clone());
        }
    }
    let recent: Vec<Place> = recent
        .into_iter()
        .take(15)
        .map(|path| Place {
            icon: "document-open-recent-symbolic",
            label: path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "/".to_owned()),
            detail: path.to_string_lossy().into_owned(),
            target: Target::Path(path),
        })
        .collect();
    if !recent.is_empty() {
        sections.push(("Historie".to_owned(), recent));
    }
    sections
}

pub fn open(
    parent: &gtk::ApplicationWindow,
    title: &str,
    hotpaths: &[Option<PathBuf>],
    history: &[PathBuf],
    servers: Vec<Server>,
    on_choose: impl Fn(PathBuf) + 'static,
    on_server: impl Fn(Server) + 'static,
) {
    let window = gtk::Window::builder()
        .transient_for(parent)
        .modal(true)
        .title(title)
        .default_width(520)
        .default_height(560)
        .build();
    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::Browse);
    let scrolled = gtk::ScrolledWindow::new();
    scrolled.set_child(Some(&list));
    scrolled.set_vexpand(true);
    window.set_child(Some(&scrolled));

    let mut targets: Vec<Option<Target>> = Vec::new();
    for (title, places) in collect_places(hotpaths, history, servers) {
        let header = gtk::Label::new(Some(&title));
        header.set_xalign(0.0);
        header.add_css_class("heading");
        header.set_margin_top(8);
        header.set_margin_start(8);
        let header_row = gtk::ListBoxRow::new();
        header_row.set_child(Some(&header));
        header_row.set_activatable(false);
        header_row.set_selectable(false);
        list.append(&header_row);
        targets.push(None);

        for place in places {
            let row_box = gtk::Box::new(gtk::Orientation::Horizontal, 10);
            row_box.set_margin_top(4);
            row_box.set_margin_bottom(4);
            row_box.set_margin_start(12);
            row_box.set_margin_end(8);
            row_box.append(&gtk::Image::from_icon_name(place.icon));
            let texts = gtk::Box::new(gtk::Orientation::Vertical, 0);
            let label = gtk::Label::new(Some(&place.label));
            label.set_xalign(0.0);
            let detail = gtk::Label::new(Some(&place.detail));
            detail.set_xalign(0.0);
            detail.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
            detail.add_css_class("dim-label");
            detail.add_css_class("caption");
            texts.append(&label);
            texts.append(&detail);
            row_box.append(&texts);
            list.append(&row_box);
            targets.push(Some(place.target));
        }
    }
    if let Some(first) = (0..targets.len())
        .find(|index| targets[*index].is_some())
        .and_then(|index| list.row_at_index(index as i32))
    {
        list.select_row(Some(&first));
    }

    let targets = Rc::new(RefCell::new(targets));
    let on_choose = Rc::new(on_choose);
    let weak_window = window.downgrade();
    list.connect_row_activated(move |_, row| {
        let Some(window) = weak_window.upgrade() else {
            return;
        };
        let Ok(index) = usize::try_from(row.index()) else {
            return;
        };
        let target = targets.borrow_mut().get_mut(index).and_then(Option::take);
        match target {
            Some(Target::Path(path)) => {
                window.close();
                on_choose(path);
            }
            Some(Target::Server(server)) => {
                window.close();
                on_server(server);
            }
            Some(Target::Volume(volume)) => {
                let operation = gtk::MountOperation::new(Some(&window));
                let on_choose = Rc::clone(&on_choose);
                let weak_window = window.downgrade();
                let mounted = volume.clone();
                volume.mount(
                    gio::MountMountFlags::NONE,
                    Some(&operation),
                    gio::Cancellable::NONE,
                    move |result| {
                        let Some(window) = weak_window.upgrade() else {
                            return;
                        };
                        window.close();
                        let path = mounted
                            .get_mount()
                            .and_then(|mount| mount.root().path());
                        match (result, path) {
                            (Ok(()), Some(path)) => on_choose(path),
                            (Err(error), _) => show_error(&window, &format!("Připojení selhalo: {error}")),
                            (Ok(()), None) => show_error(&window, "Svazek nemá místní cestu"),
                        }
                    },
                );
            }
            None => {}
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
    window.present();
    list.grab_focus();
    if let Some(row) = list.selected_row() {
        row.grab_focus();
    }
}

fn show_error(window: &gtk::Window, message: &str) {
    let parent = window.transient_for();
    gtk::AlertDialog::builder()
        .modal(true)
        .message(message)
        .build()
        .show(parent.as_ref());
}

