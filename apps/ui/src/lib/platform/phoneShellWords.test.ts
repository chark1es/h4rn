// @vitest-environment jsdom

import { afterEach, describe, expect, it, vi } from "vitest";

import { HARNESS_LABELS, threadHarnessLabel } from "@/lib/entities";
import { setLocalePreference } from "@/lib/i18n";

import { normaliseDaemon, renderConnect } from "./mobile";
import { parsePairingUrl } from "./pairingUrl";

afterEach(async () => {
  document.getElementById("h4rn-connect")?.remove();
  await setLocalePreference("en");
});

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
      "このペアリングリンクにはデーモンのアドレスまたはトークンがありません。ホストからリンク全体をもう一度コピーしてください。",
    );
  });

  it("words the first-launch connect form and its errors in the active language", async () => {
    await setLocalePreference("ja");
    renderConnect();
    const overlay = document.getElementById("h4rn-connect");
    if (!overlay) throw new Error("the connect form was not drawn");
    expect(overlay.querySelector("h1")?.textContent).toBe("このスマートフォンを接続");
    expect([...overlay.querySelectorAll("label")].map((label) => label.textContent)).toEqual([
      "デーモンの URL",
      "アクセストークン",
    ]);
    expect(overlay.querySelector("input[type=password]")?.getAttribute("placeholder")).toBe(
      "64 文字の 16 進トークン",
    );
    const buttons = [...overlay.querySelectorAll("button")].map((button) => button.textContent);
    expect(buttons).toEqual(["接続", "QR をスキャン"]);

    // A half pairing link pasted into the URL field says what to do next.
    const daemon = overlay.querySelector<HTMLInputElement>("input[type=text]");
    if (!daemon) throw new Error("the daemon field was not drawn");
    daemon.value = "h4rn://connect?daemon=ws%3A%2F%2Fx";
    overlay.querySelector("form")?.dispatchEvent(new Event("submit", { cancelable: true }));
    await vi.waitFor(() =>
      expect(overlay.querySelector("form p")?.textContent).toBe(
        "このペアリングリンクにはデーモンのアドレスまたはトークンがありません。ホストからリンク全体をもう一度コピーしてください。",
      ),
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
