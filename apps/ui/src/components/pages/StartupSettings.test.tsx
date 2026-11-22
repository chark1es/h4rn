// @vitest-environment jsdom

import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
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
        throw new Error("plugin:autostart|is_enabled not found");
      },
    });
    render(<StartupSettings />);

    // The tray row still proves the group rendered.
    expect(await screen.findByText("Keep running in tray")).toBeTruthy();
    await waitFor(() => expect(screen.queryByText("Open at login")).toBeNull());
  });

  it("keeps the row when the plugin is present but denies the capability", async () => {
    bridge.current = shell({
      autostartEnabled: async () => {
        throw new Error("plugin:autostart|is_enabled is not allowed");
      },
    });
    render(<StartupSettings />);

    // A capability denial is not a missing plugin: the build can offer the
    // setting, so the row stays and says what went wrong.
    expect(await screen.findByText("Open at login")).toBeTruthy();
    expect(screen.queryByRole("switch", { name: "Open at login" })).toBeNull();
    expect((await screen.findByRole("alert")).textContent).toContain("is not allowed");
  });

  it("shows any other read failure as an error line, not as a missing plugin", async () => {
    bridge.current = shell({
      autostartEnabled: async () => {
        throw new Error("permission denied by the login item service");
      },
    });
    render(<StartupSettings />);

    // The row stays, because this build has the plugin; only the switch is
    // withheld while there is no state to show.
    expect(await screen.findByText("Open at login")).toBeTruthy();
    expect(screen.queryByRole("switch", { name: "Open at login" })).toBeNull();
    expect((await screen.findByRole("alert")).textContent).toContain(
      "permission denied by the login item service",
    );
  });

  it("keeps the autostart read error when the tray setting changes", async () => {
    bridge.current = shell({
      autostartEnabled: async () => {
        throw new Error("permission denied by the login item service");
      },
      setKeepInTray: async () => ({ keep_in_tray: true, tray_available: true }),
    });
    render(<StartupSettings />);

    expect((await screen.findByRole("alert")).textContent).toContain(
      "permission denied by the login item service",
    );

    const tray = await screen.findByRole("switch", { name: "Keep running in tray" });
    await waitFor(() => expect(tray.hasAttribute("disabled")).toBe(false));
    fireEvent.click(tray);
    await waitFor(() =>
      expect(
        screen.getByRole("switch", { name: "Keep running in tray" }).getAttribute("aria-checked"),
      ).toBe("true"),
    );

    // The read failure belongs to the Open at login row; toggling the tray is
    // a different setting and must not clear it.
    expect(screen.getByRole("alert").textContent).toContain(
      "permission denied by the login item service",
    );
  });
});
