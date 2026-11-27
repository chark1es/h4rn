import {
  ArrowLeftIcon,
  CheckCircle2Icon,
  ChevronRightIcon,
  KeyRoundIcon,
  MoreHorizontalIcon,
  PencilIcon,
  PlugIcon,
  PlusIcon,
  RefreshCwIcon,
  Share2Icon,
  Trash2Icon,
  TriangleAlertIcon,
  UploadIcon,
  WrenchIcon,
} from "lucide-react";
import { useEffect, useMemo, useState } from "react";
import { useTranslation } from "react-i18next";

import { EntityAvatar } from "@/components/agent/EntityAvatar";
import { Popover } from "@/components/common/Popover";
import { LibraryErrorBanner } from "@/components/pages/LibraryErrorBanner";
import { PhoneLargeTitle, PhoneNavButton } from "@/components/shell/PhoneNav";
import { Button } from "@/components/ui/button";
import { Menu, MenuContent, MenuItem, MenuSeparator, MenuTrigger } from "@/components/ui/menu";
import type { HarnessKind, HostId, McpServerView } from "@/gen";
import { useIsPhone } from "@/hooks/use-media-query";
import { useNow } from "@/hooks/use-now";
import { HARNESS_LABELS, harnessSeed, machineSeed } from "@/lib/entities";
import { formatAge, formatClockUnit, formatList, formatRelativeTime, i18n } from "@/lib/i18n";
import { setLibrarySelected, useApp } from "@/lib/store/appStore";
import { cn } from "@/lib/utils";
import {
  CONFLICT_CHOICES,
  type ConflictChoice,
  callerLabel,
  conflictResolution,
  conflictText,
  deliveryLabel,
  enabledFor,
  harnessName,
  MCP_HARNESSES,
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
const BANNER_STYLE = { margin: "0 0 16px" };

/** A conflict choice's name; the import dialog uses the same words. */
const CHOICE_LABELS = {
  keep_mine: "dialogs:mcp.import.keepMine",
  use_theirs: "dialogs:mcp.import.useTheirs",
  rename: "dialogs:mcp.import.renameTheirs",
  skip: "dialogs:mcp.import.skip",
} as const satisfies Record<ConflictChoice, string>;
/** Machine-wide rows are not harness specific; show them under the first harness. */
const PRIMARY_HARNESS: HarnessKind = MCP_HARNESSES[0] ?? "claude_code";

function checkedLabel(checkedAt: number | null, now: number): string {
  if (!checkedAt) return i18n.t("library:mcp.checked.never");
  if (now - checkedAt < 60_000) return i18n.t("library:mcp.checked.justNow");
  return i18n.t("library:mcp.checked.ago", { when: formatRelativeTime(checkedAt, now) });
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
      delivery: deliveryLabel(harness),
      enabled,
      note: enabled
        ? server.tool_count > 0
          ? i18n.t("library:mcp.tools", { count: server.tool_count })
          : i18n.t("library:mcp.where.ready")
        : i18n.t("common:off"),
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
      delivery: i18n.t("library:mcp.delivery.proxied"),
      enabled,
      note: !enabled
        ? i18n.t("common:off")
        : conflict && index === 0
          ? i18n.t("library:mcp.where.nameConflict")
          : i18n.t("library:mcp.where.canCall"),
      tone: !enabled ? "off" : conflict && index === 0 ? "warn" : "ok",
      busy: false,
      onToggle: () => undefined,
    });
  });
  return rows;
}

function ConflictCard({ server }: { server: McpServerView }) {
  const { t } = useTranslation(["library", "common"]);
  const text = conflictText(server);
  if (!text) return null;
  return (
    <div className="q-conflict">
      <TriangleAlertIcon className="q-icon q-warn" aria-hidden />
      <div className="grow">
        <div className="t">{text}</div>
        <div className="q-muted text-xs mt-0.5">{t("mcp.conflict.body")}</div>
      </div>
      <Popover
        label={t("mcp.conflict.label", { name: server.name })}
        align="end"
        width={340}
        trigger={() => t("mcp.conflict.trigger")}
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
  const { t } = useTranslation(["library", "dialogs", "common"]);
  const [choice, setChoice] = useState<ConflictChoice>("keep_mine");
  const [rename, setRename] = useState("");
  const [busy, setBusy] = useState(false);
  const target = conflictResolution(choice, rename);
  // The conflict names its harness; only an unattributed one asks which.
  const locked = MCP_HARNESSES.includes(server.imported_from as HarnessKind);

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
      <div className="font-semibold mb-0.5">{t("mcp.resolve.title", { name: server.name })}</div>
      <div className="q-muted text-xs mb-2.5">
        {t("mcp.resolve.choose", { harness: HARNESS_LABELS[harness] })}
      </div>
      {locked ? (
        <div className="q-faint" style={{ fontSize: 12, marginBottom: 8 }}>
          {t("mcp.resolve.otherCopy", { harness: HARNESS_LABELS[harness] })}
        </div>
      ) : (
        <div
          className="q-seg q-resolve-seg"
          role="radiogroup"
          aria-label={t("mcp.resolve.harness")}
        >
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
      )}
      <div role="radiogroup" aria-label={t("mcp.resolve.resolution")}>
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
              <span className="font-medium">{t(CHOICE_LABELS[option.value])}</span>
              <span className="q-faint sub">{t(`mcp.resolve.hints.${option.value}`)}</span>
            </span>
          </label>
        ))}
      </div>
      {choice === "rename" ? (
        <input
          className="q-input mt-2"
          value={rename}
          onChange={(event) => setRename(event.target.value)}
          placeholder={t("mcp.resolve.newName")}
          aria-label={t("dialogs:mcp.import.newNameFor", { name: server.name })}
        />
      ) : null}
      <div className="flex justify-end gap-1.5 mt-3">
        <button type="button" className="q-btn ghost" onClick={close}>
          {t("common:cancel")}
        </button>
        <button type="submit" className="q-btn primary" disabled={busy || !target}>
          {busy ? t("mcp.resolve.resolving") : t("mcp.resolve.resolve")}
        </button>
      </div>
    </form>
  );
}

function CredentialField({ server }: { server: McpServerView }) {
  const { t } = useTranslation(["library", "common"]);
  const hosts = useApp((state) => state.hosts);
  const local = useApp((state) => state.localHostId);
  const keys = secretNames(server.transport);
  const hostName = local ? (hosts[local]?.name ?? local) : t("mcp.credential.thisHost");
  const env = server.transport.type === "stdio";
  const remoteHost = server.runs_on ? (hosts[server.runs_on]?.name ?? server.runs_on) : null;
  return (
    <div className="q-field">
      <div className="lab">
        <div className="t">{t("mcp.credential.title")}</div>
        <div className="s">{t("mcp.credential.hint")}</div>
      </div>
      <div className="ctl">
        {keys.length > 0 ? (
          <>
            <span className="q-secret">
              <KeyRoundIcon className="q-icon s12" aria-hidden />{" "}
              {t("mcp.credential.storedOn", { host: remoteHost ?? hostName })}
            </span>
            <span className="q-faint" style={{ fontSize: 12 }}>
              {t(env ? "mcp.credential.env" : "mcp.credential.headers", { count: keys.length })}
            </span>
          </>
        ) : (
          <span className="q-faint" style={{ fontSize: 12 }}>
            {t(env ? "mcp.credential.noEnv" : "mcp.credential.noHeaders")}
          </span>
        )}
        {server.shared_to.length > 0 ? (
          <span className="q-secret">
            <Share2Icon className="q-icon s12" aria-hidden />{" "}
            {t("mcp.credential.sharedWith", {
              hosts: formatList(server.shared_to.map((machine) => hosts[machine]?.name ?? machine)),
            })}
          </span>
        ) : null}
      </div>
    </div>
  );
}

function SharedWithYou({ current }: { current: McpServerView | null }) {
  const { t } = useTranslation(["library", "common"]);
  const servers = useMcp((state) => state.servers);
  const hosts = useApp((state) => state.hosts);
  const shared = servers.filter((server) => Boolean(server.runs_on));
  if (shared.length === 0) return null;
  return (
    <div className="q-sec">
      <div className="q-sec-h">
        <h2>{t("mcp.sharedWithYou.title")}</h2>
        <span className="aside">{t("mcp.sharedWithYou.aside")}</span>
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
                  <span className="q-faint font-normal">
                    {t("mcp.sharedWithYou.from", { host: host?.name ?? machine })}
                  </span>
                </div>
                <div className="s">
                  {transportKind(server.transport)} ·{" "}
                  <span className="q-mono">{transportSummary(server.transport)}</span> ·{" "}
                  {t("mcp.sharedWithYou.runsOn", { host: host?.name ?? machine })}
                </div>
              </button>
              <span className="q-secret">
                <KeyRoundIcon className="q-icon s12" aria-hidden />{" "}
                {t("mcp.credential.storedOn", { host: host?.name ?? machine })}
              </span>
              <Toggle
                checked={enabled}
                label={t("mcp.sharedWithYou.use", { name: server.name })}
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
  const { t } = useTranslation(["library", "common"]);
  const hosts = useApp((state) => state.hosts);
  if (server.shared_to.length === 0) return null;
  return (
    <div className="q-sec">
      <div className="q-sec-h">
        <h2>{t("mcp.sharedFromHere.title")}</h2>
        <span className="aside">{t("mcp.sharedFromHere.aside")}</span>
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
              <div className="s">{t("mcp.sharedFromHere.proxies")}</div>
            </div>
            <Button
              variant="ghost"
              size="sm"
              onClick={() => void unshareMcpServer(server.name, machine)}
            >
              {t("mcp.sharedFromHere.unshare")}
            </Button>
          </div>
        ))}
      </div>
    </div>
  );
}

function Activity({ server }: { server: McpServerView }) {
  const { t } = useTranslation(["library", "common"]);
  const activity = useMcp((state) => state.activity);
  const now = useNow();
  const forServer = activity.filter((entry) => entry.server === server.name);
  if (forServer.length === 0) return null;
  return (
    <div className="q-sec">
      <div className="q-sec-h">
        <h2>{t("mcp.activity.title")}</h2>
        <span className="aside">{t("mcp.activity.aside")}</span>
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
              <div className="t q-mono" style={{ fontSize: 13 }}>
                {entry.qualifiedName}
              </div>
              <div className="s">
                {callerLabel(entry.caller)} ·{" "}
                {formatClockUnit(Number(entry.durationMs), "millisecond")}
                {entry.error ? ` · ${entry.error}` : ""}
              </div>
            </div>
            <span className="q-faint q-num" style={{ fontSize: 12 }}>
              {formatAge(entry.at, now)}
            </span>
          </div>
        ))}
      </div>
    </div>
  );
}

function SkeletonDetail({ label }: { label: string }) {
  return (
    <div className="q-form wide" role="status" aria-busy="true" aria-label={label}>
      <div className="q-skel w-45 h-5.5 mb-2" />
      <div className="q-skel w-105 h-3 mb-7" />
      <div className="q-skel w-full h-12 mb-2" />
      <div className="q-skel w-full h-42" />
    </div>
  );
}

function EmptyState({ onAdd, onImport }: { onAdd: () => void; onImport: () => void }) {
  const { t } = useTranslation(["library", "dialogs", "common"]);
  return (
    <div className="q-center-empty">
      <div className="q-empty q-box" style={{ alignItems: "flex-start", padding: 0 }}>
        <h3>{t("mcp.empty.title")}</h3>
        <p>{t("mcp.empty.body")}</p>
        <div className="flex gap-2">
          <Button onClick={onAdd}>
            <PlusIcon className="q-icon s14" aria-hidden /> {t("dialogs:mcp.server.add")}
          </Button>
          <Button variant="secondary" onClick={onImport}>
            <UploadIcon className="q-icon s14" aria-hidden /> {t("dialogs:mcp.import.title")}
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
  const errorAction = useMcp((state) => state.errorAction);
  const isPhone = useIsPhone();
  const { t } = useTranslation(["library", "settings", "common"]);
  return (
    <main className="q-main" aria-label={t("settings:nav.mcpServers")}>
      {isPhone ? (
        <PhoneLargeTitle
          title={t("settings:nav.mcpServers")}
          summary={t("mcp.summary", { count: servers.length })}
          actions={
            <>
              <PhoneNavButton label={t("mcp.import")} onClick={onImport}>
                <UploadIcon className="q-icon s22" strokeWidth={1.5} aria-hidden />
              </PhoneNavButton>
              <PhoneNavButton label={t("settings:nav.addMcp")} onClick={onAdd}>
                <PlusIcon className="q-icon s22" strokeWidth={1.5} aria-hidden />
              </PhoneNavButton>
            </>
          }
        />
      ) : null}
      <div className="q-page">
        <div className="q-form wide">
          {isPhone ? null : (
            <div className="mb-5 flex flex-wrap items-start gap-3">
              <div className="grow basis-0">
                <h1 className="text-xl font-semibold">{t("settings:nav.mcpServers")}</h1>
                <div className="q-muted mt-1">{t("mcp.summary", { count: servers.length })}</div>
              </div>
              <Button variant="secondary" onClick={onImport}>
                <UploadIcon className="q-icon s14" aria-hidden /> {t("mcp.import")}
              </Button>
              <Button onClick={onAdd}>
                <PlusIcon className="q-icon s14" aria-hidden /> {t("common:add")}
              </Button>
            </div>
          )}

          <LibraryErrorBanner
            error={error}
            action={errorAction}
            style={BANNER_STYLE}
            onRetry={() => void loadMcpServers({ force: true })}
          />

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
                      <PlugIcon className="q-icon" strokeWidth={1.5} aria-hidden />
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
  const errorAction = useMcp((state) => state.errorAction);
  const pending = useMcp((state) => state.pending);
  const checkedAt = useMcp((state) => state.checkedAt);
  const selected = useApp((state) => state.ui.layout.librarySelected);
  const hosts = useApp((state) => state.hosts);
  const daemonHost = useApp((state) => state.localHostId);
  const now = useNow();
  const { t } = useTranslation(["library", "common"]);
  const errorBanner = (
    <LibraryErrorBanner
      error={error}
      action={errorAction}
      style={BANNER_STYLE}
      onRetry={() => void loadMcpServers({ force: true })}
    />
  );

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
        <main className="q-main" aria-label={t("mcp.detail")}>
          <div className="q-page">
            <SkeletonDetail label={t("mcp.loading")} />
          </div>
        </main>
        {dialogs}
      </>
    );
  }

  if (!server) {
    return (
      <>
        <main className="q-main" aria-label={t("mcp.detail")}>
          <div className="q-page">
            <div className="q-form wide">{errorBanner}</div>
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
      <main className="q-main" aria-label={t("mcp.detail")}>
        <div className="q-page">
          <div className="q-form wide">
            {errorBanner}

            {isPhone ? (
              <button
                type="button"
                className="q-btn ghost mb-3"
                onClick={() => setPhoneOpen(false)}
              >
                <ArrowLeftIcon className="q-icon s14" aria-hidden /> {t("mcp.allServers")}
              </button>
            ) : null}

            <div className="mb-5 flex items-start gap-4">
              <div className="min-w-0 flex-1">
                <h1 className="text-xl font-semibold">{server.name}</h1>
                <div className="q-muted mt-1 flex flex-wrap items-center gap-1.5">
                  <span>{transportKind(server.transport)}</span>
                  <span>·</span>
                  <span className="q-mono">{transportSummary(server.transport)}</span>
                  <span>·</span>
                  <span>{t("mcp.tools", { count: server.tool_count })}</span>
                  <span>·</span>
                  <span>{checkedLabel(checkedAt, now)}</span>
                  {refreshing ? <RefreshCwIcon className="q-icon s12 q-faint" aria-hidden /> : null}
                </div>
              </div>
              <Health server={server} />
              <div className="hidden items-center gap-2 sm:flex">
                <Button variant="secondary" onClick={() => setToolsServer(server)}>
                  <RefreshCwIcon className="q-icon s14" aria-hidden /> {t("mcp.test")}
                </Button>
                <Button
                  variant="secondary"
                  onClick={() => {
                    setEditing(server);
                    setAddOpen(true);
                  }}
                >
                  {t("common:edit")}
                </Button>
              </div>
              <Menu>
                <MenuTrigger className="q-ibtn" aria-label={t("mcp.actions")}>
                  <MoreHorizontalIcon className="q-icon" aria-hidden />
                </MenuTrigger>
                <MenuContent align="end" className="w-48">
                  <MenuItem icon={<WrenchIcon />} onSelect={() => setToolsServer(server)}>
                    {t("mcp.menu.tools")}
                  </MenuItem>
                  <MenuItem
                    icon={<PencilIcon />}
                    onSelect={() => {
                      setEditing(server);
                      setAddOpen(true);
                    }}
                  >
                    {t("mcp.menu.edit")}
                  </MenuItem>
                  {server.runs_on ? null : (
                    <MenuItem icon={<Share2Icon />} onSelect={() => setShareServer(server)}>
                      {t("mcp.menu.share")}
                    </MenuItem>
                  )}
                  <MenuSeparator />
                  <MenuItem
                    icon={<Trash2Icon />}
                    destructive
                    onSelect={() => setRemoveServer(server)}
                  >
                    {t("mcp.menu.remove")}
                  </MenuItem>
                </MenuContent>
              </Menu>
            </div>

            {isPhone ? (
              <div className="mb-3 flex gap-2">
                <Button variant="secondary" onClick={() => setToolsServer(server)}>
                  <RefreshCwIcon className="q-icon s14" aria-hidden /> {t("mcp.test")}
                </Button>
                <Button
                  variant="secondary"
                  onClick={() => {
                    setEditing(server);
                    setAddOpen(true);
                  }}
                >
                  {t("common:edit")}
                </Button>
              </div>
            ) : null}

            <CredentialField server={server} />

            <div className="q-sec mt-7">
              <div className="q-sec-h">
                <h2>{t("mcp.where.title")}</h2>
                <span className="aside">{t("mcp.where.aside")}</span>
              </div>
              <div style={{ overflowX: "auto" }}>
                <table className="q-tbl">
                  <thead>
                    <tr>
                      <th style={{ width: "24%" }}>{t("mcp.where.host")}</th>
                      <th style={{ width: "22%" }}>{t("mcp.where.harness")}</th>
                      <th>{t("mcp.where.deliveredAs")}</th>
                      <th>{t("mcp.where.status")}</th>
                      <th className="r pr-0">{t("common:on")}</th>
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
                          {row.tone === "off" ? (
                            <span className="q-faint">{row.note}</span>
                          ) : (
                            <span
                              className={cn(
                                "q-health",
                                row.tone === "ok" && "q-ok",
                                row.tone === "warn" && "q-warn",
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
                        <td className="r pr-0">
                          <Toggle
                            checked={row.enabled}
                            busy={row.busy}
                            label={t(row.enabled ? "mcp.where.turnOff" : "mcp.where.turnOn", {
                              name: server.name,
                              harness: harnessName(row.harness),
                              host: row.hostName,
                            })}
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
                <h2>{t("mcp.config.title")}</h2>
                <span className="aside">
                  {server.imported_from
                    ? t("mcp.config.importedFrom", { source: server.imported_from })
                    : t("mcp.config.managed")}
                </span>
              </div>
              <div className="q-field">
                <div className="lab">
                  <div className="t">{t("mcp.config.transport")}</div>
                  <div className="s">{t("mcp.config.transportHint")}</div>
                </div>
                <div className="ctl">
                  <span className="q-mono" style={{ fontSize: 12 }}>
                    {transportSummary(server.transport)}
                  </span>
                  {server.transport.type === "stdio" && server.transport.cwd ? (
                    <span className="q-faint" style={{ fontSize: 12 }}>
                      {t("mcp.config.cwd", { path: server.transport.cwd })}
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
