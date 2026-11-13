import {
  ChartNoAxesColumnIcon,
  Code2Icon,
  GitPullRequestIcon,
  LibraryIcon,
  SettingsIcon,
} from "lucide-react";

import { EmptyState } from "@/components/common/EmptyState";
import { Button } from "@/components/ui/button";
import { navigate } from "@/lib/store/appStore";

const SURFACES = {
  library: {
    icon: LibraryIcon,
    title: "Library",
    description:
      "Share MCP servers and skills across hosts and harnesses, then see which targets can actually receive them.",
  },
  usage: {
    icon: ChartNoAxesColumnIcon,
    title: "Usage & limits",
    description:
      "Tokens, cost, TPS and runtime per host, plus reset countdowns and the auto-resume policy.",
  },
  ide: {
    icon: Code2Icon,
    title: "IDE",
    description:
      "Read and edit files on a host, diff against HEAD or the agent's changes, and hand a selection back to the thread.",
  },
  pulls: {
    icon: GitPullRequestIcon,
    title: "Pull requests",
    description:
      "Triage pull requests across forges and repositories, read checks, and hand a comment to an agent.",
  },
  settings: {
    icon: SettingsIcon,
    title: "Settings",
    description:
      "Account and relay, devices, notifications, appearance and remappable keyboard shortcuts.",
  },
} as const;

export function PlaceholderPane({ surface }: { surface: string }) {
  const entry = SURFACES[surface as keyof typeof SURFACES] ?? {
    icon: LibraryIcon,
    title: "h4rn",
    description: "This surface lands after Phase 1b.",
  };

  return (
    <main className="flex h-full min-h-0 flex-1 flex-col bg-bg">
      <header className="flex h-12 shrink-0 items-center justify-between border-b border-line px-4">
        <h1 className="text-headline">{entry.title}</h1>
        <Button variant="secondary" size="sm" onClick={() => navigate({ kind: "inbox" })}>
          Back to inbox
        </Button>
      </header>
      <EmptyState
        icon={entry.icon}
        title={`${entry.title} is not wired up yet.`}
        description={`${entry.description} Phase 1b covers the inbox, thread stream and approvals; this surface follows once the daemon exposes its data.`}
        className="flex-1"
        action={<Button onClick={() => navigate({ kind: "inbox" })}>Open inbox</Button>}
      />
    </main>
  );
}
