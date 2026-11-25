// @vitest-environment jsdom

import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it } from "vitest";

import {
  followUpBehavior,
  multilineEnter,
  setFollowUpBehavior,
  setMultilineEnter,
} from "@/lib/chatSettings";

import { SettingsPage } from "./SettingsPage";

beforeEach(() => {
  window.localStorage.clear();
  setFollowUpBehavior("queue");
  setMultilineEnter(false);
});

afterEach(() => {
  cleanup();
  setFollowUpBehavior("queue");
  setMultilineEnter(false);
});

describe("chat settings", () => {
  it("remembers the follow-up behaviour this device sends with", () => {
    render(<SettingsPage section="chat" />);

    const steer = screen.getByRole("button", { name: "Steer" });
    expect(steer.getAttribute("aria-pressed")).toBe("false");
    fireEvent.click(steer);
    expect(steer.getAttribute("aria-pressed")).toBe("true");
    expect(followUpBehavior()).toBe("steer");

    fireEvent.click(screen.getByRole("button", { name: "Queue" }));
    expect(followUpBehavior()).toBe("queue");
  });

  it("offers the multiline Enter switch and persists it", () => {
    render(<SettingsPage section="chat" />);

    const toggle = screen.getByRole("switch", { name: "Enter inserts a new line" });
    expect(toggle.getAttribute("aria-checked")).toBe("false");
    fireEvent.click(toggle);
    expect(multilineEnter()).toBe(true);
    expect(toggle.getAttribute("aria-checked")).toBe("true");
  });
});
