/**
 * The `/` menu's h4rn actions.
 *
 * These are the same store actions the command palette and the thread header
 * call; the menu never invents a command. On a phone the panel actions open
 * the phone panel sheet instead of a desktop tab, and the IDE action is left
 * out because the phone shell never renders that view. Harness slash commands
 * and skills are absent on purpose: h4rn does not expose a harness command
 * surface to the UI, and it defines no invocation syntax of its own for skills.
 */
import type { ThreadSummary } from "@/gen";
import type { CommandSearchItem } from "@/lib/composerMenu";
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
      label: "Stop the turn",
      hint: "Interrupt what the agent is doing",
      keywords: "interrupt cancel stop",
      icon: "stop",
      run: () => void interruptThread(summary.thread_id),
    });
  }

  commands.push(
    {
      id: "panel.changes",
      label: "Open changes",
      hint: "Diff the files this thread touched",
      keywords: "diff files review",
      icon: "changes",
      run: () => openPanel("changes"),
    },
    {
      id: "panel.subagents",
      label: "Open subagents",
      hint: "See delegated work and its budget",
      keywords: "agents delegate tree",
      icon: "subagents",
      run: () => openPanel("subagents"),
    },
    {
      id: "terminal.toggle",
      label: options.terminalOpen ? "Hide the terminal" : "Open the terminal",
      hint: "The shell in this thread's workspace",
      keywords: "shell command prompt drawer",
      icon: "terminal",
      run: toggleTerminal,
    },
    {
      id: "thread.new",
      label: "New thread",
      hint: `Start one in ${projectName(summary.workspace_path)}`,
      keywords: "start create compose",
      icon: "new-thread",
      run: () => openNewThread({ host: summary.host, workspace: summary.workspace_path }),
    },
  );

  // The phone shell never renders the IDE view, so the phone menu does not
  // offer it; choosing it there would only persist a layout it cannot show.
  if (!options.phone) {
    commands.splice(commands.length - 1, 0, {
      id: "view.toggle",
      label: options.view === "ide" ? "Back to the thread" : "Open the IDE",
      hint: `Edit ${projectName(summary.workspace_path)}`,
      keywords: "editor code files project",
      icon: "ide",
      run: toggleView,
    });
  }

  return commands;
}
