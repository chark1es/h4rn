// @vitest-environment jsdom

import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { useState } from "react";
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";

const mocks = vi.hoisted(() => ({
  request: vi.fn(),
  app: {
    localHostId: null as string | null,
    hosts: {} as Record<string, { name: string }>,
    connection: { status: "online" },
  },
}));

vi.mock("@/lib/store/appStore", () => ({
  getClient: () => ({ request: mocks.request }),
  useApp: (selector: (state: typeof mocks.app) => unknown) => selector(mocks.app),
}));

vi.mock("@/features/harnesses/harnessStore", () => ({
  useHarnessVersions: () => undefined,
  useHarnesses: (selector: (state: unknown) => unknown) =>
    selector({
      rows: [
        { harness: "claude_code", installed: "2.1.0" },
        { harness: "codex", installed: "0.171.0" },
      ],
    }),
}));

import type { TitleModelMode } from "@/gen";
import { AddGenerator, ModePicker, ThreadSettings } from "./TextGenerationSettings";
import { resetTextGenerationForTests } from "./textGenerationStore";

// Radix's dropdown expects these pointer-capture APIs, which jsdom lacks.
beforeAll(() => {
  Element.prototype.hasPointerCapture ??= () => false;
  Element.prototype.setPointerCapture ??= () => undefined;
  Element.prototype.releasePointerCapture ??= () => undefined;
});

/** Opens a ChipSelect's menu; Radix listens for the pointer down, not the click. */
function openChip(trigger: HTMLElement) {
  fireEvent.pointerDown(trigger, { button: 0, ctrlKey: false, pointerType: "mouse" });
  fireEvent.click(trigger);
}

afterEach(cleanup);

function PickerHarness({ initial }: { initial: TitleModelMode }) {
  const [value, setValue] = useState<TitleModelMode>(initial);
  return <ModePicker value={value} onChange={setValue} />;
}

function radio(name: string): HTMLElement {
  return screen.getByRole("radio", { name });
}

describe("the title-mode picker", () => {
  it("moves the choice and the focus with ArrowRight and wraps at the end", () => {
    render(<PickerHarness initial="off" />);
    const off = radio("Off");
    expect(off.getAttribute("aria-checked")).toBe("true");
    expect(off.tabIndex).toBe(0);

    fireEvent.keyDown(off, { key: "ArrowRight" });
    const coding = radio("Coding agent");
    expect(coding.getAttribute("aria-checked")).toBe("true");
    expect(document.activeElement).toBe(coding);
    expect(off.getAttribute("aria-checked")).toBe("false");
    expect(off.tabIndex).toBe(-1);

    fireEvent.keyDown(coding, { key: "ArrowRight" });
    expect(radio("Text generator").getAttribute("aria-checked")).toBe("true");

    fireEvent.keyDown(radio("Text generator"), { key: "ArrowRight" });
    expect(radio("Off").getAttribute("aria-checked")).toBe("true");
    expect(document.activeElement).toBe(radio("Off"));
  });

  it("jumps to the ends with Home and End", () => {
    render(<PickerHarness initial="coding_agent" />);

    fireEvent.keyDown(radio("Coding agent"), { key: "Home" });
    expect(radio("Off").getAttribute("aria-checked")).toBe("true");
    expect(document.activeElement).toBe(radio("Off"));

    fireEvent.keyDown(radio("Off"), { key: "End" });
    const last = radio("Text generator");
    expect(last.getAttribute("aria-checked")).toBe("true");
    expect(document.activeElement).toBe(last);
  });
});

describe("the coding-agent choice", () => {
  beforeEach(() => {
    mocks.request.mockReset();
    mocks.request.mockResolvedValue({ type: "ack" });
    resetTextGenerationForTests({
      settings: {
        title: { mode: "coding_agent", harness: null, generator_id: null, model: null },
        source_control: {
          mode: "off",
          harness: null,
          generator_id: null,
          model: null,
          instructions: null,
        },
      },
      settingsLoaded: true,
      loaded: true,
      generators: [],
    });
  });

  it("defaults to following the thread's agent and can pick one agent instead", async () => {
    render(<ThreadSettings />);

    const trigger = screen.getByRole("combobox", {
      name: "Coding agent for titles: Follow the thread's agent",
    });
    openChip(trigger);
    // Codex is installed on this host but is not offered as a title agent.
    expect(screen.queryByRole("option", { name: /^Codex/ })).toBeNull();
    fireEvent.click(await screen.findByRole("option", { name: /^Claude Code/ }));

    expect(
      screen.getByRole("combobox", { name: "Coding agent for titles: Claude Code" }),
    ).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Save" }));
    await waitFor(() =>
      expect(mocks.request).toHaveBeenCalledWith({
        type: "set_text_generation_settings",
        settings: {
          title: { mode: "coding_agent", harness: "claude_code", generator_id: null, model: null },
          source_control: {
            mode: "off",
            harness: null,
            generator_id: null,
            model: null,
            instructions: null,
          },
        },
      }),
    );
  });

  it("says Codex threads keep the first line or use a text generator", () => {
    render(<ThreadSettings />);
    expect(
      screen.getByText(/Codex threads keep the first-line title or use a text generator/),
    ).toBeTruthy();
  });
});

describe("the add-generator form", () => {
  beforeEach(() => {
    mocks.request.mockReset();
    resetTextGenerationForTests();
  });

  function fill(name: string, baseUrl: string, model: string, key: string) {
    fireEvent.change(screen.getByLabelText("Generator name"), { target: { value: name } });
    fireEvent.change(screen.getByLabelText("Generator base URL"), { target: { value: baseUrl } });
    fireEvent.change(screen.getByLabelText("Generator model"), { target: { value: model } });
    fireEvent.change(screen.getByLabelText("Generator API key"), { target: { value: key } });
  }

  function keyInput(): HTMLInputElement {
    return screen.getByLabelText("Generator API key") as HTMLInputElement;
  }

  it("forgets the key, the fields and the errors on Cancel", () => {
    render(<AddGenerator />);
    fireEvent.click(screen.getByRole("button", { name: /Add generator/ }));
    fireEvent.click(screen.getByRole("button", { name: "Save generator" }));
    expect(screen.getAllByRole("alert").length).toBeGreaterThan(0);

    // Built at runtime so no credential-shaped literal lands in the repo.
    const fakeKey = ["not", "a", "real", "key"].join("-");
    fill("Local", "https://example.com/v1", "tiny-model", fakeKey);
    expect(keyInput().value).toBe(fakeKey);

    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    fireEvent.click(screen.getByRole("button", { name: /Add generator/ }));

    expect(keyInput().value).toBe("");
    expect((screen.getByLabelText("Generator name") as HTMLInputElement).value).toBe("");
    expect((screen.getByLabelText("Generator base URL") as HTMLInputElement).value).toBe("");
    expect((screen.getByLabelText("Generator model") as HTMLInputElement).value).toBe("");
    expect(screen.queryByRole("alert")).toBeNull();
  });

  it("forgets the key and a failed save's error on Cancel", async () => {
    mocks.request.mockRejectedValue(new Error("the daemon refused the generator"));
    render(<AddGenerator />);
    fireEvent.click(screen.getByRole("button", { name: /Add generator/ }));
    fill("Local", "https://example.com/v1", "tiny-model", ["not", "a", "real", "key"].join("-"));
    fireEvent.click(screen.getByRole("button", { name: "Save generator" }));
    await waitFor(() => expect(screen.getByRole("alert")).toBeTruthy());

    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    fireEvent.click(screen.getByRole("button", { name: /Add generator/ }));
    expect(screen.queryByRole("alert")).toBeNull();
    expect(keyInput().value).toBe("");
  });

  it("forgets the key and the fields after a successful save", async () => {
    mocks.request.mockResolvedValue({
      type: "text_generator",
      generator: {
        id: "tg_new",
        name: "Local",
        kind: "open_ai_compatible",
        base_url: "https://example.com/v1",
        model: "tiny-model",
        has_api_key: true,
      },
    });
    render(<AddGenerator />);
    fireEvent.click(screen.getByRole("button", { name: /Add generator/ }));
    fill("Local", "https://example.com/v1", "tiny-model", ["not", "a", "real", "key"].join("-"));
    fireEvent.click(screen.getByRole("button", { name: "Save generator" }));

    await waitFor(() => expect(screen.getByRole("button", { name: /Add generator/ })).toBeTruthy());
    fireEvent.click(screen.getByRole("button", { name: /Add generator/ }));
    expect(keyInput().value).toBe("");
    expect((screen.getByLabelText("Generator name") as HTMLInputElement).value).toBe("");
    expect((screen.getByLabelText("Generator base URL") as HTMLInputElement).value).toBe("");
  });
});
