import { useState } from "react";
import type { ConnectedInfo, DiscoveredPeer } from "../lib/types";
import * as ipc from "../lib/ipc";

interface Props {
  peers: DiscoveredPeer[];
  connected: ConnectedInfo | null;
  pairingCode: string | null;
  reconnecting: boolean;
}

/** The full-bleed "not connected yet" screen: discovered devices, a
 * fallback manual-IP connect, and the pairing-code confirmation step. Once
 * a session is live this is replaced by the transfer workspace — see
 * `App.tsx`'s `showWorkspace`. Paired-device info and "Forget" live in
 * Settings instead, since those stay relevant while connected too. */
export function ConnectionPanel({
  peers,
  connected,
  pairingCode,
  reconnecting,
}: Props) {
  const [manualAddr, setManualAddr] = useState("");
  const [connecting, setConnecting] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  async function handleConnect(addr: string) {
    setError(null);
    setConnecting(addr);
    try {
      await ipc.connectToPeer(addr);
    } catch (e) {
      setError(String(e));
    } finally {
      setConnecting(null);
    }
  }

  if (pairingCode) {
    return (
      <section className="panel">
        <h2 className="hero-title">Confirm pairing</h2>
        <p className="hero-subtitle">
          Make sure this EXACT code is shown on the other machine
        </p>
        <div className="pairing-code">{pairingCode}</div>
        <div className="row">
          <button
            className="primary"
            onClick={() => ipc.confirmPairing(true)}
          >
            Codes match — trust this device
          </button>
          <button onClick={() => ipc.confirmPairing(false)}>
            Doesn't match — abort
          </button>
        </div>
      </section>
    );
  }

  if (reconnecting) {
    const name = connected?.peer_display_name;
    return (
      <section className="panel">
        <h2 className="hero-title">Reconnecting…</h2>
        <p className="hero-subtitle">
          Connection lost{name ? ` to ${name}` : ""} — retrying
        </p>
        <button onClick={() => ipc.disconnect()}>Cancel</button>
      </section>
    );
  }

  return (
    <section className="panel">
      <h2 className="hero-title">Connect a device</h2>
      <p className="hero-subtitle">
        Open Seam on your other machine — it'll show up here automatically
      </p>

      {peers.length === 0 ? (
        <p className="muted">Looking for devices on your network…</p>
      ) : (
        <ul className="peer-list">
          {peers.map((peer) => (
            <li key={peer.node_id}>
              <span>
                {peer.display_name} <span className="muted">({peer.os})</span>
              </span>
              <button
                disabled={connecting === peer.addr}
                onClick={() => handleConnect(peer.addr)}
              >
                {connecting === peer.addr ? "Connecting…" : "Connect"}
              </button>
            </li>
          ))}
        </ul>
      )}

      {error && <p className="error">{error}</p>}

      <details className="manual-connect">
        <summary>Connect by IP address instead</summary>
        <div className="row">
          <input
            placeholder="192.168.1.50"
            value={manualAddr}
            onChange={(e) => setManualAddr(e.target.value)}
          />
          <button
            disabled={!manualAddr || connecting === manualAddr}
            onClick={() => handleConnect(manualAddr)}
          >
            {connecting === manualAddr ? "Connecting…" : "Connect"}
          </button>
        </div>
      </details>
    </section>
  );
}
