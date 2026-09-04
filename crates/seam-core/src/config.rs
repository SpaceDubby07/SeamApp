//! Persisted per-machine settings: node identity, display name, and the
//! modifier remap table (Tier 3.2's `config.rs`, M6 of the build guide).
//!
//! Stored as TOML in the OS's standard per-user config directory via
//! `directories::ProjectDirs`. Deliberately excludes anything a later
//! milestone owns (paired-peer cert fingerprints — M8; layout/edge
//! settings — not yet scheduled) rather than guessing their shape now.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::remap::RemapTable;
use crate::topology::NodeId;

/// The app identity used to locate the config directory: matches
/// `seam-app`'s own `ProjectDirs::from("com", "zach", "seam")` for the log
/// directory, so both land under the same OS-standard app data root.
const APP_QUALIFIER: (&str, &str, &str) = ("com", "zach", "seam");

/// Everything persisted across runs for one machine. Loaded once at
/// startup, held by the app shell, saved back out whenever the user changes
/// something.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Config {
    /// Stable identity for this machine, generated once on first run and
    /// kept thereafter — regenerating it every run would break peer
    /// pairing (M8) and layout placement, both of which key on it.
    pub node_id: NodeId,
    /// User-facing name shown to peers, e.g. "Zach's laptop".
    pub display_name: String,
    /// This machine's modifier remap table and scroll inversion, applied
    /// at injection time to whatever the peer sends us (Tier 7.3).
    pub remap: RemapTable,
}

/// Things that can go wrong loading or saving [`Config`].
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The config file exists but isn't valid TOML for this shape.
    #[error("failed to parse config: {0}")]
    Parse(#[from] toml::de::Error),
    /// The config couldn't be serialized back to TOML.
    #[error("failed to serialize config: {0}")]
    Serialize(#[from] toml::ser::Error),
    /// A filesystem read/write/create-dir call failed.
    #[error("config I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// The OS wouldn't tell us where per-user config data belongs (e.g. no
    /// `$HOME`).
    #[error("could not determine the platform config directory")]
    NoConfigDir,
}

impl Config {
    /// A fresh default config: a new random node identity, no remap rules,
    /// and a hostname-derived display name.
    #[must_use]
    pub fn new_default() -> Self {
        Self {
            node_id: NodeId::new(),
            display_name: default_display_name(),
            remap: RemapTable::default(),
        }
    }

    /// Loads config from `path`, creating and persisting a fresh default if
    /// nothing exists there yet.
    ///
    /// # Errors
    /// Returns an error if the file exists but fails to parse, or if
    /// writing a fresh default fails.
    pub fn load_or_create(path: &Path) -> Result<Self, ConfigError> {
        match fs::read_to_string(path) {
            Ok(contents) => Ok(toml::from_str(&contents)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let config = Self::new_default();
                config.save(path)?;
                Ok(config)
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Writes this config to `path` as TOML, creating parent directories as
    /// needed.
    ///
    /// # Errors
    /// Returns an error if the parent directory can't be created, the
    /// config can't be serialized, or the write fails.
    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let contents = toml::to_string_pretty(self)?;
        fs::write(path, contents)?;
        Ok(())
    }

    /// The default config file path for this platform:
    /// `<app config dir>/config.toml`.
    ///
    /// # Errors
    /// Returns [`ConfigError::NoConfigDir`] if the OS won't report a config
    /// directory.
    pub fn default_path() -> Result<PathBuf, ConfigError> {
        let (qualifier, organization, application) = APP_QUALIFIER;
        directories::ProjectDirs::from(qualifier, organization, application)
            .map(|dirs| dirs.config_dir().join("config.toml"))
            .ok_or(ConfigError::NoConfigDir)
    }
}

/// Best-effort hostname lookup for the default display name. Falls back to
/// a generic name rather than failing config creation over it — the user
/// can always rename in the Connection panel (Tier 8.1).
fn default_display_name() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "Unnamed machine".to_string())
}

#[cfg(test)]
mod tests {
    use super::Config;
    use crate::remap::RemapTable;

    #[test]
    fn load_or_create_persists_a_fresh_default_on_first_run() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        assert!(!path.exists());

        let created = Config::load_or_create(&path).expect("create");
        assert!(path.exists());

        let loaded = Config::load_or_create(&path).expect("load");
        assert_eq!(loaded, created);
    }

    #[test]
    fn save_then_load_roundtrips_a_customized_config() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");

        let mut config = Config::new_default();
        config.display_name = "Zach's MacBook".to_string();
        config.remap = RemapTable::windows_keyboard_on_mac();
        config.save(&path).expect("save");

        let loaded = Config::load_or_create(&path).expect("load");
        assert_eq!(loaded, config);
    }

    #[test]
    fn node_id_is_stable_across_loads() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");

        let first = Config::load_or_create(&path).expect("first load creates");
        let second = Config::load_or_create(&path).expect("second load reads back");
        assert_eq!(first.node_id, second.node_id);
    }

    #[test]
    fn malformed_config_file_fails_to_parse_rather_than_silently_resetting() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "not valid toml {{{").expect("write garbage");

        let result = Config::load_or_create(&path);
        assert!(result.is_err());
    }
}
