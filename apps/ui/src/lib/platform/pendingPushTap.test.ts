// @vitest-environment jsdom

import { afterEach, describe, expect, it, vi } from "vitest";

import { store } from "@/lib/store/appStore";

const IOS_USER_AGENT =
  "Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Mobile/15E148";

afterEach(() => {
  document.body.innerHTML = "";
  store.setState({
    ...store.getState(),
    ui: { ...store.getState().ui, route: { kind: "inbox" } },
  });
  vi.unstubAllGlobals();
});

async function bootWithPendingLink(url: string | null): Promise<void> {
  vi.spyOn(window.navigator, "userAgent", "get").mockReturnValue(IOS_USER_AGENT);
  const invoke = vi.fn(async (command: string) => {
    if (command === "push_take_pending_link") return { url };
    if (command === "deep_link_current") return [];
    if (command === "secret_get") return { value: null };
    return null;
  });
  vi.stubGlobal("__TAURI_INTERNALS__", { invoke });
  // The module reads the shell once when it loads, so import it after the stub.
  const { preparePlatform: boot } = await import("./mobile");
  await boot();
  return;
}

describe("an alert tap that cold-starts the iPhone app", () => {
  it("opens the thread the delegate buffered before the page loaded", async () => {
    await bootWithPendingLink("h4rn://thread/th_cold");

    // The tap's route is the same one a warm `h4rn-deep-link` takes.
    await vi.waitFor(() =>
      expect(store.getState().ui.route).toEqual({ kind: "thread", threadId: "th_cold" }),
    );
  });

  it("opens nothing when the launch was not a tap", async () => {
    await bootWithPendingLink(null);

    // Give the deferred route a turn before checking it stayed put.
    await new Promise((resolve) => window.setTimeout(resolve, 10));
    expect(store.getState().ui.route).toEqual({ kind: "inbox" });
  });
});
