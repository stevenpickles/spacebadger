// Typed wrappers around backend commands. Types come from sb-protocol via ts-rs.
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import type { AppInfo } from "./protocol/AppInfo";
import type { LayoutRequest } from "./protocol/LayoutRequest";
import type { NodeDetails } from "./protocol/NodeDetails";
import type { ScanStarted } from "./protocol/ScanStarted";
import type { ScanStatus } from "./protocol/ScanStatus";

export const EXPECTED_PROTOCOL_VERSION = 2;

/** Must match `sb_protocol::SCAN_STATUS_EVENT`. */
const SCAN_STATUS_EVENT = "scan-status";

export function appInfo(): Promise<AppInfo> {
  return invoke<AppInfo>("app_info");
}

/** Opens the native folder picker and starts scanning; `null` if dismissed. */
export function scanChoose(): Promise<ScanStarted | null> {
  return invoke<ScanStarted | null>("scan_choose");
}

export function scanRefresh(): Promise<ScanStarted | null> {
  return invoke<ScanStarted | null>("scan_refresh");
}

export function scanCancel(generation: number): Promise<void> {
  return invoke("scan_cancel", { generation });
}

export function scanStatus(): Promise<ScanStatus | null> {
  return invoke<ScanStatus | null>("scan_status");
}

/** Binary layout; decode with `decodeLayout`. */
export function requestLayout(request: LayoutRequest): Promise<ArrayBuffer> {
  return invoke<ArrayBuffer>("layout", { request });
}

export function nodeDetails(generation: number, node: number): Promise<NodeDetails> {
  return invoke<NodeDetails>("node_details", { generation, node });
}

/** Shows the item selected in Explorer, Finder, or the desktop's file manager. */
export function reveal(generation: number, node: number): Promise<void> {
  return invoke("reveal", { generation, node });
}

/** What the system file manager is called, for menu labels. */
export function fileManagerName(os: string): string {
  if (os === "windows") return "Explorer";
  if (os === "macos") return "Finder";
  return "file manager";
}

export function onScanStatus(handler: (status: ScanStatus) => void): Promise<UnlistenFn> {
  return listen<ScanStatus>(SCAN_STATUS_EVENT, (event) => handler(event.payload));
}
