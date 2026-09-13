// Every Tauri command/event the frontend uses, in one place with real
// types. If a Rust signature changes, this file is where TypeScript will
// complain (Tier 8.2's IPC contract convention).

import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { revealItemInDir } from "@tauri-apps/plugin-opener";
import type {
  Config,
  ConnectedInfo,
  DiscoveredPeer,
  LogLine,
  SessionEvent,
} from "./types";

// ── Commands (UI -> Rust) ──
export const getConfig = () => invoke<Config>("get_config");
export const setDisplayName = (name: string) =>
  invoke<void>("set_display_name", { name });
/** Drops the pinned pairing so the next connection re-pairs. */
export const forgetPeer = () => invoke<void>("forget_peer");
export const listDiscoveredPeers = () =>
  invoke<DiscoveredPeer[]>("list_discovered_peers");
export const connectToPeer = (addr: string) =>
  invoke<void>("connect_to_peer", { addr });
export const confirmPairing = (accept: boolean) =>
  invoke<void>("confirm_pairing", { accept });
export const sendFile = (path: string) => invoke<void>("send_file", { path });
export const respondToOffer = (transferId: string, accept: boolean) =>
  invoke<void>("respond_to_offer", { transferId, accept });
export const cancelTransfer = (transferId: string) =>
  invoke<void>("cancel_transfer", { transferId });
export const disconnect = () => invoke<void>("disconnect");

// ── Logs ──
export const getLogs = (afterSeq?: number) =>
  invoke<LogLine[]>("get_logs", { afterSeq: afterSeq ?? null });
export const clearLogs = () => invoke<void>("clear_logs");
/** Writes the log buffer to a .txt file and returns its full path. */
export const exportLogs = () => invoke<string>("export_logs");
/** Opens the OS file manager with `path` selected. */
export const revealPath = (path: string) => revealItemInDir(path);

// ── Events (Rust -> UI) ──
export const onPeersChanged = (
  handler: (peers: DiscoveredPeer[]) => void,
): Promise<UnlistenFn> =>
  listen<DiscoveredPeer[]>("peers-changed", (e) => handler(e.payload));

export const onPairingRequested = (
  handler: (code: string) => void,
): Promise<UnlistenFn> =>
  listen<string>("pairing-requested", (e) => handler(e.payload));

export const onConnected = (
  handler: (info: ConnectedInfo) => void,
): Promise<UnlistenFn> =>
  listen<ConnectedInfo>("connected", (e) => handler(e.payload));

export const onDisconnected = (handler: () => void): Promise<UnlistenFn> =>
  listen<void>("disconnected", () => handler());

/** The connection dropped and the app is retrying with backoff. A
 * following `connected` means it succeeded; `disconnected` means it gave
 * up / the user cancelled. */
export const onReconnecting = (handler: () => void): Promise<UnlistenFn> =>
  listen<void>("reconnecting", () => handler());

export const onSessionEvent = (
  handler: (event: SessionEvent) => void,
): Promise<UnlistenFn> =>
  listen<SessionEvent>("session-event", (e) => handler(e.payload));
