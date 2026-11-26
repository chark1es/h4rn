// @vitest-environment jsdom

import { cleanup, render, screen } from "@testing-library/react";
import { InboxIcon } from "lucide-react";
import { afterEach, describe, expect, it } from "vitest";

import { setLocalePreference } from "@/lib/i18n";

import { EmptyState } from "./EmptyState";

afterEach(async () => {
  cleanup();
  await setLocalePreference("en");
});

describe("EmptyState", () => {
  it("words the shortcut line in the active language and keeps the keycap", async () => {
    await setLocalePreference("ja");
    const { container } = render(
      <EmptyState icon={InboxIcon} title="空" description="なし" shortcut="Mod+N" />,
    );
    const line = container.querySelector("kbd")?.parentElement;
    expect(line?.textContent).toMatch(/^または .+ を押します$/);
    expect(container.querySelector("kbd")?.getAttribute("aria-label")).toBeTruthy();
  });

  it("reads 'or press' in English", () => {
    render(<EmptyState icon={InboxIcon} title="Empty" description="None" shortcut="Mod+N" />);
    expect(screen.getByText(/or press/)).toBeTruthy();
  });
});
