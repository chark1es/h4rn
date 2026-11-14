import { afterEach, describe, expect, it, vi } from "vitest";

import { deleteSecret, getSecret, setSecret } from "./secretStore";

function memoryStorage(): Storage {
  const map = new Map<string, string>();
  return {
    get length() {
      return map.size;
    },
    clear: () => map.clear(),
    getItem: (key) => map.get(key) ?? null,
    key: (index) => [...map.keys()][index] ?? null,
    removeItem: (key) => {
      map.delete(key);
    },
    setItem: (key, value) => {
      map.set(key, value);
    },
  };
}

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("web secret store", () => {
  it("uses sessionStorage, never localStorage", async () => {
    const session = memoryStorage();
    const local = memoryStorage();
    vi.stubGlobal("window", { sessionStorage: session, localStorage: local });

    await setSecret("token", "abc");
    expect(session.getItem("h4rn.remote.token")).toBe("abc");
    expect(local.getItem("h4rn.remote.token")).toBeNull();
    await expect(getSecret("token")).resolves.toBe("abc");

    await deleteSecret("token");
    await expect(getSecret("token")).resolves.toBeNull();
  });

  it("falls back to memory when the webview rejects storage", async () => {
    vi.stubGlobal("window", {
      get sessionStorage(): Storage {
        throw new Error("private mode");
      },
    });
    await setSecret("locked", "value");
    await expect(getSecret("locked")).resolves.toBe("value");
    await deleteSecret("locked");
    await expect(getSecret("locked")).resolves.toBeNull();
  });
});

describe("desktop secret store", () => {
  it("routes to the OS keychain through Tauri", async () => {
    const invoke = vi.fn(async (command: string) => {
      if (command === "keychain_get") return "from-keychain";
      return null;
    });
    vi.stubGlobal("window", {
      __TAURI_INTERNALS__: { invoke },
      navigator: { userAgent: "Mozilla/5.0 (X11; Linux x86_64)" },
    });

    await expect(getSecret("token")).resolves.toBe("from-keychain");
    expect(invoke).toHaveBeenCalledWith("keychain_get", {
      service: "h4rn-connect",
      account: "token",
    });

    await setSecret("token", "value");
    expect(invoke).toHaveBeenCalledWith("keychain_set", {
      service: "h4rn-connect",
      account: "token",
      secret: "value",
    });

    await deleteSecret("token");
    expect(invoke).toHaveBeenCalledWith("keychain_delete", {
      service: "h4rn-connect",
      account: "token",
    });
  });

  it("reports a rejected keychain write instead of pretending it saved", async () => {
    vi.stubGlobal("window", {
      __TAURI_INTERNALS__: {
        invoke: vi.fn(async () => {
          throw new Error("keychain locked");
        }),
      },
      navigator: { userAgent: "Mozilla/5.0 (X11; Linux x86_64)" },
    });
    await expect(setSecret("token", "value")).rejects.toThrow("keychain locked");
    await expect(getSecret("token")).resolves.toBeNull();
  });

  it("uses the mobile secret commands on Android", async () => {
    const invoke = vi.fn(async () => ({ value: null }));
    vi.stubGlobal("window", {
      __TAURI_INTERNALS__: { invoke },
      navigator: { userAgent: "Mozilla/5.0 (Linux; Android 15)" },
    });
    await getSecret("token");
    expect(invoke).toHaveBeenCalledWith("secret_get", { key: "h4rn.remote.token" });
    await setSecret("token", "value");
    expect(invoke).toHaveBeenCalledWith("secret_set", {
      key: "h4rn.remote.token",
      value: "value",
    });
  });
});
