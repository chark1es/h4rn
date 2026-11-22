import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { describe, expect, it, vi } from "vitest";

import { createSecureTransportFactory, kindForEndpoint, normalizeSecureUrl } from "./secure";

const secrets = vi.hoisted(() => {
  const store = new Map<string, string>();
  return {
    store,
    getSecret: async (key: string) => store.get(key) ?? null,
    peekSecret: (key: string) => store.get(key) ?? null,
    setSecret: async (key: string, value: string) => {
      store.set(key, value);
    },
    forgetSecret: (key: string) => {
      store.delete(key);
    },
  };
});

vi.mock("@/lib/platform/secrets", () => ({
  getSecret: secrets.getSecret,
  peekSecret: secrets.peekSecret,
  setSecret: secrets.setSecret,
  forgetSecret: secrets.forgetSecret,
}));

vi.mock("@/lib/client/wasm", () => ({ loadTransportWasm: async () => undefined }));

vi.mock("@/gen/transport-wasm/h4rn_transport_wasm", () => ({
  connect: () => ({
    status_json: () => "{}",
    send: () => undefined,
    tick: () => undefined,
    path_open: () => undefined,
    path_closed: () => undefined,
    receive: () => undefined,
    on_packet: () => undefined,
    on_frame: () => undefined,
    on_status: () => undefined,
    on_ticket: () => undefined,
    on_closed: () => undefined,
    close: () => undefined,
    free: () => undefined,
  }),
  generate_identity: () => "identity",
  parse_pairing: () => "{}",
}));

const here = dirname(fileURLToPath(import.meta.url));
/** apps/ui/src/lib/client -> repository root. */
const repo = resolve(here, "../../../../..");
const read = (relative: string): string => readFileSync(resolve(repo, relative), "utf8");

describe("candidate URLs", () => {
  it("turns a bare LAN or mDNS host into a cleartext secure endpoint", () => {
    expect(normalizeSecureUrl("192.168.1.20:47800")).toBe("ws://192.168.1.20:47800/secure");
    expect(normalizeSecureUrl("daemon.local:47800")).toBe("ws://daemon.local:47800/secure");
    expect(normalizeSecureUrl("100.101.102.103:47800")).toBe("ws://100.101.102.103:47800/secure");
  });

  it("keeps an explicit ws(s) candidate and maps http(s) onto it", () => {
    expect(normalizeSecureUrl("ws://192.168.1.20:47800/secure")).toBe(
      "ws://192.168.1.20:47800/secure",
    );
    expect(normalizeSecureUrl("http://daemon.local:47800")).toBe("ws://daemon.local:47800/secure");
    expect(normalizeSecureUrl("https://relay.example.com/relay")).toBe(
      "wss://relay.example.com/relay",
    );
  });

  it("ranks CGNAT and MagicDNS candidates as tailscale", () => {
    expect(kindForEndpoint("ws://100.64.0.1:47800/secure")).toBe("tailscale");
    expect(kindForEndpoint("ws://100.127.255.254:47800/secure")).toBe("tailscale");
    expect(kindForEndpoint("wss://host.tail1234.ts.net/secure")).toBe("tailscale");
    expect(kindForEndpoint("ws://192.168.1.20:47800/secure")).toBe("direct");
    expect(kindForEndpoint("ws://daemon.local:47800/secure")).toBe("direct");
  });
});

describe("mobile cleartext policy", () => {
  it("lets a release Android build reach private, CGNAT and mDNS candidates", () => {
    const xml = read("apps/mobile/native/android/app/src/main/res/xml/network_security_config.xml");
    expect(xml).toMatch(/<base-config cleartextTrafficPermitted="true"/);

    const gradle = read("apps/mobile/native/android/app/build.gradle.kts");
    expect(gradle).toMatch(/manifestPlaceholders\["usesCleartextTraffic"\] = "true"/);
  });

  it("keeps iOS ATS to DNS-name exceptions instead of inert CIDR keys", () => {
    const plist = read("apps/mobile/src-tauri/Info.plist");
    expect(plist).toContain("<key>NSAllowsLocalNetworking</key>");
    for (const host of ["local", "ts.net"]) {
      expect(plist).toContain(`<key>${host}</key>`);
    }
    // ATS matches DNS names, not address ranges, so a CIDR key is inert and
    // must not creep back in as if it excepted private/CGNAT IP literals.
    for (const cidr of ["10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16", "100.64.0.0/10"]) {
      expect(plist).not.toContain(`<key>${cidr}</key>`);
    }

    const tauri = JSON.parse(read("apps/mobile/src-tauri/tauri.conf.json")) as {
      bundle: { iOS: { infoPlist?: string } };
    };
    expect(tauri.bundle.iOS.infoPlist).toBe("Info.plist");
  });
});

/** A socket that records the endpoint without ever opening. */
class FakeSocket {
  static readonly CONNECTING = 0;
  static readonly OPEN = 1;
  static readonly CLOSING = 2;
  static readonly CLOSED = 3;
  static readonly opened: string[] = [];

  readyState = FakeSocket.CONNECTING;
  binaryType = "blob";
  onopen: (() => void) | null = null;
  onmessage: ((message: { data: unknown }) => void) | null = null;
  onerror: (() => void) | null = null;
  onclose: ((event: { reason: string }) => void) | null = null;

  constructor(readonly url: string) {
    FakeSocket.opened.push(url);
  }

  send(): void {}
  close(): void {
    this.readyState = FakeSocket.CLOSED;
  }
}

async function flush(): Promise<void> {
  for (let index = 0; index < 50; index += 1) await Promise.resolve();
}

async function waitFor(condition: () => boolean): Promise<void> {
  for (let index = 0; index < 200; index += 1) {
    if (condition()) return;
    await flush();
  }
  throw new Error("condition was never met");
}

describe("secure transport cancellation", () => {
  it("does not open a socket from a config attempt a wake abandoned", async () => {
    FakeSocket.opened.length = 0;
    const previous = globalThis.WebSocket;
    Object.defineProperty(globalThis, "WebSocket", {
      value: FakeSocket,
      configurable: true,
      writable: true,
    });
    try {
      const mints: Array<(url: string | null) => void> = [];
      const factory = createSecureTransportFactory({
        remote: {
          daemonPublic: "aa".repeat(32),
          paths: [{ kind: "relay", endpoint: "wss://relay.invalid/relay" }],
          machineId: "machine",
          sessionKey: "test.session",
        },
        mintRelayUrl: () =>
          new Promise<string | null>((resolve) => {
            mints.push(resolve);
          }),
      });
      const context = { url: "ws://daemon.test:47800/secure", token: null, attempt: 0 };
      const handlers = {
        onOpen: () => undefined,
        onMessage: () => undefined,
        onClose: () => undefined,
        onError: () => undefined,
      };

      const abandoned = factory(context);
      abandoned.connect(handlers);
      await waitFor(() => mints.length === 1);

      // A browser wake replaces the transport while its config is still
      // being resolved.
      abandoned.close();
      const replacement = factory(context);
      replacement.connect(handlers);
      await waitFor(() => mints.length === 2);

      mints[0]?.("wss://relay.invalid/first");
      mints[1]?.("wss://relay.invalid/second");
      await flush();

      expect(FakeSocket.opened).toEqual(["wss://relay.invalid/second"]);
      replacement.close();
    } finally {
      Object.defineProperty(globalThis, "WebSocket", {
        value: previous,
        configurable: true,
        writable: true,
      });
    }
  });
});
