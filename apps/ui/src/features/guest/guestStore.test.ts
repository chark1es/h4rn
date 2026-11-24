import { beforeEach, describe, expect, it, vi } from "vitest";

const { request, toast } = vi.hoisted(() => ({ request: vi.fn(), toast: vi.fn() }));

vi.mock("@/lib/store/appStore", () => ({
  getClient: () => ({ request }),
  pushToast: toast,
}));

import type { GuestSharing } from "@/gen";

import {
  dismissGuestRequest,
  getGuestState,
  loadGuestSettings,
  loadGuestSharing,
  openGuestShare,
  resetGuestStoreForTests,
  sendGuestRequest,
  setGuestLinksEnabled,
  shareThreadWithGuest,
  stopGuestSharing,
} from "./guestStore";

const SHARING: GuestSharing = {
  thread_id: "th_1",
  links_enabled: true,
  link: {
    url: "https://connect.example.test/guest/machine/token",
    expires_at: "2026-11-24T12:00:00Z",
    bound: false,
    no_serving_listener: false,
  },
};

beforeEach(() => {
  resetGuestStoreForTests();
  request.mockReset();
  toast.mockReset();
});

describe("guest store", () => {
  it("reads the Labs setting and turns it on", async () => {
    request.mockResolvedValueOnce({ type: "guest_settings", settings: { enabled: false } });
    await loadGuestSettings();
    expect(getGuestState().settingsEnabled).toBe(false);

    request.mockResolvedValueOnce({ type: "guest_settings", settings: { enabled: true } });
    expect(await setGuestLinksEnabled(true)).toBe(true);
    expect(request).toHaveBeenLastCalledWith({ type: "set_guest_settings", enabled: true });
    expect(getGuestState().settingsEnabled).toBe(true);
  });

  it("creates a link and keeps its URL and expiry", async () => {
    request.mockResolvedValueOnce({ type: "guest_sharing", sharing: SHARING });
    const link = await shareThreadWithGuest("th_1");
    expect(link?.url).toBe(SHARING.link?.url);
    expect(getGuestState().sharing.th_1).toEqual(SHARING);
  });

  it("loads sharing and opens the dialog for that thread", async () => {
    request
      .mockResolvedValueOnce({ type: "guest_settings", settings: { enabled: true } })
      .mockResolvedValueOnce({ type: "guest_sharing", sharing: SHARING });
    openGuestShare("th_1");
    expect(getGuestState().dialogThreadId).toBe("th_1");
    await vi.waitFor(() => expect(getGuestState().sharing.th_1).toEqual(SHARING));
  });

  it("stops sharing and clears the link", async () => {
    request.mockResolvedValueOnce({ type: "guest_sharing", sharing: SHARING });
    await loadGuestSharing("th_1");
    request.mockResolvedValueOnce({ type: "ack" });
    expect(await stopGuestSharing("th_1")).toBe(true);
    expect(getGuestState().sharing.th_1?.link).toBeNull();
  });

  it("sends and dismisses a waiting request by id", async () => {
    request.mockResolvedValue({ type: "ack" });
    expect(await sendGuestRequest("th_1", "greq_1")).toBe(true);
    expect(request).toHaveBeenCalledWith({
      type: "send_guest_request",
      thread_id: "th_1",
      request_id: "greq_1",
    });
    expect(await dismissGuestRequest("th_1", "greq_2")).toBe(true);
    expect(request).toHaveBeenLastCalledWith({
      type: "dismiss_guest_request",
      thread_id: "th_1",
      request_id: "greq_2",
    });
  });

  it("reports a refused Send and clears the busy flag", async () => {
    request.mockRejectedValueOnce({ code: "not_found", message: "gone", hint: null });
    expect(await sendGuestRequest("th_1", "greq_1")).toBe(false);
    expect(getGuestState().answering.greq_1).toBe(false);
    expect(toast).toHaveBeenCalledOnce();
  });
});
