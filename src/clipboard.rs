//! File clipboard compatible with Nautilus, PCManFM-Qt, Dolphin and others.
//!
//! Copy publishes `x-special/gnome-copied-files` (GNOME/LXQt), `text/uri-list` with the KDE
//! cut marker, and plain text paths. Paste reads the first of those that is available.

use std::path::{Path, PathBuf};

use gtk::gdk;
use gtk::gio;
use gtk::glib;
use gtk::prelude::*;

const GNOME_MIME: &str = "x-special/gnome-copied-files";
const URI_LIST_MIME: &str = "text/uri-list";
const KDE_CUT_MIME: &str = "application/x-kde-cutselection";

fn path_to_uri(path: &Path) -> String {
    gio::File::for_path(path).uri().to_string()
}

fn uri_to_path(uri: &str) -> Option<PathBuf> {
    uri.starts_with("file://")
        .then(|| gio::File::for_uri(uri).path())
        .flatten()
}

pub fn copy_files(clipboard: &gdk::Clipboard, paths: &[PathBuf], cut: bool) {
    let uris: Vec<String> = paths.iter().map(|path| path_to_uri(path)).collect();
    let gnome = format!("{}\n{}", if cut { "cut" } else { "copy" }, uris.join("\n"));
    let uri_list = format!("{}\r\n", uris.join("\r\n"));
    let text = paths
        .iter()
        .map(|path| path.to_string_lossy())
        .collect::<Vec<_>>()
        .join("\n");

    let mut providers = vec![
        gdk::ContentProvider::for_bytes(GNOME_MIME, &glib::Bytes::from_owned(gnome.into_bytes())),
        gdk::ContentProvider::for_bytes(
            URI_LIST_MIME,
            &glib::Bytes::from_owned(uri_list.into_bytes()),
        ),
        gdk::ContentProvider::for_value(&text.to_value()),
    ];
    if cut {
        providers.push(gdk::ContentProvider::for_bytes(
            KDE_CUT_MIME,
            &glib::Bytes::from_static(b"1"),
        ));
    }
    let _ = clipboard.set_content(Some(&gdk::ContentProvider::new_union(&providers)));
}

pub fn copy_text(clipboard: &gdk::Clipboard, text: &str) {
    clipboard.set_text(text);
}

#[derive(Debug, Default, PartialEq)]
pub struct Pasted {
    pub paths: Vec<PathBuf>,
    pub cut: bool,
}

pub fn parse_gnome(text: &str) -> Pasted {
    let mut lines = text.lines().map(|line| line.trim_end_matches('\r'));
    let cut = lines.next() == Some("cut");
    Pasted {
        paths: lines.filter_map(uri_to_path).collect(),
        cut,
    }
}

pub fn parse_uri_list(text: &str, cut: bool) -> Pasted {
    Pasted {
        paths: text
            .lines()
            .map(|line| line.trim_end_matches('\r'))
            .filter(|line| !line.starts_with('#'))
            .filter_map(uri_to_path)
            .collect(),
        cut,
    }
}

async fn read_mime(clipboard: &gdk::Clipboard, mime: &str) -> Result<Vec<u8>, glib::Error> {
    let (stream, _) = clipboard
        .read_future(&[mime], glib::Priority::DEFAULT)
        .await?;
    let mut data = Vec::new();
    loop {
        let bytes = stream
            .read_bytes_future(64 * 1024, glib::Priority::DEFAULT)
            .await?;
        if bytes.is_empty() {
            break;
        }
        data.extend_from_slice(&bytes);
    }
    Ok(data)
}

pub async fn paste_files(clipboard: &gdk::Clipboard) -> Result<Pasted, String> {
    let formats = clipboard.formats();
    if formats.contain_mime_type(GNOME_MIME) {
        let data = read_mime(clipboard, GNOME_MIME)
            .await
            .map_err(|error| error.to_string())?;
        return Ok(parse_gnome(&String::from_utf8_lossy(&data)));
    }
    if formats.contain_mime_type(URI_LIST_MIME) {
        let cut = if formats.contain_mime_type(KDE_CUT_MIME) {
            read_mime(clipboard, KDE_CUT_MIME)
                .await
                .is_ok_and(|data| data.first() == Some(&b'1'))
        } else {
            false
        };
        let data = read_mime(clipboard, URI_LIST_MIME)
            .await
            .map_err(|error| error.to_string())?;
        return Ok(parse_uri_list(&String::from_utf8_lossy(&data), cut));
    }
    Err("Schránka neobsahuje soubory".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_gnome_cut_with_escaped_names() {
        let pasted = parse_gnome("cut\nfile:///tmp/a%20b.txt\nfile:///home/u/%C5%BElu%C5%A5\n");
        assert!(pasted.cut);
        assert_eq!(
            pasted.paths,
            vec![PathBuf::from("/tmp/a b.txt"), PathBuf::from("/home/u/žluť")]
        );
    }

    #[test]
    fn parses_uri_list_skipping_comments_and_remote_uris() {
        let pasted = parse_uri_list("# comment\r\nfile:///etc/hosts\r\nhttps://example.com/x\r\n", false);
        assert!(!pasted.cut);
        assert_eq!(pasted.paths, vec![PathBuf::from("/etc/hosts")]);
    }

    #[test]
    fn uri_round_trip() {
        let path = Path::new("/tmp/with space/#hash?.txt");
        assert_eq!(uri_to_path(&path_to_uri(path)).as_deref(), Some(path));
    }
}
