// @vitest-environment jsdom

import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { PencilIcon } from "lucide-react";
import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";

import { installPopupStubs } from "@/test/popups";
import {
  Menu,
  MenuCheckboxItem,
  MenuContent,
  MenuContextTrigger,
  MenuFooter,
  MenuItem,
  MenuSeparator,
  MenuTrigger,
} from "./menu";

beforeAll(installPopupStubs);
afterEach(cleanup);

async function press(key: string, target: Element | null = document.activeElement) {
  if (!target) throw new Error("nothing is focused");
  await act(async () => {
    fireEvent.keyDown(target, { key });
  });
}

function renderMenu(onAction = vi.fn()) {
  render(
    <Menu onAction={onAction}>
      <MenuTrigger>Actions</MenuTrigger>
      <MenuContent>
        <MenuItem value="rename" icon={<PencilIcon />}>
          Rename
        </MenuItem>
        <MenuItem value="pin" command="thread.pin">
          Keep in Working
        </MenuItem>
        <MenuSeparator />
        <MenuCheckboxItem value="wrap">Word wrap</MenuCheckboxItem>
        <MenuItem value="settle">Settle now</MenuItem>
        <MenuFooter>Started today</MenuFooter>
      </MenuContent>
    </Menu>,
  );
  return onAction;
}

describe("Menu", () => {
  it("opens from the keyboard, moves with arrows, Home, End and typeahead, and runs with Enter", async () => {
    const onAction = renderMenu();
    const trigger = screen.getByRole("button", { name: "Actions" });
    trigger.focus();
    await press("ArrowDown", trigger);
    expect(document.activeElement?.textContent).toBe("Rename");
    await press("ArrowDown");
    expect(document.activeElement?.textContent).toContain("Keep in Working");
    await press("End");
    expect(document.activeElement?.textContent).toBe("Settle now");
    await press("Home");
    expect(document.activeElement?.textContent).toBe("Rename");
    await press("s");
    expect(document.activeElement?.textContent).toBe("Settle now");
    await press("Enter");
    expect(onAction).toHaveBeenCalledWith("settle", expect.anything());
    expect(document.activeElement).toBe(trigger);
  });

  it("closes on Escape and returns focus to the trigger", async () => {
    renderMenu();
    const trigger = screen.getByRole("button", { name: "Actions" });
    trigger.focus();
    await press("ArrowUp", trigger);
    expect(document.activeElement?.textContent).toBe("Settle now");
    await press("Escape");
    expect(screen.queryByRole("menu")).toBeNull();
    expect(document.activeElement).toBe(trigger);
  });

  it("reserves the icon column for rows without an icon and announces real bindings", async () => {
    renderMenu();
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Actions" }));
    });
    expect(screen.getByRole("menu").hasAttribute("data-has-icons")).toBe(true);
    const pin = screen.getByRole("menuitem", { name: /Keep in Working/ });
    expect(pin.hasAttribute("data-has-icon")).toBe(false);
    expect(pin.getAttribute("aria-keyshortcuts")).toMatch(/^(Control|Meta)\+Alt\+P$/);
    const hint = pin.querySelector('[data-slot="menu-shortcut"]');
    expect(hint?.querySelector('[data-slot="kbd"]')).toBeNull();
    expect(hint?.textContent).toMatch(/P$/);
    const check = screen.getByRole("menuitemcheckbox", { name: "Word wrap" });
    expect(check.hasAttribute("data-has-icon")).toBe(true);
    expect(screen.getByText("Started today").closest('[role="menuitem"]')).toBeNull();
  });

  it("opens a context menu at a right click", async () => {
    render(
      <Menu>
        <MenuContextTrigger>
          <div>Row</div>
        </MenuContextTrigger>
        <MenuContent>
          <MenuItem value="open">Open</MenuItem>
        </MenuContent>
      </Menu>,
    );
    await act(async () => {
      fireEvent.contextMenu(screen.getByText("Row"), { clientX: 10, clientY: 20 });
    });
    expect(screen.getByRole("menuitem", { name: "Open" })).toBeTruthy();
  });
});
