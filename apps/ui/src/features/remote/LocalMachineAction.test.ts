import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { ActiveRemote } from "./active";
import { UseThisMachineButton } from "./LocalMachineAction";
import { getRemoteState, remoteStore } from "./remoteStore";

const ACTIVE: ActiveRemote = {
  connectUrl: "https://connect.example",
  machineId: "mac_1",
  machineName: "atlas",
  daemonNoiseKey: "ab".repeat(32),
  addresses: ["100.101.5.4:47800", "relay:https://connect.example"],
  online: true,
  relay: true,
};

function render(): string {
  return renderToStaticMarkup(createElement(UseThisMachineButton));
}

function store(active: ActiveRemote | null): void {
  remoteStore.setState({
    ...getRemoteState(),
    active,
    activeMachineId: active?.machineId ?? null,
    machines: [],
  });
}

beforeEach(() => {
  store(ACTIVE);
});

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("UseThisMachineButton", () => {
  it("offers the way back to the local daemon when the machine list is empty", () => {
    // Offline, `machines` comes from the relay and stays empty. The stored
    // descriptor still names the machine the boot transport is following.
    store(ACTIVE);
    expect(getRemoteState().machines).toEqual([]);

    const html = render();

    expect(html).toContain("Use this machine");
    expect(html).toContain("Disconnect from atlas");
  });

  it("renders nothing when no remote is stored", () => {
    store(null);
    expect(render()).toBe("");
  });

  it("names the machine when the action disconnects", () => {
    const html = renderToStaticMarkup(createElement(UseThisMachineButton, { verb: "disconnect" }));
    expect(html).toContain("Disconnect from atlas");
    expect(html).not.toContain(">Use this machine<");
  });

  it("keeps the compact visible label inside the accessible name", () => {
    // WCAG 2.5.3: "Use local" must be part of the button's accessible name.
    const html = renderToStaticMarkup(createElement(UseThisMachineButton, { compact: true }));
    expect(html).toContain(">Use local<");
    expect(html).toContain('aria-label="Use local — disconnect from atlas"');
  });

  it("offers only Disconnect on the mobile shell, which has no local daemon", () => {
    vi.stubGlobal("window", {
      __TAURI_INTERNALS__: {},
      navigator: { userAgent: "Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X)" },
    });

    const compact = renderToStaticMarkup(createElement(UseThisMachineButton, { compact: true }));
    const full = renderToStaticMarkup(createElement(UseThisMachineButton));

    expect(compact).toContain(">Disconnect<");
    expect(compact).toContain('aria-label="Disconnect from atlas"');
    expect(compact).not.toContain("Use local");
    expect(full).toContain(">Disconnect<");
    expect(full).not.toContain("Use this machine");
  });
});
