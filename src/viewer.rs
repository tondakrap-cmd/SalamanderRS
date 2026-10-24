use std::fs::{self, File};
use std::io::{self, Read};
use std::path::Path;

use gtk::glib;
use gtk::prelude::*;

const MAX_BYTES: usize = 10 * 1024 * 1024;
const MAX_HEX_BYTES: usize = 64 * 1024;

struct Rendered {
    text: String,
    status: &'static str,
}

pub fn open(parent: &gtk::ApplicationWindow, path: &Path) {
    match fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => {}
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
    }

    let window = gtk::Window::builder()
        .transient_for(parent)
        .modal(false)
        .title(format!("Prohlížeč — {}", path.to_string_lossy()))
        .default_width(800)
        .default_height(600)
        .build();
    let content = gtk::Box::new(gtk::Orientation::Vertical, 6);
    content.set_margin_top(6);
    content.set_margin_bottom(6);
    content.set_margin_start(6);
    content.set_margin_end(6);

    let text_view = gtk::TextView::new();
    text_view.set_editable(false);
    text_view.set_cursor_visible(false);
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

    let path = path.to_path_buf();
    let error_path = path.clone();
    let (sender, receiver) = async_channel::bounded(1);
    std::thread::spawn(move || {
        let result = File::open(&path).and_then(read_limited);
        let _ = sender.send_blocking(result);
    });

    let weak_window = window.downgrade();
    let weak_buffer = buffer.downgrade();
    let weak_status = status.downgrade();
    let weak_parent = parent.downgrade();
    glib::spawn_future_local(async move {
        let Ok(result) = receiver.recv().await else {
            return;
        };
        let (Some(window), Some(buffer), Some(status)) = (
            weak_window.upgrade(),
            weak_buffer.upgrade(),
            weak_status.upgrade(),
        ) else {
            return;
        };
        match result {
            Ok(data) => {
                let rendered = render_content(&data);
                buffer.set_text(&rendered.text);
                status.set_text(rendered.status);
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
