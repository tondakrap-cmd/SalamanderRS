//! Minimal safe wrapper around libsodium: Argon2id key derivation, XChaCha20-Poly1305
//! authenticated encryption, secure random bytes and memory wiping.

use std::os::raw::{c_char, c_int, c_uchar, c_ulonglong, c_void};
use std::sync::Once;

extern "C" {
    fn sodium_init() -> c_int;
    fn randombytes_buf(buffer: *mut c_void, size: usize);
    fn sodium_memzero(pointer: *mut c_void, length: usize);
    fn sodium_mlock(pointer: *mut c_void, length: usize) -> c_int;
    fn sodium_munlock(pointer: *mut c_void, length: usize) -> c_int;
    fn sodium_memcmp(a: *const c_void, b: *const c_void, length: usize) -> c_int;

    fn crypto_pwhash(
        out: *mut c_uchar,
        out_length: c_ulonglong,
        password: *const c_char,
        password_length: c_ulonglong,
        salt: *const c_uchar,
        opslimit: c_ulonglong,
        memlimit: usize,
        algorithm: c_int,
    ) -> c_int;
    fn crypto_pwhash_saltbytes() -> usize;
    fn crypto_pwhash_alg_argon2id13() -> c_int;
    fn crypto_pwhash_opslimit_moderate() -> usize;
    fn crypto_pwhash_memlimit_moderate() -> usize;

    fn crypto_aead_xchacha20poly1305_ietf_encrypt(
        ciphertext: *mut c_uchar,
        ciphertext_length: *mut c_ulonglong,
        message: *const c_uchar,
        message_length: c_ulonglong,
        additional: *const c_uchar,
        additional_length: c_ulonglong,
        nsec: *const c_uchar,
        nonce: *const c_uchar,
        key: *const c_uchar,
    ) -> c_int;
    fn crypto_aead_xchacha20poly1305_ietf_decrypt(
        message: *mut c_uchar,
        message_length: *mut c_ulonglong,
        nsec: *mut c_uchar,
        ciphertext: *const c_uchar,
        ciphertext_length: c_ulonglong,
        additional: *const c_uchar,
        additional_length: c_ulonglong,
        nonce: *const c_uchar,
        key: *const c_uchar,
    ) -> c_int;
    fn crypto_aead_xchacha20poly1305_ietf_keybytes() -> usize;
    fn crypto_aead_xchacha20poly1305_ietf_npubbytes() -> usize;
    fn crypto_aead_xchacha20poly1305_ietf_abytes() -> usize;
}

pub const KEY_BYTES: usize = 32;

fn init() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        // Returns 1 when already initialized; only -1 is a failure.
        assert!(unsafe { sodium_init() } >= 0, "libsodium se nepodařilo inicializovat");
        assert_eq!(unsafe { crypto_aead_xchacha20poly1305_ietf_keybytes() }, KEY_BYTES);
    });
}

pub fn random_bytes(length: usize) -> Vec<u8> {
    init();
    let mut buffer = vec![0u8; length];
    unsafe { randombytes_buf(buffer.as_mut_ptr().cast(), buffer.len()) };
    buffer
}

pub fn random_hex(bytes: usize) -> String {
    hex(&random_bytes(bytes))
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub fn unhex(text: &str) -> Option<Vec<u8>> {
    if text.len() % 2 != 0 {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(text.get(index..index + 2)?, 16).ok())
        .collect()
}

/// Constant-time comparison.
pub fn equal(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && unsafe { sodium_memcmp(a.as_ptr().cast(), b.as_ptr().cast(), a.len()) } == 0
}

/// Bytes that are wiped from memory when dropped and never printed.
pub struct Secret(Vec<u8>);

impl Secret {
    /// Takes ownership of `bytes` without copying them.
    pub fn new(bytes: Vec<u8>) -> Secret {
        Secret(bytes)
    }

    pub fn from_str(text: &str) -> Secret {
        let mut bytes = Vec::with_capacity(text.len());
        bytes.extend_from_slice(text.as_bytes());
        Secret(bytes)
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    #[cfg(test)]
    pub fn as_str(&self) -> Option<&str> {
        std::str::from_utf8(&self.0).ok()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn clone_secret(&self) -> Secret {
        Secret::new(self.0.clone())
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        init();
        // Wipe the whole allocation, including capacity beyond the length.
        let capacity = self.0.capacity();
        unsafe { sodium_memzero(self.0.as_mut_ptr().cast(), capacity) };
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Secret(***)")
    }
}

/// A 256-bit key kept in locked (non-swappable) memory and wiped on drop.
pub struct Key(Box<[u8; KEY_BYTES]>);

impl Key {
    fn zeroed() -> Key {
        init();
        let mut key = Key(Box::new([0u8; KEY_BYTES]));
        // Best effort: mlock may fail under a low RLIMIT_MEMLOCK; the key is still wiped.
        unsafe { sodium_mlock(key.0.as_mut_ptr().cast(), KEY_BYTES) };
        key
    }

    #[cfg(test)]
    pub fn random() -> Key {
        let mut key = Key::zeroed();
        unsafe { randombytes_buf(key.0.as_mut_ptr().cast(), KEY_BYTES) };
        key
    }
}

impl Drop for Key {
    fn drop(&mut self) {
        // munlock also zeroes the memory.
        unsafe { sodium_munlock(self.0.as_mut_ptr().cast(), KEY_BYTES) };
    }
}

impl std::fmt::Debug for Key {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Key(***)")
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KdfParams {
    pub opslimit: u64,
    pub memlimit: usize,
}

impl KdfParams {
    /// libsodium's "moderate" Argon2id profile (about 256 MiB of memory).
    pub fn moderate() -> KdfParams {
        init();
        KdfParams {
            opslimit: unsafe { crypto_pwhash_opslimit_moderate() } as u64,
            memlimit: unsafe { crypto_pwhash_memlimit_moderate() },
        }
    }
}

pub fn salt_bytes() -> usize {
    init();
    unsafe { crypto_pwhash_saltbytes() }
}

/// Derives a key from a password with Argon2id.
pub fn derive_key(password: &Secret, salt: &[u8], params: KdfParams) -> Result<Key, String> {
    init();
    if salt.len() != salt_bytes() {
        return Err("neplatná délka soli".to_owned());
    }
    let mut key = Key::zeroed();
    let rc = unsafe {
        crypto_pwhash(
            key.0.as_mut_ptr(),
            KEY_BYTES as c_ulonglong,
            password.as_bytes().as_ptr().cast(),
            password.as_bytes().len() as c_ulonglong,
            salt.as_ptr(),
            params.opslimit,
            params.memlimit,
            crypto_pwhash_alg_argon2id13(),
        )
    };
    if rc == 0 {
        Ok(key)
    } else {
        Err("odvození klíče selhalo (nedostatek paměti?)".to_owned())
    }
}

/// Encrypts with a fresh random nonce; returns `(nonce, ciphertext)`.
pub fn encrypt(key: &Key, plaintext: &[u8], additional: &[u8]) -> (Vec<u8>, Vec<u8>) {
    init();
    let nonce = random_bytes(unsafe { crypto_aead_xchacha20poly1305_ietf_npubbytes() });
    let mut ciphertext = vec![0u8; plaintext.len() + unsafe { crypto_aead_xchacha20poly1305_ietf_abytes() }];
    let mut length: c_ulonglong = 0;
    unsafe {
        crypto_aead_xchacha20poly1305_ietf_encrypt(
            ciphertext.as_mut_ptr(),
            &mut length,
            plaintext.as_ptr(),
            plaintext.len() as c_ulonglong,
            additional.as_ptr(),
            additional.len() as c_ulonglong,
            std::ptr::null(),
            nonce.as_ptr(),
            key.0.as_ptr(),
        );
    }
    ciphertext.truncate(length as usize);
    (nonce, ciphertext)
}

/// Decrypts and authenticates; fails on a wrong key or any tampering.
pub fn decrypt(key: &Key, nonce: &[u8], ciphertext: &[u8], additional: &[u8]) -> Result<Secret, String> {
    init();
    let abytes = unsafe { crypto_aead_xchacha20poly1305_ietf_abytes() };
    if nonce.len() != unsafe { crypto_aead_xchacha20poly1305_ietf_npubbytes() } || ciphertext.len() < abytes {
        return Err("poškozená data".to_owned());
    }
    let mut plaintext = Secret::new(vec![0u8; ciphertext.len() - abytes]);
    let mut length: c_ulonglong = 0;
    let rc = unsafe {
        crypto_aead_xchacha20poly1305_ietf_decrypt(
            plaintext.0.as_mut_ptr(),
            &mut length,
            std::ptr::null_mut(),
            ciphertext.as_ptr(),
            ciphertext.len() as c_ulonglong,
            additional.as_ptr(),
            additional.len() as c_ulonglong,
            nonce.as_ptr(),
            key.0.as_ptr(),
        )
    };
    if rc != 0 {
        return Err("nesprávný klíč nebo poškozená data".to_owned());
    }
    plaintext.0.truncate(length as usize);
    Ok(plaintext)
}

#[cfg(test)]
mod tests {
    use super::*;

    pub fn fast() -> KdfParams {
        KdfParams {
            opslimit: 1,
            memlimit: 8192 * 1024,
        }
    }

    #[test]
    fn encryption_round_trip_and_tamper_detection() {
        let key = Key::random();
        let (nonce, mut ciphertext) = encrypt(&key, b"heslo123", b"server-1");
        assert_eq!(decrypt(&key, &nonce, &ciphertext, b"server-1").unwrap().as_bytes(), b"heslo123");
        assert!(decrypt(&key, &nonce, &ciphertext, b"server-2").is_err());
        assert!(decrypt(&Key::random(), &nonce, &ciphertext, b"server-1").is_err());
        ciphertext[0] ^= 1;
        assert!(decrypt(&key, &nonce, &ciphertext, b"server-1").is_err());
    }

    #[test]
    fn key_derivation_is_deterministic_per_salt() {
        let salt = random_bytes(salt_bytes());
        let password = Secret::from_str("tajné heslo");
        let a = derive_key(&password, &salt, fast()).unwrap();
        let b = derive_key(&password, &salt, fast()).unwrap();
        let (nonce, ciphertext) = encrypt(&a, b"x", b"");
        assert!(decrypt(&b, &nonce, &ciphertext, b"").is_ok());
        let other = derive_key(&password, &random_bytes(salt_bytes()), fast()).unwrap();
        assert!(decrypt(&other, &nonce, &ciphertext, b"").is_err());
    }

    #[test]
    fn hex_round_trip_and_debug_hides_secrets() {
        assert_eq!(unhex(&hex(&[0, 1, 255])).unwrap(), [0, 1, 255]);
        assert!(unhex("abc").is_none());
        assert!(unhex("zz").is_none());
        assert_eq!(format!("{:?}", Secret::from_str("tajne")), "Secret(***)");
        assert!(equal(b"abc", b"abc"));
        assert!(!equal(b"abc", b"abd"));
    }
}
