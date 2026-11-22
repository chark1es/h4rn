import { describe, expect, it } from "vitest";

import {
  ACP_ID_PATTERN,
  acpAgentSpecFromForm,
  acpModelChoices,
  acpPickerOptions,
  acpRuntimeModeChoices,
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

  it("accepts either platform's absolute form, because the daemon decides", () => {
    expect(isPlatformAbsolutePath("/home/me/agent")).toBe(true);
    // A phone or a browser on one platform may configure a daemon on another.
    expect(isPlatformAbsolutePath("C:\\agent.exe")).toBe(true);
    expect(isPlatformAbsolutePath("C:/agent.exe")).toBe(true);
    expect(isPlatformAbsolutePath("\\\\server\\share\\agent.exe")).toBe(true);
    expect(isPlatformAbsolutePath("relative/agent")).toBe(false);
    expect(isPlatformAbsolutePath("agent.exe")).toBe(false);
    expect(isPlatformAbsolutePath("C:agent.exe")).toBe(false);

    const base = { ...EMPTY_ACP_FORM, id: "demo", name: "Demo", command: "demo" };
    expect(
      validateAcpAgentForm({ ...base, workingDir: "fixed_path", path: "C:\\work" }),
    ).toBeNull();
    expect(
      validateAcpAgentForm({ ...base, workingDir: "fixed_path", path: "\\\\server\\share" }),
    ).toBeNull();
  });

  it("counts display-name characters the way the daemon does", () => {
    const base = { ...EMPTY_ACP_FORM, id: "demo", command: "demo" };
    // 80 characters, even though they are 160 bytes in UTF-8.
    expect(validateAcpAgentForm({ ...base, name: "é".repeat(80) })).toBeNull();
    expect(validateAcpAgentForm({ ...base, name: "é".repeat(81) })).toContain("80 characters");
    // An emoji is one character, not two UTF-16 units.
    expect(validateAcpAgentForm({ ...base, name: "🦀".repeat(80) })).toBeNull();
    expect(validateAcpAgentForm({ ...base, name: "🦀".repeat(81) })).toContain("80 characters");
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

describe("ACP model choices", () => {
  it("uses a successful probe's models", () => {
    expect(
      acpModelChoices({ models: [{ id: "a" }, { id: "b" }] }, false, [{ id: "cached" }]),
    ).toEqual(["a", "b"]);
  });

  it("falls back to the daemon's cached models when the probe failed", () => {
    expect(acpModelChoices(undefined, false, [{ id: "cached" }])).toEqual(["cached"]);
    // A probe that ran but failed carries no models; the last good result in
    // the list is still the truth about this agent.
    expect(acpModelChoices({ models: [] }, true, [{ id: "cached" }])).toEqual(["cached"]);
    // Nothing known yet is an empty list, not an invented model.
    expect(acpModelChoices(undefined, true, undefined)).toEqual([]);
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

  it("maps an agent's own mode ids onto the modes the daemon accepts", () => {
    expect(acpRuntimeModeChoices(["acceptEdits", "bypassPermissions"])).toEqual([
      { value: "auto_accept_edits", label: "Auto-accept edits" },
      { value: "full_access", label: "Full access" },
    ]);
    // A plan mode is offered when the agent advertises one, with the daemon's
    // other spellings accepted too.
    expect(acpRuntimeModeChoices(["read-only", "accept-edits"])).toEqual([
      { value: "auto_accept_edits", label: "Auto-accept edits" },
      { value: "plan", label: "Plan" },
    ]);
    expect(acpRuntimeModeChoices(["code", "architect"])).toEqual([]);
  });
});
