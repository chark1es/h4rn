// @vitest-environment jsdom

import { act, cleanup, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it } from "vitest";

import { setLocalePreference } from "@/lib/i18n";
import type { AppUpdateProgress } from "@/lib/platform/appUpdate";

import { AppUpdateNotice } from "./AppUpdateNotice";
import { applyStatus, getAppUpdateState, resetAppUpdateStoreForTests } from "./appUpdateStore";

/** The desktop shell reported 1.1.0, with the given progress. */
function offer(progress: AppUpdateProgress | null = null): void {
  Object.assign(getAppUpdateState(), { supported: true, progress });
  applyStatus({
    current_version: "1.0.1",
    available: { version: "1.1.0", notes: null },
    checked_at_ms: 1_700_000_000_000,
    auto_check: true,
    error: null,
  });
}

function notice(): string {
  return screen.getByRole("status").textContent ?? "";
}

beforeEach(() => resetAppUpdateStoreForTests());

afterEach(async () => {
  cleanup();
  resetAppUpdateStoreForTests();
  await setLocalePreference("en");
});

describe("AppUpdateNotice", () => {
  it("reads as before in English", () => {
    offer();
    render(<AppUpdateNotice />);
    expect(notice()).toBe("h4rn 1.1.0 is availableRestart to updateLater");
  });

  it("words the offer and its buttons in the active language", async () => {
    offer();
    render(<AppUpdateNotice />);
    await act(() => setLocalePreference("es"));
    expect(notice()).toBe("h4rn 1.1.0 está disponibleReiniciar para actualizarMás tarde");
    expect(screen.getByRole("status").querySelector("b")?.textContent).toBe("h4rn 1.1.0");
  });

  it("formats the download's progress for the language", async () => {
    await setLocalePreference("es");
    offer({ phase: "downloading", downloaded: 42, total: 100 });
    render(<AppUpdateNotice />);
    // Spanish puts a no-break space before the sign.
    expect(notice()).toMatch(/^Descargando h4rn 1\.1\.0 · 42\s%$/);
  });

  it("says it is installing, then restarting", async () => {
    await setLocalePreference("ja");
    offer({ phase: "installing" });
    render(<AppUpdateNotice />);
    expect(notice()).toBe("h4rn 1.1.0 をインストール中 · 再起動します");
  });

  it("words the client's own install failure and shows the shell's as sent", async () => {
    offer({ phase: "failed", message: null });
    render(<AppUpdateNotice />);
    await act(() => setLocalePreference("zh-CN"));
    expect(notice()).toContain("无法安装更新。");
    expect(screen.getByRole("button", { name: "再试一次" })).toBeTruthy();

    cleanup();
    offer({ phase: "failed", message: "Signature mismatch." });
    render(<AppUpdateNotice />);
    expect(notice()).toContain("Signature mismatch.");
  });
});
