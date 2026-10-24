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

use crate::dialogs;
use crate::ops::{self, ConflictAction, Op, OpEvent, OpKind};
use crate::panel::Panel;
use crate::viewer;

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
    active: Cell<WhichPanel>,
    busy: Cell<bool>,
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
}

pub fn build(app: &gtk::Application) {
    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title("Salamander RS")
        .default_width(1200)
        .default_height(700)
        .build();

    let start_dir = env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_dir())
        .unwrap_or_else(|| PathBuf::from("/"));
    let state = Rc::new(WindowState {
        left: Panel::new(&start_dir),
        right: Panel::new(&start_dir),
        active: Cell::new(WhichPanel::Left),
        busy: Cell::new(false),
    });

    let menu_bar = build_menu_bar();
    let paned = gtk::Paned::new(gtk::Orientation::Horizontal);
    paned.set_start_child(Some(&state.left.root));
    paned.set_end_child(Some(&state.right.root));
    paned.set_resize_start_child(true);
    paned.set_resize_end_child(true);
    paned.set_shrink_start_child(false);
    paned.set_shrink_end_child(false);
    paned.set_position(600);
    paned.set_vexpand(true);

    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.append(&menu_bar);
    content.append(&paned);
    window.set_child(Some(&content));

    install_active_panel_style();
    set_active_panel(&state, WhichPanel::Left);
    connect_panel(&window, &state, WhichPanel::Left);
    connect_panel(&window, &state, WhichPanel::Right);
    install_actions(app, &window, &state);

    // The window owns the shared state for its whole lifetime. Other callbacks
    // use weak references so the panel widgets do not form reference cycles.
    let owned_state = Rc::clone(&state);
    window.connect_close_request(move |window| {
        if owned_state.busy.get() {
            show_error(window, "Nejprve dokončete nebo zrušte probíhající operaci");
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    });

    window.present();
    state.left.view().grab_focus();
}

fn build_menu_bar() -> gtk::PopoverMenuBar {
    let file_menu = gio::Menu::new();
    file_menu.append(Some("Přejmenovat…"), Some("win.rename"));
    file_menu.append(Some("Prohlédnout"), Some("win.view"));
    file_menu.append(Some("Kopírovat…"), Some("win.copy"));
    file_menu.append(Some("Přesunout…"), Some("win.move"));
    file_menu.append(Some("Nová složka…"), Some("win.mkdir"));
    file_menu.append(Some("Přesunout do koše"), Some("win.trash"));
    file_menu.append(Some("Trvale smazat"), Some("win.delete"));
    file_menu.append(Some("Zavřít"), Some("win.close"));

    let view_menu = gio::Menu::new();
    view_menu.append(Some("Přepnout panel"), Some("win.switch-panel"));
    view_menu.append(Some("Zobrazit skryté položky"), Some("win.toggle-hidden"));
    view_menu.append(Some("Obnovit panel"), Some("win.reload"));

    let help_menu = gio::Menu::new();
    help_menu.append(Some("O aplikaci"), Some("win.about"));

    let model = gio::Menu::new();
    model.append_submenu(Some("Soubor"), &file_menu);
    model.append_submenu(Some("Zobrazit"), &view_menu);
    model.append_submenu(Some("Nápověda"), &help_menu);
    gtk::PopoverMenuBar::from_model(Some(&model))
}

fn install_active_panel_style() {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(
        ".active-panel { background-color: alpha(@theme_selected_bg_color, 0.10); }",
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
        let _ = gio::prelude::ActionGroupExt::activate_action(&window, "activate-entry", None);
    });

    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    let weak_state = Rc::downgrade(state);
    let weak_window = window.downgrade();
    keys.connect_key_pressed(move |_, key, _, modifiers| {
        if let Some(state) = weak_state.upgrade() {
            set_active_panel(&state, which);
        }
        let action = match key {
            gtk::gdk::Key::Tab => Some("switch-panel"),
            gtk::gdk::Key::Return | gtk::gdk::Key::KP_Enter => Some("activate-entry"),
            gtk::gdk::Key::BackSpace => Some("go-parent"),
            gtk::gdk::Key::Insert | gtk::gdk::Key::space => Some("toggle-mark"),
            gtk::gdk::Key::a if modifiers.contains(gtk::gdk::ModifierType::CONTROL_MASK) => {
                Some("mark-all")
            }
            gtk::gdk::Key::d if modifiers.contains(gtk::gdk::ModifierType::CONTROL_MASK) => {
                Some("clear-marks")
            }
            gtk::gdk::Key::Delete if modifiers.contains(gtk::gdk::ModifierType::SHIFT_MASK) => {
                Some("delete")
            }
            gtk::gdk::Key::Delete => Some("trash"),
            _ => None,
        };
        let Some(action) = action else {
            return glib::Propagation::Proceed;
        };
        if let Some(window) = weak_window.upgrade() {
            let _ = gio::prelude::ActionGroupExt::activate_action(&window, action, None);
        }
        glib::Propagation::Stop
    });
    panel.view().add_controller(keys);
}

fn install_actions(
    app: &gtk::Application,
    window: &gtk::ApplicationWindow,
    state: &Rc<WindowState>,
) {
    let weak_state = Rc::downgrade(state);
    add_action(window, "switch-panel", move || {
        if let Some(state) = weak_state.upgrade() {
            let next = state.active.get().other();
            set_active_panel(&state, next);
            state.panel(next).view().grab_focus();
        }
    });

    let weak_state = Rc::downgrade(state);
    let weak_window = window.downgrade();
    add_action(window, "activate-entry", move || {
        if let (Some(state), Some(window)) = (weak_state.upgrade(), weak_window.upgrade()) {
            activate_entry(state.active_panel(), &window);
        }
    });

    let weak_state = Rc::downgrade(state);
    add_action(window, "go-parent", move || {
        if let Some(state) = weak_state.upgrade() {
            state.active_panel().navigate_parent();
        }
    });

    let weak_state = Rc::downgrade(state);
    add_action(window, "toggle-mark", move || {
        if let Some(state) = weak_state.upgrade() {
            state.active_panel().toggle_mark_and_advance();
        }
    });

    let weak_state = Rc::downgrade(state);
    add_action(window, "mark-all", move || {
        if let Some(state) = weak_state.upgrade() {
            state.active_panel().mark_all();
        }
    });

    let weak_state = Rc::downgrade(state);
    add_action(window, "clear-marks", move || {
        if let Some(state) = weak_state.upgrade() {
            state.active_panel().clear_marks();
        }
    });

    let weak_state = Rc::downgrade(state);
    add_action(window, "toggle-hidden", move || {
        if let Some(state) = weak_state.upgrade() {
            state.active_panel().toggle_hidden();
        }
    });
    app.set_accels_for_action("win.toggle-hidden", &["<Control>h"]);

    let weak_state = Rc::downgrade(state);
    add_action(window, "reload", move || {
        if let Some(state) = weak_state.upgrade() {
            state.active_panel().reload();
        }
    });
    app.set_accels_for_action("win.reload", &["<Control>r"]);

    let weak_state = Rc::downgrade(state);
    let weak_window = window.downgrade();
    add_action(window, "rename", move || {
        if let (Some(state), Some(window)) = (weak_state.upgrade(), weak_window.upgrade()) {
            begin_rename(&window, &state);
        }
    });
    app.set_accels_for_action("win.rename", &["F2"]);

    let weak_state = Rc::downgrade(state);
    let weak_window = window.downgrade();
    add_action(window, "view", move || {
        if let (Some(state), Some(window)) = (weak_state.upgrade(), weak_window.upgrade()) {
            if let Some(path) = state.active_panel().cursor_path() {
                viewer::open(&window, &path);
            }
        }
    });
    app.set_accels_for_action("win.view", &["F3"]);

    let weak_state = Rc::downgrade(state);
    let weak_window = window.downgrade();
    add_action(window, "copy", move || {
        if let (Some(state), Some(window)) = (weak_state.upgrade(), weak_window.upgrade()) {
            begin_copy_move(&window, &state, OpKind::Copy);
        }
    });
    app.set_accels_for_action("win.copy", &["F5"]);

    let weak_state = Rc::downgrade(state);
    let weak_window = window.downgrade();
    add_action(window, "move", move || {
        if let (Some(state), Some(window)) = (weak_state.upgrade(), weak_window.upgrade()) {
            begin_copy_move(&window, &state, OpKind::Move);
        }
    });
    app.set_accels_for_action("win.move", &["F6"]);

    let weak_state = Rc::downgrade(state);
    let weak_window = window.downgrade();
    add_action(window, "mkdir", move || {
        if let (Some(state), Some(window)) = (weak_state.upgrade(), weak_window.upgrade()) {
            begin_mkdir(&window, &state);
        }
    });
    app.set_accels_for_action("win.mkdir", &["F7"]);

    let weak_state = Rc::downgrade(state);
    let weak_window = window.downgrade();
    add_action(window, "trash", move || {
        if let (Some(state), Some(window)) = (weak_state.upgrade(), weak_window.upgrade()) {
            begin_removal(&window, &state, false);
        }
    });
    app.set_accels_for_action("win.trash", &["F8"]);

    let weak_state = Rc::downgrade(state);
    let weak_window = window.downgrade();
    add_action(window, "delete", move || {
        if let (Some(state), Some(window)) = (weak_state.upgrade(), weak_window.upgrade()) {
            begin_removal(&window, &state, true);
        }
    });
    app.set_accels_for_action("win.delete", &["<Shift>F8"]);

    let weak_window = window.downgrade();
    add_action(window, "close", move || {
        if let Some(window) = weak_window.upgrade() {
            window.close();
        }
    });
    app.set_accels_for_action("win.close", &["<Alt>F4"]);

    let weak_window = window.downgrade();
    add_action(window, "about", move || {
        if let Some(window) = weak_window.upgrade() {
            let about = gtk::AboutDialog::builder()
                .transient_for(&window)
                .modal(true)
                .program_name("Salamander RS")
                .version(env!("CARGO_PKG_VERSION"))
                .comments("Dvoupanelový správce souborů")
                .license_type(gtk::License::Gpl30)
                .build();
            about.present();
        }
    });
}

fn try_begin_busy(state: &WindowState) -> bool {
    if state.busy.get() {
        false
    } else {
        state.busy.set(true);
        true
    }
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
            if !check_new_path(&window, &new_path) || !try_begin_busy(&state) {
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
    if state.busy.get() {
        return;
    }
    let panel = state.active_panel().clone();
    let weak_state = Rc::downgrade(state);
    let weak_window = window.downgrade();
    dialogs::prompt(
        window,
        "Nová složka",
        "Jméno složky:",
        "",
        "Vytvořit",
        move |name| {
            let (Some(state), Some(window)) = (weak_state.upgrade(), weak_window.upgrade()) else {
                return;
            };
            if let Err(message) = validate_name(&name) {
                show_error(&window, &message);
                return;
            }
            let new_path = panel.dir().join(name);
            if !check_new_path(&window, &new_path) || !try_begin_busy(&state) {
                return;
            }
            let worker_path = new_path.clone();
            run_path_mutation(
                &window,
                &state,
                panel,
                new_path,
                "Vytvoření složky selhalo",
                move || fs::create_dir(worker_path),
            );
        },
    );
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
    let paths = state.active_panel().selected_or_cursor();
    if paths.is_empty() {
        return;
    }
    let count = paths.len();
    let (title, message, button) = if permanent {
        (
            "Trvalé smazání",
            format!("Trvale smazat {count} položek?"),
            "Smazat",
        )
    } else {
        (
            "Přesunout do koše",
            format!("Přesunout {count} položek do koše?"),
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
    let source_panel = state.active_panel().clone();
    let sources = source_panel.selected_or_cursor();
    if sources.is_empty() {
        return;
    }
    let destination = state.panel(state.active.get().other()).dir();
    let (title, label, button) = match kind {
        OpKind::Copy => ("Kopírovat", "Cílový adresář:", "Kopírovat"),
        OpKind::Move => ("Přesunout", "Cílový adresář:", "Přesunout"),
    };
    let weak_state = Rc::downgrade(state);
    let weak_window = window.downgrade();
    dialogs::prompt(
        window,
        title,
        label,
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
                source_panel,
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
        self.counters.set_text(&format!(
            "{} / {} souborů, {} / {} bajtů",
            progress.done_files, progress.total_files, progress.done_bytes, progress.total_bytes
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
        let message = gtk::Label::new(Some(&format!(
            "Cíl už existuje.\nZdroj: {}\nCíl: {}",
            conflict.source.to_string_lossy(),
            conflict.target.to_string_lossy()
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
            ("Přepsat", ConflictAction::Overwrite),
            ("Přeskočit", ConflictAction::Skip),
            ("Přepsat vše", ConflictAction::OverwriteAll),
            ("Přeskočit vše", ConflictAction::SkipAll),
            ("Přerušit", ConflictAction::Abort),
        ] {
            let button = gtk::Button::with_label(label);
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
    source_panel: Panel,
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
                        source_panel.clear_marks();
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

fn add_action(window: &gtk::ApplicationWindow, name: &str, activate: impl Fn() + 'static) {
    let action = gio::SimpleAction::new(name, None);
    action.connect_activate(move |_, _| activate());
    window.add_action(&action);
}

fn activate_entry(panel: &Panel, window: &gtk::ApplicationWindow) {
    let Some(path) = panel.cursor_path() else {
        return;
    };
    match fs::metadata(&path) {
        Ok(metadata) if metadata.is_dir() => panel.navigate(&path),
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
