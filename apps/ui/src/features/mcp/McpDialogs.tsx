import { PlusIcon, Trash2Icon } from "lucide-react";
import { useEffect, useMemo, useState } from "react";

import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Textarea } from "@/components/ui/textarea";
import type {
  HarnessKind,
  HostId,
  McpImportPreview,
  McpServerInput,
  McpServerView,
  McpSharePolicy,
  McpTestCallResult,
  McpToolInfo,
  McpTransportInput,
  McpTransportView,
} from "@/gen";
import { HARNESS_LABELS } from "@/lib/entities";
import { useApp } from "@/lib/store/appStore";
import { type ConflictChoice, conflictResolution } from "./labels";
import {
  applyMcpImport,
  loadMcpTools,
  previewMcpImport,
  removeMcpServer,
  resolveMcpConflict,
  shareMcpServer,
  testMcpTool,
  upsertMcpServer,
  useMcp,
} from "./store";

const HARNESSES: HarnessKind[] = ["claude_code", "codex", "opencode", "antigravity"];

function FieldLabel({
  children,
  htmlFor,
  hint,
}: {
  children: React.ReactNode;
  htmlFor?: string;
  hint?: string;
}) {
  return (
    <label htmlFor={htmlFor} className="flex items-baseline justify-between">
      <span className="text-[12px] text-[var(--fg-3)]">{children}</span>
      {hint ? <span className="text-[11.5px] text-[var(--fg-3)]">{hint}</span> : null}
    </label>
  );
}

const NO_TOOLS: McpToolInfo[] = [];

const selectClass =
  "h-7 w-full rounded-[6px] border-0 bg-bg-raised px-2 text-[12.5px] text-fg shadow-[inset_0_0_0_1px_var(--line-2)] outline-none focus-visible:shadow-[inset_0_0_0_1px_var(--accent)]";

type SecretRow = { id: string; name: string; value: string };

let secretSeq = 0;

function newSecretRow(name = "", value = ""): SecretRow {
  secretSeq += 1;
  return { id: `secret-${secretSeq}`, name, value };
}

function secretRows(transport: McpTransportView | undefined): SecretRow[] {
  if (!transport) return [newSecretRow()];
  const names = transport.type === "stdio" ? transport.env_keys : transport.header_keys;
  return names.length > 0 ? names.map((name) => newSecretRow(name)) : [newSecretRow()];
}

function splitList(value: string): string[] {
  return value
    .split(/[\n,]/)
    .map((entry) => entry.trim())
    .filter(Boolean);
}

// ---------------------------------------------------------------------------
// Add / edit
// ---------------------------------------------------------------------------

export function McpServerDialog({
  open,
  server,
  onClose,
}: {
  open: boolean;
  server: McpServerView | null;
  onClose: () => void;
}) {
  const [name, setName] = useState("");
  const [kind, setKind] = useState<McpTransportInput["type"]>("stdio");
  const [command, setCommand] = useState("");
  const [args, setArgs] = useState("");
  const [cwd, setCwd] = useState("");
  const [url, setUrl] = useState("");
  const [secrets, setSecrets] = useState<SecretRow[]>([newSecretRow()]);
  const [saving, setSaving] = useState(false);
  const [problem, setProblem] = useState<string | null>(null);

  useEffect(() => {
    if (!open) return;
    const transport = server?.transport;
    setName(server?.name ?? "");
    setKind(transport?.type ?? "stdio");
    setCommand(transport?.type === "stdio" ? transport.command : "");
    setArgs(transport?.type === "stdio" ? transport.args.join("\n") : "");
    setCwd(transport?.type === "stdio" ? (transport.cwd ?? "") : "");
    setUrl(transport && transport.type !== "stdio" ? transport.url : "");
    setSecrets(secretRows(transport));
    setProblem(null);
    setSaving(false);
  }, [open, server]);

  const isStdio = kind === "stdio";
  const secretHeading = isStdio ? "Environment variables" : "Headers";

  const submit = async () => {
    const trimmed = name.trim();
    if (!trimmed) {
      setProblem("A name is required; it becomes the tool namespace.");
      return;
    }
    if (!isStdio && !url.trim()) {
      setProblem("An HTTP or SSE server needs a URL.");
      return;
    }
    if (isStdio && !command.trim()) {
      setProblem("A stdio server needs a command.");
      return;
    }
    const rows = secrets.filter((row) => row.name.trim());
    const transport: McpTransportInput = isStdio
      ? {
          type: "stdio",
          command: command.trim(),
          args: args
            .split("\n")
            .map((entry) => entry.trim())
            .filter(Boolean),
          cwd: cwd.trim() || null,
          env: rows.map((row) => ({
            name: row.name.trim(),
            value: row.value === "" ? null : row.value,
          })),
        }
      : {
          type: kind as "http" | "sse",
          url: url.trim(),
          headers: rows.map((row) => ({
            name: row.name.trim(),
            value: row.value === "" ? null : row.value,
          })),
        };
    const input: McpServerInput = {
      name: trimmed,
      transport,
      tags: server?.tags ?? [],
      enabled: server?.enabled ?? { by_harness: [], by_machine: [] },
    };
    setSaving(true);
    const saved = await upsertMcpServer(input);
    setSaving(false);
    if (saved) onClose();
    else setProblem("The daemon refused the configuration. The previous one is unchanged.");
  };

  return (
    <Dialog open={open} onOpenChange={(value) => (value ? undefined : onClose())}>
      <DialogContent className="gap-3 sm:max-w-[620px]">
        <DialogHeader>
          <DialogTitle>{server ? `Edit ${server.name}` : "Add MCP server"}</DialogTitle>
          <DialogDescription>
            Secret values are write-only. Existing values stay on this machine and are never shown
            again.
          </DialogDescription>
        </DialogHeader>

        <div className="grid gap-3">
          <div className="flex flex-col gap-1">
            <FieldLabel htmlFor="mcp-name">Name</FieldLabel>
            <Input
              id="mcp-name"
              value={name}
              onChange={(event) => setName(event.target.value)}
              placeholder="github"
              autoComplete="off"
              spellCheck={false}
            />
          </div>

          <div className="flex flex-col gap-1">
            <FieldLabel>Transport</FieldLabel>
            <div className="q-seg inline" role="tablist" aria-label="Transport">
              {(["stdio", "http", "sse"] as const).map((option) => (
                <button
                  key={option}
                  type="button"
                  role="tab"
                  aria-selected={kind === option}
                  onClick={() => setKind(option)}
                >
                  {option === "stdio" ? "stdio" : option.toUpperCase()}
                </button>
              ))}
            </div>
          </div>

          {isStdio ? (
            <>
              <div className="flex flex-col gap-1">
                <FieldLabel htmlFor="mcp-command">Command</FieldLabel>
                <Input
                  id="mcp-command"
                  value={command}
                  onChange={(event) => setCommand(event.target.value)}
                  placeholder="npx"
                  spellCheck={false}
                />
              </div>
              <div className="flex flex-col gap-1">
                <FieldLabel htmlFor="mcp-args" hint="One per line">
                  Arguments
                </FieldLabel>
                <Textarea
                  id="mcp-args"
                  value={args}
                  onChange={(event) => setArgs(event.target.value)}
                  placeholder={"-y\n@modelcontextprotocol/server-github"}
                  spellCheck={false}
                  className="min-h-[64px] font-mono text-[12px]"
                />
              </div>
              <div className="flex flex-col gap-1">
                <FieldLabel htmlFor="mcp-cwd" hint="Optional">
                  Working directory
                </FieldLabel>
                <Input
                  id="mcp-cwd"
                  value={cwd}
                  onChange={(event) => setCwd(event.target.value)}
                  placeholder="~/code/api"
                  spellCheck={false}
                />
              </div>
            </>
          ) : (
            <div className="flex flex-col gap-1">
              <FieldLabel htmlFor="mcp-url">Endpoint URL</FieldLabel>
              <Input
                id="mcp-url"
                value={url}
                onChange={(event) => setUrl(event.target.value)}
                placeholder="http://127.0.0.1:8931/mcp"
                spellCheck={false}
              />
            </div>
          )}

          <div className="flex flex-col gap-1">
            <FieldLabel hint="Values are stored as secrets">{secretHeading}</FieldLabel>
            <div className="flex flex-col gap-1.5">
              {secrets.map((row, index) => (
                <div key={row.id} className="flex items-center gap-1.5">
                  <Input
                    value={row.name}
                    aria-label={`${secretHeading} name ${index + 1}`}
                    placeholder="GITHUB_TOKEN"
                    spellCheck={false}
                    onChange={(event) =>
                      setSecrets((current) =>
                        current.map((entry, position) =>
                          position === index ? { ...entry, name: event.target.value } : entry,
                        ),
                      )
                    }
                  />
                  <Input
                    value={row.value}
                    type="password"
                    aria-label={`${secretHeading} value ${index + 1}`}
                    placeholder={row.name && server ? "Unchanged" : "Value"}
                    autoComplete="new-password"
                    onChange={(event) =>
                      setSecrets((current) =>
                        current.map((entry, position) =>
                          position === index ? { ...entry, value: event.target.value } : entry,
                        ),
                      )
                    }
                  />
                  <button
                    type="button"
                    className="q-ibtn"
                    aria-label={`Remove ${secretHeading} row ${index + 1}`}
                    onClick={() =>
                      setSecrets((current) =>
                        current.length === 1
                          ? [newSecretRow()]
                          : current.filter((_, position) => position !== index),
                      )
                    }
                  >
                    <Trash2Icon className="q-icon s14" aria-hidden />
                  </button>
                </div>
              ))}
              <button
                type="button"
                className="q-btn ghost"
                style={{ alignSelf: "flex-start" }}
                onClick={() => setSecrets((current) => [...current, newSecretRow()])}
              >
                <PlusIcon className="q-icon s14" aria-hidden /> Add row
              </button>
            </div>
          </div>

          {problem ? <div className="q-banner err">{problem}</div> : null}
        </div>

        <DialogFooter>
          <Button variant="ghost" onClick={onClose}>
            Cancel
          </Button>
          <Button onClick={() => void submit()} disabled={saving}>
            {saving ? "Saving…" : server ? "Save changes" : "Add server"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

// ---------------------------------------------------------------------------
// Import
// ---------------------------------------------------------------------------

export function McpImportDialog({
  open,
  initialHarness,
  onClose,
}: {
  open: boolean;
  initialHarness: HarnessKind;
  onClose: () => void;
}) {
  const hosts = useApp((state) => state.hosts);
  const localHostId = useApp((state) => state.localHostId);
  const [harness, setHarness] = useState<HarnessKind>(initialHarness);
  const [workspace, setWorkspace] = useState("");
  const [preview, setPreview] = useState<McpImportPreview | null>(null);
  const [selected, setSelected] = useState<Record<string, boolean>>({});
  const [resolutions, setResolutions] = useState<Record<string, string>>({});
  const [renames, setRenames] = useState<Record<string, string>>({});
  const [busy, setBusy] = useState(false);
  const [problem, setProblem] = useState<string | null>(null);

  useEffect(() => {
    if (!open) return;
    setHarness(initialHarness);
    setWorkspace("");
    setPreview(null);
    setSelected({});
    setResolutions({});
    setRenames({});
    setProblem(null);
    setBusy(false);
  }, [open, initialHarness]);

  const host = localHostId ? hosts[localHostId] : Object.values(hosts)[0];
  const read = async () => {
    setBusy(true);
    setProblem(null);
    const result = await previewMcpImport(harness, workspace.trim() || null);
    setBusy(false);
    if (!result) {
      setProblem("That harness config could not be read.");
      return;
    }
    setPreview(result);
    setSelected(
      Object.fromEntries(
        result.candidates
          .filter((candidate) => !candidate.existing)
          .map((candidate) => [candidate.name, true]),
      ),
    );
    setResolutions(
      Object.fromEntries(result.conflicts.map((conflict) => [conflict.name, "keep_mine"])),
    );
  };

  const apply = async () => {
    if (!preview) return;
    setBusy(true);
    const ok = await applyMcpImport(harness, workspace.trim() || null);
    let conflictOk = true;
    for (const conflict of preview.conflicts) {
      const choice = (resolutions[conflict.name] ?? "skip") as ConflictChoice;
      if (choice === "skip") continue;
      const resolution = conflictResolution(choice, renames[conflict.name] ?? "");
      if (!resolution) {
        conflictOk = false;
        continue;
      }
      const settled = await resolveMcpConflict(
        harness,
        conflict.name,
        resolution,
        workspace.trim() || null,
      );
      conflictOk = conflictOk && settled;
    }
    setBusy(false);
    if (ok && conflictOk) onClose();
    else setProblem("Some entries were not imported. The preview still lists what remains.");
  };

  const candidates = preview?.candidates ?? [];
  const chosen = candidates.filter((candidate) => selected[candidate.name]).length;

  return (
    <Dialog open={open} onOpenChange={(value) => (value ? undefined : onClose())}>
      <DialogContent className="gap-3 sm:max-w-[640px]">
        <DialogHeader>
          <DialogTitle>Import from a harness</DialogTitle>
          <DialogDescription>
            h4rn reads the harness&apos;s own files and shows a preview. Nothing is written until
            you import, and conflicts need an explicit answer.
          </DialogDescription>
        </DialogHeader>

        <div className="grid gap-3 sm:grid-cols-2">
          <div className="flex flex-col gap-1">
            <FieldLabel htmlFor="mcp-import-harness">Harness</FieldLabel>
            <select
              id="mcp-import-harness"
              className={selectClass}
              value={harness}
              onChange={(event) => setHarness(event.target.value as HarnessKind)}
            >
              {HARNESSES.map((entry) => (
                <option key={entry} value={entry}>
                  {HARNESS_LABELS[entry]}
                </option>
              ))}
            </select>
          </div>
          <div className="flex flex-col gap-1">
            <FieldLabel htmlFor="mcp-import-workspace" hint="Optional">
              Workspace
            </FieldLabel>
            <Input
              id="mcp-import-workspace"
              value={workspace}
              onChange={(event) => setWorkspace(event.target.value)}
              placeholder="~/code/api"
              spellCheck={false}
            />
          </div>
        </div>

        <div className="flex items-center gap-2">
          <Button variant="secondary" onClick={() => void read()} disabled={busy}>
            {busy ? "Reading…" : "Read config"}
          </Button>
          {preview ? (
            <span className="q-faint" style={{ fontSize: 12 }}>
              {preview.origins.join(" · ") || "No config files found"}
            </span>
          ) : null}
        </div>

        {preview ? (
          <>
            <div className="q-sec-h" style={{ marginTop: 4 }}>
              <h2>Entries</h2>
              <span className="aside">
                {chosen} selected · {preview.conflicts.length} conflicts
              </span>
            </div>
            <div className="q-list">
              {candidates.length === 0 ? (
                <div className="q-li">
                  <span className="q-faint">No servers found in {HARNESS_LABELS[harness]}.</span>
                </div>
              ) : (
                candidates.map((candidate) => (
                  <label key={candidate.name} className="q-li" style={{ cursor: "pointer" }}>
                    <span className="q-check" aria-hidden>
                      <input
                        type="checkbox"
                        checked={Boolean(selected[candidate.name])}
                        disabled={candidate.existing}
                        onChange={(event) =>
                          setSelected((current) => ({
                            ...current,
                            [candidate.name]: event.target.checked,
                          }))
                        }
                        aria-label={`Import ${candidate.name}`}
                      />
                    </span>
                    <div className="grow">
                      <div className="t">{candidate.name}</div>
                      <div className="s">
                        {candidate.transport.type} ·{" "}
                        <span className="q-mono">{origin(candidate.transport)}</span>
                      </div>
                    </div>
                    {candidate.existing ? <span className="q-badge">Already added</span> : null}
                  </label>
                ))
              )}
            </div>

            {preview.conflicts.length > 0 ? (
              <div className="flex flex-col gap-2">
                {preview.conflicts.map((conflict) => (
                  <div key={conflict.name} className="q-conflict">
                    <div className="grow">
                      <div className="t">{conflict.name} already exists</div>
                      <div className="q-muted" style={{ fontSize: 12, marginTop: 2 }}>
                        h4rn has a different configuration for this name. Pick which one to keep.
                      </div>
                      <div className="flex items-center gap-1.5" style={{ marginTop: 8 }}>
                        <select
                          className={selectClass}
                          aria-label={`Resolution for ${conflict.name}`}
                          value={resolutions[conflict.name] ?? "keep_mine"}
                          onChange={(event) =>
                            setResolutions((current) => ({
                              ...current,
                              [conflict.name]: event.target.value,
                            }))
                          }
                        >
                          <option value="keep_mine">Keep mine</option>
                          <option value="use_theirs">Use theirs</option>
                          <option value="rename">Rename theirs</option>
                          <option value="skip">Skip</option>
                        </select>
                        {resolutions[conflict.name] === "rename" ? (
                          <Input
                            value={renames[conflict.name] ?? ""}
                            onChange={(event) =>
                              setRenames((current) => ({
                                ...current,
                                [conflict.name]: event.target.value,
                              }))
                            }
                            placeholder="new name"
                            aria-label={`New name for ${conflict.name}`}
                          />
                        ) : null}
                      </div>
                    </div>
                  </div>
                ))}
              </div>
            ) : null}
          </>
        ) : null}

        {problem ? <div className="q-banner err">{problem}</div> : null}

        <DialogFooter>
          <Button variant="ghost" onClick={onClose}>
            Cancel
          </Button>
          <Button onClick={() => void apply()} disabled={busy || !preview}>
            {busy ? "Importing…" : `Import ${chosen} ${chosen === 1 ? "server" : "servers"}`}
          </Button>
        </DialogFooter>
        {host ? (
          <div className="q-faint" style={{ fontSize: 11.5 }}>
            Reading on {host.name}. Secrets move into the host&apos;s secret store.
          </div>
        ) : null}
      </DialogContent>
    </Dialog>
  );
}

function origin(transport: McpTransportView): string {
  return transport.type === "stdio" ? transport.command : transport.url;
}

// ---------------------------------------------------------------------------
// Share
// ---------------------------------------------------------------------------

export function McpShareDialog({
  open,
  server,
  onClose,
}: {
  open: boolean;
  server: McpServerView | null;
  onClose: () => void;
}) {
  const hosts = useApp((state) => state.hosts);
  const localHost = useApp((state) => state.localHostId);
  const [machine, setMachine] = useState<HostId>("");
  const [allowed, setAllowed] = useState("");
  const [confirmTools, setConfirmTools] = useState("");
  const [confirmAll, setConfirmAll] = useState(false);
  const [busy, setBusy] = useState(false);
  const [problem, setProblem] = useState<string | null>(null);

  const choices = useMemo(
    () =>
      Object.values(hosts)
        .filter((host) => host.host_id !== localHost)
        .sort((left, right) => left.name.localeCompare(right.name)),
    [hosts, localHost],
  );

  useEffect(() => {
    if (!open) return;
    setMachine(choices[0]?.host_id ?? "");
    setAllowed("");
    setConfirmTools("");
    setConfirmAll(false);
    setProblem(null);
    setBusy(false);
  }, [open, choices]);

  const submit = async () => {
    if (!server) return;
    if (!machine) {
      setProblem("Pick a machine to share with.");
      return;
    }
    const policy: McpSharePolicy = {
      allowed_tools: splitList(allowed).length > 0 ? splitList(allowed) : null,
      confirm_tools: splitList(confirmTools),
      confirm_all: confirmAll,
      allowed_devices: null,
    };
    setBusy(true);
    const ok = await shareMcpServer(server.name, machine, policy);
    setBusy(false);
    if (ok) onClose();
    else setProblem("The daemon refused the grant. Nothing was shared.");
  };

  return (
    <Dialog open={open} onOpenChange={(value) => (value ? undefined : onClose())}>
      <DialogContent className="gap-3 sm:max-w-[560px]">
        <DialogHeader>
          <DialogTitle>Share {server?.name ?? "server"}</DialogTitle>
          <DialogDescription>
            The other machine proxies the server; this machine still runs it, enforces the policy
            and keeps the credentials.
          </DialogDescription>
        </DialogHeader>

        <div className="grid gap-3">
          <div className="flex flex-col gap-1">
            <FieldLabel htmlFor="mcp-share-machine">Machine</FieldLabel>
            <select
              id="mcp-share-machine"
              className={selectClass}
              value={machine}
              onChange={(event) => setMachine(event.target.value as HostId)}
            >
              {choices.length === 0 ? <option value="">No other machine</option> : null}
              {choices.map((host) => (
                <option key={host.host_id} value={host.host_id}>
                  {host.name}
                </option>
              ))}
            </select>
          </div>
          <div className="flex flex-col gap-1">
            <FieldLabel htmlFor="mcp-share-allowed" hint="Blank allows every tool">
              Tools allowed
            </FieldLabel>
            <Input
              id="mcp-share-allowed"
              value={allowed}
              onChange={(event) => setAllowed(event.target.value)}
              placeholder="search, read_file"
              spellCheck={false}
            />
          </div>
          <div className="flex flex-col gap-1">
            <FieldLabel htmlFor="mcp-share-confirm" hint="Comma separated">
              Confirm before each call
            </FieldLabel>
            <Input
              id="mcp-share-confirm"
              value={confirmTools}
              onChange={(event) => setConfirmTools(event.target.value)}
              placeholder="write_file, run"
              spellCheck={false}
            />
          </div>
          <label className="flex items-center gap-2 text-[12.5px] text-[var(--fg-2)]">
            <input
              type="checkbox"
              checked={confirmAll}
              onChange={(event) => setConfirmAll(event.target.checked)}
            />
            Ask on this machine before every call
          </label>
        </div>

        {problem ? <div className="q-banner err">{problem}</div> : null}

        <DialogFooter>
          <Button variant="ghost" onClick={onClose}>
            Cancel
          </Button>
          <Button onClick={() => void submit()} disabled={busy || choices.length === 0}>
            {busy ? "Sharing…" : "Share"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

// ---------------------------------------------------------------------------
// Tools and test call
// ---------------------------------------------------------------------------

export function McpToolsDialog({
  open,
  server,
  onClose,
}: {
  open: boolean;
  server: McpServerView | null;
  onClose: () => void;
}) {
  const toolMap = useMcp((state) => state.tools);
  const tools = server ? (toolMap[server.name] ?? NO_TOOLS) : NO_TOOLS;
  const [tool, setTool] = useState("");
  const [args, setArgs] = useState("{}");
  const [result, setResult] = useState<McpTestCallResult | null>(null);
  const [busy, setBusy] = useState(false);
  const [problem, setProblem] = useState<string | null>(null);

  useEffect(() => {
    if (!open || !server) return;
    setTool("");
    setArgs("{}");
    setResult(null);
    setProblem(null);
    void loadMcpTools(server.name);
  }, [open, server]);

  const call = async () => {
    if (!server || !tool) return;
    let parsed: unknown;
    try {
      parsed = JSON.parse(args || "{}");
    } catch {
      setProblem("Arguments must be valid JSON.");
      return;
    }
    setBusy(true);
    setProblem(null);
    const outcome = await testMcpTool(server.name, tool, parsed);
    setBusy(false);
    if (outcome) setResult(outcome);
    else setProblem("The call failed. The server status and arguments are unchanged.");
  };

  return (
    <Dialog open={open} onOpenChange={(value) => (value ? undefined : onClose())}>
      <DialogContent className="gap-3 sm:max-w-[680px]">
        <DialogHeader>
          <DialogTitle>{server?.name ?? "Server"} tools</DialogTitle>
          <DialogDescription>
            Listing starts the server if needed. A test call uses your local authority and is
            recorded on this machine.
          </DialogDescription>
        </DialogHeader>

        <div className="grid gap-2">
          <div className="q-sec-h">
            <h2>Tools</h2>
            <span className="aside">{tools.length} available</span>
          </div>
          <div className="q-list" style={{ maxHeight: 220, overflow: "auto" }}>
            {tools.length === 0 ? (
              <div className="q-li">
                <span className="q-faint">No tools reported yet.</span>
              </div>
            ) : (
              tools.map((entry) => (
                <button
                  key={entry.name}
                  type="button"
                  className="q-li"
                  style={{ textAlign: "left" }}
                  aria-pressed={tool === entry.tool}
                  onClick={() => setTool(entry.tool)}
                >
                  <span className="q-mono" style={{ fontSize: 12 }}>
                    {entry.tool}
                  </span>
                  <div className="grow">
                    <div className="s">{entry.description ?? "No description"}</div>
                  </div>
                  {entry.read_only ? <span className="q-badge ok">read-only</span> : null}
                </button>
              ))
            )}
          </div>

          <div className="flex flex-col gap-1">
            <FieldLabel htmlFor="mcp-call-args" hint="JSON">
              Arguments
            </FieldLabel>
            <Textarea
              id="mcp-call-args"
              value={args}
              onChange={(event) => setArgs(event.target.value)}
              spellCheck={false}
              className="min-h-[72px] font-mono text-[12px]"
            />
          </div>

          {result ? (
            <div className="q-code" style={{ padding: 10, whiteSpace: "pre-wrap", maxHeight: 200 }}>
              <span className={result.is_error ? "q-danger" : "q-ok"}>
                {result.is_error ? "Tool error" : "OK"} · {Number(result.duration_ms)} ms
                {result.truncated ? " · output truncated" : ""}
              </span>
              {"\n"}
              {result.content || "(no content)"}
            </div>
          ) : null}
          {problem ? <div className="q-banner err">{problem}</div> : null}
        </div>

        <DialogFooter>
          <Button variant="ghost" onClick={onClose}>
            Close
          </Button>
          <Button onClick={() => void call()} disabled={busy || !tool}>
            {busy ? "Calling…" : "Test call"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

// ---------------------------------------------------------------------------
// Remove
// ---------------------------------------------------------------------------

export function McpRemoveDialog({
  open,
  server,
  onClose,
  onRemoved,
}: {
  open: boolean;
  server: McpServerView | null;
  onClose: () => void;
  onRemoved: () => void;
}) {
  const [busy, setBusy] = useState(false);
  const [problem, setProblem] = useState<string | null>(null);

  useEffect(() => {
    if (!open) return;
    setBusy(false);
    setProblem(null);
  }, [open]);

  const remove = async () => {
    if (!server) return;
    setBusy(true);
    const ok = await removeMcpServer(server.name);
    setBusy(false);
    if (ok) {
      onRemoved();
      onClose();
    } else {
      setProblem("The daemon did not remove the server. It is still configured.");
    }
  };

  return (
    <Dialog open={open} onOpenChange={(value) => (value ? undefined : onClose())}>
      <DialogContent className="gap-3 sm:max-w-[460px]">
        <DialogHeader>
          <DialogTitle>Remove {server?.name ?? "server"}?</DialogTitle>
          <DialogDescription>
            Its configuration and every secret value stored for it are deleted from this machine.
            Harness sessions that are already running keep their copy until they restart.
          </DialogDescription>
        </DialogHeader>
        {problem ? <div className="q-banner err">{problem}</div> : null}
        <DialogFooter>
          <Button variant="ghost" onClick={onClose}>
            Cancel
          </Button>
          <Button variant="destructive" onClick={() => void remove()} disabled={busy || !server}>
            {busy ? "Removing…" : "Remove server"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
