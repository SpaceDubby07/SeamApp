import { useState } from "react";
import type { Config, ConnectedInfo, DiscoveredPeer } from "../lib/types";
import * as ipc from "../lib/ipc";

interface Props {
  config: Config | null;
  peers: DiscoveredPeer[];
  connected: ConnectedInfo | null;
  pairingCode: string | null;
  onConfigChanged: (config: Config) => void;
}

export function ConnectionPanel({
  config,
  peers,
  connected,
  pairingCode,
  onConfigChanged,
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

  async function handleNameChange(name: string) {
    if (!config) return;
    onConfigChanged({ ...config, display_name: name });
    try {
      await ipc.setDisplayName(name);
    } catch (e) {
      setError(String(e));
    }
  }

  if (pairingCode) {
    return (
      <section className="panel">
        <h2>Confirm pairing</h2>
        <p>Confirm this EXACT code is shown on the other machine:</p>
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

  if (connected) {
    return (
      <section className="panel">
        <h2>Connected</h2>
        <p>
          Connected to <strong>{connected.peer_display_name}</strong>
        </p>
        <button onClick={() => ipc.disconnect()}>Disconnect</button>
      </section>
    );
  }

  return (
    <section className="panel">
      <h2>Connection</h2>
      <label className="field">
        This device's name
        <input
          value={config?.display_name ?? ""}
          onChange={(e) => handleNameChange(e.target.value)}
        />
      </label>

      <h3>Discovered devices</h3>
      {peers.length === 0 ? (
        <p className="muted">Looking for devices on your network...</p>
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
                {connecting === peer.addr ? "Connecting..." : "Connect"}
              </button>
            </li>
          ))}
        </ul>
      )}

      <h3>Manual IP</h3>
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
          {connecting === manualAddr ? "Connecting..." : "Connect"}
        </button>
      </div>

      {error && <p className="error">{error}</p>}
    </section>
  );
}
