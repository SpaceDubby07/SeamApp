//! Screen layout: displays, the node-placement graph, and edge-crossing
//! math (Tier 7.2 of the build guide).
//!
//! Deliberately a *graph* of node rectangles keyed by [`NodeId`], not a
//! hardcoded left/right pair — Tier 15's design note for keeping a third
//! machine cheap to add later.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Stable identity for one node (machine), generated once on first run and
/// persisted in config. A UUID rather than e.g. a hostname or an index, so
/// it survives renames and never collides across machines (Tier 15).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NodeId(pub Uuid);

impl NodeId {
    /// Generates a new, random node identity.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for NodeId {
    fn default() -> Self {
        Self::new()
    }
}

/// A point in local virtual-desktop pixel coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Point {
    /// X coordinate in pixels.
    pub x: i32,
    /// Y coordinate in pixels.
    pub y: i32,
}

/// An axis-aligned rectangle in local virtual-desktop pixel coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rect {
    /// X coordinate of the top-left corner.
    pub x: i32,
    /// Y coordinate of the top-left corner.
    pub y: i32,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

impl Rect {
    /// X coordinate of the right edge (inclusive — the last pixel column
    /// still inside the rectangle).
    #[must_use]
    fn right(self) -> i32 {
        self.x + self.width.cast_signed() - 1
    }

    /// Y coordinate of the bottom edge (inclusive), matching `right`.
    #[must_use]
    fn bottom(self) -> i32 {
        self.y + self.height.cast_signed() - 1
    }
}

/// Stable identifier for one physical display, scoped to the local machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DisplayId(pub u32);

/// One physical display attached to the local machine.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Display {
    /// Identifier for this display, stable for the current session.
    pub id: DisplayId,
    /// Bounds of this display within the local virtual desktop.
    pub bounds: Rect,
    /// DPI scale factor (e.g. `2.0` on a Retina display).
    pub scale_factor: f64,
    /// Whether this is the OS-designated primary display.
    pub is_primary: bool,
}

/// The bounding box of every display's `bounds` — i.e. the whole virtual
/// desktop. Displays are typically packed with no gaps, but this makes no
/// such assumption; it just takes the outer extent.
///
/// Shared by every platform's `ScreenInfo::virtual_bounds()` — this is
/// pure geometry with no OS dependency, so each platform's `screens.rs`
/// calls this rather than reimplementing it.
#[must_use]
pub fn union_of_display_bounds(displays: &[Display]) -> Rect {
    let Some(first) = displays.first() else {
        return Rect {
            x: 0,
            y: 0,
            width: 0,
            height: 0,
        };
    };

    let mut min_x = first.bounds.x;
    let mut min_y = first.bounds.y;
    let mut max_x = first.bounds.x + first.bounds.width.cast_signed();
    let mut max_y = first.bounds.y + first.bounds.height.cast_signed();

    for d in &displays[1..] {
        min_x = min_x.min(d.bounds.x);
        min_y = min_y.min(d.bounds.y);
        max_x = max_x.max(d.bounds.x + d.bounds.width.cast_signed());
        max_y = max_y.max(d.bounds.y + d.bounds.height.cast_signed());
    }

    Rect {
        x: min_x,
        y: min_y,
        width: (max_x - min_x).cast_unsigned(),
        height: (max_y - min_y).cast_unsigned(),
    }
}

/// One of the four sides of a rectangle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Edge {
    /// The top edge.
    Top,
    /// The right edge.
    Right,
    /// The bottom edge.
    Bottom,
    /// The left edge.
    Left,
}

impl Edge {
    /// The edge on the far side of a shared boundary: crossing out through
    /// your `Right` edge means entering the neighbor through its `Left`.
    #[must_use]
    pub fn opposite(self) -> Self {
        match self {
            Self::Top => Self::Bottom,
            Self::Bottom => Self::Top,
            Self::Left => Self::Right,
            Self::Right => Self::Left,
        }
    }
}

/// A point along one edge of a machine's screen, normalized `0.0..=1.0`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct EdgePoint {
    /// Which edge of the receiver this point is on.
    pub edge: Edge,
    /// Position along that edge, `0.0` at the top/left end.
    pub pos: f32,
}

/// Converts a cursor position on the LOCAL virtual desktop into a
/// normalized entry point on the REMOTE machine's shared edge.
///
/// Normalization is the whole trick: we never send pixels. A cursor at 60%
/// down the right edge of a 1440p Mac enters at 60% down the left edge of a
/// 1080p Windows box, regardless of DPI or resolution differences.
///
/// The `f32` conversions below are lossy in principle (`clippy::pedantic`
/// flags them) but not in any way that matters here: `f32`'s 23-bit
/// mantissa represents pixel coordinates exactly up to ~16.7 million, far
/// beyond any real display.
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn compute_entry_point(local_bounds: Rect, cursor: Point, edge: Edge) -> EdgePoint {
    let pos = match edge {
        Edge::Right | Edge::Left => (cursor.y - local_bounds.y) as f32 / local_bounds.height as f32,
        Edge::Top | Edge::Bottom => (cursor.x - local_bounds.x) as f32 / local_bounds.width as f32,
    };
    EdgePoint {
        edge: edge.opposite(),
        pos: pos.clamp(0.0, 1.0),
    }
}

/// Returns the edge `cur` is pressed against and still moving into, if any
/// — the outward handoff trigger (Tier 7.2).
///
/// Two things worth knowing if you're changing this:
/// - The OS clamps cursor coordinates to the screen bounds, so once a user
///   pushes into an edge, repeated samples report the *same* clamped
///   position rather than going negative. Comparing with `<=`/`>=` (not
///   strict `<`/`>`) against `prev` is what makes a cursor pinned against
///   an edge still count as "moving that way" on every subsequent sample,
///   not just the first one that reached it.
/// - `dead_zone_px` excludes the corners specifically so that clicking a
///   corner UI element doesn't trigger an accidental handoff — it does
///   *not* replace the 200ms post-handoff cooldown, which lives in
///   `state.rs` since it needs wall-clock time, not just geometry.
#[must_use]
pub fn detect_edge_crossing(
    bounds: Rect,
    prev: Point,
    cur: Point,
    dead_zone_px: u32,
) -> Option<Edge> {
    let dead_zone = dead_zone_px.cast_signed();
    let dist_from_left = cur.x - bounds.x;
    let dist_from_right = bounds.right() - cur.x;
    let dist_from_top = cur.y - bounds.y;
    let dist_from_bottom = bounds.bottom() - cur.y;

    let in_corner = dist_from_left.min(dist_from_right) < dead_zone
        && dist_from_top.min(dist_from_bottom) < dead_zone;
    if in_corner {
        return None;
    }

    if cur.x <= bounds.x && cur.x <= prev.x {
        Some(Edge::Left)
    } else if cur.x >= bounds.right() && cur.x >= prev.x {
        Some(Edge::Right)
    } else if cur.y <= bounds.y && cur.y <= prev.y {
        Some(Edge::Top)
    } else if cur.y >= bounds.bottom() && cur.y >= prev.y {
        Some(Edge::Bottom)
    } else {
        None
    }
}

/// Returns `true` once `cur` has moved measurably back toward the interior
/// of `bounds` from `edge` — the reclaim trigger while driving a peer
/// (Tier 7.1's `RemoteActive` → `LocalActive` transition).
///
/// This takes the straightforward reading of "the local cursor moved back
/// inward": while `RemoteActive`, local capture keeps running (only the
/// OS-visible cursor is suppressed), and this checks whatever position it
/// reports. Exactly how Windows/macOS report cursor position while
/// suppressed needs verification against real hardware — tracked for M4,
/// the first real cross-machine handoff.
#[must_use]
pub fn detect_edge_reclaim(bounds: Rect, edge: Edge, cur: Point, threshold_px: i32) -> bool {
    match edge {
        Edge::Left => cur.x > bounds.x + threshold_px,
        Edge::Right => cur.x < bounds.right() - threshold_px,
        Edge::Top => cur.y > bounds.y + threshold_px,
        Edge::Bottom => cur.y < bounds.bottom() - threshold_px,
    }
}

/// Slack, in pixels, still allowed between two display edges before they
/// stop counting as "touching" for seam resolution. Real multi-monitor
/// arrangements pack flush and the layout canvas snaps to exact contact, so
/// this is only insurance against a 1px inclusive/exclusive rounding slip —
/// not a reason to treat a deliberately-separated pair as adjacent.
const SEAM_CONTACT_SLACK_PX: i32 = 1;

/// One resolved handoff boundary: an outer edge of a LOCAL display that a
/// PEER display sits flush against.
///
/// This — not the whole virtual desktop — is what edge detection and
/// entry-point normalization work against once either machine has more than
/// one monitor. A cursor leaving 80% of the way down a 1080-tall side
/// monitor has to enter the peer 80% of the way down *that monitor's* edge,
/// not 80% of the way down a union rectangle that's 2192px tall because
/// some other monitor is bigger.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Seam {
    /// Bounds of the local display whose edge forms this boundary.
    pub local_display: Rect,
    /// Which edge of `local_display` the peer is across.
    pub edge: Edge,
    /// Bounds of the peer display on the far side, in LOCAL coordinate
    /// space (already translated out of the peer's own coordinates).
    pub peer_display: Rect,
    /// Length, in pixels, of the span the two edges actually share,
    /// measured perpendicular to `edge`. When several seams exist a caller
    /// wanting a single "the seam" takes the largest.
    pub overlap_px: u32,
}

/// Whether `other` occupies screen space beyond `r`'s `edge` (overlapping
/// it along the edge) — i.e. that edge is an *interior* boundary of the
/// local virtual desktop, one a cursor just slides across to the next
/// monitor, not an outer edge it can cross to another machine.
fn lies_beyond(r: Rect, edge: Edge, other: Rect) -> bool {
    match edge {
        Edge::Right => other.right() > r.right() && vertically_overlaps(r, other),
        Edge::Left => other.x < r.x && vertically_overlaps(r, other),
        Edge::Bottom => other.bottom() > r.bottom() && horizontally_overlaps(r, other),
        Edge::Top => other.y < r.y && horizontally_overlaps(r, other),
    }
}

/// Pixels of shared extent between `a` and `b` along the vertical axis
/// (`0` if they don't overlap).
fn vertical_overlap_px(a: Rect, b: Rect) -> u32 {
    (a.bottom().min(b.bottom()) - a.y.max(b.y) + 1)
        .max(0)
        .cast_unsigned()
}

/// Pixels of shared extent between `a` and `b` along the horizontal axis
/// (`0` if they don't overlap).
fn horizontal_overlap_px(a: Rect, b: Rect) -> u32 {
    (a.right().min(b.right()) - a.x.max(b.x) + 1)
        .max(0)
        .cast_unsigned()
}

/// If peer display `p` sits flush against `r`'s `edge` (within
/// [`SEAM_CONTACT_SLACK_PX`]) sharing a non-empty span, the length of that
/// span in pixels.
fn edge_contact(r: Rect, edge: Edge, p: Rect) -> Option<u32> {
    let (gap, overlap) = match edge {
        Edge::Right => (p.x - (r.right() + 1), vertical_overlap_px(r, p)),
        Edge::Left => ((p.right() + 1) - r.x, vertical_overlap_px(r, p)),
        Edge::Bottom => (p.y - (r.bottom() + 1), horizontal_overlap_px(r, p)),
        Edge::Top => ((p.bottom() + 1) - r.y, horizontal_overlap_px(r, p)),
    };
    (gap.abs() <= SEAM_CONTACT_SLACK_PX && overlap > 0).then_some(overlap)
}

/// Whether `p` lies within `r`'s extent along the axis *parallel* to
/// `edge`, ignoring the perpendicular one — so a cursor level with a
/// different monitor can't match a seam it merely shares a coordinate with.
fn within_edge_span(r: Rect, edge: Edge, p: Point) -> bool {
    match edge {
        Edge::Left | Edge::Right => p.y >= r.y && p.y <= r.bottom(),
        Edge::Top | Edge::Bottom => p.x >= r.x && p.x <= r.right(),
    }
}

/// Resolves every handoff boundary between this machine's `local` display
/// rectangles and the peer's (`peer_in_local_space` — the peer's displays
/// already translated into this machine's coordinate space using the
/// origin offset from `ControlMessage::LayoutUpdate`).
///
/// An edge qualifies only if it's on the *outer* hull of the local virtual
/// desktop (no other local display beyond it) AND a peer display sits flush
/// against it. That one rule is what makes a 3-monitor machine behave: the
/// edges between adjacent local monitors produce no seam, the outer edges
/// with nothing beyond them produce no seam, and only the monitor the other
/// machine is actually placed against becomes a handoff surface.
///
/// Sorted by `overlap_px` descending, so `.first()` is the most
/// substantial boundary when a caller wants just one.
#[must_use]
pub fn resolve_seams(local: &[Rect], peer_in_local_space: &[Rect]) -> Vec<Seam> {
    let mut seams = Vec::new();
    for (i, &l) in local.iter().enumerate() {
        for edge in [Edge::Top, Edge::Right, Edge::Bottom, Edge::Left] {
            let interior = local
                .iter()
                .enumerate()
                .any(|(j, &o)| j != i && lies_beyond(l, edge, o));
            if interior {
                continue;
            }
            for &peer_display in peer_in_local_space {
                if let Some(overlap_px) = edge_contact(l, edge, peer_display) {
                    seams.push(Seam {
                        local_display: l,
                        edge,
                        peer_display,
                        overlap_px,
                    });
                }
            }
        }
    }
    seams.sort_by_key(|s| core::cmp::Reverse(s.overlap_px));
    seams
}

/// The seam whose edge the cursor is crossing right now — within a pixel of
/// it and still moving outward — or `None`.
///
/// The multi-monitor replacement for running [`detect_edge_crossing`]
/// against the whole virtual desktop: it also requires the cursor to be
/// within the seam display's own extent along the edge, so pushing off the
/// top of a left-hand monitor isn't mistaken for crossing a right-hand
/// seam that merely shares an x coordinate.
#[must_use]
pub fn detect_seam_crossing(
    seams: &[Seam],
    prev: Point,
    cur: Point,
    dead_zone_px: u32,
) -> Option<&Seam> {
    seams.iter().find(|s| {
        within_edge_span(s.local_display, s.edge, cur)
            && detect_edge_crossing(s.local_display, prev, cur, dead_zone_px) == Some(s.edge)
    })
}

/// The shared layout: where each node's screen sits on an abstract canvas
/// (Tier 8.1's drag-and-snap tiles), used to answer "who's on the other
/// side of this edge?"
#[derive(Debug, Clone, Default)]
pub struct Layout {
    placements: HashMap<NodeId, Rect>,
}

impl Layout {
    /// Creates an empty layout with no nodes placed.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Places (or moves) `node` to `bounds` on the shared canvas.
    pub fn set_placement(&mut self, node: NodeId, bounds: Rect) {
        self.placements.insert(node, bounds);
    }

    /// The placed bounds of `node`, if it's been placed.
    #[must_use]
    pub fn bounds_of(&self, node: NodeId) -> Option<Rect> {
        self.placements.get(&node).copied()
    }

    /// The node sharing `edge` of `node`, if any — whichever other placed
    /// rectangle touches that full side. Touching only at a corner doesn't
    /// count as adjacent.
    #[must_use]
    pub fn neighbor(&self, node: NodeId, edge: Edge) -> Option<NodeId> {
        let bounds = self.bounds_of(node)?;
        self.placements
            .iter()
            .find(|&(&id, &other)| id != node && is_adjacent(bounds, other, edge))
            .map(|(&id, _)| id)
    }
}

fn is_adjacent(a: Rect, b: Rect, edge: Edge) -> bool {
    match edge {
        Edge::Right => a.right() + 1 == b.x && vertically_overlaps(a, b),
        Edge::Left => b.right() + 1 == a.x && vertically_overlaps(a, b),
        Edge::Bottom => a.bottom() + 1 == b.y && horizontally_overlaps(a, b),
        Edge::Top => b.bottom() + 1 == a.y && horizontally_overlaps(a, b),
    }
}

fn vertically_overlaps(a: Rect, b: Rect) -> bool {
    a.y <= b.bottom() && b.y <= a.bottom()
}

fn horizontally_overlaps(a: Rect, b: Rect) -> bool {
    a.x <= b.right() && b.x <= a.right()
}

#[cfg(test)]
// Test fixtures use small literal pixel values well within f32's exact
// range; the precision/truncation/wrap lints that matter for arbitrary
// production input aren't meaningful noise to fix here.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap
)]
mod tests {
    use super::{
        Display, DisplayId, Edge, Layout, NodeId, Point, Rect, compute_entry_point,
        detect_edge_crossing, detect_edge_reclaim, detect_seam_crossing, resolve_seams,
        union_of_display_bounds,
    };

    fn rect(x: i32, y: i32, width: u32, height: u32) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn entry_point_scales_across_mismatched_resolutions() {
        // 60% down a 1440p right edge -> 60% down the neighbor's left edge.
        let ep = compute_entry_point(
            rect(0, 0, 2560, 1440),
            Point { x: 2559, y: 864 }, // 864 / 1440 = 0.6
            Edge::Right,
        );
        assert_eq!(ep.edge, Edge::Left);
        assert!((ep.pos - 0.6).abs() < 0.001);
    }

    #[test]
    fn entry_point_clamps_to_valid_range() {
        // A cursor at the very first row on a Top-edge crossing should
        // never produce a negative or >1.0 normalized position.
        let ep = compute_entry_point(rect(0, 0, 1000, 1000), Point { x: 0, y: 0 }, Edge::Top);
        assert!((0.0..=1.0).contains(&ep.pos));
    }

    #[test]
    fn edge_point_roundtrips_across_many_screen_sizes() {
        for (w1, h1, w2, h2, y) in [
            (1920u32, 1080u32, 2560u32, 1440u32, 500i32),
            (800, 600, 3840, 2160, 0),
            (3840, 2160, 800, 600, 2159),
            (1280, 720, 1280, 720, 360),
        ] {
            let bounds1 = rect(0, 0, w1, h1);
            let entry = compute_entry_point(
                bounds1,
                Point {
                    x: w1 as i32 - 1,
                    y,
                },
                Edge::Right,
            );
            let expected = y as f32 / h1 as f32;
            assert!(
                (entry.pos - expected).abs() < 0.01,
                "w1={w1} h1={h1} y={y}: got {}, want {expected}",
                entry.pos
            );
            // The same normalized position, applied to a different-sized
            // neighbor, must land within a pixel of the proportionally
            // equivalent row.
            let bounds2 = rect(0, 0, w2, h2);
            let landed_y = (entry.pos * bounds2.height as f32).round() as i32;
            let expected_y = (expected * h2 as f32).round() as i32;
            assert!((landed_y - expected_y).abs() <= 1);
        }
    }

    #[test]
    fn detects_crossing_at_each_edge() {
        let bounds = rect(0, 0, 1000, 1000);
        let center = Point { x: 500, y: 500 };

        assert_eq!(
            detect_edge_crossing(bounds, center, Point { x: 0, y: 500 }, 20),
            Some(Edge::Left)
        );
        assert_eq!(
            detect_edge_crossing(bounds, center, Point { x: 999, y: 500 }, 20),
            Some(Edge::Right)
        );
        assert_eq!(
            detect_edge_crossing(bounds, center, Point { x: 500, y: 0 }, 20),
            Some(Edge::Top)
        );
        assert_eq!(
            detect_edge_crossing(bounds, center, Point { x: 500, y: 999 }, 20),
            Some(Edge::Bottom)
        );
    }

    #[test]
    fn no_crossing_when_not_at_a_boundary() {
        let bounds = rect(0, 0, 1000, 1000);
        assert_eq!(
            detect_edge_crossing(
                bounds,
                Point { x: 400, y: 400 },
                Point { x: 401, y: 400 },
                20
            ),
            None
        );
    }

    #[test]
    fn pinned_cursor_keeps_reporting_a_crossing_on_repeated_samples() {
        // Simulates the OS clamping: several consecutive samples at the
        // exact same boundary position, as happens when a user keeps
        // pushing into an edge. Every one of them must still count.
        let bounds = rect(0, 0, 1000, 1000);
        let at_edge = Point { x: 999, y: 500 };
        assert_eq!(
            detect_edge_crossing(bounds, at_edge, at_edge, 20),
            Some(Edge::Right)
        );
    }

    #[test]
    fn corner_dead_zone_suppresses_crossing() {
        let bounds = rect(0, 0, 1000, 1000);
        let near_corner = Point { x: 999, y: 5 };
        assert_eq!(
            detect_edge_crossing(bounds, Point { x: 990, y: 5 }, near_corner, 20),
            None,
            "within the dead zone of the top-right corner"
        );
        let just_outside = Point { x: 999, y: 25 };
        assert_eq!(
            detect_edge_crossing(bounds, Point { x: 990, y: 25 }, just_outside, 20),
            Some(Edge::Right),
            "outside the dead zone, even though still fairly close to the corner"
        );
    }

    #[test]
    fn reclaim_requires_moving_measurably_inward() {
        let bounds = rect(0, 0, 1000, 1000);
        assert!(!detect_edge_reclaim(
            bounds,
            Edge::Right,
            Point { x: 999, y: 0 },
            4
        ));
        assert!(!detect_edge_reclaim(
            bounds,
            Edge::Right,
            Point { x: 997, y: 0 },
            4
        ));
        assert!(detect_edge_reclaim(
            bounds,
            Edge::Right,
            Point { x: 990, y: 0 },
            4
        ));
    }

    #[test]
    fn layout_finds_the_adjacent_neighbor_and_only_that_edge() {
        let mut layout = Layout::new();
        let a = NodeId::new();
        let b = NodeId::new();
        let c = NodeId::new();

        // B sits directly to the right of A; C is placed far away and
        // touches nothing.
        layout.set_placement(a, rect(0, 0, 1000, 1000));
        layout.set_placement(b, rect(1000, 0, 1000, 1000));
        layout.set_placement(c, rect(5000, 5000, 1000, 1000));

        assert_eq!(layout.neighbor(a, Edge::Right), Some(b));
        assert_eq!(layout.neighbor(b, Edge::Left), Some(a));
        assert_eq!(layout.neighbor(a, Edge::Left), None);
        assert_eq!(layout.neighbor(a, Edge::Top), None);
        assert_eq!(layout.neighbor(c, Edge::Left), None);
    }

    #[test]
    fn layout_requires_full_edge_contact_not_just_a_touching_corner() {
        let mut layout = Layout::new();
        let a = NodeId::new();
        let b = NodeId::new();

        // B's top-left corner touches A's bottom-right corner, but they
        // don't share a full edge.
        layout.set_placement(a, rect(0, 0, 1000, 1000));
        layout.set_placement(b, rect(1000, 1000, 1000, 1000));

        assert_eq!(layout.neighbor(a, Edge::Right), None);
        assert_eq!(layout.neighbor(a, Edge::Bottom), None);
    }

    #[test]
    fn unplaced_node_has_no_neighbors() {
        let layout = Layout::new();
        assert_eq!(layout.neighbor(NodeId::new(), Edge::Right), None);
    }

    fn display(id: u32, bounds: Rect) -> Display {
        Display {
            id: DisplayId(id),
            bounds,
            scale_factor: 1.0,
            is_primary: id == 0,
        }
    }

    #[test]
    fn union_of_display_bounds_covers_every_display() {
        let displays = [
            display(0, rect(0, 0, 1920, 1080)),
            display(1, rect(1920, -200, 2560, 1440)),
        ];
        let union = union_of_display_bounds(&displays);
        assert_eq!(union, rect(0, -200, 4480, 1440));
    }

    #[test]
    fn union_of_display_bounds_of_empty_slice_is_zeroed() {
        assert_eq!(union_of_display_bounds(&[]), rect(0, 0, 0, 0));
    }

    // ─────────────────────────── Seam resolution ──────────────────────────
    //
    // Fixture: the real layout that motivated this — a single-monitor Mac
    // (1920×1080) placed to the LEFT of a Windows machine whose three
    // monitors are, left to right, a 1920×1080 landscape, a 1080×1920
    // portrait, and a 3840×2160 TV. The Windows virtual desktop is
    // 6840×2160; only the leftmost Windows monitor actually borders the Mac.

    /// The three Windows monitors, in the Windows machine's own coordinates.
    fn windows_monitors() -> [Rect; 3] {
        [
            rect(0, 0, 1920, 1080),    // left, landscape
            rect(1920, 0, 1080, 1920), // middle, portrait
            rect(3000, 0, 3840, 2160), // right, the TV
        ]
    }

    #[test]
    fn seam_is_the_one_monitor_the_other_machine_borders() {
        // From the Windows side: local = 3 monitors, peer = the Mac,
        // translated so it sits flush against the left monitor's left edge.
        let [left, portrait, tv] = windows_monitors();
        let mac_in_windows_space = rect(-1920, 0, 1920, 1080);

        let seams = resolve_seams(&[left, portrait, tv], &[mac_in_windows_space]);

        assert_eq!(seams.len(), 1, "exactly one handoff boundary");
        assert_eq!(seams[0].local_display, left);
        assert_eq!(seams[0].edge, Edge::Left);
        assert_eq!(seams[0].peer_display, mac_in_windows_space);
        assert_eq!(seams[0].overlap_px, 1080);
    }

    #[test]
    fn interior_edges_between_local_monitors_are_never_seams() {
        // Put a peer display flush against *every* outer edge the union has,
        // then confirm the left↔portrait and portrait↔TV interior seams
        // still never appear — a cursor there just slides to the next
        // monitor, it must not hand off.
        let [left, portrait, tv] = windows_monitors();
        let peers = [
            rect(-100, 0, 100, 1080), // left of the left monitor
            rect(6840, 0, 100, 2160), // right of the TV
            rect(0, -100, 6840, 100), // above everything
            rect(0, 2160, 6840, 100), // below everything
        ];

        let seams = resolve_seams(&[left, portrait, tv], &peers);

        // The only interior edges are the vertical ones where two monitors
        // abut: left|portrait and portrait|TV. Their top/bottom edges are
        // still genuine hull edges and may legitimately seam.
        for s in &seams {
            let interior = (s.local_display == left && s.edge == Edge::Right)
                || (s.local_display == portrait && matches!(s.edge, Edge::Left | Edge::Right))
                || (s.local_display == tv && s.edge == Edge::Left);
            assert!(!interior, "interior edge leaked a seam: {s:?}");
        }
        // And the boundaries that *should* resolve, do.
        assert!(
            seams
                .iter()
                .any(|s| s.local_display == left && s.edge == Edge::Left)
        );
        assert!(
            seams
                .iter()
                .any(|s| s.local_display == tv && s.edge == Edge::Right)
        );
    }

    #[test]
    fn outer_edge_with_nothing_beyond_it_is_not_a_seam() {
        // Just the Mac to the left — the TV's huge outer right edge, and
        // the top/bottom of every monitor, border nothing and must produce
        // no seam.
        let [left, portrait, tv] = windows_monitors();
        let seams = resolve_seams(&[left, portrait, tv], &[rect(-1920, 0, 1920, 1080)]);
        assert!(
            seams
                .iter()
                .all(|s| s.edge == Edge::Left && s.local_display == left)
        );
    }

    #[test]
    fn entry_point_normalizes_against_the_seam_monitor_not_the_union() {
        // The bug: leaving the bottom of the 1080-tall left monitor used to
        // normalize against the 2160-tall union and land the cursor
        // half-way down the peer. Against the seam display it's ~1.0.
        let [left, _, _] = windows_monitors();
        let seams = resolve_seams(
            &[left, windows_monitors()[1], windows_monitors()[2]],
            &[rect(-1920, 0, 1920, 1080)],
        );
        let seam = seams[0];

        let ep = compute_entry_point(seam.local_display, Point { x: 0, y: 1079 }, seam.edge);
        assert_eq!(ep.edge, Edge::Right);
        assert!((ep.pos - 1.0).abs() < 0.01, "got {}", ep.pos);

        // 60% down that monitor -> 60%, regardless of the union's height.
        let ep = compute_entry_point(seam.local_display, Point { x: 0, y: 648 }, seam.edge);
        assert!((ep.pos - 0.6).abs() < 0.01, "got {}", ep.pos);
    }

    #[test]
    fn seam_crossing_ignores_a_cursor_level_with_a_different_monitor() {
        // Stacked local monitors: a small one on top, a wide one below, with
        // the peer only against the TOP monitor's right edge. A cursor
        // pushed off the RIGHT at a y that belongs to the bottom monitor
        // must not read as crossing the top monitor's seam.
        let top = rect(0, 0, 800, 600);
        let bottom = rect(0, 600, 1600, 900);
        let peer = rect(800, 0, 400, 600); // flush to top's right edge only
        let seams = resolve_seams(&[top, bottom], &[peer]);
        assert_eq!(seams.len(), 1);
        assert_eq!(seams[0].local_display, top);

        // y = 1000 is on the bottom monitor -> no crossing.
        assert!(
            detect_seam_crossing(
                &seams,
                Point { x: 700, y: 1000 },
                Point { x: 799, y: 1000 },
                20
            )
            .is_none()
        );
        // y = 300 is on the top monitor -> crossing.
        assert!(
            detect_seam_crossing(
                &seams,
                Point { x: 700, y: 300 },
                Point { x: 799, y: 300 },
                20
            )
            .is_some()
        );
    }

    #[test]
    fn single_display_seam_matches_the_old_whole_desktop_behaviour() {
        // With one local display == the virtual desktop and one peer rect
        // adjacent on the right, seam resolution must reproduce exactly what
        // detect_edge_crossing against the union used to do.
        let desktop = rect(0, 0, 2560, 1440);
        let peer = rect(2560, 0, 1920, 1080);
        let seams = resolve_seams(&[desktop], &[peer]);
        assert_eq!(seams.len(), 1);
        assert_eq!(seams[0].edge, Edge::Right);
        assert_eq!(seams[0].local_display, desktop);

        let center = Point { x: 1280, y: 720 };
        assert!(detect_seam_crossing(&seams, center, Point { x: 2559, y: 720 }, 20).is_some());
        assert!(detect_seam_crossing(&seams, center, Point { x: 1280, y: 0 }, 20).is_none());
    }
}
