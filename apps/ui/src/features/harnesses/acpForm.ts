import type { AcpAgentSpec, AcpWorkingDir } from "@/gen";

/** The editable shape of the "Add / edit ACP agent" form. */
export type AcpAgentForm = {
  id: string;
  name: string;
  command: string;
  args: string[];
  env: { key: string; value: string }[];
  workingDir: "thread_workspace" | "fixed_path";
  path: string;
};

export const EMPTY_ACP_FORM: AcpAgentForm = {
  id: "",
  name: "",
  command: "",
  args: [""],
  env: [{ key: "", value: "" }],
  workingDir: "thread_workspace",
  path: "",
};

/** A slug we accept as an instance id: lowercase, digits, single dashes. */
export const ACP_ID_PATTERN = /^[a-z0-9](?:[a-z0-9]|-(?=[a-z0-9]))*$/;

/**
 * Validates the form before a request; the daemon validates again.
 *
 * `existingIds` lets the add form refuse an id that is already configured.
 * Editing passes the id being edited as `editingId`.
 */
export function validateAcpAgentForm(
  form: AcpAgentForm,
  options: { existingIds?: string[]; editingId?: string } = {},
): string | null {
  const id = form.id.trim();
  const name = form.name.trim();
  const command = form.command.trim();

  if (!id) return "An instance id is required.";
  if (!ACP_ID_PATTERN.test(id) || id.includes("--")) {
    return "Use lowercase letters, digits and single dashes; start and end with a letter or digit.";
  }
  if (options.editingId !== undefined && id !== options.editingId) {
    return "The instance id cannot be changed while editing.";
  }
  if (options.editingId === undefined && options.existingIds?.includes(id)) {
    return "That instance id is already in use.";
  }
  if (!name) return "A display name is required.";
  if (name.length > 80) return "Keep the display name under 80 characters.";
  if (/[\n\r\0]/.test(name)) return "The display name must be one line.";
  if (!command) return "A command is required.";
  if (/[\n\r\0]/.test(command)) return "The command must be one line.";
  if ((command.includes("/") || command.includes("\\")) && !isAbsolutePath(command)) {
    return "Use an executable name on PATH, or an absolute path.";
  }

  for (const [index, argument] of form.args.entries()) {
    if (argument.includes("\0")) return `Argument ${index + 1} contains a NUL byte.`;
  }

  for (const [index, row] of form.env.entries()) {
    if (!row.key.trim() && !row.value) continue;
    if (!/^[A-Za-z_][A-Za-z0-9_]*$/.test(row.key.trim())) {
      return `Environment row ${index + 1} needs a name like NAME or NAME_2.`;
    }
    if (row.value.includes("\0")) {
      return `Environment row ${index + 1} contains a NUL byte.`;
    }
  }

  if (form.workingDir === "fixed_path") {
    const path = form.path.trim();
    if (!path) return "Enter the fixed working directory.";
    if (!isAbsolutePath(path)) return "The working directory must be an absolute path.";
  }

  return null;
}

function isAbsolutePath(path: string): boolean {
  return path.startsWith("/") || /^[A-Za-z]:[\\/]/.test(path);
}

/** Converts the form to the daemon's configuration shape. */
export function acpAgentSpecFromForm(form: AcpAgentForm): AcpAgentSpec {
  const working_dir: AcpWorkingDir =
    form.workingDir === "fixed_path"
      ? { kind: "fixed_path", path: form.path.trim() }
      : { kind: "thread_workspace" };
  return {
    id: form.id.trim(),
    name: form.name.trim(),
    command: form.command.trim(),
    args: form.args.filter((argument) => argument.length > 0),
    env: Object.fromEntries(
      form.env.filter((row) => row.key.trim().length > 0).map((row) => [row.key.trim(), row.value]),
    ),
    working_dir,
  };
}

/** Fills the form from a configured agent, for editing. */
export function formFromAcpAgent(agent: {
  id: string;
  name: string;
  command: string;
  args: string[];
  env: Record<string, string | undefined>;
  working_dir: AcpWorkingDir;
}): AcpAgentForm {
  return {
    id: agent.id,
    name: agent.name,
    command: agent.command,
    args: agent.args.length > 0 ? [...agent.args] : [""],
    env:
      Object.keys(agent.env).length > 0
        ? Object.entries(agent.env).map(([key, value]) => ({ key, value: value ?? "" }))
        : [{ key: "", value: "" }],
    workingDir: agent.working_dir.kind,
    path: agent.working_dir.kind === "fixed_path" ? agent.working_dir.path : "",
  };
}

/**
 * The new-thread harness picker: built-ins first, then configured agents.
 *
 * A configured agent that is missing its executable stays listed so the
 * reason a thread cannot start is visible, not hidden behind an empty list.
 */
export function acpPickerOptions(
  builtins: { value: string; label: string }[],
  agents: { id: string; name: string; installed: boolean }[],
): { value: string; label: string }[] {
  return [
    ...builtins,
    ...agents.map((agent) => ({
      value: `acp:${agent.id}`,
      label: agent.installed ? agent.name : `${agent.name} · not installed`,
    })),
  ];
}
