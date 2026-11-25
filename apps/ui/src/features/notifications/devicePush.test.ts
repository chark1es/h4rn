// @vitest-environment jsdom

import { afterEach, describe, expect, it, vi } from "vitest";

import type { ConnectClient } from "@/features/remote/connect";
import { remoteStore } from "@/features/remote/remoteStore";
import { setConnectClient } from "@/features/remote/session";

import {
  type DeviceAlertsState,
  deregisterDeviceAlertsOnSignOut,
  deviceAvailabilityNote,
  deviceStatusLine,
  deviceToggleUsable,
  disableDeviceAlerts,
  enableDeviceAlerts,
  readDeviceAlerts,
  reconcileDeviceDeregistration,
  registerDeviceAlerts,
} from "./devicePush";

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

function stubAndroidShell(): ReturnType<typeof vi.fn> {
  vi.spyOn(window.navigator, "userAgent", "get").mockReturnValue(
    "Mozilla/5.0 (Linux; Android 14; Pixel 8)",
  );
  const invoke = vi.fn(async (command: string) => {
    if (command !== "push_status") return null;
    return {
      available: true,
      permission: "granted",
      token: "token",
      channel: true,
      appVersion: "1.2.3",
    };
  });
  vi.stubGlobal("__TAURI_INTERNALS__", { invoke });
  return invoke;
}

/** The shell bridge the test just installed. */
function shellInvoke(): ReturnType<typeof vi.fn> {
  return (window as unknown as { __TAURI_INTERNALS__: { invoke: ReturnType<typeof vi.fn> } })
    .__TAURI_INTERNALS__.invoke;
}

afterEach(() => {
  setConnectClient(null);
  window.localStorage.clear();
  remoteStore.setState({ ...remoteStore.getState(), phase: "loading" });
  vi.unstubAllGlobals();
});

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

  it("does not blame the operator when a sender is configured", () => {
    expect(deviceAvailabilityNote(state({ configured: true }))).toBeNull();
  });

  it("names a missing sender when the relay says there is none", () => {
    expect(deviceAvailabilityNote(state({ configured: false }))).toContain(
      "no push sender configured",
    );
  });

  it("reports an unknown sender as unknown, not as missing", () => {
    const note = deviceAvailabilityNote(state({ configured: null, error: "offline" }));
    expect(note).toContain("Could not check");
    expect(note).not.toContain("no push sender configured");
  });

  it("keeps the switch usable when the shell could not answer", () => {
    // A failed status call is not a build without Firebase, so it must not
    // disable the switch and leave the person stuck on "Unavailable".
    expect(deviceToggleUsable(state({ available: null }), true, false)).toBe(true);
    expect(deviceToggleUsable(state({ available: false }), true, false)).toBe(false);
    expect(deviceToggleUsable(state({ permission: "denied" }), true, false)).toBe(false);
    expect(deviceToggleUsable(state(), false, false)).toBe(false);
    expect(deviceToggleUsable(state(), true, true)).toBe(false);
    expect(deviceToggleUsable(null, true, false)).toBe(false);
  });
});

describe("reading this phone's real alert state", () => {
  it("uses the caller's own registration, not the account's device count", async () => {
    stubAndroidShell();
    window.localStorage.setItem("h4rn.push.deviceAlerts", "1");
    const pushStatus = vi.fn(async () => ({
      configured: true,
      devices: 3,
      registered: false,
    }));
    setConnectClient({ pushStatus } as unknown as ConnectClient);

    const device = await readDeviceAlerts();

    expect(device.registered).toBe(false);
    expect(device.enabled).toBe(true);
    expect(device.configured).toBe(true);
    expect(deviceStatusLine(device, true)).toContain("has not confirmed");
  });

  it("keeps a failed status call unknown instead of claiming no sender", async () => {
    stubAndroidShell();
    const pushStatus = vi.fn(async () => {
      throw new Error("connect is unreachable");
    });
    setConnectClient({ pushStatus } as unknown as ConnectClient);

    const device = await readDeviceAlerts();

    expect(device.configured).toBeNull();
    expect(device.error).toContain("unreachable");
    expect(deviceAvailabilityNote(device)).toContain("Could not check");
  });

  it("reports a shell status failure as unknown and asks to try again", async () => {
    stubAndroidShell();
    shellInvoke().mockImplementation(async (command: string) => {
      if (command === "push_status") throw new Error("bridge busy");
      return null;
    });

    const device = await readDeviceAlerts();

    expect(device.available).toBeNull();
    expect(device.error).toContain("bridge busy");
    expect(deviceStatusLine(device, true)).toContain("Try again");
    expect(deviceStatusLine(device, true)).not.toContain("Unavailable in this build");
  });
});

describe("turning device notifications off", () => {
  it("retries a failed removal until Connect confirms the row is gone", async () => {
    stubAndroidShell();
    window.localStorage.setItem("h4rn.push.deviceAlerts", "1");
    const deregisterPushDevice = vi.fn(async (): Promise<void> => {
      throw new Error("connect is unreachable");
    });
    const pushStatus = vi.fn(async () => ({ configured: true, devices: 1, registered: true }));
    setConnectClient({
      device: { id: "device-1" },
      deregisterPushDevice,
      pushStatus,
    } as unknown as ConnectClient);

    const off = await disableDeviceAlerts();

    expect(off.enabled).toBe(false);
    expect(deregisterPushDevice).toHaveBeenCalledOnce();
    expect(window.localStorage.getItem("h4rn.push.deregisterPending")).toBe("1");

    // The next signed-in visit (launch or visibility change) finishes it.
    deregisterPushDevice.mockImplementation(async () => undefined);
    await reconcileDeviceDeregistration();

    expect(deregisterPushDevice).toHaveBeenCalledTimes(2);
    expect(window.localStorage.getItem("h4rn.push.deregisterPending")).toBeNull();

    // Connect confirmed the row is gone; nothing more to do.
    await reconcileDeviceDeregistration();
    expect(deregisterPushDevice).toHaveBeenCalledTimes(2);
  });

  it("keeps the removal pending while the relay stays unreachable", async () => {
    stubAndroidShell();
    window.localStorage.setItem("h4rn.push.deregisterPending", "1");
    const deregisterPushDevice = vi.fn(async () => {
      throw new Error("connect is unreachable");
    });
    const pushStatus = vi.fn(async () => ({ configured: true, devices: 1, registered: true }));
    setConnectClient({
      device: { id: "device-1" },
      deregisterPushDevice,
      pushStatus,
    } as unknown as ConnectClient);

    await reconcileDeviceDeregistration();

    expect(deregisterPushDevice).toHaveBeenCalledOnce();
    expect(window.localStorage.getItem("h4rn.push.deregisterPending")).toBe("1");
  });

  it("does not try to remove anything without a pending marker", async () => {
    stubAndroidShell();
    const deregisterPushDevice = vi.fn(async () => undefined);
    setConnectClient({
      device: { id: "device-1" },
      deregisterPushDevice,
    } as unknown as ConnectClient);

    await reconcileDeviceDeregistration();

    expect(deregisterPushDevice).not.toHaveBeenCalled();
  });
});

describe("turning device notifications on", () => {
  it("shows a refused permission request and leaves the switch usable", async () => {
    stubAndroidShell();
    const invoke = shellInvoke();
    invoke.mockImplementation(async (command: string) => {
      if (command === "push_request_permission") throw new Error("permission dialog failed");
      if (command !== "push_status") return null;
      return {
        available: true,
        permission: "denied",
        token: null,
        channel: true,
        appVersion: "1.2.3",
      };
    });

    const refused = await enableDeviceAlerts();

    expect(refused.enabled).toBe(false);
    expect(refused.error).toContain("permission dialog failed");
    expect(deviceStatusLine(refused, true)).toBe("Blocked in system settings.");

    // The next attempt is not stuck behind the failure.
    invoke.mockImplementation(async (command: string) => {
      if (command === "push_token") return "fcm-token";
      if (command !== "push_status") return null;
      return {
        available: true,
        permission: "granted",
        token: "fcm-token",
        channel: true,
        appVersion: "1.2.3",
      };
    });
    const registerPushDevice = vi.fn(async () => undefined);
    setConnectClient({
      device: { id: "device-1" },
      registerPushDevice,
      pushStatus: vi.fn(async () => ({ configured: true, devices: 1, registered: true })),
    } as unknown as ConnectClient);

    const enabled = await enableDeviceAlerts();

    expect(registerPushDevice).toHaveBeenCalledOnce();
    expect(enabled.enabled).toBe(true);
    expect(enabled.error).toBeNull();
    expect(deviceStatusLine(enabled, true)).toBe("Registered with this phone.");
  });
});

describe("the native switch mirror", () => {
  function grantedShell(): void {
    stubAndroidShell();
    shellInvoke().mockImplementation(async (command: string) => {
      if (command === "push_token") return "fcm-token";
      if (command !== "push_status") return null;
      return {
        available: true,
        permission: "granted",
        token: "fcm-token",
        channel: true,
        appVersion: "1.2.3",
      };
    });
  }

  it("follows this phone's switch while signed in", async () => {
    grantedShell();
    remoteStore.setState({ ...remoteStore.getState(), phase: "signed_in" });
    setConnectClient({
      device: { id: "device-1" },
      registerPushDevice: vi.fn(async () => undefined),
      deregisterPushDevice: vi.fn(async () => undefined),
      pushStatus: vi.fn(async () => ({ configured: true, devices: 1, registered: true })),
    } as unknown as ConnectClient);

    await enableDeviceAlerts();
    const invoke = shellInvoke();
    expect(invoke).toHaveBeenCalledWith("push_set_enabled", { enabled: true });

    invoke.mockClear();
    await disableDeviceAlerts();
    expect(invoke).toHaveBeenCalledWith("push_set_enabled", { enabled: false });
  });

  it("turns off on sign-out and records the removal for the next sign-in", async () => {
    grantedShell();
    window.localStorage.setItem("h4rn.push.deviceAlerts", "1");
    remoteStore.setState({ ...remoteStore.getState(), phase: "signed_in" });
    const deregisterPushDevice = vi.fn(async (): Promise<void> => {
      throw new Error("connect is unreachable");
    });
    setConnectClient({
      device: { id: "device-1" },
      deregisterPushDevice,
    } as unknown as ConnectClient);

    deregisterDeviceAlertsOnSignOut();

    const invoke = shellInvoke();
    expect(invoke).toHaveBeenCalledWith("push_set_enabled", { enabled: false });
    expect(window.localStorage.getItem("h4rn.push.deregisterPending")).toBe("1");
    await vi.waitFor(() => expect(deregisterPushDevice).toHaveBeenCalledOnce());
    // The failed removal stays marked for the next signed-in launch.
    expect(window.localStorage.getItem("h4rn.push.deregisterPending")).toBe("1");
  });
});

describe("a registration that finishes after the switch went off", () => {
  it("keeps the removal pending and removes the late registration", async () => {
    stubAndroidShell();
    window.localStorage.setItem("h4rn.push.deviceAlerts", "1");
    remoteStore.setState({ ...remoteStore.getState(), phase: "signed_in" });

    let release!: () => void;
    const held = new Promise<void>((resolve) => {
      release = resolve;
    });
    const registerPushDevice = vi.fn(async () => held);
    const deregisterPushDevice = vi.fn(async (): Promise<void> => {
      throw new Error("connect is unreachable");
    });
    setConnectClient({
      device: { id: "device-1" },
      registerPushDevice,
      deregisterPushDevice,
      pushStatus: vi.fn(async () => ({ configured: true, devices: 1, registered: true })),
    } as unknown as ConnectClient);

    const registering = registerDeviceAlerts();
    await vi.waitFor(() => expect(registerPushDevice).toHaveBeenCalledOnce());

    // The person turns the switch off while the PUT is still in flight.
    const off = await disableDeviceAlerts();
    expect(off.enabled).toBe(false);
    expect(window.localStorage.getItem("h4rn.push.deregisterPending")).toBe("1");

    release();
    await registering;

    // The PUT's own success must not clear the marker: the removal ran again.
    expect(deregisterPushDevice).toHaveBeenCalledTimes(2);
    expect(window.localStorage.getItem("h4rn.push.deregisterPending")).toBe("1");
  });
});
