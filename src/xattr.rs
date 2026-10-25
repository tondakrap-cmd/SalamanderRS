//! Extended attributes (xattr), filesystem flags (`chattr`/`lsattr`) and POSIX ACLs.
//!
//! ACLs are read and written directly as the `system.posix_acl_*` xattrs, so no libacl
//! is needed. Nothing here follows a final symlink.

use std::ffi::CString;
use std::fmt::Write as _;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use crate::sys;

fn c_path(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "cesta obsahuje NUL"))
}

fn c_name(name: &str) -> io::Result<CString> {
    CString::new(name)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "jméno obsahuje NUL"))
}

fn not_present(error: &io::Error) -> bool {
    error.raw_os_error() == Some(libc::ENODATA)
}

pub fn list(path: &Path) -> io::Result<Vec<String>> {
    let path = c_path(path)?;
    loop {
        let size = unsafe { libc::llistxattr(path.as_ptr(), std::ptr::null_mut(), 0) };
        if size < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut buffer = vec![0u8; size as usize];
        let read = unsafe { libc::llistxattr(path.as_ptr(), buffer.as_mut_ptr().cast(), buffer.len()) };
        if read < 0 {
            let error = io::Error::last_os_error();
            // The list grew between the two calls.
            if error.raw_os_error() == Some(libc::ERANGE) {
                continue;
            }
            return Err(error);
        }
        buffer.truncate(read as usize);
        let mut names: Vec<String> = buffer
            .split(|byte| *byte == 0)
            .filter(|name| !name.is_empty())
            .map(|name| String::from_utf8_lossy(name).into_owned())
            .collect();
        names.sort();
        return Ok(names);
    }
}

/// Value of one attribute; `Ok(None)` when it does not exist.
pub fn get(path: &Path, name: &str) -> io::Result<Option<Vec<u8>>> {
    let path = c_path(path)?;
    let name = c_name(name)?;
    loop {
        let size = unsafe { libc::lgetxattr(path.as_ptr(), name.as_ptr(), std::ptr::null_mut(), 0) };
        if size < 0 {
            let error = io::Error::last_os_error();
            return if not_present(&error) { Ok(None) } else { Err(error) };
        }
        let mut buffer = vec![0u8; size as usize];
        let read = unsafe {
            libc::lgetxattr(path.as_ptr(), name.as_ptr(), buffer.as_mut_ptr().cast(), buffer.len())
        };
        if read < 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ERANGE) {
                continue;
            }
            return if not_present(&error) { Ok(None) } else { Err(error) };
        }
        buffer.truncate(read as usize);
        return Ok(Some(buffer));
    }
}

pub fn set(path: &Path, name: &str, value: &[u8]) -> io::Result<()> {
    let path = c_path(path)?;
    let name = c_name(name)?;
    let rc = unsafe { libc::lsetxattr(path.as_ptr(), name.as_ptr(), value.as_ptr().cast(), value.len(), 0) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

pub fn remove(path: &Path, name: &str) -> io::Result<()> {
    let path = c_path(path)?;
    let name = c_name(name)?;
    if unsafe { libc::lremovexattr(path.as_ptr(), name.as_ptr()) } == 0 {
        Ok(())
    } else {
        let error = io::Error::last_os_error();
        if not_present(&error) {
            Ok(())
        } else {
            Err(error)
        }
    }
}

/// Copies user attributes and ACLs from `source` to `target`; best effort, returns the
/// first error but tries every attribute.
pub fn copy_all(source: &Path, target: &Path) -> io::Result<()> {
    let names = match list(source) {
        Ok(names) => names,
        Err(error) if error.raw_os_error() == Some(libc::ENOTSUP) => return Ok(()),
        Err(error) => return Err(error),
    };
    let mut first_error = None;
    for name in names {
        // security.* and trusted.* need privileges; system.* other than ACLs is not portable.
        let portable = name.starts_with("user.") || name == ACL_ACCESS || name == ACL_DEFAULT;
        if !portable {
            continue;
        }
        let result = get(source, &name).and_then(|value| match value {
            Some(value) => set(target, &name, &value),
            None => Ok(()),
        });
        if let Err(error) = result {
            first_error.get_or_insert(error);
        }
    }
    first_error.map_or(Ok(()), Err)
}

/// Shows a value as text when it is printable UTF-8, otherwise as `0x…` hex.
pub fn format_value(value: &[u8]) -> String {
    let trimmed = value.strip_suffix(&[0]).unwrap_or(value);
    match std::str::from_utf8(trimmed) {
        Ok(text) if text.chars().all(|ch| !ch.is_control() || ch == '\n' || ch == '\t') => text.to_owned(),
        _ => {
            let mut hex = String::from("0x");
            for byte in value {
                let _ = write!(hex, "{byte:02x}");
            }
            hex
        }
    }
}

/// Inverse of [`format_value`]: `0x…` is parsed as hex, anything else stored as UTF-8.
pub fn parse_value(text: &str) -> Result<Vec<u8>, String> {
    let Some(hex) = text.strip_prefix("0x") else {
        return Ok(text.as_bytes().to_vec());
    };
    if hex.len() % 2 != 0 {
        return Err("hexadecimální hodnota musí mít sudý počet číslic".to_owned());
    }
    (0..hex.len())
        .step_by(2)
        .map(|index| {
            u8::from_str_radix(&hex[index..index + 2], 16)
                .map_err(|_| format!("neplatná hexadecimální číslice v „{}“", &hex[index..index + 2]))
        })
        .collect()
}

// ---- Filesystem flags (chattr) ----

const FS_IOC_GETFLAGS: libc::c_ulong = 0x8008_6601;
const FS_IOC_SETFLAGS: libc::c_ulong = 0x4008_6602;

pub struct FlagInfo {
    pub bit: u32,
    pub letter: char,
    pub label: &'static str,
    pub tooltip: &'static str,
    /// Only the kernel sets it (e.g. extents); shown but not editable.
    pub read_only: bool,
}

pub const FLAGS: &[FlagInfo] = &[
    FlagInfo { bit: 0x10, letter: 'i', label: "Neměnný (i)", tooltip: "Soubor nelze měnit, mazat, přejmenovat ani na něj vytvořit odkaz; nastaví jen root", read_only: false },
    FlagInfo { bit: 0x20, letter: 'a', label: "Jen připisovat (a)", tooltip: "Soubor lze otevřít jen pro připisování na konec; nastaví jen root", read_only: false },
    FlagInfo { bit: 0x40, letter: 'd', label: "Nezálohovat (d)", tooltip: "Program dump soubor vynechá", read_only: false },
    FlagInfo { bit: 0x80, letter: 'A', label: "Neměnit čas přístupu (A)", tooltip: "Při čtení se neaktualizuje atime", read_only: false },
    FlagInfo { bit: 0x8, letter: 'S', label: "Synchronní zápis (S)", tooltip: "Změny se zapisují na disk okamžitě", read_only: false },
    FlagInfo { bit: 0x10000, letter: 'D', label: "Synchronní adresář (D)", tooltip: "Změny adresáře se zapisují na disk okamžitě", read_only: false },
    FlagInfo { bit: 0x4, letter: 'c', label: "Komprimovat (c)", tooltip: "Transparentní komprese (btrfs)", read_only: false },
    FlagInfo { bit: 0x800000, letter: 'C', label: "Bez copy-on-write (C)", tooltip: "Vypne copy-on-write (btrfs); vhodné pro obrazy disků a databáze", read_only: false },
    FlagInfo { bit: 0x4000, letter: 'j', label: "Žurnálovat data (j)", tooltip: "ext3/ext4: data procházejí žurnálem", read_only: false },
    FlagInfo { bit: 0x20000, letter: 'T', label: "Vrchol hierarchie (T)", tooltip: "Alokátor ext4 bere adresář jako kořen samostatné hierarchie", read_only: false },
    FlagInfo { bit: 0x8000, letter: 't', label: "Bez slučování konců (t)", tooltip: "Konec souboru se neslučuje s jinými soubory", read_only: false },
    FlagInfo { bit: 0x20000000, letter: 'P', label: "Dědit projekt (P)", tooltip: "Nové položky dědí ID projektu (kvóty)", read_only: false },
    FlagInfo { bit: 0x80000, letter: 'e', label: "Extenty (e)", tooltip: "Soubor používá extenty (nastavuje souborový systém)", read_only: true },
    FlagInfo { bit: 0x40000000, letter: 'F', label: "Bez rozlišení velikosti písmen (F)", tooltip: "Adresář nerozlišuje velikost písmen (nastavuje se jen u prázdného adresáře)", read_only: false },
    FlagInfo { bit: 0x100000, letter: 'E', label: "Šifrováno (E)", tooltip: "Šifrováno pomocí fscrypt (nastavuje souborový systém)", read_only: true },
    FlagInfo { bit: 0x10000000, letter: 'I', label: "Indexovaný adresář (I)", tooltip: "Adresář používá hash index (nastavuje souborový systém)", read_only: true },
    FlagInfo { bit: 0x1000000, letter: 'V', label: "Ověřování (V)", tooltip: "fs-verity (nastavuje souborový systém)", read_only: true },
];

fn open_for_flags(path: &Path) -> io::Result<std::os::fd::OwnedFd> {
    use std::os::fd::FromRawFd;
    let path = c_path(path)?;
    let fd = unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NOCTTY,
        )
    };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) })
    }
}

/// `lsattr` flags. Fails with ENOTTY/EOPNOTSUPP on filesystems without them and ELOOP on
/// symlinks.
pub fn get_flags(path: &Path) -> io::Result<u32> {
    use std::os::fd::AsRawFd;
    let file = open_for_flags(path)?;
    let mut flags: libc::c_int = 0;
    if unsafe { libc::ioctl(file.as_raw_fd(), FS_IOC_GETFLAGS as _, &mut flags) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(flags as u32)
}

pub fn set_flags(path: &Path, flags: u32) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    let file = open_for_flags(path)?;
    let flags = flags as libc::c_int;
    if unsafe { libc::ioctl(file.as_raw_fd(), FS_IOC_SETFLAGS as _, &flags) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// `lsattr`-style letters for the set flags.
pub fn format_flags(flags: u32) -> String {
    FLAGS
        .iter()
        .filter(|flag| flags & flag.bit != 0)
        .map(|flag| flag.letter)
        .collect()
}

// ---- POSIX ACL ----

pub const ACL_ACCESS: &str = "system.posix_acl_access";
pub const ACL_DEFAULT: &str = "system.posix_acl_default";
const ACL_VERSION: u32 = 2;
const ACL_UNDEFINED_ID: u32 = u32::MAX;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum AclTag {
    UserObj = 0x01,
    User = 0x02,
    GroupObj = 0x04,
    Group = 0x08,
    Mask = 0x10,
    Other = 0x20,
}

impl AclTag {
    fn from_raw(raw: u16) -> Option<AclTag> {
        Some(match raw {
            0x01 => AclTag::UserObj,
            0x02 => AclTag::User,
            0x04 => AclTag::GroupObj,
            0x08 => AclTag::Group,
            0x10 => AclTag::Mask,
            0x20 => AclTag::Other,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AclEntry {
    pub tag: AclTag,
    /// uid/gid for named entries.
    pub id: Option<u32>,
    /// rwx as 4/2/1.
    pub perm: u16,
}

pub fn decode_acl(data: &[u8]) -> Result<Vec<AclEntry>, String> {
    if data.len() < 4 || (data.len() - 4) % 8 != 0 {
        return Err("poškozené ACL".to_owned());
    }
    let version = u32::from_le_bytes(data[..4].try_into().unwrap());
    if version != ACL_VERSION {
        return Err(format!("nepodporovaná verze ACL {version}"));
    }
    data[4..]
        .chunks_exact(8)
        .map(|chunk| {
            let tag = u16::from_le_bytes([chunk[0], chunk[1]]);
            let perm = u16::from_le_bytes([chunk[2], chunk[3]]);
            let id = u32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]);
            let tag = AclTag::from_raw(tag).ok_or_else(|| format!("neznámý typ položky ACL {tag:#x}"))?;
            Ok(AclEntry {
                tag,
                id: matches!(tag, AclTag::User | AclTag::Group).then_some(id),
                perm: perm & 7,
            })
        })
        .collect()
}

pub fn encode_acl(entries: &[AclEntry]) -> Vec<u8> {
    let mut data = ACL_VERSION.to_le_bytes().to_vec();
    for entry in entries {
        data.extend_from_slice(&(entry.tag as u16).to_le_bytes());
        data.extend_from_slice(&entry.perm.to_le_bytes());
        data.extend_from_slice(&entry.id.unwrap_or(ACL_UNDEFINED_ID).to_le_bytes());
    }
    data
}

fn perm_text(perm: u16) -> String {
    [(4, 'r'), (2, 'w'), (1, 'x')]
        .iter()
        .map(|(bit, ch)| if perm & bit != 0 { *ch } else { '-' })
        .collect()
}

/// `getfacl`-like lines, e.g. `user:alice:rw-`; `prefix` is "" or "default:".
pub fn format_acl(entries: &[AclEntry], prefix: &str) -> String {
    let mut text = String::new();
    for entry in entries {
        let (tag, qualifier) = match entry.tag {
            AclTag::UserObj => ("user", String::new()),
            AclTag::User => ("user", entry.id.map(|id| sys::user_name(id).unwrap_or_else(|| id.to_string())).unwrap_or_default()),
            AclTag::GroupObj => ("group", String::new()),
            AclTag::Group => ("group", entry.id.map(|id| sys::group_name(id).unwrap_or_else(|| id.to_string())).unwrap_or_default()),
            AclTag::Mask => ("mask", String::new()),
            AclTag::Other => ("other", String::new()),
        };
        let _ = writeln!(text, "{prefix}{tag}:{qualifier}:{}", perm_text(entry.perm));
    }
    text
}

/// Whether the entries add anything beyond the plain permission bits.
pub fn is_extended(entries: &[AclEntry]) -> bool {
    entries
        .iter()
        .any(|entry| matches!(entry.tag, AclTag::User | AclTag::Group | AclTag::Mask))
}

fn parse_perm(text: &str) -> Result<u16, String> {
    let mut perm = 0;
    for ch in text.chars() {
        perm |= match ch {
            'r' => 4,
            'w' => 2,
            'x' | 'X' => 1,
            '-' => 0,
            _ => return Err(format!("neplatné oprávnění „{text}“ (použijte r, w, x a -)")),
        };
    }
    Ok(perm)
}

/// Parses `getfacl`/`setfacl` text into (access, default) entry lists, sorted as the kernel
/// requires and with the mask computed when named entries lack one.
pub fn parse_acl_text(text: &str) -> Result<(Vec<AclEntry>, Vec<AclEntry>), String> {
    let mut access = Vec::new();
    let mut default = Vec::new();
    for (number, line) in text.lines().enumerate() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let error = |message: String| format!("řádek {}: {message}", number + 1);
        let (target, rest) = match line.strip_prefix("default:").or_else(|| line.strip_prefix("d:")) {
            Some(rest) => (&mut default, rest),
            None => (&mut access, line),
        };
        let parts: Vec<&str> = rest.split(':').collect();
        let (tag, qualifier, perm) = match parts.as_slice() {
            [tag, qualifier, perm] => (*tag, *qualifier, *perm),
            // "other::r--" and "mask::rwx" are canonical, but "other:r--" is accepted too.
            [tag, perm] if matches!(*tag, "other" | "o" | "mask" | "m") => (*tag, "", *perm),
            _ => return Err(error(format!("očekáván formát typ:jméno:práva, nalezeno „{rest}“"))),
        };
        let perm = parse_perm(perm.trim()).map_err(error)?;
        let qualifier = qualifier.trim();
        let entry = match (tag.trim(), qualifier.is_empty()) {
            ("user" | "u", true) => AclEntry { tag: AclTag::UserObj, id: None, perm },
            ("user" | "u", false) => AclEntry {
                tag: AclTag::User,
                id: Some(sys::user_id(qualifier).ok_or_else(|| error(format!("neznámý uživatel „{qualifier}“")))?),
                perm,
            },
            ("group" | "g", true) => AclEntry { tag: AclTag::GroupObj, id: None, perm },
            ("group" | "g", false) => AclEntry {
                tag: AclTag::Group,
                id: Some(sys::group_id(qualifier).ok_or_else(|| error(format!("neznámá skupina „{qualifier}“")))?),
                perm,
            },
            ("mask" | "m", _) => AclEntry { tag: AclTag::Mask, id: None, perm },
            ("other" | "o", _) => AclEntry { tag: AclTag::Other, id: None, perm },
            (other, _) => return Err(error(format!("neznámý typ „{other}“ (user, group, mask, other)"))),
        };
        if target.iter().any(|existing: &AclEntry| existing.tag == entry.tag && existing.id == entry.id) {
            return Err(error("položka je uvedena dvakrát".to_owned()));
        }
        target.push(entry);
    }
    let access = finish_acl(access, "přístupové ACL")?;
    let default = if default.is_empty() { default } else { finish_acl(default, "výchozí ACL")? };
    Ok((access, default))
}

fn finish_acl(mut entries: Vec<AclEntry>, what: &str) -> Result<Vec<AclEntry>, String> {
    if entries.is_empty() {
        return Ok(entries);
    }
    for (tag, name) in [(AclTag::UserObj, "user::"), (AclTag::GroupObj, "group::"), (AclTag::Other, "other::")] {
        if !entries.iter().any(|entry| entry.tag == tag) {
            return Err(format!("{what}: chybí položka {name}"));
        }
    }
    let named = entries.iter().any(|entry| matches!(entry.tag, AclTag::User | AclTag::Group));
    if named && !entries.iter().any(|entry| entry.tag == AclTag::Mask) {
        // Like setfacl: the mask is the union of all group-class permissions.
        let perm = entries
            .iter()
            .filter(|entry| matches!(entry.tag, AclTag::User | AclTag::Group | AclTag::GroupObj))
            .fold(0, |acc, entry| acc | entry.perm);
        entries.push(AclEntry { tag: AclTag::Mask, id: None, perm });
    }
    entries.sort_by_key(|entry| (entry.tag, entry.id));
    Ok(entries)
}

/// Reads the access and default ACL as `getfacl`-like text (empty when there is none).
pub fn read_acl_text(path: &Path) -> io::Result<String> {
    let mut text = String::new();
    for (name, prefix) in [(ACL_ACCESS, ""), (ACL_DEFAULT, "default:")] {
        match get(path, name) {
            Ok(Some(data)) => {
                let entries = decode_acl(&data).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                text.push_str(&format_acl(&entries, prefix));
            }
            Ok(None) => {}
            Err(error) if error.raw_os_error() == Some(libc::EOPNOTSUPP) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(text)
}

/// Writes ACLs from text; empty access/default sections remove the respective ACL.
pub fn write_acl_text(path: &Path, text: &str, is_dir: bool) -> Result<(), String> {
    let (access, default) = parse_acl_text(text)?;
    if !default.is_empty() && !is_dir {
        return Err("výchozí ACL (default:) lze nastavit jen adresáři".to_owned());
    }
    let apply = |name: &str, entries: &[AclEntry]| {
        if entries.is_empty() {
            remove(path, name)
        } else {
            set(path, name, &encode_acl(entries))
        }
    };
    apply(ACL_ACCESS, &access).map_err(|error| format!("přístupové ACL: {error}"))?;
    if is_dir {
        apply(ACL_DEFAULT, &default).map_err(|error| format!("výchozí ACL: {error}"))?;
    }
    Ok(())
}

/// Cheap check for the `+` marker in the permissions column.
pub fn has_extended_acl(path: &Path) -> bool {
    matches!(get(path, ACL_ACCESS), Ok(Some(data)) if decode_acl(&data).is_ok_and(|entries| is_extended(&entries)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn home_temp() -> tempfile::TempDir {
        // /tmp may be tmpfs without user xattrs in some setups; $HOME is the user's real fs.
        tempfile::tempdir_in(std::env::var_os("HOME").unwrap()).unwrap()
    }

    #[test]
    fn user_xattrs_round_trip_and_copy() {
        let temp = home_temp();
        let file = temp.path().join("file");
        let copy = temp.path().join("copy");
        fs::write(&file, "").unwrap();
        fs::write(&copy, "").unwrap();
        if set(&file, "user.comment", b"ahoj").is_err() {
            eprintln!("filesystem without user xattrs, skipping");
            return;
        }
        set(&file, "user.bin", &[0, 1, 255]).unwrap();
        assert_eq!(list(&file).unwrap(), ["user.bin", "user.comment"]);
        assert_eq!(get(&file, "user.comment").unwrap().as_deref(), Some(&b"ahoj"[..]));
        assert_eq!(get(&file, "user.missing").unwrap(), None);
        copy_all(&file, &copy).unwrap();
        assert_eq!(get(&copy, "user.bin").unwrap().as_deref(), Some(&[0, 1, 255][..]));
        remove(&file, "user.comment").unwrap();
        remove(&file, "user.comment").unwrap();
        assert_eq!(list(&file).unwrap(), ["user.bin"]);
    }

    #[test]
    fn values_format_and_parse() {
        assert_eq!(format_value(b"text"), "text");
        assert_eq!(format_value(b"text\0"), "text");
        assert_eq!(format_value(&[0, 1, 0xab]), "0x0001ab");
        assert_eq!(parse_value("0x0001ab").unwrap(), [0, 1, 0xab]);
        assert_eq!(parse_value("plain").unwrap(), b"plain");
        assert!(parse_value("0x123").is_err());
        assert!(parse_value("0xzz").is_err());
    }

    #[test]
    fn acl_text_parses_sorts_and_adds_mask() {
        let (access, default) = parse_acl_text(
            "# comment\nother::r--\ngroup::r-x\nuser:root:rw-\nuser::rwx\ndefault:user::rwx\nd:group::---\ndefault:other::---\n",
        )
        .unwrap();
        assert_eq!(
            access,
            vec![
                AclEntry { tag: AclTag::UserObj, id: None, perm: 7 },
                AclEntry { tag: AclTag::User, id: Some(0), perm: 6 },
                AclEntry { tag: AclTag::GroupObj, id: None, perm: 5 },
                AclEntry { tag: AclTag::Mask, id: None, perm: 7 },
                AclEntry { tag: AclTag::Other, id: None, perm: 4 },
            ]
        );
        assert_eq!(default.len(), 3);
        assert_eq!(decode_acl(&encode_acl(&access)).unwrap(), access);
        assert_eq!(format_acl(&access, ""), "user::rwx\nuser:root:rw-\ngroup::r-x\nmask::rwx\nother::r--\n");
        assert!(is_extended(&access));
        assert!(parse_acl_text("user::rwx\nother::r--").is_err());
        assert!(parse_acl_text("user::rwz\ngroup::r--\nother::r--").is_err());
        assert!(parse_acl_text("user:nosuchuser-xyz:r--").is_err());
        assert_eq!(parse_acl_text("").unwrap(), (vec![], vec![]));
    }

    #[test]
    fn acl_write_and_read_on_real_filesystem() {
        let temp = home_temp();
        let dir = temp.path().join("dir");
        fs::create_dir(&dir).unwrap();
        let text = "user::rwx\nuser:root:r-x\ngroup::r-x\nmask::r-x\nother::---\ndefault:user::rwx\ndefault:group::r-x\ndefault:other::---\n";
        if let Err(error) = write_acl_text(&dir, text, true) {
            eprintln!("filesystem without ACL support ({error}), skipping");
            return;
        }
        assert_eq!(read_acl_text(&dir).unwrap(), text);
        assert!(has_extended_acl(&dir));
        // The access ACL is reflected in the mode bits (group bits show the mask).
        use std::os::unix::fs::MetadataExt;
        assert_eq!(fs::metadata(&dir).unwrap().mode() & 0o777, 0o750);
        write_acl_text(&dir, "", true).unwrap();
        assert!(!has_extended_acl(&dir));
        assert_eq!(read_acl_text(&dir).unwrap(), "");
        assert!(write_acl_text(&temp.path().join("dir"), "default:user::rwx\ndefault:group::---\ndefault:other::---\nuser::rwx\ngroup::---\nother::---", false).is_err());
    }

    #[test]
    fn flags_read_on_real_filesystem() {
        let temp = home_temp();
        let file = temp.path().join("file");
        fs::write(&file, "").unwrap();
        match get_flags(&file) {
            Ok(flags) => {
                // Toggling "no dump" is allowed for the owner on ext4/btrfs/xfs.
                set_flags(&file, flags | 0x40).unwrap();
                assert!(format_flags(get_flags(&file).unwrap()).contains('d'));
                set_flags(&file, flags).unwrap();
            }
            Err(error) => eprintln!("filesystem without flags ({error}), skipping"),
        }
        let link = temp.path().join("link");
        std::os::unix::fs::symlink(&file, &link).unwrap();
        assert!(get_flags(&link).is_err());
    }
}
