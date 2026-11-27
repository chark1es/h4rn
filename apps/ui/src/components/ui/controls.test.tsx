// @vitest-environment jsdom

import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { useState } from "react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { Checkbox } from "./checkbox";
import { Collapsible, CollapsiblePanel, CollapsibleTrigger } from "./collapsible";
import { Radio, RadioGroup } from "./radio-group";
import { SegmentedControl, SegmentedControlItem } from "./segmented-control";
import { Switch } from "./switch";
import { Tabs, TabsList, TabsPanel, TabsTab } from "./tabs";

afterEach(cleanup);

function RangeExample() {
  const [value, setValue] = useState<string | null>("day");
  return (
    <SegmentedControl aria-label="Range" value={value} onValueChange={setValue}>
      <SegmentedControlItem value="day">Day</SegmentedControlItem>
      <SegmentedControlItem value="week">Week</SegmentedControlItem>
    </SegmentedControl>
  );
}

describe("Switch", () => {
  it("is a named switch that Space and click toggle", () => {
    const onChange = vi.fn();
    render(<Switch aria-label="Auto-check" onCheckedChange={onChange} />);
    const control = screen.getByRole("switch", { name: "Auto-check" });
    expect(control.getAttribute("aria-checked")).toBe("false");
    expect(control.dataset.slot).toBe("switch");
    fireEvent.click(control);
    expect(onChange).toHaveBeenLastCalledWith(true, expect.anything());
    expect(control.getAttribute("aria-checked")).toBe("true");
    expect(control.hasAttribute("data-checked")).toBe(true);
  });

  it("can't be changed while disabled", () => {
    const onChange = vi.fn();
    render(<Switch aria-label="Fast" disabled onCheckedChange={onChange} />);
    fireEvent.click(screen.getByRole("switch", { name: "Fast" }));
    expect(onChange).not.toHaveBeenCalled();
  });
});

describe("SegmentedControl", () => {
  it("is a radio group whose arrows check the next item", () => {
    render(<RangeExample />);
    const group = screen.getByRole("radiogroup", { name: "Range" });
    expect(group.dataset.slot).toBe("segmented-control");
    const day = screen.getByRole("radio", { name: "Day" });
    expect(day.getAttribute("aria-checked")).toBe("true");
    day.focus();
    fireEvent.keyDown(day, { key: "ArrowRight" });
    const week = screen.getByRole("radio", { name: "Week" });
    expect(week.getAttribute("aria-checked")).toBe("true");
    expect(document.activeElement).toBe(week);
  });
});

describe("Tabs", () => {
  it("moves between tabs with the arrows and shows the matching panel", () => {
    render(
      <Tabs defaultValue="changes">
        <TabsList aria-label="Panel">
          <TabsTab value="changes">Changes</TabsTab>
          <TabsTab value="files">Files</TabsTab>
        </TabsList>
        <TabsPanel value="changes">Diff</TabsPanel>
        <TabsPanel value="files">Tree</TabsPanel>
      </Tabs>,
    );
    expect(screen.getByRole("tablist", { name: "Panel" })).toBeTruthy();
    const changes = screen.getByRole("tab", { name: "Changes" });
    expect(changes.getAttribute("aria-selected")).toBe("true");
    changes.focus();
    fireEvent.keyDown(changes, { key: "ArrowRight" });
    expect(screen.getByRole("tab", { name: "Files" }).getAttribute("aria-selected")).toBe("true");
    expect(screen.getByRole("tabpanel").textContent).toBe("Tree");
  });
});

describe("Checkbox and RadioGroup", () => {
  it("checks a named checkbox", () => {
    render(<Checkbox aria-label="Include unstaged" />);
    const box = screen.getByRole("checkbox", { name: "Include unstaged" });
    fireEvent.click(box);
    expect(box.getAttribute("aria-checked")).toBe("true");
  });

  it("moves the checked radio with the arrows", () => {
    render(
      <RadioGroup aria-label="Access" defaultValue="read">
        <Radio value="read" aria-label="Read" />
        <Radio value="write" aria-label="Write" />
      </RadioGroup>,
    );
    const read = screen.getByRole("radio", { name: "Read" });
    read.focus();
    fireEvent.keyDown(read, { key: "ArrowDown" });
    expect(screen.getByRole("radio", { name: "Write" }).getAttribute("aria-checked")).toBe("true");
  });
});

describe("Collapsible", () => {
  it("opens its panel from the trigger and reports it with aria-expanded", () => {
    render(
      <Collapsible>
        <CollapsibleTrigger>Details</CollapsibleTrigger>
        <CollapsiblePanel>Log</CollapsiblePanel>
      </Collapsible>,
    );
    const trigger = screen.getByRole("button", { name: "Details" });
    expect(trigger.getAttribute("aria-expanded")).toBe("false");
    fireEvent.click(trigger);
    expect(trigger.getAttribute("aria-expanded")).toBe("true");
    expect(screen.getByText("Log")).toBeTruthy();
  });
});
