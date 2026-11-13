import {
  CircleSlash2Icon,
  ExternalLinkIcon,
  FileDiffIcon,
  ListTreeIcon,
  MoreHorizontalIcon,
  PinIcon,
  Share2Icon,
  SquareIcon,
  SunMoonIcon,
  TerminalIcon,
} from "lucide-react";

import { AgentGlyph } from "@/components/agent/AgentGlyph";
import { EntityAvatar } from "@/components/agent/EntityAvatar";
import { MOD, SHIFT } from "@/components/common/Kbd";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import type { HostSummary, ThreadSummary } from "@/gen";
import { useIsPhone } from "@/hooks/use-media-query";
import { glyphForThread } from "@/lib/agent";
import { harnessLabel, harnessSeed, machineSeed } from "@/lib/entities";
import { absoluteDateTime, relativeAge } from "@/lib/format";
import {
  acknowledgeThread,
  interruptThread,
  navigate,
  pushToast,
  setThreadTab,
  snoozeThread,
  togglePin,
  toggleTheme,
} from "@/lib/store/appStore";
import { cn } from "@/lib/utils";

export function ThreadHeader({
  summary,
  host,
  onBack,
  terminalOpen = false,
  onToggleTerminal,
  subagentsOpen = false,
  onToggleSubagents,
  stale = false,
}: {
  summary: ThreadSummary;
  host: HostSummary | undefined;
  onBack: () => void;
  terminalOpen?: boolean;
  onToggleTerminal?: () => void;
  subagentsOpen?: boolean;
  onToggleSubagents?: () => void;
  stale?: boolean;
}) {
  const hostOnline = host?.online ?? false;
  const working = summary.tab === "working";
  // A dropped connection outranks the last agent state: do not animate work
  // the daemon can no longer confirm.
  const glyph =
    stale && working
      ? { kind: "offline" as const, label: "Host unreachable" }
      : glyphForThread(summary, hostOnline);
  const isPhone = useIsPhone();
  const hostName = host?.name ?? summary.host;
  const stateLabel =
    summary.tab === "done" && summary.agent_state === "done"
      ? `Done · ${relativeAge(summary.last_activity)}`
      : glyph.label;

  const threadMenu = (
    <DropdownMenu>
      <DropdownMenuTrigger asChild>
        <button type="button" className={isPhone ? "navbtn" : "q-ibtn"} aria-label="Thread actions">
          <MoreHorizontalIcon className={isPhone ? "q-icon s22" : "q-icon"} strokeWidth={1.5} />
        </button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="end" className="w-56">
        <DropdownMenuLabel>Thread</DropdownMenuLabel>
        <DropdownMenuItem onSelect={() => void togglePin(summary.thread_id)}>
          <PinIcon aria-hidden />
          {summary.pinned ? "Unpin from Working" : "Keep in Working"}
        </DropdownMenuItem>
        <DropdownMenuItem onSelect={() => void setThreadTab(summary.thread_id, "done")}>
          Move to Done
        </DropdownMenuItem>
        <DropdownMenuItem onSelect={() => void setThreadTab(summary.thread_id, "settled")}>
          Settle now
        </DropdownMenuItem>
        <DropdownMenuItem
          onSelect={() =>
            void snoozeThread(summary.thread_id, new Date(Date.now() + 3_600_000).toISOString())
          }
        >
          Snooze for 1 h
        </DropdownMenuItem>
        <DropdownMenuSeparator />
        <DropdownMenuItem
          onSelect={() => {
            void navigator.clipboard
              ?.writeText(`${window.location.origin}/t/${summary.thread_id}`)
              .then(() => pushToast({ title: "Thread link copied.", variant: "success" }));
          }}
        >
          <Share2Icon aria-hidden />
          Copy thread link
        </DropdownMenuItem>
        <DropdownMenuItem onSelect={() => navigate({ kind: "hosts" })}>
          <ExternalLinkIcon aria-hidden />
          Open hosts
        </DropdownMenuItem>
        <DropdownMenuItem onSelect={() => navigate({ kind: "ide" })}>
          <CircleSlash2Icon aria-hidden />
          Open in IDE
        </DropdownMenuItem>
        <DropdownMenuSeparator />
        <DropdownMenuItem onSelect={toggleTheme}>
          <SunMoonIcon aria-hidden />
          Toggle theme
          <span className="ml-auto text-small text-fg-3">
            {MOD} {SHIFT} L
          </span>
        </DropdownMenuItem>
        <DropdownMenuSeparator />
        <DropdownMenuItem disabled>
          <span className="text-small">Started {absoluteDateTime(summary.last_activity)}</span>
        </DropdownMenuItem>
      </DropdownMenuContent>
    </DropdownMenu>
  );

  if (isPhone) {
    return (
      <>
        <div className="q-ph-nav">
          <button type="button" className="navbtn" aria-label="Back to inbox" onClick={onBack}>
            <span aria-hidden className="q-ph-back-glyph">
              ‹
            </span>
            Inbox
          </button>
          <span className="ttl" />
          <div className="q-ph-nav-actions">
            {onToggleTerminal ? (
              <button
                type="button"
                className={cn("navbtn", terminalOpen && "on")}
                aria-label="Toggle terminal"
                aria-pressed={terminalOpen}
                onClick={onToggleTerminal}
              >
                <TerminalIcon className="q-icon s22" strokeWidth={1.5} aria-hidden />
              </button>
            ) : null}
            {threadMenu}
          </div>
        </div>
        <div className="q-ph-th-title">
          <div className="q-ph-th-name">{summary.title}</div>
          <div className="q-ph-th-meta">
            <EntityAvatar
              seed={machineSeed(summary.host)}
              kind="machine"
              label={hostName}
              size={16}
            />
            <span>{hostName}</span>
            <span className="q-sep">·</span>
            <EntityAvatar
              seed={harnessSeed(summary.host, summary.harness)}
              kind="harness"
              label={harnessLabel(summary.harness)}
              size={14}
            />
            <span>{harnessLabel(summary.harness)}</span>
            <span className="grow" />
            <span className={cn("q-th-state", summary.needs_attention && "q-accent")}>
              <AgentGlyph glyph={glyph} size={14} />
              <span>{stateLabel}</span>
            </span>
          </div>
        </div>
      </>
    );
  }

  return (
    <header className="q-th-head">
      <div className="q-th-titles">
        <h1>{summary.title}</h1>
        <div className="q-prov">
          <EntityAvatar
            seed={machineSeed(summary.host)}
            kind="machine"
            label={hostName}
            size={14}
          />
          <span>{hostName}</span>
          <span className="q-sep">·</span>
          <EntityAvatar
            seed={harnessSeed(summary.host, summary.harness)}
            kind="harness"
            label={harnessLabel(summary.harness)}
            size={14}
          />
          <span>{harnessLabel(summary.harness)}</span>
          <span className="opt">
            <span className="q-sep">·</span> {summary.model}
          </span>
          <span className="br">
            <span className="q-sep">·</span>
            <span className="q-mono" style={{ fontSize: 11.5 }}>
              {summary.workspace_path}
            </span>
          </span>
        </div>
      </div>

      <div className={cn("q-th-state", summary.needs_attention && "q-accent")}>
        <AgentGlyph glyph={glyph} size={14} />
        <span>{stateLabel}</span>
      </div>

      <div className="q-th-actions">
        {onToggleTerminal ? (
          <button
            type="button"
            className={cn("q-ibtn", terminalOpen && "on")}
            aria-label="Toggle terminal"
            aria-pressed={terminalOpen}
            title="Terminal · T"
            onClick={onToggleTerminal}
          >
            <TerminalIcon className="q-icon" strokeWidth={1.5} aria-hidden />
          </button>
        ) : null}
        {onToggleSubagents ? (
          <button
            type="button"
            className={cn("q-ibtn", subagentsOpen && "on")}
            aria-label="Toggle subagents"
            aria-pressed={subagentsOpen}
            title={`Subagents · ${MOD} J`}
            onClick={onToggleSubagents}
          >
            <ListTreeIcon className="q-icon" strokeWidth={1.5} aria-hidden />
          </button>
        ) : null}
        <button
          type="button"
          className="q-ibtn"
          aria-label="Changes"
          title="Changes · D"
          onClick={() => navigate({ kind: "ide" })}
        >
          <FileDiffIcon className="q-icon" strokeWidth={1.5} aria-hidden />
        </button>
        <span className="q-vsep" />

        {working && !stale ? (
          <button
            type="button"
            className="q-btn secondary"
            onClick={() => void interruptThread(summary.thread_id)}
          >
            <SquareIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
            Stop
          </button>
        ) : null}

        {summary.tab === "done" ? (
          <>
            <button
              type="button"
              className="q-btn secondary"
              onClick={() =>
                void snoozeThread(summary.thread_id, new Date(Date.now() + 3_600_000).toISOString())
              }
            >
              Snooze
            </button>
            <button
              type="button"
              className="q-btn primary"
              onClick={() => void acknowledgeThread(summary.thread_id)}
            >
              Acknowledge <span className="q-kbd">A</span>
            </button>
          </>
        ) : null}

        {threadMenu}
      </div>
    </header>
  );
}
