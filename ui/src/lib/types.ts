// TS mirrors of the Rust types that cross the Tauri IPC boundary
// (crates/seam-core/src/{config,protocol}.rs and
// crates/seam-app/src/{state,connect}.rs). Keep these in sync by hand —
// there's no codegen wired up yet.

export type NodeId = string; // a UUID, serialized transparently by serde

export type OsKind = "MacOs" | "Windows";

export type AcceptPolicy = "Ask" | "AlwaysAccept" | "AlwaysDeny";

export interface PairedPeer {
  node_id: NodeId;
  // seam_core::net::tls::Fingerprint serializes as a 64-char hex string.
  fingerprint: string;
}

export interface Config {
  node_id: NodeId;
  display_name: string;
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
  | { type: "Status"; rtt_micros: number | null };

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
