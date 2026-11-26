import {
  BellIcon,
  CalendarClockIcon,
  CommandIcon,
  FlaskConicalIcon,
  GitBranchIcon,
  GitCommitHorizontalIcon,
  GitPullRequestIcon,
  LayersIcon,
  MessageSquareIcon,
  ServerIcon,
  SettingsIcon,
  Share2Icon,
  SunIcon,
  TerminalIcon,
  TimerIcon,
  UserIcon,
} from "lucide-react";

/** A Settings page. Its label comes from the `settings` catalogue: `nav.sections.<key>`. */
export type SettingsSection = {
  key: SettingsSectionKey;
  icon: typeof SettingsIcon;
  group?: SettingsGroup;
};

export type SettingsSectionKey =
  | "general"
  | "appearance"
  | "notifications"
  | "chat"
  | "shortcuts"
  | "threads"
  | "schedules"
  | "source-control"
  | "hosts"
  | "harnesses"
  | "worktrees"
  | "providers"
  | "resume"
  | "forges"
  | "labs"
  | "account"
  | "access";

/** A nav group heading; its label is `nav.groups.<group>`. */
export type SettingsGroup = "Machines" | "Account";

export const SETTINGS_SECTIONS: SettingsSection[] = [
  { key: "general", icon: SettingsIcon },
  { key: "appearance", icon: SunIcon },
  { key: "notifications", icon: BellIcon },
  { key: "chat", icon: MessageSquareIcon },
  { key: "shortcuts", icon: CommandIcon },
  { key: "threads", icon: MessageSquareIcon },
  { key: "schedules", icon: CalendarClockIcon },
  { key: "source-control", icon: GitCommitHorizontalIcon },
  { key: "hosts", icon: ServerIcon, group: "Machines" },
  { key: "harnesses", icon: TerminalIcon, group: "Machines" },
  { key: "worktrees", icon: GitBranchIcon, group: "Machines" },
  { key: "providers", icon: LayersIcon, group: "Machines" },
  { key: "resume", icon: TimerIcon, group: "Machines" },
  { key: "forges", icon: GitPullRequestIcon, group: "Machines" },
  { key: "labs", icon: FlaskConicalIcon, group: "Machines" },
  { key: "account", icon: UserIcon, group: "Account" },
  { key: "access", icon: Share2Icon, group: "Account" },
];

export function settingsPath(section: string): string {
  return `/settings/${section}`;
}
