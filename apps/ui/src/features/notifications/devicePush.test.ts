import { describe, expect, it } from "vitest";

import { type DeviceAlertsState, deviceStatusLine } from "./devicePush";

function state(patch: Partial<DeviceAlertsState> = {}): DeviceAlertsState {
  return {
    supported: true,
    available: true,
    permission: "granted",
    enabled: false,
    registered: false,
    configured: true,
    token: "token",
    error: null,
    ...patch,
  };
}

describe("device alert settings states", () => {
  it("asks for a Connect sign-in before anything else", () => {
    expect(deviceStatusLine(state({ enabled: true }), false)).toBe("Sign in to Connect first.");
  });

  it("reports a build without Firebase rather than pretending", () => {
    expect(deviceStatusLine(state({ available: false }), true)).toBe("Unavailable in this build.");
  });

  it("says when Android blocked notifications", () => {
    expect(deviceStatusLine(state({ permission: "denied" }), true)).toBe(
      "Blocked in system settings.",
    );
  });

  it("distinguishes not asked yet from off", () => {
    expect(deviceStatusLine(state({ permission: "default" }), true)).toContain(
      "will ask for permission",
    );
    expect(deviceStatusLine(state(), true)).toBe("Off.");
  });

  it("says when registration is real", () => {
    expect(deviceStatusLine(state({ enabled: true, registered: true }), true)).toBe(
      "Registered with this phone.",
    );
    expect(deviceStatusLine(state({ enabled: true }), true)).toContain("has not confirmed");
  });
});
