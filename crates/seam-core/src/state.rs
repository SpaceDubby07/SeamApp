//! Handoff state machine.
//!
//! Every input event and network message is fed through [`StateMachine::handle`],
//! which returns a list of [`Action`]s for the caller to execute. The state
//! machine itself performs NO I/O — this is what makes it fully unit
//! testable (Tier 7.1 of the build guide). The one piece of "environment"
//! it needs, wall-clock time for the post-handoff cooldown, is passed in by
//! the caller rather than read internally, so tests stay deterministic.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::protocol::{InputEvent, KeyCode, Modifiers};
use crate::topology::{
    Edge, EdgePoint, Layout, NodeId, Point, Rect, Seam, compute_entry_point, detect_edge_crossing,
    detect_edge_reclaim, detect_seam_crossing, resolve_seams,
};

/// Default time after a handoff before the reverse handoff is allowed to
/// fire. Without this, the boundary flickers (Tier 7.2). Overridable per
/// machine via [`StateMachine::set_edge_settings`] (the Layout panel).
const HANDOFF_COOLDOWN: Duration = Duration::from_millis(200);

/// Default corner dead zone, in pixels, so hitting a corner UI element
/// doesn't trigger an accidental handoff (Tier 7.2).
const DEFAULT_CORNER_DEAD_ZONE_PX: u32 = 20;

/// How far inside the entry edge a handoff lands the cursor by default —
/// Barrier's `avoidJumpZone`. The driven side warps here
/// ([`StateMachine::on_received_handoff`]); the driver seeds its
/// authoritative cursor the same distance in ([`crate::session`]), so with
/// a default configuration both ends agree on the landing point and
/// there's no visible correction hop on the first relayed move. Kept equal
/// to [`DEFAULT_CORNER_DEAD_ZONE_PX`] so the Layout panel's one "edge
/// margin" control still governs the driven-side inset via
/// [`StateMachine::entry_inset_px_for`].
///
/// Must stay equal to [`DEFAULT_CORNER_DEAD_ZONE_PX`] (a `u32`); a
/// `debug_assert` in [`StateMachine::new`] guards the two from drifting.
pub const HANDOFF_ENTRY_INSET_PX: i32 = 20;

/// How far (in pixels) the peer-driven cursor must travel *past its inset
/// entry point* (see [`StateMachine::entry_inset_px_for`]), inward, before a
/// push back out through the entry edge counts as "give control back"
/// rather than entry jitter or fast-flick overshoot.
///
/// Reclaim happens on the machine BEING driven (see
/// [`StateMachine::on_driven_cursor_moved`] and
/// `ControlMessage::ReleaseBack`), not by the driver watching its own
/// suppressed cursor — that earlier local-side approach reclaimed on any
/// few-pixel wobble right after a handoff and, on macOS (where a suppressed
/// cursor keeps physically moving), fired constantly, producing the
/// "it never leaves either screen, keeps re-grabbing" behaviour.
///
/// Together with the inset entry, this is Seam's version of Barrier's
/// jump-zone / `clearWait` guard against the "cursor passes through the
/// computer" bug: the arm threshold is `entry_inset + DRIVEN_BACKOUT_ARM_PX`
/// from the edge, so residual velocity from the flick that caused the
/// handoff can't immediately trip the reverse handoff.
const DRIVEN_BACKOUT_ARM_PX: i32 = 12;

/// Lower bound on the inset entry distance even when the configured corner
/// dead zone is small or zero — a fast flick still needs *some* landing
/// room on the new screen. 8px is below human "did the cursor move?"
/// perception but enough to absorb one over-large relayed delta.
const MIN_ENTRY_INSET_PX: i32 = 8;

/// The handoff state machine's current mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Not connected to a peer. All input passes through locally.
    Disconnected,
    /// Connected; local machine has control. Input goes to the local OS.
    /// We watch cursor position for edge crossings.
    LocalActive,
    /// Connected; the REMOTE machine has control. Our input is captured
    /// and suppressed locally, then forwarded over the wire.
    RemoteActive,
    /// Connected; the remote is driving US. We inject what they send. Our
    /// own local input (if any) is ignored while in this state.
    BeingDriven,
    /// Edge handoff temporarily disabled by the user (lock-to-screen).
    Locked,
}

/// One event fed into the state machine: local input, a network message
/// from the peer, or a lifecycle signal.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Input {
    /// The peer handshake completed successfully.
    PeerHandshakeOk(NodeId),
    /// The local cursor is now at this position (post-capture, in local
    /// virtual-desktop pixels). Only meaningful in `LocalActive` — while
    /// driving a peer our own cursor is suppressed and its position isn't
    /// consulted (reclaim is the driven side's job now).
    CursorMoved(Point),
    /// While `BeingDriven`: the peer-driven cursor is now at this position
    /// on our screen (in local pixels), integrated by the caller from the
    /// relayed motion it's injecting. Used to detect the cursor being
    /// pushed back out through the shared edge, which hands control back.
    DrivenCursorMoved(Point),
    /// The configured escape hotkey was pressed locally. In the real app
    /// this bypasses the normal event queue for latency (Tier 7.7), but
    /// the state machine's reaction to it is the same either way.
    EscapeHotkey,
    /// The user toggled lock-to-screen.
    LockToggled(bool),
    /// We received a `Handoff` message from `from`.
    ReceivedHandoff {
        /// Which peer is handing off to us.
        from: NodeId,
        /// Where on our screen to enter.
        entry: EdgePoint,
    },
    /// We received a `Reclaim` message from the peer we were driving.
    ReceivedReclaim,
    /// We received a `ReleaseBack` from the peer we were driving: it
    /// pushed the cursor back out through the shared edge, so control
    /// returns to us. Only meaningful in `RemoteActive`.
    ReceivedReleaseBack,
    /// We received an `EmergencyRelease` from the peer.
    ReceivedEmergencyRelease,
    /// The connection to the active peer was lost.
    ConnectionLost,
    /// The session is shutting down cleanly (the user hit Disconnect, or
    /// the peer sent `Goodbye`). Like [`Input::ConnectionLost`]'s cleanup
    /// — drop to `Disconnected`, release modifiers/suppression if we were
    /// driving or being driven — but with no reconnect, since this end is
    /// stopping on purpose (M12).
    Shutdown,
}

/// One thing the caller should do in response to an `Input`. The state
/// machine only decides *what* happens; executing it (sending a message,
/// calling into `seam-platform`) is the caller's job.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Action {
    /// Send a modifier snapshot to the peer. Always precedes `SendHandoff`
    /// and `SendReclaim` — Tier 7.1's non-negotiable invariant, and the fix
    /// for the stuck-modifier bug class.
    SendModifierState(Modifiers),
    /// Tell the peer "you have control now, enter here."
    SendHandoff(EdgePoint),
    /// Tell the peer "I'm taking control back." Sent by the driver.
    SendReclaim,
    /// Tell the peer "your cursor came back onto your own screen; you're
    /// driving again." Sent by the machine that was BEING driven, when the
    /// cursor is pushed back out through the shared edge.
    SendReleaseBack,
    /// Tell both sides to drop everything and return control local.
    SendEmergencyRelease,
    /// Enable/disable local input suppression
    /// (`InputCapture::set_suppression`).
    SetSuppression(bool),
    /// Release all locally-held modifier keys
    /// (`InputSink::release_all_modifiers`).
    ReleaseAllModifiers,
    /// Entering (`true`) or leaving (`false`) `BeingDriven`
    /// (`InputSink::set_being_driven`) — holds/releases an OS-level
    /// stay-awake assertion, since a machine receiving nothing but
    /// synthetic input can't be woken back up remotely once it sleeps.
    SetBeingDriven(bool),
    /// Warp the local cursor to this position (`InputSink::warp_cursor`).
    WarpCursor {
        /// Target X coordinate.
        x: i32,
        /// Target Y coordinate.
        y: i32,
    },
    /// Begin sending heartbeats / watching peer health.
    StartHeartbeat,
    /// Begin exponential-backoff reconnect attempts.
    StartReconnect,
    /// Pull the peer's current clipboard content (or push ours — the exact
    /// direction is a network-layer concern; this just marks "sync now").
    SyncClipboard,
}

/// The handoff state machine for one local node with, in v1, exactly one
/// peer. See Tier 15 of the build guide for what a third machine would
/// need — `peer` becoming a set rather than an `Option` is most of it,
/// since routing already goes through `Layout` rather than a hardcoded
/// pair.
pub struct StateMachine {
    state: State,
    local_node: NodeId,
    local_bounds: Rect,
    layout: Layout,
    /// This machine's individual displays, in local virtual-desktop
    /// coordinates. Defaults to a single rectangle equal to `local_bounds`
    /// (the pre-multi-monitor behaviour); `Session` replaces it with the
    /// real list once platform screen enumeration has run.
    local_display_rects: Vec<Rect>,
    /// The peer's individual displays, translated into OUR coordinate space
    /// (peer origin + the layout offset). Empty until the peer's
    /// `ScreenConfig` and a layout placement have both arrived — until then
    /// seam resolution falls back to the peer's whole-desktop bounds.
    peer_display_rects: Vec<Rect>,
    /// Resolved handoff boundaries (see [`crate::topology::Seam`]),
    /// recomputed whenever the displays or the layout change so the
    /// cursor-move hot path never has to.
    cached_seams: Vec<Seam>,
    /// While `BeingDriven`: the local display the peer's cursor entered on,
    /// so reclaim (back-out) detection measures against that monitor rather
    /// than the whole virtual desktop. `None` in every other state.
    driven_seam_display: Option<Rect>,
    /// The peer we're either driving (`RemoteActive`) or being driven by
    /// (`BeingDriven`). `None` in every other state.
    peer: Option<NodeId>,
    /// While `BeingDriven`: which local edge the peer's cursor entered
    /// through (the far side of the shared boundary), i.e. the edge it has
    /// to cross back out of to hand control back. `None` in every other
    /// state.
    driven_entry_edge: Option<Edge>,
    /// While `BeingDriven`: how far inside `driven_entry_edge` the cursor
    /// was placed on entry (see [`Self::entry_inset_px_for`]). The back-out
    /// detector arms at `driven_entry_inset + DRIVEN_BACKOUT_ARM_PX` from
    /// the edge. `0` in every other state.
    driven_entry_inset: i32,
    /// While `BeingDriven`: the last integrated position of the
    /// peer-driven cursor, in local pixels, for back-out edge detection.
    last_driven_cursor: Option<Point>,
    /// While `BeingDriven`: set once the driven cursor has moved far enough
    /// inward past its inset entry point that a subsequent push back out
    /// through the entry edge is a deliberate exit and not entry jitter or
    /// fast-flick overshoot.
    driven_backout_armed: bool,
    last_cursor: Option<Point>,
    last_handoff_at: Option<Instant>,
    held_modifiers: Modifiers,
    /// Tier 7.7: remembered per-peer cursor position, so a reclaim warps
    /// back to where you left off rather than dumping you at the edge.
    remembered_cursor: HashMap<NodeId, Point>,
    corner_dead_zone_px: u32,
    /// Post-handoff cooldown before the reverse handoff can fire. Defaults
    /// to [`HANDOFF_COOLDOWN`]; set from config via
    /// [`Self::set_edge_settings`].
    handoff_cooldown: Duration,
}

impl StateMachine {
    /// Creates a disconnected state machine for `local_node`, with
    /// `local_bounds` as its virtual-desktop bounds and `layout` as the
    /// shared canvas used to resolve which peer sits across a given edge.
    #[must_use]
    pub fn new(local_node: NodeId, local_bounds: Rect, layout: Layout) -> Self {
        debug_assert_eq!(
            HANDOFF_ENTRY_INSET_PX.cast_unsigned(),
            DEFAULT_CORNER_DEAD_ZONE_PX,
            "the driver's authoritative-cursor seed inset must match the driven-side default"
        );
        let mut sm = Self {
            state: State::Disconnected,
            local_node,
            local_bounds,
            layout,
            local_display_rects: vec![local_bounds],
            peer_display_rects: Vec::new(),
            cached_seams: Vec::new(),
            driven_seam_display: None,
            peer: None,
            driven_entry_edge: None,
            driven_entry_inset: 0,
            last_driven_cursor: None,
            driven_backout_armed: false,
            last_cursor: None,
            last_handoff_at: None,
            held_modifiers: Modifiers::default(),
            remembered_cursor: HashMap::new(),
            corner_dead_zone_px: DEFAULT_CORNER_DEAD_ZONE_PX,
            handoff_cooldown: HANDOFF_COOLDOWN,
        };
        sm.recompute_seams();
        sm
    }

    /// Recomputes [`Self::cached_seams`] from the current local displays,
    /// peer displays, and layout placement. Cheap (a handful of rectangle
    /// comparisons), but kept off the cursor-move path all the same — call
    /// this from every setter that changes an input to seam resolution, not
    /// from `handle`.
    fn recompute_seams(&mut self) {
        let peer_rects: Vec<Rect> = if self.peer_display_rects.is_empty() {
            // No per-display info yet — fall back to the peer's whole
            // virtual desktop as a single rectangle, which reproduces the
            // pre-multi-monitor "one tile per machine" behaviour exactly.
            self.peer_bounds().into_iter().collect()
        } else {
            self.peer_display_rects.clone()
        };
        self.cached_seams = resolve_seams(&self.local_display_rects, &peer_rects);
    }

    /// Sets this machine's individual displays (local virtual-desktop
    /// coordinates). Takes effect on the next crossing/reclaim check.
    pub fn set_local_displays(&mut self, displays: Vec<Rect>) {
        self.local_display_rects = if displays.is_empty() {
            vec![self.local_bounds]
        } else {
            displays
        };
        self.recompute_seams();
    }

    /// Sets the peer's individual displays, already translated into OUR
    /// coordinate space. Takes effect on the next crossing/reclaim check.
    pub fn set_peer_displays(&mut self, displays_in_local_space: Vec<Rect>) {
        self.peer_display_rects = displays_in_local_space;
        self.recompute_seams();
    }

    /// Applies the Layout panel's edge-handoff tuning (Tier 8.1). Takes
    /// effect on the next crossing check; leaves the current state alone.
    pub fn set_edge_settings(&mut self, corner_dead_zone_px: u32, handoff_cooldown_ms: u64) {
        self.corner_dead_zone_px = corner_dead_zone_px;
        self.handoff_cooldown = Duration::from_millis(handoff_cooldown_ms);
    }

    /// The current state.
    #[must_use]
    pub fn state(&self) -> State {
        self.state
    }

    /// The local machine's virtual-desktop bounds, as given to `new`. A
    /// caller driving this state machine (e.g. `Session`) needs this to
    /// normalize/denormalize coordinates for messages that cross the wire.
    #[must_use]
    pub fn local_bounds(&self) -> Rect {
        self.local_bounds
    }

    /// Updates where `peer` sits on the shared layout canvas (Tier 8.1's
    /// drag-and-snap tiles, M11) — after the local user rearranges it, or
    /// after the peer sends its own arrangement via
    /// `ControlMessage::LayoutUpdate`. Takes effect on the next crossing/
    /// reclaim check; doesn't touch whichever state we're already in, so
    /// rearranging mid-session is safe (if momentarily surprising if you
    /// move a shared edge out from under an active handoff).
    pub fn set_peer_placement(&mut self, peer: NodeId, bounds: Rect) {
        self.layout.set_placement(peer, bounds);
        self.recompute_seams();
    }

    /// The peer's current bounds on the shared layout canvas, if placed.
    #[must_use]
    pub fn peer_bounds(&self) -> Option<Rect> {
        self.peer.and_then(|peer| self.layout.bounds_of(peer))
    }

    /// Which modifier keys are currently physically held, as tracked by
    /// `track_modifier`.
    #[must_use]
    pub fn held_modifiers(&self) -> Modifiers {
        self.held_modifiers
    }

    /// Diagnostics only: while `BeingDriven`, the edge the peer's cursor
    /// entered through and whether the back-out detector has armed yet
    /// (see [`Self::on_driven_cursor_moved`]). `None` in every other
    /// state. Used by `Session` to log why a reclaim is or isn't firing.
    #[must_use]
    pub fn driven_backout_state(&self) -> Option<(Edge, bool)> {
        self.driven_entry_edge
            .map(|edge| (edge, self.driven_backout_armed))
    }

    /// Jumps directly to `state` without going through a transition,
    /// bypassing whatever invariants a real transition would set up (which
    /// peer is active, cursor history, etc). Test-only, for setting up a
    /// scenario's starting point.
    #[cfg(test)]
    fn force_state(&mut self, state: State, peer: Option<NodeId>) {
        self.state = state;
        self.peer = peer;
    }

    /// Tracks currently-held modifiers from raw key events, independent of
    /// `handle`, so a snapshot is always ready the instant a handoff needs
    /// one. Call this for every `KeyDown`/`KeyUp` local capture reports,
    /// regardless of state.
    pub fn track_modifier(&mut self, event: &InputEvent) {
        let (code, down) = match *event {
            InputEvent::KeyDown { code, .. } => (code, true),
            InputEvent::KeyUp { code } => (code, false),
            _ => return,
        };
        match code {
            KeyCode::LeftShift | KeyCode::RightShift => self.held_modifiers.shift = down,
            KeyCode::LeftCtrl | KeyCode::RightCtrl => self.held_modifiers.ctrl = down,
            KeyCode::LeftAlt | KeyCode::RightAlt => self.held_modifiers.alt = down,
            KeyCode::LeftMeta | KeyCode::RightMeta => self.held_modifiers.meta = down,
            // Caps Lock is a toggle, not a hold — flip on press, ignore release.
            KeyCode::CapsLock if down => self.held_modifiers.caps = !self.held_modifiers.caps,
            _ => {}
        }
    }

    /// Feeds one input into the state machine at time `now`, returning the
    /// actions the caller should perform.
    pub fn handle(&mut self, input: Input, now: Instant) -> Vec<Action> {
        match input {
            Input::PeerHandshakeOk(peer) => self.on_handshake_ok(peer),
            Input::CursorMoved(pos) => self.on_cursor_moved(pos, now),
            Input::DrivenCursorMoved(pos) => self.on_driven_cursor_moved(pos),
            Input::EscapeHotkey => self.on_escape_hotkey(),
            Input::LockToggled(locked) => self.on_lock_toggled(locked),
            Input::ReceivedHandoff { from, entry } => self.on_received_handoff(from, entry),
            Input::ReceivedReclaim => self.on_received_reclaim(),
            Input::ReceivedReleaseBack => self.on_received_release_back(now),
            Input::ReceivedEmergencyRelease => self.on_emergency_release(),
            Input::ConnectionLost => self.on_connection_lost(),
            Input::Shutdown => self.on_shutdown(),
        }
    }

    fn on_handshake_ok(&mut self, peer: NodeId) -> Vec<Action> {
        if self.state != State::Disconnected {
            return Vec::new();
        }
        self.peer = Some(peer);
        self.state = State::LocalActive;
        // Now that we know who the peer is, its layout placement can
        // resolve into seams (before this `peer_bounds()` was `None`).
        self.recompute_seams();
        vec![Action::StartHeartbeat, Action::SyncClipboard]
    }

    fn on_cursor_moved(&mut self, pos: Point, now: Instant) -> Vec<Action> {
        let prev = self.last_cursor.replace(pos);

        match self.state {
            State::LocalActive => self.try_handoff(prev, pos, now),
            // While `RemoteActive` our own cursor is suppressed (and on
            // macOS decoupled/frozen); reclaim is decided on the driven
            // side now (`on_driven_cursor_moved`), so there's nothing to
            // do with a local position reading here.
            State::RemoteActive | State::BeingDriven | State::Disconnected | State::Locked => {
                Vec::new()
            }
        }
    }

    /// While `BeingDriven`, tracks the peer-driven cursor and hands
    /// control back once it's been pushed out through the shared edge it
    /// entered on. This is the reclaim trigger — it lives here, on the
    /// machine being driven, rather than on the driver (which can't see
    /// where the cursor visibly is, and whose own suppressed cursor is an
    /// unreliable proxy — especially on macOS).
    fn on_driven_cursor_moved(&mut self, pos: Point) -> Vec<Action> {
        if self.state != State::BeingDriven {
            return Vec::new();
        }
        let Some(edge) = self.driven_entry_edge else {
            return Vec::new();
        };
        // Back-out geometry is a hybrid: the entry-edge axis comes from the
        // seam monitor (so "pushed back out" means the line the cursor
        // actually crossed to get here — which need not be the union's
        // outer edge), but the PERPENDICULAR axis spans the whole virtual
        // desktop, because a driven cursor roams every monitor. Using the
        // narrow seam monitor for both axes was a bug: once the cursor's
        // off-seam-monitor coordinate left that monitor's span, the corner
        // dead-zone check in `detect_edge_crossing` tripped on a negative
        // distance and reclaim could never fire — control got stuck on this
        // side.
        let bounds = self.driven_backout_bounds();
        let prev = self.last_driven_cursor.replace(pos);

        if !self.driven_backout_armed {
            // Arm only once the cursor is unambiguously inside our screen —
            // measured from the edge, so it accounts for the inset entry
            // point (a fast flick that overshot toward the edge stays
            // unarmed, and so can't bounce straight back).
            let arm_from_edge = self.driven_entry_inset + DRIVEN_BACKOUT_ARM_PX;
            if detect_edge_reclaim(bounds, edge, pos, arm_from_edge) {
                self.driven_backout_armed = true;
            }
            return Vec::new();
        }

        let Some(prev) = prev else {
            return Vec::new();
        };
        if detect_edge_crossing(bounds, prev, pos, self.corner_dead_zone_px) != Some(edge) {
            return Vec::new();
        }

        // Pushed back out through the shared edge — control returns to the
        // peer. Release every modifier we were holding on its behalf
        // (Tier 7.1: mandatory on every exit from `BeingDriven`).
        self.state = State::LocalActive;
        self.peer = None;
        self.clear_driven_tracking();
        vec![
            Action::SendReleaseBack,
            Action::ReleaseAllModifiers,
            Action::SetBeingDriven(false),
        ]
    }

    /// The rectangle back-out detection runs against while `BeingDriven`
    /// (see [`Self::on_driven_cursor_moved`]): the seam monitor's extent
    /// along the entry-edge axis, the whole virtual desktop's along the
    /// other. Falls back to the whole desktop when there's no seam monitor
    /// (single-monitor machine, or a handoff with no resolved seam).
    fn driven_backout_bounds(&self) -> Rect {
        let Some(seam) = self.driven_seam_display else {
            return self.local_bounds;
        };
        let desk = self.local_bounds;
        match self.driven_entry_edge {
            Some(Edge::Left | Edge::Right) => Rect {
                x: seam.x,
                width: seam.width,
                y: desk.y,
                height: desk.height,
            },
            Some(Edge::Top | Edge::Bottom) => Rect {
                x: desk.x,
                width: desk.width,
                y: seam.y,
                height: seam.height,
            },
            None => desk,
        }
    }

    /// Resets the `BeingDriven` cursor-tracking bookkeeping. Called on
    /// every transition out of `BeingDriven`.
    fn clear_driven_tracking(&mut self) {
        self.driven_entry_edge = None;
        self.driven_entry_inset = 0;
        self.driven_seam_display = None;
        self.last_driven_cursor = None;
        self.driven_backout_armed = false;
    }

    /// How far inside the entry edge to place the peer-driven cursor on a
    /// handoff — Barrier's `avoidJumpZone` idea. Landing a jump-zone width
    /// *in*, rather than exactly on the edge, means a fast flick that
    /// carried the cursor across the boundary puts it visibly on the new
    /// screen with room to stop, instead of pinned against the far side of
    /// the edge where the slightest reverse motion bounces control
    /// straight back ("the cursor passes through the computer").
    ///
    /// Tracks the configured corner dead zone (so the Layout panel's one
    /// "edge margin" control governs both), floored at
    /// [`MIN_ENTRY_INSET_PX`] and capped at a third of the smaller
    /// dimension of `bounds` so a large dead zone can't warp past the
    /// middle. `bounds` is the *seam monitor*, not the whole virtual
    /// desktop, so the cap tracks the panel the cursor actually lands on
    /// (e.g. a 1080-wide portrait display).
    fn entry_inset_px_for(&self, bounds: Rect) -> i32 {
        let cap = bounds.width.min(bounds.height).cast_signed() / 3;
        self.corner_dead_zone_px
            .cast_signed()
            .max(MIN_ENTRY_INSET_PX)
            .min(cap.max(0))
    }

    fn try_handoff(&mut self, prev: Option<Point>, pos: Point, now: Instant) -> Vec<Action> {
        let Some(prev) = prev else {
            return Vec::new();
        };
        if let Some(cooldown_started) = self.last_handoff_at
            && now.duration_since(cooldown_started) < self.handoff_cooldown
        {
            return Vec::new();
        }
        if self.peer.is_none() {
            return Vec::new();
        }
        // Match the cursor against a resolved seam — one specific local
        // display edge that borders the peer — rather than the outer edge
        // of the whole virtual desktop. On a single-monitor machine the two
        // are identical; on a multi-monitor one this is what stops an
        // interior edge between two local monitors, or an outer edge facing
        // nothing, from triggering a handoff. `cached_seams` is kept in
        // step by `recompute_seams`, so the hot path only reads it.
        let Some(&seam) =
            detect_seam_crossing(&self.cached_seams, prev, pos, self.corner_dead_zone_px)
        else {
            return Vec::new();
        };

        let entry = compute_entry_point(seam.local_display, pos, seam.edge);
        self.state = State::RemoteActive;
        self.last_handoff_at = Some(now);
        self.remembered_cursor.insert(self.local_node, pos);

        vec![
            Action::SendModifierState(self.held_modifiers),
            Action::SendHandoff(entry),
            Action::SetSuppression(true),
        ]
    }

    /// Driver side: the peer we were driving pushed the cursor back out
    /// through the shared edge and sent `ReleaseBack`. Return to
    /// `LocalActive`, drop suppression, warp our cursor back to where the
    /// user left off, and release any modifiers (Tier 7.1: mandatory on
    /// every exit from `RemoteActive`). No `SendReclaim` — the peer
    /// initiated this and already knows.
    fn on_received_release_back(&mut self, now: Instant) -> Vec<Action> {
        if self.state != State::RemoteActive {
            return Vec::new();
        }
        self.state = State::LocalActive;
        // Re-arm the post-handoff cooldown so a flick that carried the
        // cursor all the way through the peer and back can't immediately
        // hand off again — the boundary-oscillation guard, matching
        // Barrier's `stopSwitch` on a screen switch.
        self.last_handoff_at = Some(now);
        let warp_to = self
            .remembered_cursor
            .get(&self.local_node)
            .copied()
            .unwrap_or(Point { x: 0, y: 0 });
        vec![
            Action::SetSuppression(false),
            Action::WarpCursor {
                x: warp_to.x,
                y: warp_to.y,
            },
            Action::ReleaseAllModifiers,
        ]
    }

    fn on_escape_hotkey(&mut self) -> Vec<Action> {
        match self.state {
            // Force-return control to the local machine regardless of
            // which side of the handoff we're currently on — Tier 0's
            // "force-return control to local machine" and Tier 7.7's "must
            // work when everything else is broken".
            State::RemoteActive => {
                self.state = State::LocalActive;
                vec![
                    Action::SendEmergencyRelease,
                    Action::SetSuppression(false),
                    Action::ReleaseAllModifiers,
                ]
            }
            State::BeingDriven => {
                self.state = State::LocalActive;
                self.peer = None;
                self.clear_driven_tracking();
                vec![
                    Action::SendEmergencyRelease,
                    Action::ReleaseAllModifiers,
                    Action::SetBeingDriven(false),
                ]
            }
            State::LocalActive | State::Disconnected | State::Locked => Vec::new(),
        }
    }

    fn on_lock_toggled(&mut self, locked: bool) -> Vec<Action> {
        match (self.state, locked) {
            (State::LocalActive, true) => {
                self.state = State::Locked;
                Vec::new()
            }
            (State::Locked, false) => {
                self.state = State::LocalActive;
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn on_received_handoff(&mut self, from: NodeId, entry: EdgePoint) -> Vec<Action> {
        self.state = State::BeingDriven;
        self.peer = Some(from);
        // Land on the seam monitor that borders the driver, not somewhere
        // in the whole virtual desktop: pick the resolved seam matching the
        // entry edge (falling back to the largest seam, then to the whole
        // desktop if we somehow have none). `entry.pos` is normalized
        // against the driver's seam monitor, so it maps 1:1 onto ours.
        let bounds = self
            .cached_seams
            .iter()
            .find(|s| s.edge == entry.edge)
            .or_else(|| self.cached_seams.first())
            .map_or(self.local_bounds, |s| s.local_display);
        self.driven_seam_display = Some(bounds);
        // Land the cursor `inset` px INSIDE the entry edge, not on it (see
        // `entry_inset_px_for`). `pos` still fixes the coordinate *along* the
        // edge; the inset is the offset *into* the screen.
        let inset = self.entry_inset_px_for(bounds);
        let Point { x, y } = crate::topology::place_on_edge(bounds, entry.edge, entry.pos, inset);
        // Arm the back-out detector fresh: it won't trip until the cursor
        // has travelled `driven_entry_inset + DRIVEN_BACKOUT_ARM_PX` inward
        // from the edge, so residual velocity from the flick that caused
        // this handoff can't bounce control straight back
        // (`on_driven_cursor_moved`).
        self.driven_entry_edge = Some(entry.edge);
        self.driven_entry_inset = inset;
        self.last_driven_cursor = Some(Point { x, y });
        self.driven_backout_armed = false;
        vec![Action::WarpCursor { x, y }, Action::SetBeingDriven(true)]
    }

    fn on_received_reclaim(&mut self) -> Vec<Action> {
        if self.state != State::BeingDriven {
            return Vec::new();
        }
        self.state = State::LocalActive;
        self.peer = None;
        self.clear_driven_tracking();
        vec![Action::ReleaseAllModifiers, Action::SetBeingDriven(false)]
    }

    fn on_emergency_release(&mut self) -> Vec<Action> {
        match self.state {
            State::RemoteActive | State::BeingDriven => {
                let was_being_driven = self.state == State::BeingDriven;
                self.state = State::LocalActive;
                self.peer = None;
                self.clear_driven_tracking();
                let mut actions = vec![Action::SetSuppression(false), Action::ReleaseAllModifiers];
                if was_being_driven {
                    actions.push(Action::SetBeingDriven(false));
                }
                actions
            }
            State::LocalActive | State::Disconnected | State::Locked => Vec::new(),
        }
    }

    fn on_connection_lost(&mut self) -> Vec<Action> {
        if self.state == State::Disconnected {
            return Vec::new();
        }
        let was_active_or_driven = matches!(self.state, State::RemoteActive | State::BeingDriven);
        let was_being_driven = self.state == State::BeingDriven;
        self.state = State::Disconnected;
        self.peer = None;
        self.clear_driven_tracking();

        let mut actions = Vec::new();
        if was_active_or_driven {
            actions.push(Action::SetSuppression(false));
            actions.push(Action::ReleaseAllModifiers);
        }
        if was_being_driven {
            actions.push(Action::SetBeingDriven(false));
        }
        actions.push(Action::StartReconnect);
        actions
    }

    /// Clean shutdown: identical cleanup to [`Self::on_connection_lost`]
    /// (release modifiers and suppression on the way out of a driving
    /// state — Tier 7.1's non-negotiable invariant) but without the
    /// `StartReconnect` action, since this end is stopping deliberately.
    fn on_shutdown(&mut self) -> Vec<Action> {
        if self.state == State::Disconnected {
            return Vec::new();
        }
        let was_active_or_driven = matches!(self.state, State::RemoteActive | State::BeingDriven);
        let was_being_driven = self.state == State::BeingDriven;
        self.state = State::Disconnected;
        self.peer = None;
        self.clear_driven_tracking();

        if !was_active_or_driven {
            return Vec::new();
        }
        let mut actions = vec![Action::SetSuppression(false), Action::ReleaseAllModifiers];
        if was_being_driven {
            actions.push(Action::SetBeingDriven(false));
        }
        actions
    }
}

#[cfg(test)]
#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
mod tests {
    use super::{Action, Input, Layout, NodeId, Point, Rect, State, StateMachine};
    use crate::protocol::{InputEvent, KeyCode, Modifiers};
    use std::time::{Duration, Instant};

    fn local_bounds() -> Rect {
        Rect {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        }
    }

    /// A machine with a single neighbor placed directly to its right.
    fn two_node_machine() -> (StateMachine, NodeId, NodeId) {
        let local = NodeId::new();
        let peer = NodeId::new();
        let mut layout = Layout::new();
        layout.set_placement(local, local_bounds());
        layout.set_placement(
            peer,
            Rect {
                x: 1920,
                y: 0,
                width: 1920,
                height: 1080,
            },
        );
        (
            StateMachine::new(local, local_bounds(), layout),
            local,
            peer,
        )
    }

    #[test]
    fn handshake_starts_heartbeat_and_moves_to_local_active() {
        let (mut sm, _, peer) = two_node_machine();
        let actions = sm.handle(Input::PeerHandshakeOk(peer), Instant::now());
        assert_eq!(sm.state(), State::LocalActive);
        assert!(actions.contains(&Action::StartHeartbeat));
        assert!(actions.contains(&Action::SyncClipboard));
    }

    /// The stuck-modifier bug is the single most common failure mode in
    /// tools like this. Test it explicitly, at every exit path.
    #[test]
    fn handoff_with_held_modifier_sends_snapshot_before_handoff_and_suppresses() {
        let (mut sm, _, peer) = two_node_machine();
        sm.handle(Input::PeerHandshakeOk(peer), Instant::now());

        // User holds Ctrl, then slides across the right edge.
        sm.track_modifier(&InputEvent::KeyDown {
            code: KeyCode::LeftCtrl,
            repeat: false,
        });
        sm.handle(Input::CursorMoved(Point { x: 960, y: 540 }), Instant::now());
        let actions = sm.handle(
            Input::CursorMoved(Point { x: 1919, y: 540 }),
            Instant::now(),
        );

        assert_eq!(sm.state(), State::RemoteActive);
        // The modifier snapshot MUST precede the handoff message.
        let modifier_idx = actions
            .iter()
            .position(|a| matches!(a, Action::SendModifierState(_)))
            .expect("must send a modifier snapshot");
        let handoff_idx = actions
            .iter()
            .position(|a| matches!(a, Action::SendHandoff(_)))
            .expect("must send a handoff");
        assert!(modifier_idx < handoff_idx);
        assert_eq!(
            actions[modifier_idx],
            Action::SendModifierState(Modifiers {
                ctrl: true,
                ..Modifiers::default()
            })
        );
        assert!(actions.contains(&Action::SetSuppression(true)));
    }

    #[test]
    fn connection_loss_always_releases_modifiers_and_disables_suppression() {
        for state in [State::RemoteActive, State::BeingDriven] {
            let (mut sm, _, peer) = two_node_machine();
            sm.force_state(state, Some(peer));

            let actions = sm.handle(Input::ConnectionLost, Instant::now());

            assert_eq!(sm.state(), State::Disconnected, "state was {state:?}");
            assert!(
                actions.contains(&Action::ReleaseAllModifiers),
                "state {state:?} failed to release modifiers on disconnect"
            );
            assert!(
                actions.contains(&Action::SetSuppression(false)),
                "state {state:?} failed to disable suppression on disconnect"
            );
            assert_eq!(
                actions.contains(&Action::SetBeingDriven(false)),
                state == State::BeingDriven,
                "SetBeingDriven(false) must fire iff we were actually BeingDriven, state {state:?}"
            );
            assert!(actions.contains(&Action::StartReconnect));
        }
    }

    #[test]
    fn shutdown_releases_modifiers_and_suppression_but_does_not_reconnect() {
        for state in [State::RemoteActive, State::BeingDriven] {
            let (mut sm, _, peer) = two_node_machine();
            sm.force_state(state, Some(peer));

            let actions = sm.handle(Input::Shutdown, Instant::now());

            assert_eq!(sm.state(), State::Disconnected, "state was {state:?}");
            assert!(
                actions.contains(&Action::ReleaseAllModifiers),
                "state {state:?} failed to release modifiers on shutdown"
            );
            assert!(
                actions.contains(&Action::SetSuppression(false)),
                "state {state:?} failed to disable suppression on shutdown"
            );
            assert_eq!(
                actions.contains(&Action::SetBeingDriven(false)),
                state == State::BeingDriven,
                "SetBeingDriven(false) must fire iff we were actually BeingDriven, state {state:?}"
            );
            assert!(
                !actions.contains(&Action::StartReconnect),
                "shutdown must not schedule a reconnect"
            );
        }
    }

    #[test]
    fn connection_loss_while_disconnected_is_a_no_op() {
        let (mut sm, ..) = two_node_machine();
        assert_eq!(sm.state(), State::Disconnected);
        let actions = sm.handle(Input::ConnectionLost, Instant::now());
        assert!(actions.is_empty());
    }

    #[test]
    fn escape_from_remote_active_releases_modifiers_and_returns_local() {
        let (mut sm, _, peer) = two_node_machine();
        sm.force_state(State::RemoteActive, Some(peer));

        let actions = sm.handle(Input::EscapeHotkey, Instant::now());

        assert_eq!(sm.state(), State::LocalActive);
        assert!(actions.contains(&Action::SendEmergencyRelease));
        assert!(actions.contains(&Action::SetSuppression(false)));
        assert!(actions.contains(&Action::ReleaseAllModifiers));
    }

    #[test]
    fn escape_from_being_driven_also_forces_local_control_back() {
        let (mut sm, _, peer) = two_node_machine();
        sm.force_state(State::BeingDriven, Some(peer));

        let actions = sm.handle(Input::EscapeHotkey, Instant::now());

        assert_eq!(sm.state(), State::LocalActive);
        assert!(actions.contains(&Action::ReleaseAllModifiers));
        assert!(actions.contains(&Action::SetBeingDriven(false)));
    }

    #[test]
    fn escape_while_already_local_is_a_no_op() {
        let (mut sm, _, peer) = two_node_machine();
        sm.handle(Input::PeerHandshakeOk(peer), Instant::now());
        assert_eq!(sm.state(), State::LocalActive);

        let actions = sm.handle(Input::EscapeHotkey, Instant::now());
        assert!(actions.is_empty());
        assert_eq!(sm.state(), State::LocalActive);
    }

    #[test]
    fn received_handoff_warps_cursor_inset_from_the_entry_edge() {
        let (mut sm, _, peer) = two_node_machine();
        let entry = crate::topology::EdgePoint {
            edge: crate::topology::Edge::Left,
            pos: 0.25,
        };

        let actions = sm.handle(Input::ReceivedHandoff { from: peer, entry }, Instant::now());

        assert_eq!(sm.state(), State::BeingDriven);
        // Lands the default 20px dead-zone width inside the left edge, not
        // at x=0 — Barrier's `avoidJumpZone` behaviour.
        assert_eq!(
            actions[0],
            Action::WarpCursor {
                x: 20,
                y: (0.25 * 1080.0) as i32
            }
        );
        assert!(actions.contains(&Action::SetBeingDriven(true)));
    }

    #[test]
    fn handoff_entry_inset_is_capped_on_a_small_screen() {
        let local = NodeId::new();
        let peer = NodeId::new();
        let tiny = Rect {
            x: 0,
            y: 0,
            width: 60,
            height: 60,
        };
        let mut layout = Layout::new();
        layout.set_placement(local, tiny);
        layout.set_placement(
            peer,
            Rect {
                x: -60,
                y: 0,
                width: 60,
                height: 60,
            },
        );
        let mut sm = StateMachine::new(local, tiny, layout);
        // Default dead zone is 20, but a third of 60 is 20, so this still
        // fits — push the dead zone up to force the cap.
        sm.set_edge_settings(50, 200);
        sm.handle(Input::PeerHandshakeOk(peer), Instant::now());

        let actions = sm.handle(
            Input::ReceivedHandoff {
                from: peer,
                entry: crate::topology::EdgePoint {
                    edge: crate::topology::Edge::Left,
                    pos: 0.5,
                },
            },
            Instant::now(),
        );
        // Capped at width/3 = 20, never past the middle.
        assert_eq!(actions[0], Action::WarpCursor { x: 20, y: 30 });
    }

    /// The "cursor passes through the computer" regression: right after a
    /// handoff, residual velocity from the same fast flick keeps arriving
    /// as inward-then-outward deltas. Because entry is inset and the arm
    /// threshold sits past the inset, none of it trips the reverse
    /// handoff — control stays put.
    #[test]
    fn fast_flick_overshoot_after_handoff_does_not_bounce_control_back() {
        let local = NodeId::new();
        let peer = NodeId::new();
        let mut layout = Layout::new();
        layout.set_placement(local, local_bounds());
        layout.set_placement(
            peer,
            Rect {
                x: -1920,
                y: 0,
                width: 1920,
                height: 1080,
            },
        );
        let mut sm = StateMachine::new(local, local_bounds(), layout);
        sm.handle(Input::PeerHandshakeOk(peer), Instant::now());

        let actions = sm.handle(
            Input::ReceivedHandoff {
                from: peer,
                entry: crate::topology::EdgePoint {
                    edge: crate::topology::Edge::Left,
                    pos: 0.5,
                },
            },
            Instant::now(),
        );
        assert_eq!(actions[0], Action::WarpCursor { x: 20, y: 540 });

        // A fast leftward flick's tail: the cursor briefly moves a little
        // further in (still settling) then is carried back out past the
        // edge. Never armed (never reached x = 20 + 12), so no hand-back.
        for x in [26, 22, 10, 0, -8, -30] {
            let actions = sm.handle(
                Input::DrivenCursorMoved(Point { x, y: 540 }),
                Instant::now(),
            );
            assert!(
                actions.is_empty(),
                "x={x} produced {actions:?} — a flick tail must not reclaim"
            );
            assert_eq!(sm.state(), State::BeingDriven, "bounced back at x={x}");
        }

        // A deliberate move well inside then back out the edge still works.
        sm.handle(
            Input::DrivenCursorMoved(Point { x: 500, y: 540 }),
            Instant::now(),
        );
        let actions = sm.handle(
            Input::DrivenCursorMoved(Point { x: -2, y: 540 }),
            Instant::now(),
        );
        assert_eq!(sm.state(), State::LocalActive);
        assert!(actions.contains(&Action::SendReleaseBack));
    }

    /// The driver side of the same concern: a `ReleaseBack` re-arms the
    /// post-handoff cooldown, so a flick that carried the cursor through
    /// the peer and back can't immediately hand off again even if the
    /// original handoff was long ago.
    #[test]
    fn release_back_re_arms_the_handoff_cooldown() {
        let (mut sm, _, peer) = two_node_machine(); // peer on the RIGHT
        sm.handle(Input::PeerHandshakeOk(peer), Instant::now());
        let t0 = Instant::now();

        // Hand off to the right, then get it back a full second later.
        sm.handle(Input::CursorMoved(Point { x: 960, y: 540 }), t0);
        sm.handle(Input::CursorMoved(Point { x: 1919, y: 540 }), t0);
        assert_eq!(sm.state(), State::RemoteActive);
        let t_back = t0 + Duration::from_secs(1);
        sm.handle(Input::ReceivedReleaseBack, t_back);
        assert_eq!(sm.state(), State::LocalActive);

        // 50ms after the reclaim — inside the (re-armed) 200ms cooldown —
        // pushing the edge again must NOT hand off.
        sm.handle(
            Input::CursorMoved(Point { x: 1919, y: 540 }),
            t_back + Duration::from_millis(50),
        );
        assert_eq!(sm.state(), State::LocalActive, "re-handoff inside cooldown");

        // Past the cooldown, it can.
        sm.handle(
            Input::CursorMoved(Point { x: 1919, y: 540 }),
            t_back + Duration::from_millis(250),
        );
        assert_eq!(sm.state(), State::RemoteActive);
    }

    #[test]
    fn reclaim_from_being_driven_releases_modifiers_and_returns_local() {
        let (mut sm, _, peer) = two_node_machine();
        sm.force_state(State::BeingDriven, Some(peer));

        let actions = sm.handle(Input::ReceivedReclaim, Instant::now());

        assert_eq!(sm.state(), State::LocalActive);
        assert!(actions.contains(&Action::ReleaseAllModifiers));
        assert!(actions.contains(&Action::SetBeingDriven(false)));
    }

    #[test]
    fn reclaim_is_ignored_outside_being_driven() {
        let (mut sm, _, peer) = two_node_machine();
        sm.handle(Input::PeerHandshakeOk(peer), Instant::now());
        assert_eq!(sm.state(), State::LocalActive);

        let actions = sm.handle(Input::ReceivedReclaim, Instant::now());
        assert!(actions.is_empty());
        assert_eq!(sm.state(), State::LocalActive);
    }

    #[test]
    fn emergency_release_from_peer_returns_control_local() {
        for state in [State::RemoteActive, State::BeingDriven] {
            let (mut sm, _, peer) = two_node_machine();
            sm.force_state(state, Some(peer));

            let actions = sm.handle(Input::ReceivedEmergencyRelease, Instant::now());

            assert_eq!(sm.state(), State::LocalActive, "state was {state:?}");
            assert!(actions.contains(&Action::ReleaseAllModifiers));
            assert_eq!(
                actions.contains(&Action::SetBeingDriven(false)),
                state == State::BeingDriven,
                "SetBeingDriven(false) must fire iff we were actually BeingDriven, state {state:?}"
            );
        }
    }

    #[test]
    fn handoff_cooldown_blocks_immediate_reverse_handoff() {
        let (mut sm, _, peer) = two_node_machine();
        sm.handle(Input::PeerHandshakeOk(peer), Instant::now());
        let t0 = Instant::now();
        sm.handle(Input::CursorMoved(Point { x: 960, y: 540 }), t0);
        sm.handle(Input::CursorMoved(Point { x: 1919, y: 540 }), t0);
        assert_eq!(sm.state(), State::RemoteActive);

        // The peer pushes the cursor back onto its own screen and sends
        // `ReleaseBack` — control returns to us immediately (this path
        // isn't cooldown-gated; only the outward handoff is, per Tier 7.2).
        let actions = sm.handle(Input::ReceivedReleaseBack, t0);
        assert_eq!(sm.state(), State::LocalActive);
        assert!(actions.contains(&Action::SetSuppression(false)));

        // Immediately sliding back to the edge again, inside the 200ms
        // cooldown, must NOT trigger a second handoff.
        let actions = sm.handle(
            Input::CursorMoved(Point { x: 1919, y: 540 }),
            t0 + Duration::from_millis(50),
        );
        assert_eq!(sm.state(), State::LocalActive);
        assert!(!actions.iter().any(|a| matches!(a, Action::SendHandoff(_))));

        // After the cooldown elapses, the handoff can fire again.
        let actions = sm.handle(
            Input::CursorMoved(Point { x: 1919, y: 540 }),
            t0 + Duration::from_millis(250),
        );
        assert_eq!(sm.state(), State::RemoteActive, "actions were {actions:?}");
    }

    /// The Layout panel's edge-handoff tuning (Tier 8.1) is honoured: a
    /// longer configured cooldown keeps a reverse handoff blocked past the
    /// point the 200ms default would have allowed it.
    #[test]
    fn set_edge_settings_lengthens_the_handoff_cooldown() {
        let (mut sm, _, peer) = two_node_machine();
        sm.set_edge_settings(20, 1000);
        sm.handle(Input::PeerHandshakeOk(peer), Instant::now());
        let t0 = Instant::now();
        sm.handle(Input::CursorMoved(Point { x: 960, y: 540 }), t0);
        sm.handle(Input::CursorMoved(Point { x: 1919, y: 540 }), t0);
        assert_eq!(sm.state(), State::RemoteActive);
        sm.handle(Input::ReceivedReleaseBack, t0);
        assert_eq!(sm.state(), State::LocalActive);

        // 250ms in — past the default cooldown, still inside the 1s one.
        sm.handle(
            Input::CursorMoved(Point { x: 1919, y: 540 }),
            t0 + Duration::from_millis(250),
        );
        assert_eq!(sm.state(), State::LocalActive, "still cooling down");

        sm.handle(
            Input::CursorMoved(Point { x: 1919, y: 540 }),
            t0 + Duration::from_millis(1100),
        );
        assert_eq!(sm.state(), State::RemoteActive);
    }

    /// Reclaim now happens on the driven side: the machine being driven
    /// integrates the cursor it's injecting and, once the cursor has been
    /// pushed back out through the edge it entered on, hands control back.
    #[test]
    fn driven_side_hands_control_back_when_cursor_pushed_out_the_shared_edge() {
        // `local` sits to the RIGHT of `peer` here, so a handoff from
        // `peer` enters through `local`'s LEFT edge.
        let local = NodeId::new();
        let peer = NodeId::new();
        let mut layout = Layout::new();
        layout.set_placement(local, local_bounds());
        layout.set_placement(
            peer,
            Rect {
                x: -1920,
                y: 0,
                width: 1920,
                height: 1080,
            },
        );
        let mut sm = StateMachine::new(local, local_bounds(), layout);
        sm.handle(Input::PeerHandshakeOk(peer), Instant::now());

        let entry = crate::topology::EdgePoint {
            edge: crate::topology::Edge::Left,
            pos: 0.5,
        };
        let actions = sm.handle(Input::ReceivedHandoff { from: peer, entry }, Instant::now());
        assert_eq!(sm.state(), State::BeingDriven);
        // Inset 20px inside the left edge (default dead zone).
        assert!(matches!(actions[0], Action::WarpCursor { x: 20, .. }));

        // A tiny outward wobble before the cursor has come inward past the
        // inset must NOT be read as an exit.
        let actions = sm.handle(
            Input::DrivenCursorMoved(Point { x: -3, y: 540 }),
            Instant::now(),
        );
        assert!(actions.is_empty());
        assert_eq!(sm.state(), State::BeingDriven);

        // Cursor travels well inside our screen — arms the back-out detector.
        sm.handle(
            Input::DrivenCursorMoved(Point { x: 400, y: 540 }),
            Instant::now(),
        );
        assert_eq!(sm.state(), State::BeingDriven);

        // Now pushed back out through the LEFT (shared) edge -> hand back.
        let actions = sm.handle(
            Input::DrivenCursorMoved(Point { x: -1, y: 540 }),
            Instant::now(),
        );
        assert_eq!(sm.state(), State::LocalActive);
        assert!(actions.contains(&Action::SendReleaseBack));
        assert!(actions.contains(&Action::ReleaseAllModifiers));
        assert!(actions.contains(&Action::SetBeingDriven(false)));
    }

    #[test]
    fn driven_cursor_moving_out_a_different_edge_does_not_hand_back() {
        let (mut sm, _local, peer) = two_node_machine();
        sm.handle(Input::PeerHandshakeOk(peer), Instant::now());
        let entry = crate::topology::EdgePoint {
            edge: crate::topology::Edge::Left,
            pos: 0.5,
        };
        sm.handle(Input::ReceivedHandoff { from: peer, entry }, Instant::now());
        assert_eq!(sm.state(), State::BeingDriven);

        sm.handle(
            Input::DrivenCursorMoved(Point { x: 400, y: 540 }),
            Instant::now(),
        );
        // Straight out the RIGHT edge — not the edge it entered on.
        let actions = sm.handle(
            Input::DrivenCursorMoved(Point { x: 1919, y: 540 }),
            Instant::now(),
        );
        assert!(actions.is_empty());
        assert_eq!(sm.state(), State::BeingDriven);
    }

    #[test]
    fn received_release_back_only_acts_while_remote_active() {
        let (mut sm, _, peer) = two_node_machine();
        sm.handle(Input::PeerHandshakeOk(peer), Instant::now());
        // In LocalActive it's a no-op.
        assert!(
            sm.handle(Input::ReceivedReleaseBack, Instant::now())
                .is_empty()
        );
        assert_eq!(sm.state(), State::LocalActive);

        // From RemoteActive it returns control and drops suppression.
        sm.handle(Input::CursorMoved(Point { x: 960, y: 540 }), Instant::now());
        sm.handle(
            Input::CursorMoved(Point { x: 1919, y: 540 }),
            Instant::now(),
        );
        assert_eq!(sm.state(), State::RemoteActive);
        let actions = sm.handle(Input::ReceivedReleaseBack, Instant::now());
        assert_eq!(sm.state(), State::LocalActive);
        assert!(actions.contains(&Action::SetSuppression(false)));
        assert!(actions.contains(&Action::ReleaseAllModifiers));
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, Action::WarpCursor { .. }))
        );
    }

    #[test]
    fn no_handoff_across_an_edge_with_no_neighbor() {
        let (mut sm, _, peer) = two_node_machine();
        sm.handle(Input::PeerHandshakeOk(peer), Instant::now());

        // The peer is to the right; nothing is placed to the left.
        sm.handle(Input::CursorMoved(Point { x: 960, y: 540 }), Instant::now());
        let actions = sm.handle(Input::CursorMoved(Point { x: 0, y: 540 }), Instant::now());

        assert!(actions.is_empty());
        assert_eq!(sm.state(), State::LocalActive);
    }

    #[test]
    fn lock_toggle_prevents_handoff_and_unlock_restores_it() {
        let (mut sm, _, peer) = two_node_machine();
        sm.handle(Input::PeerHandshakeOk(peer), Instant::now());

        sm.handle(Input::LockToggled(true), Instant::now());
        assert_eq!(sm.state(), State::Locked);

        // Cursor slamming into the edge while locked must not hand off.
        sm.handle(Input::CursorMoved(Point { x: 960, y: 540 }), Instant::now());
        let actions = sm.handle(
            Input::CursorMoved(Point { x: 1919, y: 540 }),
            Instant::now(),
        );
        assert!(actions.is_empty());
        assert_eq!(sm.state(), State::Locked);

        sm.handle(Input::LockToggled(false), Instant::now());
        assert_eq!(sm.state(), State::LocalActive);
    }

    #[test]
    fn caps_lock_toggles_rather_than_holds() {
        // A press+release cycle should leave caps toggled ON, not
        // immediately cancel back out on the release — verified indirectly
        // via what ends up in a handoff's modifier snapshot.
        let (mut sm, _, peer) = two_node_machine();
        sm.handle(Input::PeerHandshakeOk(peer), Instant::now());
        sm.track_modifier(&InputEvent::KeyDown {
            code: KeyCode::CapsLock,
            repeat: false,
        });
        sm.track_modifier(&InputEvent::KeyUp {
            code: KeyCode::CapsLock,
        });
        sm.handle(Input::CursorMoved(Point { x: 960, y: 540 }), Instant::now());
        let actions = sm.handle(
            Input::CursorMoved(Point { x: 1919, y: 540 }),
            Instant::now(),
        );
        assert!(actions.contains(&Action::SendModifierState(Modifiers {
            caps: true,
            ..Modifiers::default()
        })));
    }

    // ───────────────────────── Multi-monitor seams ────────────────────────
    //
    // A machine with three monitors laid out left-to-right — 1920×1080,
    // 1080×1920 portrait, 3840×2160 — with the single-monitor peer placed
    // flush to the LEFT of the leftmost one. Only that first monitor's left
    // edge is a handoff surface.

    fn three_monitor_machine() -> (StateMachine, NodeId, NodeId) {
        let local = NodeId::new();
        let peer = NodeId::new();
        let union = Rect {
            x: 0,
            y: 0,
            width: 6840,
            height: 2160,
        };
        let mut layout = Layout::new();
        layout.set_placement(local, union);
        layout.set_placement(
            peer,
            Rect {
                x: -1920,
                y: 0,
                width: 1920,
                height: 1080,
            },
        );
        let mut sm = StateMachine::new(local, union, layout);
        sm.set_local_displays(vec![
            Rect {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            },
            Rect {
                x: 1920,
                y: 0,
                width: 1080,
                height: 1920,
            },
            Rect {
                x: 3000,
                y: 0,
                width: 3840,
                height: 2160,
            },
        ]);
        sm.set_peer_displays(vec![Rect {
            x: -1920,
            y: 0,
            width: 1920,
            height: 1080,
        }]);
        sm.handle(Input::PeerHandshakeOk(peer), Instant::now());
        (sm, local, peer)
    }

    #[test]
    fn interior_edge_between_local_monitors_does_not_hand_off() {
        let (mut sm, _, _) = three_monitor_machine();
        // Slide right off the first monitor into the portrait one — an
        // interior boundary at x≈1919. The old union-rectangle check would
        // have seen "not at the union's right edge" and done nothing here
        // too, but the point is it must ALSO do nothing at x=1919 where the
        // first monitor actually ends.
        sm.handle(Input::CursorMoved(Point { x: 900, y: 540 }), Instant::now());
        let actions = sm.handle(
            Input::CursorMoved(Point { x: 1919, y: 540 }),
            Instant::now(),
        );
        assert!(
            actions.is_empty(),
            "interior edge triggered a handoff: {actions:?}"
        );
        assert_eq!(sm.state(), State::LocalActive);
    }

    #[test]
    fn seam_monitor_outer_edge_hands_off_and_normalizes_against_that_monitor() {
        let (mut sm, local, _) = three_monitor_machine();
        // Leave low on the first (1080-tall) monitor's left edge — clear of
        // the 20px corner dead zone.
        sm.handle(Input::CursorMoved(Point { x: 40, y: 950 }), Instant::now());
        let actions = sm.handle(Input::CursorMoved(Point { x: 0, y: 1000 }), Instant::now());
        assert_eq!(sm.state(), State::RemoteActive);
        let entry = actions
            .iter()
            .find_map(|a| match a {
                Action::SendHandoff(e) => Some(*e),
                _ => None,
            })
            .expect("a handoff was sent");
        assert_eq!(entry.edge, crate::topology::Edge::Right);
        // 1000/1080 ≈ 0.93 down the seam monitor — NOT 1000/2160 ≈ 0.46 as
        // it would be if normalized against the whole virtual desktop.
        assert!(
            (entry.pos - 0.926).abs() < 0.02,
            "pos {} — normalized against the union, not the seam monitor",
            entry.pos
        );
        assert_eq!(
            sm.remembered_cursor.get(&local),
            Some(&Point { x: 0, y: 1000 })
        );
    }

    #[test]
    fn exiting_being_driven_through_a_seam_monitor_releases_modifiers() {
        // The non-negotiable invariant on the multi-monitor path: a handoff
        // onto the seam monitor, then a push back out, must still release
        // every modifier.
        let (mut sm, _, peer) = three_monitor_machine();
        let warp = sm.handle(
            Input::ReceivedHandoff {
                from: peer,
                entry: crate::topology::EdgePoint {
                    edge: crate::topology::Edge::Left,
                    pos: 0.5,
                },
            },
            Instant::now(),
        );
        assert_eq!(sm.state(), State::BeingDriven);
        // Lands inside the seam monitor's left edge (x≈20), and at y=540 —
        // half of the 1080-tall seam monitor, not half of the 2160 union.
        assert_eq!(warp[0], Action::WarpCursor { x: 20, y: 540 });

        // Arm by moving well inside, then push back out through the left edge.
        sm.handle(
            Input::DrivenCursorMoved(Point { x: 300, y: 540 }),
            Instant::now(),
        );
        let out = sm.handle(
            Input::DrivenCursorMoved(Point { x: 0, y: 540 }),
            Instant::now(),
        );
        assert_eq!(sm.state(), State::LocalActive);
        assert!(out.contains(&Action::ReleaseAllModifiers), "got {out:?}");
        assert!(out.contains(&Action::SendReleaseBack));
    }

    #[test]
    fn reclaim_fires_when_the_driven_cursor_backs_out_from_off_the_seam_monitor() {
        // Regression: while BeingDriven the peer roams onto a taller monitor
        // (y past the 1080-tall seam monitor), then pushes back out the
        // entry edge. Back-out detection must still fire — using the narrow
        // seam monitor for the perpendicular axis made the corner check
        // trip on a negative distance and control got stuck on this side.
        let (mut sm, _, peer) = three_monitor_machine();
        sm.handle(
            Input::ReceivedHandoff {
                from: peer,
                entry: crate::topology::EdgePoint {
                    edge: crate::topology::Edge::Left,
                    pos: 0.5,
                },
            },
            Instant::now(),
        );
        assert_eq!(sm.state(), State::BeingDriven);

        // Roam onto the portrait monitor (y = 1600, well below the seam
        // monitor's 1080) — this arms the back-out detector.
        sm.handle(
            Input::DrivenCursorMoved(Point { x: 2500, y: 1600 }),
            Instant::now(),
        );
        // Push back out through the left edge at that same y.
        let out = sm.handle(
            Input::DrivenCursorMoved(Point { x: 0, y: 1600 }),
            Instant::now(),
        );
        assert_eq!(sm.state(), State::LocalActive, "reclaim never fired");
        assert!(out.contains(&Action::SendReleaseBack), "got {out:?}");
        assert!(out.contains(&Action::ReleaseAllModifiers));
    }
}
