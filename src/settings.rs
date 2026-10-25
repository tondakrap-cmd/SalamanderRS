//! Settings window (Nástroje → Nastavení…).
//!
//! Every option is a stateful window action (`win.<name>`), so the check buttons here, the
//! menus and the configuration stay in sync, and a change applies immediately.

use gtk::prelude::*;

use crate::dialogs::dialog_window;

/// One section of the window: a title and its check buttons as `(label, action, hint)`.
struct Section {
    id: &'static str,
    title: &'static str,
    groups: &'static [(&'static str, &'static [(&'static str, &'static str, &'static str)])],
}

const SECTIONS: &[Section] = &[Section {
    id: "panels",
    title: "Panely",
    groups: &[
        (
            "Procházení",
            &[(
                "Zobrazovat položku „..“ pro přechod do nadřazené složky",
                "win.parent-entry",
                "První řádek seznamu vede o úroveň výš (stejně jako Backspace).",
            )],
        ),
        (
            "Sloupce",
            &[
                ("Přípona", "win.column-ext", ""),
                ("Práva", "win.column-permissions", ""),
                ("Vlastník a skupina", "win.column-owner", ""),
            ],
        ),
    ],
}];

pub fn open(parent: &gtk::ApplicationWindow) {
    let (window, content) = dialog_window(parent, "Nastavení", 620);
    window.set_resizable(true);
    window.set_default_height(380);
    // The check buttons drive the main window's actions.
    window.insert_action_group("win", Some(parent));

    let stack = gtk::Stack::new();
    stack.set_hexpand(true);
    stack.set_vexpand(true);
    for section in SECTIONS {
        stack.add_titled(&section_page(section), Some(section.id), section.title);
    }
    let sidebar = gtk::StackSidebar::new();
    sidebar.set_stack(&stack);
    sidebar.set_size_request(150, -1);

    let body = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    body.append(&sidebar);
    body.append(&stack);
    content.append(&body);

    let close = gtk::Button::with_mnemonic("_Zavřít");
    close.set_halign(gtk::Align::End);
    let weak_window = window.downgrade();
    close.connect_clicked(move |_| {
        if let Some(window) = weak_window.upgrade() {
            window.close();
        }
    });
    content.append(&close);
    window.present();
}

fn section_page(section: &Section) -> gtk::Widget {
    let page = gtk::Box::new(gtk::Orientation::Vertical, 6);
    page.set_margin_start(6);
    for (index, (heading, options)) in section.groups.iter().enumerate() {
        let label = gtk::Label::new(None);
        label.set_markup(&format!("<b>{}</b>", gtk::glib::markup_escape_text(heading)));
        label.set_xalign(0.0);
        if index > 0 {
            label.set_margin_top(12);
        }
        page.append(&label);
        for (text, action, hint) in options.iter() {
            let check = gtk::CheckButton::with_label(text);
            check.set_action_name(Some(action));
            check.set_margin_start(12);
            page.append(&check);
            if !hint.is_empty() {
                let hint_label = gtk::Label::new(Some(hint));
                hint_label.set_xalign(0.0);
                hint_label.set_wrap(true);
                hint_label.set_margin_start(40);
                hint_label.add_css_class("dim-label");
                page.append(&hint_label);
            }
        }
    }
    page.upcast()
}
