import { beforeEach, describe, expect, it, vi } from "vitest";

import type { ProxyModel, ProxyModelCatalog, ProxyProfile, ProxyProfileId } from "@/gen";

const { request } = vi.hoisted(() => ({ request: vi.fn() }));

vi.mock("@/lib/store/appStore", () => ({
  getClient: () => ({ request }),
  useApp: () => undefined,
}));

import {
  getProxyState,
  proxyDefaultModel,
  proxyModelsFor,
  proxyOverrideFor,
  resetProxyStoreForTests,
  setHarnessOverride,
} from "./proxyStore";

function model(id: string): ProxyModel {
  return {
    id,
    display_name: null,
    shape: "open_ai",
    source: "open_ai",
    alias_of: null,
    owned_by: null,
    context_length: null,
    max_output_tokens: null,
    fork_enriched: false,
  };
}

function profile(overrides: Partial<ProxyProfile> = {}): ProxyProfile {
  return {
    id: "proxy_a" as ProxyProfileId,
    name: "Home proxy",
    base_url: "https://proxy.example",
    default: true,
    has_management_key: true,
    has_api_key: true,
    harnesses: [],
    capabilities: null,
    ...overrides,
  };
}

function catalog(profileId: ProxyProfileId, models: ProxyModel[]): ProxyModelCatalog {
  return {
    profile_id: profileId,
    models,
    aliases_merged: true,
    fetched_at: "2026-11-13T00:00:00.000Z",
  };
}

describe("proxy harness overrides", () => {
  beforeEach(() => resetProxyStoreForTests());

  it("finds the first profile with an enabled toggle for the harness", () => {
    resetProxyStoreForTests({
      profiles: [
        profile({
          id: "proxy_a",
          harnesses: [
            { harness: "codex", enabled: false, default_model: null, verified: false, note: null },
          ],
        }),
        profile({
          id: "proxy_b",
          harnesses: [
            { harness: "codex", enabled: true, default_model: "gpt-5", verified: true, note: null },
          ],
        }),
      ],
    });
    const active = proxyOverrideFor("codex");
    expect(active?.profile.id).toBe("proxy_b");
    expect(active?.override.default_model).toBe("gpt-5");
    expect(proxyOverrideFor("claude_code")).toBeNull();
  });

  it("lists only the active profile's catalogue models", () => {
    resetProxyStoreForTests({
      profiles: [
        profile({
          id: "proxy_a",
          harnesses: [
            {
              harness: "claude_code",
              enabled: true,
              default_model: null,
              verified: true,
              note: null,
            },
          ],
        }),
      ],
      catalogs: {
        proxy_a: catalog("proxy_a", [model("claude-opus"), model("claude-sonnet")]),
      },
    });
    expect(proxyModelsFor("claude_code").map((entry) => entry.id)).toEqual([
      "claude-opus",
      "claude-sonnet",
    ]);
    expect(proxyModelsFor("codex")).toEqual([]);
  });

  it("prefers the explicit default, then the first catalogue model", () => {
    resetProxyStoreForTests({
      profiles: [
        profile({
          id: "proxy_a",
          harnesses: [
            {
              harness: "claude_code",
              enabled: true,
              default_model: null,
              verified: true,
              note: null,
            },
            {
              harness: "opencode",
              enabled: true,
              default_model: "chosen",
              verified: true,
              note: null,
            },
          ],
        }),
      ],
      catalogs: { proxy_a: catalog("proxy_a", [model("first"), model("second")]) },
    });
    expect(proxyDefaultModel("claude_code")).toBe("first");
    expect(proxyDefaultModel("opencode")).toBe("chosen");
    expect(proxyDefaultModel("codex")).toBeNull();
  });

  it("has no models before the catalogue arrives", () => {
    resetProxyStoreForTests({
      profiles: [
        profile({
          id: "proxy_a",
          harnesses: [
            { harness: "codex", enabled: true, default_model: "gpt-5", verified: true, note: null },
          ],
        }),
      ],
    });
    expect(proxyModelsFor("codex")).toEqual([]);
    expect(proxyDefaultModel("codex")).toBe("gpt-5");
  });
});

describe("proxy harness override toggles", () => {
  beforeEach(() => {
    request.mockReset();
    resetProxyStoreForTests({
      profiles: [
        profile({
          id: "proxy_a",
          harnesses: [
            {
              harness: "claude_code",
              enabled: false,
              default_model: null,
              verified: true,
              note: null,
            },
          ],
        }),
      ],
    });
  });

  it("flips immediately, then adopts the daemon's projection", async () => {
    const saved = profile({
      id: "proxy_a",
      harnesses: [
        {
          harness: "claude_code",
          enabled: true,
          default_model: "claude-sonnet-5-5",
          verified: true,
          note: "Routed",
        },
      ],
    });
    request.mockResolvedValue({ type: "proxy_profile", profile: saved });

    const call = setHarnessOverride("proxy_a", "claude_code", true, "claude-sonnet-5-5");
    // Optimistic: the switch is already on before the round trip finishes.
    expect(getProxyState().profiles[0]?.harnesses[0]?.enabled).toBe(true);
    await call;

    expect(request).toHaveBeenCalledWith({
      type: "set_proxy_harness_override",
      profile_id: "proxy_a",
      harness: "claude_code",
      enabled: true,
      default_model: "claude-sonnet-5-5",
    });
    expect(getProxyState().profiles[0]?.harnesses[0]?.note).toBe("Routed");
    expect(getProxyState().overridesSaving["proxy_a:claude_code"]).toBe(false);
    expect(getProxyState().saveError).toBeNull();
  });

  it("rolls the toggle back when the daemon rejects the write", async () => {
    request.mockRejectedValue(new Error("no credential store"));

    const call = setHarnessOverride("proxy_a", "claude_code", true, "claude-sonnet-5-5");
    expect(getProxyState().profiles[0]?.harnesses[0]?.enabled).toBe(true);
    await call;

    expect(getProxyState().profiles[0]?.harnesses[0]?.enabled).toBe(false);
    expect(getProxyState().saveError?.message).toContain("no credential store");
    expect(getProxyState().overridesSaving["proxy_a:claude_code"]).toBe(false);
  });
});
