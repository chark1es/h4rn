// @vitest-environment jsdom

import { act, cleanup, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("./connect", () => ({ listProviders: () => Promise.resolve([]) }));

import { setLocalePreference } from "@/lib/i18n";

import { busyText, ConnectSignIn } from "./ConnectSignIn";
import { type RemoteBusy, remoteStore } from "./remoteStore";

const initial = remoteStore.getState();

beforeEach(() => {
  remoteStore.setState({
    ...initial,
    phase: "signing_in",
    login: {
      grantId: "grant-1",
      userCode: "ABCD-EFGH",
      verificationUri: "https://relay.test/device",
      verificationUriComplete: null,
      expiresAt: Date.now() + 60_000,
    },
    busy: { kind: "waitingForApproval" },
  });
});

afterEach(async () => {
  cleanup();
  remoteStore.setState(initial);
  await act(() => setLocalePreference("system"));
  window.localStorage.clear();
});

const KINDS: RemoteBusy[] = [
  { kind: "openingSignIn" },
  { kind: "completingSignIn" },
  { kind: "requestingCode" },
  { kind: "waitingForApproval" },
  { kind: "claimingPairing" },
  { kind: "generatingCode" },
  { kind: "connecting", machineId: "machine-1", machineName: "atlas" },
];

describe("Connect sign-in progress", () => {
  it("shows the store's step in the reader's language", async () => {
    await act(() => setLocalePreference("ja"));
    render(<ConnectSignIn />);
    expect(await screen.findByText("承認を待っています…")).toBeTruthy();
    expect(screen.queryByText(/Waiting for approval/)).toBeNull();

    act(() =>
      remoteStore.setState({ ...remoteStore.getState(), busy: { kind: "requestingCode" } }),
    );
    expect(screen.getByText("コードをリクエストしています…")).toBeTruthy();
  });

  it("has words for every step in every language", async () => {
    const english = KINDS.map(busyText);
    expect(english).toContain("Connecting to atlas…");
    for (const locale of ["zh-CN", "es", "ja"] as const) {
      await act(() => setLocalePreference(locale));
      const words = KINDS.map(busyText);
      words.forEach((text, index) => {
        expect(text, `${locale} ${KINDS[index]?.kind}`).not.toBe(english[index]);
      });
      expect(words.at(-1)).toContain("atlas");
    }
  });
});
