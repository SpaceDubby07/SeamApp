import { useEffect, useState } from "react";
import type { Config, Hotkey, KeyCode, RemapTable } from "../lib/types";
import * as ipc from "../lib/ipc";

interface Props {
  config: Config | null;
  onConfigChanged: (config: Config) => void;
  /** Whether a session is live — the lock toggle needs one. */
  connected: boolean;
  /** Current lock-to-screen state, from the status stream. */
  locked: boolean;
}

// Browser KeyboardEvent.code → seam_core::protocol::KeyCode variant name.
// Only the keys worth binding an emergency hotkey to; returns null for a
// bare modifier or anything unmapped.
function codeToKeyCode(code: string): KeyCode | null {
  if (/^Key[A-Z]$/.test(code)) return code.slice(3);
  if (/^Digit[0-9]$/.test(code)) return code;
  if (/^F([1-9]|1[0-2])$/.test(code)) return code;
  if (/^Numpad[0-9]$/.test(code)) return code;
  const direct: Record<string, KeyCode> = {
    Escape: "Escape",
    Tab: "Tab",
    CapsLock: "CapsLock",
    Space: "Space",
    Enter: "Enter",
    Backspace: "Backspace",
    Delete: "Delete",
    Insert: "Insert",
    Home: "Home",
    End: "End",
    PageUp: "PageUp",
    PageDown: "PageDown",
    ArrowUp: "ArrowUp",
    ArrowDown: "ArrowDown",
    ArrowLeft: "ArrowLeft",
    ArrowRight: "ArrowRight",
    Minus: "Minus",
    Equal: "Equal",
    BracketLeft: "LeftBracket",
    BracketRight: "RightBracket",
    Backslash: "Backslash",
    Semicolon: "Semicolon",
    Quote: "Quote",
    Comma: "Comma",
    Period: "Period",
    Slash: "Slash",
    Backquote: "Backquote",
    NumLock: "NumLock",
    NumpadAdd: "NumpadAdd",
    NumpadSubtract: "NumpadSubtract",
    NumpadMultiply: "NumpadMultiply",
    NumpadDivide: "NumpadDivide",
    NumpadDecimal: "NumpadDecimal",
    NumpadEnter: "NumpadEnter",
    PrintScreen: "PrintScreen",
    ScrollLock: "ScrollLock",
    Pause: "Pause",
    ContextMenu: "ContextMenu",
  };
  return direct[code] ?? null;
}

function hotkeyLabel(hk: Hotkey): string {
  const parts: string[] = [];
  if (hk.ctrl) parts.push("Ctrl");
  if (hk.shift) parts.push("Shift");
  if (hk.alt) parts.push("Alt");
  if (hk.meta) parts.push("Cmd");
  parts.push(hk.key);
  return parts.join(" + ");
}

const EMPTY_TABLE: RemapTable = {
  rules: [],
  invert_scroll_y: false,
  invert_scroll_x: false,
};

// Mirrors seam_core::remap::RemapTable::windows_keyboard_on_mac().
const WINDOWS_ON_MAC: RemapTable = {
  rules: [
    { physical: "LeftCtrl", injected: "LeftMeta" },
    { physical: "RightCtrl", injected: "RightMeta" },
    { physical: "LeftMeta", injected: "LeftCtrl" },
    { physical: "RightMeta", injected: "RightCtrl" },
  ],
  invert_scroll_y: true,
  invert_scroll_x: false,
};

// The named variants of seam_core::protocol::KeyCode, grouped for the
// row selects. Kept in sync with protocol/messages.rs by hand.
const KEY_GROUPS: { label: string; keys: KeyCode[] }[] = [
  { label: "Letters", keys: "ABCDEFGHIJKLMNOPQRSTUVWXYZ".split("") },
  {
    label: "Digits",
    keys: Array.from({ length: 10 }, (_, i) => `Digit${i}`),
  },
  {
    label: "Function",
    keys: Array.from({ length: 12 }, (_, i) => `F${i + 1}`),
  },
  {
    label: "Modifiers",
    keys: [
      "LeftShift",
      "RightShift",
      "LeftCtrl",
      "RightCtrl",
      "LeftAlt",
      "RightAlt",
      "LeftMeta",
      "RightMeta",
    ],
  },
  {
    label: "Navigation & editing",
    keys: [
      "Escape",
      "Tab",
      "CapsLock",
      "Space",
      "Enter",
      "Backspace",
      "Delete",
      "Insert",
      "Home",
      "End",
      "PageUp",
      "PageDown",
      "ArrowUp",
      "ArrowDown",
      "ArrowLeft",
      "ArrowRight",
    ],
  },
  {
    label: "Punctuation",
    keys: [
      "Minus",
      "Equal",
      "LeftBracket",
      "RightBracket",
      "Backslash",
      "Semicolon",
      "Quote",
      "Comma",
      "Period",
      "Slash",
      "Backquote",
    ],
  },
  {
    label: "Other",
    keys: ["PrintScreen", "ScrollLock", "Pause", "ContextMenu", "NumLock"],
  },
  {
    label: "Numpad",
    keys: [
      ...Array.from({ length: 10 }, (_, i) => `Numpad${i}`),
      "NumpadAdd",
      "NumpadSubtract",
      "NumpadMultiply",
      "NumpadDivide",
      "NumpadDecimal",
      "NumpadEnter",
    ],
  },
];

function KeySelect({
  value,
  onChange,
}: {
  value: KeyCode;
  onChange: (k: KeyCode) => void;
}) {
  // An existing rule might name a key not in KEY_GROUPS (older config, or
  // an Unknown(n) that stringified) — keep it selectable so editing one
  // field doesn't silently rewrite the other.
  const known = KEY_GROUPS.some((g) => g.keys.includes(value));
  return (
    <select value={value} onChange={(e) => onChange(e.target.value)}>
      {!known && <option value={value}>{value}</option>}
      {KEY_GROUPS.map((g) => (
        <optgroup key={g.label} label={g.label}>
          {g.keys.map((k) => (
            <option key={k} value={k}>
              {k}
            </option>
          ))}
        </optgroup>
      ))}
    </select>
  );
}

/** Tier 8.1 panel 3 (Input): modifier-remap table, the Ctrl↔Cmd preset,
 * and scroll-direction toggles. Every edit is persisted and pushed into a
 * running session immediately (see `ipc.setRemap`). Escape-hotkey binding
 * and lock-to-screen are not wired yet. */
export function InputPanel({
  config,
  onConfigChanged,
  connected,
  locked,
}: Props) {
  const [error, setError] = useState<string | null>(null);
  const [recording, setRecording] = useState(false);
  const table = config?.remap ?? EMPTY_TABLE;
  const hotkey = config?.escape_hotkey ?? null;

  function commit(next: RemapTable) {
    if (!config) return;
    setError(null);
    onConfigChanged({ ...config, remap: next });
    ipc.setRemap(next).catch((e) => setError(String(e)));
  }

  function commitHotkey(next: Hotkey) {
    if (!config) return;
    setError(null);
    onConfigChanged({ ...config, escape_hotkey: next });
    ipc.setEscapeHotkey(next).catch((e) => setError(String(e)));
  }

  // Click-to-record: capture the next real key press while armed.
  useEffect(() => {
    if (!recording) return;
    function onKey(e: KeyboardEvent) {
      e.preventDefault();
      e.stopPropagation();
      const key = codeToKeyCode(e.code);
      if (!key) return; // bare modifier or unmapped — keep waiting
      if (!(e.ctrlKey || e.shiftKey || e.altKey || e.metaKey)) {
        setError("Add at least one modifier (Ctrl / Shift / Alt / Cmd).");
        setRecording(false);
        return;
      }
      commitHotkey({
        ctrl: e.ctrlKey,
        shift: e.shiftKey,
        alt: e.altKey,
        meta: e.metaKey,
        key,
      });
      setRecording(false);
    }
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [recording, config]);

  const isPreset =
    table.invert_scroll_y &&
    !table.invert_scroll_x &&
    table.rules.length === WINDOWS_ON_MAC.rules.length &&
    WINDOWS_ON_MAC.rules.every((want) =>
      table.rules.some(
        (r) => r.physical === want.physical && r.injected === want.injected,
      ),
    );

  function setRule(i: number, patch: Partial<RemapTable["rules"][number]>) {
    commit({
      ...table,
      rules: table.rules.map((r, j) => (j === i ? { ...r, ...patch } : r)),
    });
  }

  return (
    <section className="panel">
      <h2>Input</h2>
      <p className="muted">
        Remapping is applied on this machine as keys are injected. Changes
        take effect immediately, including on an active connection.
      </p>

      <div className="row">
        <button
          onClick={() => commit(WINDOWS_ON_MAC)}
          disabled={!config || isPreset}
        >
          {isPreset ? "Preset active" : "Windows keyboard on Mac (swap Ctrl/Cmd)"}
        </button>
        {table.rules.length > 0 && (
          <button onClick={() => commit({ ...table, rules: [] })}>
            Clear mappings
          </button>
        )}
      </div>

      <h3>Scroll direction</h3>
      <label className="check">
        <input
          type="checkbox"
          checked={table.invert_scroll_y}
          disabled={!config}
          onChange={(e) =>
            commit({ ...table, invert_scroll_y: e.target.checked })
          }
        />
        Invert vertical scroll
      </label>
      <label className="check">
        <input
          type="checkbox"
          checked={table.invert_scroll_x}
          disabled={!config}
          onChange={(e) =>
            commit({ ...table, invert_scroll_x: e.target.checked })
          }
        />
        Invert horizontal scroll
      </label>

      <h3>Escape hotkey</h3>
      <div className="row">
        <code className="hotkey-label">
          {recording
            ? "Press a combo…"
            : hotkey
              ? hotkeyLabel(hotkey)
              : "—"}
        </code>
        <button
          disabled={!config}
          onClick={() => {
            setError(null);
            setRecording((r) => !r);
          }}
        >
          {recording ? "Cancel" : "Record…"}
        </button>
      </div>
      <p className="muted">
        Force-returns control to this machine from anywhere — works even
        while a peer is driving.
      </p>

      <h3>Lock to screen</h3>
      <label className="check">
        <input
          type="checkbox"
          checked={locked}
          disabled={!connected}
          onChange={(e) => {
            setError(null);
            ipc.setLocked(e.target.checked).catch((err) => setError(String(err)));
          }}
        />
        Disable edge handoff {connected ? "" : "(connect to a peer first)"}
      </label>

      <h3>Key mappings</h3>
      {table.rules.length === 0 ? (
        <p className="muted">No mappings — every key injects as itself.</p>
      ) : (
        <ul className="remap-list">
          {table.rules.map((rule, i) => (
            <li key={i} className="remap-row">
              <KeySelect
                value={rule.physical}
                onChange={(k) => setRule(i, { physical: k })}
              />
              <span className="remap-arrow">→</span>
              <KeySelect
                value={rule.injected}
                onChange={(k) => setRule(i, { injected: k })}
              />
              <button
                className="link-btn"
                onClick={() =>
                  commit({
                    ...table,
                    rules: table.rules.filter((_, j) => j !== i),
                  })
                }
              >
                Remove
              </button>
            </li>
          ))}
        </ul>
      )}
      <button
        disabled={!config}
        onClick={() =>
          commit({
            ...table,
            rules: [...table.rules, { physical: "A", injected: "A" }],
          })
        }
      >
        Add mapping
      </button>

      {error && <p className="error">{error}</p>}
    </section>
  );
}
