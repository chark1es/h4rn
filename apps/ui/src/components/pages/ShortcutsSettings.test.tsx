// @vitest-environment jsdom

import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";

import { setLocalePreference } from "@/lib/i18n";
import { rebind, resetAllBindings } from "@/lib/keybindings";

import { SettingsPage } from "./SettingsPage";

afterEach(async () => {
  cleanup();
  resetAllBindings();
  await act(() => setLocalePreference("system"));
  window.localStorage.clear();
});

describe("Settings → Shortcuts in another language", () => {
  it("shows translated group headings and command names", async () => {
    await act(() => setLocalePreference("ja"));
    render(<SettingsPage section="shortcuts" />);

    expect(screen.getByRole("heading", { level: 1, name: "ショートカット" })).toBeTruthy();
    const groups = screen.getAllByRole("heading", { level: 2 }).map((h) => h.textContent);
    expect(groups).toEqual(["一般", "ナビゲーション", "スレッド", "パネル"]);

    expect(screen.getByText("新しいスレッド")).toBeTruthy();
    expect(screen.getByText("「整理済み」を表示")).toBeTruthy();
    expect(screen.queryByText("New thread")).toBeNull();
    expect(screen.queryByText("General")).toBeNull();

    // The button's accessible name carries the translated command name too.
    expect(
      screen.getByRole("button", { name: /^新しいスレッド のショートカットを変更/ }),
    ).toBeTruthy();
  });

  it("names the clashing command in the active language", async () => {
    await act(() => setLocalePreference("ja"));
    const settings = rebind("thread.new", "mod+,");
    expect(settings).toEqual({ ok: false, reason: "「設定」で使用されています。" });
  });

  it("puts the translated name in the recording label", async () => {
    await act(() => setLocalePreference("es"));
    render(<SettingsPage section="shortcuts" />);
    fireEvent.click(screen.getByRole("button", { name: /^Cambiar el atajo de Nuevo hilo,/ }));
    expect(screen.getByRole("button", { name: "Grabando un atajo para Nuevo hilo" })).toBeTruthy();
    fireEvent.keyDown(window, { key: "Escape" });
  });
});
