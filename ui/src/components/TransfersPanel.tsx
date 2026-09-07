import { useCallback, useEffect, useRef, useState } from "react";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import * as ipc from "../lib/ipc";
import type { SessionEvent } from "../lib/types";

interface Props {
  /** Active (non-history) rows are cleared when this goes false. */
  connected: boolean;
}

type Status = "pending" | "active" | "completed" | "failed" | "rejected";

interface Transfer {
  id: string;
  name: string;
  incoming: boolean;
  total: number;
  bytesDone: number;
  status: Status;
  path?: string;
  reason?: string;
  /** Recent (time, bytes) points for a rolling-average speed. */
  samples: { t: number; bytes: number }[];
}

const SPEED_WINDOW_MS = 1500;
const DONE: Status[] = ["completed", "failed", "rejected"];

function fmtBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  const units = ["KB", "MB", "GB", "TB"];
  let v = n / 1024;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  return `${v.toFixed(v < 10 ? 1 : 0)} ${units[i]}`;
}

/** Bytes/sec over the trailing ~1s of samples (Tier 8.1: "rolling 1s
 * average, not instantaneous"), or null with too little history. */
function rollingSpeed(samples: Transfer["samples"]): number | null {
  if (samples.length < 2) return null;
  const last = samples[samples.length - 1];
  const first = samples[0];
  const dt = (last.t - first.t) / 1000;
  if (dt <= 0) return null;
  return (last.bytes - first.bytes) / dt;
}

function fmtEta(seconds: number): string {
  if (!Number.isFinite(seconds) || seconds < 0) return "—";
  if (seconds < 60) return `${Math.ceil(seconds)}s`;
  const m = Math.floor(seconds / 60);
  const s = Math.ceil(seconds % 60);
  return `${m}m ${s.toString().padStart(2, "0")}s`;
}

/** Tier 8.1 panel 4: drop zone, active transfers with progress/speed/ETA/
 * cancel, incoming-offer prompts, and a completed-transfer history. */
export function TransfersPanel({ connected }: Props) {
  const [transfers, setTransfers] = useState<Map<string, Transfer>>(new Map());
  const [dragOver, setDragOver] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const wasConnected = useRef(connected);

  const upsert = useCallback(
    (id: string, patch: Partial<Transfer>, base?: Partial<Transfer>) => {
      setTransfers((prev) => {
        const next = new Map(prev);
        const existing = next.get(id);
        const row: Transfer = existing
          ? { ...existing, ...patch }
          : {
              id,
              name: "file",
              incoming: false,
              total: 0,
              bytesDone: 0,
              status: "active",
              samples: [],
              ...base,
              ...patch,
            };
        next.set(id, row);
        return next;
      });
    },
    [],
  );

  // Session events → transfer rows.
  useEffect(() => {
    const handle = (event: SessionEvent) => {
      switch (event.type) {
        case "OfferReceived":
          upsert(
            event.transfer_id,
            {
              name: event.manifest.name,
              incoming: true,
              total: event.manifest.size,
              status: "pending",
            },
            {},
          );
          break;
        case "Progress": {
          const now = Date.now();
          setTransfers((prev) => {
            const next = new Map(prev);
            const existing = next.get(event.transfer_id);
            const samples = [
              ...(existing?.samples ?? []),
              { t: now, bytes: event.bytes_done },
            ].filter((s) => now - s.t <= SPEED_WINDOW_MS);
            next.set(event.transfer_id, {
              id: event.transfer_id,
              name: event.name,
              incoming: event.incoming,
              total: event.total,
              bytesDone: event.bytes_done,
              status: "active",
              path: existing?.path,
              reason: existing?.reason,
              samples,
            });
            return next;
          });
          break;
        }
        case "Completed":
          upsert(event.transfer_id, {
            status: "completed",
            path: event.path,
            samples: [],
          });
          break;
        case "Failed":
          upsert(event.transfer_id, {
            status: "failed",
            reason: event.reason,
            samples: [],
          });
          break;
        case "Rejected":
          upsert(event.transfer_id, {
            status: "rejected",
            reason: event.reason,
            samples: [],
          });
          break;
        default:
          break;
      }
    };
    const unlisten = ipc.onSessionEvent(handle);
    return () => {
      unlisten.then((f) => f());
    };
  }, [upsert]);

  // Drop a file anywhere on the window to offer it (Tier 15 keeps
  // cross-edge drag out of v1; this in-app drop zone is the whole story).
  useEffect(() => {
    const unlisten = getCurrentWebview().onDragDropEvent((event) => {
      if (event.payload.type === "over" || event.payload.type === "enter") {
        setDragOver(true);
      } else if (event.payload.type === "leave") {
        setDragOver(false);
      } else if (event.payload.type === "drop") {
        setDragOver(false);
        setError(null);
        for (const path of event.payload.paths) {
          ipc.sendFile(path).catch((e) => setError(String(e)));
        }
      }
    });
    return () => {
      unlisten.then((f) => f());
    };
  }, []);

  // Drop the in-flight rows on disconnect; keep the history.
  useEffect(() => {
    if (wasConnected.current && !connected) {
      setTransfers((prev) => {
        const next = new Map<string, Transfer>();
        for (const [id, t] of prev) {
          if (DONE.includes(t.status)) next.set(id, t);
          else if (t.status !== "pending")
            next.set(id, { ...t, status: "failed", reason: "disconnected" });
        }
        return next;
      });
    }
    wasConnected.current = connected;
  }, [connected]);

  const rows = [...transfers.values()];
  const active = rows.filter((t) => !DONE.includes(t.status));
  const history = rows.filter((t) => DONE.includes(t.status)).reverse();

  return (
    <section className="panel">
      <h2>Transfers</h2>

      <div className={`drop-zone ${dragOver ? "is-over" : ""}`}>
        {connected
          ? "Drop files here to send them to the peer"
          : "Connect to a peer to send files"}
      </div>

      {error && <p className="error">{error}</p>}

      {active.length > 0 && (
        <ul className="xfer-list">
          {active.map((t) => {
            const speed = rollingSpeed(t.samples);
            const pct = t.total > 0 ? (t.bytesDone / t.total) * 100 : 0;
            const eta =
              speed && speed > 0 ? (t.total - t.bytesDone) / speed : Infinity;
            return (
              <li key={t.id} className="xfer">
                <div className="xfer-head">
                  <span className="xfer-name">
                    {t.incoming ? "↓" : "↑"} {t.name}
                  </span>
                  {t.status === "pending" ? (
                    <span className="row">
                      <button
                        className="primary"
                        onClick={() => ipc.respondToOffer(t.id, true)}
                      >
                        Accept
                      </button>
                      <button onClick={() => ipc.respondToOffer(t.id, false)}>
                        Reject
                      </button>
                    </span>
                  ) : (
                    <button
                      onClick={() =>
                        ipc.cancelTransfer(t.id).catch((e) => setError(String(e)))
                      }
                    >
                      Cancel
                    </button>
                  )}
                </div>
                {t.status === "pending" ? (
                  <span className="muted">
                    Incoming · {fmtBytes(t.total)}
                  </span>
                ) : (
                  <>
                    <div className="xfer-bar">
                      <div
                        className="xfer-bar-fill"
                        style={{ width: `${pct}%` }}
                      />
                    </div>
                    <span className="muted">
                      {fmtBytes(t.bytesDone)} / {fmtBytes(t.total)}
                      {speed !== null && ` · ${fmtBytes(speed)}/s`}
                      {speed !== null && ` · ${fmtEta(eta)} left`}
                    </span>
                  </>
                )}
              </li>
            );
          })}
        </ul>
      )}

      {history.length > 0 && (
        <>
          <h3>History</h3>
          <ul className="xfer-list">
            {history.map((t) => (
              <li key={t.id} className="xfer xfer-done">
                <div className="xfer-head">
                  <span className="xfer-name">
                    {t.incoming ? "↓" : "↑"} {t.name}
                  </span>
                  <span
                    className={`xfer-status xfer-status-${t.status}`}
                    title={t.reason ?? ""}
                  >
                    {t.status}
                    {t.status === "completed" && t.path && (
                      <button
                        className="link-btn"
                        onClick={() =>
                          ipc.revealPath(t.path!).catch(() => undefined)
                        }
                      >
                        Reveal
                      </button>
                    )}
                  </span>
                </div>
              </li>
            ))}
          </ul>
        </>
      )}
    </section>
  );
}
