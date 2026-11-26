// @vitest-environment jsdom

import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const { request } = vi.hoisted(() => ({ request: vi.fn() }));

vi.mock("@/lib/store/appStore", () => ({
  getClient: () => ({ request }),
  pushToast: vi.fn(),
  useApp: (selector: (state: unknown) => unknown) => selector({ connection: { status: "online" } }),
}));

import { clientProto, H4rnRequestError } from "@/lib/client/errors";
import { setLocalePreference } from "@/lib/i18n";
import { resetAccessStore } from "../access/store";
import { resetRewindSettingsForTests } from "../rewind/settings";
import { GuestSettingsForm } from "./GuestSettings";
import { resetGuestStoreForTests } from "./guestStore";

beforeEach(() => {
  resetGuestStoreForTests();
  resetAccessStore();
  resetRewindSettingsForTests();
  request.mockReset();
  request.mockImplementation(
    (body: { type: string; enabled?: boolean; request?: { type: string; enabled?: boolean } }) => {
      if (body.type === "guest_settings" || body.type === "set_guest_settings") {
        return Promise.resolve({ type: "guest_settings", settings: { enabled: false } });
      }
      if (body.type === "get_rewind_settings" || body.type === "set_rewind_settings_enabled") {
        return Promise.resolve({
          type: "rewind_settings",
          settings: { files_enabled: body.enabled === true },
        });
      }
      if (body.type !== "mcp") throw new Error(`unexpected request ${body.type}`);
      switch (body.request?.type) {
        case "outside_agent_settings":
          return Promise.resolve({ type: "outside_agent_settings", settings: { enabled: false } });
        case "set_outside_agent_settings_enabled":
          return Promise.resolve({
            type: "outside_agent_settings",
            settings: { enabled: body.request.enabled },
          });
        default:
          throw new Error(`unexpected MCP request ${body.request?.type}`);
      }
    },
  );
});

afterEach(async () => {
  cleanup();
  await setLocalePreference("system");
  resetGuestStoreForTests();
  resetAccessStore();
  resetRewindSettingsForTests();
});

describe("Settings → Labs", () => {
  it("keeps outside agents off by default and turns them on from the switch", async () => {
    render(<GuestSettingsForm />);
    const toggle = (await screen.findByRole("switch", {
      name: "Outside agents (OAuth)",
    })) as HTMLButtonElement;
    expect(toggle.getAttribute("aria-checked")).toBe("false");

    fireEvent.click(toggle);
    await waitFor(() =>
      expect(
        screen.getByRole("switch", { name: "Outside agents (OAuth)" }).getAttribute("aria-checked"),
      ).toBe("true"),
    );
    expect(request).toHaveBeenCalledWith({
      type: "mcp",
      request: { type: "set_outside_agent_settings_enabled", enabled: true },
    });
  });

  it("keeps the guest-link consequences in the Sharing group", async () => {
    render(<GuestSettingsForm />);
    const sharing = (await screen.findByRole("heading", { name: "Sharing" })).closest(".q-group");
    expect(sharing?.textContent).toContain(
      "Turning guest links off ends every existing guest link",
    );
  });

  it("keeps rewind file restore off by default and turns it on from the switch", async () => {
    render(<GuestSettingsForm />);
    const toggle = (await screen.findByRole("switch", {
      name: "Rewind files with Edit from here",
    })) as HTMLButtonElement;
    expect(toggle.getAttribute("aria-checked")).toBe("false");

    fireEvent.click(toggle);
    await waitFor(() =>
      expect(
        screen
          .getByRole("switch", { name: "Rewind files with Edit from here" })
          .getAttribute("aria-checked"),
      ).toBe("true"),
    );
    expect(request).toHaveBeenCalledWith({
      type: "set_rewind_settings_enabled",
      enabled: true,
    });
  });

  it("shows a failed outside-agent setting load under its switch", async () => {
    request.mockImplementation((body: { type: string; request?: { type: string } }) => {
      if (body.type === "guest_settings") {
        return Promise.resolve({ type: "guest_settings", settings: { enabled: false } });
      }
      if (body.type === "get_rewind_settings") {
        return Promise.resolve({ type: "rewind_settings", settings: { files_enabled: false } });
      }
      if (body.type === "mcp" && body.request?.type === "outside_agent_settings") {
        return Promise.reject(new Error("daemon unavailable"));
      }
      throw new Error(`unexpected request ${body.type}`);
    });
    render(<GuestSettingsForm />);
    expect(await screen.findByText(/daemon unavailable/)).toBeTruthy();
    const group = screen.getByRole("heading", { name: "Outside agents" }).closest(".q-group");
    expect(group?.querySelector('[role="alert"]')?.textContent).toContain("daemon unavailable");
  });

  it("words a guest-link error the client wrote again when the language changes", async () => {
    const fallback = request.getMockImplementation();
    request.mockImplementation((body: { type: string }) => {
      if (body.type === "get_guest_settings") {
        return Promise.reject(new H4rnRequestError(clientProto("internal", "notConnected")));
      }
      return fallback?.(body);
    });
    render(<GuestSettingsForm />);
    const group = (await screen.findByRole("heading", { name: "Sharing" })).closest(".q-group");
    await waitFor(() =>
      expect(group?.querySelector('[role="alert"]')?.textContent).toBe(
        "The daemon is not connected.",
      ),
    );

    await act(() => setLocalePreference("es"));
    expect(group?.querySelector('[role="alert"]')?.textContent).toBe(
      "El daemon no está conectado.",
    );
  });

  it("words a rewind-switch error the client wrote again when the language changes", async () => {
    const fallback = request.getMockImplementation();
    request.mockImplementation((body: { type: string }) => {
      if (body.type === "get_rewind_settings") {
        return Promise.reject(new H4rnRequestError(clientProto("internal", "notConnected")));
      }
      return fallback?.(body);
    });
    render(<GuestSettingsForm />);
    const group = (await screen.findByRole("heading", { name: "Threads" })).closest(".q-group");
    await waitFor(() =>
      expect(group?.querySelector('[role="alert"]')?.textContent).toBe(
        "The daemon is not connected.",
      ),
    );

    await act(() => setLocalePreference("ja"));
    expect(group?.querySelector('[role="alert"]')?.textContent).toBe(
      "デーモンに接続されていません。",
    );
  });
});
