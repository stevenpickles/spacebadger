// Typed wrappers around backend commands. Types come from sb-protocol via ts-rs.
import { invoke } from "@tauri-apps/api/core";
import type { AppInfo } from "./protocol/AppInfo";

export const EXPECTED_PROTOCOL_VERSION = 1;

export function appInfo(): Promise<AppInfo> {
  return invoke<AppInfo>("app_info");
}
