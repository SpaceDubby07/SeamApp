import { useEffect, useState } from "react";
import "./App.css";
import { ConnectionPanel } from "./components/ConnectionPanel";
import { SettingsPanel } from "./components/SettingsPanel";
import { StatusBar } from "./components/StatusBar";
import { TransfersPanel } from "./components/TransfersPanel";
import * as ipc from "./lib/ipc";
import type { Config, ConnectedInfo, DiscoveredPeer } from "./lib/types";

function App() {
  const [config, setConfig] = useState<Config | null>(null);
  const [peers, setPeers] = useState<DiscoveredPeer[]>([]);
  const [pairingCode, setPairingCode] = useState<string | null>(null);
  const [connected, setConnected] = useState<ConnectedInfo | null>(null);
  const [rttMicros, setRttMicros] = useState<number | null>(null);
  const [reconnecting, setReconnecting] = useState(false);
  const [settingsOpen, setSettingsOpen] = useState(false);

  useEffect(() => {
    ipc.getConfig().then(setConfig).catch(console.error);
    ipc.listDiscoveredPeers().then(setPeers).catch(console.error);

    const unlisten = Promise.all([
      ipc.onPeersChanged(setPeers),
      ipc.onPairingRequested(setPairingCode),
      ipc.onConnected((info) => {
        setPairingCode(null);
        setReconnecting(false);
        setConnected(info);
      }),
      ipc.onReconnecting(() => {
        setReconnecting(true);
        setRttMicros(null);
      }),
      ipc.onDisconnected(() => {
        setConnected(null);
        setReconnecting(false);
        setRttMicros(null);
      }),
      ipc.onSessionEvent((event) => {
        if (event.type === "Status" && event.rtt_micros !== null) {
          setRttMicros(event.rtt_micros);
        }
      }),
    ]);

    return () => {
      unlisten.then((fns) => fns.forEach((f) => f()));
    };
  }, []);

  // A session, once established, is connected for its whole lifetime —
  // reconnect-with-backoff lives one layer up. So the workspace (transfer
  // drop zone + history) stays up across a reconnect blip instead of
  // bouncing back to the connect screen; only a deliberate disconnect (or
  // never having connected yet) shows the connect screen.
  const showWorkspace = connected !== null || reconnecting;

  return (
    <div className="app">
      <header className="titlebar">
        <span className="brand">
          <span className="brand-mark" />
          Seam
        </span>
        <StatusBar
          peerName={connected?.peer_display_name ?? null}
          rttMicros={rttMicros}
          reconnecting={reconnecting}
        />
        <button
          className="icon-btn"
          onClick={() => setSettingsOpen(true)}
          title="Settings"
          aria-label="Settings"
        >
          ⚙
        </button>
      </header>

      <main className="content">
        {showWorkspace ? (
          <div className="workspace">
            <TransfersPanel connected={connected !== null && !reconnecting} />
          </div>
        ) : (
          <div className="hero">
            <ConnectionPanel
              peers={peers}
              connected={connected}
              pairingCode={pairingCode}
              reconnecting={reconnecting}
            />
          </div>
        )}
      </main>

      <SettingsPanel
        open={settingsOpen}
        onClose={() => setSettingsOpen(false)}
        config={config}
        onConfigChanged={setConfig}
      />
    </div>
  );
}

export default App;
