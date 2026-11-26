// @vitest-environment jsdom

import { act, cleanup, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

const mock = vi.hoisted(() => ({
  state: {
    hosts: {} as Record<string, unknown>,
    loadingThreads: false,
  },
}));

vi.mock("@/lib/store/appStore", () => ({
  useApp: (selector: (state: unknown) => unknown) => selector(mock.state),
}));

import { setLocalePreference } from "@/lib/i18n";

import { HostsPane } from "./HostsPane";

const HOUR = 3_600_000;

function offlineHost(lastSeenMsAgo: number) {
  mock.state.hosts = {
    atlas: {
      host_id: "atlas",
      name: "atlas",
      platform: "linux",
      online: false,
      harnesses: [],
      last_seen: new Date(Date.now() - lastSeenMsAgo).toISOString(),
    },
  };
}

afterEach(async () => {
  cleanup();
  mock.state.hosts = {};
  await act(() => setLocalePreference("system"));
  window.localStorage.clear();
});

/** The line under the host's name: platform · presence. */
function presenceLine(): string {
  const row = screen.getByRole("listitem");
  return row.querySelector("p.text-small")?.textContent ?? "";
}

describe("Hosts pane", () => {
  it("says when an offline host was last seen as a whole phrase", () => {
    offlineHost(3 * HOUR + 60_000);
    render(<HostsPane />);
    expect(presenceLine()).toMatch(/· last seen 3 hours ago$/);
  });

  it("reads naturally in Japanese, with no Latin unit", async () => {
    offlineHost(3 * HOUR + 60_000);
    render(<HostsPane />);
    await act(() => setLocalePreference("ja"));
    const line = presenceLine();
    expect(line).toContain("最終確認：3 時間前");
    expect(line).not.toMatch(/\dh/);
    expect(line).not.toContain("前に確認");
  });

  it("does not say “hace ahora” in Spanish for a host seen moments ago", async () => {
    offlineHost(0);
    render(<HostsPane />);
    await act(() => setLocalePreference("es"));
    const line = presenceLine();
    expect(line).toMatch(/· visto por última vez (ahora|hace \d+ segundos?)$/);
    expect(line).not.toContain("hace ahora");
  });

  it("does not put “ago” after “now” in English", () => {
    offlineHost(0);
    render(<HostsPane />);
    expect(presenceLine()).not.toMatch(/now ago/);
  });
});
