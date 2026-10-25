//! Password vault: server passwords encrypted with a key derived from a master password.
//!
//! File `~/.config/salamander-rs/vault` (mode 0600), one record per line:
//!
//! ```text
//! salamander-rs-vault 1
//! kdf argon2id13 <opslimit> <memlimit> <salt>
//! check <nonce> <ciphertext>
//! entry <id> <nonce> <ciphertext>
//! ```
//!
//! Every value is encrypted separately with XChaCha20-Poly1305 and authenticated together
//! with its id, so entries cannot be swapped between servers. The master password itself is
//! never stored; the `check` record only verifies that the derived key is right.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use crate::sodium::{self, KdfParams, Key, Secret};

const HEADER: &str = "salamander-rs-vault 1";
const CHECK_PLAINTEXT: &[u8] = b"salamander-rs-vault-ok";
const CHECK_AD: &[u8] = b"salamander-rs-vault-check";
pub const MIN_MASTER_LENGTH: usize = 8;

pub fn default_path() -> PathBuf {
    gtk::glib::user_config_dir().join("salamander-rs").join("vault")
}

fn entry_ad(id: &str) -> Vec<u8> {
    format!("salamander-rs-vault-entry:{id}").into_bytes()
}

pub struct Vault {
    path: PathBuf,
    params: KdfParams,
    salt: Vec<u8>,
    check: (Vec<u8>, Vec<u8>),
    entries: BTreeMap<String, (Vec<u8>, Vec<u8>)>,
}

impl Vault {
    pub fn load(path: &Path) -> Result<Option<Vault>, String> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(format!("Nelze číst trezor: {error}")),
        };
        let corrupt = |line: usize| format!("Trezor {} je poškozený (řádek {line})", path.to_string_lossy());
        let mut lines = text.lines().enumerate();
        if lines.next().map(|(_, line)| line) != Some(HEADER) {
            return Err(corrupt(1));
        }
        let mut params = None;
        let mut salt = None;
        let mut check = None;
        let mut entries = BTreeMap::new();
        for (number, line) in lines {
            let parts: Vec<&str> = line.split(' ').collect();
            match parts.as_slice() {
                ["kdf", "argon2id13", ops, mem, salt_hex] => {
                    params = Some(KdfParams {
                        opslimit: ops.parse().map_err(|_| corrupt(number + 1))?,
                        memlimit: mem.parse().map_err(|_| corrupt(number + 1))?,
                    });
                    salt = Some(sodium::unhex(salt_hex).ok_or_else(|| corrupt(number + 1))?);
                }
                ["check", nonce, ciphertext] => {
                    check = Some((
                        sodium::unhex(nonce).ok_or_else(|| corrupt(number + 1))?,
                        sodium::unhex(ciphertext).ok_or_else(|| corrupt(number + 1))?,
                    ));
                }
                ["entry", id, nonce, ciphertext] => {
                    entries.insert(
                        (*id).to_owned(),
                        (
                            sodium::unhex(nonce).ok_or_else(|| corrupt(number + 1))?,
                            sodium::unhex(ciphertext).ok_or_else(|| corrupt(number + 1))?,
                        ),
                    );
                }
                [""] => {}
                _ => return Err(corrupt(number + 1)),
            }
        }
        match (params, salt, check) {
            (Some(params), Some(salt), Some(check)) => Ok(Some(Vault {
                path: path.to_path_buf(),
                params,
                salt,
                check,
                entries,
            })),
            _ => Err(corrupt(0)),
        }
    }

    pub fn create(path: &Path, master: &Secret) -> Result<(Vault, Key), String> {
        Self::create_with(path, master, KdfParams::moderate())
    }

    pub fn create_with(path: &Path, master: &Secret, params: KdfParams) -> Result<(Vault, Key), String> {
        if path.exists() {
            return Err("Trezor již existuje".to_owned());
        }
        if master.as_bytes().len() < MIN_MASTER_LENGTH {
            return Err(format!("Hlavní heslo musí mít alespoň {MIN_MASTER_LENGTH} znaků"));
        }
        let salt = sodium::random_bytes(sodium::salt_bytes());
        let key = sodium::derive_key(master, &salt, params)?;
        let vault = Vault {
            path: path.to_path_buf(),
            params,
            salt,
            check: sodium::encrypt(&key, CHECK_PLAINTEXT, CHECK_AD),
            entries: BTreeMap::new(),
        };
        vault.save()?;
        Ok((vault, key))
    }

    /// Derives the key (deliberately slow) and verifies it against the check record.
    pub fn unlock(&self, master: &Secret) -> Result<Key, String> {
        let key = sodium::derive_key(master, &self.salt, self.params)?;
        match sodium::decrypt(&key, &self.check.0, &self.check.1, CHECK_AD) {
            Ok(plain) if sodium::equal(plain.as_bytes(), CHECK_PLAINTEXT) => Ok(key),
            _ => Err("Nesprávné hlavní heslo".to_owned()),
        }
    }

    pub fn contains(&self, id: &str) -> bool {
        self.entries.contains_key(id)
    }

    pub fn get(&self, key: &Key, id: &str) -> Result<Option<Secret>, String> {
        match self.entries.get(id) {
            None => Ok(None),
            Some((nonce, ciphertext)) => sodium::decrypt(key, nonce, ciphertext, &entry_ad(id)).map(Some),
        }
    }

    pub fn set(&mut self, key: &Key, id: &str, secret: &Secret) -> Result<(), String> {
        if id.is_empty() || id.contains(char::is_whitespace) {
            return Err("neplatný identifikátor".to_owned());
        }
        self.entries
            .insert(id.to_owned(), sodium::encrypt(key, secret.as_bytes(), &entry_ad(id)));
        self.save()
    }

    pub fn remove(&mut self, id: &str) -> Result<(), String> {
        if self.entries.remove(id).is_some() {
            self.save()?;
        }
        Ok(())
    }

    /// Re-encrypts everything under a new master password; returns the new key.
    pub fn change_master(&mut self, key: &Key, new_master: &Secret) -> Result<Key, String> {
        if new_master.as_bytes().len() < MIN_MASTER_LENGTH {
            return Err(format!("Hlavní heslo musí mít alespoň {MIN_MASTER_LENGTH} znaků"));
        }
        let mut plain = Vec::new();
        for id in self.entries.keys() {
            plain.push((id.clone(), self.get(key, id)?.expect("entry exists")));
        }
        let salt = sodium::random_bytes(sodium::salt_bytes());
        let new_key = sodium::derive_key(new_master, &salt, self.params)?;
        self.salt = salt;
        self.check = sodium::encrypt(&new_key, CHECK_PLAINTEXT, CHECK_AD);
        self.entries = plain
            .into_iter()
            .map(|(id, secret)| {
                let encrypted = sodium::encrypt(&new_key, secret.as_bytes(), &entry_ad(&id));
                (id, encrypted)
            })
            .collect();
        self.save()?;
        Ok(new_key)
    }

    fn save(&self) -> Result<(), String> {
        let mut text = format!(
            "{HEADER}\nkdf argon2id13 {} {} {}\ncheck {} {}\n",
            self.params.opslimit,
            self.params.memlimit,
            sodium::hex(&self.salt),
            sodium::hex(&self.check.0),
            sodium::hex(&self.check.1)
        );
        for (id, (nonce, ciphertext)) in &self.entries {
            text.push_str(&format!("entry {id} {} {}\n", sodium::hex(nonce), sodium::hex(ciphertext)));
        }
        let error = |error: io::Error| format!("Nelze uložit trezor: {error}");
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(error)?;
        }
        // Create the temporary file with 0600 from the start so the data is never readable
        // by others, then atomically replace the vault.
        let temporary = self.path.with_extension("tmp");
        let _ = fs::remove_file(&temporary);
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
            .map_err(error)?;
        file.write_all(text.as_bytes()).map_err(error)?;
        file.sync_all().map_err(error)?;
        drop(file);
        fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600)).map_err(error)?;
        fs::rename(&temporary, &self.path).map_err(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fast() -> KdfParams {
        KdfParams {
            opslimit: 1,
            memlimit: 8192 * 1024,
        }
    }

    #[test]
    fn create_unlock_store_and_reload() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("vault");
        let master = Secret::from_str("velmi tajné");
        let (mut vault, key) = Vault::create_with(&path, &master, fast()).unwrap();
        vault.set(&key, "srv1", &Secret::from_str("heslo-k-serveru")).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        let text = fs::read_to_string(&path).unwrap();
        assert!(!text.contains("heslo-k-serveru"));
        assert!(!text.contains("velmi"));

        let reloaded = Vault::load(&path).unwrap().unwrap();
        assert!(reloaded.contains("srv1"));
        assert!(reloaded.unlock(&Secret::from_str("špatné heslo")).is_err());
        let key = reloaded.unlock(&master).unwrap();
        assert_eq!(reloaded.get(&key, "srv1").unwrap().unwrap().as_str(), Some("heslo-k-serveru"));
        assert!(reloaded.get(&key, "nope").unwrap().is_none());
    }

    #[test]
    fn entries_cannot_be_swapped_between_servers() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("vault");
        let (mut vault, key) = Vault::create_with(&path, &Secret::from_str("12345678"), fast()).unwrap();
        vault.set(&key, "a", &Secret::from_str("A")).unwrap();
        vault.set(&key, "b", &Secret::from_str("B")).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        let swapped = text.replace("entry a ", "entry x ").replace("entry b ", "entry a ").replace("entry x ", "entry b ");
        fs::write(&path, swapped).unwrap();
        let tampered = Vault::load(&path).unwrap().unwrap();
        assert!(tampered.get(&key, "a").is_err());
    }

    #[test]
    fn change_master_and_validation() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("vault");
        assert!(Vault::create_with(&path, &Secret::from_str("short"), fast()).is_err());
        let (mut vault, key) = Vault::create_with(&path, &Secret::from_str("old-master"), fast()).unwrap();
        assert!(Vault::create_with(&path, &Secret::from_str("other-master"), fast()).is_err());
        vault.set(&key, "s", &Secret::from_str("pw")).unwrap();
        let new_key = vault.change_master(&key, &Secret::from_str("new-master")).unwrap();
        assert_eq!(vault.get(&new_key, "s").unwrap().unwrap().as_str(), Some("pw"));
        let reloaded = Vault::load(&path).unwrap().unwrap();
        assert!(reloaded.unlock(&Secret::from_str("old-master")).is_err());
        assert!(reloaded.unlock(&Secret::from_str("new-master")).is_ok());
        vault.remove("s").unwrap();
        assert!(!Vault::load(&path).unwrap().unwrap().contains("s"));
        fs::write(&path, "garbage").unwrap();
        assert!(Vault::load(&path).is_err());
    }
}
