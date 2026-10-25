//! Dialogs for packing (Alt+F5) and unpacking (Alt+F9) archives.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gtk::glib;
use gtk::prelude::*;

use crate::archive::PackFormat;

fn dialog_window(parent: &gtk::ApplicationWindow, title: &str) -> (gtk::Window, gtk::Box) {
    let window = gtk::Window::builder()
        .transient_for(parent)
        .modal(true)
        .title(title)
        .default_width(560)
        .resizable(false)
        .build();
    let content = gtk::Box::new(gtk::Orientation::Vertical, 10);
    content.set_margin_top(18);
    content.set_margin_bottom(18);
    content.set_margin_start(18);
    content.set_margin_end(18);
    window.set_child(Some(&content));
    let keys = gtk::EventControllerKey::new();
    let weak = window.downgrade();
    keys.connect_key_pressed(move |_, key, _, _| {
        if key == gtk::gdk::Key::Escape {
            if let Some(window) = weak.upgrade() {
                window.close();
            }
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    });
    window.add_controller(keys);
    (window, content)
}

fn buttons(window: &gtk::Window, confirm_label: &str) -> (gtk::Box, gtk::Button) {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    row.set_halign(gtk::Align::End);
    let cancel = gtk::Button::with_mnemonic("_Zrušit");
    let confirm = gtk::Button::with_mnemonic(confirm_label);
    confirm.add_css_class("suggested-action");
    row.append(&cancel);
    row.append(&confirm);
    let weak = window.downgrade();
    cancel.connect_clicked(move |_| {
        if let Some(window) = weak.upgrade() {
            window.close();
        }
    });
    window.set_default_widget(Some(&confirm));
    (row, confirm)
}

/// Strips any known archive extension, e.g. "data.tar.gz" → "data".
pub fn strip_archive_extension(name: &str) -> &str {
    let lower = name.to_lowercase();
    for extension in [
        ".tar.gz", ".tar.bz2", ".tar.xz", ".tar.zst", ".tar.lz", ".tar.lzma", ".tar.lz4",
    ] {
        if lower.ends_with(extension) {
            return &name[..name.len() - extension.len()];
        }
    }
    match name.rfind('.') {
        Some(index) if index > 0 => &name[..index],
        _ => name,
    }
}

const LAST_FORMAT_KEY: &str = "salamander-rs-last-pack-format";

/// Asks for the archive path and format. `default_name` has no extension.
pub fn pack_dialog(
    parent: &gtk::ApplicationWindow,
    count: usize,
    dest_dir: &Path,
    default_name: &str,
    on_confirm: impl FnOnce(PathBuf, PackFormat) + 'static,
) {
    let (window, content) = dialog_window(parent, "Zabalit");
    let label = gtk::Label::new(Some(&format!("Zabalit {count} položek do archivu:")));
    label.set_xalign(0.0);
    content.append(&label);

    // Remember the last format for the lifetime of the main window.
    let last = unsafe { parent.data::<u32>(LAST_FORMAT_KEY).map(|value| *value.as_ref()) }.unwrap_or(0);
    let format_index = last.min(PackFormat::ALL.len() as u32 - 1);
    let initial_format = PackFormat::ALL[format_index as usize];
    let entry = gtk::Entry::new();
    entry.set_text(&dest_dir.join(format!("{default_name}{}", initial_format.extension())).to_string_lossy());
    entry.set_activates_default(true);
    content.append(&entry);
    let focus = entry.clone();

    let labels: Vec<&str> = PackFormat::ALL.iter().map(|format| format.label()).collect();
    let formats = gtk::DropDown::from_strings(&labels);
    formats.set_selected(format_index);
    let format_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let format_label = gtk::Label::with_mnemonic("_Formát:");
    format_label.set_mnemonic_widget(Some(&formats));
    format_row.append(&format_label);
    format_row.append(&formats);
    content.append(&format_row);

    // Switching the format swaps the extension in the file name.
    let previous = Rc::new(RefCell::new(initial_format));
    let weak_entry = entry.downgrade();
    let previous_format = Rc::clone(&previous);
    formats.connect_selected_notify(move |dropdown| {
        let Some(entry) = weak_entry.upgrade() else {
            return;
        };
        let format = PackFormat::ALL[dropdown.selected() as usize];
        let old = std::mem::replace(&mut *previous_format.borrow_mut(), format);
        let text = entry.text().to_string();
        let base = text.strip_suffix(old.extension()).unwrap_or(&text);
        entry.set_text(&format!("{base}{}", format.extension()));
    });

    let (row, confirm) = buttons(&window, "_Zabalit");
    content.append(&row);
    let callback = Rc::new(RefCell::new(Some(on_confirm)));
    let weak_window = window.downgrade();
    let weak_parent = parent.downgrade();
    confirm.connect_clicked(move |_| {
        let Some(window) = weak_window.upgrade() else {
            return;
        };
        let format = PackFormat::ALL[formats.selected() as usize];
        if let Some(parent) = weak_parent.upgrade() {
            unsafe { parent.set_data(LAST_FORMAT_KEY, formats.selected()) };
        }
        let path = PathBuf::from(entry.text().as_str());
        window.close();
        if let Some(callback) = callback.borrow_mut().take() {
            callback(path, format);
        }
    });
    window.present();
    focus.grab_focus();
}

pub struct UnpackOptions {
    pub dest: PathBuf,
    pub subfolder: bool,
    pub overwrite: bool,
}

/// Asks where to unpack. `subfolder_default` suggests one folder per archive.
pub fn unpack_dialog(
    parent: &gtk::ApplicationWindow,
    title: &str,
    message: &str,
    dest: &Path,
    offer_subfolder: bool,
    confirm_label: &str,
    on_confirm: impl FnOnce(UnpackOptions) + 'static,
) {
    let (window, content) = dialog_window(parent, title);
    let label = gtk::Label::new(Some(message));
    label.set_xalign(0.0);
    label.set_wrap(true);
    content.append(&label);
    let entry = gtk::Entry::new();
    entry.set_text(&dest.to_string_lossy());
    entry.set_activates_default(true);
    content.append(&entry);
    let focus = entry.clone();
    let subfolder = gtk::CheckButton::with_mnemonic("Každý archiv do _vlastní složky");
    subfolder.set_active(true);
    if offer_subfolder {
        content.append(&subfolder);
    }
    let overwrite = gtk::CheckButton::with_mnemonic("_Přepsat existující soubory");
    content.append(&overwrite);
    let (row, confirm) = buttons(&window, confirm_label);
    content.append(&row);

    let callback = Rc::new(RefCell::new(Some(on_confirm)));
    let weak_window = window.downgrade();
    confirm.connect_clicked(move |_| {
        let Some(window) = weak_window.upgrade() else {
            return;
        };
        let options = UnpackOptions {
            dest: PathBuf::from(entry.text().as_str()),
            subfolder: offer_subfolder && subfolder.is_active(),
            overwrite: overwrite.is_active(),
        };
        window.close();
        if let Some(callback) = callback.borrow_mut().take() {
            callback(options);
        }
    });
    window.present();
    focus.grab_focus();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_archive_extensions() {
        assert_eq!(strip_archive_extension("data.tar.gz"), "data");
        assert_eq!(strip_archive_extension("Data.TAR.XZ"), "Data");
        assert_eq!(strip_archive_extension("photos.zip"), "photos");
        assert_eq!(strip_archive_extension(".hidden"), ".hidden");
        assert_eq!(strip_archive_extension("noext"), "noext");
    }
}
