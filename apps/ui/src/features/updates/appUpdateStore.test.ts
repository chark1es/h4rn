import { beforeEach, describe, expect, it } from "vitest";

import { setLocalePreference } from "@/lib/i18n";
import type { AppUpdateStatus } from "@/lib/platform/appUpdate";

import {
  applyStatus,
  appUpdateErrorText,
  getAppUpdateState,
  noticeVisible,
  resetAppUpdateStoreForTests,
  snoozeAppUpdate,
} from "./appUpdateStore";

function status(overrides: Partial<AppUpdateStatus> = {}): AppUpdateStatus {
  return {
    current_version: "1.1.0",
    available: { version: "1.2.0", notes: null },
    checked_at_ms: 1_700_000_000_000,
    auto_check: true,
    error: null,
    ...overrides,
  };
}

describe("app update store", () => {
  beforeEach(() => resetAppUpdateStoreForTests());

  it("shows nothing until the shell says it can update", () => {
    applyStatus(status());
    expect(getAppUpdateState().availableVersion).toBe("1.2.0");
    // Web and mobile never set `supported`, so no notice.
    expect(noticeVisible(getAppUpdateState())).toBe(false);
  });

  it("hides the notice on Later and brings it back on the next check", () => {
    applyStatus(status());
    // Pretend the desktop shell connected.
    Object.assign(getAppUpdateState(), { supported: true });
    expect(noticeVisible(getAppUpdateState())).toBe(true);

    snoozeAppUpdate();
    expect(noticeVisible(getAppUpdateState())).toBe(false);

    applyStatus(status({ checked_at_ms: 1_700_000_900_000 }));
    expect(getAppUpdateState().snoozed).toBe(false);
  });

  it("has no notice when the endpoint offers nothing newer", () => {
    applyStatus(status({ available: null }));
    expect(getAppUpdateState().availableVersion).toBeNull();
  });

  it("keeps a failed check as a quiet message, not a notice", () => {
    applyStatus(status({ available: null, error: "Couldn't reach the update server." }));
    expect(getAppUpdateState().error).toEqual({
      kind: "sent",
      text: "Couldn't reach the update server.",
    });
    expect(noticeVisible(getAppUpdateState())).toBe(false);
  });

  it("words its own failed check in the active language, the shell's text as sent", async () => {
    await setLocalePreference("ja");
    try {
      expect(appUpdateErrorText({ kind: "unreachable" })).toBe(
        "アップデートサーバーに接続できませんでした。",
      );
      expect(appUpdateErrorText({ kind: "sent", text: "shell said no" })).toBe("shell said no");
    } finally {
      await setLocalePreference("system");
    }
  });
});
