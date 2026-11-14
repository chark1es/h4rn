import { describe, expect, it } from "vitest";

import { base64UrlToHex, normalizeTokens } from "./connect";

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
