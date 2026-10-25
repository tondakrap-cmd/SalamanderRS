mod archive;
mod archive_ui;
mod attrs;
mod clipboard;
mod config;
mod dialogs;
mod find;
mod model;
mod ops;
mod panel;
mod places;
mod props;
mod remote;
mod remote_ui;
mod settings;
mod sodium;
mod sys;
mod terminal;
mod vault;
mod viewer;
mod window;
mod xattr;

use std::path::PathBuf;

use gtk::prelude::*;

fn main() -> gtk::glib::ExitCode {
    // ssh runs this binary as its SSH_ASKPASS helper; answer and exit before starting GTK.
    if let Some(code) = remote::askpass::askpass_main() {
        std::process::exit(code);
    }
    // `salamander-rs DIR` (or a file:// URI from a file manager) opens DIR in the left panel.
    let open_dir = std::env::args_os().nth(1).map(|arg| {
        let text = arg.to_string_lossy();
        if text.starts_with("file://") {
            gtk::gio::File::for_uri(&text)
                .path()
                .unwrap_or_else(|| PathBuf::from(&arg))
        } else {
            PathBuf::from(&arg)
        }
    });
    // Directories and archives are opened directly; for other files their directory.
    // Relative paths (`salamander-rs .`) are made absolute.
    let open_dir = open_dir.map(|path| {
        let path = std::fs::canonicalize(&path).unwrap_or(path);
        if path.is_dir() || archive::looks_like_archive(&path) {
            path
        } else {
            path.parent().map(PathBuf::from).unwrap_or(path)
        }
    });
    archive::cleanup_temp();
    remote::askpass::cleanup_stale();

    let app = gtk::Application::builder()
        .application_id("cz.k4.salamander-rs")
        .flags(gtk::gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.connect_activate(move |app| window::build(app, open_dir.clone()));
    // Arguments are handled above; GTK must not interpret them as files to open.
    app.run_with_args::<&str>(&[])
}
