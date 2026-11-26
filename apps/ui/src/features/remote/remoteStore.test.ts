import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const host = vi.hoisted(() => ({
  status: "offline" as "connecting" | "online" | "offline" | "error",
  restarts: 0,
  toasts: [] as { title?: string; description?: string }[],
}));

vi.mock("@/lib/store/appStore", () => ({
  pushToast: (toast: { title?: string; description?: string }) => host.toasts.push(toast),
  restartConnection: () => {
    host.restarts += 1;
  },
  navigate: vi.fn(),
  getState: () => ({ connection: { status: host.status } }),
}));

import type { MachineSummary, PairingInfo } from "@/gen";
import { setLocalePreference } from "@/lib/i18n";
import { remoteErrorText } from "./remoteError";
import {
  activateMachine,
  confirmPairingSas,
  getRemoteState,
  pairByCode,
  pairMachine,
  refresh,
  remoteStore,
} from "./remoteStore";
import { setConnectClient } from "./session";

const MACHINE: MachineSummary = {
  id: "machine-1",
  name: "atlas",
  online: true,
  last_seen: null,
  daemon_device_id: "daemon-1",
  hosts: [],
  addresses: ["relay:http://relay.test"],
};

const PAIRING: PairingInfo = {
  id: "pairing-1",
  machine_id: MACHINE.id,
  state: "claimed",
  created_at: new Date(0).toISOString(),
  expires_at: new Date(60_000).toISOString(),
  claimed_by_device_id: "phone",
  scopes: ["view", "approve"],
};

const PHONE_SAS = "1234 5678 9012";

function keyB64(byte: number): string {
  return Buffer.alloc(32, byte).toString("base64url");
}

function qrLink(): string {
  return (
    `h4rn://pair?c=${encodeURIComponent("http://relay.test")}` +
    `&m=${MACHINE.id}&k=${keyB64(0xaa)}&e=${Math.floor(Date.now() / 1_000) + 60}` +
    `&p=view,approve`
  );
}

function fakeClient(claimedDaemonKeyByte = 0xbb) {
  const client = {
    connectUrl: "http://relay.test",
    device: { id: "phone" },
    claimPairing: vi.fn(async () => ({
      pairing: PAIRING,
      machine: MACHINE,
      daemon_noise_public_key: keyB64(claimedDaemonKeyByte),
    })),
    claimPairingCode: vi.fn(async () => ({
      pairing: PAIRING,
      machine: MACHINE,
      daemon_noise_public_key: keyB64(claimedDaemonKeyByte),
    })),
    pairingSas: vi.fn(async () => PHONE_SAS),
    me: vi.fn(async () => ({
      user: { id: "user-1", email: null, display_name: "Dev", avatar_url: null },
      devices: [{ id: "phone" }],
    })),
    machines: vi.fn(async () => [MACHINE]),
    linkedMachines: vi.fn(async () => ({ supported: true, machines: [MACHINE] })),
    listPairings: vi.fn(async () => [PAIRING]),
  };
  return client;
}

function signedIn() {
  remoteStore.setState({
    ...remoteStore.getState(),
    phase: "signed_in",
    connectUrl: "http://relay.test",
    user: { id: "user-1", email: null, display_name: "Dev", avatar_url: null },
    machines: [MACHINE],
    linkedMachines: [],
    linkedSupported: null,
    lastSyncAt: null,
    active: null,
    activeMachineId: null,
    sasConfirm: null,
    busy: null,
    error: null,
  });
}

async function waitUntilOnline(confirmation: Promise<boolean>) {
  host.status = "online";
  await vi.advanceTimersByTimeAsync(2_000);
  return confirmation;
}

beforeEach(() => {
  vi.useFakeTimers();
  host.status = "offline";
  host.restarts = 0;
  host.toasts = [];
  signedIn();
});

afterEach(() => {
  setConnectClient(null);
  vi.useRealTimers();
});

describe("QR pairing", () => {
  it("shows the SAS derived from the QR's pinned key and does not activate", async () => {
    const client = fakeClient();
    setConnectClient(client as never);

    const paired = await pairMachine(qrLink());

    expect(paired).toBe(false);
    expect(getRemoteState().active).toBeNull();
    expect(getRemoteState().sasConfirm).toMatchObject({
      sas: PHONE_SAS,
      machineName: "atlas",
      method: "qr",
      waiting: false,
    });
    // The QR's key, not the key connect echoed back, is what was compared.
    expect(client.pairingSas).toHaveBeenCalledWith("aa".repeat(32));
  });

  it("waits for host approval and only then reports the pairing", async () => {
    const client = fakeClient();
    setConnectClient(client as never);
    expect(await pairMachine(qrLink())).toBe(false);

    const confirmation = confirmPairingSas();
    await vi.advanceTimersByTimeAsync(0);
    expect(host.restarts).toBeGreaterThan(0);
    expect(getRemoteState().sasConfirm?.waiting).toBe(true);
    expect(getRemoteState().active).not.toBeNull();

    expect(await waitUntilOnline(confirmation)).toBe(true);
    expect(getRemoteState().sasConfirm).toBeNull();
    expect(host.toasts.at(-1)?.title).toContain("Paired with atlas");
  });

  it("does not report success when the host never approves", async () => {
    const client = fakeClient();
    setConnectClient(client as never);
    expect(await pairMachine(qrLink())).toBe(false);

    const confirmation = confirmPairingSas();
    await vi.advanceTimersByTimeAsync(5 * 60_000 + 2_000);

    expect(await confirmation).toBe(false);
    expect(getRemoteState().sasConfirm).toMatchObject({ method: "qr", waiting: false });
    expect(getRemoteState().error).toEqual({ kind: "notApproved", params: { name: "atlas" } });
    expect(remoteErrorText(getRemoteState().error ?? { kind: "sent", text: "" })).toContain(
      "has not approved this device yet",
    );
    expect(host.toasts.some((toast) => toast.title?.includes("Paired with"))).toBe(false);
  });
});

describe("typed-code pairing", () => {
  it("shows the SAS from the claimed daemon key and does not activate", async () => {
    const client = fakeClient();
    setConnectClient(client as never);

    const paired = await pairByCode("ABCD-EFGH");

    expect(paired).toBe(false);
    expect(getRemoteState().active).toBeNull();
    expect(getRemoteState().sasConfirm).toMatchObject({
      sas: PHONE_SAS,
      machineName: "atlas",
      method: "code",
      waiting: false,
    });
    expect(client.pairingSas).toHaveBeenCalledWith("bb".repeat(32));
  });

  it("waits for host approval and only then reports the pairing", async () => {
    const client = fakeClient();
    setConnectClient(client as never);
    expect(await pairByCode("ABCD-EFGH")).toBe(false);

    const confirmation = confirmPairingSas();
    await vi.advanceTimersByTimeAsync(0);
    expect(getRemoteState().sasConfirm?.waiting).toBe(true);

    expect(await waitUntilOnline(confirmation)).toBe(true);
    expect(getRemoteState().sasConfirm).toBeNull();
    expect(getRemoteState().active?.machineId).toBe(MACHINE.id);
  });
});

describe("account refresh", () => {
  it("keeps the account when linked presence cannot be fetched", async () => {
    const client = fakeClient();
    client.linkedMachines = vi.fn(async () => {
      throw new Error("the relay rejected the proof");
    });
    setConnectClient(client as never);

    await refresh();

    const state = getRemoteState();
    expect(state.phase).toBe("signed_in");
    expect(state.machines).toEqual([MACHINE]);
    expect(state.linkedMachines).toEqual([]);
    // A failed fetch says nothing about the relay: it must not read as an
    // older relay that ignored `?linked=true`.
    expect(state.linkedSupported).toBeNull();
    expect(state.error).toBeNull();
  });

  it("stays unknown while the first linked fetch is still pending", async () => {
    const client = fakeClient();
    client.linkedMachines = vi.fn(
      () => new Promise<never>(() => undefined),
    ) as unknown as typeof client.linkedMachines;
    setConnectClient(client as never);

    // The cached sign-in already says signed_in; the relay has not answered
    // about linked machines yet, so this must not read as an older relay.
    void refresh();
    await vi.advanceTimersByTimeAsync(0);

    const state = getRemoteState();
    expect(state.phase).toBe("signed_in");
    expect(state.linkedSupported).toBeNull();
  });

  it("does not treat an older relay's unfiltered list as this device's links", async () => {
    const client = fakeClient();
    // An older relay ignores `?linked=true`; the client sees no filter marker
    // and must not turn the whole account into presence rows.
    client.linkedMachines = vi.fn(async () => ({ supported: false, machines: [] }));
    setConnectClient(client as never);

    await refresh();

    const state = getRemoteState();
    expect(state.phase).toBe("signed_in");
    expect(state.machines).toEqual([MACHINE]);
    expect(state.linkedMachines).toEqual([]);
    expect(state.linkedSupported).toBe(false);
  });

  it("stores the linked machines the relay answered with", async () => {
    const client = fakeClient();
    setConnectClient(client as never);

    await refresh();

    expect(getRemoteState().linkedMachines).toEqual([MACHINE]);
    expect(getRemoteState().linkedSupported).toBe(true);
  });
});

describe("selecting a machine", () => {
  afterEach(() => setLocalePreference("system"));

  it("explains a missing host key in the active language, keeping the command", async () => {
    await setLocalePreference("ja");
    // The account lists the machine, but its daemon has not published a key.
    setConnectClient(fakeClient() as never);

    const selected = await activateMachine(MACHINE.id);

    expect(selected).toBe(false);
    const expected =
      "atlas はまだ接続用の鍵を公開していません。そのマシンで h4rnd login をもう一度実行してください。";
    expect(getRemoteState().error).toEqual({ kind: "noNoiseKey", params: { name: "atlas" } });
    expect(remoteErrorText(getRemoteState().error ?? { kind: "sent", text: "" })).toBe(expected);
    expect(host.toasts.at(-1)).toEqual({
      variant: "error",
      title: "そのマシンを選択できませんでした。",
      description: expected,
    });
  });
});
