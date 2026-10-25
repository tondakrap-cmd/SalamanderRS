//! Dialogs for remote servers: server manager and editor, ssh prompts (password, host key)
//! and the password vault (create, unlock, change master password).

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use gtk::glib;
use gtk::prelude::*;

use crate::remote::servers::{self, AuthMethod, Server};
use crate::sodium::Secret;
use crate::vault;

fn dialog(parent: &impl IsA<gtk::Window>, title: &str, width: i32) -> (gtk::Window, gtk::Box) {
    let window = gtk::Window::builder()
        .transient_for(parent)
        .modal(true)
        .title(title)
        .default_width(width)
        .resizable(false)
        .build();
    let content = gtk::Box::new(gtk::Orientation::Vertical, 10);
    content.set_margin_top(18);
    content.set_margin_bottom(18);
    content.set_margin_start(18);
    content.set_margin_end(18);
    window.set_child(Some(&content));
    (window, content)
}

fn label(text: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_xalign(0.0);
    label.set_wrap(true);
    label.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    label
}

fn note(text: &str) -> gtk::Label {
    let label = label(text);
    label.add_css_class("dim-label");
    label
}

fn buttons(window: &gtk::Window, confirm_label: &str) -> (gtk::Box, gtk::Button, gtk::Button) {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    row.set_halign(gtk::Align::End);
    let cancel = gtk::Button::with_mnemonic("_Zrušit");
    let confirm = gtk::Button::with_mnemonic(confirm_label);
    confirm.add_css_class("suggested-action");
    row.append(&cancel);
    row.append(&confirm);
    window.set_default_widget(Some(&confirm));
    (row, cancel, confirm)
}

/// Secret text from a password entry. The GTK buffer cannot be wiped, but it is cleared
/// right away and our copy is wiped when dropped.
fn take_secret(entry: &gtk::PasswordEntry) -> Secret {
    let secret = Secret::from_str(&entry.text());
    entry.set_text("");
    secret
}

fn password_entry() -> gtk::PasswordEntry {
    let entry = gtk::PasswordEntry::new();
    entry.set_show_peek_icon(true);
    entry.set_activates_default(true);
    entry.set_hexpand(true);
    entry
}

/// Calls `done` exactly once, also when the window is closed with Esc or the title bar.
fn once<T: 'static>(done: impl FnOnce(Option<T>) + 'static) -> Rc<RefCell<Option<Box<dyn FnOnce(Option<T>)>>>> {
    Rc::new(RefCell::new(Some(Box::new(done))))
}

fn finish<T>(callback: &Rc<RefCell<Option<Box<dyn FnOnce(Option<T>)>>>>, value: Option<T>) {
    if let Some(callback) = callback.borrow_mut().take() {
        callback(value);
    }
}

fn close_on_escape(window: &gtk::Window) {
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
}

/// Asks for a password or passphrase requested by ssh. `offer_save` shows the
/// "save to vault" check box with the given default.
pub fn ask_secret(
    parent: &impl IsA<gtk::Window>,
    title: &str,
    message: &str,
    offer_save: Option<bool>,
    done: impl FnOnce(Option<(Secret, bool)>) + 'static,
) {
    let (window, content) = dialog(parent, title, 460);
    content.append(&label(message));
    let entry = password_entry();
    content.append(&entry);
    let save = gtk::CheckButton::with_mnemonic("_Uložit heslo do šifrovaného trezoru");
    if let Some(default) = offer_save {
        save.set_active(default);
        content.append(&save);
    }
    let (row, cancel, confirm) = buttons(&window, "_Přihlásit");
    content.append(&row);
    close_on_escape(&window);

    let callback = once(done);
    let weak = window.downgrade();
    cancel.connect_clicked(move |_| {
        if let Some(window) = weak.upgrade() {
            window.close();
        }
    });
    {
        let callback = Rc::clone(&callback);
        let weak = window.downgrade();
        let entry = entry.clone();
        confirm.connect_clicked(move |_| {
            let secret = take_secret(&entry);
            finish(&callback, Some((secret, offer_save.is_some() && save.is_active())));
            if let Some(window) = weak.upgrade() {
                window.close();
            }
        });
    }
    window.connect_close_request(move |_| {
        finish(&callback, None);
        glib::Propagation::Proceed
    });
    window.present();
    entry.grab_focus();
}

/// Shows ssh's unknown-host message (with the key fingerprint) and asks for trust.
pub fn confirm_host_key(parent: &impl IsA<gtk::Window>, message: &str, done: impl FnOnce(bool) + 'static) {
    let (window, content) = dialog(parent, "Neznámý server", 620);
    let heading = label("Server zatím není mezi známými (known_hosts). Ověřte otisk klíče, \
        např. u správce serveru, než mu budete důvěřovat.");
    heading.add_css_class("heading");
    content.append(&heading);
    let text = gtk::Label::new(Some(message.trim()));
    text.set_xalign(0.0);
    text.set_wrap(true);
    text.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    text.set_selectable(true);
    text.add_css_class("monospace");
    content.append(&text);
    let (row, cancel, confirm) = buttons(&window, "_Důvěřovat a připojit");
    // Trusting a new key must be a deliberate click, so Enter does not confirm.
    window.set_default_widget(None::<&gtk::Widget>);
    content.append(&row);
    close_on_escape(&window);
    let callback = once(move |value: Option<bool>| done(value.unwrap_or(false)));
    let weak = window.downgrade();
    cancel.connect_clicked(move |_| {
        if let Some(window) = weak.upgrade() {
            window.close();
        }
    });
    {
        let callback = Rc::clone(&callback);
        let weak = window.downgrade();
        confirm.connect_clicked(move |_| {
            finish(&callback, Some(true));
            if let Some(window) = weak.upgrade() {
                window.close();
            }
        });
    }
    window.connect_close_request(move |_| {
        finish(&callback, Some(false));
        glib::Propagation::Proceed
    });
    window.present();
    cancel.grab_focus();
    text.select_region(0, 0);
}

/// Creates the vault: asks for a new master password twice.
pub fn create_vault(parent: &impl IsA<gtk::Window>, done: impl FnOnce(Option<Secret>) + 'static) {
    new_master_dialog(
        parent,
        "Nový trezor hesel",
        "Hesla k serverům se ukládají zašifrovaná (Argon2id + XChaCha20-Poly1305) a odemykají \
         se hlavním heslem. Hlavní heslo se nikam neukládá a nelze je obnovit — při jeho \
         ztrátě je nutné hesla k serverům zadat znovu.",
        done,
    );
}

pub fn new_master_dialog(
    parent: &impl IsA<gtk::Window>,
    title: &str,
    explanation: &str,
    done: impl FnOnce(Option<Secret>) + 'static,
) {
    let (window, content) = dialog(parent, title, 480);
    content.append(&note(explanation));
    let grid = gtk::Grid::new();
    grid.set_row_spacing(6);
    grid.set_column_spacing(8);
    let first = password_entry();
    let second = password_entry();
    for (row, (text, entry)) in [("_Hlavní heslo:", &first), ("_Znovu:", &second)].into_iter().enumerate() {
        let caption = gtk::Label::with_mnemonic(text);
        caption.set_xalign(0.0);
        caption.set_mnemonic_widget(Some(entry));
        grid.attach(&caption, 0, row as i32, 1, 1);
        grid.attach(entry, 1, row as i32, 1, 1);
    }
    content.append(&grid);
    let status = note(&format!("Alespoň {} znaků; doporučena delší věta.", vault::MIN_MASTER_LENGTH));
    content.append(&status);
    let (row, cancel, confirm) = buttons(&window, "_Vytvořit");
    content.append(&row);
    close_on_escape(&window);
    let callback = once(done);
    let weak = window.downgrade();
    cancel.connect_clicked(move |_| {
        if let Some(window) = weak.upgrade() {
            window.close();
        }
    });
    {
        let callback = Rc::clone(&callback);
        let weak = window.downgrade();
        let (first, second) = (first.clone(), second.clone());
        confirm.connect_clicked(move |_| {
            let a = Secret::from_str(&first.text());
            let b = Secret::from_str(&second.text());
            if a.as_bytes().len() < vault::MIN_MASTER_LENGTH {
                status.set_text(&format!("Hlavní heslo musí mít alespoň {} znaků.", vault::MIN_MASTER_LENGTH));
                return;
            }
            if !crate::sodium::equal(a.as_bytes(), b.as_bytes()) {
                status.set_text("Hesla se neshodují.");
                return;
            }
            first.set_text("");
            second.set_text("");
            finish(&callback, Some(a));
            if let Some(window) = weak.upgrade() {
                window.close();
            }
        });
    }
    window.connect_close_request(move |_| {
        finish(&callback, None);
        glib::Propagation::Proceed
    });
    window.present();
    first.grab_focus();
}

/// Asks for the master password. `error` is shown after a failed attempt.
pub fn unlock_vault(parent: &impl IsA<gtk::Window>, error: Option<&str>, done: impl FnOnce(Option<Secret>) + 'static) {
    let (window, content) = dialog(parent, "Odemknout trezor hesel", 420);
    content.append(&label("Zadejte hlavní heslo trezoru:"));
    let entry = password_entry();
    content.append(&entry);
    if let Some(error) = error {
        let message = label(error);
        message.add_css_class("error");
        content.append(&message);
    }
    let (row, cancel, confirm) = buttons(&window, "_Odemknout");
    content.append(&row);
    close_on_escape(&window);
    let callback = once(done);
    let weak = window.downgrade();
    cancel.connect_clicked(move |_| {
        if let Some(window) = weak.upgrade() {
            window.close();
        }
    });
    {
        let callback = Rc::clone(&callback);
        let weak = window.downgrade();
        let entry = entry.clone();
        confirm.connect_clicked(move |_| {
            finish(&callback, Some(take_secret(&entry)));
            if let Some(window) = weak.upgrade() {
                window.close();
            }
        });
    }
    window.connect_close_request(move |_| {
        finish(&callback, None);
        glib::Propagation::Proceed
    });
    window.present();
    entry.grab_focus();
}

/// Result of the server editor.
pub struct EditedServer {
    pub server: Server,
    /// New password to store (only when "save password" is on and something was typed).
    pub password: Option<Secret>,
    pub forget_password: bool,
}

pub fn edit_server(
    parent: &impl IsA<gtk::Window>,
    server: Server,
    has_stored_password: bool,
    done: impl FnOnce(EditedServer) + 'static,
) {
    let creating = server.host.is_empty();
    let (window, content) = dialog(parent, if creating { "Nový server" } else { "Upravit server" }, 560);
    let grid = gtk::Grid::new();
    grid.set_row_spacing(6);
    grid.set_column_spacing(8);
    let mut row = 0;
    let mut add = |caption: &str, widget: &gtk::Widget| {
        let text = gtk::Label::with_mnemonic(caption);
        text.set_xalign(0.0);
        text.set_mnemonic_widget(Some(widget));
        widget.set_hexpand(true);
        grid.attach(&text, 0, row, 1, 1);
        grid.attach(widget, 1, row, 1, 1);
        row += 1;
    };
    let entry = |text: &str, placeholder: &str| {
        let entry = gtk::Entry::new();
        entry.set_text(text);
        entry.set_placeholder_text(Some(placeholder));
        entry.set_activates_default(true);
        entry
    };
    let name = entry(&server.name, "např. Produkční web");
    let host = entry(&server.host, "adresa nebo jméno z ~/.ssh/config");
    let port = gtk::SpinButton::with_range(1.0, 65535.0, 1.0);
    port.set_value(f64::from(server.port));
    let user = entry(&server.user, "uživatel na serveru");
    let labels: Vec<&str> = AuthMethod::ALL.iter().map(|method| method.label()).collect();
    let auth = gtk::DropDown::from_strings(&labels);
    auth.set_selected(AuthMethod::ALL.iter().position(|method| *method == server.auth).unwrap_or(0) as u32);
    let identity_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let identity = entry(
        &server.identity.as_deref().map(|path| path.to_string_lossy().into_owned()).unwrap_or_default(),
        "~/.ssh/id_ed25519 (nepovinné)",
    );
    identity.set_hexpand(true);
    let browse = gtk::Button::with_label("…");
    identity_row.append(&identity);
    identity_row.append(&browse);
    let dir = entry(&server.initial_dir, "domovský adresář");
    let options = entry(&server.options, "např. -o ProxyJump=bastion");
    add("_Název:", name.upcast_ref());
    add("_Server:", host.upcast_ref());
    add("_Port:", port.upcast_ref());
    add("_Uživatel:", user.upcast_ref());
    add("_Ověření:", auth.upcast_ref());
    add("_Klíč:", identity_row.upcast_ref());
    add("_Adresář:", dir.upcast_ref());
    add("_Volby SSH:", options.upcast_ref());
    content.append(&grid);

    let save = gtk::CheckButton::with_mnemonic("Ukládat _heslo do šifrovaného trezoru");
    save.set_active(server.save_password);
    content.append(&save);
    let password = password_entry();
    password.set_placeholder_text(Some(if has_stored_password {
        "heslo je uloženo — vyplňte jen pro změnu"
    } else {
        "nepovinné; jinak se zeptá při připojení"
    }));
    password.set_sensitive(server.save_password);
    let password_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let password_label = gtk::Label::with_mnemonic("Hes_lo:");
    password_label.set_mnemonic_widget(Some(&password));
    password_row.append(&password_label);
    password_row.append(&password);
    content.append(&password_row);
    let forget = gtk::CheckButton::with_mnemonic("_Smazat uložené heslo");
    if has_stored_password {
        content.append(&forget);
    }
    {
        let password = password.clone();
        save.connect_toggled(move |check| password.set_sensitive(check.is_active()));
    }
    content.append(&note(
        "Přihlášení klíčem a ssh-agent fungují automaticky, stejně jako nastavení z ~/.ssh/config. \
         Při prvním připojení se zobrazí otisk klíče serveru k potvrzení.",
    ));
    let status = note("");
    content.append(&status);
    let (buttons_row, cancel, confirm) = buttons(&window, "_Uložit");
    content.append(&buttons_row);
    close_on_escape(&window);

    {
        let window = window.clone();
        let identity = identity.clone();
        browse.connect_clicked(move |_| {
            let chooser = gtk::FileDialog::builder().title("Soukromý klíč").modal(true).build();
            if let Some(ssh) = std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".ssh")) {
                chooser.set_initial_folder(Some(&gtk::gio::File::for_path(ssh)));
            }
            let identity = identity.clone();
            chooser.open(Some(&window), gtk::gio::Cancellable::NONE, move |result| {
                if let Ok(file) = result {
                    if let Some(path) = file.path() {
                        identity.set_text(&path.to_string_lossy());
                    }
                }
            });
        });
    }
    let weak = window.downgrade();
    cancel.connect_clicked(move |_| {
        if let Some(window) = weak.upgrade() {
            window.close();
        }
    });
    let done = Rc::new(RefCell::new(Some(done)));
    let weak = window.downgrade();
    let focus = if creating { host.clone() } else { name.clone() };
    confirm.connect_clicked(move |_| {
        let identity_text = identity.text().trim().to_owned();
        let identity_path = (!identity_text.is_empty()).then(|| {
            identity_text
                .strip_prefix("~/")
                .and_then(|rest| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(rest)))
                .unwrap_or_else(|| PathBuf::from(&identity_text))
        });
        let edited = Server {
            id: server.id.clone(),
            name: name.text().trim().to_owned(),
            host: host.text().trim().to_owned(),
            port: port.value() as u16,
            user: user.text().trim().to_owned(),
            auth: AuthMethod::ALL[auth.selected() as usize],
            identity: identity_path,
            initial_dir: dir.text().trim().to_owned(),
            options: options.text().trim().to_owned(),
            save_password: save.is_active(),
        };
        if let Err(error) = edited.validate() {
            status.set_text(&error);
            return;
        }
        let typed = take_secret(&password);
        let result = EditedServer {
            password: (edited.save_password && !typed.is_empty()).then_some(typed),
            forget_password: forget.is_active() || (!edited.save_password && has_stored_password),
            server: edited,
        };
        if let Some(window) = weak.upgrade() {
            window.close();
        }
        if let Some(done) = done.borrow_mut().take() {
            done(result);
        }
    });
    window.present();
    focus.grab_focus();
}

/// Callbacks of the server manager, implemented by the main window.
pub struct ManagerActions {
    pub connect: Box<dyn Fn(Server)>,
    pub saved: Box<dyn Fn(&Server, Option<Secret>, bool)>,
    pub deleted: Box<dyn Fn(&Server)>,
    pub has_password: Box<dyn Fn(&str) -> bool>,
}

/// Server database window: list, connect, add, edit, duplicate, delete.
pub fn server_manager(parent: &gtk::ApplicationWindow, actions: ManagerActions) {
    let window = gtk::Window::builder()
        .transient_for(parent)
        .modal(true)
        .title("Vzdálené servery")
        .default_width(620)
        .default_height(440)
        .build();
    let content = gtk::Box::new(gtk::Orientation::Vertical, 8);
    content.set_margin_top(12);
    content.set_margin_bottom(12);
    content.set_margin_start(12);
    content.set_margin_end(12);
    window.set_child(Some(&content));
    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::Browse);
    let scrolled = gtk::ScrolledWindow::new();
    scrolled.set_vexpand(true);
    scrolled.set_child(Some(&list));
    let frame = gtk::Frame::new(None);
    frame.set_child(Some(&scrolled));
    content.append(&frame);
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let connect = gtk::Button::with_mnemonic("_Připojit");
    connect.add_css_class("suggested-action");
    let add = gtk::Button::with_mnemonic("_Nový…");
    let edit = gtk::Button::with_mnemonic("_Upravit…");
    let duplicate = gtk::Button::with_mnemonic("_Duplikovat");
    let delete = gtk::Button::with_mnemonic("_Smazat");
    let close = gtk::Button::with_mnemonic("Za_vřít");
    close.set_hexpand(true);
    close.set_halign(gtk::Align::End);
    for button in [&connect, &add, &edit, &duplicate, &delete, &close] {
        row.append(button);
    }
    content.append(&row);
    content.append(&note(
        "Enter připojí vybraný server do aktivního panelu. Hesla jsou jen v šifrovaném trezoru; \
         práce se servery se zapisuje do logu (Vzdálené → Log vzdálených operací).",
    ));
    close_on_escape(&window);

    let servers = Rc::new(RefCell::new(servers::load(&servers::default_path())));
    let actions = Rc::new(actions);
    let refresh: Rc<dyn Fn(Option<&str>)> = {
        let list = list.downgrade();
        let servers = Rc::clone(&servers);
        let actions = Rc::clone(&actions);
        Rc::new(move |select: Option<&str>| {
            let Some(list) = list.upgrade() else { return };
            list.remove_all();
            let mut selected_row = None;
            for server in servers.borrow().iter() {
                let line = gtk::Box::new(gtk::Orientation::Horizontal, 10);
                line.set_margin_top(4);
                line.set_margin_bottom(4);
                line.set_margin_start(6);
                line.append(&gtk::Image::from_icon_name("network-server-symbolic"));
                let texts = gtk::Box::new(gtk::Orientation::Vertical, 0);
                let title = gtk::Label::new(Some(&server.display_name()));
                title.set_xalign(0.0);
                let mut detail_text = format!("{} — {}", server.label(), server.auth.label());
                if (actions.has_password)(&server.id) {
                    detail_text.push_str(" — heslo v trezoru");
                }
                let detail = gtk::Label::new(Some(&detail_text));
                detail.set_xalign(0.0);
                detail.add_css_class("dim-label");
                detail.add_css_class("caption");
                texts.append(&title);
                texts.append(&detail);
                line.append(&texts);
                let item = gtk::ListBoxRow::new();
                item.set_child(Some(&line));
                item.set_widget_name(&server.id);
                list.append(&item);
                if select == Some(server.id.as_str()) {
                    selected_row = Some(item);
                }
            }
            if let Some(item) = selected_row.or_else(|| list.row_at_index(0)) {
                list.select_row(Some(&item));
            }
        })
    };
    refresh(None);

    let selected = {
        let list = list.downgrade();
        let servers = Rc::clone(&servers);
        Rc::new(move || -> Option<Server> {
            let id = list.upgrade()?.selected_row()?.widget_name().to_string();
            servers.borrow().iter().find(|server| server.id == id).cloned()
        })
    };
    let persist = {
        let servers = Rc::clone(&servers);
        let window = window.downgrade();
        Rc::new(move || {
            if let Err(error) = servers::save(&servers::default_path(), &servers.borrow()) {
                if let Some(window) = window.upgrade() {
                    gtk::AlertDialog::builder().modal(true).message(error).build().show(Some(&window));
                }
            }
        })
    };
    let open_editor = {
        let window = window.downgrade();
        let servers = Rc::clone(&servers);
        let refresh = Rc::clone(&refresh);
        let persist = Rc::clone(&persist);
        let actions = Rc::clone(&actions);
        Rc::new(move |server: Server| {
            let Some(window) = window.upgrade() else { return };
            let has_password = (actions.has_password)(&server.id);
            let servers = Rc::clone(&servers);
            let refresh = Rc::clone(&refresh);
            let persist = Rc::clone(&persist);
            let actions = Rc::clone(&actions);
            edit_server(&window, server, has_password, move |edited| {
                {
                    let mut list = servers.borrow_mut();
                    match list.iter_mut().find(|server| server.id == edited.server.id) {
                        Some(existing) => *existing = edited.server.clone(),
                        None => list.push(edited.server.clone()),
                    }
                    list.sort_by_key(|server| server.display_name().to_lowercase());
                }
                persist();
                (actions.saved)(&edited.server, edited.password, edited.forget_password);
                refresh(Some(&edited.server.id));
            });
        })
    };

    {
        let selected = Rc::clone(&selected);
        let actions = Rc::clone(&actions);
        let weak = window.downgrade();
        let do_connect = Rc::new(move || {
            if let Some(server) = selected() {
                if let Some(window) = weak.upgrade() {
                    window.close();
                }
                (actions.connect)(server);
            }
        });
        let again = Rc::clone(&do_connect);
        connect.connect_clicked(move |_| again());
        list.connect_row_activated(move |_, _| do_connect());
    }
    {
        let open_editor = Rc::clone(&open_editor);
        add.connect_clicked(move |_| open_editor(Server::new()));
    }
    {
        let open_editor = Rc::clone(&open_editor);
        let selected = Rc::clone(&selected);
        edit.connect_clicked(move |_| {
            if let Some(server) = selected() {
                open_editor(server);
            }
        });
    }
    {
        let open_editor = Rc::clone(&open_editor);
        let selected = Rc::clone(&selected);
        duplicate.connect_clicked(move |_| {
            if let Some(server) = selected() {
                let mut copy = server.clone();
                copy.id = Server::new().id;
                copy.name = format!("{} (kopie)", server.display_name());
                open_editor(copy);
            }
        });
    }
    {
        let selected = Rc::clone(&selected);
        let servers = Rc::clone(&servers);
        let refresh = Rc::clone(&refresh);
        let persist = Rc::clone(&persist);
        let actions = Rc::clone(&actions);
        let weak = window.downgrade();
        delete.connect_clicked(move |_| {
            let (Some(server), Some(window)) = (selected(), weak.upgrade()) else { return };
            let servers = Rc::clone(&servers);
            let refresh = Rc::clone(&refresh);
            let persist = Rc::clone(&persist);
            let actions = Rc::clone(&actions);
            let alert = gtk::AlertDialog::builder()
                .modal(true)
                .message(format!("Smazat server {}?", server.display_name()))
                .detail("Smaže se i jeho uložené heslo v trezoru.")
                .buttons(["Zrušit", "Smazat"])
                .cancel_button(0)
                .default_button(0)
                .build();
            alert.choose(Some(&window), gtk::gio::Cancellable::NONE, move |choice| {
                if choice == Ok(1) {
                    servers.borrow_mut().retain(|item| item.id != server.id);
                    persist();
                    (actions.deleted)(&server);
                    refresh(None);
                }
            });
        });
    }
    let weak = window.downgrade();
    close.connect_clicked(move |_| {
        if let Some(window) = weak.upgrade() {
            window.close();
        }
    });
    window.present();
    list.grab_focus();
    if let Some(row) = list.selected_row() {
        row.grab_focus();
    }
}
