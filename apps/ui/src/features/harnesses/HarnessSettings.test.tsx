// @vitest-environment jsdom

import { readFileSync } from "node:fs";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

vi.mock("@/lib/store/appStore", () => ({
  useApp: (selector: (state: unknown) => unknown) => selector({ connection: { status: "online" } }),
}));

vi.mock("./harnessStore", () => ({
  useHarnessVersions: () => undefined,
  useAcpAgents: () => undefined,
  useHarnesses: (selector: (state: unknown) => unknown) =>
    selector({
      rows: [],
      loaded: true,
      loading: false,
      error: null,
      agents: [],
      agentsLoaded: true,
      agentsError: null,
      probes: {},
      probing: {},
      updating: {},
      output: {},
      results: {},
    }),
  loadHarnessVersions: () => Promise.resolve(),
  updateHarness: () => Promise.resolve(),
  probeAcpAgent: () => Promise.resolve(null),
  addAcpAgent: () => Promise.resolve(null),
  updateAcpAgent: () => Promise.resolve(null),
  removeAcpAgent: () => Promise.resolve(null),
  loadAcpAgentSpec: () => Promise.resolve({}),
}));

import { HarnessSettings } from "./HarnessSettings";

describe("ACP agent form layout", () => {
  afterEach(() => {
    cleanup();
  });

  it("keeps one environment row together and stacks it on a phone", async () => {
    render(<HarnessSettings />);
    fireEvent.click(await screen.findByRole("button", { name: /Add ACP agent/ }));

    const name = await screen.findByLabelText("Environment name 1");
    const value = screen.getByLabelText("Environment value 1");
    const remove = screen.getByLabelText("Remove environment row 1");
    const row = name.closest(".q-acp-row");
    expect(row).not.toBeNull();
    // The name, value and Remove are one row the phone can stack.
    expect(row?.querySelector('[aria-label="Environment value 1"]')).toBe(value);
    expect(row?.querySelector('[aria-label="Remove environment row 1"]')).toBe(remove);

    // The phone breakpoint stacks the three controls and gives each a
    // comfortable 44 px target; a single row would squeeze them.
    const css = readFileSync("src/styles/quiet.css", "utf8");
    expect(css).toMatch(
      /@media \(max-width: 639px\) \{[\s\S]*?\.q-acp-row \{[\s\S]*?flex-direction: column;[\s\S]*?\.q-acp-row \.q-input,[\s\S]*?\.q-acp-row \.q-btn \{[\s\S]*?min-height: 44px;/,
    );
    expect(css).toContain(".q-acp-row .q-acp-name");
    expect(css).toContain(".q-acp-row .q-acp-value");
  });
});
