import { afterEach, describe, expect, it, vi } from "vitest";

const signed = vi.hoisted(() => [] as { method: string; path: string }[]);

vi.mock("@/gen/transport-wasm/h4rn_transport_wasm", () => ({
  connect_proof: (
    _identity: string,
    method: string,
    path: string,
    timestamp: bigint,
    nonce: string,
  ) => {
    signed.push({ method, path });
    return JSON.stringify({ proof: "proof", timestamp: Number(timestamp), nonce });
  },
  identity_public: () => JSON.stringify({ proofPublicKey: "proof-key", noisePublic: "noise-key" }),
  pairing_sas: () => "0000 0000 0000",
}));

vi.mock("@/lib/client/secure", () => ({
  ensureDeviceIdentity: async () => "device-identity",
}));

vi.mock("@/lib/client/wasm", () => ({
  loadTransportWasm: async () => undefined,
}));

import {
  base64UrlToHex,
  ConnectClient,
  listProviders,
  normalizeTokens,
  requestTimeoutSignal,
} from "./connect";

afterEach(() => {
  vi.unstubAllGlobals();
  signed.length = 0;
});

describe("normalizeTokens", () => {
  it("narrows the wire i64 to a number", () => {
    expect(
      normalizeTokens({
        access_token: "a",
        refresh_token: "r",
        expires_in: 3_600n,
        token_type: "Bearer",
      }),
    ).toEqual({
      access_token: "a",
      refresh_token: "r",
      expires_in: 3_600,
      token_type: "Bearer",
    });
  });
});

describe("base64UrlToHex", () => {
  it("decodes unpadded base64url to lowercase hex", () => {
    // 32 bytes of 0xab, the daemon Noise key shape.
    const key = "q6urq6urq6urq6urq6urq6urq6urq6urq6urq6urq6s";
    expect(base64UrlToHex(key)).toBe("ab".repeat(32));
  });

  it("accepts url-safe characters and padding", () => {
    expect(base64UrlToHex("-_8")).toBe("fbff");
    expect(base64UrlToHex("-_8=")).toBe("fbff");
  });
});

describe("connect request deadlines", () => {
  it("aborts a request signal after its deadline", async () => {
    const signal = requestTimeoutSignal(5);
    expect(signal.aborted).toBe(false);
    await new Promise<void>((resolve) => {
      signal.addEventListener("abort", () => resolve(), { once: true });
    });
    expect(signal.aborted).toBe(true);
  });

  it("passes a deadline signal on every public request", async () => {
    const fetchMock = vi.fn(
      async (_input: RequestInfo | URL, init?: RequestInit): Promise<Response> => {
        expect(init?.signal).toBeInstanceOf(AbortSignal);
        return new Response("[]", {
          status: 200,
          headers: { "content-type": "application/json" },
        });
      },
    );
    vi.stubGlobal("fetch", fetchMock);

    await expect(listProviders("http://relay.test")).resolves.toEqual([]);
    expect(fetchMock).toHaveBeenCalledTimes(1);
  });
});

describe("signed request path", () => {
  function credentials() {
    return {
      connectUrl: "https://relay.test",
      user: { id: "user-1", email: null, display_name: "Dev", avatar_url: null },
      device: {
        id: "device-1",
        kind: "desktop" as const,
        name: "laptop",
        machine_id: null,
        proof_public_key: "proof-key",
        noise_static_public_key: null,
        platform: "linux",
        created_at: new Date(0).toISOString(),
        last_seen: null,
        revoked_at: null,
        scopes: [],
      },
      tokens: {
        access_token: "access",
        refresh_token: "refresh",
        expires_in: 3_600,
        token_type: "Bearer",
      },
      expires_at: Date.now() + 3_600_000,
    };
  }

  it("signs the pathname while the fetch keeps the query string", async () => {
    const fetchMock = vi.fn(
      async (_input: RequestInfo | URL): Promise<Response> =>
        new Response("[]", { status: 200, headers: { "content-type": "application/json" } }),
    );
    vi.stubGlobal("fetch", fetchMock);

    // The real client path is what the relay will verify: a proof over
    // `/v1/machines?linked=true` is rejected because the server only sees
    // `parts.uri.path()`.
    const client = new ConnectClient(credentials());
    await client.machines({ linked: true });

    expect(signed).toEqual([{ method: "GET", path: "/v1/machines" }]);
    expect(String(fetchMock.mock.calls[0]?.[0])).toBe("https://relay.test/v1/machines?linked=true");
  });

  it("still signs query-free endpoints unchanged", async () => {
    const fetchMock = vi.fn(
      async (_input: RequestInfo | URL): Promise<Response> =>
        new Response("[]", { status: 200, headers: { "content-type": "application/json" } }),
    );
    vi.stubGlobal("fetch", fetchMock);

    const client = new ConnectClient(credentials());
    await client.machines();

    expect(signed).toEqual([{ method: "GET", path: "/v1/machines" }]);
  });

  it("accepts a linked list only when the relay says it filtered", async () => {
    const fetchMock = vi.fn(
      async (_input: RequestInfo | URL): Promise<Response> =>
        new Response(JSON.stringify([{ id: "machine-1" }]), {
          status: 200,
          headers: {
            "content-type": "application/json",
            "x-h4rn-machine-filter": "linked",
          },
        }),
    );
    vi.stubGlobal("fetch", fetchMock);

    const client = new ConnectClient(credentials());
    const answer = await client.linkedMachines();

    expect(answer).toEqual({ supported: true, machines: [{ id: "machine-1" }] });
    expect(String(fetchMock.mock.calls[0]?.[0])).toBe("https://relay.test/v1/machines?linked=true");
  });

  it("treats a missing filter marker as an older relay", async () => {
    const fetchMock = vi.fn(
      async (_input: RequestInfo | URL): Promise<Response> =>
        new Response(JSON.stringify([{ id: "machine-1" }]), {
          status: 200,
          headers: { "content-type": "application/json" },
        }),
    );
    vi.stubGlobal("fetch", fetchMock);

    const client = new ConnectClient(credentials());
    // The older relay ignored the query and returned the whole account; the
    // caller must not mistake that for this device's links.
    await expect(client.linkedMachines()).resolves.toEqual({ supported: false, machines: [] });
  });
});
