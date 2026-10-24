# Salamander RS — Rust dvoupanelový správce souborů pro Ubuntu 24.04+ (GTK4)

## Context

Nová Rust aplikace od nuly (prázdný adresář `C:/Projects/Test K4.1`): dvoupanelový správce souborů inspirovaný OpenSalamanderem (https://github.com/OpenSalamander/salamander — dual-pane správce pro Windows, GPLv2; zde kód nekopírujeme, jde o nezávislou implementaci v Rustu). Cíl: aplikace běžící na Ubuntu 24.04 a novějších, včetně flavoru LXQt (X11 i Wayland). Uživatel zvolil: **GTK4 přes crate gtk-rs**, rozsah **"jádro Salamandera"**, ovládání **klávesnice-first**, distribuce jako **.deb (cargo-deb)**.

Ověřená fakta (zjištěná tento týden):
- `gtk4` crate verze **0.11.5** (Sep 2026), MSRV Rust **1.83**, MIT; výchozí features cílí GTK 4.0 API — Ubuntu 24.04 má GTK 4.14, vše potřebné (Paned, TreeView, ListStore, EventControllerKey, Dialog) je API úrovně 4.0.
- `trash` crate verze **5.2.9** (Sep 2026), MIT, podporuje FreeDesktop Trash (funguje na GNOME, KDE, LXQt…). API: `trash::delete(&[path])` / `trash::delete_all`.
- `cargo-deb` verze **3.8.0**; konfigurace přes `[package.metadata.deb]` v `Cargo.toml`, binárka v `target/release/`, výstup `target/debian/*.deb`.

## Rozhodnutá design volby (implementer je nemění)

- Jméno: crate a binárka **`salamander-rs`**, GTK application-id **`cz.k4.salamander-rs`**, display name **"Salamander RS"**.
- Souborové seznamy: `gtk::TreeView` + `gtk::ListStore` (není deprecated v GTK4, jednodušší než ListView/GObject subclassing, zvládne tisíce řádků).
- Vláknový model: GUI vlastní hlavní vlákno; copy/move/delete běží ve worker vlákně; komunikace přes `glib::MainContext::channel` (progress/finish) a `std::sync::mpsc` (odpovědi UI→worker na konflikty); cancel přes `std::sync::atomic::AtomicBool`.
- Žádný konfigurační soubor, žádné pluginy, žádné archivy, žádné find — rozsah "jádro". Žádné ukládání stavu na disk.
- F8 maže do FreeDesktop koše (`trash` crate); Shift+F8 trvale maže po potvrzení.
- Symlinky: při kopíru se přenáší jako symlink (znovu vytvořený); move používá `fs::rename`, při selhání (různý filesystém, EXDEV) fallback copy+delete.
- Formát velikosti: vlastní `format_size` (binární, "1.5 MB"); formát data: `chrono` "YYYY-MM-DD HH:MM".
- Otevírání souborů: `std::process::Command::new("xdg-open")`.

## Approach

Postupně, každý krok končí stavem, kdy `cargo build` (resp. `cargo test`) prochází. Veškerý vývoj a build **na Ubuntu 24.04+** (nativní, VM, nebo WSL2 s WSLg — GTK4 GUI funguje ve WSLg). Prerekvizity na Ubuntu:

```bash
sudo apt install build-essential pkg-config libgtk-4-dev
# Rust přes rustup (apt má starou verzi, MSRV 1.83): curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

### Krok 1 — Scaffold

Vytvoř `Cargo.toml`:

```toml
[package]
name = "salamander-rs"
version = "0.1.0"
edition = "2021"
license = "GPL-3.0-or-later"
description = "Dual-pane file manager for Ubuntu, inspired by Open Salamander"

[dependencies]
gtk = { package = "gtk4", version = "0.11" }
trash = "5"
chrono = "0.4"

[dev-dependencies]
tempfile = "3"
```

`src/main.rs`: `gtk::Application` s `application_id("cz.k4.salamander-rs")`, `connect_activate` volá `window::build(app)`; vrátí `app.run()`. Zatím prázdné okno `gtk::ApplicationWindow` (default 1200×700, title "Salamander RS"). Zbytek souborů vytvoř v tomto kroku jako prázdné moduly: `window.rs`, `panel.rs`, `ops.rs`, `dialogs.rs`, `viewer.rs` (deklarace `mod` v `main.rs`).

### Krok 2 — Panel (`src/panel.rs`)

`pub struct Panel` zapouzdřující jeden panel:

```rust
pub struct Panel {
    pub root: gtk::Box,          // svislá: path entry + treeview (ve ScrolledWindow)
    pub path_entry: gtk::Entry,  // hlavička panelu = aktuální adresář, editovatelná
    pub view: gtk::TreeView,
    pub store: gtk::ListStore,   // sloupce viz níže
    pub dir: PathBuf,
    pub show_hidden: bool,
}
```

`ListStore` sloupce (pořadí je load-bearing, indexuje se dál v kódu):
- 0: `String` jméno souboru
- 1: `String` formátovaná velikost (adresáře: `<dir>`)
- 2: `String` modifikace "YYYY-MM-DD HH:MM"
- 3: `Bool` `is_dir`
- 4: `String` celá cesta
- 5: `Bool` `hidden` (začíná `.`)

Viditelné sloupce TreeView: Name, Size, Modified — `TreeViewColumn` s `set_resizable(true)` a klikací hlavičkou (`set_sort_column_id`). Řazení: na sloupci 0 custom `set_sort_func` — adresáře vždy první, pak case-insensitive porovnání jmen; sloupec 1 řadí dle skutečné velikosti (ulož navíc `i64` byty jako sloupec 6, řaď podle něj, zobraz 1).

Funkce:
- `Panel::new(start_dir: &Path) -> Panel` — start_dir = domovský adresář (`std::env::var("HOME")`), při chybě `/` .
- `Panel::reload(&self)` — `fs::read_dir`, na každou položku `fs::symlink_metadata` (nesledovat symlink — jinak by symlink na adresář vypadal jako adresář a symlink smyčky rozbily listing), naplní store; prázdný adresář = prázdný seznam; při `read_dir` chybě ( permission) `gtk::AlertDialog` "Nelze číst adresář: {path}: {err}" a zůstane původní obsah.
- `Panel::cd(&mut self, path: &Path)` — nastaví `dir`, aktualizuje `path_entry`, `reload`; po reload zachová kurzor na stejném jménu (pokud v novém listingu existuje), jinak řádek 0.
- `Panel::selected_or_cursor(&self) -> Vec<PathBuf>` — vybrané řádky přes `TreeSelection::selected_rows()` + sloupec 4; když je výběr prázdný, vrátí řádek pod kurzorem; když je i kurzor mimo (prázdný adresář), vrátí `vec![]`.
- Výběr: `TreeSelection` mode `Multiple`; `Insert`/`Space` = toggle vybranosti řádku pod kurzorem + posun kurzoru o řádek dolů (chování Norton Commandera).
- `path_entry` `activate` signál → `Panel::cd` na zadanou cestu (expand `~` přes `shellexpand`? NE — ručně: pokud začíná `~/`, nahraď za `HOME`).
- Dvojklik (`row_activated`) = stejná akce jako Enter (krok 4).

### Krok 3 — Hlavní okno (`src/window.rs`)

`pub fn build(app: &gtk::Application)`:

- `gtk::ApplicationWindow`; uvnitř svislá `gtk::Box`: `gtk::MenuBar` + `gtk::Paned` (horizontální, position 600).
- `MenuBar` (přes `gtk::PopoverMenuBar.from_model` nebo klasický `gtk::MenuBar` s `gio::Menu` — zvol `gio::Menu` + `PopoverMenuBar`): menu **File** (New Folder F7, Copy F5, Move F6, Rename F4, Delete F8, Delete permanently Shift+F8, Quit F10), **View** (Show hidden files Ctrl+H, Refresh Ctrl+R), **Help** (About). Menu itemy emitují stejný `ActionMap` akce jako klávesy (viz níže) — žádné duplicity logiky, `gio::SimpleAction` `win.copy` atd. s `accelerator` nastavenými jen informativně; F-keys se zpracovávají ručně (viz keymap).
- Dva `Panel`y v `Paned`; stav `struct AppState { left: Panel, right: Panel, active: Rc<Cell<WhichPanel>> }` kde `enum WhichPanel { Left, Right }`. `WhichPanel::other()` pro cíl operace. Aktivní panel se vizuálně odliší `set_sensitive`? Ne — fokus: `view.grab_focus()` na aktivním panelu; aktivní = ten, jehož `view` má fokus; `Tab` = `other.view.grab_focus()`.
- Fokus stopy: `connect_state_flags_changed` na obou `view` — při `FocUSED` flag nastaví `active`.

Klávesová mapa — `gtk::EventControllerKey` na window, `connect_key_pressed`:

| Klávesa | Akce |
|---|---|
| `Tab` | přepnout aktivní panel |
| `F3` | viewer (krok 6) |
| `F4` | rename (krok 5) |
| `F5` | copy (krok 5) |
| `F6` | move (krok 5) |
| `F7` | nová složka (krok 5) |
| `F8` | smazat do koše (krok 5) |
| `Shift+F8` | trvale smazat s potvrzením (krok 5) |
| `F10` | `window.close()` |
| `Ctrl+H` | toggle `show_hidden` + reload aktivního panelu |
| `Ctrl+R` | reload aktivního panelu |
| `Ctrl+A` / `Ctrl+Shift+A` | vybrat vše / zrušit výběr |
| `Enter` | na adresáři `cd`, na souboru `xdg-open` (spawn, ignoruj chybu spawnu `Ok(_)`) |
| `BackSpace` | `cd` do rodiče (`dir.parent()`, `/` zůstává `/`) |

Detekce: porovnání `event.keyval() == gdk::Key::F5` atd.; modifikátory přes `event.state()`, `contains(gdk::ModifierType::SHIFT)`. Handler vrací `glib::Propagation::Stop` jen když klávesu zpracoval (jinak `Proceed`, aby Entry/text input normálně fungoval).

### Krok 4 — Copy/Move/Delete engine (`src/ops.rs`)

Čistý Rust bez GTK (až na `glib::Sender`) — testovatelný:

```rust
pub enum OpKind { Copy, Move }
pub struct Op { pub kind: OpKind, pub sources: Vec<PathBuf>, pub dest_dir: PathBuf }
pub enum OpEvent {
    Progress { done_files: u64, done_bytes: u64, total_files: u64, total_bytes: u64, current: String },
    Finished { errors: Vec<OpError> },
    Cancelled,
}
pub struct OpError { pub path: PathBuf, pub error: String }
pub enum ConflictAction { Overwrite, Skip, OverwriteAll, SkipAll, Abort }
```

`pub fn start(op: Op, tx: glib::Sender<OpEvent>, rx_reply: std::sync::mpsc::Receiver<ConflictAction>, cancel: Arc<AtomicBool>) -> std::thread::JoinHandle<()>`:

- `std::thread::spawn`; uvnitř nejdřív `count_totals` (průchod `fs::read_dir` rekurzivně spočítá total_files/total_bytes; chyby při počítání ignoruje — soubory co nelze přečíst, se projeví při vlastní operaci).
- Pro každý source: cílová cesta `dest_dir.join(file_name)`; **konflikt** = cíl existuje (a `is_dir` shodná kategorie zdroje? Ne — existuje-li cíl vůbec, je to konflikt): pošle přes `rx_reply` požadavek? Naopak: worker **čeká** — konflikt dialog drží odpověď: worker pošle `OpEvent::Conflict { path }`? Zjednodušení: worker **blokuje** na `rx_reply.recv()` TÉMĚŘ hned, jak narazí na konflikt; UI před spuštěním workeru vytvoří `mpsc::channel<ConflictAction>`, drží `Sender` v progress dialogu. Worker konflikt signalizuje přes `OpEvent::Progress { current: "CONFLICT: {path}" }`? Ne — přidej `OpEvent::Conflict { path: PathBuf }` varianta; UI handler na `Conflict` ukáže dialog a pošle odpověď přes `Sender<ConflictAction>`. Worker `recv()` → pokračuje. Abort → pošle `Finished` s dosavadními errors. `OverwriteAll`/`SkipAll` si worker cachuje a další konflikty nehlásí.
- Copy soubor: `fs::copy`; copy adresář: `fs::create_dir_all(cíl)` + rekurze; symlink: `fs::read_link` + `fs::symlink` na cíli (cíl symlinku se NEkontroluje); konflikt u symlinku: smaž cíl (`fs::remove_file`) a vytvoř znovu.
- Move: nejdřív `fs::rename(source, dest)`; při chybě `io::ErrorKind::CrossesDevices` (Raw OsError 18 — kontrola: `err.raw_os_error() == Some(libc::EXDEV)` bez závislosti na `libc` → prostě `Some(18)`) fallback copy (výše) + smazání zdroje. Jiná chyba = `OpError`.
- Delete do koše neřízen engine — volá UI přímo `trash::delete_all(&paths)` ve worker vlákně (krok 5), protože nepotřebuje progress.
- Cancel: `cancel.load(Relaxed)` kontrolovaný před každým souborem → `OpEvent::Cancelled` + return.
- Progres: `OpEvent::Progress` po každém souboru; `current` = cesta zpracovávaného souboru.
- Chyby per-soubor se sbírají do `errors` (operace pokračuje dál) — na konci `Finished { errors }`.

Jednotkové testy v `ops.rs` (pomocí `tempfile::TempDir`, volají interní `copy_recursive`/`move_one` s dummy kanály — vytvoř `let (tx, _) = glib::MainContext::channel(...)` a `let (reply_tx, reply_rx) = std::sync::mpsc::channel()`; `reply_tx` může zůstat nedotčený když v testu není konflikt, nebo test pošle `OverwriteAll`):
1. `copy_recursive` zkopíruje vnořenou strukturu `a/x.txt`, `a/b/y.txt` do `dest/a/...` s identickým obsahem.
2. Konflikt: existující cílový soubor + `Skip` odpověď → zdroj zůstane, cíl beze změny; `Overwrite` → cíl přepsán obsahem zdroje.
3. `Move` v rámci jednoho filesystému (tempdir) přemístí soubor (zdroj zmizí, cíl existuje).

### Krok 5 — Dialogy a napojení operací (`src/dialogs.rs` + wiring v `window.rs`)

- **Progress dialog**: `gtk::Window` (modal, transienční k hlavnímu oknu, 500×150): `gtk::ProgressBar`, `gtk::Label` "Kopírování {n}/{total} — {bytes}/{total_bytes}" a druhý label s `current`, tlačítko **Cancel** (nastaví `cancel` AtomicBool). `rx` z `glib::MainContext::channel` attachne handler, který aktualizuje UI; na `Finished`/`Cancelled` dialog zavře, **reloadne OBA panely**, ukáže error summary (viz níše) a dropne `reply_sender`. Na `Conflict` ukáže konflikt dialog.
- **Konflikt dialog**: modal `gtk::AlertDialog` NE (ten neumí 5 tlačítek) → vlastní `gtk::Window` s textem "Soubor existuje: {path}" a tlačítky `Overwrite` `Skip` `Overwrite all` `Skip all` `Abort`; klik pošle `ConflictAction` přes `reply_sender` (uložený v progress dialogu) a zavře konflikt dialog. Worker mezitím blokuje v `recv()`.
- **Error summary**: pokud `errors` neprázdné → `gtk::AlertDialog` s prvních max 10 chyb ("{path}: {error}") oddělených novými řádky + "… a dalších {n}" když víc.
- **F5/F6**: `let items = active.selected_or_cursor(); let dest = other.dir.clone();` — když `items` prázdné, nic (return). Otevře progress dialog, spustí `ops::start(Op { kind, sources: items, dest_dir: dest }, …)` s novými kanály.
- **F8**: `items` přes aktivní panel; worker vlákno `trash::delete_all(&items)`; poté reload obou panelů; chyby → error summary dialog (výjimka `trash::Error` → text). Bez progress dialogu.
- **Shift+F8**: `gtk::AlertDialog` "Trvale smazat {n} položek?"; na OK worker vlákno: `fs::remove_dir_all` pro adresáře / `fs::remove_file` pro soubory (podle `fs::symlink_metadata().is_dir()`); reload; error summary.
- **F4 (rename)**: přesně 1 položka z `selected_or_cursor` (jinak `AlertDialog` "Přejmenovat lze jen jednu položku"); dialog s `gtk::Entry` předvyplněný jménem, tlačítko OK → `fs::rename(old, dir.join(new))`; validace: prázdné jméno nebo obsahuje `/` → chyba; cíl existuje → chyba; reload aktivního panelu.
- **F7 (nová složka)**: dialog s `gtk::Entry` (prázdný), OK → `fs::create_dir(active.dir.join(name))`; existuje → chyba; reload aktivního panelu a kurzor na novou složku.

### Krok 6 — Viewer F3 (`src/viewer.rs`)

`pub fn open(parent: &gtk::Window, path: &Path)`:
- `gtk::Window` (800×600, title = jméno souboru, modal=false), `gtk::ScrolledWindow` s `gtk::TextView` `set_editable(false)`, monospace (`set_monospace(true)` pro hex režim).
- Obsah: `fs::read` celý soubor pokud < 10 MB, jinak prvních 10 MB (label v hlavičce "zobrazeno prvních 10 MB"). `String::from_utf8` úspěch → text přímo; neúspěch → hex dump prvních 64 KB (16 bytů na řádek, offset + hexy + ASCII sloupec, vlastní funkce `fn hex_dump(data: &[u8]) -> String` — žádná nová závislost).
- Chyba čtení → `AlertDialog`.

### Krok 7 — Balíčkování (.deb)

- `assets/salamander-rs.desktop`:

```ini
[Desktop Entry]
Type=Application
Name=Salamander RS
Comment=Dual-pane file manager
Exec=salamander-rs
Icon=salamander-rs
Terminal=false
Categories=System;FileTools;FileManager;
StartupWMClass=cz.k4.salamander-rs
```

- `assets/salamander-rs.svg`: jednoduchá vlastní ikona (dva svislé obdélníky panelů v motivu akcentní barvy, `viewBox="0 0 128 128"` — ručně napsané SVG, žádná externí ikona).
- Do `Cargo.toml`:

```toml
[package.metadata.deb]
maintainer = "Test K4 <dev@example.invalid>"   # uprav dle skutečného maintainera
depends = ["libgtk-4-1"]
section = "utils"
priority = "optional"
assets = [
    ["target/release/salamander-rs", "usr/bin/", "755"],
    ["assets/salamander-rs.desktop", "usr/share/applications/", "644"],
    ["assets/salamander-rs.svg", "usr/share/icons/hicolor/scalable/apps/salamander-rs.svg", "644"],
]
```

Poznámka: runtime závislost `libgtk-4-1` je GTK4 runtime balíček na Ubuntu 24.04; ikona se zaindexuje přes dpkg trigger `hicolor-icon-theme` automaticky (bez maintainer scripts).

## Critical files & anchors

| Soubor | Proč |
|---|---|
| `Cargo.toml` | gtk4 0.11 / trash 5 / chrono / tempfile (dev) + `[package.metadata.deb]`; sloupce ListStore v panel.rs závisí na pořadí typů |
| `src/ops.rs` | Jádro operací + jednotkové testy; konflikt protokol worker↔UI přes `ConflictAction` |
| `src/window.rs` | Celý keymap, aktivní panel, napojení F-keys na operace |
| `src/panel.rs` | ListStore sloupce 0–6, `selected_or_cursor`, cd/reload, kurzor-preserve |
| `src/dialogs.rs` | Progress+konflikt komunikace (glib channel + mpsc), error summary |

## Verification

Vše na Ubuntu 24.04+ (VM nebo WSL2 s WSLg stačí; LXQt flavor ověření = spuštění stejné binárky v LXQt session, GTK4 tam běží přes X11/Wayland normálně).

1. `cargo test` — 3 unit testy v `ops.rs` procházejí (kopie struktury, konflikt Skip/Overwrite, move).
2. `cargo run` — okno se dvěma panely; v levém panelu `~/Desktop` struktura:
   - `mkdir -p /tmp/t1/sub; echo a > /tmp/t1/f.txt; echo b > /tmp/t1/sub/g.txt; head -c 100 /dev/urandom > /tmp/t1/bin.dat`
   - F5 na `/tmp/t1` → objeví se v protilehlém panelu `t1` i s `sub/g.txt`; progress dialog proběhl.
   - Znovu F5 na stejné položce → konflikt dialog → Skip → beze změny; znovu → Overwrite → OK.
   - F3 na `f.txt` → text "a"; F3 na `bin.dat` → hex dump.
   - F4 na `f.txt` → přejmenovat na `f2.txt` → v listingu `f2.txt`.
   - F7 → vytvoří složku `nova`, kurzor na ní.
   - F8 na `nova` → zmizí z panelu, objeví se v `~/.local/share/Trash/files/nova` (ověřit `ls`).
   - Shift+F8 na kopii → potvrzení → trvale zmizí.
   - Ctrl+H → zobrazí/skryje `.` soubory; Ctrl+A vybere vše; Insert přidává řádky do výběru; Tab přepíná aktivní panel (změna focus rámečku).
   - Chyba oprávnění: `sudo mkdir /root/x; sudo chmod 000 /root/x` není potřeba — stačí F5 do `/proc`? Ne: F5 souboru do adresáře bez práva zápisu (`chmod 555` cílový) → operace skončí s error summary dialogem, aplikace nespadne.
3. `.deb`: `cargo install cargo-deb && cargo deb` → `target/debian/salamander-rs_0.1.0-1_amd64.deb`; `sudo dpkg -i target/debian/*.deb` → `gtk-launch salamander-rs` (nebo hledání "Salamander RS" v menu LXQt) spustí aplikaci; `dpkg -r salamander-rs` odinstaluje.

## Assumptions & contingencies

- **Build na Ubuntu**, ne cross-compile z Windows (GTK4 má nativní systémové závislosti). Kontingence: WSL2 Ubuntu 24.04 s WSLg plnohodnotně stačí pro `cargo run` i `cargo deb`; LXQt GUI ověření vyžaduje VM/instalaci s LXQt — pokud není k dispozici, ověř `gtk-launch` v dostupném desktopu a LXQt považuj za pokrytý podporou X11/Wayland (žádný LXQt-specifický kód v aplikaci není).
- Rust toolchain z rustup (apt verze může být pod MSRV 1.83).
- `cargo add gtk4 --package gtk4` by vygenerovalo `gtk = { package = "gtk4", version = "0.11" }` — pokud resolver vezme 0.11.x, fine; nikdy nemíchat gtk4-rs crates z git a crates.io.
- Pokud `cargo deb` narazí na konflikt názvu balíčku s existujícím Debian balíkem (neexistuje `salamander-rs` v Ubuntu repo — ověřeno nepřímo), pokračuj bez změny.
- GTK app-id a `.desktop` název musí být shodné (`cz.k4.salamander-rs`), jinak Wayland/X11 sesazení okna se zástupcem ne funguje; StartupWMClass je pojistka pro X11.
- Version of crates: plán cílí gtk4 0.11.x; pokud v době implementace vyjde 0.12 s breaking changes, zůstaň na `version = "0.11"` (plán je na ni psán).
