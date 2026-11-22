import { describe, expect, it } from "vitest";

import { HARNESS_MODELS } from "@/lib/models";
import { delegateModel } from "./DelegateDialog";

describe("delegateModel", () => {
  it("keeps the parent's model when the child keeps the parent's harness", () => {
    expect(delegateModel({ harness: "opencode", model: "gpt-5.1-codex" }, "opencode")).toBe(
      "gpt-5.1-codex",
    );
  });

  it("starts on the fallback harness's default when the parent runs an ACP agent", () => {
    const model = delegateModel({ harness: "acp", model: "deepseek-chat" }, "claude_code");
    expect(model).toBe(HARNESS_MODELS.claude_code.default);
    expect(model).not.toBe("deepseek-chat");
    expect(model).not.toBe("default");
  });

  it("never carries a model across harnesses", () => {
    for (const harness of ["claude_code", "codex", "opencode", "antigravity"] as const) {
      expect(delegateModel({ harness: "acp", model: "default" }, harness)).toBe(
        HARNESS_MODELS[harness].default,
      );
      expect(delegateModel({ harness: "codex", model: "gpt-5.1-codex" }, harness)).toBe(
        HARNESS_MODELS[harness].default,
      );
    }
  });
});
