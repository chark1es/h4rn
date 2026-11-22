/**
 * The desktop shell's startup switches, as the view sees them.
 *
 * "Open at login" is the autostart plugin's own OS registration. "Keep running
 * in tray" lives in the shell (`apps/desktop/src-tauri/src/settings.rs`)
 * because the window's close handler has to know it before the UI loads. The
 * browser and mobile builds have neither switch, so this returns `null` there.
 */

import { secretShell } from "@/lib/platform/secrets";
import { tauriInternals } from "@/lib/platform/tauri";

export type ShellSettings = {
  keep_in_tray: boolean;
};

export type DesktopShellBridge = {
  autostartEnabled(): Promise<boolean>;
  setAutostart(enabled: boolean): Promise<void>;
  shellSettings(): Promise<ShellSettings>;
  setKeepInTray(enabled: boolean): Promise<ShellSettings>;
};

/** The desktop shell bridge, or `null` in the browser and on mobile. */
export function desktopShellBridge(): DesktopShellBridge | null {
  if (typeof window === "undefined" || secretShell() !== "desktop") return null;
  const shell = tauriInternals();
  if (!shell) return null;
  return {
    autostartEnabled: () => shell.invoke<boolean>("plugin:autostart|is_enabled"),
    setAutostart: async (enabled) => {
      await shell.invoke(`plugin:autostart|${enabled ? "enable" : "disable"}`);
    },
    shellSettings: () => shell.invoke<ShellSettings>("shell_settings"),
    setKeepInTray: (enabled) => shell.invoke<ShellSettings>("shell_set_keep_in_tray", { enabled }),
  };
}
