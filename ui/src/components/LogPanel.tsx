import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import * as ipc from "../lib/ipc";
import type { LogLevel, LogLine } from "../lib/types";

const POLL_MS = 1000;
const MAX_LINES = 5000;

// Lowest level shown for each filter choice; higher-severity levels are
// always included.
const LEVEL_ORDER: Record<LogLevel, number> = {
  TRACE: 0,
  DEBUG: 1,
  INFO: 2,
  WARN: 3,
  ERROR: 4,
};

type LevelFilter = "TRACE" | "DEBUG" | "INFO" | "WARN";

function formatLine(l: LogLine): string {
  const t = new Date(l.ts_millis).toLocaleTimeString(undefined, {
    hour12: false,
  });
  const ms = String(l.ts_millis % 1000).padStart(3, "0");
  return `${t}.${ms}  ${l.level.padEnd(5)}  ${l.target}  ${l.message}`;
}

export function LogPanel() {
  const [lines, setLines] = useState<LogLine[]>([]);
  const [collapsed, setCollapsed] = useState(false);
  const [levelFilter, setLevelFilter] = useState<LevelFilter>("DEBUG");
  const [textFilter, setTextFilter] = useState("");
  const [autoScroll, setAutoScroll] = useState(true);
  const [exportedPath, setExportedPath] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const lastSeq = useRef(0);
  const scrollRef = useRef<HTMLDivElement | null>(null);

  // Live tail.
  useEffect(() => {
    let stopped = false;
    async function poll() {
      try {
        const fresh = await ipc.getLogs(lastSeq.current || undefined);
        if (stopped || fresh.length === 0) return;
        lastSeq.current = fresh[fresh.length - 1].seq;
        setLines((prev) => {
          const next = prev.concat(fresh);
          return next.length > MAX_LINES ? next.slice(-MAX_LINES) : next;
        });
      } catch (e) {
        console.error("get_logs failed", e);
      }
    }
    poll();
    const id = setInterval(poll, POLL_MS);
    return () => {
      stopped = true;
      clearInterval(id);
    };
  }, []);

  const shown = useMemo(() => {
    const min = LEVEL_ORDER[levelFilter];
    const needle = textFilter.trim().toLowerCase();
    return lines.filter((l) => {
      if (LEVEL_ORDER[l.level] < min) return false;
      if (!needle) return true;
      return (
        l.message.toLowerCase().includes(needle) ||
        l.target.toLowerCase().includes(needle)
      );
    });
  }, [lines, levelFilter, textFilter]);

  // Auto-scroll to bottom when new lines arrive, unless the user scrolled up.
  useEffect(() => {
    if (!autoScroll || collapsed) return;
    const el = scrollRef.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, [shown, autoScroll, collapsed]);

  const onScroll = useCallback(() => {
    const el = scrollRef.current;
    if (!el) return;
    const atBottom =
      el.scrollHeight - el.scrollTop - el.clientHeight < 24;
    setAutoScroll(atBottom);
  }, []);

  async function handleCopy() {
    const text = shown.map(formatLine).join("\n");
    try {
      await navigator.clipboard.writeText(text);
    } catch (e) {
      console.error("clipboard write failed", e);
    }
  }

  async function handleExport() {
    setBusy(true);
    setExportedPath(null);
    try {
      const path = await ipc.exportLogs();
      setExportedPath(path);
    } catch (e) {
      console.error("export_logs failed", e);
      setExportedPath(`export failed: ${String(e)}`);
    } finally {
      setBusy(false);
    }
  }

  async function handleClear() {
    try {
      await ipc.clearLogs();
    } catch (e) {
      console.error("clear_logs failed", e);
    }
    setLines([]);
    setExportedPath(null);
  }

  return (
    <section className="panel log-panel">
      <div className="log-head">
        <h2>
          Logs{" "}
          <span className="muted">
            ({shown.length}
            {shown.length !== lines.length ? ` of ${lines.length}` : ""})
          </span>
        </h2>
        <button onClick={() => setCollapsed((c) => !c)}>
          {collapsed ? "Show" : "Hide"}
        </button>
      </div>

      {!collapsed && (
        <>
          <div className="row log-controls">
            <select
              value={levelFilter}
              onChange={(e) => setLevelFilter(e.target.value as LevelFilter)}
              title="Minimum level to show"
            >
              <option value="TRACE">Trace+</option>
              <option value="DEBUG">Debug+</option>
              <option value="INFO">Info+</option>
              <option value="WARN">Warn+</option>
            </select>
            <input
              placeholder="Filter text…"
              value={textFilter}
              onChange={(e) => setTextFilter(e.target.value)}
            />
            <button onClick={handleCopy} disabled={shown.length === 0}>
              Copy
            </button>
            <button onClick={handleExport} disabled={busy}>
              {busy ? "Exporting…" : "Export"}
            </button>
            <button onClick={handleClear}>Clear</button>
          </div>

          {exportedPath && (
            <p className="muted log-exported">
              Saved to <code>{exportedPath}</code>{" "}
              {!exportedPath.startsWith("export failed") && (
                <button
                  className="link-btn"
                  onClick={() => ipc.revealPath(exportedPath).catch(console.error)}
                >
                  Reveal
                </button>
              )}
            </p>
          )}

          <div className="log-scroll" ref={scrollRef} onScroll={onScroll}>
            {shown.length === 0 ? (
              <p className="muted">No log lines match.</p>
            ) : (
              shown.map((l) => (
                <div
                  key={l.seq}
                  className={`log-line log-${l.level.toLowerCase()}`}
                >
                  {formatLine(l)}
                </div>
              ))
            )}
          </div>

          {!autoScroll && (
            <button
              className="log-jump"
              onClick={() => {
                setAutoScroll(true);
                const el = scrollRef.current;
                if (el) el.scrollTop = el.scrollHeight;
              }}
            >
              ↓ Jump to latest
            </button>
          )}
        </>
      )}
    </section>
  );
}
