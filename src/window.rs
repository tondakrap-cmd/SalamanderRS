use std::cell::{Cell, RefCell};
use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

use gtk::gio;
use gtk::glib;
use gtk::prelude::*;

use crate::archive;
use crate::archive_ui;
use crate::attrs::{self, AttrChange};
use crate::clipboard;
use crate::config::{Config, HOTPATH_COUNT};
use crate::dialogs;
use crate::find;
use crate::model::{compare_listings, format_bytes, format_bytes_exact, Mask};
use crate::ops::{self, ConflictAction, Op, OpEvent, OpKind};
use crate::panel::{OptionalColumn, Panel, SortKey};
use crate::places;
use crate::props;
use crate::viewer;

#[path = "window_remote.rs"]
mod remote_actions;
use remote_actions::*;

#[derive(Clone, Copy, PartialEq, Eq)]
enum WhichPanel {
    Left,
    Right,
}

impl WhichPanel {
    fn other(self) -> Self {
        match self {
            Self::Left => Self::Right,
            Self::Right => Self::Left,
        }
    }
}

struct WindowState {
    left: Panel,
    right: Panel,
    paned: gtk::Paned,
    active: Cell<WhichPanel>,
    busy: Cell<bool>,
    config: RefCell<Config>,
    last_mask: RefCell<String>,
    message: gtk::Label,
    message_generation: Cell<u64>,
    /// Unlocked password vault key; dropped (and wiped) on lock.
    vault_key: RefCell<Option<crate::sodium::Key>>,
    vault_used: Cell<std::time::Instant>,
    /// Watches on remote files being edited (F4), uploading them on save.
    remote_edits: RefCell<Vec<gio::FileMonitor>>,
}

impl WindowState {
    fn panel(&self, which: WhichPanel) -> &Panel {
        match which {
            WhichPanel::Left => &self.left,
            WhichPanel::Right => &self.right,
        }
    }

    fn active_panel(&self) -> &Panel {
        self.panel(self.active.get())
    }

    fn other_panel(&self) -> &Panel {
        self.panel(self.active.get().other())
    }

    /// Shows a short message in the bottom bar for a few seconds.
    fn flash(self: &Rc<Self>, text: &str) {
        let generation = self.message_generation.get().wrapping_add(1);
        self.message_generation.set(generation);
        self.message.set_text(text);
        let weak = Rc::downgrade(self);
        glib::timeout_add_local_once(Duration::from_secs(4), move || {
            if let Some(state) = weak.upgrade() {
                if state.message_generation.get() == generation {
                    state.message.set_text("");
                }
            }
        });
    }
}

fn start_dir(saved: Option<&Path>) -> PathBuf {
    saved
        .filter(|path| path.is_dir())
        .map(Path::to_path_buf)
        .or_else(|| env::var_os("HOME").map(PathBuf::from).filter(|path| path.is_dir()))
        .unwrap_or_else(|| PathBuf::from("/"))
}

pub fn build(app: &gtk::Application, open_dir: Option<PathBuf>) {
    let config = Config::load();
    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title("Salamander RS")
        .default_width(config.width)
        .default_height(config.height)
        .build();
    if config.maximized {
        window.maximize();
    }

    // An archive given on the command line is opened inside the left panel.
    let open_archive = open_dir
        .clone()
        .filter(|path| path.is_file() && archive::looks_like_archive(path));
    let left_dir = open_dir
        .filter(|path| path.is_dir())
        .or_else(|| config.left_dir.clone());
    let left = Panel::new(&start_dir(left_dir.as_deref()), config.show_hidden);
    let right = Panel::new(&start_dir(config.right_dir.as_deref()), config.show_hidden);
    for panel in [&left, &right] {
        panel.set_column_visible(OptionalColumn::Extension, config.show_ext_column);
        panel.set_column_visible(OptionalColumn::Permissions, config.show_permissions_column);
        panel.set_column_visible(OptionalColumn::Owner, config.show_owner_column);
    }

    let paned = gtk::Paned::new(gtk::Orientation::Horizontal);
    paned.set_start_child(Some(&left.root));
    paned.set_end_child(Some(&right.root));
    paned.set_resize_start_child(true);
    paned.set_resize_end_child(true);
    paned.set_shrink_start_child(false);
    paned.set_shrink_end_child(false);
    paned.set_position(config.paned_position);
    paned.set_vexpand(true);

    let message = gtk::Label::new(None);
    message.set_xalign(1.0);
    message.set_hexpand(true);
    message.set_ellipsize(gtk::pango::EllipsizeMode::Start);
    message.set_margin_end(8);

    let state = Rc::new(WindowState {
        left,
        right,
        paned: paned.clone(),
        active: Cell::new(WhichPanel::Left),
        busy: Cell::new(false),
        config: RefCell::new(config),
        last_mask: RefCell::new("*.*".to_owned()),
        message,
        message_generation: Cell::new(0),
        vault_key: RefCell::new(None),
        vault_used: Cell::new(std::time::Instant::now()),
        remote_edits: RefCell::new(Vec::new()),
    });

    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.append(&build_menu_bar(&state.config.borrow()));
    content.append(&paned);
    content.append(&build_button_bar(&state.message));
    window.set_child(Some(&content));

    install_style();
    set_active_panel(&state, WhichPanel::Left);
    connect_panel(&window, &state, WhichPanel::Left);
    connect_panel(&window, &state, WhichPanel::Right);
    install_actions(app, &window, &state);
    install_vault_autolock(&state);
    if let Some(path) = open_archive {
        state.left.navigate(&path);
    }

    // The window owns the shared state for its whole lifetime. Other callbacks
    // use weak references so the panel widgets do not form reference cycles.
    let owned_state = Rc::clone(&state);
    window.connect_close_request(move |window| {
        if owned_state.busy.get() {
            show_error(window, "Nejprve dokončete nebo zrušte probíhající operaci");
            return glib::Propagation::Stop;
        }
        save_config(window, &owned_state);
        lock_vault(&owned_state);
        disconnect_all();
        glib::Propagation::Proceed
    });

    // After a dialog closes or the window is re-activated without a focused widget, keys
    // must go to the active panel rather than nowhere.
    let weak_state = Rc::downgrade(&state);
    window.connect_is_active_notify(move |window| {
        if window.is_active() && gtk::prelude::RootExt::focus(window).is_none() {
            if let Some(state) = weak_state.upgrade() {
                state.active_panel().view().grab_focus();
            }
        }
    });

    window.present();
    state.left.view().grab_focus();
}

fn save_config(window: &gtk::ApplicationWindow, state: &WindowState) {
    let mut config = state.config.borrow_mut();
    config.left_dir = Some(state.left.dir());
    config.right_dir = Some(state.right.dir());
    config.show_hidden = state.left.show_hidden() || state.right.show_hidden();
    config.maximized = window.is_maximized();
    if !config.maximized {
        let (width, height) = window.default_size();
        if width > 0 && height > 0 {
            config.width = width;
            config.height = height;
        }
    }
    config.paned_position = state.paned.position();
    if let Err(error) = config.save() {
        eprintln!("salamander-rs: nelze uložit nastavení: {error}");
    }
}

fn menu_item(menu: &gio::Menu, label: &str, action: &str, accel: Option<&str>) {
    let item = gio::MenuItem::new(Some(label), Some(action));
    if let Some(accel) = accel {
        // Display-only: these keys are handled by the panel so text entries keep them.
        item.set_attribute_value("accel", Some(&accel.to_variant()));
    }
    menu.append_item(&item);
}

fn section(items: &[(&str, &str, Option<&str>)]) -> gio::Menu {
    let menu = gio::Menu::new();
    for (label, action, accel) in items {
        menu_item(&menu, label, action, *accel);
    }
    menu
}

fn build_menu_bar(_config: &Config) -> gtk::PopoverMenuBar {
    let file_menu = gio::Menu::new();
    file_menu.append_section(
        None,
        &section(&[
            ("Přejmenovat…", "win.rename", None),
            ("Zobrazit", "win.view", None),
            ("Upravit", "win.edit", None),
            ("Nový soubor…", "win.new-file", None),
            ("Kopírovat…", "win.copy", None),
            ("Přesunout…", "win.move", None),
            ("Nová složka…", "win.mkdir", None),
            ("Vytvořit symbolický odkaz…", "win.symlink", None),
        ]),
    );
    file_menu.append_section(
        None,
        &section(&[
            ("Přesunout do koše", "win.trash", None),
            ("Trvale smazat", "win.delete", None),
        ]),
    );
    file_menu.append_section(
        None,
        &section(&[
            ("Vlastnosti…", "win.properties", None),
            ("Oprávnění a atributy…", "win.attributes", None),
            ("Spočítat obsazené místo", "win.calc-size", None),
        ]),
    );
    file_menu.append_section(
        None,
        &section(&[
            ("Zabalit…", "win.pack", None),
            ("Rozbalit…", "win.unpack", None),
            ("Otevřít v aplikaci", "win.open-external", Some("<Shift>Return")),
        ]),
    );
    file_menu.append_section(None, &section(&[("Zavřít", "win.close", None)]));

    let edit_menu = gio::Menu::new();
    edit_menu.append_section(
        None,
        &section(&[
            ("Kopírovat do schránky", "win.clip-copy", Some("<Control>c")),
            ("Vyjmout do schránky", "win.clip-cut", Some("<Control>x")),
            ("Vložit ze schránky", "win.clip-paste", Some("<Control>v")),
            ("Kopírovat cestu jako text", "win.copy-path", Some("<Control><Shift>c")),
        ]),
    );
    edit_menu.append_section(
        None,
        &section(&[
            ("Vybrat vše", "win.mark-all", Some("<Control>a")),
            ("Zrušit výběr", "win.clear-marks", Some("<Control>d")),
            ("Vybrat skupinu…", "win.select-group", Some("KP_Add")),
            ("Odznačit skupinu…", "win.unselect-group", Some("KP_Subtract")),
            ("Invertovat výběr", "win.invert-marks", Some("KP_Multiply")),
            ("Porovnat adresáře", "win.compare", None),
        ]),
    );

    let go_menu = gio::Menu::new();
    go_menu.append_section(
        None,
        &section(&[
            ("Levý panel: místa a svazky…", "win.places-left", None),
            ("Pravý panel: místa a svazky…", "win.places-right", None),
            ("Upravit cestu", "win.focus-path", None),
        ]),
    );
    go_menu.append_section(
        None,
        &section(&[
            ("Zpět", "win.back", None),
            ("Vpřed", "win.forward", None),
            ("Nadřazený adresář", "win.go-parent", Some("BackSpace")),
            ("Domovská složka", "win.home", None),
        ]),
    );
    go_menu.append_section(
        None,
        &section(&[
            ("Prohodit panely", "win.swap-panels", None),
            ("Stejný adresář v druhém panelu", "win.same-dir", None),
            ("Uložit klávesovou cestu…", "win.hotpath-set", Some("<Control><Shift>1")),
        ]),
    );

    let view_menu = gio::Menu::new();
    view_menu.append_section(
        None,
        &section(&[
            ("Přepnout panel", "win.switch-panel", Some("Tab")),
            ("Zobrazit skryté položky", "win.toggle-hidden", None),
            ("Obnovit panel", "win.reload", None),
        ]),
    );
    view_menu.append_section(
        Some("Řazení"),
        &section(&[
            ("Podle jména", "win.sort-name", None),
            ("Podle přípony", "win.sort-ext", None),
            ("Podle času", "win.sort-time", None),
            ("Podle velikosti", "win.sort-size", None),
        ]),
    );
    view_menu.append_section(
        Some("Sloupce"),
        &section(&[
            ("Přípona", "win.column-ext", None),
            ("Práva", "win.column-permissions", None),
            ("Vlastník", "win.column-owner", None),
        ]),
    );

    let tools_menu = section(&[
        ("Hledat soubory…", "win.find", None),
        ("Otevřít terminál zde", "win.terminal", None),
    ]);

    let help_menu = section(&[
        ("Klávesové zkratky", "win.shortcuts", None),
        ("O aplikaci", "win.about", None),
    ]);

    let model = gio::Menu::new();
    model.append_submenu(Some("_Soubor"), &file_menu);
    model.append_submenu(Some("Úp_ravy"), &edit_menu);
    model.append_submenu(Some("_Přejít"), &go_menu);
    model.append_submenu(Some("_Zobrazit"), &view_menu);
    model.append_submenu(Some("_Nástroje"), &tools_menu);
    let remote_menu = gio::Menu::new();
    remote_menu.append_section(
        None,
        &section(&[
            ("Připojit k serveru…", "win.servers", None),
            ("Odpojit", "win.disconnect", None),
        ]),
    );
    remote_menu.append_section(
        None,
        &section(&[
            ("Log vzdálených operací", "win.remote-log", None),
            ("Zamknout trezor hesel", "win.vault-lock", None),
            ("Změnit hlavní heslo trezoru…", "win.vault-change", None),
        ]),
    );
    model.append_submenu(Some("_Vzdálené"), &remote_menu);
    model.append_submenu(Some("Nápo_věda"), &help_menu);
    gtk::PopoverMenuBar::from_model(Some(&model))
}

fn build_button_bar(message: &gtk::Label) -> gtk::Box {
    let bar = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    bar.add_css_class("fkey-bar");
    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 2);
    buttons.set_homogeneous(true);
    buttons.set_margin_start(4);
    buttons.set_margin_bottom(4);
    for (key, label, action) in [
        ("F2", "Přejmenovat", "win.rename"),
        ("F3", "Zobrazit", "win.view"),
        ("F4", "Upravit", "win.edit"),
        ("F5", "Kopírovat", "win.copy"),
        ("F6", "Přesunout", "win.move"),
        ("F7", "Nová složka", "win.mkdir"),
        ("F8", "Smazat", "win.trash"),
        ("Alt+F7", "Hledat", "win.find"),
    ] {
        let button = gtk::Button::new();
        let text = gtk::Label::new(None);
        text.set_markup(&format!("<b>{key}</b> {label}"));
        button.set_child(Some(&text));
        button.set_action_name(Some(action));
        // Clicking must not steal keyboard focus from the active panel.
        button.set_focusable(false);
        button.set_focus_on_click(false);
        button.add_css_class("flat");
        buttons.append(&button);
    }
    bar.append(&buttons);
    bar.append(message);
    bar
}

fn install_style() {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(
        ".active-panel { background-color: alpha(@theme_selected_bg_color, 0.10); }
         .file-list .marked { color: #e01b24; font-weight: bold; }
         .symlink { font-style: italic; }
         .panel-info, .panel-status { font-size: smaller; }
         .fkey-bar button { padding: 2px 4px; min-height: 0; }",
    );
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

fn set_active_panel(state: &WindowState, active: WhichPanel) {
    state.active.set(active);
    state.left.root.remove_css_class("active-panel");
    state.right.root.remove_css_class("active-panel");
    state.panel(active).root.add_css_class("active-panel");
}

fn activate(window: &gtk::ApplicationWindow, action: &str) {
    let _ = gio::prelude::ActionGroupExt::activate_action(window, action, None);
}

/// Character typed without Ctrl/Alt/Super, used for quick search.
fn typed_char(key: gtk::gdk::Key, modifiers: gtk::gdk::ModifierType) -> Option<char> {
    use gtk::gdk::ModifierType;
    if modifiers.intersects(
        ModifierType::CONTROL_MASK | ModifierType::ALT_MASK | ModifierType::SUPER_MASK,
    ) {
        return None;
    }
    key.to_unicode().filter(|ch| !ch.is_control())
}

fn connect_panel(window: &gtk::ApplicationWindow, state: &Rc<WindowState>, which: WhichPanel) {
    let panel = state.panel(which);

    let weak_state = Rc::downgrade(state);
    panel.path_entry.connect_has_focus_notify(move |entry| {
        if entry.has_focus() {
            if let Some(state) = weak_state.upgrade() {
                set_active_panel(&state, which);
            }
        }
    });

    let click = gtk::GestureClick::new();
    let weak_state = Rc::downgrade(state);
    click.connect_pressed(move |_, _, _, _| {
        if let Some(state) = weak_state.upgrade() {
            set_active_panel(&state, which);
        }
    });
    panel.view().add_controller(click);

    let weak_state = Rc::downgrade(state);
    let weak_window = window.downgrade();
    panel.view().connect_activate(move |_, position| {
        let (Some(state), Some(window)) = (weak_state.upgrade(), weak_window.upgrade()) else {
            return;
        };
        set_active_panel(&state, which);
        state.panel(which).set_cursor_position(position);
        activate(&window, "activate-entry");
    });

    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    let weak_state = Rc::downgrade(state);
    let weak_window = window.downgrade();
    keys.connect_key_pressed(move |_, key, keycode, modifiers| {
        use gtk::gdk::{Key, ModifierType};
        let (Some(state), Some(window)) = (weak_state.upgrade(), weak_window.upgrade()) else {
            return glib::Propagation::Proceed;
        };
        set_active_panel(&state, which);
        let panel = state.panel(which).clone();
        let ctrl = modifiers.contains(ModifierType::CONTROL_MASK);
        let shift = modifiers.contains(ModifierType::SHIFT_MASK);
        let alt = modifiers.contains(ModifierType::ALT_MASK);
        let plain = !ctrl && !shift && !alt;

        if panel.quick_search_active() {
            match key {
                Key::Escape => {
                    panel.quick_search_end();
                    return glib::Propagation::Stop;
                }
                Key::BackSpace => {
                    panel.quick_search_pop();
                    return glib::Propagation::Stop;
                }
                Key::Down if plain => {
                    panel.quick_search_next(true);
                    return glib::Propagation::Stop;
                }
                Key::Up if plain => {
                    panel.quick_search_next(false);
                    return glib::Propagation::Stop;
                }
                _ => {}
            }
            if let Some(ch) = typed_char(key, modifiers) {
                panel.quick_search_push(ch);
                return glib::Propagation::Stop;
            }
            panel.quick_search_end();
        }

        // Hot paths use hardware keycodes of the digit row, so they also work on layouts
        // such as Czech where the unshifted row produces accented letters.
        if ctrl && !alt && (10..=19).contains(&keycode) {
            let index = ((keycode - 9) % 10) as usize;
            if shift {
                set_hotpath(&state, index);
            } else {
                go_hotpath(&window, &state, index);
            }
            return glib::Propagation::Stop;
        }

        let action = match key {
            Key::Tab | Key::ISO_Left_Tab => Some("switch-panel"),
            Key::Return | Key::KP_Enter if alt && shift => Some("calc-size"),
            Key::Return | Key::KP_Enter if alt => Some("properties"),
            Key::Return | Key::KP_Enter if shift => Some("open-external"),
            Key::Return | Key::KP_Enter => Some("activate-entry"),
            Key::BackSpace if plain => Some("go-parent"),
            Key::Insert if plain => Some("toggle-mark"),
            Key::space if plain => Some("toggle-mark"),
            Key::Up if shift && !ctrl => {
                panel.mark_and_move(-1);
                return glib::Propagation::Stop;
            }
            Key::Down if shift && !ctrl => {
                panel.mark_and_move(1);
                return glib::Propagation::Stop;
            }
            Key::KP_Add => Some("select-group"),
            Key::KP_Subtract => Some("unselect-group"),
            Key::KP_Multiply => Some("invert-marks"),
            Key::a | Key::A if ctrl => Some("mark-all"),
            Key::d | Key::D if ctrl => Some("clear-marks"),
            Key::c | Key::C if ctrl && shift => Some("copy-path"),
            Key::c | Key::C if ctrl => Some("clip-copy"),
            Key::x | Key::X if ctrl => Some("clip-cut"),
            Key::v | Key::V if ctrl => Some("clip-paste"),
            Key::Page_Up if ctrl => Some("go-parent"),
            Key::Page_Down if ctrl => Some("activate-entry"),
            Key::Left if ctrl && !alt => {
                open_in_panel(&state, WhichPanel::Left);
                return glib::Propagation::Stop;
            }
            Key::Right if ctrl && !alt => {
                open_in_panel(&state, WhichPanel::Right);
                return glib::Propagation::Stop;
            }
            Key::Delete if shift => Some("delete"),
            Key::Delete | Key::KP_Delete => Some("trash"),
            _ => None,
        };
        if let Some(action) = action {
            activate(&window, action);
            return glib::Propagation::Stop;
        }
        if let Some(ch) = typed_char(key, modifiers).filter(|ch| *ch != ' ') {
            panel.quick_search_push(ch);
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    });
    panel.view().add_controller(keys);
}

fn add_window_action(
    app: &gtk::Application,
    window: &gtk::ApplicationWindow,
    state: &Rc<WindowState>,
    name: &str,
    accels: &[&str],
    handler: impl Fn(&gtk::ApplicationWindow, &Rc<WindowState>) + 'static,
) {
    let weak_state = Rc::downgrade(state);
    let weak_window = window.downgrade();
    let action = gio::SimpleAction::new(name, None);
    action.connect_activate(move |_, _| {
        if let (Some(state), Some(window)) = (weak_state.upgrade(), weak_window.upgrade()) {
            handler(&window, &state);
        }
    });
    window.add_action(&action);
    if !accels.is_empty() {
        app.set_accels_for_action(&format!("win.{name}"), accels);
    }
}

fn add_column_toggle(
    window: &gtk::ApplicationWindow,
    state: &Rc<WindowState>,
    name: &str,
    column: OptionalColumn,
    initial: bool,
) {
    let action = gio::SimpleAction::new_stateful(name, None, &initial.to_variant());
    let weak_state = Rc::downgrade(state);
    action.connect_activate(move |action, _| {
        let visible = !action
            .state()
            .and_then(|state| state.get::<bool>())
            .unwrap_or(false);
        action.set_state(&visible.to_variant());
        if let Some(state) = weak_state.upgrade() {
            state.left.set_column_visible(column, visible);
            state.right.set_column_visible(column, visible);
            let mut config = state.config.borrow_mut();
            match column {
                OptionalColumn::Extension => config.show_ext_column = visible,
                OptionalColumn::Permissions => config.show_permissions_column = visible,
                OptionalColumn::Owner => config.show_owner_column = visible,
            }
        }
    });
    window.add_action(&action);
}

fn install_actions(
    app: &gtk::Application,
    window: &gtk::ApplicationWindow,
    state: &Rc<WindowState>,
) {
    let action = |name: &str, accels: &[&str], handler: fn(&gtk::ApplicationWindow, &Rc<WindowState>)| {
        add_window_action(app, window, state, name, accels, handler);
    };
    // Actions that only make sense on real files are refused inside archives and on servers.
    let fs_action = |name: &str, accels: &[&str], handler: fn(&gtk::ApplicationWindow, &Rc<WindowState>)| {
        add_window_action(app, window, state, name, accels, move |window, state| {
            if state.active_panel().remote().is_some() {
                show_error(window, REMOTE_UNSUPPORTED);
            } else if state.active_panel().archive().is_some() {
                show_error(window, ARCHIVE_UNSUPPORTED);
            } else {
                handler(window, state);
            }
        });
    };
    // Actions with a separate implementation for remote panels; refused inside archives.
    let routed = |name: &str,
                  accels: &[&str],
                  local: fn(&gtk::ApplicationWindow, &Rc<WindowState>),
                  remote: fn(&gtk::ApplicationWindow, &Rc<WindowState>)| {
        add_window_action(app, window, state, name, accels, move |window, state| {
            if state.active_panel().remote().is_some() {
                remote(window, state);
            } else if state.active_panel().archive().is_some() {
                show_error(window, ARCHIVE_UNSUPPORTED);
            } else {
                local(window, state);
            }
        });
    };

    // Panel and navigation.
    action("switch-panel", &[], |_, state| {
        let next = state.active.get().other();
        set_active_panel(state, next);
        state.panel(next).view().grab_focus();
    });
    action("activate-entry", &[], |window, state| {
        activate_entry(state.active_panel(), window);
    });
    action("go-parent", &[], |_, state| state.active_panel().navigate_parent());
    action("back", &["<Alt>Left"], |_, state| state.active_panel().history_back());
    action("forward", &["<Alt>Right"], |_, state| {
        state.active_panel().history_forward()
    });
    action("home", &["<Alt>Home"], |_, state| state.active_panel().navigate_home());
    action("focus-path", &["<Control>l", "<Shift>F7"], |_, state| {
        let entry = &state.active_panel().path_entry;
        entry.grab_focus();
        entry.select_region(0, -1);
    });
    action("swap-panels", &["<Control>u"], |_, state| {
        let (left, right) = (state.left.location().display(), state.right.location().display());
        state.left.navigate(&right);
        state.right.navigate(&left);
    });
    action("same-dir", &[], |_, state| {
        let dir = state.active_panel().location().display();
        state.other_panel().navigate(&dir);
    });
    action("places-left", &["<Alt>F1"], |window, state| {
        open_places(window, state, WhichPanel::Left)
    });
    action("places-right", &["<Alt>F2"], |window, state| {
        open_places(window, state, WhichPanel::Right)
    });
    action("hotpath-set", &[], |window, state| {
        let weak_state = Rc::downgrade(state);
        let weak_window = window.downgrade();
        dialogs::prompt(
            window,
            "Uložit klávesovou cestu",
            "Číslo 0–9 (pak skok Ctrl+číslo):",
            "1",
            "Uložit",
            move |text| {
                let (Some(state), Some(window)) = (weak_state.upgrade(), weak_window.upgrade())
                else {
                    return;
                };
                match text.trim().parse::<usize>() {
                    Ok(index) if index < HOTPATH_COUNT => set_hotpath(&state, index),
                    _ => show_error(&window, "Zadejte číslo 0–9"),
                }
            },
        );
    });

    // Marks.
    action("toggle-mark", &[], |_, state| {
        state.active_panel().toggle_mark_and_advance()
    });
    action("mark-all", &[], |_, state| state.active_panel().mark_all());
    action("clear-marks", &[], |_, state| state.active_panel().clear_marks());
    action("invert-marks", &[], |_, state| state.active_panel().invert_marks());
    action("select-group", &[], |window, state| begin_mark_group(window, state, true));
    action("unselect-group", &[], |window, state| begin_mark_group(window, state, false));
    action("compare", &["<Shift>F2"], |window, state| compare_directories(window, state));

    // View.
    action("toggle-hidden", &["<Control>h"], |_, state| {
        state.active_panel().toggle_hidden()
    });
    action("reload", &["<Control>r"], |_, state| state.active_panel().reload());
    action("sort-name", &["<Control>F3"], |_, state| {
        state.active_panel().sort_by(SortKey::Name)
    });
    action("sort-ext", &["<Control>F4"], |_, state| {
        state.active_panel().sort_by(SortKey::Extension)
    });
    action("sort-time", &["<Control>F5"], |_, state| {
        state.active_panel().sort_by(SortKey::Time)
    });
    action("sort-size", &["<Control>F6"], |_, state| {
        state.active_panel().sort_by(SortKey::Size)
    });
    {
        let config = state.config.borrow();
        add_column_toggle(window, state, "column-ext", OptionalColumn::Extension, config.show_ext_column);
        add_column_toggle(
            window,
            state,
            "column-permissions",
            OptionalColumn::Permissions,
            config.show_permissions_column,
        );
        add_column_toggle(window, state, "column-owner", OptionalColumn::Owner, config.show_owner_column);
    }

    // File operations.
    routed("rename", &["F2"], begin_rename, remote_rename);
    action("view", &["F3"], |window, state| {
        let panel = state.active_panel();
        if panel.remote().is_some() {
            remote_view(window, state);
        } else if let Some((archive, inner, is_dir)) = archive_cursor(panel) {
            if !is_dir {
                with_extracted(window, archive, inner, |window, path| viewer::open(window, &path));
            }
        } else if let Some(path) = panel.cursor_path() {
            viewer::open(window, &path);
        }
    });
    action("open-external", &[], |window, state| {
        let panel = state.active_panel();
        if panel.remote().is_some() {
            remote_open_external(window, state);
        } else if let Some((archive, inner, is_dir)) = archive_cursor(panel) {
            if !is_dir {
                with_extracted(window, archive, inner, |window, path| open_file(window, path));
            }
        } else if let Some(path) = panel.cursor_path() {
            open_file(window, path);
        }
    });
    routed(
        "edit",
        &["F4"],
        |window, state| {
            if let Some(path) = state.active_panel().cursor_path() {
                if path.is_dir() {
                    show_error(window, "Adresář nelze upravit; použijte Enter");
                } else {
                    edit_file(window, &path);
                }
            }
        },
        remote_edit,
    );
    routed("new-file", &["<Shift>F4"], begin_new_file, remote_new_file);
    action("copy", &["F5"], |window, state| begin_copy_move(window, state, OpKind::Copy));
    action("move", &["F6"], |window, state| begin_copy_move(window, state, OpKind::Move));
    routed("mkdir", &["F7"], begin_mkdir, remote_mkdir);
    fs_action("symlink", &["<Control><Shift>F5"], begin_symlink);
    fs_action("pack", &["<Alt>F5"], begin_pack);
    action("unpack", &["<Alt>F9"], begin_unpack);
    action("trash", &["F8"], |window, state| begin_removal(window, state, false));
    action("delete", &["<Shift>F8"], |window, state| begin_removal(window, state, true));
    action("properties", &["<Alt>Return"], |window, state| {
        if state.active_panel().remote().is_some() {
            remote_info(window, state);
        } else if state.active_panel().archive().is_some() {
            show_archive_info(window, state.active_panel());
        } else {
            open_properties(window, state, props::Page::General)
        }
    });
    routed(
        "attributes",
        &["<Control>F2"],
        |window, state| open_properties(window, state, props::Page::Permissions),
        remote_chmod,
    );
    fs_action("calc-size", &["<Alt><Shift>Return"], calculate_size);

    // Clipboard.
    fs_action("clip-copy", &[], |window, state| clipboard_copy(window, state, false));
    fs_action("clip-cut", &[], |window, state| clipboard_copy(window, state, true));
    fs_action("clip-paste", &[], clipboard_paste);
    action("copy-path", &[], |window, state| {
        let paths = state.active_panel().selected_or_cursor();
        if paths.is_empty() {
            return;
        }
        let text = paths
            .iter()
            .map(|path| path.to_string_lossy())
            .collect::<Vec<_>>()
            .join("\n");
        clipboard::copy_text(&window.clipboard(), &text);
        state.flash(&format!("Cesta zkopírována: {}", paths.len()));
    });

    // Tools and application.
    fs_action("find", &["<Alt>F7"], |window, state| {
        let weak_state = Rc::downgrade(state);
        let weak_window = window.downgrade();
        find::open(
            window,
            &state.active_panel().dir(),
            move |path| {
                if let Some(state) = weak_state.upgrade() {
                    let panel = state.active_panel();
                    if let Some(parent) = path.parent() {
                        panel.navigate_and_focus(parent, Some(&path));
                    }
                    panel.view().grab_focus();
                }
            },
            move |path| {
                if let Some(window) = weak_window.upgrade() {
                    if path.is_file() {
                        viewer::open(&window, &path);
                    }
                }
            },
        );
    });
    action("terminal", &["<Control>t"], |window, state| {
        if state.active_panel().remote().is_some() {
            remote_terminal(window, state);
        } else {
            open_terminal(window, &state.active_panel().dir())
        }
    });

    // Remote servers.
    action("servers", &["<Control>k"], open_server_manager);
    action("disconnect", &[], disconnect_active);
    action("remote-log", &[], |window, _| show_remote_log(window));
    action("vault-lock", &[], |_, state| lock_vault(state));
    action("vault-change", &[], change_master);
    action("close", &["<Alt>F4", "<Control>q"], |window, _| window.close());
    action("shortcuts", &["F1"], |window, _| {
        dialogs::info(window, "Klávesové zkratky", SHORTCUTS)
    });
    action("about", &[], |window, _| {
        let about = gtk::AboutDialog::builder()
            .transient_for(window)
            .modal(true)
            .program_name("Salamander RS")
            .logo_icon_name("salamander-rs")
            .version(env!("CARGO_PKG_VERSION"))
            .comments("Dvoupanelový správce souborů pro Linux inspirovaný Open Salamanderem")
            .website("https://github.com/tondakrap-cmd/SalamanderRS")
            .license_type(gtk::License::Gpl30)
            .build();
        about.present();
    });
}

const SHORTCUTS: &str = "\
Panely
  Tab — přepnout panel          Ctrl+U — prohodit panely
  Enter — otevřít               Backspace, Ctrl+PgUp — nadřazený adresář
  Alt+←/→ — historie zpět/vpřed Alt+Home — domovská složka
  Alt+F1 / Alt+F2 — místa a svazky pro levý / pravý panel
  Ctrl+← / Ctrl+→ — otevřít adresář pod kurzorem v levém / pravém panelu
  Ctrl+L, Shift+F7 — upravit cestu
  Ctrl+1…0 — klávesová cesta, Ctrl+Shift+1…0 — uložit klávesovou cestu
  psaní písmen — rychlé hledání (↑/↓ další shoda, Esc konec)

Výběr
  Insert, mezerník — označit a posunout   Shift+↑/↓ — označovat
  Num + / Num − — vybrat / odznačit skupinu podle masky
  Num * — invertovat výběr   Ctrl+A / Ctrl+D — vše / nic
  Shift+F2 — porovnat adresáře (označí odlišné soubory)

Soubory
  F2 přejmenovat, F3 zobrazit, F4 upravit, Shift+F4 nový soubor
  F5 kopírovat, F6 přesunout, F7 nová složka, Ctrl+Shift+F5 symlink
  F8, Delete — do koše; Shift+F8, Shift+Delete — trvale smazat
  Alt+Enter — vlastnosti; Ctrl+F2 — oprávnění, vlastník, časy
  Alt+Shift+Enter — spočítat obsazené místo
  Shift+Enter — otevřít v přidružené aplikaci

Archivy (zip, tar.gz/xz/zst/bz2, 7z, rar, iso, deb, …)
  Enter na archivu — procházet jako adresář, Backspace — zpět ven
  F5 z archivu — rozbalit vybrané; F5 do archivu — přidat; F8 v archivu — odstranit
  Alt+F5 — zabalit výběr, Alt+F9 — rozbalit vybrané archivy
  Ctrl+C / Ctrl+X / Ctrl+V — schránka (kompatibilní s jinými správci)
  Ctrl+Shift+C — zkopírovat cestu jako text

Zobrazení a nástroje
  Ctrl+H skryté soubory, Ctrl+R obnovit, Ctrl+F3…F6 řadit jméno/přípona/čas/velikost
  Alt+F7 hledat soubory, Ctrl+T terminál, Ctrl+Q / Alt+F4 konec

Vzdálené servery (SSH/SFTP)
  Ctrl+K — správce serverů a připojení; servery jsou i v Alt+F1/Alt+F2
  F5/F6 — stáhnout nebo nahrát (přesunout); F4 — upravit, uložení se nahraje zpět
  F2, F7, Shift+F4, F8 — přejmenovat, složka, soubor, smazat; Ctrl+F2 — oprávnění
  Ctrl+T — terminál se SSH v aktuálním adresáři; Vzdálené → Odpojit, Log, Trezor

Prohlížeč (F3)
  Ctrl+F / F7 hledat, F3 / Shift+F3 další / předchozí, H text/hex, W zalamování, Esc zavřít";

const ARCHIVE_UNSUPPORTED: &str =
    "V archivu tato operace není podporována. Archiv lze procházet, rozbalovat (F5), \
     doplňovat (F5 z druhého panelu) a mazat z něj (F8).";

/// `(archive, inner path, is directory)` of the cursor entry inside an archive panel.
fn archive_cursor(panel: &Panel) -> Option<(PathBuf, PathBuf, bool)> {
    let (archive, _) = panel.archive()?;
    let entry = panel.cursor_entry()?;
    let (path, is_dir) = entry.with_entry(|item| (item.path.clone(), item.is_dir_like()));
    let inner = path.strip_prefix(&archive).ok()?.to_path_buf();
    Some((archive, inner, is_dir))
}

/// Extracts one archive member to a private temporary directory in the background.
fn with_extracted(
    window: &gtk::ApplicationWindow,
    archive: PathBuf,
    inner: PathBuf,
    then: impl FnOnce(&gtk::ApplicationWindow, PathBuf) + 'static,
) {
    let (sender, receiver) = async_channel::bounded(1);
    std::thread::spawn(move || {
        let _ = sender.send_blocking(archive::extract_to_temp(&archive, &inner));
    });
    let weak_window = window.downgrade();
    glib::spawn_future_local(async move {
        let (Ok(result), Some(window)) = (receiver.recv().await, weak_window.upgrade()) else {
            return;
        };
        match result {
            Ok(path) => then(&window, path),
            Err(error) => show_error(&window, &format!("Nelze rozbalit: {error}")),
        }
    });
}

type JobFn = dyn FnOnce(&AtomicBool, &mut dyn FnMut(&Path)) -> archive::JobResult + Send;

/// Runs a long archive job in a worker thread with a cancellable progress window, then
/// refreshes both panels and reports errors.
fn run_job(
    window: &gtk::ApplicationWindow,
    state: &Rc<WindowState>,
    title: &str,
    clear_marks: Option<Panel>,
    job: Box<JobFn>,
) {
    if !try_begin_busy(state) {
        return;
    }
    let cancel = Arc::new(AtomicBool::new(false));
    let progress_cancel = Arc::clone(&cancel);
    let progress = dialogs::Progress::new(window, title, move || {
        progress_cancel.store(true, Ordering::Relaxed)
    });
    let (sender, receiver) = async_channel::unbounded::<Result<archive::JobResult, PathBuf>>();
    std::thread::spawn(move || {
        let mut last = std::time::Instant::now();
        let progress_sender = sender.clone();
        let mut report = |path: &Path| {
            if last.elapsed() >= Duration::from_millis(100) {
                last = std::time::Instant::now();
                let _ = progress_sender.send_blocking(Err(path.to_path_buf()));
            }
        };
        let result = job(&cancel, &mut report);
        let _ = sender.send_blocking(Ok(result));
    });
    let weak_state = Rc::downgrade(state);
    let weak_window = window.downgrade();
    glib::spawn_future_local(async move {
        while let Ok(message) = receiver.recv().await {
            match message {
                Err(path) => progress.update(&path.to_string_lossy()),
                Ok(result) => {
                    progress.finish();
                    if let Some(state) = weak_state.upgrade() {
                        state.busy.set(false);
                        state.left.reload();
                        state.right.reload();
                        if let Some(panel) = &clear_marks {
                            panel.clear_marks();
                        }
                        state.flash(&format!(
                            "Hotovo, položek: {}{}",
                            result.done,
                            if result.cancelled { " (přerušeno)" } else { "" }
                        ));
                    }
                    if let Some(window) = weak_window.upgrade() {
                        dialogs::show_errors(&window, &result.errors);
                    }
                    break;
                }
            }
        }
    });
}

fn absolute_in(base: &Path, typed: &str) -> PathBuf {
    let path = PathBuf::from(typed);
    if path.is_absolute() {
        path
    } else {
        base.join(path)
    }
}

fn begin_extract(
    window: &gtk::ApplicationWindow,
    state: &Rc<WindowState>,
    archive: PathBuf,
    base: PathBuf,
) {
    let source_panel = state.active_panel().clone();
    let items = source_panel.archive_selection();
    if items.is_empty() {
        return;
    }
    let dest = state.other_panel().dir();
    let base_dest = dest.clone();
    let weak_state = Rc::downgrade(state);
    let weak_window = window.downgrade();
    archive_ui::unpack_dialog(
        window,
        "Rozbalit z archivu",
        &format!("Rozbalit {} položek z {} do:", items.len(), archive.to_string_lossy()),
        &dest,
        false,
        "_Rozbalit",
        move |options| {
            let (Some(state), Some(window)) = (weak_state.upgrade(), weak_window.upgrade()) else {
                return;
            };
            let dest = absolute_in(&base_dest, &options.dest.to_string_lossy());
            run_job(
                &window,
                &state,
                "Rozbaluji",
                Some(source_panel),
                Box::new(move |cancel, progress| {
                    if let Err(error) = fs::create_dir_all(&dest) {
                        let mut result = archive::JobResult::default();
                        result.errors.push(crate::model::OpError {
                            source: dest,
                            target: None,
                            error: error.to_string(),
                        });
                        return result;
                    }
                    archive::extract(
                        &archive::ExtractRequest {
                            archive: &archive,
                            items: &items,
                            base: &base,
                            dest: &dest,
                            overwrite: options.overwrite,
                        },
                        cancel,
                        progress,
                    )
                }),
            );
        },
    );
}

fn begin_add_to_archive(
    window: &gtk::ApplicationWindow,
    state: &Rc<WindowState>,
    archive: PathBuf,
    inner: PathBuf,
) {
    let source_panel = state.active_panel().clone();
    let sources = source_panel.selected_or_cursor();
    if sources.is_empty() {
        return;
    }
    let index = match archive::index(&archive) {
        Ok(index) => index,
        Err(error) => {
            show_error(window, &format!("Nelze číst archiv: {error}"));
            return;
        }
    };
    if !index.is_writable() {
        show_error(window, &format!("Do archivu formátu {} nelze zapisovat", index.format));
        return;
    }
    let replaced = sources
        .iter()
        .filter_map(|source| source.file_name())
        .filter(|name| index.contains(&inner.join(name)))
        .count();
    let mut message = format!(
        "Přidat {} položek do {}?",
        sources.len(),
        crate::panel::Location::Archive {
            archive: archive.clone(),
            inner: inner.clone(),
        }
        .display()
        .to_string_lossy()
    );
    if replaced > 0 {
        message.push_str(&format!("\n{replaced} stejnojmenných položek v archivu bude nahrazeno."));
    }
    let weak_state = Rc::downgrade(state);
    let weak_window = window.downgrade();
    dialogs::confirm(window, "Přidat do archivu", &message, "Přidat", move || {
        let (Some(state), Some(window)) = (weak_state.upgrade(), weak_window.upgrade()) else {
            return;
        };
        run_job(
            &window,
            &state,
            "Přidávám do archivu",
            Some(source_panel),
            Box::new(move |cancel, progress| {
                archive::modify(&archive, &[], &sources, &inner, cancel, progress)
            }),
        );
    });
}

fn begin_archive_delete(window: &gtk::ApplicationWindow, state: &Rc<WindowState>, archive: PathBuf) {
    let items = state.active_panel().archive_selection();
    if items.is_empty() {
        return;
    }
    let writable = archive::index(&archive).is_ok_and(|index| index.is_writable());
    if !writable {
        show_error(window, "Tento formát archivu je jen pro čtení");
        return;
    }
    let weak_state = Rc::downgrade(state);
    let weak_window = window.downgrade();
    dialogs::confirm(
        window,
        "Odstranit z archivu",
        &format!(
            "Odstranit {} položek z archivu {}? Tuto akci nelze vrátit.",
            items.len(),
            archive.to_string_lossy()
        ),
        "Odstranit",
        move || {
            let (Some(state), Some(window)) = (weak_state.upgrade(), weak_window.upgrade()) else {
                return;
            };
            run_job(
                &window,
                &state,
                "Odstraňuji z archivu",
                None,
                Box::new(move |cancel, progress| {
                    archive::modify(&archive, &items, &[], Path::new(""), cancel, progress)
                }),
            );
        },
    );
}

fn begin_pack(window: &gtk::ApplicationWindow, state: &Rc<WindowState>) {
    if state.busy.get() {
        return;
    }
    let source_panel = state.active_panel().clone();
    let sources = source_panel.selected_or_cursor();
    if sources.is_empty() {
        return;
    }
    let dest_dir = if state.other_panel().archive().is_none() {
        state.other_panel().dir()
    } else {
        source_panel.dir()
    };
    let default_name = if sources.len() == 1 {
        let name = sources[0]
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        archive_ui::strip_archive_extension(&name).to_owned()
    } else {
        source_panel
            .dir()
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "archiv".to_owned())
    };
    let weak_state = Rc::downgrade(state);
    let weak_window = window.downgrade();
    archive_ui::pack_dialog(window, sources.len(), &dest_dir, &default_name, move |target, format| {
        let (Some(state), Some(window)) = (weak_state.upgrade(), weak_window.upgrade()) else {
            return;
        };
        let target = absolute_in(&source_panel.dir(), &target.to_string_lossy());
        if target.exists() {
            show_error(&window, &format!("{} již existuje", target.to_string_lossy()));
            return;
        }
        run_job(
            &window,
            &state,
            &format!("Balím do {}", format.label()),
            Some(source_panel),
            Box::new(move |cancel, progress| archive::create(&target, format, &sources, cancel, progress)),
        );
    });
}

fn begin_unpack(window: &gtk::ApplicationWindow, state: &Rc<WindowState>) {
    if state.busy.get() {
        return;
    }
    if state.active_panel().remote().is_some() {
        show_error(window, REMOTE_UNSUPPORTED);
        return;
    }
    if state.active_panel().archive().is_some() {
        // Inside an archive Alt+F9 extracts the selection, like F5.
        begin_copy_move(window, state, OpKind::Copy);
        return;
    }
    let archives: Vec<PathBuf> = state
        .active_panel()
        .selected_or_cursor()
        .into_iter()
        .filter(|path| path.is_file() && archive::looks_like_archive(path))
        .collect();
    if archives.is_empty() {
        show_error(window, "Vyberte archivy k rozbalení (zip, tar.gz, 7z, …)");
        return;
    }
    let dest = if state.other_panel().archive().is_none() {
        state.other_panel().dir()
    } else {
        state.active_panel().dir()
    };
    let base_dest = dest.clone();
    let weak_state = Rc::downgrade(state);
    let weak_window = window.downgrade();
    archive_ui::unpack_dialog(
        window,
        "Rozbalit archivy",
        &format!("Rozbalit {} archivů do:", archives.len()),
        &dest,
        true,
        "_Rozbalit",
        move |options| {
            let (Some(state), Some(window)) = (weak_state.upgrade(), weak_window.upgrade()) else {
                return;
            };
            let dest = absolute_in(&base_dest, &options.dest.to_string_lossy());
            run_job(
                &window,
                &state,
                "Rozbaluji archivy",
                None,
                Box::new(move |cancel, progress| {
                    let mut total = archive::JobResult::default();
                    for source in &archives {
                        let target = if options.subfolder {
                            let name = source
                                .file_name()
                                .map(|name| name.to_string_lossy().into_owned())
                                .unwrap_or_default();
                            dest.join(archive_ui::strip_archive_extension(&name))
                        } else {
                            dest.clone()
                        };
                        if let Err(error) = fs::create_dir_all(&target) {
                            total.errors.push(crate::model::OpError {
                                source: target,
                                target: None,
                                error: error.to_string(),
                            });
                            continue;
                        }
                        let result = archive::extract(
                            &archive::ExtractRequest {
                                archive: source,
                                items: &[],
                                base: Path::new(""),
                                dest: &target,
                                overwrite: options.overwrite,
                            },
                            cancel,
                            progress,
                        );
                        total.done += result.done;
                        total.errors.extend(result.errors);
                        if result.cancelled {
                            total.cancelled = true;
                            break;
                        }
                    }
                    total
                }),
            );
        },
    );
}

fn show_archive_info(window: &gtk::ApplicationWindow, panel: &Panel) {
    let (Some((archive, inner, _)), Some(entry)) = (archive_cursor(panel), panel.cursor_entry()) else {
        return;
    };
    let format = archive::index(&archive)
        .map(|index| index.format.clone())
        .unwrap_or_default();
    let size = entry.display_size();
    let text = entry.with_entry(|item| {
        let mut text = format!(
            "Název: {}\nCesta v archivu: {}\nArchiv: {}\nFormát: {format}\nVelikost: {size}\n\
             Změněno: {}\nOprávnění: {}\nVlastník: {}:{}",
            item.name.to_string_lossy(),
            inner.to_string_lossy(),
            archive.to_string_lossy(),
            crate::model::format_time(item.modified, "%Y-%m-%d %H:%M:%S"),
            item.mode_text(),
            item.owner,
            item.group
        );
        if let Some(target) = &item.link_target {
            text.push_str(&format!("\nCíl odkazu: {}", target.to_string_lossy()));
        }
        text
    });
    dialogs::info(window, "Vlastnosti položky archivu", &text);
}

fn try_begin_busy(state: &WindowState) -> bool {
    if state.busy.get() {
        false
    } else {
        state.busy.set(true);
        true
    }
}

fn open_in_panel(state: &Rc<WindowState>, target: WhichPanel) {
    let active = state.active_panel();
    let path = active
        .cursor_entry()
        .filter(|entry| entry.with_entry(|item| item.is_dir_like()))
        .map(|entry| entry.with_entry(|item| item.path.clone()))
        .unwrap_or_else(|| active.location().display());
    state.panel(target).navigate(&path);
}

fn open_places(window: &gtk::ApplicationWindow, state: &Rc<WindowState>, which: WhichPanel) {
    let panel = state.panel(which).clone();
    let title = match which {
        WhichPanel::Left => "Levý panel — přejít na",
        WhichPanel::Right => "Pravý panel — přejít na",
    };
    let hotpaths = state.config.borrow().hotpaths.clone();
    let weak_state = Rc::downgrade(state);
    let server_state = Rc::downgrade(state);
    let server_window = window.downgrade();
    let servers = crate::remote::servers::load(&crate::remote::servers::default_path());
    places::open(
        window,
        title,
        &hotpaths,
        &panel.history(),
        servers,
        move |path| {
            panel.navigate(&path);
            if let Some(state) = weak_state.upgrade() {
                set_active_panel(&state, which);
            }
            panel.view().grab_focus();
        },
        move |server| {
            if let (Some(state), Some(window)) = (server_state.upgrade(), server_window.upgrade()) {
                connect_server(&window, &state, server, which);
            }
        },
    );
}

fn set_hotpath(state: &Rc<WindowState>, index: usize) {
    let dir = state.active_panel().dir();
    let result = {
        let mut config = state.config.borrow_mut();
        config.hotpaths[index] = Some(dir.clone());
        config.save()
    };
    match result {
        Ok(()) => state.flash(&format!(
            "Klávesová cesta Ctrl+{index} = {}",
            dir.to_string_lossy()
        )),
        Err(error) => state.flash(&format!("Nelze uložit nastavení: {error}")),
    }
}

fn go_hotpath(window: &gtk::ApplicationWindow, state: &Rc<WindowState>, index: usize) {
    let path = state.config.borrow().hotpaths[index].clone();
    match path {
        Some(path) => state.active_panel().navigate(&path),
        None => show_error(
            window,
            &format!("Klávesová cesta Ctrl+{index} není nastavena (uložíte ji Ctrl+Shift+{index})"),
        ),
    }
}

fn begin_mark_group(window: &gtk::ApplicationWindow, state: &Rc<WindowState>, mark: bool) {
    let initial = state.last_mask.borrow().clone();
    let weak_state = Rc::downgrade(state);
    let (title, button) = if mark {
        ("Vybrat skupinu", "Vybrat")
    } else {
        ("Odznačit skupinu", "Odznačit")
    };
    dialogs::prompt_with_check(
        window,
        title,
        "Maska (např. *.txt;*.md, vyloučení za |):",
        &initial,
        "Včetně _adresářů",
        false,
        button,
        move |mask, include_dirs| {
            if let Some(state) = weak_state.upgrade() {
                state
                    .active_panel()
                    .mark_matching(&Mask::parse(&mask), include_dirs, mark);
                *state.last_mask.borrow_mut() = mask;
            }
        },
    );
}

fn compare_directories(window: &gtk::ApplicationWindow, state: &Rc<WindowState>) {
    let (left, right) = compare_listings(&state.left.compare_items(), &state.right.compare_items());
    state.left.mark_names(&left);
    state.right.mark_names(&right);
    if left.is_empty() && right.is_empty() {
        dialogs::info(window, "Porovnat adresáře", "Soubory v obou adresářích jsou shodné (podle jména, velikosti a času).");
    } else {
        state.flash(&format!(
            "Odlišné soubory označeny: vlevo {}, vpravo {}",
            left.len(),
            right.len()
        ));
    }
}

fn calculate_size(window: &gtk::ApplicationWindow, state: &Rc<WindowState>) {
    let entries = state.active_panel().selected_entries();
    if entries.is_empty() {
        return;
    }
    let weak_window = window.downgrade();
    state.active_panel().calculate_sizes(entries, move |total| {
        if let Some(window) = weak_window.upgrade() {
            let mut text = format!(
                "Obsazené místo: {} ({} B)\nSouborů: {}\nAdresářů: {}",
                format_bytes(total.bytes),
                format_bytes_exact(total.bytes),
                total.files,
                total.dirs
            );
            if total.errors > 0 {
                text.push_str(&format!("\nNečitelných položek: {}", total.errors));
            }
            dialogs::info(&window, "Obsazené místo", &text);
        }
    });
}

fn open_properties(window: &gtk::ApplicationWindow, state: &Rc<WindowState>, page: props::Page) {
    if state.busy.get() {
        return;
    }
    let paths = state.active_panel().selected_or_cursor();
    if paths.is_empty() {
        return;
    }
    let weak_state = Rc::downgrade(state);
    let weak_window = window.downgrade();
    props::open(window, paths, page, move |paths, change| {
        if let (Some(state), Some(window)) = (weak_state.upgrade(), weak_window.upgrade()) {
            run_attr_change(&window, &state, paths, change);
        }
    });
}

fn run_attr_change(
    window: &gtk::ApplicationWindow,
    state: &Rc<WindowState>,
    paths: Vec<PathBuf>,
    change: AttrChange,
) {
    if !try_begin_busy(state) {
        return;
    }
    let cancel = Arc::new(AtomicBool::new(false));
    let progress_cancel = Arc::clone(&cancel);
    let progress = Rc::new(dialogs::Progress::new(
        window,
        &format!("Měním atributy ({})", props::describe_change(&change)),
        move || progress_cancel.store(true, Ordering::Relaxed),
    ));
    let (sender, receiver) = async_channel::unbounded::<Result<attrs::AttrResult, (PathBuf, u64)>>();
    std::thread::spawn(move || {
        let mut last = std::time::Instant::now();
        let progress_sender = sender.clone();
        let result = attrs::apply(&paths, &change, &cancel, |path, count| {
            if last.elapsed() >= Duration::from_millis(100) {
                last = std::time::Instant::now();
                let _ = progress_sender.send_blocking(Err((path.to_path_buf(), count)));
            }
        });
        let _ = sender.send_blocking(Ok(result));
    });

    let weak_state = Rc::downgrade(state);
    let weak_window = window.downgrade();
    glib::spawn_future_local(async move {
        while let Ok(message) = receiver.recv().await {
            match message {
                Err((path, count)) => {
                    progress.update(&format!("{count} — {}", path.to_string_lossy()));
                }
                Ok(result) => {
                    progress.finish();
                    if let Some(state) = weak_state.upgrade() {
                        state.busy.set(false);
                        state.left.reload();
                        state.right.reload();
                        let mut text = format!("Změněno položek: {}", result.changed);
                        if result.cancelled {
                            text.push_str(" (přerušeno)");
                        }
                        state.flash(&text);
                    }
                    if let Some(window) = weak_window.upgrade() {
                        dialogs::show_errors(&window, &result.errors);
                    }
                    break;
                }
            }
        }
    });
}

fn clipboard_copy(window: &gtk::ApplicationWindow, state: &Rc<WindowState>, cut: bool) {
    let paths = state.active_panel().selected_or_cursor();
    if paths.is_empty() {
        return;
    }
    clipboard::copy_files(&window.clipboard(), &paths, cut);
    state.flash(&format!(
        "{} do schránky: {} položek",
        if cut { "Vyjmuto" } else { "Zkopírováno" },
        paths.len()
    ));
}

fn clipboard_paste(window: &gtk::ApplicationWindow, state: &Rc<WindowState>) {
    if state.busy.get() {
        return;
    }
    let clip = window.clipboard();
    let weak_state = Rc::downgrade(state);
    let weak_window = window.downgrade();
    glib::spawn_future_local(async move {
        let result = clipboard::paste_files(&clip).await;
        let (Some(state), Some(window)) = (weak_state.upgrade(), weak_window.upgrade()) else {
            return;
        };
        match result {
            Ok(pasted) if !pasted.paths.is_empty() => {
                if !try_begin_busy(&state) {
                    return;
                }
                let panel = state.active_panel().clone();
                if pasted.cut {
                    // A cut can only be pasted once.
                    clip.set_content(None::<&gtk::gdk::ContentProvider>).ok();
                }
                run_copy_move(
                    &window,
                    &state,
                    None,
                    Op {
                        kind: if pasted.cut { OpKind::Move } else { OpKind::Copy },
                        sources: pasted.paths,
                        dest_dir: panel.dir(),
                    },
                );
            }
            Ok(_) => {}
            Err(error) => state.flash(&error),
        }
    });
}

fn begin_rename(window: &gtk::ApplicationWindow, state: &Rc<WindowState>) {
    if state.busy.get() {
        return;
    }
    let panel = state.active_panel().clone();
    let Some(old_path) = panel.cursor_path() else {
        return;
    };
    let initial = old_path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let weak_state = Rc::downgrade(state);
    let weak_window = window.downgrade();
    dialogs::prompt(
        window,
        "Přejmenovat",
        "Nové jméno:",
        &initial,
        "Přejmenovat",
        move |name| {
            let (Some(state), Some(window)) = (weak_state.upgrade(), weak_window.upgrade()) else {
                return;
            };
            if let Err(message) = validate_name(&name) {
                show_error(&window, &message);
                return;
            }
            let Some(parent) = old_path.parent() else {
                show_error(&window, "Přejmenovávaná položka nemá nadřazený adresář");
                return;
            };
            let new_path = parent.join(name);
            if new_path == old_path {
                return;
            }
            // On case-insensitive filesystems (FAT, exFAT, NTFS) a case-only rename finds the
            // "target" already existing as the very same inode; that is allowed. Any other
            // existing target is refused so rename never silently replaces a file.
            let same_entry = match (fs::symlink_metadata(&old_path), fs::symlink_metadata(&new_path)) {
                (Ok(old), Ok(new)) => {
                    use std::os::unix::fs::MetadataExt;
                    old.dev() == new.dev() && old.ino() == new.ino()
                }
                _ => false,
            };
            if (!same_entry && !check_new_path(&window, &new_path)) || !try_begin_busy(&state) {
                return;
            }
            let worker_old = old_path.clone();
            let worker_new = new_path.clone();
            run_path_mutation(
                &window,
                &state,
                panel,
                new_path,
                "Přejmenování selhalo",
                move || fs::rename(worker_old, worker_new),
            );
        },
    );
}

fn begin_mkdir(window: &gtk::ApplicationWindow, state: &Rc<WindowState>) {
    begin_create(window, state, false);
}

fn begin_new_file(window: &gtk::ApplicationWindow, state: &Rc<WindowState>) {
    begin_create(window, state, true);
}

fn begin_create(window: &gtk::ApplicationWindow, state: &Rc<WindowState>, file: bool) {
    if state.busy.get() {
        return;
    }
    let panel = state.active_panel().clone();
    let weak_state = Rc::downgrade(state);
    let weak_window = window.downgrade();
    let (title, label, button) = if file {
        ("Nový soubor", "Jméno souboru:", "Vytvořit a upravit")
    } else {
        ("Nová složka", "Jméno složky (lze i a/b/c):", "Vytvořit")
    };
    dialogs::prompt(window, title, label, "", button, move |name| {
        let (Some(state), Some(window)) = (weak_state.upgrade(), weak_window.upgrade()) else {
            return;
        };
        // Nested folders like Salamander: "a/b/c" creates the whole chain.
        let valid = if file {
            validate_name(&name)
        } else {
            name.split('/')
                .filter(|part| !part.is_empty())
                .try_for_each(validate_name)
                .and_then(|()| {
                    if name.trim_matches('/').is_empty() {
                        Err("Jméno nesmí být prázdné".to_owned())
                    } else {
                        Ok(())
                    }
                })
        };
        if let Err(message) = valid {
            show_error(&window, &message);
            return;
        }
        let new_path = panel.dir().join(name.trim_matches('/'));
        if !check_new_path(&window, &new_path) {
            return;
        }
        if file {
            // Creating one empty file is instant, so it is done here and the editor opens
            // only after it really exists.
            match fs::OpenOptions::new().write(true).create_new(true).open(&new_path) {
                Ok(_) => {
                    panel.reload_and_focus(&new_path);
                    edit_file(&window, &new_path);
                }
                Err(error) => show_error(
                    &window,
                    &format!("Vytvoření souboru selhalo {}: {error}", new_path.to_string_lossy()),
                ),
            }
            return;
        }
        if !try_begin_busy(&state) {
            return;
        }
        let worker_path = new_path.clone();
        // Focus the first created component in the current directory.
        let focus = panel
            .dir()
            .join(Path::new(name.trim_matches('/')).components().next().map_or_else(
                || PathBuf::from(&name),
                |component| PathBuf::from(component.as_os_str()),
            ));
        run_path_mutation(
            &window,
            &state,
            panel,
            focus,
            "Vytvoření složky selhalo",
            move || fs::create_dir_all(worker_path),
        );
    });
}

fn begin_symlink(window: &gtk::ApplicationWindow, state: &Rc<WindowState>) {
    if state.busy.get() {
        return;
    }
    let sources = state.active_panel().selected_or_cursor();
    if sources.is_empty() {
        return;
    }
    let dest_dir = state.other_panel().dir();
    let weak_state = Rc::downgrade(state);
    let weak_window = window.downgrade();
    let create = move |links: Vec<(PathBuf, PathBuf)>| {
        let (Some(state), Some(window)) = (weak_state.upgrade(), weak_window.upgrade()) else {
            return;
        };
        let mut errors = Vec::new();
        for (target, link) in links {
            if let Err(error) = std::os::unix::fs::symlink(&target, &link) {
                errors.push(crate::model::OpError {
                    source: target,
                    target: Some(link),
                    error: error.to_string(),
                });
            }
        }
        state.left.reload();
        state.right.reload();
        dialogs::show_errors(&window, &errors);
    };
    if sources.len() == 1 {
        let source = sources[0].clone();
        let initial = dest_dir.join(source.file_name().unwrap_or_default());
        dialogs::prompt(
            window,
            "Vytvořit symbolický odkaz",
            &format!("Odkaz na {} vytvořit jako:", source.to_string_lossy()),
            &initial.to_string_lossy(),
            "Vytvořit",
            move |link| create(vec![(source, PathBuf::from(link))]),
        );
    } else {
        let count = sources.len();
        dialogs::confirm(
            window,
            "Vytvořit symbolické odkazy",
            &format!(
                "Vytvořit {count} symbolických odkazů v {}?",
                dest_dir.to_string_lossy()
            ),
            "Vytvořit",
            move || {
                create(
                    sources
                        .into_iter()
                        .map(|source| {
                            let link = dest_dir.join(source.file_name().unwrap_or_default());
                            (source, link)
                        })
                        .collect(),
                )
            },
        );
    }
}

fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\0') {
        Err("Jméno nesmí být prázdné, . ani .. a nesmí obsahovat / nebo NUL".to_owned())
    } else {
        Ok(())
    }
}

fn check_new_path(window: &gtk::ApplicationWindow, path: &Path) -> bool {
    match fs::symlink_metadata(path) {
        Ok(_) => {
            show_error(
                window,
                &format!("Cíl {} již existuje", path.to_string_lossy()),
            );
            false
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => true,
        Err(error) => {
            show_error(
                window,
                &format!("Nelze ověřit cíl {}: {error}", path.to_string_lossy()),
            );
            false
        }
    }
}

fn run_path_mutation(
    window: &gtk::ApplicationWindow,
    state: &Rc<WindowState>,
    panel: Panel,
    new_path: PathBuf,
    error_title: &'static str,
    operation: impl FnOnce() -> io::Result<()> + Send + 'static,
) {
    let (sender, receiver) = async_channel::bounded(1);
    std::thread::spawn(move || {
        let result = operation();
        let _ = sender.send_blocking(result);
    });
    let weak_state = Rc::downgrade(state);
    let weak_window = window.downgrade();
    glib::spawn_future_local(async move {
        let Ok(result) = receiver.recv().await else {
            return;
        };
        if let Some(state) = weak_state.upgrade() {
            state.busy.set(false);
        }
        let Some(window) = weak_window.upgrade() else {
            return;
        };
        match result {
            Ok(()) => panel.reload_and_focus(&new_path),
            Err(error) => show_error(
                &window,
                &format!("{error_title} {}: {error}", new_path.to_string_lossy()),
            ),
        }
    });
}

fn begin_removal(window: &gtk::ApplicationWindow, state: &Rc<WindowState>, permanent: bool) {
    if state.busy.get() {
        return;
    }
    if state.active_panel().remote().is_some() {
        begin_remote_delete(window, state);
        return;
    }
    if let Some((archive, _)) = state.active_panel().archive() {
        begin_archive_delete(window, state, archive);
        return;
    }
    let paths = state.active_panel().selected_or_cursor();
    if paths.is_empty() {
        return;
    }
    let count = paths.len();
    let what = if count == 1 {
        format!(
            "„{}“",
            paths[0]
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default()
        )
    } else {
        format!("{count} položek")
    };
    let (title, message, button) = if permanent {
        (
            "Trvalé smazání",
            format!("Trvale smazat {what}? Tuto akci nelze vrátit."),
            "Smazat",
        )
    } else {
        (
            "Přesunout do koše",
            format!("Přesunout {what} do koše?"),
            "Přesunout do koše",
        )
    };
    let weak_state = Rc::downgrade(state);
    let weak_window = window.downgrade();
    dialogs::confirm(window, title, &message, button, move || {
        let (Some(state), Some(window)) = (weak_state.upgrade(), weak_window.upgrade()) else {
            return;
        };
        if !try_begin_busy(&state) {
            return;
        }
        let (sender, receiver) = async_channel::bounded(1);
        std::thread::spawn(move || {
            let errors = if permanent {
                ops::delete_permanently(paths)
            } else {
                ops::trash_paths(paths)
            };
            let _ = sender.send_blocking(errors);
        });
        let weak_state = Rc::downgrade(&state);
        let weak_window = window.downgrade();
        glib::spawn_future_local(async move {
            let Ok(errors) = receiver.recv().await else {
                return;
            };
            if let Some(state) = weak_state.upgrade() {
                state.busy.set(false);
                state.left.reload();
                state.right.reload();
            }
            if let Some(window) = weak_window.upgrade() {
                dialogs::show_errors(&window, &errors);
            }
        });
    });
}

fn begin_copy_move(window: &gtk::ApplicationWindow, state: &Rc<WindowState>, kind: OpKind) {
    if state.busy.get() {
        return;
    }
    let active_archive = state.active_panel().archive();
    let other_archive = state.other_panel().archive();
    let active_remote = state.active_panel().remote().is_some();
    let other_remote = state.other_panel().remote().is_some();
    if active_remote || other_remote {
        if active_remote && other_remote {
            show_error(window, "Kopírování mezi dvěma vzdálenými panely není podporováno; použijte místní adresář");
        } else if active_archive.is_some() || other_archive.is_some() {
            show_error(window, "Přenos mezi archivem a serverem není podporován; archiv nejprve rozbalte");
        } else if active_remote {
            begin_download(window, state, kind);
        } else {
            begin_upload(window, state, kind);
        }
        return;
    }
    if kind == OpKind::Move && (active_archive.is_some() || other_archive.is_some()) {
        show_error(window, "Přesun z archivu nebo do archivu není podporován; použijte F5 a potom F8");
        return;
    }
    match (active_archive, other_archive) {
        (Some(_), Some(_)) => {
            show_error(window, "Kopírování mezi dvěma archivy není podporováno");
            return;
        }
        (Some((archive, inner)), None) => {
            begin_extract(window, state, archive, inner);
            return;
        }
        (None, Some((archive, inner))) => {
            begin_add_to_archive(window, state, archive, inner);
            return;
        }
        (None, None) => {}
    }
    let source_panel = state.active_panel().clone();
    let sources = source_panel.selected_or_cursor();
    if sources.is_empty() {
        return;
    }
    let destination = state.other_panel().dir();
    let count = sources.len();
    let what = if count == 1 {
        format!(
            "„{}“",
            sources[0]
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default()
        )
    } else {
        format!("{count} položek")
    };
    let (title, label, button) = match kind {
        OpKind::Copy => ("Kopírovat", format!("Kopírovat {what} do:"), "Kopírovat"),
        OpKind::Move => ("Přesunout", format!("Přesunout {what} do:"), "Přesunout"),
    };
    let weak_state = Rc::downgrade(state);
    let weak_window = window.downgrade();
    dialogs::prompt(
        window,
        title,
        &label,
        &destination.to_string_lossy(),
        button,
        move |destination| {
            let (Some(state), Some(window)) = (weak_state.upgrade(), weak_window.upgrade()) else {
                return;
            };
            if !try_begin_busy(&state) {
                return;
            }
            let dest_dir = source_panel.resolve_path(Path::new(&destination));
            run_copy_move(
                &window,
                &state,
                Some(source_panel),
                Op {
                    kind,
                    sources,
                    dest_dir,
                },
            );
        },
    );
}

struct ConflictPrompt {
    window: gtk::Window,
    answered: Cell<bool>,
}

struct OperationUi {
    window: gtk::Window,
    progress: gtk::ProgressBar,
    current: gtk::Label,
    counters: gtk::Label,
    pulse: RefCell<Option<glib::SourceId>>,
    conflict: RefCell<Option<Rc<ConflictPrompt>>>,
    replies: mpsc::Sender<ConflictAction>,
    cancel: Arc<AtomicBool>,
    finished: Cell<bool>,
    started: std::time::Instant,
}

impl OperationUi {
    fn new(
        parent: &gtk::ApplicationWindow,
        title: &str,
        replies: mpsc::Sender<ConflictAction>,
        cancel: Arc<AtomicBool>,
    ) -> Rc<Self> {
        let window = gtk::Window::builder()
            .transient_for(parent)
            .modal(true)
            .title(title)
            .default_width(600)
            .resizable(false)
            .build();
        let content = gtk::Box::new(gtk::Orientation::Vertical, 10);
        content.set_margin_top(18);
        content.set_margin_bottom(18);
        content.set_margin_start(18);
        content.set_margin_end(18);
        window.set_child(Some(&content));

        let progress = gtk::ProgressBar::new();
        progress.set_show_text(true);
        progress.set_fraction(0.0);
        content.append(&progress);
        let current = gtk::Label::new(None);
        current.set_xalign(0.0);
        current.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        content.append(&current);
        let counters = gtk::Label::new(None);
        counters.set_xalign(0.0);
        content.append(&counters);
        let cancel_button = gtk::Button::with_label("Zrušit");
        cancel_button.set_halign(gtk::Align::End);
        content.append(&cancel_button);

        let ui = Rc::new(Self {
            window,
            progress,
            current,
            counters,
            pulse: RefCell::new(None),
            conflict: RefCell::new(None),
            replies,
            cancel,
            finished: Cell::new(false),
            started: std::time::Instant::now(),
        });
        let weak_ui = Rc::downgrade(&ui);
        cancel_button.connect_clicked(move |_| {
            if let Some(ui) = weak_ui.upgrade() {
                ui.request_cancel();
            }
        });
        let weak_ui = Rc::downgrade(&ui);
        ui.window.connect_close_request(move |_| {
            if let Some(ui) = weak_ui.upgrade() {
                if ui.finished.get() {
                    return glib::Propagation::Proceed;
                }
                ui.request_cancel();
            }
            glib::Propagation::Stop
        });
        ui
    }

    fn preparing(&self, path: &Path) {
        self.current
            .set_text(&format!("Připravuji… {}", path.to_string_lossy()));
        if self.pulse.borrow().is_some() {
            return;
        }
        let weak_progress = self.progress.downgrade();
        let source = glib::timeout_add_local(Duration::from_millis(100), move || {
            if let Some(progress) = weak_progress.upgrade() {
                progress.pulse();
                glib::ControlFlow::Continue
            } else {
                glib::ControlFlow::Break
            }
        });
        *self.pulse.borrow_mut() = Some(source);
    }

    fn stop_pulse(&self) {
        if let Some(source) = self.pulse.borrow_mut().take() {
            source.remove();
        }
    }

    fn progress(&self, progress: &ops::OpProgress) {
        self.stop_pulse();
        let fraction = if progress.total_bytes > 0 {
            progress.done_bytes as f64 / progress.total_bytes as f64
        } else if progress.total_files > 0 {
            progress.done_files as f64 / progress.total_files as f64
        } else {
            0.0
        };
        self.progress.set_fraction(fraction.clamp(0.0, 1.0));
        self.current.set_text(&progress.current.to_string_lossy());
        let elapsed = self.started.elapsed().as_secs_f64();
        let speed = if elapsed > 0.5 {
            format!(", {}/s", format_bytes((progress.done_bytes as f64 / elapsed) as u64))
        } else {
            String::new()
        };
        self.counters.set_text(&format!(
            "{} / {} souborů, {} / {}{speed}",
            progress.done_files,
            progress.total_files,
            format_bytes(progress.done_bytes),
            format_bytes(progress.total_bytes)
        ));
    }

    fn show_conflict(self: &Rc<Self>, conflict: ops::Conflict) {
        let dialog = gtk::Window::builder()
            .transient_for(&self.window)
            .modal(true)
            .title("Konflikt cíle")
            .default_width(560)
            .resizable(false)
            .build();
        let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
        content.set_margin_top(18);
        content.set_margin_bottom(18);
        content.set_margin_start(18);
        content.set_margin_end(18);
        dialog.set_child(Some(&content));
        let describe = |path: &Path| match crate::model::stat_entry(path) {
            Ok(entry) => format!(
                "{}\n    {}, {}",
                path.to_string_lossy(),
                crate::model::format_size(&entry),
                crate::model::format_time(entry.modified, "%Y-%m-%d %H:%M:%S")
            ),
            Err(_) => path.to_string_lossy().into_owned(),
        };
        let message = gtk::Label::new(Some(&format!(
            "Cíl už existuje.\nZdroj: {}\nCíl: {}",
            describe(&conflict.source),
            describe(&conflict.target)
        )));
        message.set_wrap(true);
        message.set_selectable(true);
        message.set_xalign(0.0);
        content.append(&message);

        let prompt = Rc::new(ConflictPrompt {
            window: dialog,
            answered: Cell::new(false),
        });
        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        buttons.set_halign(gtk::Align::End);
        for (label, action) in [
            ("_Přepsat", ConflictAction::Overwrite),
            ("Př_eskočit", ConflictAction::Skip),
            ("Přepsat _vše", ConflictAction::OverwriteAll),
            ("Přeskočit vš_e", ConflictAction::SkipAll),
            ("P_řerušit", ConflictAction::Abort),
        ] {
            let button = gtk::Button::with_mnemonic(label);
            let weak_prompt = Rc::downgrade(&prompt);
            let replies = self.replies.clone();
            button.connect_clicked(move |_| {
                if let Some(prompt) = weak_prompt.upgrade() {
                    if !prompt.answered.replace(true) {
                        let _ = replies.send(action);
                    }
                    prompt.window.close();
                }
            });
            buttons.append(&button);
        }
        content.append(&buttons);

        let weak_prompt = Rc::downgrade(&prompt);
        let replies = self.replies.clone();
        prompt.window.connect_close_request(move |_| {
            if let Some(prompt) = weak_prompt.upgrade() {
                if !prompt.answered.replace(true) {
                    let _ = replies.send(ConflictAction::Abort);
                }
            }
            glib::Propagation::Proceed
        });
        *self.conflict.borrow_mut() = Some(prompt);
        self.conflict.borrow().as_ref().unwrap().window.present();
    }

    fn close_conflict(&self) {
        if let Some(prompt) = self.conflict.borrow_mut().take() {
            prompt.answered.set(true);
            prompt.window.close();
        }
    }

    fn request_cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
        let _ = self.replies.send(ConflictAction::Abort);
        self.close_conflict();
    }

    fn finish(&self) {
        self.finished.set(true);
        self.stop_pulse();
        self.close_conflict();
        self.window.close();
    }
}

fn run_copy_move(
    window: &gtk::ApplicationWindow,
    state: &Rc<WindowState>,
    source_panel: Option<Panel>,
    op: Op,
) {
    let title = match op.kind {
        OpKind::Copy => "Kopírování",
        OpKind::Move => "Přesouvání",
    };
    let (events, receiver) = async_channel::bounded(64);
    let (replies, reply_receiver) = mpsc::channel();
    let cancel = Arc::new(AtomicBool::new(false));
    let ui = OperationUi::new(window, title, replies, Arc::clone(&cancel));
    drop(ops::start(op, events, reply_receiver, cancel));
    ui.window.present();

    let weak_state = Rc::downgrade(state);
    let weak_window = window.downgrade();
    glib::spawn_future_local(async move {
        while let Ok(event) = receiver.recv().await {
            match event {
                OpEvent::Preparing { current } => ui.preparing(&current),
                OpEvent::Progress(progress) => ui.progress(&progress),
                OpEvent::Conflict(conflict) => ui.show_conflict(conflict),
                OpEvent::Finished(result) => {
                    ui.finish();
                    if let Some(state) = weak_state.upgrade() {
                        state.busy.set(false);
                        state.left.reload();
                        state.right.reload();
                        if let Some(panel) = &source_panel {
                            panel.clear_marks();
                        }
                        if result.outcome == ops::Outcome::Cancelled {
                            state.flash("Operace přerušena");
                        }
                    }
                    if let Some(window) = weak_window.upgrade() {
                        dialogs::show_errors(&window, &result.errors);
                    }
                    break;
                }
            }
        }
    });
}

fn activate_entry(panel: &Panel, window: &gtk::ApplicationWindow) {
    if panel.remote().is_some() {
        remote_activate(window, panel);
        return;
    }
    if let Some((archive, inner, is_dir)) = archive_cursor(panel) {
        if is_dir {
            panel.navigate(&archive.join(inner));
        } else {
            with_extracted(window, archive, inner, |window, path| open_file(window, path));
        }
        return;
    }
    let Some(path) = panel.cursor_path() else {
        return;
    };
    match fs::metadata(&path) {
        Ok(metadata) if metadata.is_dir() => panel.navigate(&path),
        // Like Salamander, Enter on an archive opens it as a directory (Shift+Enter runs
        // the associated application).
        Ok(metadata) if metadata.is_file() && archive::looks_like_archive(&path) => {
            panel.navigate(&path)
        }
        Ok(metadata) if metadata.is_file() => open_file(window, path),
        Ok(_) => show_error(
            window,
            &format!(
                "Nelze otevřít {}: není běžný soubor ani adresář",
                path.to_string_lossy()
            ),
        ),
        Err(error) => show_error(
            window,
            &format!("Nelze otevřít {}: {error}", path.to_string_lossy()),
        ),
    }
}

/// Runs a command in the background and reaps it so no zombie process is left behind.
fn spawn_detached(command: &mut Command) -> io::Result<()> {
    let mut child = command.spawn()?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// Opens a file in the desktop's default text editor (F4), falling back to xdg-open.
fn edit_file(window: &gtk::ApplicationWindow, path: &Path) {
    let Some(app) = gio::AppInfo::default_for_type("text/plain", false) else {
        open_file(window, path.to_path_buf());
        return;
    };
    let context = WidgetExt::display(window).app_launch_context();
    if let Err(error) = app.launch(&[gio::File::for_path(path)], Some(&context)) {
        show_error(window, &format!("Nelze spustit editor {}: {error}", app.name()));
    }
}

fn open_terminal(window: &gtk::ApplicationWindow, dir: &Path) {
    let mut candidates: Vec<String> = env::var("TERMINAL").into_iter().collect();
    candidates.extend(
        [
            "x-terminal-emulator",
            "qterminal",
            "gnome-terminal",
            "konsole",
            "xfce4-terminal",
            "lxterminal",
            "kitty",
            "alacritty",
            "xterm",
        ]
        .map(String::from),
    );
    for candidate in candidates {
        match spawn_detached(Command::new(&candidate).current_dir(dir)) {
            Ok(()) => return,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                show_error(window, &format!("Nelze spustit {candidate}: {error}"));
                return;
            }
        }
    }
    show_error(
        window,
        "Nenalezen žádný emulátor terminálu (nastavte proměnnou TERMINAL)",
    );
}

fn open_file(window: &gtk::ApplicationWindow, path: PathBuf) {
    let mut child = match Command::new("xdg-open").arg(&path).spawn() {
        Ok(child) => child,
        Err(error) => {
            show_error(window, &format!("Nelze spustit xdg-open: {error}"));
            return;
        }
    };

    let (sender, receiver) = async_channel::bounded(1);
    std::thread::spawn(move || {
        let result = child.wait();
        let _ = sender.send_blocking(result);
    });

    let weak_window = window.downgrade();
    glib::spawn_future_local(async move {
        let Ok(result) = receiver.recv().await else {
            return;
        };
        let Some(window) = weak_window.upgrade() else {
            return;
        };
        match result {
            Ok(status) if !status.success() => {
                show_error(&window, &format!("xdg-open selhal: {status}"));
            }
            Err(error) => show_error(&window, &format!("xdg-open selhal: {error}")),
            Ok(_) => {}
        }
    });
}

fn show_error(window: &gtk::ApplicationWindow, message: &str) {
    let dialog = gtk::AlertDialog::builder()
        .modal(true)
        .message(message)
        .build();
    dialog.show(Some(window));
}
