// @vitest-environment jsdom

import { beforeEach, describe, expect, it } from "vitest";

import {
  getThreadSettings,
  setDefaultPermissionMode,
  THREAD_SETTINGS_DEFAULTS,
} from "@/features/threads/settings";

describe("thread settings", () => {
  beforeEach(() => window.localStorage.clear());

  it("starts on approval required", () => {
    expect(getThreadSettings().defaultPermissionMode).toBe(
      THREAD_SETTINGS_DEFAULTS.defaultPermissionMode,
    );
  });

  it("remembers the default permission mode on this device", () => {
    setDefaultPermissionMode("plan");
    expect(getThreadSettings().defaultPermissionMode).toBe("plan");
    expect(window.localStorage.getItem("h4rn.threads")).toContain('"plan"');
  });
});
