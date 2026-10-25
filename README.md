# Salamander RS

Dvoupanelový správce souborů pro Linux (GTK 4, Rust) inspirovaný
[Open Salamanderem](https://github.com/OpenSalamander/salamander). Ovládá se hlavně
z klávesnice. Jde o nezávislou implementaci, kód Open Salamandera nepřebírá.

Cílí na Ubuntu 24.04 a novější, včetně LXQt (X11 i Wayland).

## Funkce

**Panely**
- sloupce Název (s ikonou podle MIME typu), Přípona, Velikost, Změněno, Práva
  (`rwxr-sr-t`), Vlastník:Skupina; viditelnost sloupců se nastavuje v *Zobrazit → Sloupce*
- adresáře vždy nahoře, řazení klikem na záhlaví nebo Ctrl+F3–F6
- informační řádek s položkou pod kurzorem (cíl symlinku, práva, vlastník)
- stavový řádek se součty výběru a volným místem na svazku
- automatická obnova při změnách na disku (inotify přes GIO)
- rychlé hledání: stačí začít psát jméno (podporuje `*` a `?`)
- historie (Alt+←/→); Backspace vrátí kurzor na adresář, ze kterého jste přišli
- klávesové cesty Ctrl+Shift+0–9 (uložit) a Ctrl+0–9 (skok); fungují i s českým rozložením
- Alt+F1 / Alt+F2: místa, připojené i nepřipojené svazky (připojí se po Enter),
  záložky GTK, klávesové cesty a historie

**Soubory**
- kopírování a přesun s řešením konfliktů a průběhem (zachovává práva i časy)
- koš (FreeDesktop) i trvalé mazání, přejmenování, nová složka (i `a/b/c`), nový soubor
- F4 otevře soubor ve výchozím textovém editoru prostředí
- symbolické odkazy (Ctrl+Shift+F5)
- schránka kompatibilní s Nautilem, PCManFM-Qt a Dolphinem (Ctrl+C/X/V)
- výpočet obsazeného místa (Alt+Shift+Enter)
- hledání souborů podle masky a obsahu (Alt+F7)
- porovnání adresářů (Shift+F2) označí chybějící, novější a jinak velké soubory
- výběr skupiny maskou (`*.rs;*.toml|target*`), invertování výběru

**Vlastnosti a oprávnění (Alt+Enter, Ctrl+F2)**
- typ souboru (MIME), velikost včetně obsahu adresáře, i-uzel, počet odkazů,
  časy vytvoření, změny, přístupu a změny i-uzlu
- práva pro vlastníka, skupinu a ostatní, SUID/SGID/sticky, zadání v osmičkové soustavě
- u více položek najednou třístavová zaškrtávátka: mění se jen bity, na které kliknete
- změna vlastníka a skupiny (chown/chgrp)
- nastavení času změny a přístupu
- rekurzivní použití na soubory i adresáře, jen soubory nebo jen adresáře,
  včetně sémantiky `chmod X`
- symbolické odkazy se nesledují: mění se vlastník a časy odkazu, ne cíle

**Archivy** (přes systémovou knihovnu libarchive)
- Enter na archivu ho otevře jako adresář, Backspace z kořene archivu vyjde ven;
  čtení zvládá zip, tar.gz/xz/zst/bz2, 7z, rar, iso, cpio, deb, rpm, cab a další
- F3/Enter na souboru v archivu ho rozbalí do soukromého dočasného adresáře a zobrazí/otevře
- F5 z archivu rozbalí vybrané položky, F5 do otevřeného archivu soubory přidá
  (stejnojmenné nahradí), F8 v archivu položky odstraní — zápis do zip, tar.*, 7z a cpio
- Alt+F5 zabalí výběr (zip, tar.gz, tar.xz, tar.zst, tar.bz2, tar, 7z),
  Alt+F9 rozbalí vybrané archivy, volitelně každý do vlastní složky
- ochrana proti „zip slip“: položky s `..` nebo cestou přes symbolický odkaz se nerozbalí,
  bity setuid/setgid se z archivu neobnovují

**Rozšířené atributy** (záložka *Rozšířené* ve Vlastnostech)
- příznaky souborového systému jako `chattr`/`lsattr` (neměnný, jen připisovat,
  nezálohovat, bez copy-on-write, …)
- POSIX ACL ve formátu `getfacl`/`setfacl` včetně výchozích ACL adresářů; mask se dopočítá;
  soubory s ACL mají ve sloupci Práva značku `+` jako v `ls -l`
- seznam, přidání, úprava a mazání xattr (text i binární hodnoty `0x…`)
- kopírování zachovává uživatelské xattr i ACL

**Prohlížeč (F3)** — text/hex (H), zalamování (W), hledání (Ctrl+F, F3/Shift+F3), Esc zavře.

Úplný přehled kláves je v aplikaci pod F1.

## Sestavení

Na Ubuntu 24.04+ (nebo ve WSL2 s WSLg):

```bash
sudo apt install build-essential pkg-config libgtk-4-dev libarchive13t64
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh   # Rust ≥ 1.85
cargo test
cargo run -- ~/Dokumenty        # volitelně adresář pro levý panel
```

Balíček `.deb`:

```bash
cargo install cargo-deb
cargo deb
sudo apt install ./target/debian/salamander-rs_*_amd64.deb
```

Nastavení (poslední adresáře, velikost okna, sloupce, klávesové cesty) se ukládá
do `~/.config/salamander-rs/config`.

## Licence

GPL-3.0-or-later
