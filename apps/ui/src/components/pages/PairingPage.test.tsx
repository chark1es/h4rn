// @vitest-environment jsdom

import { act, cleanup, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

vi.mock("@/lib/store/appStore", () => ({ navigate: vi.fn() }));

import { pairMachine, remoteStore } from "@/features/remote/remoteStore";
import { setLocalePreference } from "@/lib/i18n";

import { PairingPage } from "./PairingPage";

const initial = remoteStore.getState();

afterEach(async () => {
  cleanup();
  remoteStore.setState(initial);
  await act(() => setLocalePreference("system"));
  window.localStorage.clear();
});

function sasState(waiting: boolean) {
  remoteStore.setState({
    ...initial,
    sasConfirm: {
      sas: "1234 5678 9012",
      machineName: "atlas",
      method: "qr",
      waiting,
      active: {
        connectUrl: "https://relay.test",
        machineId: "machine-atlas",
        machineName: "atlas",
        daemonNoiseKey: "00".repeat(32),
        addresses: [],
        online: true,
        relay: true,
      },
    },
  });
}

describe("Pairing page", () => {
  it("reads the same in English as before", () => {
    render(<PairingPage />);
    expect(screen.getByRole("main", { name: "Pair a host" })).toBeTruthy();
    expect(screen.getByRole("heading", { name: "Scan a pairing code" })).toBeTruthy();
    expect(screen.getByRole("textbox", { name: "Pairing code" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "Pair with code" })).toBeTruthy();
    expect(screen.getByText("Codes work once and expire in 5 minutes")).toBeTruthy();
    expect(
      screen.getByText(
        "Credentials stay on the host. You can revoke this phone from any other device.",
      ),
    ).toBeTruthy();
  });

  it("switches language in place, code shape and product names untouched", async () => {
    render(<PairingPage />);
    await act(() => setLocalePreference("es"));
    expect(screen.getByRole("main", { name: "Vincular un host" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "Vincular con código" })).toBeTruthy();
    expect(screen.getByText("Los códigos sirven una vez y caducan en 5 minutos")).toBeTruthy();
    expect(screen.getByText("Inicia sesión en h4rn-connect antes de vincular")).toBeTruthy();
    const input = screen.getByRole("textbox", { name: "Código de vinculación" });
    expect(input.getAttribute("placeholder")).toBe("ABCD-EFGH");

    await act(() => setLocalePreference("ja"));
    expect(screen.getByRole("main", { name: "ホストとペアリング" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "コードでペアリング" })).toBeTruthy();
    expect(screen.queryByText(/Pair with code/)).toBeNull();
  });

  it("names a linked host's scopes with Settings' words, in the active language", async () => {
    await act(() => setLocalePreference("es"));
    const expires = Math.floor((Date.now() + 10 * 60_000) / 1_000);
    // Signed out, so the claim stops at once and keeps the link for the page.
    await act(async () => {
      await pairMachine(
        `h4rn://pair?c=https://relay.test&m=atlas&k=bW9jay1ub2lzZQ&p=view,terminal&e=${expires}`,
      );
    });
    render(<PairingPage />);

    expect(screen.getByRole("main", { name: "Vincular con atlas" })).toBeTruthy();
    expect(screen.getByText("Ver hilos")).toBeTruthy();
    expect(screen.getByText("Siempre incluido")).toBeTruthy();
    expect(screen.getByText("Escribir en shells de atlas")).toBeTruthy();
    expect(screen.getByText("Gestionar hosts y dispositivos")).toBeTruthy();
    expect(screen.getAllByText("No incluido")).toHaveLength(3);
    expect(screen.getAllByText("Incluido")).toHaveLength(2);
    expect(screen.getByText("relay.test · por relay")).toBeTruthy();
    // The store's error is written in the language that was active.
    expect(
      screen.getByText(
        "Inicia sesión en h4rn-connect primero y vuelve a escanear el código de vinculación.",
      ),
    ).toBeTruthy();
    expect(screen.queryByText("View threads")).toBeNull();
  });

  it("asks to compare the code in the active language", async () => {
    sasState(false);
    render(<PairingPage />);
    expect(screen.getByRole("main", { name: "Check the code for atlas" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "They match" })).toBeTruthy();

    await act(() => setLocalePreference("zh-CN"));
    expect(screen.getByRole("main", { name: "核对 atlas 的代码" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "一致" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "不一致" })).toBeTruthy();
    expect(screen.getByTestId("pair-sas").textContent).toBe("1234 5678 9012");
  });

  it("waits for the host in the active language", async () => {
    await act(() => setLocalePreference("ja"));
    sasState(true);
    render(<PairingPage />);
    expect(screen.getByTestId("pair-waiting").textContent).toBe("atlas を待っています");
    expect(screen.getByRole("button", { name: "ホストを待っています…" })).toBeTruthy();
  });
});
