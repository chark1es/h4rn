// @vitest-environment jsdom

import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { toast } from "sonner";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { setLocalePreference } from "@/lib/i18n";
import { promiseToast, pushToast } from "@/lib/toast";
import { Toaster, toastClearance } from "./sonner";

function stubViewport(phone: boolean) {
  window.matchMedia = ((query: string) => ({
    matches: query.includes("max-width: 639px") ? phone : false,
    media: query,
    onchange: null,
    addEventListener: () => undefined,
    removeEventListener: () => undefined,
    addListener: () => undefined,
    removeListener: () => undefined,
    dispatchEvent: () => false,
  })) as typeof window.matchMedia;
}

beforeEach(() => stubViewport(false));

afterEach(async () => {
  act(() => {
    toast.dismiss();
  });
  cleanup();
  document.documentElement.className = "";
  await act(() => setLocalePreference("system"));
});

function toaster() {
  return document.querySelector("[data-sonner-toaster]") as HTMLElement | null;
}

describe("Toaster", () => {
  it("names the region and the dismiss button in the active language", async () => {
    render(<Toaster />);
    act(() => {
      pushToast({ title: "Saved." });
    });
    expect(await screen.findByRole("region", { name: "Notifications" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "Dismiss" })).toBeTruthy();

    await act(() => setLocalePreference("ja"));
    expect(screen.getByRole("region", { name: "通知" })).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "閉じる" }));
    await waitFor(() => expect(screen.queryByText("Saved.")).toBeNull());
  });

  it("announces toasts politely and marks their kind", async () => {
    render(<Toaster />);
    act(() => {
      pushToast({ title: "Couldn't send.", description: "The relay closed.", variant: "error" });
    });
    const title = await screen.findByText("Couldn't send.");
    const item = title.closest("[data-sonner-toast]");
    expect(item?.getAttribute("data-type")).toBe("error");
    expect(screen.getByText("The relay closed.")).toBeTruthy();
    expect(screen.getByRole("region", { name: "Notifications" }).getAttribute("aria-live")).toBe(
      "polite",
    );
  });

  it("runs a toast's action", async () => {
    const undo = vi.fn();
    render(<Toaster />);
    act(() => {
      pushToast({ title: "Thread archived.", action: { label: "Undo", onClick: undo } });
    });
    fireEvent.click(await screen.findByRole("button", { name: "Undo" }));
    expect(undo).toHaveBeenCalledTimes(1);
  });

  it("turns a loading toast into the outcome of the operation", async () => {
    render(<Toaster />);
    let finish: (value: string) => void = () => undefined;
    const operation = new Promise<string>((resolve) => {
      finish = resolve;
    });
    let result: Promise<string> | undefined;
    act(() => {
      result = promiseToast(operation, {
        loading: "Pushing…",
        success: (branch) => `Pushed ${branch}.`,
        error: "Couldn't push.",
      });
    });
    expect(await screen.findByText("Pushing…")).toBeTruthy();
    await act(async () => {
      finish("main");
      await result;
    });
    expect(await screen.findByText("Pushed main.")).toBeTruthy();
    await expect(result).resolves.toBe("main");
  });

  it("follows the app's theme and sits at the top on the phone", async () => {
    document.documentElement.classList.add("dark");
    stubViewport(true);
    render(<Toaster />);
    act(() => {
      pushToast({ title: "Saved." });
    });
    await screen.findByText("Saved.");
    expect(toaster()?.getAttribute("data-sonner-theme")).toBe("dark");
    expect(toaster()?.getAttribute("data-y-position")).toBe("top");
    expect(toaster()?.getAttribute("data-x-position")).toBe("center");
  });

  it("sits at the bottom right on desktop", async () => {
    render(<Toaster />);
    act(() => {
      pushToast({ title: "Saved." });
    });
    await screen.findByText("Saved.");
    expect(toaster()?.getAttribute("data-sonner-theme")).toBe("light");
    expect(toaster()?.getAttribute("data-y-position")).toBe("bottom");
    expect(toaster()?.getAttribute("data-x-position")).toBe("right");
  });

  it("shows the same message once, however often it is raised", async () => {
    render(<Toaster />);
    act(() => {
      pushToast({ title: "Couldn't reach atlas.", variant: "error" });
      pushToast({ title: "Couldn't reach atlas.", variant: "error" });
      pushToast({ title: "Couldn't reach orion.", variant: "error" });
    });
    await screen.findByText("Couldn't reach orion.");
    expect(screen.getAllByText("Couldn't reach atlas.")).toHaveLength(1);
  });

  it("keeps errors up longer than other toasts", () => {
    pushToast({ title: "Saved." });
    pushToast({ title: "Couldn't save.", variant: "error" });
    const history = toast.getHistory() as { title?: unknown; duration?: number }[];
    const saved = history.find((entry) => entry.title === "Saved.");
    const failed = history.find((entry) => entry.title === "Couldn't save.");
    expect(saved?.duration).toBeUndefined();
    expect(failed?.duration).toBe(8000);
  });

  it("lets Tab leave the stack in document order", async () => {
    render(
      <>
        <button type="button">First</button>
        <input aria-label="Last" />
        <Toaster />
      </>,
    );
    act(() => {
      pushToast({ title: "Couldn't save.", variant: "error" });
    });
    await screen.findByText("Couldn't save.");
    const last = screen.getByRole("textbox", { name: "Last" });
    const toastItem = toaster()?.querySelector<HTMLElement>("[data-sonner-toast]");
    const dismiss = screen.getByRole("button", { name: "Dismiss" });

    act(() => last.focus());
    act(() => toastItem?.focus());
    act(() => dismiss.focus());
    expect(fireEvent.keyDown(dismiss, { key: "Tab" })).toBe(false);
    expect(document.activeElement).toBe(screen.getByRole("button", { name: "First" }));

    act(() => toastItem?.focus());
    expect(fireEvent.keyDown(toastItem as HTMLElement, { key: "Tab", shiftKey: true })).toBe(false);
    expect(document.activeElement).toBe(last);
  });

  it("keeps the hotkey's return to where focus came from", async () => {
    render(
      <>
        <input aria-label="Field" />
        <Toaster />
      </>,
    );
    act(() => {
      pushToast({ title: "Saved." });
    });
    await screen.findByText("Saved.");
    const field = screen.getByRole("textbox", { name: "Field" });
    act(() => field.focus());
    fireEvent.keyDown(document, { key: "t", code: "KeyT", altKey: true });
    expect(document.activeElement).toBe(toaster());
    const dismiss = screen.getByRole("button", { name: "Dismiss" });
    act(() => dismiss.focus());
    expect(fireEvent.keyDown(dismiss, { key: "Tab" })).toBe(true);
    act(() => dismiss.blur());
    expect(document.activeElement).toBe(field);
  });

  it("lifts desktop toasts above the dock it is told about", async () => {
    const dock = document.createElement("div");
    Object.defineProperty(dock, "offsetHeight", { value: 180 });
    const release = toastClearance(dock);
    expect(document.documentElement.style.getPropertyValue("--toast-dock")).toBe("180px");
    release?.();
    expect(document.documentElement.style.getPropertyValue("--toast-dock")).toBe("");
  });
});
