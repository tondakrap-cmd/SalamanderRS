mod dialogs;
mod model;
mod ops;
mod panel;
mod viewer;
mod window;

use gtk::prelude::*;

fn main() -> gtk::glib::ExitCode {
    let app = gtk::Application::builder()
        .application_id("cz.k4.salamander-rs")
        .build();
    app.connect_activate(window::build);
    app.run()
}
