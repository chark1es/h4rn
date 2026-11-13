import { CaseSensitiveIcon, Loader2Icon, RegexIcon, SearchIcon } from "lucide-react";
import { useEffect, useMemo, useRef } from "react";

import type { FsSearchMatch } from "@/gen";
import { cn } from "@/lib/utils";
import type { SearchState } from "./useIdeSession";

type SearchPanelProps = {
  search: SearchState;
  onQuery: (query: string) => void;
  onOption: (patch: Partial<Pick<SearchState, "regex" | "caseSensitive" | "glob">>) => void;
  onRun: () => void;
  onOpen: (path: string, line: number) => void;
};

function group(matches: FsSearchMatch[]): Array<{ path: string; matches: FsSearchMatch[] }> {
  const groups = new Map<string, FsSearchMatch[]>();
  for (const match of matches) {
    const list = groups.get(match.path);
    if (list) list.push(match);
    else groups.set(match.path, [match]);
  }
  return [...groups.entries()].map(([path, grouped]) => ({ path, matches: grouped }));
}

export function SearchPanel({ search, onQuery, onOption, onRun, onOpen }: SearchPanelProps) {
  const input = useRef<HTMLInputElement | null>(null);
  const groups = useMemo(() => group(search.results), [search.results]);

  useEffect(() => {
    input.current?.focus();
  }, []);

  return (
    <section aria-label="Search files" className="flex min-h-0 flex-1 flex-col bg-paper">
      <header className="shrink-0 border-b border-line px-3 py-2">
        <div className="flex items-center gap-1.5 rounded-sm border border-control-border bg-raised px-2 focus-within:ring-1 focus-within:ring-ring">
          <SearchIcon className="size-3.5 shrink-0 text-ink-faint" aria-hidden />
          <input
            ref={input}
            value={search.query}
            onChange={(event) => onQuery(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === "Enter") onRun();
              if (event.key === "Escape") onQuery("");
            }}
            placeholder="Find in workspace"
            aria-label="Find in workspace"
            spellCheck={false}
            className="h-8 min-w-0 flex-1 bg-transparent font-mono text-[11px] text-ink outline-none placeholder:text-ink-faint"
          />
          {search.running ? (
            <Loader2Icon className="size-3.5 animate-spin text-ink-faint" aria-hidden />
          ) : null}
        </div>
        <div className="mt-2 flex items-center gap-1">
          <button
            type="button"
            aria-pressed={search.caseSensitive}
            onClick={() => onOption({ caseSensitive: !search.caseSensitive })}
            title="Match case"
            className={cn(
              "grid size-6 place-items-center rounded-[2px] border text-[11px]",
              search.caseSensitive
                ? "border-cue text-cue-ink"
                : "border-transparent text-ink-faint hover:bg-panel hover:text-ink",
            )}
          >
            <CaseSensitiveIcon className="size-3.5" aria-hidden />
            <span className="sr-only">Match case</span>
          </button>
          <button
            type="button"
            aria-pressed={search.regex}
            onClick={() => onOption({ regex: !search.regex })}
            title="Regular expression"
            className={cn(
              "grid size-6 place-items-center rounded-[2px] border",
              search.regex
                ? "border-cue text-cue-ink"
                : "border-transparent text-ink-faint hover:bg-panel hover:text-ink",
            )}
          >
            <RegexIcon className="size-3.5" aria-hidden />
            <span className="sr-only">Regular expression</span>
          </button>
          <input
            value={search.glob ?? ""}
            onChange={(event) => onOption({ glob: event.target.value || null })}
            placeholder="*.ts"
            aria-label="File glob"
            spellCheck={false}
            className="h-6 w-24 rounded-[2px] border border-line bg-panel px-1.5 font-mono text-[10px] text-ink outline-none placeholder:text-ink-faint focus-visible:border-cue"
          />
          <button
            type="button"
            onClick={onRun}
            className="ml-auto h-6 rounded-[2px] bg-cue-tint px-2 text-[10px] font-medium text-cue-ink hover:brightness-95"
          >
            Search
          </button>
        </div>
      </header>

      {search.error ? (
        <div className="m-3 rounded-sm border border-line bg-raised p-3 text-compact">
          <p className="font-medium text-ink">Search failed.</p>
          <p className="mt-1 text-ink-muted">{search.error.message}</p>
        </div>
      ) : null}

      <div className="min-h-0 flex-1 overflow-auto">
        {groups.length === 0 && !search.running ? (
          <p className="p-4 text-compact text-ink-muted">
            {search.searchId
              ? "No matches in this workspace."
              : "Type a pattern, then Enter. Results stream in as the daemon walks the tree."}
          </p>
        ) : null}
        {groups.map((group) => (
          <div key={group.path} className="border-b border-line/70">
            <div className="flex h-7 items-center gap-2 bg-panel px-3">
              <span className="min-w-0 flex-1 truncate font-mono text-[10px] text-ink-muted">
                {group.path}
              </span>
              <span className="font-mono text-[10px] text-ink-faint">{group.matches.length}</span>
            </div>
            <ul>
              {group.matches.map((match) => (
                <li key={`${match.line_number}-${match.column}`}>
                  <button
                    type="button"
                    onClick={() => onOpen(match.path, match.line_number)}
                    className="flex w-full items-baseline gap-2 px-3 py-1 text-left hover:bg-raised"
                  >
                    <span className="w-8 shrink-0 text-right font-mono text-[10px] text-ink-faint">
                      {match.line_number}
                    </span>
                    <span className="min-w-0 flex-1 truncate font-mono text-[11px] text-ink-muted">
                      {match.preview}
                    </span>
                  </button>
                </li>
              ))}
            </ul>
          </div>
        ))}
      </div>

      <footer className="flex h-7 shrink-0 items-center justify-between border-t border-line bg-panel px-3 font-mono text-[10px] text-ink-faint">
        <span>
          {search.results.length} {search.results.length === 1 ? "match" : "matches"}
        </span>
        {search.truncated ? <span className="text-cue-ink">Result cap reached</span> : null}
      </footer>
    </section>
  );
}
