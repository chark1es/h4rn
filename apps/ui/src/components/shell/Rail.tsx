import type { LucideIcon } from "lucide-react";
import {
  ChartNoAxesColumnIcon,
  Code2Icon,
  CommandIcon,
  GitPullRequestIcon,
  InboxIcon,
  KeyboardIcon,
  LibraryIcon,
  SearchIcon,
  ServerIcon,
  SettingsIcon,
  SunMoonIcon,
} from "lucide-react";
import type * as React from "react";

import { EntityAvatar } from "@/components/agent/EntityAvatar";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { userSeed } from "@/lib/entities";
import type { Route } from "@/lib/router";
import {
  navigate,
  openShortcuts,
  requestSearchFocus,
  setUi,
  toggleTheme,
  useApp,
} from "@/lib/store/appStore";
import { cn } from "@/lib/utils";

type RailItem = {
  label: string;
  icon: LucideIcon;
  route?: Route;
  action?: () => void;
  key: string;
};

const ITEMS: RailItem[] = [
  { label: "Inbox", icon: InboxIcon, route: { kind: "inbox" }, key: "inbox" },
  { label: "Hosts", icon: ServerIcon, route: { kind: "hosts" }, key: "hosts" },
  {
    label: "Library",
    icon: LibraryIcon,
    route: { kind: "placeholder", surface: "library", path: "/library" },
    key: "library",
  },
  {
    label: "Usage",
    icon: ChartNoAxesColumnIcon,
    route: { kind: "placeholder", surface: "usage", path: "/usage" },
    key: "usage",
  },
  {
    label: "IDE",
    icon: Code2Icon,
    route: { kind: "placeholder", surface: "ide", path: "/ide" },
    key: "ide",
  },
  {
    label: "Pull requests",
    icon: GitPullRequestIcon,
    route: { kind: "placeholder", surface: "pulls", path: "/pulls" },
    key: "pulls",
  },
];

function itemsActive(route: Route, key: string): boolean {
  if (route.kind === "inbox") return key === "inbox";
  if (route.kind === "thread") return key === "inbox";
  if (route.kind === "hosts") return key === "hosts";
  if (route.kind === "placeholder") {
    return route.path.startsWith(`/${key === "pulls" ? "pulls" : key}`);
  }
  return false;
}

function RailButton({
  label,
  icon: Icon,
  active,
  onClick,
  children,
}: {
  label: string;
  icon?: LucideIcon;
  active?: boolean;
  onClick: () => void;
  children?: React.ReactNode;
}) {
  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <button
          type="button"
          aria-label={label}
          aria-current={active ? "page" : undefined}
          onClick={onClick}
          className={cn(
            "relative flex size-10 items-center justify-center rounded-sm transition-colors duration-[var(--dur-instant)]",
            active
              ? "bg-white/5 text-stage-ink"
              : "text-stage-ink-muted hover:bg-white/5 hover:text-stage-ink",
          )}
        >
          {active ? (
            <span aria-hidden className="absolute inset-y-1.5 left-0 w-0.5 rounded-r-sm bg-cue" />
          ) : null}
          {Icon ? <Icon className="size-5" strokeWidth={1.5} /> : children}
        </button>
      </TooltipTrigger>
      <TooltipContent side="right">{label}</TooltipContent>
    </Tooltip>
  );
}

export function Rail() {
  const route = useApp((state) => state.ui.route);
  const hostCount = useApp((state) => Object.keys(state.hosts).length);
  const onlineHosts = useApp(
    (state) => Object.values(state.hosts).filter((host) => host.online).length,
  );

  return (
    <nav
      aria-label="Primary"
      className="flex w-14 shrink-0 flex-col items-center gap-1 bg-stage py-3 text-stage-ink"
    >
      <div className="mb-2 flex size-8 items-center justify-center rounded-sm bg-cue text-[13px] font-semibold text-ink">
        h4
      </div>

      {ITEMS.map((item) => (
        <RailButton
          key={item.key}
          label={item.label}
          icon={item.icon}
          active={itemsActive(route, item.key)}
          onClick={() => {
            if (item.route) navigate(item.route);
          }}
        />
      ))}

      <div className="mt-auto flex flex-col items-center gap-1">
        <RailButton
          label="Search threads"
          icon={SearchIcon}
          active={false}
          onClick={() => {
            navigate({ kind: "inbox" });
            requestSearchFocus();
          }}
        />
        <RailButton
          label="Commands"
          icon={CommandIcon}
          onClick={() => setUi({ paletteOpen: true })}
        />
        <RailButton label="Shortcuts" icon={KeyboardIcon} onClick={openShortcuts} />
        <RailButton
          label="Settings"
          icon={SettingsIcon}
          onClick={() => navigate({ kind: "placeholder", surface: "settings", path: "/settings" })}
        />
        <RailButton label="Toggle theme" icon={SunMoonIcon} onClick={toggleTheme} />
        <div className="mt-1" title={`${onlineHosts} of ${hostCount} hosts online`}>
          <EntityAvatar seed={userSeed("local")} kind="user" label="You" size={24} />
        </div>
      </div>
    </nav>
  );
}
