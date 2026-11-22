// @vitest-environment jsdom

import { cleanup, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import type { DesktopShellBridge } from "@/lib/platform/desktopShell";

const bridge = vi.hoisted(() => ({ current: null as DesktopShellBridge | null }));

vi.mock("@/lib/platform/desktopShell", () => ({
  desktopShellBridge: () => bridge.current,
}));

import { StartupSettings } from "./SettingsPage";

afterEach(() => {
  cleanup();
  bridge.current = null;
});

function shell(patch: Partial<DesktopShellBridge> = {}): DesktopShellBridge {
  return {
    autostartEnabled: async () => true,
    setAutostart: async () => undefined,
    shellSettings: async () => ({ keep_in_tray: false, tray_available: true }),
    setKeepInTray: async () => ({ keep_in_tray: false, tray_available: true }),
    ...patch,
  };
}

describe("Startup settings", () => {
  it("shows the real login-item state from the OS", async () => {
    bridge.current = shell({ autostartEnabled: async () => true });
    render(<StartupSettings />);

    const toggle = await screen.findByRole("switch", { name: "Open at login" });
    expect(toggle.getAttribute("aria-checked")).toBe("true");
  });

  it("does not show a made-up off switch while the state loads", async () => {
    bridge.current = shell({ autostartEnabled: () => new Promise<boolean>(() => undefined) });
    render(<StartupSettings />);

    expect(screen.getByText("Checking…")).toBeTruthy();
    expect(screen.queryByRole("switch", { name: "Open at login" })).toBeNull();
  });

  it("hides the row when the shell has no autostart plugin", async () => {
    bridge.current = shell({
      autostartEnabled: async () => {
        throw new Error("plugin:autostart|is_enabled is not allowed");
      },
    });
    render(<StartupSettings />);

    // The tray row still proves the group rendered.
    expect(await screen.findByText("Keep running in tray")).toBeTruthy();
    await waitFor(() => expect(screen.queryByText("Open at login")).toBeNull());
  });
});
