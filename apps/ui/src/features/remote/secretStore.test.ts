import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { resetSecretStoreForTests } from "@/lib/platform/secrets";

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

function stubWindow(extra: Record<string, unknown> = {}): {
  local: Storage;
  session: Storage;
} {
  const local = memoryStorage();
  const session = memoryStorage();
  vi.stubGlobal("window", { localStorage: local, sessionStorage: session, ...extra });
  return { local, session };
}

beforeEach(() => {
  resetSecretStoreForTests();
});

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("web secret store", () => {
  it("never writes a connect secret to localStorage or sessionStorage", async () => {
    const { local, session } = stubWindow();

    await setSecret("credentials", "refresh-token");
    await expect(getSecret("credentials")).resolves.toBe("refresh-token");

    expect(local.length).toBe(0);
    expect(session.length).toBe(0);
    expect(local.getItem("h4rn.remote.credentials")).toBeNull();
    expect(local.getItem("refresh-token")).toBeNull();

    await deleteSecret("credentials");
    await expect(getSecret("credentials")).resolves.toBeNull();
  });
});

describe("desktop secret store", () => {
  it("routes connect secrets through the OS keychain", async () => {
    const store = new Map<string, string>();
    const invoke = vi.fn(async (command: string, args?: Record<string, unknown>) => {
      if (command === "keychain_get") {
        if (args?.service === "h4rn-connect") return null;
        return store.get(String(args?.account)) ?? null;
      }
      if (command === "keychain_set") {
        store.set(String(args?.account), String(args?.secret));
        return null;
      }
      if (command === "keychain_delete") {
        store.delete(String(args?.account));
        return null;
      }
      return null;
    });
    const { local, session } = stubWindow({
      __TAURI_INTERNALS__: { invoke },
      navigator: { userAgent: "Mozilla/5.0 (X11; Linux x86_64)" },
    });

    await setSecret("credentials", "refresh-token");
    expect(store.get("h4rn.remote.credentials")).toBe("refresh-token");
    expect(invoke).toHaveBeenCalledWith("keychain_set", {
      service: "h4rn",
      account: "h4rn.remote.credentials",
      secret: "refresh-token",
    });

    await expect(getSecret("credentials")).resolves.toBe("refresh-token");

    await deleteSecret("credentials");
    expect(store.has("h4rn.remote.credentials")).toBe(false);
    expect(local.length).toBe(0);
    expect(session.length).toBe(0);
  });
});

describe("mobile secret store", () => {
  it("uses the Keystore/Keychain commands on Android", async () => {
    const invoke = vi.fn(async (command: string) => {
      if (command === "secret_get") return { value: null };
      return null;
    });
    const { local, session } = stubWindow({
      __TAURI_INTERNALS__: { invoke },
      navigator: { userAgent: "Mozilla/5.0 (Linux; Android 15)" },
    });

    await expect(getSecret("credentials")).resolves.toBeNull();
    expect(invoke).toHaveBeenCalledWith("secret_get", { key: "h4rn.remote.credentials" });

    await setSecret("credentials", "refresh-token");
    expect(invoke).toHaveBeenCalledWith("secret_set", {
      key: "h4rn.remote.credentials",
      value: "refresh-token",
    });

    await deleteSecret("credentials");
    expect(invoke).toHaveBeenCalledWith("secret_delete", { key: "h4rn.remote.credentials" });

    expect(local.length).toBe(0);
    expect(session.length).toBe(0);
  });
});
