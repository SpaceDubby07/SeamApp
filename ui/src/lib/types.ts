// TS mirrors of the Rust types that cross the Tauri IPC boundary
// (crates/seam-core/src/{config,topology,protocol}.rs and
// crates/seam-app/src/{state,connect}.rs). Keep these in sync by hand —
// there's no codegen wired up yet.

export type NodeId = string; // a UUID, serialized transparently by serde

export interface Rect {
  x: number;
  y: number;
  width: number;
  height: number;
}

export type OsKind = "MacOs" | "Windows";

export interface Display {
  id: number;
  bounds: Rect;
  scale_factor: number;
  is_primary: boolean;
}

export type AcceptPolicy = "Ask" | "AlwaysAccept" | "AlwaysDeny";

// A physical key code. Named variants of seam_core::protocol::KeyCode
// serialize as their bare name ("LeftCtrl", "Digit4", …); the catch-all
// `Unknown(u32)` serializes as `{ Unknown: n }` — the remap editor only
// deals in named keys, so this is typed loosely as string.
export type KeyCode = string;

// Mirrors seam_core::remap::RemapTableRepr (the on-wire / on-disk shape of
// RemapTable — its `rules` HashMap travels as a list of pairs).
export interface RemapRule {
  physical: KeyCode;
  injected: KeyCode;
}

export interface RemapTable {
  rules: RemapRule[];
  invert_scroll_y: boolean;
  invert_scroll_x: boolean;
}

export interface PairedPeer {
  node_id: NodeId;
  fingerprint: number[];
}

export interface Config {
  node_id: NodeId;
  display_name: string;
  remap: RemapTable;
  clipboard_max_bytes: number;
  paired_peer: PairedPeer | null;
  accept_policy: AcceptPolicy;
  download_dir: string | null;
}

export interface DiscoveredPeer {
  node_id: NodeId;
  display_name: string;
  os: OsKind;
  addr: string;
  control_port: number;
}

export interface FileManifest {
  name: string;
  size: number;
  hash: number[];
  chunk_size: number;
  modified: number | null;
}

export type TransferId = string; // a UUID

// Which machine is driving input right now — mirrors
// seam_core::session::LinkStatus.
export type LinkStatus = "Local" | "Driving" | "Driven";

// Mirrors seam_core::session::SessionEvent, tagged with `#[serde(tag =
// "type")]` on the Rust side.
export type SessionEvent =
  | { type: "PeerScreenConfig"; displays: Display[]; virtual_bounds: Rect }
  | { type: "LayoutChanged"; peer_bounds: Rect }
  | { type: "OfferReceived"; transfer_id: TransferId; manifest: FileManifest }
  | {
      type: "Progress";
      transfer_id: TransferId;
      name: string;
      incoming: boolean;
      bytes_done: number;
      total: number;
    }
  | { type: "Rejected"; transfer_id: TransferId; reason: string }
  | { type: "Completed"; transfer_id: TransferId; path: string }
  | { type: "Failed"; transfer_id: TransferId; reason: string }
  | { type: "Status"; link: LinkStatus; rtt_micros: number | null };

export interface ConnectedInfo {
  peer_display_name: string;
}

// Mirrors seam_app_lib::logbuf::LogLine.
export type LogLevel = "TRACE" | "DEBUG" | "INFO" | "WARN" | "ERROR";

export interface LogLine {
  seq: number;
  ts_millis: number;
  level: LogLevel;
  target: string;
  message: string;
}
