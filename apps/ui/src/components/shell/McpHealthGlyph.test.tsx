// @vitest-environment jsdom

import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";

import { setLocalePreference } from "@/lib/i18n";
import { McpHealthGlyph } from "./Sidebar";

afterEach(async () => {
  cleanup();
  await setLocalePreference("en");
});

describe("McpHealthGlyph", () => {
  it("picks the key icon from the status kind, not its English words", async () => {
    await setLocalePreference("ja");
    render(
      <McpHealthGlyph
        status={{ kind: "needsCredential", tone: "warn", label: "認証情報が必要" }}
      />,
    );
    const glyph = screen.getByRole("img", { name: "認証情報が必要" });
    expect(glyph.getAttribute("class")).toContain("lucide-key-round");
  });

  it("keeps the alert icon for other warnings", () => {
    render(<McpHealthGlyph status={{ kind: "starting", tone: "warn", label: "Starting" }} />);
    expect(screen.getByRole("img", { name: "Starting" }).getAttribute("class")).toContain(
      "lucide-circle-alert",
    );
  });
});
