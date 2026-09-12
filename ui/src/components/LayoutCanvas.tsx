import { useMemo, useRef, useState } from "react";
import type { Display, EdgeSettings, Rect } from "../lib/types";

interface Props {
  localName: string;
  /** This machine's individual monitors, in local virtual-desktop coords. */
  localDisplays: Display[];
  /** This machine's whole virtual desktop (fallback if `localDisplays` is empty). */
  localBounds: Rect;
  peerName: string | null;
  /** The peer's individual monitors, in the PEER's own coords. `null` until
   *  its `ScreenConfig` has arrived. */
  peerDisplays: Display[] | null;
  /** The peer's whole virtual desktop, in the PEER's own coords. */
  peerVirtualBounds: Rect | null;
  /** Where the peer's virtual desktop sits in OUR coords (from `LayoutChanged`). */
  peerBounds: Rect | null;
  /** Called with the peer's new whole-desktop bounds, in our coords. */
  onPeerBoundsChange: (bounds: Rect) => void;
  /** `null` until config has loaded. */
  edgeSettings: EdgeSettings | null;
  onEdgeSettingsChange: (settings: EdgeSettings) => void;
}

const CANVAS_WIDTH = 700;
const CANVAS_HEIGHT = 400;
const CANVAS_PADDING = 44;

interface Placed {
  rect: Rect;
  label: string;
  isPrimary: boolean;
}

function unionOf(rects: Rect[]): Rect | null {
  if (rects.length === 0) return null;
  let minX = Infinity;
  let minY = Infinity;
  let maxX = -Infinity;
  let maxY = -Infinity;
  for (const r of rects) {
    minX = Math.min(minX, r.x);
    minY = Math.min(minY, r.y);
    maxX = Math.max(maxX, r.x + r.width);
    maxY = Math.max(maxY, r.y + r.height);
  }
  return { x: minX, y: minY, width: maxX - minX, height: maxY - minY };
}

function translated(r: Rect, dx: number, dy: number): Rect {
  return { x: r.x + dx, y: r.y + dy, width: r.width, height: r.height };
}

/** Do `a` and `b` overlap along the axis perpendicular to a vertical shared
 *  edge (i.e. do their y-ranges overlap)? */
function overlapsY(a: Rect, b: Rect): boolean {
  return a.y < b.y + b.height && b.y < a.y + a.height;
}
function overlapsX(a: Rect, b: Rect): boolean {
  return a.x < b.x + b.width && b.x < a.x + a.width;
}

/** The single-axis translation that would snap the peer group flush against
 *  a local monitor, if one is within `threshold` real px. */
function bestSnap(peers: Rect[], locals: Rect[], threshold: number): { dx: number; dy: number } {
  let best: { dist: number; dx: number; dy: number } = { dist: threshold, dx: 0, dy: 0 };
  for (const p of peers) {
    for (const l of locals) {
      // Horizontal: peer's left to local's right, peer's right to local's left.
      if (overlapsY(p, l)) {
        for (const d of [
          l.x + l.width - p.x, // p.left -> l.right
          l.x - (p.x + p.width), // p.right -> l.left
        ]) {
          if (Math.abs(d) < best.dist) best = { dist: Math.abs(d), dx: d, dy: 0 };
        }
      }
      // Vertical: peer's top to local's bottom, peer's bottom to local's top.
      if (overlapsX(p, l)) {
        for (const d of [
          l.y + l.height - p.y,
          l.y - (p.y + p.height),
        ]) {
          if (Math.abs(d) < best.dist) best = { dist: Math.abs(d), dx: 0, dy: d };
        }
      }
    }
  }
  return { dx: best.dx, dy: best.dy };
}

/** Indices of the (localRect, peerRect) pair that form the handoff seam —
 *  flush within 1px with a real shared span — or null. Mirrors
 *  `seam_core::topology::resolve_seams` closely enough for a visual hint. */
function seamPair(peers: Rect[], locals: Rect[]): { local: number; peer: number } | null {
  for (let li = 0; li < locals.length; li++) {
    const l = locals[li];
    // Skip an edge that has another local monitor beyond it — interior.
    const interiorRight = locals.some((o) => o !== l && o.x + o.width > l.x + l.width && overlapsY(l, o));
    const interiorLeft = locals.some((o) => o !== l && o.x < l.x && overlapsY(l, o));
    const interiorBelow = locals.some((o) => o !== l && o.y + o.height > l.y + l.height && overlapsX(l, o));
    const interiorAbove = locals.some((o) => o !== l && o.y < l.y && overlapsX(l, o));
    for (let pi = 0; pi < peers.length; pi++) {
      const p = peers[pi];
      const flushRight = Math.abs(p.x - (l.x + l.width)) <= 1 && overlapsY(l, p);
      const flushLeft = Math.abs(p.x + p.width - l.x) <= 1 && overlapsY(l, p);
      const flushBelow = Math.abs(p.y - (l.y + l.height)) <= 1 && overlapsX(l, p);
      const flushAbove = Math.abs(p.y + p.height - l.y) <= 1 && overlapsX(l, p);
      if (
        (flushRight && !interiorRight) ||
        (flushLeft && !interiorLeft) ||
        (flushBelow && !interiorBelow) ||
        (flushAbove && !interiorAbove)
      ) {
        return { local: li, peer: pi };
      }
    }
  }
  return null;
}

export function LayoutCanvas({
  localName,
  localDisplays,
  localBounds,
  peerName,
  peerDisplays,
  peerVirtualBounds,
  peerBounds,
  onPeerBoundsChange,
  edgeSettings,
  onEdgeSettingsChange,
}: Props) {
  const [dragOffset, setDragOffset] = useState<{ dx: number; dy: number } | null>(null);
  const dragState = useRef<{ startX: number; startY: number } | null>(null);

  const localRects: Rect[] = useMemo(
    () => (localDisplays.length > 0 ? localDisplays.map((d) => d.bounds) : [localBounds]),
    [localDisplays, localBounds],
  );

  // Peer monitors, expressed in OUR coordinate space: shift each of the
  // peer's own-coord display rects by (where its desktop sits here) minus
  // (its desktop origin in its own coords). Same translation the backend
  // does in Session::refresh_peer_display_seams.
  const peerRectsBase: Rect[] | null = useMemo(() => {
    if (peerDisplays && peerDisplays.length > 0 && peerVirtualBounds && peerBounds) {
      const dx = peerBounds.x - peerVirtualBounds.x;
      const dy = peerBounds.y - peerVirtualBounds.y;
      return peerDisplays.map((d) => translated(d.bounds, dx, dy));
    }
    if (peerBounds) return [peerBounds];
    return null;
  }, [peerDisplays, peerVirtualBounds, peerBounds]);

  const localPlaced: Placed[] = useMemo(
    () =>
      localRects.map((rect, i) => ({
        rect,
        label: `${rect.width}×${rect.height}`,
        isPrimary: localDisplays[i]?.is_primary ?? false,
      })),
    [localRects, localDisplays],
  );

  const scale = useMemo(() => {
    const all = [...localRects, ...(peerRectsBase ?? [])];
    const u = unionOf(all) ?? localBounds;
    const availableW = CANVAS_WIDTH - CANVAS_PADDING * 2;
    const availableH = CANVAS_HEIGHT - CANVAS_PADDING * 2;
    const s = Math.min(availableW / u.width, availableH / u.height);
    return Number.isFinite(s) && s > 0 ? Math.min(s, 0.25) : 0.05;
  }, [localRects, peerRectsBase, localBounds]);

  // Effective (possibly dragged + snapped) peer rects.
  const peerRects: Rect[] | null = useMemo(() => {
    if (!peerRectsBase) return null;
    if (!dragOffset) return peerRectsBase;
    const moved = peerRectsBase.map((r) => translated(r, dragOffset.dx, dragOffset.dy));
    const snap = bestSnap(moved, localRects, 16 / scale);
    return moved.map((r) => translated(r, snap.dx, snap.dy));
  }, [peerRectsBase, dragOffset, localRects, scale]);

  const seam = useMemo(
    () => (peerRects ? seamPair(peerRects, localRects) : null),
    [peerRects, localRects],
  );

  const origin = useMemo(() => {
    const u = unionOf([...localRects, ...(peerRects ?? [])]) ?? localBounds;
    return { x: u.x, y: u.y };
  }, [localRects, peerRects, localBounds]);

  const toScreen = (r: Rect) => ({
    left: CANVAS_PADDING + (r.x - origin.x) * scale,
    top: CANVAS_PADDING + (r.y - origin.y) * scale,
    width: Math.max(r.width * scale, 6),
    height: Math.max(r.height * scale, 6),
  });

  function onPointerDown(e: React.PointerEvent) {
    if (!peerRectsBase) return;
    (e.target as Element).setPointerCapture(e.pointerId);
    dragState.current = { startX: e.clientX, startY: e.clientY };
    setDragOffset({ dx: 0, dy: 0 });
  }
  function onPointerMove(e: React.PointerEvent) {
    if (!dragState.current) return;
    setDragOffset({
      dx: (e.clientX - dragState.current.startX) / scale,
      dy: (e.clientY - dragState.current.startY) / scale,
    });
  }
  function onPointerUp() {
    if (!dragState.current || !peerRects || !peerRectsBase) {
      dragState.current = null;
      setDragOffset(null);
      return;
    }
    dragState.current = null;
    setDragOffset(null);
    // Round the whole-group TRANSLATION to an integer, then re-derive every
    // peer rect (and the union sent below) from that — not the other way
    // around. `peerRects` already carries a rigid, uniform shift off
    // `peerRectsBase` (drag delta + `bestSnap`'s correction), so any index
    // recovers the same (fractional) dx/dy. Rounding the DERIVED union
    // instead used to be able to snap the wrong display in a multi-monitor
    // peer group: whichever member happens to define the union's corner
    // gets the rounding, not necessarily the one `bestSnap` actually
    // aligned flush, which could leave the real seam a fraction of a pixel
    // off (see `SEAM_CONTACT_SLACK_PX`'s doc comment on the Rust side).
    const dx = Math.round(peerRects[0].x - peerRectsBase[0].x);
    const dy = Math.round(peerRects[0].y - peerRectsBase[0].y);
    const snapped = peerRectsBase.map((r) => translated(r, dx, dy));
    const u = unionOf(snapped);
    if (u) onPeerBoundsChange(u);
  }

  return (
    <section className="panel">
      <h2>Layout</h2>
      <p className="muted">
        {peerRectsBase
          ? "Drag the peer's monitors to match your real desk — the group snaps when a monitor edge meets one of yours. The accent edge is the handoff seam."
          : "Waiting for the peer's screen info…"}
      </p>
      <div className="layout-canvas" style={{ width: CANVAS_WIDTH, height: CANVAS_HEIGHT }}>
        {localPlaced.map((d, i) => {
          const s = toScreen(d.rect);
          return (
            <div
              key={`l${i}`}
              className={`tile local-tile${seam?.local === i ? " seam-tile" : ""}`}
              style={{ left: s.left, top: s.top, width: s.width, height: s.height }}
            >
              <span>{i === 0 ? localName : ""}</span>
              <span className="tile-res">
                {d.label}
                {d.isPrimary ? " ●" : ""}
              </span>
            </div>
          );
        })}
        {peerRects?.map((r, i) => {
          const s = toScreen(r);
          return (
            <div
              key={`p${i}`}
              className={`tile peer-tile${seam?.peer === i ? " seam-tile" : ""}`}
              style={{ left: s.left, top: s.top, width: s.width, height: s.height }}
              onPointerDown={onPointerDown}
              onPointerMove={onPointerMove}
              onPointerUp={onPointerUp}
            >
              <span>{i === 0 ? (peerName ?? "Peer") : ""}</span>
              <span className="tile-res">
                {r.width}×{r.height}
                {(peerDisplays?.[i]?.is_primary ?? false) ? " ●" : ""}
              </span>
            </div>
          );
        })}
      </div>

      {edgeSettings && (
        <div className="edge-settings">
          <label className="field">
            Corner dead zone (px)
            <input
              type="number"
              min={0}
              max={200}
              value={edgeSettings.corner_dead_zone_px}
              onChange={(e) =>
                onEdgeSettingsChange({
                  ...edgeSettings,
                  corner_dead_zone_px: Math.max(0, Number(e.target.value) || 0),
                })
              }
            />
          </label>
          <label className="field">
            Handoff cooldown (ms)
            <input
              type="number"
              min={0}
              max={2000}
              step={50}
              value={edgeSettings.handoff_cooldown_ms}
              onChange={(e) =>
                onEdgeSettingsChange({
                  ...edgeSettings,
                  handoff_cooldown_ms: Math.max(0, Number(e.target.value) || 0),
                })
              }
            />
          </label>
        </div>
      )}
    </section>
  );
}
