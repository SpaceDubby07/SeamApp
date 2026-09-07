import type { LinkStatus } from "../lib/types";

interface Props {
  /** `null` when no peer is connected. */
  peerName: string | null;
  localName: string;
  /** Latest control owner, or `null` before the first `Status` event. */
  link: LinkStatus | null;
  /** Latest round-trip time in microseconds, or `null` before the first pong. */
  rttMicros: number | null;
  /** Whether edge handoff is currently locked to this screen. */
  locked: boolean;
  /** The connection dropped and the app is retrying with backoff. */
  reconnecting: boolean;
}

/** Tier 8.1 panel 6: the always-visible bottom bar — connection dot + peer
 * name, latency, which machine currently has control, and a lock
 * indicator. Deliberately terse; it's chrome, not a panel. */
export function StatusBar({
  peerName,
  localName,
  link,
  rttMicros,
  locked,
  reconnecting,
}: Props) {
  const connected = peerName !== null;

  if (reconnecting) {
    return (
      <footer className="status-bar">
        <span className="status-dot is-reconnecting" />
        <span className="status-peer">
          Reconnecting{peerName ? ` to ${peerName}` : ""}…
        </span>
      </footer>
    );
  }

  const latency =
    rttMicros === null ? "—" : `${Math.round(rttMicros / 1000)} ms`;

  const control = !connected
    ? "—"
    : link === "Driven"
      ? `${peerName} is driving this device`
      : link === "Driving"
        ? `controlling ${peerName}`
        : localName;

  return (
    <footer className="status-bar">
      <span className={`status-dot ${connected ? "is-connected" : ""}`} />
      <span className="status-peer">
        {connected ? peerName : "Not connected"}
      </span>
      <span className="status-sep">·</span>
      <span title="Control-channel round-trip">{latency}</span>
      <span className="status-sep">·</span>
      <span title="Which machine currently has control">{control}</span>
      {locked && (
        <>
          <span className="status-sep">·</span>
          <span className="status-lock" title="Edge handoff is locked">
            🔒 locked
          </span>
        </>
      )}
    </footer>
  );
}
