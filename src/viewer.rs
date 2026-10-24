use std::cell::{Cell, RefCell};
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::Path;
use std::rc::Rc;

use gtk::glib;
use gtk::prelude::*;

use crate::model::format_bytes;

const MAX_BYTES: usize = 10 * 1024 * 1024;
const MAX_HEX_BYTES: usize = 64 * 1024;

struct Rendered {
    text: String,
    status: &'static str,
}

struct ViewerState {
    data: RefCell<Vec<u8>>,
    hex: Cell<bool>,
    file_size: u64,
}

/// Opens the internal viewer. Keys: Esc closes, Ctrl+F/F7 search, F3/Shift+F3 next/previous,
/// H toggles text/hex, W toggles line wrapping.
pub fn open(parent: &gtk::ApplicationWindow, path: &Path) {
    let file_size = match fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => metadata.len(),
        Ok(_) => {
            show_error(
                parent,
                &format!(
                    "Nelze prohlédnout {}: není běžný soubor",
                    path.to_string_lossy()
                ),
            );
            return;
        }
        Err(error) => {
            show_error(
                parent,
                &format!("Nelze otevřít {}: {error}", path.to_string_lossy()),
            );
            return;
        }
    };

    let window = gtk::Window::builder()
        .transient_for(parent)
        .modal(false)
        .title(format!("Prohlížeč — {}", path.to_string_lossy()))
        .default_width(900)
        .default_height(650)
        .build();
    let content = gtk::Box::new(gtk::Orientation::Vertical, 6);
    content.set_margin_top(6);
    content.set_margin_bottom(6);
    content.set_margin_start(6);
    content.set_margin_end(6);

    let search_box = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let search_entry = gtk::SearchEntry::new();
    search_entry.set_hexpand(true);
    search_entry.set_placeholder_text(Some("Hledat (Enter/F3 další, Shift+F3 předchozí)"));
    let previous = gtk::Button::from_icon_name("go-up-symbolic");
    previous.set_tooltip_text(Some("Předchozí (Shift+F3)"));
    let next = gtk::Button::from_icon_name("go-down-symbolic");
    next.set_tooltip_text(Some("Další (F3)"));
    search_box.append(&search_entry);
    search_box.append(&previous);
    search_box.append(&next);
    let revealer = gtk::Revealer::new();
    revealer.set_child(Some(&search_box));
    content.append(&revealer);

    let text_view = gtk::TextView::new();
    text_view.set_editable(false);
    text_view.set_cursor_visible(true);
    text_view.set_monospace(true);
    let buffer = text_view.buffer();
    let scrolled = gtk::ScrolledWindow::new();
    scrolled.set_vexpand(true);
    scrolled.set_child(Some(&text_view));
    content.append(&scrolled);

    let status = gtk::Label::new(Some("Načítám…"));
    status.set_xalign(0.0);
    content.append(&status);
    window.set_child(Some(&content));

    let state = Rc::new(ViewerState {
        data: RefCell::new(Vec::new()),
        hex: Cell::new(false),
        file_size,
    });

    let render = {
        let state = Rc::clone(&state);
        let buffer = buffer.downgrade();
        let status = status.downgrade();
        let text_view = text_view.downgrade();
        move || {
            let (Some(buffer), Some(status), Some(text_view)) =
                (buffer.upgrade(), status.upgrade(), text_view.upgrade())
            else {
                return;
            };
            let data = state.data.borrow();
            let rendered = if state.hex.get() {
                Rendered {
                    text: hex_dump(&data[..data.len().min(MAX_HEX_BYTES)]),
                    status: "Hexadecimální výpis",
                }
            } else {
                render_content(&data)
            };
            buffer.set_text(&rendered.text);
            buffer.place_cursor(&buffer.start_iter());
            let wrap = match text_view.wrap_mode() {
                gtk::WrapMode::None => "",
                _ => ", zalamování",
            };
            status.set_text(&format!(
                "{} — {}{wrap}   (H text/hex, W zalamování, Ctrl+F hledat, Esc zavřít)",
                rendered.status,
                format_bytes(state.file_size)
            ));
        }
    };
    let render = Rc::new(render);

    let find = {
        let buffer = buffer.downgrade();
        let text_view = text_view.downgrade();
        let search_entry = search_entry.downgrade();
        move |forward: bool| {
            let (Some(buffer), Some(text_view), Some(search_entry)) =
                (buffer.upgrade(), text_view.upgrade(), search_entry.upgrade())
            else {
                return;
            };
            let needle = search_entry.text();
            if needle.is_empty() {
                return;
            }
            let flags = gtk::TextSearchFlags::CASE_INSENSITIVE | gtk::TextSearchFlags::TEXT_ONLY;
            let (start, end) = buffer
                .selection_bounds()
                .unwrap_or_else(|| {
                    let cursor = buffer.iter_at_mark(&buffer.get_insert());
                    (cursor, cursor)
                });
            let found = if forward {
                end.forward_search(&needle, flags, None)
                    .or_else(|| buffer.start_iter().forward_search(&needle, flags, None))
            } else {
                start
                    .backward_search(&needle, flags, None)
                    .or_else(|| buffer.end_iter().backward_search(&needle, flags, None))
            };
            match found {
                Some((mut match_start, match_end)) => {
                    buffer.select_range(&match_start, &match_end);
                    text_view.scroll_to_iter(&mut match_start, 0.1, false, 0.0, 0.0);
                }
                None => text_view.error_bell(),
            }
        }
    };
    let find = Rc::new(find);

    {
        let find = Rc::clone(&find);
        search_entry.connect_activate(move |_| find(true));
    }
    {
        let find = Rc::clone(&find);
        next.connect_clicked(move |_| find(true));
    }
    {
        let find = Rc::clone(&find);
        previous.connect_clicked(move |_| find(false));
    }

    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    {
        let weak_window = window.downgrade();
        let revealer = revealer.downgrade();
        let search_entry = search_entry.downgrade();
        let text_view_weak = text_view.downgrade();
        let state = Rc::clone(&state);
        let render = Rc::clone(&render);
        let find = Rc::clone(&find);
        keys.connect_key_pressed(move |_, key, _, modifiers| {
            use gtk::gdk::{Key, ModifierType};
            let ctrl = modifiers.contains(ModifierType::CONTROL_MASK);
            let shift = modifiers.contains(ModifierType::SHIFT_MASK);
            let in_search = search_entry
                .upgrade()
                .is_some_and(|entry| entry.has_focus() || entry.focus_child().is_some());
            match key {
                Key::Escape => {
                    if in_search {
                        if let (Some(revealer), Some(view)) =
                            (revealer.upgrade(), text_view_weak.upgrade())
                        {
                            revealer.set_reveal_child(false);
                            view.grab_focus();
                        }
                    } else if let Some(window) = weak_window.upgrade() {
                        window.close();
                    }
                    glib::Propagation::Stop
                }
                Key::f if ctrl => {
                    show_search(&revealer, &search_entry);
                    glib::Propagation::Stop
                }
                Key::F7 => {
                    show_search(&revealer, &search_entry);
                    glib::Propagation::Stop
                }
                Key::F3 => {
                    find(!shift);
                    glib::Propagation::Stop
                }
                Key::h | Key::H if !ctrl && !in_search => {
                    state.hex.set(!state.hex.get());
                    render();
                    glib::Propagation::Stop
                }
                Key::w | Key::W if !ctrl && !in_search => {
                    if let Some(view) = text_view_weak.upgrade() {
                        view.set_wrap_mode(match view.wrap_mode() {
                            gtk::WrapMode::None => gtk::WrapMode::WordChar,
                            _ => gtk::WrapMode::None,
                        });
                    }
                    render();
                    glib::Propagation::Stop
                }
                _ => glib::Propagation::Proceed,
            }
        });
    }
    window.add_controller(keys);

    let path = path.to_path_buf();
    let error_path = path.clone();
    let (sender, receiver) = async_channel::bounded(1);
    std::thread::spawn(move || {
        let result = File::open(&path).and_then(read_limited);
        let _ = sender.send_blocking(result);
    });

    let weak_window = window.downgrade();
    let weak_parent = parent.downgrade();
    glib::spawn_future_local(async move {
        let Ok(result) = receiver.recv().await else {
            return;
        };
        let Some(window) = weak_window.upgrade() else {
            return;
        };
        match result {
            Ok(data) => {
                // Text mode falls back to a hex dump by itself for binary data.
                *state.data.borrow_mut() = data;
                render();
            }
            Err(error) => {
                window.close();
                if let Some(parent) = weak_parent.upgrade() {
                    show_error(
                        &parent,
                        &format!(
                            "Nelze přečíst {}: {error}",
                            error_path.to_string_lossy()
                        ),
                    );
                }
            }
        }
    });

    window.present();
    text_view.grab_focus();
}

fn show_search(revealer: &glib::WeakRef<gtk::Revealer>, entry: &glib::WeakRef<gtk::SearchEntry>) {
    if let (Some(revealer), Some(entry)) = (revealer.upgrade(), entry.upgrade()) {
        revealer.set_reveal_child(true);
        entry.grab_focus();
    }
}

fn read_limited(reader: impl Read) -> io::Result<Vec<u8>> {
    let mut data = Vec::new();
    reader
        .take((MAX_BYTES + 1) as u64)
        .read_to_end(&mut data)?;
    Ok(data)
}

fn render_content(data: &[u8]) -> Rendered {
    let truncated = data.len() > MAX_BYTES;
    let data = &data[..data.len().min(MAX_BYTES)];
    let status = if truncated {
        "Zobrazeno prvních 10 MiB"
    } else if data.contains(&0) || std::str::from_utf8(data).is_err() {
        "Hexadecimální výpis"
    } else {
        "Textový soubor"
    };

    if !data.contains(&0) {
        match std::str::from_utf8(data) {
            Ok(text) => {
                return Rendered {
                    text: text.to_owned(),
                    status,
                };
            }
            Err(error) if truncated && error.error_len().is_none() => {
                let prefix = std::str::from_utf8(&data[..error.valid_up_to()])
                    .expect("UTF-8 prefix reported valid by Utf8Error");
                return Rendered {
                    text: prefix.to_owned(),
                    status,
                };
            }
            Err(_) => {}
        }
    }

    Rendered {
        text: hex_dump(&data[..data.len().min(MAX_HEX_BYTES)]),
        status,
    }
}

fn hex_dump(data: &[u8]) -> String {
    let mut output = String::new();
    for (line, bytes) in data.chunks(16).enumerate() {
        use std::fmt::Write;

        let _ = write!(output, "{:08x}  ", line * 16);
        for index in 0..16 {
            if let Some(byte) = bytes.get(index) {
                let _ = write!(output, "{byte:02x} ");
            } else {
                output.push_str("   ");
            }
            if index == 7 {
                output.push(' ');
            }
        }
        output.push_str(" |");
        for byte in bytes {
            output.push(if (0x20..=0x7e).contains(byte) {
                char::from(*byte)
            } else {
                '.'
            });
        }
        output.push_str("|\n");
    }
    output
}

fn show_error(parent: &gtk::ApplicationWindow, message: &str) {
    gtk::AlertDialog::builder()
        .modal(true)
        .message(message)
        .build()
        .show(Some(parent));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_dump_contains_offset_bytes_and_ascii() {
        let rendered = render_content(&[0x00, 0x41, 0xff]);
        assert!(rendered.text.contains("00000000"));
        assert!(rendered.text.contains("00 41 ff"));
        assert!(rendered.text.contains(".A."));
    }

    #[test]
    fn reader_is_limited_to_ten_mib_plus_one() {
        let input = io::repeat(b'x').take((MAX_BYTES + 2) as u64);
        let data = read_limited(input).unwrap();
        assert_eq!(data.len(), MAX_BYTES + 1);
        assert!(data.iter().all(|byte| *byte == b'x'));
    }

    #[test]
    fn valid_utf8_is_displayed_as_text() {
        let rendered = render_content("Příliš žluťoučký kůň\n".as_bytes());
        assert_eq!(rendered.text, "Příliš žluťoučký kůň\n");
        assert_eq!(rendered.status, "Textový soubor");
    }

    #[test]
    fn truncated_multibyte_sequence_at_limit_uses_valid_prefix() {
        let mut data = vec![b'a'; MAX_BYTES - 1];
        data.extend_from_slice(&[0xe2, 0x82]);
        let rendered = render_content(&data);
        assert_eq!(rendered.text.len(), MAX_BYTES - 1);
        assert!(rendered.text.bytes().all(|byte| byte == b'a'));
        assert_eq!(rendered.status, "Zobrazeno prvních 10 MiB");
    }
}
