/**
 * The `/` menu's h4rn actions.
 *
 * These are the same store actions the command palette and the thread header
 * call; the menu never invents a command. On a phone the panel actions open
 * the phone panel sheet instead of a desktop tab, and the IDE action is left
 * out because the phone shell never renders that view. Harness slash commands
 * and skills are absent on purpose: h4rn does not expose a harness command
 * surface to the UI, and it defines no invocation syntax of its own for skills.
 *
 * Labels and hints follow the active language. The English keywords stay, so
 * typing an English word still finds a command in any language.
 */
import type { ThreadSummary } from "@/gen";
import type { CommandSearchItem } from "@/lib/composerMenu";
import { i18n } from "@/lib/i18n";
import type { PhonePanelTab } from "@/lib/phonePanels";
import {
  interruptThread,
  openNewThread,
  setPanelTab,
  toggleTerminal,
  toggleView,
} from "@/lib/store/appStore";

export type ComposerCommandIcon =
  | "changes"
  | "ide"
  | "new-thread"
  | "stop"
  | "subagents"
  | "terminal";

export type ComposerCommand = CommandSearchItem & {
  id: string;
  icon: ComposerCommandIcon;
  /** The command opens a dialog or sheet, so the composer must not steal focus back. */
  opensDialogOrSheet?: boolean;
  run: () => void;
};

function projectName(path: string): string {
  return path.split("/").filter(Boolean).pop() ?? path;
}

export function composerCommands(
  summary: ThreadSummary,
  options: {
    terminalOpen: boolean;
    view: "thread" | "ide";
    /** On a phone, Changes and Subagents belong to the panel sheet. */
    phone?: boolean;
    openPhonePanel?: (tab: PhonePanelTab) => void;
  },
): ComposerCommand[] {
  const commands: ComposerCommand[] = [];
  const openPanel = (tab: PhonePanelTab) => {
    if (options.phone) options.openPhonePanel?.(tab);
    else setPanelTab(tab);
  };

  if (summary.tab === "working") {
    commands.push({
      id: "thread.stop",
      label: i18n.t("composer:commands.stop.label"),
      hint: i18n.t("composer:commands.stop.hint"),
      keywords: "interrupt cancel stop",
      icon: "stop",
      run: () => void interruptThread(summary.thread_id),
    });
  }

  commands.push(
    {
      id: "panel.changes",
      label: i18n.t("composer:commands.changes.label"),
      hint: i18n.t("composer:commands.changes.hint"),
      keywords: "diff files review",
      icon: "changes",
      // On a phone this opens the panel sheet; on desktop it is just a tab.
      opensDialogOrSheet: options.phone === true,
      run: () => openPanel("changes"),
    },
    {
      id: "panel.subagents",
      label: i18n.t("composer:commands.subagents.label"),
      hint: i18n.t("composer:commands.subagents.hint"),
      keywords: "agents delegate tree",
      icon: "subagents",
      opensDialogOrSheet: options.phone === true,
      run: () => openPanel("subagents"),
    },
    {
      id: "terminal.toggle",
      label: options.terminalOpen
        ? i18n.t("composer:commands.terminal.hide")
        : i18n.t("composer:commands.terminal.open"),
      hint: i18n.t("composer:commands.terminal.hint"),
      keywords: "shell command prompt drawer",
      icon: "terminal",
      run: toggleTerminal,
    },
    {
      id: "thread.new",
      label: i18n.t("composer:commands.newThread.label"),
      hint: i18n.t("composer:commands.newThread.hint", {
        project: projectName(summary.workspace_path),
      }),
      keywords: "start create compose",
      icon: "new-thread",
      opensDialogOrSheet: true,
      run: () => openNewThread({ host: summary.host, workspace: summary.workspace_path }),
    },
  );

  // The phone shell never renders the IDE view, so the phone menu does not
  // offer it; choosing it there would only persist a layout it cannot show.
  if (!options.phone) {
    commands.splice(commands.length - 1, 0, {
      id: "view.toggle",
      label:
        options.view === "ide"
          ? i18n.t("composer:commands.ide.back")
          : i18n.t("composer:commands.ide.open"),
      hint: i18n.t("composer:commands.ide.hint", { project: projectName(summary.workspace_path) }),
      keywords: "editor code files project",
      icon: "ide",
      run: toggleView,
    });
  }

  return commands;
}
