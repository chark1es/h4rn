import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { peekSecret, prepareSecrets, resetSecretStoreForTests, setSecret } from "./secrets";

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

describe("platform secret store", () => {
  it("never puts a secret in localStorage or sessionStorage", async () => {
    const { local, session } = stubWindow();
    await setSecret("h4rn.token", "super-secret-token");
    await expect(prepareSecrets()).resolves.toBeUndefined();
    expect(local.length).toBe(0);
    expect(session.length).toBe(0);
    expect(peekSecret("h4rn.token")).toBe("super-secret-token");
  });

  it("migrates legacy browser storage and deletes the copy", async () => {
    const local = memoryStorage();
    local.setItem("h4rn.token", "legacy-token");
    local.setItem("h4rn.secure.identity", "legacy-identity");
    local.setItem("h4rn.theme", "dark");
    const session = memoryStorage();
    session.setItem("h4rn.remote.credentials", "legacy-credentials");
    vi.stubGlobal("window", { localStorage: local, sessionStorage: session });

    await prepareSecrets();

    expect(peekSecret("h4rn.token")).toBe("legacy-token");
    expect(peekSecret("h4rn.secure.identity")).toBe("legacy-identity");
    expect(peekSecret("h4rn.remote.credentials")).toBe("legacy-credentials");
    // Secret copies are gone; non-secret UI state is left alone.
    expect(local.getItem("h4rn.token")).toBeNull();
    expect(local.getItem("h4rn.secure.identity")).toBeNull();
    expect(local.getItem("h4rn.theme")).toBe("dark");
    expect(session.getItem("h4rn.remote.credentials")).toBeNull();
  });

  it("ingests boot query secrets and strips them from the address bar", async () => {
    const replaceState = vi.fn();
    const local = memoryStorage();
    const session = memoryStorage();
    vi.stubGlobal("window", {
      localStorage: local,
      sessionStorage: session,
      history: { replaceState },
      location: {
        href: "http://127.0.0.1:1420/t/abc?token=boot-token&pair=h4rn%3A%2F%2Fpair%3Fc%3D1&theme=dark",
      },
    });

    await prepareSecrets();

    expect(peekSecret("h4rn.token")).toBe("boot-token");
    expect(peekSecret("h4rn.secure.pairing-uri")).toBe("h4rn://pair?c=1");
    expect(replaceState).toHaveBeenCalledOnce();
    const [, , url] = replaceState.mock.calls[0] as [unknown, unknown, string];
    expect(url).not.toContain("token=");
    expect(url).not.toContain("pair=");
    expect(url).toContain("theme=dark");
    expect(local.length).toBe(0);
    expect(session.length).toBe(0);
  });
});
