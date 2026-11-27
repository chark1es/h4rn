// @vitest-environment jsdom

import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { ProtoError } from "@/gen";

const mock = vi.hoisted(() => ({
  state: {
    connection: { status: "offline", attempt: 0, error: null as ProtoError | null },
    lastSyncAt: null as number | null,
  },
  reconnectNow: vi.fn(),
}));

vi.mock("@/lib/store/appStore", () => ({
  useApp: (selector: (state: unknown) => unknown) => selector(mock.state),
  getClient: () => ({ reconnectNow: mock.reconnectNow }),
}));

import { clientProto, protoError } from "@/lib/client/errors";
import { setLocalePreference } from "@/lib/i18n";
import { OfflineBanner } from "./OfflineBanner";

beforeEach(() => {
  mock.state.connection = { status: "offline", attempt: 0, error: null };
  mock.state.lastSyncAt = null;
});

afterEach(async () => {
  cleanup();
  await act(() => setLocalePreference("system"));
});

describe("connection banner", () => {
  it("reads the same in English as before", () => {
    mock.state.connection = {
      status: "degraded",
      attempt: 3,
      error: clientProto("internal", "offline"),
    };
    render(<OfflineBanner />);
    const banner = screen.getByRole("status");
    expect(banner.textContent).toBe("Reconnecting · attempt 3Reconnect");
    expect(banner.getAttribute("title")).toBe(
      "The daemon connection is offline. Start the daemon or check the network, then press Reconnect.",
    );
    fireEvent.click(screen.getByRole("button", { name: "Reconnect" }));
    expect(mock.reconnectNow).toHaveBeenCalled();
  });

  it("says offline with no cache, and relabels the client's error on a language switch", async () => {
    mock.state.connection = {
      status: "offline",
      attempt: 0,
      error: clientProto("internal", "notConnected"),
    };
    render(<OfflineBanner />);
    expect(screen.getByRole("status").textContent).toBe("Offline · no cached threadsReconnect");

    await act(() => setLocalePreference("es"));
    const banner = screen.getByRole("status");
    expect(banner.textContent).toBe("Sin conexión · no hay hilos en cachéReconectar");
    // The error was raised in English; the banner words its kind again.
    expect(banner.getAttribute("title")).toBe(
      "El daemon no está conectado. Espera a que la barra de estado indique que está en línea y vuelve a intentarlo.",
    );

    await act(() => setLocalePreference("zh-CN"));
    expect(screen.getByRole("status").getAttribute("title")).toBe(
      "守护进程未连接。等状态栏显示在线后再试。",
    );
  });

  it("shows a daemon's own error as sent in every language", async () => {
    mock.state.connection = {
      status: "offline",
      attempt: 0,
      error: protoError("unauthorized", "token revoked", "pair again"),
    };
    render(<OfflineBanner />);
    await act(() => setLocalePreference("ja"));
    expect(screen.getByRole("status").getAttribute("title")).toBe("token revoked pair again");
    expect(screen.getByRole("button", { name: "再接続" })).toBeTruthy();
  });

  it("formats the attempt count for the language", async () => {
    mock.state.connection = { status: "degraded", attempt: 1200, error: null };
    render(<OfflineBanner />);
    await act(() => setLocalePreference("es"));
    expect(screen.getByRole("status").textContent).toBe("Reconectando · intento 1200Reconectar");
    await act(() => setLocalePreference("ja"));
    expect(screen.getByRole("status").textContent).toBe("再接続中 · 1,200 回目再接続");
  });
});
