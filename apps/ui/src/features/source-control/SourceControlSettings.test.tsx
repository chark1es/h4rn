// @vitest-environment jsdom

import { cleanup, render } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("@/lib/store/appStore", () => ({
  getClient: () => ({ request: vi.fn() }),
  useApp: (selector: (state: unknown) => unknown) =>
    selector({ localHostId: null, hosts: {}, connection: { status: "online" } }),
}));

vi.mock("@/features/harnesses/harnessStore", () => ({
  useHarnessVersions: () => undefined,
  useHarnesses: (selector: (state: unknown) => unknown) => selector({ rows: [] }),
}));

import { resetTextGenerationForTests } from "@/features/text-generation/textGenerationStore";
import { SourceControlSettings, sourceControlSummary } from "./SourceControlSettings";

afterEach(cleanup);

describe("the Source control settings page", () => {
  beforeEach(() => {
    resetTextGenerationForTests({
      settings: {
        title: { mode: "coding_agent", harness: null, generator_id: null, model: null },
        source_control: {
          mode: "text_generator",
          harness: null,
          generator_id: "tg_1",
          model: "tiny-model",
          instructions: null,
        },
      },
      settingsLoaded: true,
      loaded: true,
      generators: [],
    });
  });

  it("describes the message model in the footer, not the title model", () => {
    render(<SourceControlSettings />);
    const footers = [...document.querySelectorAll("p.q-faint")].map((node) => node.textContent);
    expect(footers).toContain("Messages are written by a text generator with tiny-model.");
    expect(footers.some((text) => text?.includes("Titles are written"))).toBe(false);
    expect(document.body.textContent).not.toMatch(/Titles are written/);
  });

  it("says what non-Claude threads really need", () => {
    expect(
      sourceControlSummary({
        mode: "coding_agent",
        harness: null,
        generator_id: null,
        model: null,
        instructions: null,
      }),
    ).toBe(
      "Claude Code threads use Claude Code; other threads need a text generator, otherwise nothing is generated.",
    );
  });
});
