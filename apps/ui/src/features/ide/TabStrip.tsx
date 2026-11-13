import { XIcon } from "lucide-react";

import { cn } from "@/lib/utils";
import { type IdeTab, isDirty } from "./tabs";

type TabStripProps = {
  tabs: IdeTab[];
  activePath: string | null;
  onActivate: (path: string) => void;
  onClose: (path: string) => void;
  onReorder: (path: string, index: number) => void;
};

export function TabStrip({ tabs, activePath, onActivate, onClose, onReorder }: TabStripProps) {
  if (tabs.length === 0) return null;

  return (
    <div
      role="tablist"
      aria-label="Open files"
      className="flex h-9 shrink-0 items-stretch overflow-x-auto border-b border-line bg-panel"
    >
      {tabs.map((tab, index) => {
        const active = tab.path === activePath;
        const dirty = isDirty(tab);
        return (
          // Biome's a11y rules want the interactive element to be a button; the
          // tab itself is a button and drag is a pointer affordance on the row.
          // biome-ignore lint/a11y/noStaticElementInteractions: the row is a drop target; the tab and close controls inside are real buttons.
          <div
            key={tab.path}
            onDragOver={(event) => {
              event.preventDefault();
              event.dataTransfer.dropEffect = "move";
            }}
            onDrop={(event) => {
              event.preventDefault();
              const from = event.dataTransfer.getData("text/plain");
              if (from) onReorder(from, index);
            }}
            className={cn(
              "group flex shrink-0 items-center border-r border-line border-b-2 border-b-transparent",
              active ? "border-b-cue bg-paper" : "bg-panel hover:bg-raised",
            )}
          >
            <button
              type="button"
              role="tab"
              aria-selected={active}
              draggable
              onDragStart={(event) => {
                event.dataTransfer.setData("text/plain", tab.path);
                event.dataTransfer.effectAllowed = "move";
              }}
              onClick={() => onActivate(tab.path)}
              className={cn(
                "flex h-full max-w-[220px] items-center gap-1.5 pl-3 text-compact",
                active ? "text-ink" : "text-ink-muted hover:text-ink",
                tabs.length > 1 ? "pr-1" : "pr-3",
              )}
              title={tab.path}
            >
              <span className="truncate font-mono text-[11px]">{tab.name}</span>
              {dirty ? (
                <span
                  role="img"
                  aria-label="Unsaved changes"
                  title="Unsaved changes"
                  className="size-1.5 shrink-0 rounded-full bg-cue"
                />
              ) : null}
            </button>
            <button
              type="button"
              aria-label={`Close ${tab.name}`}
              onClick={() => onClose(tab.path)}
              className={cn(
                "mr-1 grid size-5 shrink-0 place-items-center rounded-sm text-ink-faint",
                "opacity-0 transition-opacity duration-[var(--dur-instant)] group-hover:opacity-100",
                "hover:bg-panel hover:text-ink focus-visible:opacity-100 focus-visible:ring-1 focus-visible:ring-ring focus-visible:outline-none",
                active && "opacity-100 md:opacity-0 md:group-hover:opacity-100",
              )}
            >
              <XIcon className="size-3" aria-hidden />
            </button>
          </div>
        );
      })}
    </div>
  );
}
