import { useEffect, useState } from "react";
import "./App.css";
import { ConnectionPanel } from "./components/ConnectionPanel";
import { InputPanel } from "./components/InputPanel";
import { LayoutCanvas } from "./components/LayoutCanvas";
import { LogPanel } from "./components/LogPanel";
import { StatusBar } from "./components/StatusBar";
import { TransfersPanel } from "./components/TransfersPanel";
import * as ipc from "./lib/ipc";
import type {
  Config,
  ConnectedInfo,
  DiscoveredPeer,
  Display,
  EdgeSettings,
  LinkStatus,
  Rect,
} from "./lib/types";

function App() {
  const [config, setConfig] = useState<Config | null>(null);
  const [localScreens, setLocalScreens] = useState<[Display[], Rect] | null>(
    null,
  );
  const [peers, setPeers] = useState<DiscoveredPeer[]>([]);
  const [pairingCode, setPairingCode] = useState<string | null>(null);
  const [connected, setConnected] = useState<ConnectedInfo | null>(null);
  const [peerBounds, setPeerBounds] = useState<Rect | null>(null);
  const [link, setLink] = useState<LinkStatus | null>(null);
  const [rttMicros, setRttMicros] = useState<number | null>(null);
  const [locked, setLocked] = useState(false);

  useEffect(() => {
    ipc.getConfig().then(setConfig).catch(console.error);
    ipc.getLocalScreens().then(setLocalScreens).catch(console.error);
    ipc.listDiscoveredPeers().then(setPeers).catch(console.error);

    const unlisten = Promise.all([
      ipc.onPeersChanged(setPeers),
      ipc.onPairingRequested(setPairingCode),
      ipc.onConnected((info) => {
        setPairingCode(null);
        setConnected(info);
      }),
      ipc.onDisconnected(() => {
        setConnected(null);
        setPeerBounds(null);
        setLink(null);
        setRttMicros(null);
        setLocked(false);
      }),
      ipc.onSessionEvent((event) => {
        // `LayoutChanged` always carries our own naive initial placement
        // first, then the peer's real size once `PeerScreenConfig`
        // arrives (see Session::handle_peer_screen_config) — so this is
        // the one event the canvas needs for both position and size.
        if (event.type === "LayoutChanged") {
          setPeerBounds(event.peer_bounds);
        } else if (event.type === "Status") {
          setLink(event.link);
          setLocked(event.locked);
          if (event.rtt_micros !== null) setRttMicros(event.rtt_micros);
        }
      }),
    ]);

    return () => {
      unlisten.then((fns) => fns.forEach((f) => f()));
    };
  }, []);

  function handleLayoutDrag(bounds: Rect) {
    setPeerBounds(bounds);
    ipc.updateLayout(bounds).catch(console.error);
  }

  function handleEdgeSettingsChange(settings: EdgeSettings) {
    if (!config) return;
    setConfig({ ...config, edge_settings: settings });
    ipc.setEdgeSettings(settings).catch(console.error);
  }

  const [, localBounds] = localScreens ?? [[], null];

  return (
    <main className="app">
      <h1>Seam</h1>
      <ConnectionPanel
        config={config}
        peers={peers}
        connected={connected}
        pairingCode={pairingCode}
        onConfigChanged={setConfig}
      />
      {localBounds && (
        <LayoutCanvas
          localName={config?.display_name ?? "This device"}
          localBounds={localBounds}
          peerName={connected?.peer_display_name ?? null}
          peerBounds={peerBounds}
          onPeerBoundsChange={handleLayoutDrag}
          edgeSettings={config?.edge_settings ?? null}
          onEdgeSettingsChange={handleEdgeSettingsChange}
        />
      )}
      <InputPanel
        config={config}
        onConfigChanged={setConfig}
        connected={connected !== null}
        locked={locked}
      />
      <TransfersPanel connected={connected !== null} />
      <LogPanel />
      <StatusBar
        localName={config?.display_name ?? "This device"}
        peerName={connected?.peer_display_name ?? null}
        link={link}
        rttMicros={rttMicros}
        locked={locked}
      />
    </main>
  );
}

export default App;
