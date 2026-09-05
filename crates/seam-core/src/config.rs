//! Persisted per-machine settings: node identity, display name, the
//! modifier remap table (Tier 3.2's `config.rs`, M6 of the build guide),
//! and the clipboard sync size cap (M7, Tier 7.4).
//!
//! Stored as TOML in the OS's standard per-user config directory via
//! `directories::ProjectDirs`. Deliberately excludes anything a later
//! milestone owns (paired-peer cert fingerprints — M8; layout/edge
//! settings — not yet scheduled) rather than guessing their shape now.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::net::tls::{Fingerprint, Trust};
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
    /// Hard cap, in bytes, on clipboard content we'll sync to the peer
    /// (Tier 7.4). Content over this is skipped entirely — not truncated —
    /// and logged. Defaults to 10 MB. `#[serde(default)]` so a config file
    /// written before this field existed (M6) still loads.
    #[serde(default = "default_clipboard_max_bytes")]
    pub clipboard_max_bytes: u64,
    /// The one peer we've paired with (Tier 7.6, M8) — a single `Option`
    /// rather than a collection, matching v1's "exactly one peer"
    /// simplification used throughout (`StateMachine`'s `peer: Option<NodeId>`
    /// is the same call; see Tier 15 for what a third machine would need).
    /// `#[serde(default)]` so a config file written before pairing existed
    /// (M6/M7) still loads, as "not yet paired with anyone."
    #[serde(default)]
    pub paired_peer: Option<PairedPeer>,
}

/// The peer this machine has paired with: its node identity and the
/// certificate fingerprint pinned for it.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PairedPeer {
    /// The peer's stable node identity, learned during pairing.
    pub node_id: NodeId,
    /// The peer's certificate fingerprint, pinned once a human confirms
    /// the pairing code matches on both screens.
    pub fingerprint: Fingerprint,
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
    /// a hostname-derived display name, the default 10 MB clipboard cap,
    /// and no paired peer yet.
    #[must_use]
    pub fn new_default() -> Self {
        Self {
            node_id: NodeId::new(),
            display_name: default_display_name(),
            remap: RemapTable::default(),
            clipboard_max_bytes: default_clipboard_max_bytes(),
            paired_peer: None,
        }
    }

    /// Which [`Trust`] mode a connection attempt should use: pinned to our
    /// paired peer's fingerprint if we have one, or [`Trust::OnFirstUse`]
    /// if we've never paired with anyone yet. Used by BOTH `connect` and
    /// `accept` — v1's single-peer simplification means there's no
    /// per-connection identity to look up ahead of time, only "have we
    /// paired with anyone at all" (Tier 7.6).
    #[must_use]
    pub fn trust_mode(&self) -> Trust {
        match &self.paired_peer {
            Some(peer) => Trust::Pinned(peer.fingerprint),
            None => Trust::OnFirstUse,
        }
    }

    /// Pins `fingerprint` as the trusted identity for `node_id`, after a
    /// human has confirmed the pairing code matches on both screens.
    pub fn pin_peer(&mut self, node_id: NodeId, fingerprint: Fingerprint) {
        self.paired_peer = Some(PairedPeer {
            node_id,
            fingerprint,
        });
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

    /// The default directory for this machine's TLS identity
    /// (`identity_cert.der`/`identity_key.der` — see
    /// [`crate::net::tls::NodeIdentity::load_or_create`]): the same
    /// OS-standard config directory `default_path` uses.
    ///
    /// # Errors
    /// Returns [`ConfigError::NoConfigDir`] if the OS won't report a config
    /// directory.
    pub fn identity_dir() -> Result<PathBuf, ConfigError> {
        let (qualifier, organization, application) = APP_QUALIFIER;
        directories::ProjectDirs::from(qualifier, organization, application)
            .map(|dirs| dirs.config_dir().to_path_buf())
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

/// 10 MB — Tier 7.4's default clipboard sync size cap.
fn default_clipboard_max_bytes() -> u64 {
    10 * 1024 * 1024
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

    /// M6 wrote config files without `clipboard_max_bytes` (added in M7).
    /// Loading one of those must not fail — it should fall back to the
    /// default cap via `#[serde(default)]`.
    #[test]
    fn config_written_before_clipboard_cap_existed_still_loads() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        let pre_m7_toml = format!(
            "node_id = \"{}\"\ndisplay_name = \"old machine\"\n\n[remap]\nrules = []\ninvert_scroll_y = false\ninvert_scroll_x = false\n",
            uuid::Uuid::new_v4()
        );
        std::fs::write(&path, pre_m7_toml).expect("write pre-M7 config");

        let loaded = Config::load_or_create(&path).expect("load");
        assert_eq!(
            loaded.clipboard_max_bytes,
            super::default_clipboard_max_bytes()
        );
    }
}
