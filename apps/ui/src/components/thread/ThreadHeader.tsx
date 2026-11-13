import {
  ArrowLeftIcon,
  CircleSlash2Icon,
  ExternalLinkIcon,
  MoreHorizontalIcon,
  PinIcon,
  Share2Icon,
  SquareIcon,
  SunMoonIcon,
} from "lucide-react";
import type * as React from "react";

import { AgentGlyph } from "@/components/agent/AgentGlyph";
import { EntityAvatar } from "@/components/agent/EntityAvatar";
import { MOD, SHIFT } from "@/components/common/Kbd";
import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import type { HostSummary, ThreadSummary } from "@/gen";
import { glyphForThread } from "@/lib/agent";
import { harnessLabel, harnessSeed, machineSeed } from "@/lib/entities";
import { absoluteDateTime } from "@/lib/format";
import {
  interruptThread,
  navigate,
  pushToast,
  setThreadTab,
  snoozeThread,
  togglePin,
  toggleTheme,
} from "@/lib/store/appStore";
import { cn } from "@/lib/utils";

function Chip({
  children,
  title,
  mono,
}: {
  children: React.ReactNode;
  title?: string;
  mono?: boolean;
}) {
  return (
    <span
      title={title}
      className={cn(
        "inline-flex h-5 max-w-[220px] items-center gap-1 truncate rounded-sm border border-line bg-panel px-1.5 text-micro text-ink-muted",
        mono && "font-mono",
      )}
    >
      {children}
    </span>
  );
}

export function ThreadHeader({
  summary,
  host,
  onBack,
}: {
  summary: ThreadSummary;
  host: HostSummary | undefined;
  onBack: () => void;
}) {
  const hostOnline = host?.online ?? false;
  const glyph = glyphForThread(summary, hostOnline);
  const working = summary.tab === "working";

  return (
    <header className="shrink-0 border-b border-line bg-paper">
      <div className="flex h-13 items-center gap-2 px-3 sm:h-14 sm:px-4">
        <Button
          size="icon"
          variant="ghost"
          className="-ml-1 lg:hidden"
          aria-label="Back to inbox"
          onClick={onBack}
        >
          <ArrowLeftIcon />
        </Button>

        <div className="flex min-w-0 flex-1 flex-col justify-center">
          <h2 className="truncate text-compact font-semibold text-ink sm:text-title">
            {summary.title}
          </h2>
          <div className="hidden items-center gap-1.5 md:flex">
            <Chip title={host?.name ?? summary.host}>
              <EntityAvatar
                seed={machineSeed(summary.host)}
                kind="machine"
                label={host?.name ?? summary.host}
                size={12}
              />
              <span className="truncate">{host?.name ?? summary.host}</span>
            </Chip>
            <Chip>
              <EntityAvatar
                seed={harnessSeed(summary.host, summary.harness)}
                kind="harness"
                label={harnessLabel(summary.harness)}
                size={12}
              />
              <span className="truncate">{harnessLabel(summary.harness)}</span>
            </Chip>
            <Chip mono title={summary.model}>
              {summary.model}
            </Chip>
            <Chip mono title={summary.workspace_path}>
              {summary.workspace_path}
            </Chip>
          </div>
        </div>

        <div className="flex shrink-0 items-center gap-1">
          <span className="mr-1 hidden items-center gap-1.5 text-micro text-ink-muted sm:flex">
            <AgentGlyph glyph={glyph} size={18} />
            <span className={cn(summary.needs_attention && "text-cue-ink")}>{glyph.label}</span>
          </span>

          {working ? (
            <Button
              size="icon"
              variant="ghost"
              aria-label="Interrupt turn"
              title="Interrupt turn (Ctrl .)"
              onClick={() => void interruptThread(summary.thread_id)}
            >
              <SquareIcon className="size-3.5" />
            </Button>
          ) : null}

          <DropdownMenu>
            <DropdownMenuTrigger asChild>
              <Button size="icon" variant="ghost" aria-label="Thread actions">
                <MoreHorizontalIcon />
              </Button>
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
                  void snoozeThread(
                    summary.thread_id,
                    new Date(Date.now() + 3_600_000).toISOString(),
                  )
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
              <DropdownMenuItem
                onSelect={() => navigate({ kind: "placeholder", surface: "ide", path: "/ide" })}
              >
                <CircleSlash2Icon aria-hidden />
                Open in IDE
              </DropdownMenuItem>
              <DropdownMenuSeparator />
              <DropdownMenuItem onSelect={toggleTheme}>
                <SunMoonIcon aria-hidden />
                Toggle theme
                <span className="ml-auto text-micro text-ink-faint">
                  {MOD} {SHIFT} L
                </span>
              </DropdownMenuItem>
              <DropdownMenuSeparator />
              <DropdownMenuItem disabled>
                <span className="text-micro">
                  Started {absoluteDateTime(summary.last_activity)}
                </span>
              </DropdownMenuItem>
            </DropdownMenuContent>
          </DropdownMenu>
        </div>
      </div>

      <div className="flex items-center gap-1.5 overflow-x-auto px-3 pb-2 md:hidden">
        <Chip title={host?.name ?? summary.host}>
          <EntityAvatar
            seed={machineSeed(summary.host)}
            kind="machine"
            label={host?.name ?? summary.host}
            size={12}
          />
          {host?.name ?? summary.host}
        </Chip>
        <Chip>{harnessLabel(summary.harness)}</Chip>
        <Chip mono>{summary.model}</Chip>
      </div>
    </header>
  );
}
