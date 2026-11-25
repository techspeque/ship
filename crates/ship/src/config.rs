//! The client's targets, kept in a 0600 JSON file.
//!
//! `$SHIP_CONFIG`, else `$XDG_CONFIG_HOME/ship/config.json`, else
//! `~/.config/ship/config.json`. The file holds target URLs and socket paths
//! only: never a secret. It is rewritten atomically: a new 0600 file is
//! written and synced beside it, then renamed over it.

use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use ship_core::Slug;
use ship_net::{Target, TargetError};

/// Mode of the config file.
pub const FILE_MODE: u32 = 0o600;
/// Mode of a config directory this client creates.
pub const DIR_MODE: u32 = 0o700;

/// One stored target: the URL as given and an optional daemon socket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredTarget {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub socket: Option<String>,
}

impl StoredTarget {
    /// The parsed target. Stored values were validated when added; a file
    /// edited by hand is validated again here.
    pub fn target(&self) -> Result<Target, TargetError> {
        Target::parse(&self.url, self.socket.as_deref())
    }
}

/// The whole config file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    /// The target commands use; `None` means the local default socket.
    #[serde(default)]
    pub current: Option<String>,
    #[serde(default)]
    pub targets: BTreeMap<String, StoredTarget>,
}

/// Why the config could not be read, changed or written.
#[derive(Debug)]
pub enum ConfigError {
    NoLocation,
    Read(PathBuf, io::Error),
    Parse(PathBuf, serde_json::Error),
    Write(PathBuf, io::Error),
    InvalidName(String),
    Target(TargetError),
    Exists(String),
    Unknown(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::NoLocation => f.write_str(
                "cannot locate the config file: set SHIP_CONFIG, XDG_CONFIG_HOME or HOME",
            ),
            ConfigError::Read(p, e) => write!(f, "reading {}: {e}", p.display()),
            ConfigError::Parse(p, e) => write!(f, "{} is not valid: {e}", p.display()),
            ConfigError::Write(p, e) => write!(f, "writing {}: {e}", p.display()),
            ConfigError::InvalidName(why) => write!(f, "invalid target name: {why}"),
            ConfigError::Target(e) => e.fmt(f),
            ConfigError::Exists(name) => write!(
                f,
                "a target named `{name}` already exists; pick another name"
            ),
            ConfigError::Unknown(name) => write!(
                f,
                "no target named `{name}`; `ship target list` shows the targets"
            ),
        }
    }
}

impl std::error::Error for ConfigError {}

impl ConfigError {
    /// Whether the error is the user's input rather than the environment.
    pub fn is_usage(&self) -> bool {
        matches!(
            self,
            ConfigError::InvalidName(_)
                | ConfigError::Target(_)
                | ConfigError::Exists(_)
                | ConfigError::Unknown(_)
        )
    }
}

/// Where the config file lives, from the given environment values.
pub fn location(
    ship_config: Option<&str>,
    xdg_config_home: Option<&str>,
    home: Option<&str>,
) -> Option<PathBuf> {
    let set = |v: Option<&str>| v.filter(|s| !s.is_empty()).map(PathBuf::from);
    if let Some(p) = set(ship_config) {
        return Some(p);
    }
    if let Some(p) = set(xdg_config_home).filter(|p| p.is_absolute()) {
        return Some(p.join("ship").join("config.json"));
    }
    set(home).map(|h| h.join(".config").join("ship").join("config.json"))
}

/// The config file's location from this process's environment.
pub fn default_location() -> Result<PathBuf, ConfigError> {
    let var = |k: &str| std::env::var(k).ok();
    location(
        var("SHIP_CONFIG").as_deref(),
        var("XDG_CONFIG_HOME").as_deref(),
        var("HOME").as_deref(),
    )
    .ok_or(ConfigError::NoLocation)
}

impl Config {
    /// Reads `path`; a missing file is an empty config.
    pub fn load(path: &Path) -> Result<Config, ConfigError> {
        match fs::read(path) {
            Ok(bytes) => {
                serde_json::from_slice(&bytes).map_err(|e| ConfigError::Parse(path.into(), e))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(ConfigError::Read(path.into(), e)),
        }
    }

    /// Writes the config to `path` atomically at mode 0600, creating its
    /// directory at 0700 if missing.
    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        let err = |e| ConfigError::Write(path.into(), e);
        let dir = path
            .parent()
            .filter(|d| !d.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        if !dir.exists() {
            DirBuilder::new()
                .recursive(true)
                .mode(DIR_MODE)
                .create(dir)
                .map_err(err)?;
        }
        let mut text = serde_json::to_string_pretty(self).map_err(|e| err(io::Error::other(e)))?;
        text.push('\n');

        let name = path
            .file_name()
            .ok_or_else(|| err(io::Error::other("config path has no file name")))?;
        let mut tmp_name = name.to_os_string();
        tmp_name.push(format!(".tmp-{}", std::process::id()));
        let tmp = dir.join(tmp_name);
        let _ = fs::remove_file(&tmp);
        let write = || -> io::Result<()> {
            let mut f = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(FILE_MODE)
                .open(&tmp)?;
            f.write_all(text.as_bytes())?;
            f.sync_all()?;
            fs::rename(&tmp, path)
        };
        write().map_err(|e| {
            let _ = fs::remove_file(&tmp);
            err(e)
        })
    }

    /// Adds target `name`. The first target added becomes current.
    /// Returns whether it became current.
    pub fn add(
        &mut self,
        name: &str,
        url: &str,
        socket: Option<&str>,
    ) -> Result<bool, ConfigError> {
        Slug::parse(name).map_err(|e| ConfigError::InvalidName(e.to_string()))?;
        let target = Target::parse(url, socket).map_err(ConfigError::Target)?;
        if self.targets.contains_key(name) {
            return Err(ConfigError::Exists(name.to_owned()));
        }
        self.targets.insert(
            name.to_owned(),
            StoredTarget {
                url: target.url(),
                socket: socket.map(str::to_owned),
            },
        );
        let current = self.current.is_none();
        if current {
            self.current = Some(name.to_owned());
        }
        Ok(current)
    }

    /// Makes `name` the current target.
    pub fn use_target(&mut self, name: &str) -> Result<(), ConfigError> {
        if !self.targets.contains_key(name) {
            return Err(ConfigError::Unknown(name.to_owned()));
        }
        self.current = Some(name.to_owned());
        Ok(())
    }

    /// The target commands use: the current one, or `None` for the local
    /// default socket when no target is current.
    pub fn current(&self) -> Result<Option<(&str, Target)>, ConfigError> {
        let Some(name) = self.current.as_deref() else {
            return Ok(None);
        };
        let stored = self
            .targets
            .get(name)
            .ok_or_else(|| ConfigError::Unknown(name.to_owned()))?;
        let target = stored.target().map_err(ConfigError::Target)?;
        Ok(Some((name, target)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn mode(p: &Path) -> u32 {
        fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn locations() {
        assert_eq!(
            location(Some("/x/c.json"), Some("/xdg"), Some("/home/a")),
            Some(PathBuf::from("/x/c.json"))
        );
        assert_eq!(
            location(None, Some("/xdg"), Some("/home/a")),
            Some(PathBuf::from("/xdg/ship/config.json"))
        );
        assert_eq!(
            location(Some(""), Some("relative"), Some("/home/a")),
            Some(PathBuf::from("/home/a/.config/ship/config.json"))
        );
        assert_eq!(location(None, None, None), None);
    }

    #[test]
    fn missing_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            Config::load(&dir.path().join("none.json")).unwrap(),
            Config::default()
        );
    }

    #[test]
    fn save_is_0600_in_a_0700_dir_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a").join("b").join("config.json");
        let mut c = Config::default();
        assert!(c.add("lab", "ssh://adam@lab:2222", None).unwrap());
        assert!(!c.add("here", "local", Some("/tmp/s.sock")).unwrap());
        c.save(&path).unwrap();
        assert_eq!(mode(&path), FILE_MODE);
        assert_eq!(mode(path.parent().unwrap()), DIR_MODE);
        assert_eq!(Config::load(&path).unwrap(), c);
        let leftovers: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(leftovers, ["config.json"]);
    }

    #[test]
    fn save_narrows_a_wider_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        fs::write(&path, "{}").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        Config::default().save(&path).unwrap();
        assert_eq!(mode(&path), FILE_MODE);
    }

    #[test]
    fn first_target_is_current_and_use_switches() {
        let mut c = Config::default();
        assert_eq!(c.current().unwrap(), None);
        c.add("lab", "ssh://lab", None).unwrap();
        c.add("gpu", "ssh://gpu", None).unwrap();
        assert_eq!(c.current().unwrap().unwrap().0, "lab");
        c.use_target("gpu").unwrap();
        assert_eq!(c.current().unwrap().unwrap().0, "gpu");
        assert!(matches!(c.use_target("nope"), Err(ConfigError::Unknown(_))));
        assert_eq!(c.current().unwrap().unwrap().0, "gpu");
    }

    #[test]
    fn add_refuses_bad_input_and_duplicates() {
        let mut c = Config::default();
        c.add("lab", "ssh://lab", None).unwrap();
        let before = c.clone();
        assert!(matches!(
            c.add("lab", "ssh://other", None),
            Err(ConfigError::Exists(_))
        ));
        assert!(matches!(
            c.add("Bad Name", "local", None),
            Err(ConfigError::InvalidName(_))
        ));
        assert!(matches!(
            c.add("x", "tailnet://lab", None),
            Err(ConfigError::Target(TargetError::TailnetNotYet))
        ));
        assert!(matches!(
            c.add("x", "ssh://lab;id", None),
            Err(ConfigError::Target(TargetError::InvalidHost))
        ));
        assert_eq!(c, before);
    }

    #[test]
    fn stored_url_is_normalised() {
        let mut c = Config::default();
        c.add("lab", "SSH://lab", None).unwrap();
        assert_eq!(c.targets["lab"].url, "ssh://lab");
    }
}
