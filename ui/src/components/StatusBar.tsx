interface Props {
  /** `null` when no peer is connected. */
  peerName: string | null;
  /** Latest round-trip time in microseconds, or `null` before the first pong. */
  rttMicros: number | null;
  /** The connection dropped and the app is retrying with backoff. */
  reconnecting: boolean;
  /** Called when the user clicks "Disconnect" (or "Cancel" while
   * reconnecting). Omitted entirely when there's nothing to disconnect. */
  onDisconnect: () => void;
}

/** Lives inline in the titlebar: a connection dot, the peer's name,
 * latency, and a disconnect action. Renders nothing while there's no peer
 * and nothing to retry — the connect screen already says "not connected"
 * more prominently than a status strip could. */
export function StatusBar({ peerName, rttMicros, reconnecting, onDisconnect }: Props) {
  if (!peerName && !reconnecting) return null;

  if (reconnecting) {
    return (
      <span className="titlebar-status">
        <span className="status-dot is-reconnecting" />
        <span className="status-peer">
          Reconnecting{peerName ? ` to ${peerName}` : ""}…
        </span>
        <button className="link-btn" onClick={onDisconnect}>
          Cancel
        </button>
      </span>
    );
  }

  const latency =
    rttMicros === null ? null : `${Math.round(rttMicros / 1000)} ms`;

  return (
    <span className="titlebar-status">
      <span className="status-dot is-connected" />
      <span className="status-peer">{peerName}</span>
      {latency && (
        <>
          <span className="status-sep">·</span>
          <span title="Control-channel round-trip">{latency}</span>
        </>
      )}
      <button className="link-btn" onClick={onDisconnect}>
        Disconnect
      </button>
    </span>
  );
}
