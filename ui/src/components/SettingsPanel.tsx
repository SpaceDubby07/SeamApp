import { useState } from "react";
import * as ipc from "../lib/ipc";
import type { Config } from "../lib/types";
import { LogPanel } from "./LogPanel";

interface Props {
  open: boolean;
  onClose: () => void;
  config: Config | null;
  onConfigChanged: (config: Config) => void;
}

/** Slide-over drawer: this device's name, the paired peer (with Forget),
 * and the debug log — everything that isn't part of the primary
 * connect/transfer flow lives here instead of cluttering it. */
export function SettingsPanel({ open, onClose, config, onConfigChanged }: Props) {
  const [error, setError] = useState<string | null>(null);

  async function handleNameChange(name: string) {
    if (!config) return;
    onConfigChanged({ ...config, display_name: name });
    try {
      await ipc.setDisplayName(name);
    } catch (e) {
      setError(String(e));
    }
  }

  async function handleForget() {
    if (!config) return;
    onConfigChanged({ ...config, paired_peer: null });
    try {
      await ipc.forgetPeer();
    } catch (e) {
      setError(String(e));
    }
  }

  const pairedPeer = config?.paired_peer ?? null;

  return (
    <div
      className={`settings-overlay ${open ? "is-open" : ""}`}
      onMouseDown={(e) => {
        if (e.target === e.currentTarget) onClose();
      }}
    >
      <aside className="settings-drawer">
        <div className="settings-head">
          <h2>Settings</h2>
          <button className="icon-btn" onClick={onClose} title="Close" aria-label="Close">
            ✕
          </button>
        </div>

        <div className="settings-section">
          <label className="field">
            This device's name
            <input
              value={config?.display_name ?? ""}
              onChange={(e) => handleNameChange(e.target.value)}
            />
          </label>

          {pairedPeer ? (
            <div className="paired-row">
              <span className="muted">
                Paired · <code>{pairedPeer.fingerprint.slice(0, 12)}…</code>
              </span>
              <button className="link-btn" onClick={handleForget}>
                Forget
              </button>
            </div>
          ) : (
            <p className="muted">No device paired yet.</p>
          )}

          {error && <p className="error">{error}</p>}
        </div>

        <div className="settings-section">
          <LogPanel />
        </div>
      </aside>
    </div>
  );
}
