// @vitest-environment jsdom

import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeAll, describe, expect, it } from "vitest";

import { setLocalePreference } from "@/lib/i18n";

import { SettingsPage } from "./SettingsPage";

beforeAll(() => {
  // Radix's dropdown expects pointer-capture APIs jsdom lacks.
  Element.prototype.hasPointerCapture ??= () => false;
  Element.prototype.setPointerCapture ??= () => undefined;
  Element.prototype.releasePointerCapture ??= () => undefined;
});

afterEach(async () => {
  cleanup();
  await act(() => setLocalePreference("system"));
  window.localStorage.clear();
});

/** Opens a ChipSelect's menu; Radix listens for the pointer down, not the click. */
function openChip(trigger: HTMLElement) {
  fireEvent.pointerDown(trigger, { button: 0, ctrlKey: false, pointerType: "mouse" });
  fireEvent.click(trigger);
}

describe("Settings → Appearance → Language", () => {
  beforeAll(() => {
    // The System choice resolves from the browser's languages.
    Object.defineProperty(window.navigator, "languages", {
      configurable: true,
      value: ["en-US"],
    });
  });

  it("offers System and each language in its own name", async () => {
    render(<SettingsPage section="appearance" />);
    openChip(screen.getByRole("combobox", { name: "Interface language: System" }));
    const names = (await screen.findAllByRole("option")).map((item) => item.textContent);
    // System names the language it resolves to as its hint.
    expect(names).toEqual(["SystemEnglish", "English", "简体中文", "Español", "日本語"]);
    fireEvent.keyDown(document.activeElement ?? document.body, { key: "Escape" });
  });

  it("switches the page at once, with no reload, and keeps the choice", async () => {
    render(<SettingsPage section="appearance" />);
    expect(screen.getByRole("heading", { level: 1, name: "Appearance" })).toBeTruthy();

    openChip(screen.getByRole("combobox", { name: "Interface language: System" }));
    await act(async () => {
      fireEvent.click(await screen.findByRole("option", { name: "日本語" }));
    });

    expect(await screen.findByRole("heading", { level: 1, name: "外観" })).toBeTruthy();
    expect(screen.getByRole("combobox", { name: "表示言語: 日本語" })).toBeTruthy();
    expect(screen.getByRole("main", { name: "設定" })).toBeTruthy();
    expect(document.documentElement.lang).toBe("ja");
    expect(window.localStorage.getItem("h4rn.locale")).toBe("ja");

    // Every language re-renders the same page in place.
    for (const [locale, title] of [
      ["zh-CN", "外观"],
      ["es", "Apariencia"],
      ["system", "Appearance"],
    ] as const) {
      await act(() => setLocalePreference(locale));
      expect(screen.getByRole("heading", { level: 1, name: title })).toBeTruthy();
    }
  });
});
