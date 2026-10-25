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
- volitelná položka `..` na prvním řádku panelu pro přechod do nadřazené složky
  (Nástroje → Nastavení…, Ctrl+,)
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

**Vzdálené servery (SSH/SFTP)**
- databáze serverů (Ctrl+K): adresa, port, uživatel, způsob ověření (automaticky, heslo,
  klíč), soubor s klíčem, počáteční adresář a další volby ssh (např. `-o ProxyJump=…`);
  servery jsou i v nabídce Alt+F1/Alt+F2
- vzdálený adresář se otevře v panelu jako `sftp://uživatel@server/cesta`:
  F5/F6 stahuje a nahrává (rekurzivně, se zachováním časů a práv), F3 zobrazí,
  F4 upraví kopii a každé uložení nahraje zpět, F2, F7, Shift+F4, F8, Ctrl+F2 (chmod),
  Ctrl+T otevře terminál se SSH přímo v aktuálním adresáři
- spojení obstarává systémový OpenSSH (`ssh -s sftp`), takže fungují `~/.ssh/config`,
  ssh-agent, klíče i ProxyJump; protokol SFTP je stejný, jaký používá moderní `scp`;
  další přenosy sdílejí ověřené spojení (ControlMaster), heslo se nezadává znovu
- neznámý klíč serveru je nutné výslovně potvrdit (zobrazí se otisk), změněný klíč ssh odmítne

**Ukládání hesel** — volitelné, pro každý server zvlášť
- hesla jsou v trezoru `~/.config/salamander-rs/vault` (práva 0600), každé zvlášť zašifrované
  XChaCha20-Poly1305 a svázané s id serveru; klíč se odvozuje z hlavního hesla pomocí
  Argon2id (libsodium, profil „moderate“, 256 MiB), hlavní heslo se nikam neukládá
- odemčený klíč je jen v paměti (uzamčené stránky, při zamčení se přepíše nulami);
  trezor se zamkne po 15 minutách nečinnosti, při ukončení nebo ručně (Vzdálené → Zamknout)
- heslo se do `ssh` předává přes SSH_ASKPASS a soukromý Unix socket s jednorázovým tokenem —
  nikdy na příkazové řádce, v proměnných prostředí ani v souboru
- seznam serverů (`~/.config/salamander-rs/servers`) hesla neobsahuje

**Log vzdálených operací** — `~/.local/state/salamander-rs/remote.log` (0600, rotace po 5 MiB):
připojení a odpojení, žádosti o ověření, přijetí či odmítnutí klíče serveru s otiskem,
výpisy adresářů, každé stažení, nahrání, smazání, přejmenování, chmod a chyby; nikdy hesla.
Zobrazí se přes Vzdálené → Log vzdálených operací.

**Prohlížeč (F3)** — text/hex (H), zalamování (W), hledání (Ctrl+F, F3/Shift+F3), Esc zavře.

Úplný přehled kláves je v aplikaci pod F1.

## Sestavení

Na Ubuntu 24.04+ (nebo ve WSL2 s WSLg):

```bash
sudo apt install build-essential pkg-config libgtk-4-dev libarchive13t64 libsodium23 openssh-client
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

Balíček `.rpm` pro AlmaLinux / RHEL 9 a 10 (GNOME):

```bash
packaging/rpm/build-rpm.sh       # → target/rpm/salamander-rs-<verze>-1.x86_64.rpm
sudo dnf install ./salamander-rs-*.x86_64.rpm     # na AlmaLinuxu
```

Skript potřebuje `rpmbuild` a statickou `libsodium.a` (Ubuntu: `libsodium-dev`, jinak
`SODIUM_STATIC_DIR`). libsodium je přilinkovaná staticky, takže balíček vyžaduje jen
balíčky z BaseOS a AppStream (`gtk4` ≥ 4.12, `libarchive`, `openssh-clients`), EPEL není
potřeba. Binárka sestavená na Ubuntu 24.04 běží i s glibc 2.34 z AlmaLinuxu 9 (novější
symboly používá Rust jen jako volitelné).

Nastavení (okno Nástroje → Nastavení…, poslední adresáře, velikost okna, sloupce,
klávesové cesty) se ukládá
do `~/.config/salamander-rs/config`.

### Testy vzdálených funkcí

Testy SFTP potřebují `sftp-server` a testy spojení neprivilegovaný `sshd` na 127.0.0.1:2222
s klíčem `~/srs-sshtest/client_ed25519` (bez nich se přeskočí). Stačí rozbalit balíčky
`openssh-server` a `openssh-sftp-server` (`dpkg-deb -x … ~/srs-sshtest/root`) a spustit
`sshd -f` s vlastní konfigurací (`UsePAM no`, `HostKey` a `AuthorizedKeysFile` v témže adresáři).

## Licence

GPL-3.0-or-later
