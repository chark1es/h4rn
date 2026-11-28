import { PlusIcon, Trash2Icon } from "lucide-react";
import { useEffect, useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import { ChipSelect } from "@/components/common/ChipSelect";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogBody,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { SegmentedControl, SegmentedControlItem } from "@/components/ui/segmented-control";
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
import { formatNumber } from "@/lib/i18n";
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
      <span className="text-xs text-[var(--fg-3)]">{children}</span>
      {hint ? <span className="text-xs text-[var(--fg-3)]">{hint}</span> : null}
    </label>
  );
}

const NO_TOOLS: McpToolInfo[] = [];

/** Example values for the placeholders: code, so the same in every language. */
const EXAMPLE_ARGS = "-y\n@modelcontextprotocol/server-github";
const EXAMPLE_ALLOWED_TOOLS = "search, read_file";
const EXAMPLE_CONFIRM_TOOLS = "write_file, run";

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
  const { t } = useTranslation(["dialogs", "common"]);
  const [secrets, setSecrets] = useState<SecretRow[]>([newSecretRow()]);
  const [saving, setSaving] = useState(false);
  const [problem, setProblem] = useState<
    "nameRequired" | "urlRequired" | "commandRequired" | "refused" | null
  >(null);

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
  const secretKind = isStdio ? "env" : "headers";

  const submit = async () => {
    const trimmed = name.trim();
    if (!trimmed) {
      setProblem("nameRequired");
      return;
    }
    if (!isStdio && !url.trim()) {
      setProblem("urlRequired");
      return;
    }
    if (isStdio && !command.trim()) {
      setProblem("commandRequired");
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
    else setProblem("refused");
  };

  return (
    <Dialog open={open} onOpenChange={(value) => (value ? undefined : onClose())}>
      <DialogContent size="lg">
        <DialogHeader>
          <DialogTitle>
            {server ? t("mcp.server.titleEdit", { name: server.name }) : t("mcp.server.titleAdd")}
          </DialogTitle>
          <DialogDescription>{t("mcp.server.description")}</DialogDescription>
        </DialogHeader>
        <DialogBody>
          <div className="grid gap-3">
            <div className="flex flex-col gap-1">
              <FieldLabel htmlFor="mcp-name">{t("fields.name")}</FieldLabel>
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
              <FieldLabel>{t("mcp.server.transport")}</FieldLabel>
              <SegmentedControl
                className="self-start"
                aria-label={t("mcp.server.transport")}
                value={kind}
                onValueChange={(value) => {
                  if (value) setKind(value as typeof kind);
                }}
              >
                {(["stdio", "http", "sse"] as const).map((option) => (
                  <SegmentedControlItem key={option} value={option}>
                    {option === "stdio" ? "stdio" : option.toUpperCase()}
                  </SegmentedControlItem>
                ))}
              </SegmentedControl>
            </div>

            {isStdio ? (
              <>
                <div className="flex flex-col gap-1">
                  <FieldLabel htmlFor="mcp-command">{t("mcp.server.command")}</FieldLabel>
                  <Input
                    id="mcp-command"
                    value={command}
                    onChange={(event) => setCommand(event.target.value)}
                    placeholder="npx"
                    spellCheck={false}
                  />
                </div>
                <div className="flex flex-col gap-1">
                  <FieldLabel htmlFor="mcp-args" hint={t("mcp.server.argsHint")}>
                    {t("fields.arguments")}
                  </FieldLabel>
                  <Textarea
                    id="mcp-args"
                    value={args}
                    onChange={(event) => setArgs(event.target.value)}
                    placeholder={EXAMPLE_ARGS}
                    spellCheck={false}
                    className="min-h-[64px] font-mono text-xs"
                  />
                </div>
                <div className="flex flex-col gap-1">
                  <FieldLabel htmlFor="mcp-cwd" hint={t("fields.optional")}>
                    {t("mcp.server.cwd")}
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
                <FieldLabel htmlFor="mcp-url">{t("mcp.server.url")}</FieldLabel>
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
              <FieldLabel hint={t("mcp.server.secretsHint")}>
                {t(`mcp.server.${secretKind}.heading`)}
              </FieldLabel>
              <div className="flex flex-col gap-1.5">
                {secrets.map((row, index) => (
                  <div key={row.id} className="flex items-center gap-1.5">
                    <Input
                      value={row.name}
                      aria-label={t(`mcp.server.${secretKind}.name`, { index: index + 1 })}
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
                      aria-label={t(`mcp.server.${secretKind}.value`, { index: index + 1 })}
                      placeholder={
                        row.name && server ? t("mcp.server.unchanged") : t("mcp.server.value")
                      }
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
                      aria-label={t(`mcp.server.${secretKind}.remove`, { index: index + 1 })}
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
                  className="q-btn ghost q-inline-start"
                  style={{ alignSelf: "flex-start" }}
                  onClick={() => setSecrets((current) => [...current, newSecretRow()])}
                >
                  <PlusIcon className="q-icon s14" aria-hidden /> {t("mcp.server.addRow")}
                </button>
              </div>
            </div>

            {problem ? (
              <div className="q-banner err">{t(`mcp.server.problems.${problem}`)}</div>
            ) : null}
          </div>
        </DialogBody>
        <DialogFooter>
          <Button variant="ghost" onClick={onClose}>
            {t("common:cancel")}
          </Button>
          <Button onClick={() => void submit()} disabled={saving}>
            {saving ? t("common:saving") : server ? t("common:saveChanges") : t("mcp.server.add")}
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
  const { t } = useTranslation(["dialogs", "common"]);
  const [renames, setRenames] = useState<Record<string, string>>({});
  const [busy, setBusy] = useState(false);
  const [problem, setProblem] = useState<"unreadable" | "partial" | null>(null);

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
      setProblem("unreadable");
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
    else setProblem("partial");
  };

  const candidates = preview?.candidates ?? [];
  const chosen = candidates.filter((candidate) => selected[candidate.name]).length;

  return (
    <Dialog open={open} onOpenChange={(value) => (value ? undefined : onClose())}>
      <DialogContent size="lg">
        <DialogHeader>
          <DialogTitle>{t("mcp.import.title")}</DialogTitle>
          <DialogDescription>{t("mcp.import.description")}</DialogDescription>
        </DialogHeader>
        <DialogBody>
          <div className="grid gap-3 sm:grid-cols-2">
            <div className="flex flex-col gap-1">
              <FieldLabel htmlFor="mcp-import-harness">{t("fields.harness")}</FieldLabel>
              <ChipSelect
                id="mcp-import-harness"
                block
                label={t("fields.harness")}
                value={harness}
                options={HARNESSES.map((entry) => ({ value: entry, label: HARNESS_LABELS[entry] }))}
                onChange={setHarness}
              />
            </div>
            <div className="flex flex-col gap-1">
              <FieldLabel htmlFor="mcp-import-workspace" hint={t("fields.optional")}>
                {t("fields.workspace")}
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
              {busy ? t("mcp.import.reading") : t("mcp.import.read")}
            </Button>
            {preview ? (
              <span className="q-faint" style={{ fontSize: 12 }}>
                {preview.origins.join(" · ") || t("mcp.import.noFiles")}
              </span>
            ) : null}
          </div>

          {preview ? (
            <>
              <div className="q-sec-h" style={{ marginTop: 4 }}>
                <h2>{t("mcp.import.entries")}</h2>
                <span className="aside">
                  {t("mcp.import.summary", { selected: chosen, count: preview.conflicts.length })}
                </span>
              </div>
              <div className="q-list">
                {candidates.length === 0 ? (
                  <div className="q-li">
                    <span className="q-faint">
                      {t("mcp.import.noServers", { harness: HARNESS_LABELS[harness] })}
                    </span>
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
                          aria-label={t("mcp.import.importOne", { name: candidate.name })}
                        />
                      </span>
                      <div className="grow">
                        <div className="t">{candidate.name}</div>
                        <div className="s">
                          {candidate.transport.type} ·{" "}
                          <span className="q-mono">{origin(candidate.transport)}</span>
                        </div>
                      </div>
                      {candidate.existing ? (
                        <span className="q-badge">{t("mcp.import.alreadyAdded")}</span>
                      ) : null}
                    </label>
                  ))
                )}
              </div>

              {preview.conflicts.length > 0 ? (
                <div className="flex flex-col gap-2">
                  {preview.conflicts.map((conflict) => (
                    <div key={conflict.name} className="q-conflict">
                      <div className="grow">
                        <div className="t">
                          {t("mcp.import.conflictTitle", { name: conflict.name })}
                        </div>
                        <div className="q-muted" style={{ fontSize: 12, marginTop: 2 }}>
                          {t("mcp.import.conflictBody")}
                        </div>
                        <div className="flex items-center gap-1.5" style={{ marginTop: 8 }}>
                          <ChipSelect
                            label={t("mcp.import.resolutionFor", { name: conflict.name })}
                            className="w-40"
                            value={resolutions[conflict.name] ?? "keep_mine"}
                            options={[
                              { value: "keep_mine", label: t("mcp.import.keepMine") },
                              { value: "use_theirs", label: t("mcp.import.useTheirs") },
                              { value: "rename", label: t("mcp.import.renameTheirs") },
                              { value: "skip", label: t("mcp.import.skip") },
                            ]}
                            onChange={(value) =>
                              setResolutions((current) => ({
                                ...current,
                                [conflict.name]: value,
                              }))
                            }
                          />
                          {resolutions[conflict.name] === "rename" ? (
                            <Input
                              value={renames[conflict.name] ?? ""}
                              onChange={(event) =>
                                setRenames((current) => ({
                                  ...current,
                                  [conflict.name]: event.target.value,
                                }))
                              }
                              placeholder={t("mcp.import.newName")}
                              aria-label={t("mcp.import.newNameFor", { name: conflict.name })}
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

          {problem ? (
            <div className="q-banner err">{t(`mcp.import.problems.${problem}`)}</div>
          ) : null}
        </DialogBody>
        <DialogFooter>
          <Button variant="ghost" onClick={onClose}>
            {t("common:cancel")}
          </Button>
          <Button onClick={() => void apply()} disabled={busy || !preview}>
            {busy ? t("mcp.import.importing") : t("mcp.import.submit", { count: chosen })}
          </Button>
        </DialogFooter>
        {host ? (
          <div className="q-faint" style={{ fontSize: 12 }}>
            {t("mcp.import.readingOn", { host: host.name })}
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
  const { t } = useTranslation(["dialogs", "common"]);
  const [confirmAll, setConfirmAll] = useState(false);
  const [busy, setBusy] = useState(false);
  const [problem, setProblem] = useState<"pickMachine" | "refused" | null>(null);

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
      setProblem("pickMachine");
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
    else setProblem("refused");
  };

  return (
    <Dialog open={open} onOpenChange={(value) => (value ? undefined : onClose())}>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>
            {server ? t("mcp.share.title", { name: server.name }) : t("mcp.share.titleFallback")}
          </DialogTitle>
          <DialogDescription>{t("mcp.share.description")}</DialogDescription>
        </DialogHeader>
        <DialogBody>
          <div className="grid gap-3">
            <div className="flex flex-col gap-1">
              <FieldLabel htmlFor="mcp-share-machine">{t("mcp.share.machine")}</FieldLabel>
              <ChipSelect
                id="mcp-share-machine"
                block
                label={t("mcp.share.machine")}
                placeholder={t("mcp.share.noMachine")}
                value={machine}
                options={choices.map((host) => ({ value: host.host_id, label: host.name }))}
                onChange={setMachine}
              />
            </div>
            <div className="flex flex-col gap-1">
              <FieldLabel htmlFor="mcp-share-allowed" hint={t("mcp.share.allowedHint")}>
                {t("mcp.share.allowed")}
              </FieldLabel>
              <Input
                id="mcp-share-allowed"
                value={allowed}
                onChange={(event) => setAllowed(event.target.value)}
                placeholder={EXAMPLE_ALLOWED_TOOLS}
                spellCheck={false}
              />
            </div>
            <div className="flex flex-col gap-1">
              <FieldLabel htmlFor="mcp-share-confirm" hint={t("mcp.share.confirmHint")}>
                {t("mcp.share.confirm")}
              </FieldLabel>
              <Input
                id="mcp-share-confirm"
                value={confirmTools}
                onChange={(event) => setConfirmTools(event.target.value)}
                placeholder={EXAMPLE_CONFIRM_TOOLS}
                spellCheck={false}
              />
            </div>
            <label className="flex items-center gap-2 text-sm text-[var(--fg-2)]">
              <input
                type="checkbox"
                checked={confirmAll}
                onChange={(event) => setConfirmAll(event.target.checked)}
              />
              {t("mcp.share.confirmAll")}
            </label>
          </div>

          {problem ? (
            <div className="q-banner err">{t(`mcp.share.problems.${problem}`)}</div>
          ) : null}
        </DialogBody>
        <DialogFooter>
          <Button variant="ghost" onClick={onClose}>
            {t("common:cancel")}
          </Button>
          <Button onClick={() => void submit()} disabled={busy || choices.length === 0}>
            {busy ? t("mcp.share.sharing") : t("mcp.share.share")}
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
  const { t } = useTranslation(["dialogs", "common"]);
  const [result, setResult] = useState<McpTestCallResult | null>(null);
  const [busy, setBusy] = useState(false);
  const [problem, setProblem] = useState<"badJson" | "failed" | null>(null);

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
      setProblem("badJson");
      return;
    }
    setBusy(true);
    setProblem(null);
    const outcome = await testMcpTool(server.name, tool, parsed);
    setBusy(false);
    if (outcome) setResult(outcome);
    else setProblem("failed");
  };

  return (
    <Dialog open={open} onOpenChange={(value) => (value ? undefined : onClose())}>
      <DialogContent size="lg">
        <DialogHeader>
          <DialogTitle>
            {server ? t("mcp.tools.title", { name: server.name }) : t("mcp.tools.titleFallback")}
          </DialogTitle>
          <DialogDescription>{t("mcp.tools.description")}</DialogDescription>
        </DialogHeader>
        <DialogBody>
          <div className="grid gap-2">
            <div className="q-sec-h">
              <h2>{t("mcp.tools.heading")}</h2>
              <span className="aside">{t("mcp.tools.available", { count: tools.length })}</span>
            </div>
            <div className="q-list" style={{ maxHeight: 220, overflow: "auto" }}>
              {tools.length === 0 ? (
                <div className="q-li">
                  <span className="q-faint">{t("mcp.tools.none")}</span>
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
                      <div className="s">{entry.description ?? t("mcp.tools.noDescription")}</div>
                    </div>
                    {entry.read_only ? (
                      <span className="q-badge ok">{t("mcp.tools.readOnly")}</span>
                    ) : null}
                  </button>
                ))
              )}
            </div>

            <div className="flex flex-col gap-1">
              <FieldLabel htmlFor="mcp-call-args" hint={t("mcp.tools.argsHint")}>
                {t("fields.arguments")}
              </FieldLabel>
              <Textarea
                id="mcp-call-args"
                value={args}
                onChange={(event) => setArgs(event.target.value)}
                spellCheck={false}
                className="min-h-[72px] font-mono text-xs"
              />
            </div>

            {result ? (
              <div
                className="q-code"
                style={{ padding: 10, whiteSpace: "pre-wrap", maxHeight: 200 }}
              >
                <span className={result.is_error ? "q-danger" : "q-ok"}>
                  {result.is_error ? t("mcp.tools.toolError") : t("mcp.tools.ok")} ·{" "}
                  {formatNumber(Number(result.duration_ms), { style: "unit", unit: "millisecond" })}
                  {result.truncated ? ` · ${t("mcp.tools.truncated")}` : null}
                </span>
                {"\n"}
                {result.content || t("mcp.tools.noContent")}
              </div>
            ) : null}
            {problem ? (
              <div className="q-banner err">{t(`mcp.tools.problems.${problem}`)}</div>
            ) : null}
          </div>
        </DialogBody>
        <DialogFooter>
          <Button variant="ghost" onClick={onClose}>
            {t("common:close")}
          </Button>
          <Button onClick={() => void call()} disabled={busy || !tool}>
            {busy ? t("mcp.tools.calling") : t("mcp.tools.testCall")}
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
  const { t } = useTranslation(["dialogs", "common"]);
  const [busy, setBusy] = useState(false);
  const [problem, setProblem] = useState(false);

  useEffect(() => {
    if (!open) return;
    setBusy(false);
    setProblem(false);
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
      setProblem(true);
    }
  };

  return (
    <Dialog open={open} onOpenChange={(value) => (value ? undefined : onClose())}>
      <DialogContent size="sm">
        <DialogHeader>
          <DialogTitle>
            {server ? t("mcp.remove.title", { name: server.name }) : t("mcp.remove.titleFallback")}
          </DialogTitle>
          <DialogDescription>{t("mcp.remove.description")}</DialogDescription>
        </DialogHeader>
        {problem ? (
          <DialogBody>
            <div className="q-banner err">{t("mcp.remove.problem")}</div>
          </DialogBody>
        ) : null}
        <DialogFooter>
          <Button variant="ghost" onClick={onClose}>
            {t("common:cancel")}
          </Button>
          <Button variant="destructive" onClick={() => void remove()} disabled={busy || !server}>
            {busy ? t("removing") : t("mcp.remove.button")}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
