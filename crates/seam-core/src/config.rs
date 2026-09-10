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
use crate::protocol::{KeyCode, Modifiers};
use crate::remap::RemapTable;
use crate::topology::NodeId;
use crate::transfer::AcceptPolicy;

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
    /// Whether to prompt, always accept, or always deny incoming file
    /// transfer offers from the paired peer (Tier 7.5, M10).
    /// `#[serde(default)]` so a config file written before M10 still
    /// loads, defaulting to `AcceptPolicy::Ask`.
    #[serde(default)]
    pub accept_policy: AcceptPolicy,
    /// Where accepted incoming files are written. `None` means "the OS
    /// Downloads folder," resolved lazily via [`Self::resolved_download_dir`]
    /// rather than baked in at config-creation time, so it still tracks a
    /// later OS-level change. `#[serde(default)]` for the same pre-M10
    /// compatibility reason as `accept_policy`.
    #[serde(default)]
    pub download_dir: Option<PathBuf>,
    /// The key combo that force-returns control to the local machine from
    /// anywhere (Tier 7.7, the Input panel's click-to-record binding).
    /// `#[serde(default)]` → a config written before this field existed
    /// gets [`Hotkey::default`].
    #[serde(default)]
    pub escape_hotkey: Hotkey,
    /// Edge-handoff tuning (Tier 8.1's per-edge settings, applied globally
    /// in v1's single-shared-edge topology). `#[serde(default)]` for the
    /// same forward-compat reason.
    #[serde(default)]
    pub edge_settings: EdgeSettings,
}

/// A modifier + key combo, as bound to the emergency "return control
/// here" action. Modifiers are matched exactly (all four flags), so
/// Ctrl+Shift+Alt+Q doesn't also fire a Ctrl+Q binding.
// Four independent physical modifier states, mirroring `Modifiers`
// field-for-field — not a mode selector, so clippy's enum suggestion
// doesn't apply (same call as on `Modifiers` itself).
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hotkey {
    /// Ctrl must be held (either physical Ctrl).
    #[serde(default)]
    pub ctrl: bool,
    /// Shift must be held.
    #[serde(default)]
    pub shift: bool,
    /// Alt/Option must be held.
    #[serde(default)]
    pub alt: bool,
    /// Cmd/Win must be held.
    #[serde(default)]
    pub meta: bool,
    /// The non-modifier key that triggers it.
    pub key: KeyCode,
}

impl Default for Hotkey {
    /// Ctrl+Alt+Backslash. Deliberately NOT the old Shift+Ctrl+Alt+Escape:
    /// holding Ctrl+Alt+Shift together is the prefix for Windows 10 +
    /// Microsoft 365's global "Office key" launcher hotkeys, and
    /// Ctrl+Shift+Esc is Task Manager — relaying that modifier set to a
    /// Windows peer at handoff time triggered those. `\` has no OS
    /// binding on macOS or Windows and is a comfortable one-hand combo.
    /// Rebind it in the Input panel.
    fn default() -> Self {
        Self {
            ctrl: true,
            shift: false,
            alt: true,
            meta: false,
            key: KeyCode::Backslash,
        }
    }
}

impl Hotkey {
    /// Whether a `key` press with `held` physical modifiers is this combo.
    #[must_use]
    pub fn matches(&self, key: KeyCode, held: Modifiers) -> bool {
        self.key == key
            && held.ctrl == self.ctrl
            && held.shift == self.shift
            && held.alt == self.alt
            && held.meta == self.meta
    }
}

/// Edge-handoff tuning. Per-edge in the build guide's UI sketch; a single
/// global instance here, since v1 pairs exactly two machines across one
/// shared edge (Tier 15 keeps the door open for per-edge later).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct EdgeSettings {
    /// Corner exclusion zone, in pixels: a crossing whose cursor is within
    /// this of a corner is ignored, so reaching for a corner UI element
    /// doesn't trigger a handoff (Tier 7.2).
    #[serde(default = "default_corner_dead_zone_px")]
    pub corner_dead_zone_px: u32,
    /// How long after a handoff before the reverse handoff can fire, in
    /// milliseconds — stops the boundary flickering (Tier 7.2).
    #[serde(default = "default_handoff_cooldown_ms")]
    pub handoff_cooldown_ms: u64,
}

impl Default for EdgeSettings {
    fn default() -> Self {
        Self {
            corner_dead_zone_px: default_corner_dead_zone_px(),
            handoff_cooldown_ms: default_handoff_cooldown_ms(),
        }
    }
}

/// Tier 7.2's default 20px corner dead zone.
fn default_corner_dead_zone_px() -> u32 {
    20
}

/// Tier 7.2's default 200ms post-handoff cooldown.
fn default_handoff_cooldown_ms() -> u64 {
    200
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
            accept_policy: AcceptPolicy::default(),
            download_dir: None,
            escape_hotkey: Hotkey::default(),
            edge_settings: EdgeSettings::default(),
        }
    }

    /// Where incoming transfers should be written: `download_dir` if set,
    /// else the OS's standard Downloads folder, else the current
    /// directory as a last resort (e.g. a CI/test environment with no
    /// concept of a home directory).
    #[must_use]
    pub fn resolved_download_dir(&self) -> PathBuf {
        self.download_dir.clone().unwrap_or_else(|| {
            directories::UserDirs::new()
                .and_then(|dirs| dirs.download_dir().map(Path::to_path_buf))
                .unwrap_or_else(|| PathBuf::from("."))
        })
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

    /// A config written before the Input/Layout panels existed (no
    /// `escape_hotkey` / `edge_settings` sections) still loads, falling
    /// back to the defaults via `#[serde(default)]`.
    #[test]
    fn config_without_hotkey_or_edge_settings_still_loads_with_defaults() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        let old_toml = format!(
            "node_id = \"{}\"\ndisplay_name = \"old machine\"\n\n[remap]\nrules = []\ninvert_scroll_y = false\ninvert_scroll_x = false\n",
            uuid::Uuid::new_v4()
        );
        std::fs::write(&path, old_toml).expect("write old config");

        let loaded = Config::load_or_create(&path).expect("load");
        assert_eq!(loaded.escape_hotkey, super::Hotkey::default());
        assert_eq!(loaded.edge_settings, super::EdgeSettings::default());
    }
}
