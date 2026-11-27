// @vitest-environment jsdom

import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { Button } from "./button";

afterEach(cleanup);

describe("Button", () => {
  it("renders a native button named by its text, with its variant and size", () => {
    render(
      <Button variant="secondary" size="sm">
        Save
      </Button>,
    );
    const button = screen.getByRole("button", { name: "Save" });
    expect(button.tagName).toBe("BUTTON");
    expect(button.getAttribute("type")).toBe("button");
    expect(button.dataset.slot).toBe("button");
    expect(button.dataset.variant).toBe("secondary");
    expect(button.dataset.size).toBe("sm");
    expect(button.getAttribute("aria-disabled")).toBeNull();
  });

  it("runs its handler on a click", () => {
    const onClick = vi.fn();
    render(<Button onClick={onClick}>Start</Button>);
    fireEvent.click(screen.getByRole("button", { name: "Start" }));
    expect(onClick).toHaveBeenCalledTimes(1);
  });

  it("does nothing and leaves the tab order when disabled", () => {
    const onClick = vi.fn();
    render(
      <Button disabled onClick={onClick}>
        Start
      </Button>,
    );
    const button = screen.getByRole("button", { name: "Start" }) as HTMLButtonElement;
    expect(button.disabled).toBe(true);
    expect(button.hasAttribute("data-disabled")).toBe(true);
    fireEvent.click(button);
    expect(onClick).not.toHaveBeenCalled();
  });

  it("submits its form when it is a submit button", () => {
    const onSubmit = vi.fn((event: SubmitEvent) => event.preventDefault());
    render(
      <form onSubmit={(event) => onSubmit(event.nativeEvent as SubmitEvent)}>
        <Button type="submit">Send</Button>
      </form>,
    );
    fireEvent.click(screen.getByRole("button", { name: "Send" }));
    expect(onSubmit).toHaveBeenCalledTimes(1);
  });

  it("keeps link semantics when it renders a link", () => {
    render(
      <Button variant="link" render={<a href="#docs" />}>
        Docs
      </Button>,
    );
    const link = screen.getByRole("link", { name: "Docs" });
    expect(link.getAttribute("href")).toBe("#docs");
    expect(link.dataset.slot).toBe("button");
  });

  it("lets a caller's class win over the variant's", () => {
    render(<Button className="h-9">Wide</Button>);
    const button = screen.getByRole("button", { name: "Wide" });
    expect(button.className).toContain("h-9");
    expect(button.className).not.toMatch(/(^|\s)h-7(\s|$)/);
  });
});
