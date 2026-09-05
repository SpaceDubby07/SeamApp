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

export interface RemapRule {
  from: string;
  to: string;
}

export interface RemapTable {
  rules: RemapRule[];
  invert_scroll_x: boolean;
  invert_scroll_y: boolean;
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

// Mirrors seam_core::session::SessionEvent, tagged with `#[serde(tag =
// "type")]` on the Rust side.
export type SessionEvent =
  | { type: "PeerScreenConfig"; displays: Display[]; virtual_bounds: Rect }
  | { type: "LayoutChanged"; peer_bounds: Rect }
  | { type: "OfferReceived"; transfer_id: TransferId; manifest: FileManifest }
  | { type: "Progress"; transfer_id: TransferId; bytes_done: number; total: number }
  | { type: "Rejected"; transfer_id: TransferId; reason: string }
  | { type: "Completed"; transfer_id: TransferId; path: string }
  | { type: "Failed"; transfer_id: TransferId; reason: string };

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
