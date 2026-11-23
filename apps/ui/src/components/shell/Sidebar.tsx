import type { LucideIcon } from "lucide-react";
import {
  BlocksIcon,
  ChartNoAxesColumnIcon,
  CheckIcon,
  ChevronDownIcon,
  ChevronRightIcon,
  CircleAlertIcon,
  CircleCheckIcon,
  CircleDotIcon,
  CircleIcon,
  CircleXIcon,
  ClockIcon,
  FolderPlusIcon,
  GitPullRequestIcon,
  KeyRoundIcon,
  PanelLeftIcon,
  PlugIcon,
  QrCodeIcon,
  SearchIcon,
  ServerIcon,
  SettingsIcon,
  SquarePenIcon,
  WifiOffIcon,
} from "lucide-react";
import { useEffect, useMemo, useState } from "react";

import { AgentGlyph } from "@/components/agent/AgentGlyph";
import { EntityAvatar } from "@/components/agent/EntityAvatar";
import { ShortcutCaps } from "@/components/common/Kbd";
import {
  harnessUpdateCount,
  useHarnesses,
  useHarnessVersions,
} from "@/features/harnesses/harnessStore";
import { serverStatus } from "@/features/mcp/labels";
import { loadMcpServers, pendingMcpApprovals, useMcp } from "@/features/mcp/store";
import {
  ProjectGroupSection,
  ProjectsHeader,
  ProjectsTail,
  useProjectDrag,
} from "@/features/projects/SidebarProjects";
import { loadProjects, sidebarProjects, useProjects } from "@/features/projects/store";
import { UseThisMachineButton } from "@/features/remote/LocalMachineAction";
import {
  activateMachine,
  forgetMachine,
  shellKind,
  useRemote,
} from "@/features/remote/remoteStore";
import { loadSkills, useSkills } from "@/features/skills/store";
import type { ThreadSummary, ThreadTab } from "@/gen";
import { useNow } from "@/hooks/use-now";
import { harnessLabel, machineSeed } from "@/lib/entities";
import { absoluteTime, relativeAge } from "@/lib/format";

import {
  isSnoozed,
  needsAttention,
  snoozedThreads,
  TAB_LABELS,
  TAB_ORDER,
  tabCounts,
  threadsForTab,
} from "@/lib/inbox";
import { type CommandId, titleWithShortcut } from "@/lib/keybindings";
import { SETTINGS_SECTIONS, settingsPath } from "@/lib/settings";
import {
  loadThreads,
  navigate,
  openAddProject,
  openNewThread,
  openThread,
  setLibrarySelected,
  setSidebarWidth,
  setTab,
  setUi,
  toggleSidebar,
  useApp,
} from "@/lib/store/appStore";
import { cn } from "@/lib/utils";
import { hostStatus } from "./hostStatus";
import { Resizer } from "./Resizer";

const SKELETON_WIDTHS: Array<[string, string]> = [
  ["78%", "42%"],
  ["62%", "54%"],
  ["88%", "38%"],
  ["71%", "48%"],
  ["58%", "56%"],
  ["83%", "44%"],
  ["66%", "36%"],
];

function ThreadRowSkeleton({ index = 0 }: { index?: number }) {
  const [title, meta] =
    SKELETON_WIDTHS[index % SKELETON_WIDTHS.length] ?? (["78%", "42%"] as const);
  return (
    <div className="q-row" style={{ pointerEvents: "none" }}>
      <span className="q-row-st">
        <span className="q-skel" style={{ width: 10, height: 10, borderRadius: "50%" }} />
      </span>
      <span className="q-row-main" style={{ gap: 8, paddingTop: 2 }}>
        <span className="q-skel" style={{ height: 11, width: title }} />
        <span className="q-skel" style={{ height: 9, width: meta }} />
      </span>
    </div>
  );
}

function FilterGroupSkeleton() {
  return (
    <div role="status" aria-busy="true" aria-label="Loading projects" className="q-side-skel">
      {[64, 48, 72].map((width, group) => (
        <section className="q-pg" key={width}>
          <div className="q-pg-h" style={{ pointerEvents: "none" }}>
            <span className="q-pg-tog">
              <span className="q-skel" style={{ width: 16, height: 16, borderRadius: 4 }} />
              <span className="q-skel" style={{ height: 10, width }} />
            </span>
          </div>
          <div className="q-rows">
            {Array.from({ length: group === 2 ? 1 : 2 }, (_, row) => (
              <ThreadRowSkeleton
                key={`${width}-${group === 2 ? "only" : row === 0 ? "first" : "second"}`}
                index={group + row}
              />
            ))}
          </div>
        </section>
      ))}
    </div>
  );
}

/** The prototype lists WSL distros under their Windows host. */
function parentHostId(hostId: string, wsl: boolean): string | null {
  if (!wsl) return null;
  const prefix = hostId.split("-")[0];
  return prefix && prefix !== hostId ? prefix : null;
}

type HostRow = {
  id: string;
  name: string;
  online: boolean;
  lastSeen: string | null;
  sub: boolean;
  harnesses: string;
  latency: string | null;
  /** Path the session uses, when this row is the active machine. */
  path: string | null;
  /** Connect machine id, when this row can be switched to. */
  machineId: string | null;
  active: boolean;
};

function pathLabel(transport: string): string {
  if (transport === "tailscale") return "Tailscale";
  if (transport === "relay") return "Relay";
  return "Direct";
}

function useHostRows(): HostRow[] {
  const hosts = useApp((state) => state.hosts);
  const connection = useApp((state) => state.connection);
  const machines = useRemote((state) => state.machines);
  const activeMachineId = useRemote((state) => state.activeMachineId);
  const signedIn = useRemote((state) => state.user !== null);
  const now = useNow();
  return useMemo(() => {
    if (signedIn && machines.length > 0) {
      // A connect machine is addressable per host: a Windows box and each of
      // its WSL distros are separate rows. While a machine is active the live
      // transport decides presence and RTT; otherwise connect's presence does.
      const localByName = new Map(
        Object.values(hosts).map((host) => [host.name.toLowerCase(), host]),
      );
      const rows: HostRow[] = [];
      for (const machine of machines) {
        const active = machine.id === activeMachineId;
        const machineHosts =
          machine.hosts.length > 0
            ? machine.hosts
            : [{ id: machine.id, name: machine.name, kind: "host" as const, distro: null }];
        for (const host of machineHosts) {
          const local = localByName.get(host.name.toLowerCase());
          const online = active ? connection.status === "online" : machine.online;
          const latency = active
            ? (local?.latency_ms ?? connection.latencyMs)
            : (local?.latency_ms ?? null);
          const path = online ? (active ? connection.transport : (local?.path ?? null)) : null;
          rows.push({
            id: host.id,
            name: host.name,
            online,
            lastSeen: online
              ? null
              : machine.last_seen
                ? relativeAge(machine.last_seen, now)
                : null,
            sub: host.kind === "wsl",
            harnesses: (local?.harnesses ?? []).map(harnessLabel).join(", "),
            latency:
              online && latency !== null && latency !== undefined
                ? `${Math.round(latency)} ms`
                : null,
            path: path ? pathLabel(path) : null,
            machineId: machine.id,
            active,
          });
        }
      }
      return rows;
    }
    const list = Object.values(hosts);
    const roots = list
      .filter((host) => !host.platform.startsWith("wsl:"))
      .sort((a, b) => Number(b.online) - Number(a.online) || a.name.localeCompare(b.name));
    const children = list.filter((host) => host.platform.startsWith("wsl:"));
    const rows: HostRow[] = [];
    const push = (host: (typeof list)[number], sub: boolean) => {
      rows.push({
        id: host.host_id,
        name: host.name,
        online: host.online,
        lastSeen: host.online ? null : relativeAge(host.last_seen ?? "", now),
        sub,
        harnesses: host.harnesses.map(harnessLabel).join(", "),
        latency:
          host.latency_ms !== null && host.latency_ms !== undefined
            ? `${Math.round(host.latency_ms)} ms`
            : connection.status === "online" && connection.latencyMs !== null && rows.length === 0
              ? `${Math.round(connection.latencyMs)} ms`
              : null,
        path: host.online
          ? host.path
            ? pathLabel(host.path)
            : pathLabel(connection.transport)
          : null,
        machineId: null,
        active: rows.length === 0 && host.online && connection.status === "online",
      });
    };
    for (const host of roots) {
      push(host, false);
      for (const child of children) {
        if (parentHostId(child.host_id, true) === host.host_id) push(child, true);
      }
    }
    for (const child of children) {
      if (!rows.some((row) => row.id === child.host_id)) push(child, true);
    }
    return rows;
  }, [hosts, machines, activeMachineId, signedIn, connection, now]);
}

function HostsPopover({
  rows,
  connection,
  onClose,
}: {
  rows: HostRow[];
  connection: { status: string; latencyMs: number | null; transport: string };
  onClose: () => void;
}) {
  const user = useRemote((state) => state.user);
  const busy = useRemote((state) => state.busy);
  const stored = useRemote((state) => state.active);
  const mobile = shellKind() === "mobile";
  const online = rows.filter((row) => row.online).length;
  const active = rows.find((row) => row.active) ?? rows.find((row) => row.online);
  // The daemon answers but this host does not: that is an unreachable host,
  // shown in `danger` with its last-seen time (DESIGN §States).
  const reachableLink = connection.status === "online";
  const dots = (row: HostRow) =>
    row.online ? (
      <span className="q-dot on" role="img" aria-label="online" />
    ) : (
      <span
        className={cn("q-dot off", reachableLink && "bad")}
        role="img"
        aria-label={reachableLink ? "unreachable" : "offline"}
      />
    );
  const connectionLine =
    connection.status === "online"
      ? `${pathLabel(connection.transport)} to ${active?.name ?? "local"}${
          connection.latencyMs !== null ? ` · ${Math.round(connection.latencyMs)} ms` : ""
        } · encrypted`
      : connection.status === "connecting"
        ? "Connecting…"
        : "Offline";
  return (
    <div className="q-hostpop" role="menu" aria-label="Hosts">
      <div className="q-hp-h">
        <span>Hosts</span>
        <span className="q-faint q-num">
          {online} of {rows.length} online
        </span>
      </div>
      {rows.length === 0 ? (
        <div className="q-hp-empty">
          {stored && connection.status === "online" ? (
            <>
              Connected to <b>{stored.machineName}</b>. Its host list isn’t available right now.
            </>
          ) : stored && connection.status === "connecting" ? (
            <>
              Connecting to <b>{stored.machineName}</b>…
            </>
          ) : stored ? (
            <>
              <b>{stored.machineName}</b> is unreachable.{" "}
              {mobile ? "Disconnect to stop using it." : "Use this machine to keep working."}
            </>
          ) : (
            <>
              No hosts yet. Run <span className="q-mono">h4rn host</span> on a machine.
            </>
          )}
        </div>
      ) : (
        rows.map((row) => (
          <button
            key={row.id}
            type="button"
            className={cn("q-host", row.sub && "sub")}
            role="menuitem"
            title={row.harnesses}
            aria-current={row.active ? "true" : undefined}
            disabled={row.machineId !== null && busy !== null}
            onClick={() => {
              if (row.machineId) {
                void activateMachine(row.machineId).then((ok) => {
                  if (ok) onClose();
                });
                return;
              }
              navigate({ kind: "placeholder", surface: "settings", path: settingsPath("hosts") });
              onClose();
            }}
          >
            <EntityAvatar
              seed={machineSeed(row.id)}
              kind="machine"
              label={row.name}
              size={16}
              decorative={false}
            />
            <span
              className="lbl"
              style={
                row.online ? undefined : { color: reachableLink ? "var(--danger)" : "var(--fg-3)" }
              }
            >
              {row.name}
            </span>
            {row.online ? (
              <span className="meta q-num">
                {row.path ? `${row.path}${row.latency ? ` · ${row.latency}` : ""}` : "Online"}
              </span>
            ) : (
              <span
                className="meta"
                style={{
                  display: "inline-flex",
                  gap: 4,
                  alignItems: "center",
                  color: reachableLink ? "var(--danger)" : undefined,
                }}
              >
                <WifiOffIcon className="q-icon s12" aria-hidden />
                {row.lastSeen}
              </span>
            )}
            {dots(row)}
            {row.active && !row.sub ? (
              <CheckIcon className="q-icon s12 q-host-on" strokeWidth={2} aria-label="Active" />
            ) : null}
          </button>
        ))
      )}
      <div className="q-hp-conn">{connectionLine}</div>
      {stored ? (
        <button
          type="button"
          className="q-nav"
          role="menuitem"
          onClick={() => {
            forgetMachine();
            onClose();
          }}
        >
          <WifiOffIcon className="q-icon" strokeWidth={1.5} aria-hidden />
          <span className="lbl">Disconnect from {stored.machineName}</span>
        </button>
      ) : null}
      <div className="q-hp-sep" />
      {user ? null : (
        <button
          type="button"
          className="q-nav"
          role="menuitem"
          onClick={() => {
            navigate({ kind: "placeholder", surface: "settings", path: settingsPath("devices") });
            onClose();
          }}
        >
          <ServerIcon className="q-icon" strokeWidth={1.5} aria-hidden />
          <span className="lbl">Sign in to h4rn-connect…</span>
        </button>
      )}
      <button
        type="button"
        className="q-nav"
        role="menuitem"
        onClick={() => {
          navigate({
            kind: "placeholder",
            surface: "settings",
            path: settingsPath("devices"),
          });
          onClose();
        }}
      >
        <QrCodeIcon className="q-icon" strokeWidth={1.5} aria-hidden />
        <span className="lbl">Pair a device…</span>
      </button>
      <button
        type="button"
        className="q-nav"
        role="menuitem"
        onClick={() => {
          navigate({ kind: "placeholder", surface: "settings", path: settingsPath("hosts") });
          onClose();
        }}
      >
        <ServerIcon className="q-icon" strokeWidth={1.5} aria-hidden />
        <span className="lbl">Manage hosts</span>
      </button>
    </div>
  );
}

const NO_SKILLS: never[] = [];

/** MCP servers and skills live under Settings, so the library has no button of its own. */
type Destination = "pr" | "issues" | "usage" | "settings";

const DESTINATIONS: Array<{
  key: Destination;
  label: string;
  icon: LucideIcon;
  path: string;
  command: CommandId;
}> = [
  {
    key: "pr",
    label: "Pull requests",
    icon: GitPullRequestIcon,
    path: "/pulls",
    command: "nav.pulls",
  },
  {
    key: "issues",
    label: "Issues",
    icon: CircleDotIcon,
    path: "/issues",
    command: "nav.issues",
  },
  {
    key: "usage",
    label: "Usage",
    icon: ChartNoAxesColumnIcon,
    path: "/usage",
    command: "nav.usage",
  },
  {
    key: "settings",
    label: "Settings",
    icon: SettingsIcon,
    path: settingsPath("general"),
    command: "settings.open",
  },
];

function SidebarFooter({
  destination,
  loading,
  offline,
  rows,
  connection,
}: {
  destination: Destination | null;
  loading: boolean;
  offline: boolean;
  rows: HostRow[];
  connection: { status: string; latencyMs: number | null; transport: string };
}) {
  const [popoverOpen, setPopoverOpen] = useState(false);
  const hostLine = hostStatus({ loading, offline, rows });

  return (
    <>
      <div className="q-side-ft">
        <button
          type="button"
          className={cn("q-hostbtn", popoverOpen && "on")}
          aria-haspopup="menu"
          aria-expanded={popoverOpen}
          title="Hosts and connection"
          onClick={() => setPopoverOpen((value) => !value)}
        >
          {hostLine.icon === "orb" ? (
            <AgentGlyph
              glyph={{ kind: "orb", state: "breathing", speed: 0.35, label: "Connecting" }}
              size={14}
            />
          ) : hostLine.icon === "off" ? (
            <span className="q-faint" style={{ display: "inline-flex" }}>
              <WifiOffIcon className="q-icon s14" aria-hidden />
            </span>
          ) : (
            <span className="q-dot on" />
          )}
          <span>{hostLine.text}</span>
          <ChevronDownIcon className="q-icon s12 q-faint" strokeWidth={1.5} aria-hidden />
        </button>
        {DESTINATIONS.map((item) => (
          <button
            key={item.key}
            type="button"
            className={cn("q-ibtn q-side-dest", destination === item.key && "on")}
            aria-current={destination === item.key ? "page" : undefined}
            aria-label={item.label}
            title={titleWithShortcut(item.label, item.command)}
            onClick={() =>
              navigate({
                kind: "placeholder",
                surface:
                  item.key === "pr"
                    ? "pulls"
                    : item.key === "issues"
                      ? "issues"
                      : item.key === "usage"
                        ? "usage"
                        : "settings",
                path: item.path,
              })
            }
          >
            <item.icon className="q-icon" strokeWidth={1.5} aria-hidden />
          </button>
        ))}
      </div>
      {popoverOpen ? (
        <HostsPopover rows={rows} connection={connection} onClose={() => setPopoverOpen(false)} />
      ) : null}
    </>
  );
}

function OfflineNotice({
  connection,
  remoteName,
  onReconnect,
}: {
  connection: { lastConnectedAt: number | null };
  remoteName: string | null;
  onReconnect: () => void;
}) {
  return (
    <div className="q-banner q-side-banner">
      <span className="ic">
        <WifiOffIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
      </span>
      <span className="grow">
        {remoteName ? (
          <b>Can’t reach {remoteName}.</b>
        ) : (
          <>
            <b>Offline</b>
            {connection.lastConnectedAt
              ? ` · as of ${absoluteTime(new Date(connection.lastConnectedAt).toISOString())}`
              : null}
          </>
        )}
      </span>
      {remoteName ? (
        <UseThisMachineButton compact />
      ) : (
        <button type="button" className="q-btn ghost" style={{ height: 24 }} onClick={onReconnect}>
          Reconnect
        </button>
      )}
    </div>
  );
}

function useNeedsYouCount(): number {
  const threads = useApp((state) => state.threads);
  const approvals = useMcp((state) => state.approvals);
  const now = useNow();
  return useMemo(
    () =>
      Object.values(threads).filter(
        (thread) => !isSnoozed(thread, now) && needsAttention(thread, now),
      ).length + pendingMcpApprovals(approvals).length,
    [threads, approvals, now],
  );
}

function ThreadsSidebar() {
  const route = useApp((state) => state.ui.route);
  const threads = useApp((state) => state.threads);
  const hosts = useApp((state) => state.hosts);
  const tab = useApp((state) => state.ui.tab);
  const query = useApp((state) => state.ui.query);
  const loading = useApp((state) => state.loadingThreads);
  const error = useApp((state) => state.threadsError);
  const selectedId = useApp((state) => state.ui.selectedThreadId);
  const layout = useApp((state) => state.ui.layout);
  const connection = useApp((state) => state.connection);
  const snoozedOpen = useApp((state) => state.ui.snoozedOpen);
  const state = useApp((s) => s);
  const now = useNow();
  const rows = useHostRows();
  const stored = useRemote((state) => state.active);

  const counts = useMemo(() => tabCounts(state, now), [state, now]);
  const visible = useMemo(() => threadsForTab(state, tab, now), [state, tab, now]);
  const snoozed = useMemo(() => snoozedThreads(state, now), [state, now]);
  const projects = useProjects((value) => value.projects);
  const prefs = useProjects((value) => value.prefs);
  const localHostId = useApp((value) => value.localHostId);
  const [dragging, setDragging] = useProjectDrag();
  useEffect(() => {
    if (localHostId) void loadProjects();
  }, [localHostId]);
  const groups = useMemo(
    () => sidebarProjects({ visible, all: Object.values(threads), projects, prefs }),
    [visible, threads, projects, prefs],
  );
  const groupKeys = useMemo(() => groups.map((group) => group.key), [groups]);
  const allEmpty = Object.keys(threads).length === 0 && projects.length === 0 && !loading;
  const destination: Destination | null =
    route.kind === "placeholder"
      ? route.surface === "pulls"
        ? "pr"
        : route.surface === "usage"
          ? "usage"
          : route.surface === "settings"
            ? "settings"
            : null
      : null;

  const activate = (value: ThreadTab) => setTab(value);

  return (
    <nav className="q-side" aria-label="Threads" style={{ width: layout.sidebarWidth }}>
      <div className="q-side-hd">
        <div className="q-side-top">
          <span className="q-word">h4rn</span>
          <span className="grow" />
          <button
            type="button"
            className="q-ibtn"
            aria-label="Hide sidebar"
            title={titleWithShortcut("Hide sidebar", "sidebar.toggle")}
            onClick={toggleSidebar}
          >
            <PanelLeftIcon className="q-icon" strokeWidth={1.5} aria-hidden />
          </button>
          <button
            type="button"
            className="q-ibtn"
            aria-label="New thread"
            title={titleWithShortcut("New thread", "thread.new")}
            onClick={() => openNewThread()}
          >
            <SquarePenIcon className="q-icon" strokeWidth={1.5} aria-hidden />
          </button>
        </div>
        <button
          type="button"
          className="q-side-search"
          onClick={() => setUi({ paletteOpen: true })}
        >
          <SearchIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
          <span>Search or run</span>
          <ShortcutCaps command="palette.toggle" />
        </button>
        {allEmpty ? null : (
          <div className="q-seg" role="tablist" aria-label="Thread state">
            {TAB_ORDER.map((value) => (
              <button
                key={value}
                type="button"
                role="tab"
                aria-selected={tab === value}
                onClick={() => activate(value)}
              >
                {TAB_LABELS[value]} <span className="c q-num">{loading ? "—" : counts[value]}</span>
              </button>
            ))}
          </div>
        )}
      </div>

      {connection.status === "offline" && !error ? (
        <OfflineNotice
          connection={connection}
          remoteName={stored?.machineName ?? null}
          onReconnect={() => void loadThreads()}
        />
      ) : null}

      {error ? (
        <div className="q-banner err q-side-banner">
          <span className="ic">
            <CircleAlertIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
          </span>
          <span className="grow">
            <b>Couldn’t reach {unreachableHostLabel(hosts)}.</b>{" "}
            {error.hint ?? "Showing cached threads."}
          </span>
          {connection.status === "offline" && stored ? (
            <UseThisMachineButton compact />
          ) : (
            <button
              type="button"
              className="q-btn ghost"
              style={{ height: 24 }}
              onClick={() => void loadThreads()}
            >
              Retry
            </button>
          )}
        </div>
      ) : null}

      <div className="q-side-list q-scroll">
        {loading && Object.keys(threads).length === 0 ? (
          <FilterGroupSkeleton />
        ) : allEmpty ? (
          <div className="q-empty q-side-empty">
            <h3>No projects yet</h3>
            <p>
              A project is a folder on one of your hosts. Every thread you start there is listed
              under it.
            </p>
            <button type="button" className="q-btn secondary" onClick={openAddProject}>
              <FolderPlusIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
              Add a project…
            </button>
          </div>
        ) : visible.length === 0 && groups.length === 0 ? (
          <div className="q-empty q-side-empty">
            <h3>
              {query
                ? "No threads match"
                : tab === "working"
                  ? "Working is clear"
                  : `${TAB_LABELS[tab]} is empty`}
            </h3>
            <p>
              {query
                ? "Clear the search to see every thread again."
                : tab === "working"
                  ? "Threads with a running turn, a pending approval or a failed step appear here."
                  : "Nothing is waiting in this state right now."}
            </p>
            {query ? (
              <button
                type="button"
                className="q-btn secondary"
                onClick={() => setUi({ query: "" })}
              >
                Clear search
              </button>
            ) : tab === "working" ? (
              <button type="button" className="q-btn secondary" onClick={() => openNewThread()}>
                New thread
              </button>
            ) : null}
          </div>
        ) : (
          <>
            <ProjectsHeader />
            {groups.map((group) => (
              <ProjectGroupSection
                key={group.key}
                group={group}
                order={groupKeys}
                dragging={dragging}
                onDragging={setDragging}
                hosts={hosts}
                selectedId={selectedId}
                closed={layout.closedProjects[group.key] === true}
                now={now}
              />
            ))}
            <ProjectsTail order={groupKeys} dragging={dragging} onDragging={setDragging} />
          </>
        )}

        {tab === "working" && snoozed.length > 0 ? (
          <SnoozedSection threads={snoozed} now={now} open={snoozedOpen} />
        ) : null}
      </div>

      <SidebarFooter
        destination={destination}
        loading={loading}
        offline={connection.status === "offline"}
        rows={rows}
        connection={connection}
      />
      <Resizer
        side="side"
        width={layout.sidebarWidth}
        min={240}
        max={420}
        label="Resize sidebar"
        onChange={setSidebarWidth}
      />
    </nav>
  );
}

function SnoozedSection({
  threads,
  now,
  open,
}: {
  threads: ThreadSummary[];
  now: number;
  open: boolean;
}) {
  const hosts = useApp((state) => state.hosts);
  return (
    <>
      <button
        type="button"
        className="q-snoozed"
        aria-expanded={open}
        onClick={() => setUi({ snoozedOpen: !open })}
      >
        <ClockIcon className="q-icon s14 q-faint" strokeWidth={1.5} aria-hidden />
        <span>Snoozed</span>
        <span className="q-faint q-num">{threads.length}</span>
        {open ? (
          <ChevronDownIcon className="q-icon s14 q-faint" strokeWidth={1.5} aria-hidden />
        ) : (
          <ChevronRightIcon className="q-icon s14 q-faint" strokeWidth={1.5} aria-hidden />
        )}
      </button>
      {open ? (
        <div className="q-snoozed-list">
          {threads.map((thread) => (
            <button
              key={thread.thread_id}
              type="button"
              className="q-row"
              onClick={() => openThread(thread.thread_id)}
            >
              <span className="q-row-st">
                <EntityAvatar
                  seed={machineSeed(thread.host)}
                  kind="machine"
                  label={hosts[thread.host]?.name ?? thread.host}
                  size={14}
                />
              </span>
              <span className="q-row-main">
                <span className="q-row-l1">
                  <span className="q-row-title">{thread.title}</span>
                  <span className="q-row-time q-num">
                    {thread.snoozed_until
                      ? new Date(thread.snoozed_until).toLocaleTimeString([], {
                          hour: "2-digit",
                          minute: "2-digit",
                        })
                      : ""}
                  </span>
                </span>
                <span className="q-row-l2">
                  <span className="q-row-meta">
                    wakes in {relativeAge(thread.snoozed_until ?? "", now)} ·{" "}
                    {harnessLabel(thread.harness)}
                  </span>
                </span>
              </span>
            </button>
          ))}
        </div>
      ) : null}
    </>
  );
}

function SettingsSidebar() {
  const route = useApp((state) => state.ui.route);
  const layout = useApp((state) => state.ui.layout);
  const connection = useApp((state) => state.connection);
  const loading = useApp((state) => state.loadingThreads);
  const rows = useHostRows();
  const mcpServers = useMcp((state) => state.servers);
  const skillInventory = useSkills((state) => state.inventory);
  const skills = skillInventory?.skills ?? NO_SKILLS;
  const daemonHost = useApp((state) => state.localHostId);
  const deviceCount = useRemote((state) => state.devices.length);
  useHarnessVersions();
  const harnessUpdates = useHarnesses(harnessUpdateCount);
  // The Tools group shows counts for both halves, so read them once the daemon
  // has reported its host rather than leaving a misleading zero.
  useEffect(() => {
    if (!daemonHost) return;
    void loadMcpServers();
    void loadSkills();
  }, [daemonHost]);
  const library = route.kind === "placeholder" && route.surface === "library";
  // The route decides which half of Tools is open; selection only picks a server.
  const skillsActive =
    route.kind === "placeholder" && route.path.replace(/\/+$/, "").endsWith("/skills");
  const section = route.kind === "placeholder" ? (route.path.split("/")[2] ?? "hosts") : "hosts";
  const destination: Destination = "settings";
  let group: string | undefined;

  const go = (path: string) => navigate({ kind: "placeholder", surface: "settings", path });
  const goLibrary = (path: string) => navigate({ kind: "placeholder", surface: "library", path });
  const openMcp = (name: string) => {
    setLibrarySelected(name);
    if (!library) goLibrary("/library");
    else pushLibraryPath("/library");
  };
  /** Keep the address bar on the MCP side once a subrow is picked. */
  const pushLibraryPath = (path: string) => {
    if (route.kind === "placeholder" && route.path !== path) goLibrary(path);
  };

  return (
    <nav className="q-side q-set-side" aria-label="Settings" style={{ width: layout.sidebarWidth }}>
      <div className="q-side-hd">
        <div className="q-side-top">
          <button
            type="button"
            className="q-ibtn"
            aria-label="Back to threads"
            title="Back to threads  Esc"
            onClick={() => navigate({ kind: "inbox" })}
          >
            <ChevronRightIcon
              className="q-icon"
              style={{ transform: "rotate(180deg)" }}
              strokeWidth={1.5}
              aria-hidden
            />
          </button>
          <span className="q-word">Settings</span>
        </div>
        <label className="q-side-search">
          <SearchIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
          <input placeholder="Search settings" aria-label="Search settings" />
        </label>
      </div>

      <div className="q-side-list setnav">
        {SETTINGS_SECTIONS.map((item) => {
          const header = item.group && item.group !== group ? item.group : null;
          group = item.group ?? group;
          return (
            <div key={item.key} style={{ display: "contents" }}>
              {header ? (
                <div className="q-side-h" style={{ paddingTop: 18 }}>
                  <span>{header}</span>
                </div>
              ) : null}
              <button
                type="button"
                className="q-nav"
                aria-current={
                  !library && (section === "devices" ? "hosts" : section) === item.key
                    ? "page"
                    : undefined
                }
                onClick={() => go(settingsPath(item.key))}
              >
                <item.icon className="q-icon" strokeWidth={1.5} aria-hidden />
                <span className="lbl">{item.label}</span>
                {item.key === "harnesses" && harnessUpdates > 0 ? (
                  <span className="ct q-num" title="Harness updates available">
                    {harnessUpdates}
                  </span>
                ) : null}
                {item.key === "hosts" ? (
                  <span className="ct q-num" title={`${rows.length} hosts, ${deviceCount} devices`}>
                    {rows.length}
                  </span>
                ) : null}
              </button>
              {item.key === "providers" ? (
                <>
                  <div className="q-side-h">
                    <span>Tools</span>
                    <button
                      type="button"
                      className="q-ibtn s22"
                      aria-label="Add MCP server"
                      title="Add MCP server"
                      onClick={() => {
                        setLibrarySelected("__new");
                        goLibrary("/library");
                      }}
                    >
                      +
                    </button>
                  </div>
                  <button
                    type="button"
                    className={cn("q-nav", library && !skillsActive && "parent")}
                    aria-current={library && !skillsActive ? "page" : undefined}
                    onClick={() => {
                      setLibrarySelected(mcpServers[0]?.name ?? "");
                      goLibrary("/library");
                    }}
                  >
                    <PlugIcon className="q-icon" strokeWidth={1.5} aria-hidden />
                    <span className="lbl">MCP servers</span>
                    <span className="ct q-num">{mcpServers.length}</span>
                  </button>
                  {library && !skillsActive ? (
                    <div className="q-set-sub">
                      {mcpServers.map((server) => {
                        const status = serverStatus(server);
                        return (
                          <button
                            key={server.name}
                            type="button"
                            className="q-set-subrow"
                            aria-current={
                              layout.librarySelected === server.name ? "true" : undefined
                            }
                            onClick={() => openMcp(server.name)}
                          >
                            <span>{server.name}</span>
                            <McpHealthGlyph status={status} />
                          </button>
                        );
                      })}
                    </div>
                  ) : null}
                  <button
                    type="button"
                    className="q-nav"
                    aria-current={skillsActive ? "page" : undefined}
                    onClick={() => {
                      setLibrarySelected("skills");
                      goLibrary("/library/skills");
                    }}
                  >
                    <BlocksIcon className="q-icon" strokeWidth={1.5} aria-hidden />
                    <span className="lbl">Skills</span>
                    <span className="ct q-num">{skills.length}</span>
                  </button>
                </>
              ) : null}
            </div>
          );
        })}
      </div>

      <SidebarFooter
        destination={destination}
        loading={loading}
        offline={connection.status === "offline"}
        rows={rows}
        connection={connection}
      />
      <Resizer
        side="side"
        width={layout.sidebarWidth}
        min={240}
        max={420}
        label="Resize sidebar"
        onChange={setSidebarWidth}
      />
    </nav>
  );
}

/** Health as a glyph plus an accessible word, never colour alone (DESIGN rule 3). */
function McpHealthGlyph({ status }: { status: ReturnType<typeof serverStatus> }) {
  const common = { className: "q-icon s14", strokeWidth: 1.5, role: "img" } as const;
  const label = status.label;
  if (status.tone === "ok")
    return <CircleCheckIcon {...common} aria-label={label} style={{ color: "var(--fg-3)" }} />;
  if (status.tone === "danger")
    return <CircleXIcon {...common} aria-label={label} style={{ color: "var(--danger)" }} />;
  if (status.tone === "warn")
    return label === "Needs credential" ? (
      <KeyRoundIcon {...common} aria-label={label} style={{ color: "var(--warn)" }} />
    ) : (
      <CircleAlertIcon {...common} aria-label={label} style={{ color: "var(--warn)" }} />
    );
  return <CircleIcon {...common} aria-label={label} style={{ color: "var(--fg-3)" }} />;
}

/** Names the host that is actually down; several (or none known) read as "a host". */
function unreachableHostLabel(hosts: Record<string, { name: string; online: boolean }>): string {
  const down = Object.values(hosts).filter((host) => !host.online);
  if (down.length === 1) return down[0]?.name ?? "a host";
  if (down.length > 1) return `${down.length} hosts`;
  return "the daemon";
}

export function Sidebar() {
  const route = useApp((state) => state.ui.route);
  const settingsMode =
    route.kind === "placeholder" && (route.surface === "settings" || route.surface === "library");
  return settingsMode ? <SettingsSidebar /> : <ThreadsSidebar />;
}

export function SidebarReveal() {
  const layout = useApp((state) => state.ui.layout);
  const needsYou = useNeedsYouCount();
  if (layout.sidebarOpen) return null;
  return (
    <div className="q-hd-l">
      <button
        type="button"
        className="q-ibtn"
        aria-label={needsYou > 0 ? "Show sidebar. 1 thread needs you" : "Show sidebar"}
        title={titleWithShortcut("Show sidebar", "sidebar.toggle")}
        onClick={toggleSidebar}
      >
        <PanelLeftIcon className="q-icon" strokeWidth={1.5} aria-hidden />
        {needsYou > 0 ? <span className="q-pip" aria-hidden="true" /> : null}
      </button>
      <button
        type="button"
        className="q-ibtn"
        aria-label="New thread"
        title={titleWithShortcut("New thread", "thread.new")}
        onClick={() => openNewThread()}
      >
        <SquarePenIcon className="q-icon" strokeWidth={1.5} aria-hidden />
      </button>
      <span className="q-vsep" />
    </div>
  );
}
