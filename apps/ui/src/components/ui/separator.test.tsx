// @vitest-environment jsdom

import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";

import { Badge } from "./badge";
import { Separator } from "./separator";
import { Skeleton } from "./skeleton";

afterEach(cleanup);

describe("Separator", () => {
  it("is only visual by default", () => {
    const { container } = render(<Separator />);
    const line = container.querySelector('[data-slot="separator"]');
    expect(line?.getAttribute("role")).toBe("none");
    expect(line?.getAttribute("data-orientation")).toBe("horizontal");
    expect(screen.queryByRole("separator")).toBeNull();
  });

  it("is announced with its orientation when it divides content", () => {
    render(<Separator decorative={false} orientation="vertical" />);
    const line = screen.getByRole("separator");
    expect(line.getAttribute("aria-orientation")).toBe("vertical");
    expect(line.getAttribute("data-orientation")).toBe("vertical");
  });
});

describe("Badge and Skeleton", () => {
  it("label their slot and variant, and hide the skeleton from screen readers", () => {
    const { container } = render(
      <>
        <Badge variant="ok">Merged</Badge>
        <Skeleton className="h-4 w-20" />
      </>,
    );
    const badge = screen.getByText("Merged");
    expect(badge.dataset.slot).toBe("badge");
    expect(badge.dataset.variant).toBe("ok");
    expect(container.querySelector('[data-slot="skeleton"]')?.getAttribute("aria-hidden")).toBe(
      "true",
    );
  });
});
