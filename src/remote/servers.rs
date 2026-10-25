//! Database of remote servers in `~/.config/salamander-rs/servers`. Passwords are never
//! stored here; they live encrypted in the vault under the server id.

use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuthMethod {
    /// Whatever ssh tries: agent, keys, then password.
    Auto,
    Password,
    Key,
}

impl AuthMethod {
    pub const ALL: [AuthMethod; 3] = [AuthMethod::Auto, AuthMethod::Password, AuthMethod::Key];

    pub fn label(self) -> &'static str {
        match self {
            AuthMethod::Auto => "Automaticky (agent, klíče, heslo)",
            AuthMethod::Password => "Heslo",
            AuthMethod::Key => "Soukromý klíč",
        }
    }

    fn key(self) -> &'static str {
        match self {
            AuthMethod::Auto => "auto",
            AuthMethod::Password => "password",
            AuthMethod::Key => "key",
        }
    }

    fn parse(text: &str) -> AuthMethod {
        match text {
            "password" => AuthMethod::Password,
            "key" => AuthMethod::Key,
            _ => AuthMethod::Auto,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Server {
    pub id: String,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub user: String,
    pub auth: AuthMethod,
    pub identity: Option<PathBuf>,
    pub initial_dir: String,
    /// Extra ssh arguments, e.g. `-o ProxyJump=bastion`.
    pub options: String,
    /// Offer to keep the password in the vault.
    pub save_password: bool,
}

impl Server {
    pub fn new() -> Server {
        Server {
            id: crate::sodium::random_hex(8),
            name: String::new(),
            host: String::new(),
            port: 22,
            user: std::env::var("USER").unwrap_or_default(),
            auth: AuthMethod::Auto,
            identity: None,
            initial_dir: String::new(),
            options: String::new(),
            save_password: false,
        }
    }

    /// `user@host` or `user@host:port` for non-default ports.
    pub fn label(&self) -> String {
        let user = if self.user.is_empty() { String::new() } else { format!("{}@", self.user) };
        if self.port == 22 {
            format!("{user}{}", self.host)
        } else {
            format!("{user}{}:{}", self.host, self.port)
        }
    }

    pub fn display_name(&self) -> String {
        if self.name.is_empty() {
            self.label()
        } else {
            self.name.clone()
        }
    }

    /// Rejects values that could be taken as ssh options or break the file format.
    pub fn validate(&self) -> Result<(), String> {
        let plain = |value: &str, what: &str| -> Result<(), String> {
            if value.starts_with('-') || value.chars().any(|ch| ch.is_whitespace() || ch.is_control()) {
                Err(format!("{what} nesmí začínat „-“ ani obsahovat mezery"))
            } else {
                Ok(())
            }
        };
        if self.host.is_empty() {
            return Err("Zadejte adresu serveru".to_owned());
        }
        plain(&self.host, "Adresa serveru")?;
        plain(&self.user, "Uživatelské jméno")?;
        if self.port == 0 {
            return Err("Neplatný port".to_owned());
        }
        for (value, what) in [
            (self.name.as_str(), "Název"),
            (self.initial_dir.as_str(), "Počáteční adresář"),
            (self.options.as_str(), "Volby SSH"),
        ] {
            if value.contains(['\n', '\r']) {
                return Err(format!("{what} nesmí obsahovat konec řádku"));
            }
        }
        if self.auth == AuthMethod::Key && self.identity.is_none() {
            return Err("Pro ověření klíčem vyberte soubor se soukromým klíčem".to_owned());
        }
        Ok(())
    }

    /// Extra ssh arguments split on whitespace.
    pub fn extra_args(&self) -> Vec<String> {
        self.options.split_whitespace().map(str::to_owned).collect()
    }
}

pub fn default_path() -> PathBuf {
    gtk::glib::user_config_dir().join("salamander-rs").join("servers")
}

pub fn parse(text: &str) -> Vec<Server> {
    let mut servers = Vec::new();
    let mut current: Option<Server> = None;
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if line == "[server]" {
            servers.extend(current.take());
            current = Some(Server {
                id: String::new(),
                user: String::new(),
                ..Server::new()
            });
            continue;
        }
        let (Some(server), Some((key, value))) = (current.as_mut(), line.split_once('=')) else {
            continue;
        };
        match key {
            "id" => server.id = value.to_owned(),
            "name" => server.name = value.to_owned(),
            "host" => server.host = value.to_owned(),
            "port" => server.port = value.parse().unwrap_or(22),
            "user" => server.user = value.to_owned(),
            "auth" => server.auth = AuthMethod::parse(value),
            "identity" => server.identity = (!value.is_empty()).then(|| PathBuf::from(value)),
            "dir" => server.initial_dir = value.to_owned(),
            "options" => server.options = value.to_owned(),
            "save_password" => server.save_password = value == "true",
            _ => {}
        }
    }
    servers.extend(current);
    servers.retain(|server| !server.id.is_empty() && !server.host.is_empty());
    servers
}

pub fn serialize(servers: &[Server]) -> String {
    let mut text = String::from("# Salamander RS — vzdálené servery (hesla jsou v šifrovaném trezoru)\n");
    for server in servers {
        text.push_str(&format!(
            "\n[server]\nid={}\nname={}\nhost={}\nport={}\nuser={}\nauth={}\nidentity={}\ndir={}\noptions={}\nsave_password={}\n",
            server.id,
            server.name,
            server.host,
            server.port,
            server.user,
            server.auth.key(),
            server.identity.as_deref().map(|path| path.to_string_lossy().into_owned()).unwrap_or_default(),
            server.initial_dir,
            server.options,
            server.save_password,
        ));
    }
    text
}

pub fn load(path: &Path) -> Vec<Server> {
    fs::read_to_string(path).map(|text| parse(&text)).unwrap_or_default()
}

pub fn save(path: &Path, servers: &[Server]) -> Result<(), String> {
    let error = |error: std::io::Error| format!("Nelze uložit seznam serverů: {error}");
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(error)?;
    }
    let temporary = path.with_extension("tmp");
    let _ = fs::remove_file(&temporary);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)
        .map_err(error)?;
    file.write_all(serialize(servers).as_bytes()).map_err(error)?;
    drop(file);
    fs::rename(&temporary, path).map_err(error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn servers_round_trip() {
        let mut a = Server::new();
        a.name = "Produkce".to_owned();
        a.host = "example.com".to_owned();
        a.user = "admin".to_owned();
        a.port = 2222;
        a.auth = AuthMethod::Key;
        a.identity = Some(PathBuf::from("/home/u/.ssh/id_ed25519"));
        a.initial_dir = "/var/www".to_owned();
        a.options = "-o ProxyJump=bastion".to_owned();
        a.save_password = true;
        let mut b = Server::new();
        b.host = "10.0.0.5".to_owned();
        let servers = vec![a.clone(), b.clone()];
        assert_eq!(parse(&serialize(&servers)), servers);
        assert_eq!(a.label(), "admin@example.com:2222");
        assert_eq!(a.extra_args(), ["-o", "ProxyJump=bastion"]);

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("servers");
        save(&path, &servers).unwrap();
        assert_eq!(load(&path), servers);
    }

    #[test]
    fn validation_rejects_option_injection() {
        let mut server = Server::new();
        server.host = "-oProxyCommand=evil".to_owned();
        assert!(server.validate().is_err());
        server.host = "ok.example".to_owned();
        server.user = "-l".to_owned();
        assert!(server.validate().is_err());
        server.user = "me".to_owned();
        assert!(server.validate().is_ok());
        server.name = "a\nb".to_owned();
        assert!(server.validate().is_err());
        server.name.clear();
        server.auth = AuthMethod::Key;
        assert!(server.validate().is_err());
    }
}
