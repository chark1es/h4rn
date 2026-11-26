// @vitest-environment jsdom
import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";

import { setLocalePreference } from "@/lib/i18n";
import { getState, store } from "@/lib/store/appStore";
import { ShortcutsDialog } from "./ShortcutsDialog";

function renderDialog() {
  store.setState({ ...getState(), ui: { ...getState().ui, shortcutsOpen: true } });
  render(<ShortcutsDialog />);
}

afterEach(async () => {
  cleanup();
  store.setState({ ...getState(), ui: { ...getState().ui, shortcutsOpen: false } });
  await act(() => setLocalePreference("system"));
  window.localStorage.clear();
});

describe("Keyboard shortcuts dialog", () => {
  it("links to Settings → Shortcuts inside the sentence", () => {
    renderDialog();
    const link = screen.getByRole("button", { name: "Settings → Shortcuts" });
    expect(link.closest("[data-slot='dialog-description']")?.textContent).toBe(
      "Commands are Cmd or Ctrl plus a key, so typing never triggers one. The exception, editing the newest queued message, acts only at the very start of the composer. Change any of them in Settings → Shortcuts.",
    );
    fireEvent.click(link);
    expect(getState().ui.shortcutsOpen).toBe(false);
  });

  it("lists the shared command names and groups in the active language", async () => {
    await act(() => setLocalePreference("es"));
    renderDialog();

    expect(screen.getByRole("heading", { name: "Atajos de teclado" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "Ajustes → Atajos" })).toBeTruthy();
    const groups = screen.getAllByRole("heading", { level: 3 }).map((h) => h.textContent);
    expect(groups).toEqual(["General", "Navegación", "Hilo", "Paneles", "Mientras escribes"]);
    expect(screen.getByText("Mostrar «En curso»")).toBeTruthy();
    expect(screen.getByText("Nueva línea")).toBeTruthy();
    expect(screen.queryByText("Show Working")).toBeNull();
  });
});
