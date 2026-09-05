import { useMemo, useRef, useState } from "react";
import type { Rect } from "../lib/types";

interface Props {
  localName: string;
  localBounds: Rect;
  peerName: string | null;
  /** `null` until the peer's `ScreenConfig` has arrived. */
  peerBounds: Rect | null;
  onPeerBoundsChange: (bounds: Rect) => void;
}

const CANVAS_WIDTH = 640;
const CANVAS_HEIGHT = 380;
const CANVAS_PADDING = 40;

function unionBounds(a: Rect, b: Rect | null): Rect {
  if (!b) return a;
  const x = Math.min(a.x, b.x);
  const y = Math.min(a.y, b.y);
  const right = Math.max(a.x + a.width, b.x + b.width);
  const bottom = Math.max(a.y + a.height, b.y + b.height);
  return { x, y, width: right - x, height: bottom - y };
}

/** Snaps `dragged` to touch whichever edge of `anchor` it's nearest to,
 * once it's within `thresholdPx` (in the SAME real-pixel space as both
 * rects) of doing so — Tier 8.1's "snap-to-edge when dragged near
 * another tile." Falls back to the un-snapped position otherwise. */
function snapToEdge(dragged: Rect, anchor: Rect, thresholdPx: number): Rect {
  const candidates: { rect: Rect; distance: number }[] = [
    {
      rect: { ...dragged, x: anchor.x + anchor.width, y: dragged.y },
      distance: Math.abs(dragged.x - (anchor.x + anchor.width)),
    },
    {
      rect: { ...dragged, x: anchor.x - dragged.width, y: dragged.y },
      distance: Math.abs(dragged.x + dragged.width - anchor.x),
    },
    {
      rect: { ...dragged, x: dragged.x, y: anchor.y + anchor.height },
      distance: Math.abs(dragged.y - (anchor.y + anchor.height)),
    },
    {
      rect: { ...dragged, x: dragged.x, y: anchor.y - dragged.height },
      distance: Math.abs(dragged.y + dragged.height - anchor.y),
    },
  ];
  const best = candidates.reduce((a, b) => (a.distance < b.distance ? a : b));
  return best.distance <= thresholdPx ? best.rect : dragged;
}

export function LayoutCanvas({
  localName,
  localBounds,
  peerName,
  peerBounds,
  onPeerBoundsChange,
}: Props) {
  const containerRef = useRef<HTMLDivElement | null>(null);
  const [dragPreview, setDragPreview] = useState<Rect | null>(null);
  const dragState = useRef<{
    startClientX: number;
    startClientY: number;
    startBounds: Rect;
  } | null>(null);

  const effectivePeerBounds = dragPreview ?? peerBounds;
  const union = useMemo(
    () => unionBounds(localBounds, effectivePeerBounds),
    [localBounds, effectivePeerBounds],
  );
  const scale = useMemo(() => {
    const availableW = CANVAS_WIDTH - CANVAS_PADDING * 2;
    const availableH = CANVAS_HEIGHT - CANVAS_PADDING * 2;
    const s = Math.min(availableW / union.width, availableH / union.height);
    return Number.isFinite(s) && s > 0 ? Math.min(s, 0.25) : 0.05;
  }, [union]);

  const originX = union.x;
  const originY = union.y;
  const toScreen = (r: Rect) => ({
    left: CANVAS_PADDING + (r.x - originX) * scale,
    top: CANVAS_PADDING + (r.y - originY) * scale,
    width: Math.max(r.width * scale, 4),
    height: Math.max(r.height * scale, 4),
  });

  function handlePointerDown(e: React.PointerEvent) {
    if (!peerBounds) return;
    (e.target as Element).setPointerCapture(e.pointerId);
    dragState.current = {
      startClientX: e.clientX,
      startClientY: e.clientY,
      startBounds: peerBounds,
    };
    setDragPreview(peerBounds);
  }

  function handlePointerMove(e: React.PointerEvent) {
    if (!dragState.current) return;
    const dxScreen = e.clientX - dragState.current.startClientX;
    const dyScreen = e.clientY - dragState.current.startClientY;
    const dxReal = dxScreen / scale;
    const dyReal = dyScreen / scale;
    const raw: Rect = {
      ...dragState.current.startBounds,
      x: dragState.current.startBounds.x + dxReal,
      y: dragState.current.startBounds.y + dyReal,
    };
    // Snap threshold: ~14 screen px worth of real distance, so it feels
    // consistent regardless of current zoom/scale.
    setDragPreview(snapToEdge(raw, localBounds, 14 / scale));
  }

  function handlePointerUp() {
    if (!dragState.current || !dragPreview) {
      dragState.current = null;
      return;
    }
    dragState.current = null;
    const final: Rect = {
      x: Math.round(dragPreview.x),
      y: Math.round(dragPreview.y),
      width: dragPreview.width,
      height: dragPreview.height,
    };
    setDragPreview(null);
    onPeerBoundsChange(final);
  }

  const localScreen = toScreen(localBounds);
  const peerScreen = effectivePeerBounds ? toScreen(effectivePeerBounds) : null;

  return (
    <section className="panel">
      <h2>Layout</h2>
      <p className="muted">
        {peerBounds
          ? "Drag the peer's tile to match your real desk setup — it snaps to touch an edge."
          : "Waiting for the peer's screen info..."}
      </p>
      <div
        ref={containerRef}
        className="layout-canvas"
        style={{ width: CANVAS_WIDTH, height: CANVAS_HEIGHT }}
      >
        <div
          className="tile local-tile"
          style={{
            left: localScreen.left,
            top: localScreen.top,
            width: localScreen.width,
            height: localScreen.height,
          }}
        >
          {localName}
          <span className="tile-res">
            {localBounds.width}×{localBounds.height}
          </span>
        </div>
        {peerScreen && (
          <div
            className="tile peer-tile"
            style={{
              left: peerScreen.left,
              top: peerScreen.top,
              width: peerScreen.width,
              height: peerScreen.height,
            }}
            onPointerDown={handlePointerDown}
            onPointerMove={handlePointerMove}
            onPointerUp={handlePointerUp}
          >
            {peerName ?? "Peer"}
            {effectivePeerBounds && (
              <span className="tile-res">
                {effectivePeerBounds.width}×{effectivePeerBounds.height}
              </span>
            )}
          </div>
        )}
      </div>
    </section>
  );
}
