//! Remote server actions of the main window: connecting (with ssh prompts), the password
//! vault, transfers and file operations on SFTP panels.

use std::cell::RefCell;
use std::io;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use gtk::gio;
use gtk::glib;
use gtk::prelude::*;

use super::{
    absolute_in, edit_file, open_file, run_job, run_path_mutation, set_active_panel, show_error,
    try_begin_busy, validate_name, WhichPanel, WindowState,
};
use crate::archive_ui;
use crate::dialogs;
use crate::ops::{self, OpKind};
use crate::panel::Panel;
use crate::remote::askpass::PromptKind;
use crate::remote::conn::Connection;
use crate::remote::servers::{self, Server};
use crate::remote::{self, log, transfer};
use crate::remote_ui;
use crate::sodium::{Key, Secret};
use crate::vault::{self, Vault};
use crate::viewer;

pub(super) const REMOTE_UNSUPPORTED: &str =
    "Na vzdáleném serveru tato operace není podporována. K dispozici je procházení, F3 a F4, \
     kopírování a přesun (F5, F6), F2, F7, Shift+F4, F8, Ctrl+F2 (oprávnění) a Ctrl+T (terminál).";

const VAULT_IDLE_LOCK: Duration = Duration::from_secs(15 * 60);

// ---- Password vault ----

pub(super) type Then = Box<dyn FnOnce(&gtk::ApplicationWindow, &Rc<WindowState>)>;

fn store_key(state: &Rc<WindowState>, key: Key) {
    *state.vault_key.borrow_mut() = Some(key);
    state.vault_used.set(Instant::now());
}

/// Makes sure the vault is unlocked (creating it when `create` is set), then runs `then`.
pub(super) fn ensure_vault(window: &gtk::ApplicationWindow, state: &Rc<WindowState>, create: bool, then: Then) {
    if state.vault_key.borrow().is_some() {
        state.vault_used.set(Instant::now());
        then(window, state);
        return;
    }
    match Vault::load(&vault::default_path()) {
        Err(error) => show_error(window, &error),
        Ok(None) if create => {
            let weak_state = Rc::downgrade(state);
            let weak_window = window.downgrade();
            remote_ui::create_vault(window, move |master| {
                let (Some(master), Some(state), Some(window)) = (master, weak_state.upgrade(), weak_window.upgrade()) else {
                    return;
                };
                state.flash("Vytvářím trezor…");
                derive_in_background(&window, &state, move || Vault::create(&vault::default_path(), &master).map(|(_, key)| key), move |window, state, result| match result {
                    Ok(key) => {
                        store_key(state, key);
                        log::log("trezor", "VAULT", "vytvořen nový trezor hesel");
                        then(window, state);
                    }
                    Err(error) => show_error(window, &error),
                });
            });
        }
        Ok(None) => {}
        Ok(Some(_)) => unlock(window, state, None, then),
    }
}

/// Runs the slow key derivation off the main thread.
fn derive_in_background(
    window: &gtk::ApplicationWindow,
    state: &Rc<WindowState>,
    work: impl FnOnce() -> Result<Key, String> + Send + 'static,
    done: impl FnOnce(&gtk::ApplicationWindow, &Rc<WindowState>, Result<Key, String>) + 'static,
) {
    let (sender, receiver) = async_channel::bounded(1);
    std::thread::spawn(move || {
        let _ = sender.send_blocking(work());
    });
    let weak_state = Rc::downgrade(state);
    let weak_window = window.downgrade();
    glib::spawn_future_local(async move {
        if let (Ok(result), Some(state), Some(window)) = (receiver.recv().await, weak_state.upgrade(), weak_window.upgrade()) {
            done(&window, &state, result);
        }
    });
}

fn unlock(window: &gtk::ApplicationWindow, state: &Rc<WindowState>, error: Option<String>, then: Then) {
    let weak_state = Rc::downgrade(state);
    let weak_window = window.downgrade();
    remote_ui::unlock_vault(window, error.as_deref(), move |master| {
        let (Some(master), Some(state), Some(window)) = (master, weak_state.upgrade(), weak_window.upgrade()) else {
            return;
        };
        state.flash("Odemykám trezor…");
        derive_in_background(
            &window,
            &state,
            move || {
                Vault::load(&vault::default_path())?
                    .ok_or_else(|| "Trezor neexistuje".to_owned())?
                    .unlock(&master)
            },
            move |window, state, result| match result {
                Ok(key) => {
                    store_key(state, key);
                    log::log("trezor", "VAULT", "odemčen");
                    then(window, state);
                }
                Err(error) => {
                    log::log("trezor", "VAULT", &format!("neúspěšné odemčení: {error}"));
                    unlock(window, state, Some(error), then);
                }
            },
        );
    });
}

fn vault_get(state: &Rc<WindowState>, id: &str) -> Option<Secret> {
    let key = state.vault_key.borrow();
    let key = key.as_ref()?;
    state.vault_used.set(Instant::now());
    Vault::load(&vault::default_path()).ok().flatten()?.get(key, id).ok().flatten()
}

fn vault_has(id: &str) -> bool {
    Vault::load(&vault::default_path())
        .ok()
        .flatten()
        .is_some_and(|vault| vault.contains(id))
}

fn vault_store(window: &gtk::ApplicationWindow, state: &Rc<WindowState>, server: &Server, secret: Secret) {
    let id = server.id.clone();
    let label = server.label();
    ensure_vault(
        window,
        state,
        true,
        Box::new(move |window, state| {
            let result = Vault::load(&vault::default_path()).and_then(|vault| {
                let mut vault = vault.ok_or_else(|| "Trezor neexistuje".to_owned())?;
                let key = state.vault_key.borrow();
                vault.set(key.as_ref().ok_or_else(|| "Trezor je zamčený".to_owned())?, &id, &secret)
            });
            match result {
                Ok(()) => {
                    log::log(&label, "VAULT", "heslo uloženo do trezoru");
                    state.flash("Heslo uloženo do šifrovaného trezoru");
                }
                Err(error) => show_error(window, &error),
            }
        }),
    );
}

fn vault_forget(server: &Server) {
    if let Ok(Some(mut vault)) = Vault::load(&vault::default_path()) {
        if vault.contains(&server.id) && vault.remove(&server.id).is_ok() {
            log::log(&server.label(), "VAULT", "uložené heslo smazáno");
        }
    }
}

pub(super) fn lock_vault(state: &Rc<WindowState>) {
    if state.vault_key.borrow_mut().take().is_some() {
        log::log("trezor", "VAULT", "zamčen");
        state.flash("Trezor hesel zamčen");
    }
}

pub(super) fn change_master(window: &gtk::ApplicationWindow, state: &Rc<WindowState>) {
    match Vault::load(&vault::default_path()) {
        Ok(None) => return show_error(window, "Trezor hesel zatím neexistuje; vytvoří se při prvním uložení hesla."),
        Err(error) => return show_error(window, &error),
        Ok(Some(_)) => {}
    }
    ensure_vault(
        window,
        state,
        false,
        Box::new(|window, state| {
            let weak_state = Rc::downgrade(state);
            let weak_window = window.downgrade();
            remote_ui::new_master_dialog(window, "Změnit hlavní heslo", "Všechna uložená hesla se znovu zašifrují novým klíčem.", move |master| {
                let (Some(master), Some(state), Some(window)) = (master, weak_state.upgrade(), weak_window.upgrade()) else {
                    return;
                };
                let Some(old_key) = state.vault_key.borrow_mut().take() else { return };
                derive_in_background(
                    &window,
                    &state,
                    move || {
                        let mut vault = Vault::load(&vault::default_path())?.ok_or_else(|| "Trezor neexistuje".to_owned())?;
                        vault.change_master(&old_key, &master)
                    },
                    |window, state, result| match result {
                        Ok(key) => {
                            store_key(state, key);
                            log::log("trezor", "VAULT", "hlavní heslo změněno");
                            state.flash("Hlavní heslo změněno");
                        }
                        Err(error) => show_error(window, &error),
                    },
                );
            });
        }),
    );
}

/// Locks the vault after a period without use.
pub(super) fn install_vault_autolock(state: &Rc<WindowState>) {
    let weak = Rc::downgrade(state);
    glib::timeout_add_seconds_local(60, move || {
        let Some(state) = weak.upgrade() else {
            return glib::ControlFlow::Break;
        };
        if state.vault_key.borrow().is_some() && state.vault_used.get().elapsed() > VAULT_IDLE_LOCK {
            lock_vault(&state);
        }
        glib::ControlFlow::Continue
    });
}

// ---- Connecting ----

struct PromptRequest {
    kind: PromptKind,
    message: String,
    reply: mpsc::Sender<Option<Secret>>,
}

pub(super) fn open_server_manager(window: &gtk::ApplicationWindow, state: &Rc<WindowState>) {
    let which = state.active.get();
    let connect_state = Rc::downgrade(state);
    let connect_window = window.downgrade();
    let saved_state = Rc::downgrade(state);
    let saved_window = window.downgrade();
    remote_ui::server_manager(
        window,
        remote_ui::ManagerActions {
            connect: Box::new(move |server| {
                if let (Some(state), Some(window)) = (connect_state.upgrade(), connect_window.upgrade()) {
                    connect_server(&window, &state, server, which);
                }
            }),
            saved: Box::new(move |server, password, forget| {
                if forget {
                    vault_forget(server);
                }
                if let (Some(password), Some(state), Some(window)) = (password, saved_state.upgrade(), saved_window.upgrade()) {
                    vault_store(&window, &state, server, password);
                }
            }),
            deleted: Box::new(|server| {
                vault_forget(server);
                if let Some(connection) = remote::conn::get(&server.id) {
                    std::thread::spawn(move || connection.disconnect());
                }
            }),
            has_password: Box::new(vault_has),
        },
    );
}

pub(super) fn connect_server(window: &gtk::ApplicationWindow, state: &Rc<WindowState>, server: Server, which: WhichPanel) {
    if let Some(connection) = remote::conn::get(&server.id) {
        open_remote_dir(state, which, &connection);
        return;
    }
    // A stored password needs the vault unlocked first; then try again.
    if server.save_password && vault_has(&server.id) && state.vault_key.borrow().is_none() {
        ensure_vault(
            window,
            state,
            false,
            Box::new(move |window, state| connect_server(window, state, server, which)),
        );
        return;
    }
    let stored = if server.save_password { vault_get(state, &server.id) } else { None };
    start_connection(window, state, server, which, stored);
}

fn open_remote_dir(state: &Rc<WindowState>, which: WhichPanel, connection: &Connection) {
    let dir = if connection.server.initial_dir.is_empty() {
        connection.home.clone()
    } else {
        remote::normalize(&connection.home.join(&connection.server.initial_dir))
    };
    let panel = state.panel(which);
    panel.navigate(&remote::virtual_path(&connection.server.id, &dir));
    set_active_panel(state, which);
    panel.view().grab_focus();
}

fn start_connection(
    window: &gtk::ApplicationWindow,
    state: &Rc<WindowState>,
    server: Server,
    which: WhichPanel,
    stored: Option<Secret>,
) {
    if !try_begin_busy(state) {
        return;
    }
    let label = server.label();
    let cancel = Arc::new(AtomicBool::new(false));
    let progress_cancel = Arc::clone(&cancel);
    let progress = dialogs::Progress::new(window, &format!("Připojuji k {}", server.display_name()), move || {
        progress_cancel.store(true, Ordering::Relaxed)
    });
    progress.update(&format!("ssh {label}"));

    let (prompt_sender, prompt_receiver) = async_channel::unbounded::<PromptRequest>();
    let stored = Arc::new(Mutex::new(stored));
    let handler_cancel = Arc::clone(&cancel);
    let handler_label = label.clone();
    let handler: remote::conn::PromptHandler = Box::new(move |kind, message| {
        if handler_cancel.load(Ordering::Relaxed) {
            return None;
        }
        if kind == PromptKind::Password {
            // The stored password is offered once; a wrong one makes ssh ask again.
            if let Some(password) = stored.lock().unwrap().take() {
                log::log(&handler_label, "AUTH", "heslo z trezoru");
                return Some(password);
            }
        }
        let (reply, answer) = mpsc::channel();
        prompt_sender
            .send_blocking(PromptRequest {
                kind,
                message: message.to_owned(),
                reply,
            })
            .ok()?;
        answer.recv().ok().flatten()
    });

    // Prompts from ssh are answered by dialogs on the main thread.
    let save_request: Rc<RefCell<Option<Secret>>> = Rc::new(RefCell::new(None));
    {
        let weak_window = window.downgrade();
        let save_request = Rc::clone(&save_request);
        let save_default = server.save_password;
        let label = label.clone();
        glib::spawn_future_local(async move {
            while let Ok(request) = prompt_receiver.recv().await {
                let Some(window) = weak_window.upgrade() else { break };
                let PromptRequest { kind, message, reply } = request;
                match kind {
                    PromptKind::HostKey => {
                        log::log(&label, "HOSTKEY", "neznámý klíč serveru, žádost o potvrzení");
                        let fingerprint = message
                            .lines()
                            .find(|line| line.contains("fingerprint"))
                            .unwrap_or("")
                            .trim()
                            .to_owned();
                        let label = label.clone();
                        remote_ui::confirm_host_key(&window, &message, move |trusted| {
                            log::log(&label, "HOSTKEY", &format!("{} {fingerprint}", if trusted { "přijat" } else { "odmítnut" }));
                            let _ = reply.send(Some(Secret::from_str(if trusted { "yes" } else { "no" })));
                        });
                    }
                    PromptKind::Password => {
                        log::log(&label, "AUTH", "vyžádáno heslo");
                        let save_request = Rc::clone(&save_request);
                        remote_ui::ask_secret(&window, "Heslo k serveru", &message, Some(save_default), move |answer| {
                            let _ = reply.send(answer.map(|(secret, save)| {
                                if save {
                                    *save_request.borrow_mut() = Some(secret.clone_secret());
                                }
                                secret
                            }));
                        });
                    }
                    PromptKind::Passphrase | PromptKind::Other => {
                        log::log(&label, "AUTH", if kind == PromptKind::Passphrase { "vyžádáno heslo ke klíči" } else { "vyžádán ověřovací údaj" });
                        let title = if kind == PromptKind::Passphrase { "Heslo ke klíči" } else { "Ověření" };
                        remote_ui::ask_secret(&window, title, &message, None, move |answer| {
                            let _ = reply.send(answer.map(|(secret, _)| secret));
                        });
                    }
                }
            }
        });
    }

    let (sender, receiver) = async_channel::bounded(1);
    let worker_server = server.clone();
    std::thread::spawn(move || {
        let _ = sender.send_blocking(Connection::connect(worker_server, handler));
    });
    let weak_state = Rc::downgrade(state);
    let weak_window = window.downgrade();
    glib::spawn_future_local(async move {
        let Ok(result) = receiver.recv().await else { return };
        progress.finish();
        let (Some(state), Some(window)) = (weak_state.upgrade(), weak_window.upgrade()) else { return };
        state.busy.set(false);
        match result {
            Ok(connection) if cancel.load(Ordering::Relaxed) => {
                std::thread::spawn(move || connection.disconnect());
            }
            Ok(connection) => {
                open_remote_dir(&state, which, &connection);
                state.flash(&format!("Připojeno: {}", connection.label()));
                if let Some(password) = save_request.borrow_mut().take() {
                    let mut saved = connection.server.clone();
                    if !saved.save_password {
                        saved.save_password = true;
                        let path = servers::default_path();
                        let mut list = servers::load(&path);
                        if let Some(entry) = list.iter_mut().find(|item| item.id == saved.id) {
                            entry.save_password = true;
                            let _ = servers::save(&path, &list);
                        }
                    }
                    vault_store(&window, &state, &saved, password);
                }
            }
            Err(_) if cancel.load(Ordering::Relaxed) => state.flash("Připojení zrušeno"),
            Err(error) => show_error(&window, &format!("Nelze se připojit k {}:\n{error}", server.display_name())),
        }
    });
}

pub(super) fn disconnect_active(window: &gtk::ApplicationWindow, state: &Rc<WindowState>) {
    let Some((id, _)) = state.active_panel().remote() else {
        show_error(window, "Aktivní panel není připojen k serveru");
        return;
    };
    let Some(connection) = remote::conn::get(&id) else { return };
    let label = connection.label();
    std::thread::spawn(move || connection.disconnect());
    for panel in [&state.left, &state.right] {
        if panel.remote().is_some_and(|(panel_id, _)| panel_id == id) {
            panel.navigate_home();
        }
    }
    state.flash(&format!("Odpojeno: {label}"));
}

/// Closes every connection (application exit).
pub(super) fn disconnect_all() {
    for connection in remote::conn::all() {
        connection.disconnect();
    }
}

// ---- Operations on remote panels ----

/// `(connection, remote path, is directory)` of the cursor entry in a remote panel.
fn remote_cursor(panel: &Panel) -> Option<(Arc<Connection>, PathBuf, bool)> {
    let (id, _) = panel.remote()?;
    let connection = remote::conn::get(&id)?;
    let entry = panel.cursor_entry()?;
    let (path, is_dir) = entry.with_entry(|item| (item.path.clone(), item.is_dir_like()));
    let (_, remote) = remote::split_virtual(&path)?;
    Some((connection, remote, is_dir))
}

fn panel_connection(window: &gtk::ApplicationWindow, panel: &Panel) -> Option<(Arc<Connection>, PathBuf)> {
    let (id, dir) = panel.remote()?;
    match remote::conn::get(&id) {
        Some(connection) => Some((connection, dir)),
        None => {
            show_error(window, "Server už není připojen; připojte se znovu (Ctrl+K)");
            None
        }
    }
}

fn with_download(window: &gtk::ApplicationWindow, connection: Arc<Connection>, remote: PathBuf, then: impl FnOnce(&gtk::ApplicationWindow, PathBuf) + 'static) {
    let (sender, receiver) = async_channel::bounded(1);
    std::thread::spawn(move || {
        let _ = sender.send_blocking(transfer::download_to_temp(&connection, &remote));
    });
    let weak_window = window.downgrade();
    glib::spawn_future_local(async move {
        let (Ok(result), Some(window)) = (receiver.recv().await, weak_window.upgrade()) else { return };
        match result {
            Ok(path) => then(&window, path),
            Err(error) => show_error(&window, &format!("Nelze stáhnout: {error}")),
        }
    });
}

pub(super) fn remote_activate(window: &gtk::ApplicationWindow, panel: &Panel) {
    let Some((connection, remote_path, is_dir)) = remote_cursor(panel) else { return };
    if is_dir {
        panel.navigate(&remote::virtual_path(&connection.server.id, &remote_path));
    } else {
        with_download(window, connection, remote_path, open_file);
    }
}

pub(super) fn remote_view(window: &gtk::ApplicationWindow, state: &Rc<WindowState>) {
    if let Some((connection, remote_path, false)) = remote_cursor(state.active_panel()) {
        with_download(window, connection, remote_path, |window, path| viewer::open(window, &path));
    }
}

pub(super) fn remote_open_external(window: &gtk::ApplicationWindow, state: &Rc<WindowState>) {
    if let Some((connection, remote_path, false)) = remote_cursor(state.active_panel()) {
        with_download(window, connection, remote_path, open_file);
    }
}

/// F4: edits a downloaded copy and uploads it back whenever the editor saves it.
pub(super) fn remote_edit(window: &gtk::ApplicationWindow, state: &Rc<WindowState>) {
    let Some((connection, remote_path, is_dir)) = remote_cursor(state.active_panel()) else { return };
    if is_dir {
        return show_error(window, "Adresář nelze upravit; použijte Enter");
    }
    let weak_state = Rc::downgrade(state);
    let upload_connection = Arc::clone(&connection);
    with_download(window, connection, remote_path.clone(), move |window, local| {
        let Some(state) = weak_state.upgrade() else { return };
        let Ok(monitor) = gio::File::for_path(&local).monitor_file(gio::FileMonitorFlags::NONE, gio::Cancellable::NONE) else {
            return show_error(window, "Nelze sledovat změny upravovaného souboru");
        };
        let pending = Rc::new(std::cell::Cell::new(false));
        let weak_state = Rc::downgrade(&state);
        let remote_dir = remote_path.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("/"));
        let local_path = local.clone();
        monitor.connect_changed(move |_, _, _, event| {
            if !matches!(event, gio::FileMonitorEvent::ChangesDoneHint | gio::FileMonitorEvent::Created) || pending.replace(true) {
                return;
            }
            // Editors save in bursts; upload once things settle.
            let pending = Rc::clone(&pending);
            let weak_state = weak_state.clone();
            let connection = Arc::clone(&upload_connection);
            let local_path = local_path.clone();
            let remote_dir = remote_dir.clone();
            glib::timeout_add_local_once(Duration::from_millis(500), move || {
                pending.set(false);
                let (sender, receiver) = async_channel::bounded(1);
                std::thread::spawn(move || {
                    let result = transfer::upload(&connection, &[local_path], &remote_dir, true, &AtomicBool::new(false), &mut |_| {});
                    let _ = sender.send_blocking(result);
                });
                glib::spawn_future_local(async move {
                    let (Ok(result), Some(state)) = (receiver.recv().await, weak_state.upgrade()) else { return };
                    match result.errors.first() {
                        None => {
                            state.flash("Změny nahrány na server");
                            state.left.reload();
                            state.right.reload();
                        }
                        Some(error) => state.flash(&format!("Nahrání na server selhalo: {}", error.error)),
                    }
                });
            });
        });
        state.remote_edits.borrow_mut().push(monitor);
        edit_file(window, &local);
        state.flash("Upravuje se kopie souboru; každé uložení v editoru se nahraje zpět na server");
    });
}

/// Download dialog and job for F5/F6 from a remote panel.
pub(super) fn begin_download(window: &gtk::ApplicationWindow, state: &Rc<WindowState>, kind: OpKind) {
    let source_panel = state.active_panel().clone();
    let Some((connection, _)) = panel_connection(window, &source_panel) else { return };
    let items = source_panel.remote_selection();
    if items.is_empty() {
        return;
    }
    let dest = state.other_panel().dir();
    let base_dest = dest.clone();
    let weak_state = Rc::downgrade(state);
    let weak_window = window.downgrade();
    let (title, verb, button) = match kind {
        OpKind::Copy => ("Stáhnout ze serveru", "Stáhnout", "_Stáhnout"),
        OpKind::Move => ("Přesunout ze serveru", "Přesunout", "_Přesunout"),
    };
    archive_ui::unpack_dialog(
        window,
        title,
        &format!("{verb} {} položek z {} do:", items.len(), connection.label()),
        &dest,
        false,
        button,
        move |options| {
            let (Some(state), Some(window)) = (weak_state.upgrade(), weak_window.upgrade()) else { return };
            let dest = absolute_in(&base_dest, &options.dest.to_string_lossy());
            run_job(
                &window,
                &state,
                title,
                Some(source_panel),
                Box::new(move |cancel, progress| {
                    if let Err(error) = std::fs::create_dir_all(&dest) {
                        let mut result = crate::archive::JobResult::default();
                        result.errors.push(crate::model::OpError { source: dest, target: None, error: error.to_string() });
                        return result;
                    }
                    let mut result = transfer::download(&connection, &items, &dest, options.overwrite, cancel, progress);
                    // A move deletes the remote originals only when everything arrived.
                    if kind == OpKind::Move && result.errors.is_empty() && !result.cancelled {
                        let deleted = transfer::delete(&connection, &items, cancel, progress);
                        result.errors.extend(deleted.errors);
                    }
                    result
                }),
            );
        },
    );
}

/// Upload dialog and job for F5/F6 into a remote panel.
pub(super) fn begin_upload(window: &gtk::ApplicationWindow, state: &Rc<WindowState>, kind: OpKind) {
    let source_panel = state.active_panel().clone();
    let Some((connection, remote_dir)) = panel_connection(window, state.other_panel()) else { return };
    let shown_dir = remote_dir.clone();
    let sources = source_panel.selected_or_cursor();
    if sources.is_empty() {
        return;
    }
    let weak_state = Rc::downgrade(state);
    let weak_window = window.downgrade();
    let (title, verb, button) = match kind {
        OpKind::Copy => ("Nahrát na server", "Nahrát", "_Nahrát"),
        OpKind::Move => ("Přesunout na server", "Přesunout", "_Přesunout"),
    };
    archive_ui::unpack_dialog(
        window,
        title,
        &format!("{verb} {} položek na {} do adresáře:", sources.len(), connection.label()),
        &shown_dir,
        false,
        button,
        move |options| {
            let (Some(state), Some(window)) = (weak_state.upgrade(), weak_window.upgrade()) else { return };
            let target = remote::normalize(&remote_dir.join(&options.dest));
            run_job(
                &window,
                &state,
                title,
                Some(source_panel),
                Box::new(move |cancel, progress| {
                    let mut result = transfer::upload(&connection, &sources, &target, options.overwrite, cancel, progress);
                    if kind == OpKind::Move && result.errors.is_empty() && !result.cancelled {
                        result.errors.extend(ops::delete_permanently(sources));
                    }
                    result
                }),
            );
        },
    );
}

pub(super) fn begin_remote_delete(window: &gtk::ApplicationWindow, state: &Rc<WindowState>) {
    let panel = state.active_panel();
    let Some((connection, _)) = panel_connection(window, panel) else { return };
    let items = panel.remote_selection();
    if items.is_empty() {
        return;
    }
    let what = if items.len() == 1 {
        format!("„{}“", items[0].to_string_lossy())
    } else {
        format!("{} položek", items.len())
    };
    let weak_state = Rc::downgrade(state);
    let weak_window = window.downgrade();
    dialogs::confirm(
        window,
        "Smazat na serveru",
        &format!("Trvale smazat {what} na {}? Na serveru není koš, akci nelze vrátit.", connection.label()),
        "Smazat",
        move || {
            let (Some(state), Some(window)) = (weak_state.upgrade(), weak_window.upgrade()) else { return };
            run_job(&window, &state, "Mažu na serveru", None, Box::new(move |cancel, progress| transfer::delete(&connection, &items, cancel, progress)));
        },
    );
}

pub(super) fn remote_mkdir(window: &gtk::ApplicationWindow, state: &Rc<WindowState>) {
    remote_create(window, state, false);
}

pub(super) fn remote_new_file(window: &gtk::ApplicationWindow, state: &Rc<WindowState>) {
    remote_create(window, state, true);
}

fn remote_create(window: &gtk::ApplicationWindow, state: &Rc<WindowState>, file: bool) {
    let panel = state.active_panel().clone();
    let Some((connection, dir)) = panel_connection(window, &panel) else { return };
    let weak_state = Rc::downgrade(state);
    let weak_window = window.downgrade();
    let (title, label) = if file { ("Nový soubor na serveru", "Jméno souboru:") } else { ("Nová složka na serveru", "Jméno složky:") };
    dialogs::prompt(window, title, label, "", "Vytvořit", move |name| {
        let (Some(state), Some(window)) = (weak_state.upgrade(), weak_window.upgrade()) else { return };
        if let Err(message) = validate_name(&name) {
            return show_error(&window, &message);
        }
        if !try_begin_busy(&state) {
            return;
        }
        let path = dir.join(&name);
        let focus = remote::virtual_path(&connection.server.id, &path);
        run_path_mutation(&window, &state, panel, focus, if file { "Vytvoření souboru selhalo" } else { "Vytvoření složky selhalo" }, move || {
            if file { transfer::create_file(&connection, &path) } else { transfer::mkdir(&connection, &path) }.map_err(io::Error::other)
        });
    });
}

pub(super) fn remote_rename(window: &gtk::ApplicationWindow, state: &Rc<WindowState>) {
    let panel = state.active_panel().clone();
    let Some((connection, old, _)) = remote_cursor(&panel) else { return };
    let initial = old.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    let weak_state = Rc::downgrade(state);
    let weak_window = window.downgrade();
    dialogs::prompt(window, "Přejmenovat na serveru", "Nové jméno:", &initial, "Přejmenovat", move |name| {
        let (Some(state), Some(window)) = (weak_state.upgrade(), weak_window.upgrade()) else { return };
        if let Err(message) = validate_name(&name) {
            return show_error(&window, &message);
        }
        let new = old.parent().unwrap_or(Path::new("/")).join(&name);
        if new == old || !try_begin_busy(&state) {
            return;
        }
        let focus = remote::virtual_path(&connection.server.id, &new);
        run_path_mutation(&window, &state, panel, focus, "Přejmenování selhalo", move || {
            transfer::rename(&connection, &old, &new).map_err(io::Error::other)
        });
    });
}

pub(super) fn remote_chmod(window: &gtk::ApplicationWindow, state: &Rc<WindowState>) {
    let panel = state.active_panel().clone();
    let Some((connection, _)) = panel_connection(window, &panel) else { return };
    let items = panel.remote_selection();
    if items.is_empty() {
        return;
    }
    let current = panel
        .cursor_entry()
        .map(|entry| entry.with_entry(|item| crate::model::format_octal(item.mode)))
        .unwrap_or_else(|| "0644".to_owned());
    let weak_state = Rc::downgrade(state);
    let weak_window = window.downgrade();
    dialogs::prompt(
        window,
        "Oprávnění na serveru",
        &format!("Nová oprávnění pro {} položek (osmičkově, např. 0644 nebo 0755):", items.len()),
        &current,
        "Nastavit",
        move |text| {
            let (Some(state), Some(window)) = (weak_state.upgrade(), weak_window.upgrade()) else { return };
            let Some(mode) = crate::attrs::parse_octal(&text) else {
                return show_error(&window, "Zadejte oprávnění osmičkově, např. 0644");
            };
            run_job(&window, &state, "Měním oprávnění na serveru", None, Box::new(move |cancel, progress| {
                let mut result = crate::archive::JobResult::default();
                for item in &items {
                    if cancel.load(Ordering::Relaxed) {
                        result.cancelled = true;
                        break;
                    }
                    progress(item);
                    match transfer::chmod(&connection, item, mode) {
                        Ok(()) => result.done += 1,
                        Err(error) => result.errors.push(crate::model::OpError { source: item.clone(), target: None, error }),
                    }
                }
                result
            }));
        },
    );
}

pub(super) fn remote_info(window: &gtk::ApplicationWindow, state: &Rc<WindowState>) {
    let panel = state.active_panel();
    let (Some((connection, remote_path, _)), Some(entry)) = (remote_cursor(panel), panel.cursor_entry()) else { return };
    let size = entry.display_size();
    let text = entry.with_entry(|item| {
        let mut text = format!(
            "Název: {}\nCesta: {}\nServer: {} ({})\nVelikost: {size}\nZměněno: {}\nOprávnění: {}  ({})\nVlastník: {}:{}",
            item.name.to_string_lossy(),
            remote_path.to_string_lossy(),
            connection.server.display_name(),
            connection.label(),
            crate::model::format_time(item.modified, "%Y-%m-%d %H:%M:%S"),
            item.mode_text(),
            crate::model::format_octal(item.mode),
            item.owner,
            item.group
        );
        if let Some(target) = &item.link_target {
            text.push_str(&format!("\nCíl odkazu: {}", target.to_string_lossy()));
        }
        text
    });
    dialogs::info(window, "Vlastnosti vzdálené položky", &text);
}

/// Ctrl+T on a remote panel: an ssh session in a terminal, in the current directory.
pub(super) fn remote_terminal(window: &gtk::ApplicationWindow, state: &Rc<WindowState>) {
    let Some((connection, dir)) = panel_connection(window, state.active_panel()) else { return };
    let command = connection.terminal_command(&dir);
    log::log(&connection.label(), "TERMINAL", &dir.to_string_lossy());
    // The local working directory does not matter for ssh; HOME always exists.
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"));
    if let Err(message) = crate::terminal::launch(&home, Some(&command)) {
        show_error(window, &message);
    }
}

pub(super) fn show_remote_log(window: &gtk::ApplicationWindow) {
    let path = log::log_path();
    if path.is_file() {
        viewer::open(window, &path);
    } else {
        dialogs::info(window, "Log vzdálených operací", &format!("Log je zatím prázdný.\n{}", path.to_string_lossy()));
    }
}
