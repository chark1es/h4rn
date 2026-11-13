import {
  ChartNoAxesColumnIcon,
  Code2Icon,
  GitPullRequestIcon,
  InboxIcon,
  KeyRoundIcon,
  LibraryIcon,
  MoreHorizontalIcon,
  PlusIcon,
  ServerIcon,
  SettingsIcon,
  SmartphoneIcon,
  SunMoonIcon,
} from "lucide-react";
import { useState } from "react";

import { EntityAvatar } from "@/components/agent/EntityAvatar";
import { Button } from "@/components/ui/button";
import {
  Sheet,
  SheetContent,
  SheetDescription,
  SheetHeader,
  SheetTitle,
} from "@/components/ui/sheet";
import { userSeed } from "@/lib/entities";
import type { Route } from "@/lib/router";
import { navigate, openNewThread, openShortcuts, toggleTheme, useApp } from "@/lib/store/appStore";
import { cn } from "@/lib/utils";

const NAV_ITEMS = [
  { key: "inbox", label: "Inbox", icon: InboxIcon, route: { kind: "inbox" } as Route },
  { key: "hosts", label: "Hosts", icon: ServerIcon, route: { kind: "hosts" } as Route },
] as const;

function isActive(route: Route, key: string): boolean {
  if (key === "inbox") return route.kind === "inbox" || route.kind === "thread";
  if (key === "hosts") return route.kind === "hosts";
  return false;
}

export function PhoneTopBar() {
  const hosts = useApp((state) => state.hosts);
  const status = useApp((state) => state.connection.status);
  const online = Object.values(hosts).filter((host) => host.online).length;
  const total = Object.keys(hosts).length;

  return (
    <div className="flex h-9 shrink-0 items-center gap-2 border-b border-line bg-paper px-3 text-micro text-ink-muted">
      <span className="font-mono tabular text-ink-faint">
        {new Date().toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" })}
      </span>
      <span className="ml-auto flex items-center gap-1.5">
        <span
          aria-hidden
          className={cn(
            "size-1.5 rounded-full",
            status === "online" ? "bg-standby" : status === "degraded" ? "bg-cue" : "bg-strike",
          )}
        />
        <span>
          {total === 0 ? "no hosts" : `${online} of ${total} hosts`} · {status}
        </span>
      </span>
    </div>
  );
}

function NavButton({
  label,
  icon: Icon,
  active,
  onClick,
}: {
  label: string;
  icon: typeof InboxIcon;
  active?: boolean;
  onClick: () => void;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      aria-current={active ? "page" : undefined}
      className={cn(
        "flex h-12 flex-1 flex-col items-center justify-center gap-0.5 rounded-sm text-micro",
        active ? "text-ink" : "text-ink-muted",
      )}
    >
      <Icon className="size-5" strokeWidth={1.5} aria-hidden />
      <span>{label}</span>
    </button>
  );
}

export function PhoneNav() {
  const route = useApp((state) => state.ui.route);
  const [moreOpen, setMoreOpen] = useState(false);

  return (
    <>
      <nav
        aria-label="Primary"
        className="h-safe-nav flex shrink-0 items-start border-t border-line bg-panel pb-[env(safe-area-inset-bottom)]"
      >
        {NAV_ITEMS.map((item) => (
          <NavButton
            key={item.key}
            label={item.label}
            icon={item.icon}
            active={isActive(route, item.key)}
            onClick={() => navigate(item.route)}
          />
        ))}

        <div className="flex flex-1 items-start justify-center">
          <button
            type="button"
            aria-label="New thread"
            onClick={openNewThread}
            className="mt-1.5 flex size-11 items-center justify-center rounded-sm bg-cue text-ink"
          >
            <PlusIcon className="size-5" />
          </button>
        </div>

        <NavButton
          label="Limits"
          icon={ChartNoAxesColumnIcon}
          active={route.kind === "placeholder" && route.path.startsWith("/usage")}
          onClick={() => navigate({ kind: "placeholder", surface: "usage", path: "/usage" })}
        />
        <NavButton
          label="More"
          icon={MoreHorizontalIcon}
          active={moreOpen}
          onClick={() => setMoreOpen(true)}
        />
      </nav>

      <Sheet open={moreOpen} onOpenChange={setMoreOpen}>
        <SheetContent side="bottom" className="rounded-t-[14px] pb-safe">
          <SheetHeader>
            <SheetTitle>More</SheetTitle>
            <SheetDescription>Library, pull requests, IDE and settings.</SheetDescription>
          </SheetHeader>

          <div className="grid grid-cols-2 gap-1.5 px-4">
            <MoreLink
              icon={LibraryIcon}
              label="Library"
              onClick={() =>
                navigate({ kind: "placeholder", surface: "library", path: "/library" })
              }
            />
            <MoreLink
              icon={GitPullRequestIcon}
              label="Pull requests"
              onClick={() => navigate({ kind: "placeholder", surface: "pulls", path: "/pulls" })}
            />
            <MoreLink
              icon={Code2Icon}
              label="IDE"
              onClick={() => navigate({ kind: "placeholder", surface: "ide", path: "/ide" })}
            />
            <MoreLink
              icon={SettingsIcon}
              label="Settings"
              onClick={() =>
                navigate({ kind: "placeholder", surface: "settings", path: "/settings" })
              }
            />
            <MoreLink icon={KeyRoundIcon} label="Shortcuts" onClick={openShortcuts} />
            <MoreLink icon={SunMoonIcon} label="Toggle theme" onClick={toggleTheme} />
            <MoreLink
              icon={SmartphoneIcon}
              label="Devices"
              onClick={() =>
                navigate({ kind: "placeholder", surface: "settings", path: "/settings" })
              }
            />
          </div>

          <div className="flex items-center gap-2 px-4 pt-2 pb-2">
            <EntityAvatar seed={userSeed("local")} kind="user" label="You" size={28} />
            <div className="min-w-0 flex-1">
              <p className="text-compact font-medium text-ink">You</p>
              <p className="text-micro text-ink-muted">Local device · direct</p>
            </div>
            <Button variant="ghost" size="sm" onClick={() => setMoreOpen(false)}>
              Close
            </Button>
          </div>
        </SheetContent>
      </Sheet>
    </>
  );
}

function MoreLink({
  icon: Icon,
  label,
  onClick,
}: {
  icon: typeof InboxIcon;
  label: string;
  onClick: () => void;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      className="flex h-12 items-center gap-2 rounded-sm border border-line bg-paper px-2.5 text-left text-compact text-ink transition-colors duration-[var(--dur-instant)] active:bg-panel"
    >
      <Icon className="size-4 text-ink-muted" strokeWidth={1.5} aria-hidden />
      {label}
    </button>
  );
}
