// @vitest-environment jsdom

import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import "@/lib/i18n";
import { Menu, MenuContent, MenuItem, MenuTrigger } from "./menu";
import { Tip } from "./tooltip";

afterEach(() => {
  cleanup();
  vi.useRealTimers();
});

describe("Tip", () => {
  it("keeps the control's own name, element and handlers", () => {
    const onClick = vi.fn();
    render(
      <Tip label="Settings">
        <button type="button" aria-label="Settings" className="q-ibtn" onClick={onClick}>
          <svg aria-hidden="true" />
        </button>
      </Tip>,
    );
    const button = screen.getByRole("button", { name: "Settings" });
    expect(button.className).toContain("q-ibtn");
    fireEvent.click(button);
    expect(onClick).toHaveBeenCalledTimes(1);
  });

  it("names an icon-only control that has no name of its own", async () => {
    render(
      <Tip label="New thread">
        <button type="button">
          <svg aria-hidden="true" />
        </button>
      </Tip>,
    );
    await waitFor(() => {
      const label = screen.getByRole("button").getAttribute("aria-labelledby");
      expect(label && document.getElementById(label)?.textContent).toBe("New thread");
    });
  });

  it("opens on keyboard focus with the shortcut", async () => {
    render(
      <Tip label="Search" keys="Ctrl K">
        <button type="button" aria-label="Search" />
      </Tip>,
    );
    fireEvent.keyDown(document.body, { key: "Tab" });
    await act(async () => {
      screen.getByRole("button", { name: "Search" }).focus();
    });
    const tooltip = await screen.findByRole("tooltip");
    expect(tooltip.textContent).toContain("Search");
    expect(tooltip.querySelectorAll("kbd")).toHaveLength(2);
  });

  it("waits about 300 ms on hover before opening", async () => {
    vi.useFakeTimers();
    render(
      <Tip label="Close panel">
        <button type="button" aria-label="Close panel" />
      </Tip>,
    );
    const button = screen.getByRole("button", { name: "Close panel" });
    fireEvent.pointerEnter(button, { pointerType: "mouse" });
    fireEvent.pointerMove(button, { pointerType: "mouse" });
    await act(async () => {
      vi.advanceTimersByTime(200);
    });
    expect(screen.queryByRole("tooltip")?.hasAttribute("data-open") ?? false).toBe(false);
    await act(async () => {
      vi.advanceTimersByTime(150);
    });
    expect(screen.getByRole("tooltip", { hidden: true }).hasAttribute("data-open")).toBe(true);
  });

  it("composes with a menu trigger: the menu still opens and the tooltip still shows", async () => {
    render(
      <Menu>
        <Tip label="Sort projects">
          <MenuTrigger aria-label="Sort">
            <svg aria-hidden="true" />
          </MenuTrigger>
        </Tip>
        <MenuContent>
          <MenuItem>By name</MenuItem>
        </MenuContent>
      </Menu>,
    );
    const trigger = screen.getByRole("button", { name: "Sort" });
    expect(trigger.getAttribute("aria-describedby")).toBe(
      screen.getByRole("tooltip", { hidden: true }).id,
    );
    vi.useFakeTimers();
    fireEvent.pointerEnter(trigger, { pointerType: "mouse" });
    fireEvent.pointerMove(trigger, { pointerType: "mouse" });
    await act(async () => {
      vi.advanceTimersByTime(350);
    });
    const tooltip = screen.getByRole("tooltip", { hidden: true });
    expect(tooltip.hasAttribute("data-open")).toBe(true);
    expect(tooltip.textContent).toContain("Sort projects");
    vi.useRealTimers();
    await act(async () => {
      fireEvent.click(trigger);
    });
    expect(await screen.findByRole("menuitem", { name: "By name" })).toBeTruthy();
    expect(trigger.getAttribute("aria-haspopup")).toBe("menu");
  });
});
