import {
  BellIcon,
  CommandIcon,
  FlaskConicalIcon,
  GitBranchIcon,
  GitCommitHorizontalIcon,
  GitPullRequestIcon,
  LayersIcon,
  MessageSquareIcon,
  PlugIcon,
  ServerIcon,
  SettingsIcon,
  SunIcon,
  TerminalIcon,
  TimerIcon,
  UserIcon,
} from "lucide-react";

export type SettingsSection = {
  key: string;
  label: string;
  icon: typeof SettingsIcon;
  group?: string;
};

export const SETTINGS_SECTIONS: SettingsSection[] = [
  { key: "general", label: "General", icon: SettingsIcon },
  { key: "appearance", label: "Appearance", icon: SunIcon },
  { key: "notifications", label: "Notifications", icon: BellIcon },
  { key: "shortcuts", label: "Shortcuts", icon: CommandIcon },
  { key: "threads", label: "Threads", icon: MessageSquareIcon },
  { key: "source-control", label: "Source control", icon: GitCommitHorizontalIcon },
  { key: "hosts", label: "Hosts & devices", icon: ServerIcon, group: "Machines" },
  { key: "harnesses", label: "Harnesses", icon: TerminalIcon, group: "Machines" },
  { key: "worktrees", label: "Worktrees", icon: GitBranchIcon, group: "Machines" },
  { key: "providers", label: "Providers", icon: LayersIcon, group: "Machines" },
  { key: "resume", label: "Auto-resume", icon: TimerIcon, group: "Machines" },
  { key: "forges", label: "Forges", icon: GitPullRequestIcon, group: "Machines" },
  { key: "labs", label: "Labs", icon: FlaskConicalIcon, group: "Machines" },
  { key: "account", label: "Account & relay", icon: UserIcon, group: "Account" },
];

export const TOOLS_SECTION = {
  key: "library",
  label: "MCP servers",
  icon: PlugIcon,
  group: "Tools",
} as const;

export function settingsPath(section: string): string {
  return `/settings/${section}`;
}
