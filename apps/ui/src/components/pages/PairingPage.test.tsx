// @vitest-environment jsdom

import { act, cleanup, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

vi.mock("@/lib/store/appStore", () => ({ navigate: vi.fn() }));

import { pairMachine, remoteStore } from "@/features/remote/remoteStore";
import { i18n, setLocalePreference } from "@/lib/i18n";

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
    expect(screen.getByText("relay.test · vía relay")).toBeTruthy();
    expect(screen.getByRole("button", { name: "Vincular este teléfono" })).toBeTruthy();
    expect(
      screen.getByText(
        "Inicia sesión en h4rn-connect primero y vuelve a escanear el código de vinculación.",
      ),
    ).toBeTruthy();
    expect(screen.queryByText("View threads")).toBeNull();
  });

  it("relabels a pairing error already on screen when the language changes", async () => {
    const expires = Math.floor((Date.now() + 10 * 60_000) / 1_000);
    // Signed out, so the claim stops at once with an error for the page.
    await act(async () => {
      await pairMachine(
        `h4rn://pair?c=https://relay.test&m=atlas&k=bW9jay1ub2lzZQ&p=view&e=${expires}`,
      );
    });
    // The store keeps the kind, not the words of the language that was active.
    expect(remoteStore.getState().error).toEqual({ kind: "signInToScan" });
    render(<PairingPage />);
    expect(
      screen.getByText("Sign in to h4rn-connect first, then scan the pairing code again."),
    ).toBeTruthy();

    await act(() => setLocalePreference("es"));
    expect(
      screen.getByText(
        "Inicia sesión en h4rn-connect primero y vuelve a escanear el código de vinculación.",
      ),
    ).toBeTruthy();
    await act(() => setLocalePreference("ja"));
    expect(screen.getByText(i18n.t("pairing:errors.signInToScan"))).toBeTruthy();
    expect(screen.queryByText(/Sign in to h4rn-connect first/)).toBeNull();
  });

  it("keeps a relay's own error text as sent in every language", async () => {
    remoteStore.setState({ ...initial, error: { kind: "sent", text: "relay said no" } });
    render(<PairingPage />);
    await act(() => setLocalePreference("zh-CN"));
    expect(screen.getByText("relay said no")).toBeTruthy();
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

  it("says what matching and different codes mean, plainly, in every language", async () => {
    sasState(false);
    render(<PairingPage />);
    for (const [locale, note] of [
      [
        "system",
        "atlas shows this code under Settings → Devices. If it reads the same, no third party can read this connection. If it is different, someone between you and the host has replaced the key.",
      ],
      [
        "ja",
        "atlas の「設定 → デバイス」にこのコードが表示されます。同じであれば、この接続を第三者が読み取ることはできません。異なる場合は、あなたとホストの間で何者かが鍵を差し替えています。",
      ],
      [
        "zh-CN",
        "atlas 会在“设置 → 设备”下显示此代码。如果两者相同，任何第三方都无法读取此连接。如果不同，说明你和主机之间有人替换了密钥。",
      ],
      [
        "es",
        "atlas muestra este código en Ajustes → Dispositivos. Si coincide, ningún tercero puede leer esta conexión. Si es distinto, alguien entre tú y el host ha cambiado la clave.",
      ],
    ] as const) {
      await act(() => setLocalePreference(locale));
      expect(screen.getByText(note), locale).toBeTruthy();
    }
  });

  it("says a failed pairing could not be completed, in every language", async () => {
    for (const [locale, claimFailed] of [
      ["system", "Could not complete that pairing."],
      ["zh-CN", "无法完成该配对。"],
      ["es", "No se pudo completar la vinculación."],
      ["ja", "ペアリングを完了できませんでした。"],
    ] as const) {
      await act(() => setLocalePreference(locale));
      expect(i18n.t("pairing:errors.claimFailed"), locale).toBe(claimFailed);
    }
    await act(() => setLocalePreference("es"));
    expect(i18n.t("settings:devices.toasts.pairingFailed")).toBe(
      "No se pudo crear la vinculación.",
    );
  });

  it("waits for the host in the active language", async () => {
    await act(() => setLocalePreference("ja"));
    sasState(true);
    render(<PairingPage />);
    expect(screen.getByTestId("pair-waiting").textContent).toBe("atlas を待っています");
    expect(screen.getByRole("button", { name: "ホストを待っています…" })).toBeTruthy();
  });

  it("names the host that has to approve the phone in Spanish", async () => {
    await act(() => setLocalePreference("es"));
    sasState(true);
    render(<PairingPage />);
    expect(
      screen.getByText(
        "En atlas, abre Ajustes → Dispositivos y confirma el mismo código. Esta pantalla se actualiza cuando atlas apruebe este teléfono.",
      ),
    ).toBeTruthy();
  });
});
