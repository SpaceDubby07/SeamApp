//! Modifier remap tables and scroll-direction inversion (Tier 7.3 of the
//! build guide, M6).
//!
//! Applied on the RECEIVING side, at injection time — never on the sending
//! side. Each machine owns its own remap rules, so e.g. only the Mac needs
//! to know "swap Ctrl and Cmd"; the Windows side sends physical key codes
//! unchanged and is none the wiser (`session.rs` is where this actually
//! gets called, right before `InputSink::inject`).

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::protocol::{InputEvent, KeyCode};

/// Maps physical key codes to what should actually be injected, plus
/// scroll-direction inversion. Stored per-machine in
/// [`crate::config::Config`], so a Windows keyboard driving a Mac can swap
/// Ctrl<->Cmd while the Windows side leaves everything alone.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RemapTable {
    /// Physical code -> injected code. A code with no entry passes through
    /// unchanged.
    pub rules: HashMap<KeyCode, KeyCode>,
    /// Invert scroll on the vertical axis at injection time. Needed e.g.
    /// when the injecting machine has "natural scrolling" on and the
    /// sending machine doesn't.
    pub invert_scroll_y: bool,
    /// Invert scroll on the horizontal axis at injection time.
    pub invert_scroll_x: bool,
}

impl RemapTable {
    /// The Windows-keyboard-driving-a-Mac case: Ctrl<->Cmd means
    /// Ctrl+C/V/A/Z/Tab all work as muscle memory expects, in both
    /// directions (`LeftCtrl`->`LeftMeta` AND `LeftMeta`->`LeftCtrl`, so a
    /// combo like Cmd+Shift+4 built entirely from remapped keys still comes
    /// out consistent — Tier 7.3's edge case).
    #[must_use]
    pub fn windows_keyboard_on_mac() -> Self {
        Self {
            rules: HashMap::from([
                (KeyCode::LeftCtrl, KeyCode::LeftMeta),
                (KeyCode::RightCtrl, KeyCode::RightMeta),
                (KeyCode::LeftMeta, KeyCode::LeftCtrl),
                (KeyCode::RightMeta, KeyCode::RightCtrl),
            ]),
            // Most Macs ship with natural scrolling on; a non-Mac peer's
            // wheel otherwise feels backwards once forwarded.
            invert_scroll_y: true,
            invert_scroll_x: false,
        }
    }

    /// Looks up what `code` should actually be injected as. Passes through
    /// unchanged if there's no rule for it.
    #[must_use]
    pub fn remap_key(&self, code: KeyCode) -> KeyCode {
        self.rules.get(&code).copied().unwrap_or(code)
    }

    /// Applies this table's key remap and scroll inversion to a relayed
    /// event, producing what should actually be injected locally.
    /// Non-key, non-scroll events pass through untouched.
    #[must_use]
    pub fn apply(&self, event: InputEvent) -> InputEvent {
        match event {
            InputEvent::KeyDown { code, repeat } => InputEvent::KeyDown {
                code: self.remap_key(code),
                repeat,
            },
            InputEvent::KeyUp { code } => InputEvent::KeyUp {
                code: self.remap_key(code),
            },
            InputEvent::Scroll { dx, dy } => InputEvent::Scroll {
                dx: if self.invert_scroll_x { -dx } else { dx },
                dy: if self.invert_scroll_y { -dy } else { dy },
            },
            other => other,
        }
    }
}

// TOML requires string map keys, so `rules` travels on disk as a Vec of
// `{physical, injected}` pairs rather than as a native map — these two
// private types are only the wire/disk shape, converted to/from the real
// `HashMap` immediately below.

#[derive(Serialize, Deserialize)]
struct RemapRule {
    physical: KeyCode,
    injected: KeyCode,
}

#[derive(Serialize, Deserialize)]
struct RemapTableRepr {
    rules: Vec<RemapRule>,
    invert_scroll_y: bool,
    invert_scroll_x: bool,
}

impl Serialize for RemapTable {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let repr = RemapTableRepr {
            rules: self
                .rules
                .iter()
                .map(|(&physical, &injected)| RemapRule { physical, injected })
                .collect(),
            invert_scroll_y: self.invert_scroll_y,
            invert_scroll_x: self.invert_scroll_x,
        };
        repr.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for RemapTable {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let repr = RemapTableRepr::deserialize(deserializer)?;
        Ok(Self {
            rules: repr
                .rules
                .into_iter()
                .map(|r| (r.physical, r.injected))
                .collect(),
            invert_scroll_y: repr.invert_scroll_y,
            invert_scroll_x: repr.invert_scroll_x,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::RemapTable;
    use crate::protocol::{InputEvent, KeyCode};

    #[test]
    fn unmapped_key_passes_through_unchanged() {
        let table = RemapTable::default();
        assert_eq!(table.remap_key(KeyCode::A), KeyCode::A);
    }

    #[test]
    fn windows_keyboard_on_mac_swaps_ctrl_and_cmd_both_ways() {
        let table = RemapTable::windows_keyboard_on_mac();
        assert_eq!(table.remap_key(KeyCode::LeftCtrl), KeyCode::LeftMeta);
        assert_eq!(table.remap_key(KeyCode::LeftMeta), KeyCode::LeftCtrl);
        assert_eq!(table.remap_key(KeyCode::RightCtrl), KeyCode::RightMeta);
        assert_eq!(table.remap_key(KeyCode::RightMeta), KeyCode::RightCtrl);
    }

    #[test]
    fn a_combo_built_from_remapped_keys_stays_consistent() {
        // Cmd+Shift+4 (macOS screenshot), driven from a Windows keyboard:
        // Ctrl (physical) + Shift + 4 must all still be individually
        // correct once each key is remapped independently.
        let table = RemapTable::windows_keyboard_on_mac();
        assert_eq!(table.remap_key(KeyCode::LeftCtrl), KeyCode::LeftMeta);
        assert_eq!(table.remap_key(KeyCode::LeftShift), KeyCode::LeftShift);
        assert_eq!(table.remap_key(KeyCode::Digit4), KeyCode::Digit4);
    }

    #[test]
    fn apply_remaps_keydown_and_keyup_the_same_way() {
        let table = RemapTable::windows_keyboard_on_mac();
        assert_eq!(
            table.apply(InputEvent::KeyDown {
                code: KeyCode::LeftCtrl,
                repeat: false
            }),
            InputEvent::KeyDown {
                code: KeyCode::LeftMeta,
                repeat: false
            }
        );
        assert_eq!(
            table.apply(InputEvent::KeyUp {
                code: KeyCode::LeftCtrl
            }),
            InputEvent::KeyUp {
                code: KeyCode::LeftMeta
            }
        );
    }

    #[test]
    fn scroll_inversion_only_flips_the_configured_axes() {
        let table = RemapTable {
            invert_scroll_y: true,
            ..RemapTable::default()
        };
        assert_eq!(
            table.apply(InputEvent::Scroll { dx: 3, dy: 5 }),
            InputEvent::Scroll { dx: 3, dy: -5 }
        );
    }

    #[test]
    fn non_key_non_scroll_events_pass_through_unchanged() {
        let table = RemapTable::windows_keyboard_on_mac();
        let event = InputEvent::MouseMoveAbs { x: 42, y: 7 };
        assert_eq!(table.apply(event), event);
    }

    #[test]
    fn toml_roundtrip_preserves_rules_and_inversion() {
        let table = RemapTable::windows_keyboard_on_mac();
        let toml_str = toml::to_string(&table).expect("serialize");
        let parsed: RemapTable = toml::from_str(&toml_str).expect("deserialize");
        assert_eq!(parsed, table);
    }
}
