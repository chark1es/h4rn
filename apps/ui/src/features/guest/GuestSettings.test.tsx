// @vitest-environment jsdom

import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const { request } = vi.hoisted(() => ({ request: vi.fn() }));

vi.mock("@/lib/store/appStore", () => ({
  getClient: () => ({ request }),
  pushToast: vi.fn(),
  useApp: (selector: (state: unknown) => unknown) => selector({ connection: { status: "online" } }),
}));

import { resetAccessStore } from "../access/store";
import { GuestSettingsForm } from "./GuestSettings";
import { resetGuestStoreForTests } from "./guestStore";

beforeEach(() => {
  resetGuestStoreForTests();
  resetAccessStore();
  request.mockReset();
  request.mockImplementation(
    (body: { type: string; request?: { type: string; enabled?: boolean } }) => {
      if (body.type === "guest_settings" || body.type === "set_guest_settings") {
        return Promise.resolve({ type: "guest_settings", settings: { enabled: false } });
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

afterEach(() => {
  cleanup();
  resetGuestStoreForTests();
  resetAccessStore();
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

  it("shows a failed outside-agent setting load under its switch", async () => {
    request.mockImplementation((body: { type: string; request?: { type: string } }) => {
      if (body.type === "guest_settings") {
        return Promise.resolve({ type: "guest_settings", settings: { enabled: false } });
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
});
