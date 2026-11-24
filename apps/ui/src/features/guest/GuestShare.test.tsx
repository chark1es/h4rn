// @vitest-environment jsdom

import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const harness = vi.hoisted(() => ({
  state: {
    settingsEnabled: true as boolean | null,
    settingsLoading: false,
    settingsSaving: false,
    dialogThreadId: "th_1" as string | null,
    sharing: {} as Record<string, unknown>,
    sharingLoading: {} as Record<string, boolean>,
    creating: {} as Record<string, boolean>,
    stopping: {} as Record<string, boolean>,
    answering: {} as Record<string, boolean>,
    error: null as string | null,
  },
  send: vi.fn(),
  dismiss: vi.fn(),
  share: vi.fn(),
  stop: vi.fn(),
  copy: vi.fn(),
  setEnabled: vi.fn(),
  close: vi.fn(),
}));

vi.mock("@/features/guest/guestStore", () => ({
  useGuest: () => harness.state,
  sendGuestRequest: harness.send,
  dismissGuestRequest: harness.dismiss,
  shareThreadWithGuest: harness.share,
  stopGuestSharing: harness.stop,
  copyGuestLink: harness.copy,
  setGuestLinksEnabled: harness.setEnabled,
  loadGuestSharing: vi.fn(),
  loadGuestSettings: vi.fn(),
  closeGuestShare: harness.close,
}));

vi.mock("@/lib/store/appStore", () => ({
  useApp: (selector: (state: unknown) => unknown) =>
    selector({
      threads: {
        th_1: { thread_id: "th_1", title: "Fix the flaky auth test" },
      },
      logs: {
        th_1: {
          guestRequests: [
            {
              request_id: "greq_1",
              thread_id: "th_1",
              text: "Could you check the logs?",
              attachments: [{ name: "notes.txt", mime: "text/plain", size: 2048 }],
              created_at: "2026-11-23T10:00:00Z",
            },
          ],
        },
      },
    }),
}));

import { GuestRequestCard } from "@/components/thread/GuestRequestCard";
import { GuestShareDialog } from "./GuestShareDialog";

beforeEach(() => {
  harness.state.sharing = {};
  harness.state.settingsEnabled = true;
  harness.state.dialogThreadId = "th_1";
  harness.send.mockReset();
  harness.dismiss.mockReset();
  harness.share.mockReset();
  harness.stop.mockReset();
});

afterEach(cleanup);

describe("GuestRequestCard", () => {
  it("shows the guest's message and files with Send and Dismiss", () => {
    render(<GuestRequestCard threadId="th_1" />);
    expect(screen.getByText("Request")).toBeTruthy();
    expect(screen.getByText("Could you check the logs?")).toBeTruthy();
    expect(screen.getByText("notes.txt")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: /send/i }));
    expect(harness.send).toHaveBeenCalledWith("th_1", "greq_1");
    fireEvent.click(screen.getByRole("button", { name: /dismiss/i }));
    expect(harness.dismiss).toHaveBeenCalledWith("th_1", "greq_1");
  });
});

describe("GuestShareDialog", () => {
  it("explains the Labs switch when guest links are off", () => {
    harness.state.settingsEnabled = false;
    render(<GuestShareDialog />);
    expect(screen.getByText(/guest links are off/i)).toBeTruthy();
    expect(screen.getByRole("button", { name: /turn on guest links/i })).toBeTruthy();
  });

  it("creates a link when the thread has none", () => {
    harness.share.mockResolvedValue({
      url: "https://connect.test/guest/m/token",
      expires_at: "2026-11-24T10:00:00Z",
      bound: false,
    });
    render(<GuestShareDialog />);
    fireEvent.click(screen.getByRole("button", { name: /create link/i }));
    expect(harness.share).toHaveBeenCalledWith("th_1");
  });

  it("shows the link, its expiry and Stop sharing", () => {
    harness.state.sharing = {
      th_1: {
        thread_id: "th_1",
        links_enabled: true,
        link: {
          url: "https://connect.test/guest/m/token",
          expires_at: "2099-11-24T10:00:00Z",
          bound: true,
        },
      },
    };
    render(<GuestShareDialog />);
    const input = screen.getByLabelText("Guest link") as HTMLInputElement;
    expect(input.value).toContain("/guest/");
    expect(screen.getByText(/bound to the first browser/i)).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: /stop sharing/i }));
    expect(harness.stop).toHaveBeenCalledWith("th_1");
  });

  it("explains when no listener can serve the link", () => {
    harness.state.sharing = {
      th_1: {
        thread_id: "th_1",
        links_enabled: true,
        link: {
          url: null,
          expires_at: "2099-11-24T10:00:00Z",
          bound: false,
          no_serving_listener: true,
        },
      },
    };
    render(<GuestShareDialog />);
    expect(screen.getByText(/add a loopback listener/i)).toBeTruthy();
    expect(screen.getByText(/link this machine to Connect/i)).toBeTruthy();
  });

  it("shows the daemon's remedy when creating a link fails", () => {
    harness.state.error =
      "This machine has no address a guest link can open on. Add a loopback listener, or link this machine to Connect, then share again.";
    render(<GuestShareDialog />);
    expect(screen.getByRole("alert").textContent).toContain("Add a loopback listener");
  });

  it("says only an admin device may show the URL", () => {
    harness.state.sharing = {
      th_1: {
        thread_id: "th_1",
        links_enabled: true,
        link: {
          url: null,
          expires_at: "2099-11-24T10:00:00Z",
          bound: false,
        },
      },
    };
    render(<GuestShareDialog />);
    expect(screen.getByText(/only an admin device can copy it/i)).toBeTruthy();
  });

  it("closes on Escape without losing the thread", () => {
    harness.close.mockReset();
    render(<GuestShareDialog />);
    fireEvent.keyDown(document.activeElement ?? document.body, {
      key: "Escape",
      code: "Escape",
    });
    expect(harness.close).toHaveBeenCalled();
  });
});
