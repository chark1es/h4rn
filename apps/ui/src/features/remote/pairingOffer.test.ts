import { describe, expect, it } from "vitest";

import type { PairingCreateResponse } from "@/gen";

import { mockOffer, offerFromCreate } from "./pairingOffer";

describe("offerFromCreate", () => {
  const created: PairingCreateResponse = {
    pairing: {
      id: "p1",
      machine_id: "m1",
      state: "pending",
      created_at: "2026-11-13T10:00:00Z",
      expires_at: "2026-11-13T10:05:00Z",
      claimed_by_device_id: null,
      scopes: ["approve", "view"],
    },
    qr: "h4rn://pair?x=1",
    short_code: "7XK2M9QD",
  };

  it("carries the server's code and scopes, view first", () => {
    const offer = offerFromCreate(created, 300_000);
    expect(offer).toEqual({
      id: "p1",
      qr: "h4rn://pair?x=1",
      shortCode: "7XK2M9QD",
      scopes: ["view", "approve"],
      expiresAt: Date.parse("2026-11-13T10:05:00Z"),
    });
  });

  it("falls back to the ttl when the expiry is unreadable", () => {
    const offer = offerFromCreate(
      { ...created, pairing: { ...created.pairing, expires_at: "soon" } },
      60_000,
    );
    expect(offer.expiresAt).toBeGreaterThan(Date.now());
  });
});

describe("mockOffer", () => {
  it("makes an 8-character Crockford code that follows the scopes", () => {
    const a = mockOffer("m", "http://c", ["view", "approve"], 300_000, 1_000_000);
    const b = mockOffer("m", "http://c", ["view", "approve", "files"], 300_000, 1_000_000);
    expect(a.shortCode).toMatch(/^[0-9A-HJKMNP-TV-Z]{8}$/);
    expect(a.shortCode).not.toBe(b.shortCode);
    expect(a.qr).toContain("p=view,approve");
    expect(a.expiresAt).toBe(1_300_000);
    expect(mockOffer("m", "http://c", ["approve"], 1, 0).scopes).toEqual(["view", "approve"]);
  });
});
