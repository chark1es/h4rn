// @vitest-environment jsdom

import { afterEach, describe, expect, it } from "vitest";

import { HARNESS_LABELS, threadHarnessLabel } from "@/lib/entities";
import { setLocalePreference } from "@/lib/i18n";

import { normaliseDaemon } from "./mobile";
import { parsePairingUrl } from "./pairingUrl";

afterEach(() => setLocalePreference("en"));

describe("the phone shell's own words", () => {
  it("checks a daemon URL in the active language", async () => {
    expect(() => normaliseDaemon("  ")).toThrow("Enter a daemon URL.");
    await setLocalePreference("es");
    expect(() => normaliseDaemon("  ")).toThrow("Escribe la URL del daemon.");
    expect(() => normaliseDaemon("ftp://host:47800")).toThrow(
      "La URL del daemon debe ser ws:// o wss:// (http/https también sirven).",
    );
    expect(normaliseDaemon("10.0.2.2:47800")).toBe("ws://10.0.2.2:47800/ws");
  });

  it("refuses a half pairing link in the active language", async () => {
    await setLocalePreference("ja");
    expect(() => parsePairingUrl("h4rn://connect?daemon=ws%3A%2F%2Fx")).toThrow(
      "ペアリングリンクに daemon または token がありません。",
    );
  });
});

describe("harness names", () => {
  it("words the generic ACP harness and keeps product names", async () => {
    expect(HARNESS_LABELS.acp).toBe("ACP agent");
    await setLocalePreference("zh-CN");
    expect(HARNESS_LABELS.acp).toBe("ACP 智能体");
    expect(threadHarnessLabel("acp")).toBe("ACP 智能体");
    expect(threadHarnessLabel("acp", "goose")).toBe("ACP · goose");
    expect(HARNESS_LABELS.claude_code).toBe("Claude Code");
  });
});
