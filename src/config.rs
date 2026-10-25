//! Persistent settings in `~/.config/salamander-rs/config` as simple `key=value` lines.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub const HOTPATH_COUNT: usize = 10;

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub left_dir: Option<PathBuf>,
    pub right_dir: Option<PathBuf>,
    pub show_hidden: bool,
    pub width: i32,
    pub height: i32,
    pub maximized: bool,
    pub paned_position: i32,
    pub show_ext_column: bool,
    pub show_permissions_column: bool,
    pub show_owner_column: bool,
    /// Shows the `..` row leading to the parent directory.
    pub show_parent_entry: bool,
    pub hotpaths: [Option<PathBuf>; HOTPATH_COUNT],
}

impl Default for Config {
    fn default() -> Self {
        Config {
            left_dir: None,
            right_dir: None,
            show_hidden: false,
            width: 1200,
            height: 700,
            maximized: false,
            paned_position: 600,
            show_ext_column: false,
            show_permissions_column: true,
            show_owner_column: false,
            show_parent_entry: false,
            hotpaths: Default::default(),
        }
    }
}

pub fn config_path() -> PathBuf {
    gtk::glib::user_config_dir()
        .join("salamander-rs")
        .join("config")
}

impl Config {
    pub fn load() -> Config {
        fs::read_to_string(config_path())
            .map(|text| Config::parse(&text))
            .unwrap_or_default()
    }

    pub fn save(&self) -> io::Result<()> {
        let path = config_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        // Write to a temporary file first so a crash never leaves a truncated config.
        let temporary = path.with_extension("tmp");
        fs::write(&temporary, self.serialize())?;
        fs::rename(temporary, path)
    }

    pub fn parse(text: &str) -> Config {
        let mut config = Config::default();
        for line in text.lines() {
            let line = line.trim_end_matches('\r');
            if line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let key = key.trim();
            let path = || (!value.is_empty()).then(|| PathBuf::from(value));
            let flag = || value.trim() == "true";
            let number = |default: i32| value.trim().parse().unwrap_or(default);
            match key {
                "left_dir" => config.left_dir = path(),
                "right_dir" => config.right_dir = path(),
                "show_hidden" => config.show_hidden = flag(),
                "width" => config.width = number(config.width).max(400),
                "height" => config.height = number(config.height).max(300),
                "maximized" => config.maximized = flag(),
                "paned_position" => config.paned_position = number(config.paned_position).max(0),
                "column_ext" => config.show_ext_column = flag(),
                "column_permissions" => config.show_permissions_column = flag(),
                "column_owner" => config.show_owner_column = flag(),
                "parent_entry" => config.show_parent_entry = flag(),
                _ => {
                    if let Some(index) = key
                        .strip_prefix("hotpath")
                        .and_then(|index| index.parse::<usize>().ok())
                        .filter(|index| *index < HOTPATH_COUNT)
                    {
                        config.hotpaths[index] = path();
                    }
                }
            }
        }
        config
    }

    pub fn serialize(&self) -> String {
        let mut output = String::from("# Salamander RS settings\n");
        let mut push_path = |key: &str, path: &Option<PathBuf>| {
            // Paths with newlines cannot be represented in this line format.
            if let Some(text) = path.as_deref().and_then(Path::to_str) {
                if !text.contains('\n') {
                    output.push_str(&format!("{key}={text}\n"));
                }
            }
        };
        push_path("left_dir", &self.left_dir);
        push_path("right_dir", &self.right_dir);
        for (index, hotpath) in self.hotpaths.iter().enumerate() {
            push_path(&format!("hotpath{index}"), hotpath);
        }
        output.push_str(&format!(
            "show_hidden={}\nwidth={}\nheight={}\nmaximized={}\npaned_position={}\n\
             column_ext={}\ncolumn_permissions={}\ncolumn_owner={}\nparent_entry={}\n",
            self.show_hidden,
            self.width,
            self.height,
            self.maximized,
            self.paned_position,
            self.show_ext_column,
            self.show_permissions_column,
            self.show_owner_column,
            self.show_parent_entry,
        ));
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_all_fields() {
        let mut config = Config {
            left_dir: Some(PathBuf::from("/home/user/projekty")),
            right_dir: Some(PathBuf::from("/tmp/a=b")),
            show_hidden: true,
            width: 1600,
            height: 900,
            maximized: true,
            paned_position: 777,
            show_ext_column: true,
            show_permissions_column: false,
            show_owner_column: true,
            show_parent_entry: true,
            hotpaths: Default::default(),
        };
        config.hotpaths[1] = Some(PathBuf::from("/srv/data"));
        config.hotpaths[0] = Some(PathBuf::from("/etc"));
        assert_eq!(Config::parse(&config.serialize()), config);
    }

    #[test]
    fn ignores_garbage_and_clamps_sizes() {
        let config = Config::parse("nonsense\nwidth=10\nhotpath42=/x\nheight=abc\n");
        assert_eq!(config.width, 400);
        assert_eq!(config.height, 700);
        assert!(config.hotpaths.iter().all(Option::is_none));
    }
}
