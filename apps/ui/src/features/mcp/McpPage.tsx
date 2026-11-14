import {
  ArrowLeftIcon,
  CheckCircle2Icon,
  ChevronRightIcon,
  KeyRoundIcon,
  MoreHorizontalIcon,
  PlusIcon,
  RefreshCwIcon,
  Share2Icon,
  Trash2Icon,
  TriangleAlertIcon,
  UploadIcon,
  WrenchIcon,
} from "lucide-react";
import { useEffect, useMemo, useState } from "react";

import { EntityAvatar } from "@/components/agent/EntityAvatar";
import { Popover } from "@/components/common/Popover";
import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import type { HarnessKind, HostId, McpServerView, ProtoError } from "@/gen";
import { useIsPhone } from "@/hooks/use-media-query";
import { useNow } from "@/hooks/use-now";
import { HARNESS_LABELS, harnessSeed, machineSeed } from "@/lib/entities";
import { relativeAge } from "@/lib/format";
import { setLibrarySelected, useApp } from "@/lib/store/appStore";
import { cn } from "@/lib/utils";
import {
  CONFLICT_CHOICES,
  type ConflictChoice,
  callerLabel,
  conflictResolution,
  conflictText,
  DELIVERY,
  enabledFor,
  harnessName,
  MCP_HARNESSES,
  secretLabel,
  secretNames,
  serverStatus,
  transportKind,
  transportSummary,
} from "./labels";
import {
  McpImportDialog,
  McpRemoveDialog,
  McpServerDialog,
  McpShareDialog,
  McpToolsDialog,
} from "./McpDialogs";
import {
  ensureMcpEvents,
  loadMcpServers,
  localHostId,
  resolveMcpConflict,
  setMcpEnabled,
  unshareMcpServer,
  useMcp,
} from "./store";

const NEW_SERVER = "__new";
/** Machine-wide rows are not harness specific; show them under the first harness. */
const PRIMARY_HARNESS: HarnessKind = MCP_HARNESSES[0] ?? "claude_code";

function checkedLabel(checkedAt: number | null, now: number): string {
  if (!checkedAt) return "not checked yet";
  const age = relativeAge(new Date(checkedAt).toISOString(), now);
  return age === "just now" ? "checked just now" : `checked ${age} ago`;
}

function Health({ server }: { server: McpServerView }) {
  const status = serverStatus(server);
  return (
    <span
      className={cn(
        "q-health",
        status.tone === "ok" && "q-ok",
        status.tone === "warn" && "q-warn",
        status.tone === "danger" && "q-danger",
      )}
    >
      {status.tone === "ok" ? (
        <CheckCircle2Icon className="q-icon s12" aria-hidden />
      ) : status.tone === "danger" || status.tone === "warn" ? (
        <TriangleAlertIcon className="q-icon s12" aria-hidden />
      ) : null}
      {status.label}
    </span>
  );
}

function Toggle({
  checked,
  label,
  busy,
  onToggle,
}: {
  checked: boolean;
  label: string;
  busy?: boolean;
  onToggle: () => void;
}) {
  return (
    <button
      type="button"
      role="switch"
      aria-checked={checked}
      aria-label={label}
      aria-disabled={busy ? "true" : undefined}
      disabled={busy}
      className="q-switch"
      onClick={onToggle}
    />
  );
}

function serverHarnesses(): HarnessKind[] {
  // The daemon stores only overrides; a missing entry means enabled. Show the
  // full matrix so a switch turned off somewhere never hides the other rows.
  return MCP_HARNESSES;
}

type MatrixRow = {
  key: string;
  hostId: HostId;
  hostName: string;
  harness: HarnessKind;
  /** Which dimension this row's switch writes: a harness or a whole machine. */
  target: "harness" | "machine";
  delivery: string;
  enabled: boolean;
  note: string;
  tone: "ok" | "warn" | "off";
  busy: boolean;
  onToggle: () => void;
};

function matrixRows(server: McpServerView): MatrixRow[] {
  const localHost = localHostId() ?? "local";
  const rows: MatrixRow[] = [];
  for (const harness of serverHarnesses()) {
    const enabled = enabledFor(server, harness, null);
    rows.push({
      key: `${localHost}:${harness}`,
      hostId: localHost,
      hostName: "",
      harness,
      target: "harness",
      delivery: DELIVERY[harness],
      enabled,
      note: enabled ? (server.tool_count > 0 ? `${server.tool_count} tools` : "Ready") : "Off",
      tone: enabled ? "ok" : "off",
      busy: false,
      onToggle: () => undefined,
    });
  }
  const conflict = conflictText(server);
  // Machines where this server is actually reachable: granted shares plus any
  // explicit switch the daemon already stores. The local machine is the rows
  // above, so it is not repeated here.
  const machineIds = new Set<string>([
    ...server.shared_to,
    ...server.enabled.by_machine.map((entry) => entry.machine),
  ]);
  machineIds.delete(localHost);
  [...machineIds].forEach((machine, index) => {
    const harness = PRIMARY_HARNESS;
    const enabled = enabledFor(server, harness, machine);
    rows.push({
      key: `${machine}:${harness}`,
      hostId: machine,
      hostName: "",
      harness,
      target: "machine",
      delivery: "Proxied hub entry",
      enabled,
      note: !enabled
        ? "Off"
        : conflict && index === 0
          ? "Name conflict"
          : "Can call this machine's server",
      tone: !enabled ? "off" : conflict && index === 0 ? "warn" : "ok",
      busy: false,
      onToggle: () => undefined,
    });
  });
  return rows;
}

function ConflictCard({ server }: { server: McpServerView }) {
  const text = conflictText(server);
  if (!text) return null;
  return (
    <div className="q-conflict">
      <TriangleAlertIcon className="q-icon q-warn" aria-hidden />
      <div className="grow">
        <div className="t">{text}</div>
        <div className="q-muted" style={{ fontSize: 12, marginTop: 2 }}>
          Pick which configuration the harness should see. Nothing is overwritten until you settle
          this.
        </div>
      </div>
      <Popover
        label={`Resolve the ${server.name} conflict`}
        align="end"
        width={340}
        trigger={() => "Resolve…"}
      >
        {({ close }) => <ResolvePanel server={server} close={close} />}
      </Popover>
    </div>
  );
}

/** The "Resolve…" popover: which harness, which side wins, and a name if renaming. */
function ResolvePanel({ server, close }: { server: McpServerView; close: () => void }) {
  const [harness, setHarness] = useState<HarnessKind>(
    MCP_HARNESSES.includes(server.imported_from as HarnessKind)
      ? (server.imported_from as HarnessKind)
      : "claude_code",
  );
  const [choice, setChoice] = useState<ConflictChoice>("keep_mine");
  const [rename, setRename] = useState("");
  const [busy, setBusy] = useState(false);
  const target = conflictResolution(choice, rename);

  const settle = async () => {
    if (!target) return;
    setBusy(true);
    const settled = await resolveMcpConflict(harness, server.name, target, null);
    setBusy(false);
    if (settled) close();
  };

  return (
    <form
      onSubmit={(event) => {
        event.preventDefault();
        void settle();
      }}
    >
      <div style={{ fontWeight: 600, marginBottom: 2 }}>Resolve {server.name}</div>
      <div className="q-muted" style={{ fontSize: 12, marginBottom: 10 }}>
        Choose which configuration {HARNESS_LABELS[harness]} should see.
      </div>
      <div className="q-seg q-resolve-seg" role="radiogroup" aria-label="Harness">
        {MCP_HARNESSES.map((entry) => (
          // biome-ignore lint/a11y/useSemanticElements: a segmented radio group, not a form radio
          <button
            key={entry}
            type="button"
            role="radio"
            aria-checked={harness === entry}
            aria-label={HARNESS_LABELS[entry]}
            onClick={() => setHarness(entry)}
          >
            {HARNESS_LABELS[entry]}
          </button>
        ))}
      </div>
      <div role="radiogroup" aria-label="Resolution">
        {CONFLICT_CHOICES.map((option) => (
          <label key={option.value} className="q-resolve-opt">
            <input
              type="radio"
              name={`resolve-${server.id}`}
              className="sr-only"
              checked={choice === option.value}
              onChange={() => setChoice(option.value)}
            />
            <span className={cn("q-radio", choice === option.value && "on")} aria-hidden />
            <span>
              <span style={{ fontWeight: 500 }}>{option.label}</span>
              <span className="q-faint sub">{option.hint}</span>
            </span>
          </label>
        ))}
      </div>
      {choice === "rename" ? (
        <input
          className="q-input"
          style={{ marginTop: 8 }}
          value={rename}
          onChange={(event) => setRename(event.target.value)}
          placeholder="New name"
          aria-label={`New name for ${server.name}`}
        />
      ) : null}
      <div style={{ display: "flex", justifyContent: "flex-end", gap: 6, marginTop: 12 }}>
        <button type="button" className="q-btn ghost" onClick={close}>
          Cancel
        </button>
        <button type="submit" className="q-btn primary" disabled={busy || !target}>
          {busy ? "Resolving…" : "Resolve"}
        </button>
      </div>
    </form>
  );
}

function CredentialField({ server }: { server: McpServerView }) {
  const hosts = useApp((state) => state.hosts);
  const local = useApp((state) => state.localHostId);
  const keys = secretNames(server.transport);
  const hostName = local ? (hosts[local]?.name ?? local) : "this host";
  const remoteHost = server.runs_on ? (hosts[server.runs_on]?.name ?? server.runs_on) : null;
  return (
    <div className="q-field">
      <div className="lab">
        <div className="t">Credential</div>
        <div className="s">Never leaves the host</div>
      </div>
      <div className="ctl">
        {keys.length > 0 ? (
          <>
            <span className="q-secret">
              <KeyRoundIcon className="q-icon s12" aria-hidden /> Stored on {remoteHost ?? hostName}
            </span>
            <span className="q-faint" style={{ fontSize: 12 }}>
              {keys.length} {secretLabel(server.transport)} · values are write-only
            </span>
          </>
        ) : (
          <span className="q-faint" style={{ fontSize: 12 }}>
            No {secretLabel(server.transport)} stored for this server.
          </span>
        )}
        {server.shared_to.length > 0 ? (
          <span className="q-secret">
            <Share2Icon className="q-icon s12" aria-hidden /> Shared with{" "}
            {server.shared_to.map((machine) => hosts[machine]?.name ?? machine).join(", ")}
          </span>
        ) : null}
      </div>
    </div>
  );
}

function SharedWithYou({ current }: { current: McpServerView | null }) {
  const servers = useMcp((state) => state.servers);
  const hosts = useApp((state) => state.hosts);
  const shared = servers.filter((server) => Boolean(server.runs_on));
  if (shared.length === 0) return null;
  return (
    <div className="q-sec">
      <div className="q-sec-h">
        <h2>Shared with you</h2>
        <span className="aside">Defined on another host</span>
      </div>
      <div className="q-list">
        {shared.map((server) => {
          const machine = server.runs_on ?? "";
          const host = hosts[machine];
          const enabled = enabledFor(server, PRIMARY_HARNESS, machine);
          return (
            <div className="q-li" key={server.id}>
              <EntityAvatar
                seed={machineSeed(machine)}
                kind="machine"
                label={host?.name ?? machine}
                size={18}
              />
              <button
                type="button"
                className="grow text-left"
                onClick={() => setLibrarySelected(server.name)}
              >
                <div className="t">
                  {server.name}{" "}
                  <span className="q-faint" style={{ fontWeight: 400 }}>
                    from {host?.name ?? machine}
                  </span>
                </div>
                <div className="s">
                  {transportKind(server.transport)} ·{" "}
                  <span className="q-mono">{transportSummary(server.transport)}</span> · runs on{" "}
                  {host?.name ?? machine}
                </div>
              </button>
              <span className="q-secret">
                <KeyRoundIcon className="q-icon s12" aria-hidden /> Stored on{" "}
                {host?.name ?? machine}
              </span>
              <Toggle
                checked={enabled}
                label={`Use ${server.name} on this machine`}
                onToggle={() =>
                  void setMcpEnabled(server.name, { machine: localHostId() ?? "" }, !enabled)
                }
              />
              {current?.id === server.id ? null : (
                <ChevronRightIcon className="q-icon s12 q-faint" aria-hidden />
              )}
            </div>
          );
        })}
      </div>
    </div>
  );
}

function SharedFromHere({ server }: { server: McpServerView }) {
  const hosts = useApp((state) => state.hosts);
  if (server.shared_to.length === 0) return null;
  return (
    <div className="q-sec">
      <div className="q-sec-h">
        <h2>Shared from this machine</h2>
        <span className="aside">This machine runs the server and enforces the policy</span>
      </div>
      <div className="q-list">
        {server.shared_to.map((machine) => (
          <div className="q-li" key={machine}>
            <EntityAvatar
              seed={machineSeed(machine)}
              kind="machine"
              label={hosts[machine]?.name ?? machine}
              size={18}
            />
            <div className="grow">
              <div className="t">{hosts[machine]?.name ?? machine}</div>
              <div className="s">Proxies this server over the relay or Tailscale.</div>
            </div>
            <Button
              variant="ghost"
              size="sm"
              onClick={() => void unshareMcpServer(server.name, machine)}
            >
              Unshare
            </Button>
          </div>
        ))}
      </div>
    </div>
  );
}

function Activity({ server }: { server: McpServerView }) {
  const activity = useMcp((state) => state.activity);
  const now = useNow();
  const forServer = activity.filter((entry) => entry.server === server.name);
  if (forServer.length === 0) return null;
  return (
    <div className="q-sec">
      <div className="q-sec-h">
        <h2>Recent tool calls</h2>
        <span className="aside">Live from the daemon</span>
      </div>
      <div className="q-list">
        {forServer.slice(0, 8).map((entry) => (
          <div className="q-li" key={entry.id}>
            <span className={entry.ok ? "q-ok" : "q-danger"}>
              {entry.ok ? (
                <CheckCircle2Icon className="q-icon s14" aria-hidden />
              ) : (
                <TriangleAlertIcon className="q-icon s14" aria-hidden />
              )}
            </span>
            <div className="grow">
              <div className="t q-mono" style={{ fontSize: 12.5 }}>
                {entry.qualifiedName}
              </div>
              <div className="s">
                {callerLabel(entry.caller)} · {Number(entry.durationMs)} ms
                {entry.error ? ` · ${entry.error}` : ""}
              </div>
            </div>
            <span className="q-faint q-num" style={{ fontSize: 12 }}>
              {relativeAge(entry.at, now)}
            </span>
          </div>
        ))}
      </div>
    </div>
  );
}

function SkeletonDetail() {
  return (
    <div
      className="q-wrap"
      style={{ maxWidth: 860 }}
      role="status"
      aria-busy="true"
      aria-label="Loading server"
    >
      <div className="q-skel" style={{ width: 180, height: 22, marginBottom: 8 }} />
      <div className="q-skel" style={{ width: 420, height: 12, marginBottom: 28 }} />
      <div className="q-skel" style={{ width: "100%", height: 48, marginBottom: 8 }} />
      <div className="q-skel" style={{ width: "100%", height: 168 }} />
    </div>
  );
}

function ErrorBanner({ error, onRetry }: { error: ProtoError | null; onRetry: () => void }) {
  if (!error) return null;
  return (
    <div className="q-banner err" style={{ margin: "0 0 16px" }}>
      <span className="ic">
        <TriangleAlertIcon className="q-icon s14" aria-hidden />
      </span>
      <span className="grow">
        <b>{error.message}</b> {error.hint ?? "The daemon did not answer."}
      </span>
      <Button variant="ghost" size="sm" onClick={onRetry}>
        Retry
      </Button>
    </div>
  );
}

function EmptyState({ onAdd, onImport }: { onAdd: () => void; onImport: () => void }) {
  return (
    <div className="q-center-empty">
      <div className="q-empty q-box" style={{ alignItems: "flex-start", padding: 0 }}>
        <h3>No MCP servers yet</h3>
        <p>
          Add a server once and h4rn delivers it to the harnesses and machines you choose. Secrets
          stay on the machine that runs it.
        </p>
        <div className="flex gap-2">
          <Button onClick={onAdd}>
            <PlusIcon className="q-icon s14" aria-hidden /> Add server
          </Button>
          <Button variant="secondary" onClick={onImport}>
            <UploadIcon className="q-icon s14" aria-hidden /> Import from a harness
          </Button>
        </div>
      </div>
    </div>
  );
}

function ServerList({
  onSelect,
  onAdd,
  onImport,
}: {
  onSelect: (server: McpServerView) => void;
  onAdd: () => void;
  onImport: () => void;
}) {
  const servers = useMcp((state) => state.servers);
  const loading = useMcp((state) => state.loading);
  const error = useMcp((state) => state.error);
  const isPhone = useIsPhone();
  return (
    <main className="q-main" aria-label="MCP servers">
      <div className="q-page">
        <div className="q-wrap" style={{ maxWidth: 860 }}>
          <div className="mb-5 flex items-start gap-3">
            <div className="grow">
              <h1 className="text-[20px] leading-[26px] font-semibold tracking-[-0.015em]">
                MCP servers
              </h1>
              <div className="q-muted mt-1">
                {servers.length} {servers.length === 1 ? "server" : "servers"} · delivered per
                harness and machine
              </div>
            </div>
            <Button variant="secondary" onClick={onImport}>
              <UploadIcon className="q-icon s14" aria-hidden /> Import
            </Button>
            <Button onClick={onAdd}>
              <PlusIcon className="q-icon s14" aria-hidden /> Add
            </Button>
          </div>

          <ErrorBanner error={error} onRetry={() => void loadMcpServers({ force: true })} />

          {loading && servers.length === 0 ? (
            <div className="q-list" aria-busy="true">
              {[0, 1, 2].map((row) => (
                <div className="q-li" key={row}>
                  <div className="q-skel" style={{ width: 20, height: 20, borderRadius: 6 }} />
                  <div className="grow">
                    <div className="q-skel" style={{ width: `${58 + row * 9}%`, height: 12 }} />
                    <div
                      className="q-skel"
                      style={{ width: `${36 + row * 7}%`, height: 9, marginTop: 6 }}
                    />
                  </div>
                </div>
              ))}
            </div>
          ) : servers.length === 0 ? (
            <EmptyState onAdd={onAdd} onImport={onImport} />
          ) : (
            <div className="q-list">
              {servers.map((server) => {
                const status = serverStatus(server);
                return (
                  <button
                    type="button"
                    className="q-li w-full text-left"
                    key={server.id}
                    onClick={() => onSelect(server)}
                  >
                    <span style={{ width: 20, color: "var(--fg-2)" }}>
                      <WrenchIcon className="q-icon" strokeWidth={1.5} aria-hidden />
                    </span>
                    <span className="grow min-w-0">
                      <span className="t block truncate">{server.name}</span>
                      <span className="s block truncate">
                        {transportKind(server.transport)} ·{" "}
                        <span className="q-mono">{transportSummary(server.transport)}</span>
                        {isPhone ? ` · ${status.label}` : null}
                      </span>
                    </span>
                    {isPhone ? null : <Health server={server} />}
                    {isPhone ? null : (
                      <ChevronRightIcon className="q-icon s14 q-faint" aria-hidden />
                    )}
                  </button>
                );
              })}
            </div>
          )}
        </div>
      </div>
    </main>
  );
}

export function McpPage() {
  const isPhone = useIsPhone();
  const servers = useMcp((state) => state.servers);
  const loading = useMcp((state) => state.loading);
  const refreshing = useMcp((state) => state.refreshing);
  const error = useMcp((state) => state.error);
  const pending = useMcp((state) => state.pending);
  const checkedAt = useMcp((state) => state.checkedAt);
  const selected = useApp((state) => state.ui.layout.librarySelected);
  const hosts = useApp((state) => state.hosts);
  const daemonHost = useApp((state) => state.localHostId);
  const now = useNow();

  const [phoneOpen, setPhoneOpen] = useState(false);
  const [addOpen, setAddOpen] = useState(false);
  const [editing, setEditing] = useState<McpServerView | null>(null);
  const [importOpen, setImportOpen] = useState(false);
  const [shareServer, setShareServer] = useState<McpServerView | null>(null);
  const [toolsServer, setToolsServer] = useState<McpServerView | null>(null);
  const [removeServer, setRemoveServer] = useState<McpServerView | null>(null);

  useEffect(() => {
    ensureMcpEvents();
  }, []);

  // A deep link can mount this page before the daemon's hello reports its
  // host; retry as soon as it arrives instead of leaving an empty list up.
  useEffect(() => {
    if (daemonHost) void loadMcpServers();
  }, [daemonHost]);

  useEffect(() => {
    if (selected !== NEW_SERVER) return;
    setEditing(null);
    setAddOpen(true);
  }, [selected]);

  const server = useMemo(
    () => servers.find((entry) => entry.name === selected) ?? servers[0] ?? null,
    [servers, selected],
  );

  const openAdd = () => {
    setEditing(null);
    setAddOpen(true);
  };
  const closeAdd = () => {
    setAddOpen(false);
    setEditing(null);
    if (selected === NEW_SERVER) setLibrarySelected(servers[0]?.name ?? "");
  };
  const openImport = () => setImportOpen(true);
  const selectServer = (entry: McpServerView) => {
    setLibrarySelected(entry.name);
    setPhoneOpen(true);
  };

  const importHarness =
    server && MCP_HARNESSES.includes(server.imported_from as HarnessKind)
      ? (server.imported_from as HarnessKind)
      : "claude_code";

  const dialogs = (
    <>
      <McpServerDialog open={addOpen} server={editing} onClose={closeAdd} />
      <McpImportDialog
        open={importOpen}
        initialHarness={importHarness}
        onClose={() => setImportOpen(false)}
      />
      <McpShareDialog
        open={Boolean(shareServer)}
        server={shareServer}
        onClose={() => setShareServer(null)}
      />
      <McpToolsDialog
        open={Boolean(toolsServer)}
        server={toolsServer}
        onClose={() => setToolsServer(null)}
      />
      <McpRemoveDialog
        open={Boolean(removeServer)}
        server={removeServer}
        onClose={() => setRemoveServer(null)}
        onRemoved={() => {
          const remaining = servers.filter((entry) => entry.name !== removeServer?.name);
          setLibrarySelected(remaining[0]?.name ?? "");
          setPhoneOpen(false);
        }}
      />
    </>
  );

  if (isPhone && !phoneOpen) {
    return (
      <>
        <ServerList onSelect={selectServer} onAdd={openAdd} onImport={openImport} />
        {dialogs}
      </>
    );
  }

  if (loading && servers.length === 0) {
    return (
      <>
        <main className="q-main" aria-label="MCP server detail">
          <div className="q-page">{SkeletonDetail()}</div>
        </main>
        {dialogs}
      </>
    );
  }

  if (!server) {
    return (
      <>
        <main className="q-main" aria-label="MCP server detail">
          <div className="q-page">
            <div className="q-wrap" style={{ maxWidth: 860 }}>
              <ErrorBanner error={error} onRetry={() => void loadMcpServers({ force: true })} />
            </div>
            <EmptyState onAdd={openAdd} onImport={openImport} />
          </div>
        </main>
        {dialogs}
      </>
    );
  }

  const matrix = matrixRows(server).map((row) => {
    const host = hosts[row.hostId];
    const harness = row.harness;
    const machineRow = row.target === "machine";
    const pendingKey = machineRow
      ? `${server.name}:toggle:${row.hostId}`
      : `${server.name}:toggle:${harness}`;
    const enabled = machineRow
      ? (server.enabled.by_machine.find((entry) => entry.machine === row.hostId)?.enabled ??
        row.enabled)
      : row.enabled;
    return {
      ...row,
      hostName: host?.name ?? row.hostId,
      enabled,
      busy: Boolean(pending[pendingKey]),
      onToggle: () => {
        if (machineRow) {
          void setMcpEnabled(server.name, { machine: row.hostId }, !enabled);
        } else {
          void setMcpEnabled(server.name, { harness }, !enabled);
        }
      },
    };
  });

  return (
    <>
      <main className="q-main" aria-label="MCP server detail">
        <div className="q-page">
          <div className="q-wrap" style={{ maxWidth: 860 }}>
            {error ? (
              <ErrorBanner error={error} onRetry={() => void loadMcpServers({ force: true })} />
            ) : null}

            {isPhone ? (
              <button
                type="button"
                className="q-btn ghost mb-3"
                onClick={() => setPhoneOpen(false)}
              >
                <ArrowLeftIcon className="q-icon s14" aria-hidden /> All servers
              </button>
            ) : null}

            <div className="mb-5 flex items-start gap-4">
              <div className="min-w-0 flex-1">
                <h1 className="text-[20px] leading-[26px] font-semibold tracking-[-0.015em]">
                  {server.name}
                </h1>
                <div className="q-muted mt-1 flex flex-wrap items-center gap-1.5">
                  <span>{transportKind(server.transport)}</span>
                  <span>·</span>
                  <span className="q-mono">{transportSummary(server.transport)}</span>
                  <span>·</span>
                  <span>{server.tool_count} tools</span>
                  <span>·</span>
                  <span>{checkedLabel(checkedAt, now)}</span>
                  {refreshing ? <RefreshCwIcon className="q-icon s12 q-faint" aria-hidden /> : null}
                </div>
              </div>
              <Health server={server} />
              <div className="hidden items-center gap-2 sm:flex">
                <Button variant="secondary" onClick={() => setToolsServer(server)}>
                  <RefreshCwIcon className="q-icon s14" aria-hidden /> Test
                </Button>
                <Button
                  variant="secondary"
                  onClick={() => {
                    setEditing(server);
                    setAddOpen(true);
                  }}
                >
                  Edit
                </Button>
              </div>
              <DropdownMenu>
                <DropdownMenuTrigger asChild>
                  <button type="button" className="q-ibtn" aria-label="Server actions">
                    <MoreHorizontalIcon className="q-icon" aria-hidden />
                  </button>
                </DropdownMenuTrigger>
                <DropdownMenuContent align="end" className="w-48">
                  <DropdownMenuItem onSelect={() => setToolsServer(server)}>
                    <WrenchIcon className="q-icon s14" aria-hidden /> List tools and test
                  </DropdownMenuItem>
                  <DropdownMenuItem
                    onSelect={() => {
                      setEditing(server);
                      setAddOpen(true);
                    }}
                  >
                    Edit configuration
                  </DropdownMenuItem>
                  {server.runs_on ? null : (
                    <DropdownMenuItem onSelect={() => setShareServer(server)}>
                      <Share2Icon className="q-icon s14" aria-hidden /> Share…
                    </DropdownMenuItem>
                  )}
                  <DropdownMenuSeparator />
                  <DropdownMenuItem variant="destructive" onSelect={() => setRemoveServer(server)}>
                    <Trash2Icon className="q-icon s14" aria-hidden /> Remove server…
                  </DropdownMenuItem>
                </DropdownMenuContent>
              </DropdownMenu>
            </div>

            {isPhone ? (
              <div className="mb-3 flex gap-2">
                <Button variant="secondary" onClick={() => setToolsServer(server)}>
                  <RefreshCwIcon className="q-icon s14" aria-hidden /> Test
                </Button>
                <Button
                  variant="secondary"
                  onClick={() => {
                    setEditing(server);
                    setAddOpen(true);
                  }}
                >
                  Edit
                </Button>
              </div>
            ) : null}

            <CredentialField server={server} />

            <div className="q-sec" style={{ marginTop: 28 }}>
              <div className="q-sec-h">
                <h2>Where it runs</h2>
                <span className="aside">Each target receives the server its own way</span>
              </div>
              <div style={{ overflowX: "auto" }}>
                <table className="q-tbl">
                  <thead>
                    <tr>
                      <th style={{ width: "24%" }}>Host</th>
                      <th style={{ width: "22%" }}>Harness</th>
                      <th>Delivered as</th>
                      <th>Status</th>
                      <th className="r" style={{ paddingRight: 0 }}>
                        On
                      </th>
                    </tr>
                  </thead>
                  <tbody>
                    {matrix.map((row) => (
                      <tr key={row.key} className="hov">
                        <td>
                          <span className="q-cell-id">
                            <EntityAvatar
                              seed={machineSeed(row.hostId)}
                              kind="machine"
                              label={row.hostName}
                              size={14}
                            />
                            {row.hostName}
                          </span>
                        </td>
                        <td>
                          <span className="q-cell-id">
                            <EntityAvatar
                              seed={harnessSeed(row.hostId, row.harness)}
                              kind="harness"
                              label={harnessName(row.harness)}
                              size={14}
                            />
                            {harnessName(row.harness)}
                          </span>
                        </td>
                        <td className={row.enabled ? "q-muted" : "q-faint"}>{row.delivery}</td>
                        <td>
                          {row.note === "Off" ? (
                            <span className="q-faint">Off</span>
                          ) : (
                            <span
                              className={cn(
                                "q-health",
                                row.tone === "ok" && "q-ok",
                                row.tone === "warn" && "q-warn",
                                row.tone === "off" && "q-faint",
                              )}
                            >
                              {row.tone === "warn" ? (
                                <TriangleAlertIcon className="q-icon s12" aria-hidden />
                              ) : row.tone === "ok" ? (
                                <CheckCircle2Icon className="q-icon s12" aria-hidden />
                              ) : null}
                              {row.note}
                            </span>
                          )}
                        </td>
                        <td className="r" style={{ paddingRight: 0 }}>
                          <Toggle
                            checked={row.enabled}
                            busy={row.busy}
                            label={`${row.enabled ? "Turn off" : "Turn on"} ${server.name} for ${harnessName(row.harness)} on ${row.hostName}`}
                            onToggle={row.onToggle}
                          />
                        </td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
              <ConflictCard server={server} />
            </div>

            <SharedWithYou current={server} />
            <SharedFromHere server={server} />
            {server.runs_on ? null : <Activity server={server} />}

            <div className="q-sec">
              <div className="q-sec-h">
                <h2>Configuration</h2>
                <span className="aside">
                  {server.imported_from
                    ? `Imported from ${server.imported_from}`
                    : "Managed by h4rn"}
                </span>
              </div>
              <div className="q-field">
                <div className="lab">
                  <div className="t">Transport</div>
                  <div className="s">How the hub starts or reaches it</div>
                </div>
                <div className="ctl">
                  <span className="q-mono" style={{ fontSize: 12 }}>
                    {transportSummary(server.transport)}
                  </span>
                  {server.transport.type === "stdio" && server.transport.cwd ? (
                    <span className="q-faint" style={{ fontSize: 12 }}>
                      cwd {server.transport.cwd}
                    </span>
                  ) : null}
                </div>
              </div>
              {server.last_error && !conflictText(server) ? (
                <div className="q-banner err" style={{ margin: "12px 0 0" }}>
                  <span className="ic">
                    <TriangleAlertIcon className="q-icon s14" aria-hidden />
                  </span>
                  <span className="grow">{server.last_error}</span>
                </div>
              ) : null}
            </div>
          </div>
        </div>
      </main>
      {dialogs}
    </>
  );
}
