import { invoke } from "@tauri-apps/api/core";

export interface DesktopSettings {
  close_action: "tray" | "exit";
  stop_agent_on_exit: boolean;
}
export interface DesktopSettingsClient {
  get(): Promise<DesktopSettings>;
  set(settings: DesktopSettings): Promise<DesktopSettings>;
}
function decodeSettings(value: unknown): DesktopSettings {
  if (typeof value !== "object" || value === null || !("close_action" in value) || !("stop_agent_on_exit" in value)
    || (value.close_action !== "tray" && value.close_action !== "exit") || typeof value.stop_agent_on_exit !== "boolean") {
    throw new Error("程序配置返回无效，请重新读取。");
  }
  return { close_action: value.close_action, stop_agent_on_exit: value.stop_agent_on_exit };
}
async function call(command: string, args?: Record<string, unknown>): Promise<DesktopSettings> {
  if (typeof window === "undefined" || !("__TAURI_INTERNALS__" in window)) {
    throw new Error("请从 Windows 桌面应用读取程序配置。");
  }
  try { return decodeSettings(await invoke(command, args)); }
  catch (error) {
    if (error instanceof Error) throw error;
    if (typeof error === "string") throw new Error(error);
    if (typeof error === "object" && error !== null && "message" in error && typeof error.message === "string") throw new Error(error.message);
    throw new Error("程序配置操作失败，请重试。");
  }
}
export const nativeDesktopSettings: DesktopSettingsClient = {
  get: () => call("get_desktop_settings"),
  set: settings => call("set_desktop_settings", { settings }),
};
