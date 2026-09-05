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
    Edge, EdgePoint, Layout, NodeId, Point, Rect, compute_entry_point, detect_edge_crossing,
    detect_edge_reclaim,
};

/// How long after a handoff before the reverse handoff is allowed to fire.
/// Without this, the boundary flickers (Tier 7.2).
const HANDOFF_COOLDOWN: Duration = Duration::from_millis(200);

/// Default corner dead zone, in pixels, so hitting a corner UI element
/// doesn't trigger an accidental handoff (Tier 7.2).
const DEFAULT_CORNER_DEAD_ZONE_PX: u32 = 20;

/// How far (in pixels) the peer-driven cursor must travel inward from the
/// edge it entered on before a push back out through that same edge counts
/// as "give control back" rather than entry jitter.
///
/// Reclaim now happens on the machine BEING driven (see
/// [`StateMachine::on_driven_cursor_moved`] and
/// `ControlMessage::ReleaseBack`), not by the driver watching its own
/// suppressed cursor — that earlier local-side approach reclaimed on any
/// few-pixel wobble right after a handoff and, on macOS (where a suppressed
/// cursor keeps physically moving), fired constantly, producing the
/// "it never leaves either screen, keeps re-grabbing" behaviour.
///
/// The driven cursor is warped exactly onto the shared edge on entry, so
/// without an arm step its very first outward jitter sample would look like
/// a deliberate exit. Requiring `DRIVEN_BACKOUT_ARM_PX` of inward travel
/// first makes the exit unambiguous while still reading as instant to a
/// human pushing back on purpose.
const DRIVEN_BACKOUT_ARM_PX: i32 = 12;

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
    /// The peer we're either driving (`RemoteActive`) or being driven by
    /// (`BeingDriven`). `None` in every other state.
    peer: Option<NodeId>,
    /// While `BeingDriven`: which local edge the peer's cursor entered
    /// through (the far side of the shared boundary), i.e. the edge it has
    /// to cross back out of to hand control back. `None` in every other
    /// state.
    driven_entry_edge: Option<Edge>,
    /// While `BeingDriven`: the last integrated position of the
    /// peer-driven cursor, in local pixels, for back-out edge detection.
    last_driven_cursor: Option<Point>,
    /// While `BeingDriven`: set once the driven cursor has moved far enough
    /// inward from `driven_entry_edge` (`DRIVEN_BACKOUT_ARM_PX`) that a
    /// subsequent push back out through that edge is a deliberate exit and
    /// not entry jitter.
    driven_backout_armed: bool,
    last_cursor: Option<Point>,
    last_handoff_at: Option<Instant>,
    held_modifiers: Modifiers,
    /// Tier 7.7: remembered per-peer cursor position, so a reclaim warps
    /// back to where you left off rather than dumping you at the edge.
    remembered_cursor: HashMap<NodeId, Point>,
    corner_dead_zone_px: u32,
}

impl StateMachine {
    /// Creates a disconnected state machine for `local_node`, with
    /// `local_bounds` as its virtual-desktop bounds and `layout` as the
    /// shared canvas used to resolve which peer sits across a given edge.
    #[must_use]
    pub fn new(local_node: NodeId, local_bounds: Rect, layout: Layout) -> Self {
        Self {
            state: State::Disconnected,
            local_node,
            local_bounds,
            layout,
            peer: None,
            driven_entry_edge: None,
            last_driven_cursor: None,
            driven_backout_armed: false,
            last_cursor: None,
            last_handoff_at: None,
            held_modifiers: Modifiers::default(),
            remembered_cursor: HashMap::new(),
            corner_dead_zone_px: DEFAULT_CORNER_DEAD_ZONE_PX,
        }
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
            Input::ReceivedReleaseBack => self.on_received_release_back(),
            Input::ReceivedEmergencyRelease => self.on_emergency_release(),
            Input::ConnectionLost => self.on_connection_lost(),
        }
    }

    fn on_handshake_ok(&mut self, peer: NodeId) -> Vec<Action> {
        if self.state != State::Disconnected {
            return Vec::new();
        }
        self.peer = Some(peer);
        self.state = State::LocalActive;
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
        let prev = self.last_driven_cursor.replace(pos);

        if !self.driven_backout_armed {
            // Arm only once the cursor is unambiguously inside our screen.
            if detect_edge_reclaim(self.local_bounds, edge, pos, DRIVEN_BACKOUT_ARM_PX) {
                self.driven_backout_armed = true;
            }
            return Vec::new();
        }

        let Some(prev) = prev else {
            return Vec::new();
        };
        if detect_edge_crossing(self.local_bounds, prev, pos, self.corner_dead_zone_px)
            != Some(edge)
        {
            return Vec::new();
        }

        // Pushed back out through the shared edge — control returns to the
        // peer. Release every modifier we were holding on its behalf
        // (Tier 7.1: mandatory on every exit from `BeingDriven`).
        self.state = State::LocalActive;
        self.peer = None;
        self.clear_driven_tracking();
        vec![Action::SendReleaseBack, Action::ReleaseAllModifiers]
    }

    /// Resets the `BeingDriven` cursor-tracking bookkeeping. Called on
    /// every transition out of `BeingDriven`.
    fn clear_driven_tracking(&mut self) {
        self.driven_entry_edge = None;
        self.last_driven_cursor = None;
        self.driven_backout_armed = false;
    }

    fn try_handoff(&mut self, prev: Option<Point>, pos: Point, now: Instant) -> Vec<Action> {
        let Some(prev) = prev else {
            return Vec::new();
        };
        if let Some(cooldown_started) = self.last_handoff_at
            && now.duration_since(cooldown_started) < HANDOFF_COOLDOWN
        {
            return Vec::new();
        }
        let Some(edge) =
            detect_edge_crossing(self.local_bounds, prev, pos, self.corner_dead_zone_px)
        else {
            return Vec::new();
        };
        let Some(peer) = self.peer else {
            return Vec::new();
        };
        // In v1 there's exactly one peer, but this still goes through the
        // real layout graph rather than assuming "the" peer is across
        // every edge — Tier 15's note on keeping a third machine cheap.
        if self.layout.neighbor(self.local_node, edge) != Some(peer) {
            return Vec::new();
        }

        let entry = compute_entry_point(self.local_bounds, pos, edge);
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
    fn on_received_release_back(&mut self) -> Vec<Action> {
        if self.state != State::RemoteActive {
            return Vec::new();
        }
        self.state = State::LocalActive;
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
                vec![Action::SendEmergencyRelease, Action::ReleaseAllModifiers]
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

    // Screen coordinates never approach the range where an f32 mantissa or
    // an i32 truncation would matter — see the equivalent allow on
    // `compute_entry_point` in topology.rs.
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
    fn on_received_handoff(&mut self, from: NodeId, entry: EdgePoint) -> Vec<Action> {
        self.state = State::BeingDriven;
        self.peer = Some(from);
        let bounds = self.local_bounds;
        let (x, y) = match entry.edge {
            Edge::Left => (
                bounds.x,
                bounds.y + (entry.pos * bounds.height as f32) as i32,
            ),
            Edge::Right => (
                bounds.x + bounds.width.cast_signed() - 1,
                bounds.y + (entry.pos * bounds.height as f32) as i32,
            ),
            Edge::Top => (
                bounds.x + (entry.pos * bounds.width as f32) as i32,
                bounds.y,
            ),
            Edge::Bottom => (
                bounds.x + (entry.pos * bounds.width as f32) as i32,
                bounds.y + bounds.height.cast_signed() - 1,
            ),
        };
        // Arm the back-out detector fresh: the cursor is being placed
        // exactly on `entry.edge`, and it has to travel `DRIVEN_BACKOUT_ARM_PX`
        // inward before pushing back out through that edge counts as a
        // deliberate hand-back (`on_driven_cursor_moved`).
        self.driven_entry_edge = Some(entry.edge);
        self.last_driven_cursor = Some(Point { x, y });
        self.driven_backout_armed = false;
        vec![Action::WarpCursor { x, y }]
    }

    fn on_received_reclaim(&mut self) -> Vec<Action> {
        if self.state != State::BeingDriven {
            return Vec::new();
        }
        self.state = State::LocalActive;
        self.peer = None;
        self.clear_driven_tracking();
        vec![Action::ReleaseAllModifiers]
    }

    fn on_emergency_release(&mut self) -> Vec<Action> {
        match self.state {
            State::RemoteActive | State::BeingDriven => {
                self.state = State::LocalActive;
                self.peer = None;
                self.clear_driven_tracking();
                vec![Action::SetSuppression(false), Action::ReleaseAllModifiers]
            }
            State::LocalActive | State::Disconnected | State::Locked => Vec::new(),
        }
    }

    fn on_connection_lost(&mut self) -> Vec<Action> {
        if self.state == State::Disconnected {
            return Vec::new();
        }
        let was_active_or_driven = matches!(self.state, State::RemoteActive | State::BeingDriven);
        self.state = State::Disconnected;
        self.peer = None;
        self.clear_driven_tracking();

        let mut actions = Vec::new();
        if was_active_or_driven {
            actions.push(Action::SetSuppression(false));
            actions.push(Action::ReleaseAllModifiers);
        }
        actions.push(Action::StartReconnect);
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
            assert!(actions.contains(&Action::StartReconnect));
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
    fn received_handoff_warps_cursor_to_the_entry_point() {
        let (mut sm, _, peer) = two_node_machine();
        let entry = crate::topology::EdgePoint {
            edge: crate::topology::Edge::Left,
            pos: 0.25,
        };

        let actions = sm.handle(Input::ReceivedHandoff { from: peer, entry }, Instant::now());

        assert_eq!(sm.state(), State::BeingDriven);
        assert_eq!(
            actions[0],
            Action::WarpCursor {
                x: 0,
                y: (0.25 * 1080.0) as i32
            }
        );
    }

    #[test]
    fn reclaim_from_being_driven_releases_modifiers_and_returns_local() {
        let (mut sm, _, peer) = two_node_machine();
        sm.force_state(State::BeingDriven, Some(peer));

        let actions = sm.handle(Input::ReceivedReclaim, Instant::now());

        assert_eq!(sm.state(), State::LocalActive);
        assert!(actions.contains(&Action::ReleaseAllModifiers));
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
        assert!(matches!(actions[0], Action::WarpCursor { x: 0, .. }));

        // A tiny outward wobble before the cursor has come inward at all
        // must NOT be read as an exit (the cursor was warped onto the edge).
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
}
