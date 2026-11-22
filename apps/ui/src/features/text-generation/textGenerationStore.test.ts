import { beforeEach, describe, expect, it, vi } from "vitest";

import type { TextGeneratorDraft } from "@/gen";

const { request } = vi.hoisted(() => ({ request: vi.fn() }));

vi.mock("@/lib/store/appStore", () => ({
  getClient: () => ({ request }),
  useApp: () => undefined,
}));

import {
  baseUrlError,
  getTextGenerationState,
  installedTitleAgents,
  resetTextGenerationForTests,
  testTextGenerator,
  titleModelSummary,
  validateGeneratorDraft,
} from "./textGenerationStore";

function draft(overrides: Partial<TextGeneratorDraft> = {}): TextGeneratorDraft {
  return {
    name: "Fast local",
    kind: "open_ai_compatible",
    base_url: "https://api.openai.com/v1",
    model: "gpt-5.4-mini",
    api_key: null,
    ...overrides,
  };
}

describe("text generator form validation", () => {
  it("accepts a complete draft and an https base URL", () => {
    expect(validateGeneratorDraft(draft())).toBeNull();
    expect(baseUrlError("https://api.openai.com/v1")).toBeNull();
  });

  it("names each missing field instead of sending it", () => {
    const errors = validateGeneratorDraft(draft({ name: "  ", base_url: "", model: "" }));
    expect(errors?.name).toBeDefined();
    expect(errors?.model).toBeDefined();
    expect(errors?.base_url).toBeDefined();
  });

  it("allows plain http only on this machine", () => {
    expect(baseUrlError("http://127.0.0.1:11434/v1")).toBeNull();
    expect(baseUrlError("http://localhost:1234/v1")).toBeNull();
    expect(baseUrlError("http://[::1]:1234/v1")).toBeNull();
    expect(baseUrlError("http://ollama.lan:11434/v1")).toContain("localhost");
    expect(baseUrlError("ftp://example.com")).toContain("https");
    expect(baseUrlError("not a url")).toContain("not a URL");
    // Built at runtime: never commit a user:pass URL literal.
    const withCredentials = new URL("https://example.com/v1");
    withCredentials.username = "someone";
    withCredentials.password = "not-a-real-password";
    expect(baseUrlError(withCredentials.toString())).toContain("key field");
  });
});

describe("the title-model picker", () => {
  it("offers only installed coding agents with a one-shot mode", () => {
    expect(
      installedTitleAgents([
        { harness: "claude_code", installed: "2.0.0" },
        { harness: "codex", installed: null },
        { harness: "opencode", installed: "0.8.0" },
        { harness: "codex", installed: "0.51.0" },
      ]),
    ).toEqual([
      { harness: "claude_code", installed: "2.0.0" },
      { harness: "codex", installed: "0.51.0" },
    ]);
  });

  it("describes each mode in one line", () => {
    expect(
      titleModelSummary({
        title: { mode: "off", harness: null, generator_id: null, model: null },
      }),
    ).toContain("first line");
    expect(
      titleModelSummary({
        title: { mode: "coding_agent", harness: "codex", generator_id: null, model: null },
      }),
    ).toContain("Codex");
    expect(
      titleModelSummary({
        title: {
          mode: "text_generator",
          harness: null,
          generator_id: "tg_1" as never,
          model: "tiny-model",
        },
      }),
    ).toContain("tiny-model");
  });
});

describe("testing a generator", () => {
  beforeEach(() => {
    request.mockReset();
    resetTextGenerationForTests();
  });

  it("tests the model on screen, not just the stored default", async () => {
    request.mockResolvedValue({
      type: "text_generator_tested",
      report: {
        ok: true,
        latency_ms: 42,
        reply: "ok",
        error: null,
        hint: null,
      },
    });
    await testTextGenerator("tg_1" as never, "tiny-override");
    expect(request).toHaveBeenCalledWith(
      {
        type: "test_text_generator",
        generator_id: "tg_1",
        model: "tiny-override",
      },
      // Longer than the provider's own 30 s budget, so the honest error wins.
      { timeoutMs: 40_000 },
    );
    expect(getTextGenerationState().reports.tg_1).toMatchObject({
      ok: true,
      reply: "ok",
    });
    expect(getTextGenerationState().testing.tg_1).toBe(false);
  });

  it("keeps the provider's own error text", async () => {
    request.mockRejectedValue(new Error("the provider answered 400 Bad Request"));
    await testTextGenerator("tg_1" as never);
    expect(getTextGenerationState().testErrors.tg_1?.message).toContain("400");
  });
});
