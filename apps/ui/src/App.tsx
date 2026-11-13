import { Blobatar } from "@blobatar/react";
import { CommandIcon, InboxIcon, MoonIcon, PlusIcon, SearchIcon, SettingsIcon } from "lucide-react";
import { useState } from "react";
import { ThinkingOrb } from "thinking-orbs";

import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Separator } from "@/components/ui/separator";
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { cn } from "@/lib/utils";

/**
 * Placeholder shell.
 *
 * This file is scaffolding only: it proves the tokens, fonts, generated
 * bindings import path and the motion libraries render. The real three-pane
 * shell lands in Phase 1b.
 */

type PlaceholderThread = {
  id: string;
  title: string;
  host: string;
  harness: string;
  meta: string;
  badge?: "Approval" | "Failed";
};

const threads: PlaceholderThread[] = [
  {
    id: "0199f3c1-0007-7a3e-9d1c-2b6f0a4e8c11",
    title: "Ship the lifecycle state machine",
    host: "atlas",
    harness: "claude-code",
    meta: "Running · 2m",
  },
  {
    id: "0199f3c1-0008-7b41-8e2d-5c7a1b9f3d22",
    title: "Wire the PTY transport",
    host: "atlas",
    harness: "codex",
    meta: "Done · unread · 11m",
  },
  {
    id: "0199f3c1-0009-7c55-a13f-7d8b2c0a4e33",
    title: "Review ADR-004 edge cases",
    host: "basalt",
    harness: "opencode",
    meta: "Waiting · 18m",
    badge: "Approval",
  },
  {
    id: "0199f3c1-000a-7d68-b04a-9e1c3d7f5a44",
    title: "Bump the Tauri plugin set",
    host: "cinder",
    harness: "claude-code",
    meta: "Failed · 1h",
    badge: "Failed",
  },
];

const railItems = [
  { label: "Inbox", icon: InboxIcon, active: true },
  { label: "Search", icon: SearchIcon, active: false },
  { label: "Commands", icon: CommandIcon, active: false },
] as const;

export function App() {
  const [tab, setTab] = useState("working");
  const [selected, setSelected] = useState(threads[0]?.id ?? "");

  return (
    <div className="flex h-screen overflow-hidden bg-paper text-ink">
      <nav
        aria-label="Primary"
        className="flex w-14 shrink-0 flex-col items-center gap-1 bg-stage py-3 text-stage-ink"
      >
        <div className="mb-3 flex size-8 items-center justify-center rounded-sm bg-cue text-[13px] font-semibold text-ink">
          h4
        </div>
        {railItems.map((item) => (
          <Tooltip key={item.label}>
            <TooltipTrigger asChild>
              <button
                type="button"
                aria-label={item.label}
                aria-current={item.active ? "page" : undefined}
                className={cn(
                  "relative flex size-10 items-center justify-center rounded-sm transition-colors duration-[var(--dur-instant)]",
                  item.active
                    ? "bg-white/5 text-stage-ink"
                    : "text-stage-ink-muted hover:bg-white/5 hover:text-stage-ink",
                )}
              >
                {item.active ? (
                  <span className="absolute inset-y-0 left-0 w-0.5 rounded-r-sm bg-cue" />
                ) : null}
                <item.icon className="size-5" />
              </button>
            </TooltipTrigger>
            <TooltipContent side="right">{item.label}</TooltipContent>
          </Tooltip>
        ))}
        <div className="mt-auto flex flex-col items-center gap-1">
          <Tooltip>
            <TooltipTrigger asChild>
              <button
                type="button"
                aria-label="Toggle theme"
                className="flex size-10 items-center justify-center rounded-sm text-stage-ink-muted transition-colors duration-[var(--dur-instant)] hover:bg-white/5 hover:text-stage-ink"
              >
                <MoonIcon className="size-5" />
              </button>
            </TooltipTrigger>
            <TooltipContent side="right">Theme</TooltipContent>
          </Tooltip>
          <Tooltip>
            <TooltipTrigger asChild>
              <button
                type="button"
                aria-label="Settings"
                className="flex size-10 items-center justify-center rounded-sm text-stage-ink-muted transition-colors duration-[var(--dur-instant)] hover:bg-white/5 hover:text-stage-ink"
              >
                <SettingsIcon className="size-5" />
              </button>
            </TooltipTrigger>
            <TooltipContent side="right">Settings</TooltipContent>
          </Tooltip>
          <Blobatar name="user:local" size={24} background="circle" title="You" />
        </div>
      </nav>

      <section className="flex w-[304px] shrink-0 flex-col border-r border-line bg-panel">
        <header className="flex h-12 items-center justify-between px-3">
          <h1 className="text-headline">Inbox</h1>
          <Button size="icon" aria-label="New thread">
            <PlusIcon />
          </Button>
        </header>
        <div className="px-3">
          <Tabs value={tab} onValueChange={setTab}>
            <TabsList className="w-full">
              <TabsTrigger value="working">Working · 2</TabsTrigger>
              <TabsTrigger value="done">Done · 1</TabsTrigger>
              <TabsTrigger value="settled">Settled · 1</TabsTrigger>
            </TabsList>
          </Tabs>
        </div>
        <Separator className="mt-2" />
        <ul className="flex-1 overflow-y-auto">
          {threads.map((thread) => (
            <li key={thread.id}>
              <button
                type="button"
                onClick={() => setSelected(thread.id)}
                className={cn(
                  "relative flex h-12 w-full flex-col justify-center gap-0.5 border-b border-line px-3 text-left transition-colors duration-[var(--dur-instant)] hover:bg-paper",
                  selected === thread.id && "bg-paper",
                )}
              >
                {selected === thread.id ? (
                  <span className="absolute inset-y-0 left-0 w-0.5 bg-cue" />
                ) : null}
                <span className="truncate text-compact font-medium text-ink">{thread.title}</span>
                <span className="flex items-center gap-1.5 text-micro text-ink-muted">
                  <Blobatar name={`host:${thread.host}`} size={12} background="squircle" />
                  {thread.host}
                  <span aria-hidden="true">/</span>
                  <code className="font-mono">{thread.harness}</code>
                  <span className="ml-auto tabular">{thread.meta}</span>
                  {thread.badge ? (
                    <Badge variant={thread.badge === "Failed" ? "strike" : "cue"}>
                      {thread.badge}
                    </Badge>
                  ) : null}
                </span>
              </button>
            </li>
          ))}
        </ul>
      </section>

      <main className="flex min-w-0 flex-1 flex-col">
        <header className="flex h-12 shrink-0 items-center gap-2 border-b border-line px-4">
          <h2 className="truncate text-title">
            {threads.find((thread) => thread.id === selected)?.title ?? "Nothing selected"}
          </h2>
          <Badge variant="standby" className="ml-auto">
            Phase 1a
          </Badge>
        </header>
        <div className="flex flex-1 flex-col items-center justify-center gap-4 px-6 text-center">
          <ThinkingOrb state="working" size={64} aria-label="Harness is working" />
          <div className="max-w-[46ch] space-y-1">
            <p className="text-title">Scaffold only</p>
            <p className="text-compact text-ink-muted">
              Protocol bindings, lifecycle core, harness trait and the shell are in place. The real
              thread stream, cue gutter and standby bar arrive in Phase 1b.
            </p>
          </div>
          <div className="flex items-center gap-2">
            <Button>Open prototype</Button>
            <Button variant="secondary">Read the ADRs</Button>
          </div>
        </div>
      </main>
    </div>
  );
}
