use std::cell::RefCell;
use std::rc::Rc;

use gtk::prelude::*;

use crate::model::OpError;

fn dialog_window(
    parent: &gtk::ApplicationWindow,
    title: &str,
    width: i32,
) -> (gtk::Window, gtk::Box) {
    let window = gtk::Window::builder()
        .transient_for(parent)
        .modal(true)
        .title(title)
        .default_width(width)
        .resizable(false)
        .build();
    let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
    content.set_margin_top(18);
    content.set_margin_bottom(18);
    content.set_margin_start(18);
    content.set_margin_end(18);
    window.set_child(Some(&content));
    (window, content)
}

fn dialog_buttons(window: &gtk::Window, confirm_label: &str) -> (gtk::Box, gtk::Button) {
    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    buttons.set_halign(gtk::Align::End);
    let cancel = gtk::Button::with_label("Zrušit");
    let confirm = gtk::Button::with_label(confirm_label);
    confirm.add_css_class("suggested-action");
    buttons.append(&cancel);
    buttons.append(&confirm);
    let weak_window = window.downgrade();
    cancel.connect_clicked(move |_| {
        if let Some(window) = weak_window.upgrade() {
            window.close();
        }
    });
    (buttons, confirm)
}

pub fn confirm(
    parent: &gtk::ApplicationWindow,
    title: &str,
    message: &str,
    confirm_label: &str,
    on_confirm: impl FnOnce() + 'static,
) {
    let (window, content) = dialog_window(parent, title, 440);
    let label = gtk::Label::new(Some(message));
    label.set_wrap(true);
    label.set_xalign(0.0);
    content.append(&label);
    let (buttons, confirm) = dialog_buttons(&window, confirm_label);
    content.append(&buttons);

    let callback = Rc::new(RefCell::new(Some(on_confirm)));
    let weak_window = window.downgrade();
    confirm.connect_clicked(move |_| {
        if let Some(window) = weak_window.upgrade() {
            window.close();
        }
        if let Some(callback) = callback.borrow_mut().take() {
            callback();
        }
    });
    window.present();
}

pub fn prompt(
    parent: &gtk::ApplicationWindow,
    title: &str,
    label: &str,
    initial: &str,
    confirm_label: &str,
    on_confirm: impl FnOnce(String) + 'static,
) {
    let (window, content) = dialog_window(parent, title, 520);
    let label_widget = gtk::Label::new(Some(label));
    label_widget.set_xalign(0.0);
    content.append(&label_widget);
    let entry = gtk::Entry::new();
    entry.set_text(initial);
    entry.set_activates_default(true);
    content.append(&entry);

    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    buttons.set_halign(gtk::Align::End);
    let cancel = gtk::Button::with_label("Zrušit");
    let confirm = gtk::Button::with_label(confirm_label);
    confirm.add_css_class("suggested-action");
    buttons.append(&cancel);
    buttons.append(&confirm);
    content.append(&buttons);
    window.set_default_widget(Some(&confirm));

    let weak_window = window.downgrade();
    cancel.connect_clicked(move |_| {
        if let Some(window) = weak_window.upgrade() {
            window.close();
        }
    });

    let callback = Rc::new(RefCell::new(Some(on_confirm)));
    let weak_window = window.downgrade();
    let weak_entry = entry.downgrade();
    confirm.connect_clicked(move |_| {
        let Some(entry) = weak_entry.upgrade() else {
            return;
        };
        let value = entry.text().to_string();
        if let Some(window) = weak_window.upgrade() {
            window.close();
        }
        if let Some(callback) = callback.borrow_mut().take() {
            callback(value);
        }
    });
    window.present();
    entry.grab_focus();
    entry.select_region(0, -1);
}

pub fn show_errors(parent: &gtk::ApplicationWindow, errors: &[OpError]) {
    if errors.is_empty() {
        return;
    }
    let (window, content) = dialog_window(parent, "Chyby operace", 680);
    let mut message = String::new();
    for error in errors.iter().take(10) {
        if !message.is_empty() {
            message.push('\n');
        }
        message.push_str(&error.source.to_string_lossy());
        if let Some(target) = &error.target {
            message.push_str(" → ");
            message.push_str(&target.to_string_lossy());
        }
        message.push_str(": ");
        message.push_str(&error.error);
    }
    if errors.len() > 10 {
        message.push_str(&format!("\n… a dalších {}", errors.len() - 10));
    }

    let label = gtk::Label::new(Some(&message));
    label.set_wrap(true);
    label.set_selectable(true);
    label.set_xalign(0.0);
    let scrolled = gtk::ScrolledWindow::new();
    scrolled.set_min_content_height(120);
    scrolled.set_max_content_height(360);
    scrolled.set_propagate_natural_height(true);
    scrolled.set_child(Some(&label));
    content.append(&scrolled);

    let close = gtk::Button::with_label("Zavřít");
    close.set_halign(gtk::Align::End);
    content.append(&close);
    let weak_window = window.downgrade();
    close.connect_clicked(move |_| {
        if let Some(window) = weak_window.upgrade() {
            window.close();
        }
    });
    window.present();
}
