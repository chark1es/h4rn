import { afterEach, describe, expect, it, vi } from "vitest";

import { base64UrlToHex, listProviders, normalizeTokens, requestTimeoutSignal } from "./connect";

afterEach(() => {
  vi.unstubAllGlobals();
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
