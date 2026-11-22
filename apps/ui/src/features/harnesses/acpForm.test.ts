import { describe, expect, it } from "vitest";

import {
  ACP_ID_PATTERN,
  acpAgentSpecFromForm,
  acpPickerOptions,
  EMPTY_ACP_FORM,
  formFromAcpAgent,
  isPlatformAbsolutePath,
  validateAcpAgentForm,
} from "./acpForm";

describe("ACP agent form", () => {
  it("accepts a complete configuration", () => {
    const form = {
      ...EMPTY_ACP_FORM,
      id: "deepseek",
      name: "DeepSeek Harness",
      command: "dsh",
      args: ["--profile", "acp"],
      env: [{ key: "DSH_HOME", value: "/home/me/.dsh" }],
    };
    expect(validateAcpAgentForm(form)).toBeNull();
    expect(acpAgentSpecFromForm(form)).toEqual({
      id: "deepseek",
      name: "DeepSeek Harness",
      command: "dsh",
      args: ["--profile", "acp"],
      env: { DSH_HOME: "/home/me/.dsh" },
      working_dir: { kind: "thread_workspace" },
    });
  });

  it("rejects ids that are not a lowercase slug or are too long", () => {
    for (const id of ["Demo", "-demo", "demo-", "demo--agent", "demo agent", "", "a".repeat(65)]) {
      const form = { ...EMPTY_ACP_FORM, id, name: "Demo", command: "demo" };
      expect(validateAcpAgentForm(form), id).not.toBeNull();
    }
    expect(ACP_ID_PATTERN.test("demo-agent-2")).toBe(true);
  });

  it("refuses a duplicate id on add and a changed id on edit", () => {
    const form = { ...EMPTY_ACP_FORM, id: "demo", name: "Demo", command: "demo" };
    expect(validateAcpAgentForm(form, { existingIds: ["demo"] })).toContain("already in use");
    expect(validateAcpAgentForm(form, { existingIds: ["demo"], editingId: "other" })).toContain(
      "cannot be changed",
    );
  });

  it("checks the required fields and one-line strings", () => {
    expect(validateAcpAgentForm({ ...EMPTY_ACP_FORM, command: "demo" })).toContain("instance id");
    expect(validateAcpAgentForm({ ...EMPTY_ACP_FORM, id: "demo" })).toContain("display name");
    expect(validateAcpAgentForm({ ...EMPTY_ACP_FORM, id: "demo", name: "Demo" })).toContain(
      "command",
    );
    expect(
      validateAcpAgentForm({ ...EMPTY_ACP_FORM, id: "demo", name: "De\nmo", command: "demo" }),
    ).toContain("one line");
    expect(
      validateAcpAgentForm({ ...EMPTY_ACP_FORM, id: "demo", name: "Demo", command: "bin/agent" }),
    ).toContain("absolute path");
  });

  it("validates environment names and fixed working directories", () => {
    const base = { ...EMPTY_ACP_FORM, id: "demo", name: "Demo", command: "demo" };
    expect(validateAcpAgentForm({ ...base, env: [{ key: "1BAD", value: "x" }] })).toContain(
      "Environment row 1",
    );
    expect(
      validateAcpAgentForm({ ...base, workingDir: "fixed_path", path: "relative/dir" }),
    ).toContain("absolute path");
    expect(
      validateAcpAgentForm({ ...base, workingDir: "fixed_path", path: "/tmp/work" }),
    ).toBeNull();
  });

  it("keeps explicitly added empty arguments", () => {
    const form = {
      ...EMPTY_ACP_FORM,
      id: "demo",
      name: "Demo",
      command: "demo",
      args: ["", "acp", ""],
      env: [
        { key: "", value: "" },
        { key: "TOKEN", value: "secret" },
      ],
      workingDir: "fixed_path" as const,
      path: "/tmp/work",
    };
    expect(acpAgentSpecFromForm(form)).toMatchObject({
      args: ["", "acp", ""],
      env: { TOKEN: "secret" },
      working_dir: { kind: "fixed_path", path: "/tmp/work" },
    });
    // A brand-new form has no arguments at all, not one empty placeholder.
    expect(
      acpAgentSpecFromForm({ ...EMPTY_ACP_FORM, id: "x", name: "X", command: "x" }).args,
    ).toEqual([]);
  });

  it("matches the daemon's platform-specific absolute path rules", () => {
    expect(isPlatformAbsolutePath("/home/me/agent", false)).toBe(true);
    expect(isPlatformAbsolutePath("C:\\agent.exe", false)).toBe(false);
    expect(isPlatformAbsolutePath("C:\\agent.exe", true)).toBe(true);
    expect(isPlatformAbsolutePath("\\\\server\\share\\agent.exe", true)).toBe(true);
    expect(isPlatformAbsolutePath("\\\\server\\share\\agent.exe", false)).toBe(false);
    expect(isPlatformAbsolutePath("relative/agent", true)).toBe(false);
  });

  it("round-trips a configured agent into the edit form", () => {
    const form = formFromAcpAgent({
      id: "demo",
      name: "Demo",
      command: "demo",
      args: ["acp"],
      env: { TOKEN: "secret" },
      working_dir: { kind: "fixed_path", path: "/tmp/work" },
    });
    expect(form).toMatchObject({
      id: "demo",
      name: "Demo",
      command: "demo",
      args: ["acp"],
      env: [{ key: "TOKEN", value: "secret" }],
      workingDir: "fixed_path",
      path: "/tmp/work",
    });
  });
});

describe("ACP picker options", () => {
  it("lists configured agents after the built-ins", () => {
    const options = acpPickerOptions(
      [
        { value: "claude_code", label: "Claude Code" },
        { value: "codex", label: "Codex" },
      ],
      [
        { id: "deepseek", name: "DeepSeek Harness", installed: true },
        { id: "missing", name: "Missing agent", installed: false },
      ],
    );
    expect(options.map((option) => option.value)).toEqual([
      "claude_code",
      "codex",
      "acp:deepseek",
      "acp:missing",
    ]);
    expect(options.at(-2)?.label).toBe("DeepSeek Harness");
    expect(options.at(-1)?.label).toBe("Missing agent · not installed");
  });
});
