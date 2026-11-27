import { CaseSensitiveIcon, Loader2Icon, RegexIcon, SearchIcon } from "lucide-react";
import { type CSSProperties, useEffect, useMemo, useRef } from "react";
import { useTranslation } from "react-i18next";

import { Tip } from "@/components/ui/tooltip";
import type { FsSearchMatch } from "@/gen";
import { formatNumber } from "@/lib/i18n";
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

/**
 * One status bar figure. The panel is narrow and the bar is one fixed-height
 * line, so a long translation ends in an ellipsis with the whole text on hover
 * instead of pushing past the bar's edge.
 */
function StatusText({ text, style }: { text: string; style?: CSSProperties }) {
  return (
    <span
      title={text}
      style={{
        minWidth: 0,
        overflow: "hidden",
        textOverflow: "ellipsis",
        whiteSpace: "nowrap",
        ...style,
      }}
    >
      {text}
    </span>
  );
}

export function SearchPanel({ search, onQuery, onOption, onRun, onOpen }: SearchPanelProps) {
  const { t } = useTranslation("ide");
  const input = useRef<HTMLInputElement | null>(null);
  const groups = useMemo(() => group(search.results), [search.results]);

  useEffect(() => {
    input.current?.focus();
  }, []);

  return (
    <section
      aria-label={t("search.label")}
      style={{
        display: "flex",
        flexDirection: "column",
        minHeight: 0,
        flex: 1,
        background: "var(--bg)",
      }}
    >
      <div
        style={{ flex: "none", padding: "12px 12px 10px", borderBottom: "1px solid var(--line)" }}
      >
        <div className="q-side-search" style={{ margin: 0 }}>
          <SearchIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
          <input
            ref={input}
            value={search.query}
            onChange={(event) => onQuery(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === "Enter") onRun();
              if (event.key === "Escape") onQuery("");
            }}
            placeholder={t("search.find")}
            aria-label={t("search.find")}
            spellCheck={false}
            className="q-mono"
            style={{
              flex: 1,
              minWidth: 0,
              background: "transparent",
              border: 0,
              outline: 0,
              color: "var(--fg)",
              fontSize: 12,
            }}
          />
          {search.running ? (
            <Loader2Icon className="q-icon s14 animate-spin q-faint" aria-hidden />
          ) : null}
        </div>
        <div style={{ display: "flex", alignItems: "center", gap: 4, marginTop: 8 }}>
          <Tip label={t("search.matchCase")}>
            <button
              type="button"
              aria-pressed={search.caseSensitive}
              onClick={() => onOption({ caseSensitive: !search.caseSensitive })}
              className={cn("q-ibtn", search.caseSensitive && "on")}
              style={{ width: 24, height: 24 }}
            >
              <CaseSensitiveIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
            </button>
          </Tip>
          <Tip label={t("search.regex")}>
            <button
              type="button"
              aria-pressed={search.regex}
              onClick={() => onOption({ regex: !search.regex })}
              className={cn("q-ibtn", search.regex && "on")}
              style={{ width: 24, height: 24 }}
            >
              <RegexIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
            </button>
          </Tip>
          <input
            value={search.glob ?? ""}
            onChange={(event) => onOption({ glob: event.target.value || null })}
            placeholder="*.ts"
            aria-label={t("search.glob")}
            spellCheck={false}
            className="q-mono"
            style={{
              height: 24,
              width: 88,
              borderRadius: 6,
              border: "1px solid var(--line)",
              background: "var(--bg-sunk)",
              padding: "0 6px",
              fontSize: 12,
              color: "var(--fg)",
              outline: 0,
            }}
          />
          <button
            type="button"
            onClick={onRun}
            className="q-btn secondary"
            style={{ marginLeft: "auto", height: 24 }}
          >
            {t("search.run")}
          </button>
        </div>
      </div>

      {search.error ? (
        <div style={{ margin: 12, fontSize: 12, color: "var(--fg-2)" }}>
          <p style={{ margin: 0, fontWeight: 500, color: "var(--fg)" }}>{t("search.failed")}</p>
          <p style={{ margin: "4px 0 0" }}>{search.error.message}</p>
        </div>
      ) : null}

      <div className="q-scroll">
        {groups.length === 0 && !search.running ? (
          <p
            className="q-faint"
            style={{ margin: 0, padding: 16, fontSize: 12, lineHeight: "18px" }}
          >
            {search.searchId ? t("search.noMatches") : t("search.intro")}
          </p>
        ) : null}
        {groups.map((group) => (
          <div key={group.path} style={{ borderBottom: "1px solid var(--line)" }}>
            <div
              style={{
                display: "flex",
                alignItems: "center",
                gap: 8,
                height: 28,
                padding: "0 12px",
                background: "var(--bg-sunk)",
              }}
            >
              <span
                className="q-mono"
                style={{
                  flex: 1,
                  minWidth: 0,
                  overflow: "hidden",
                  textOverflow: "ellipsis",
                  whiteSpace: "nowrap",
                  fontSize: 12,
                  color: "var(--fg-2)",
                }}
              >
                {group.path}
              </span>
              <span className="q-num q-faint" style={{ fontSize: 12 }}>
                {formatNumber(group.matches.length)}
              </span>
            </div>
            <ul style={{ listStyle: "none", margin: 0, padding: 0 }}>
              {group.matches.map((match) => (
                <li key={`${match.line_number}-${match.column}`}>
                  <button
                    type="button"
                    onClick={() => onOpen(match.path, match.line_number)}
                    style={{
                      display: "flex",
                      width: "100%",
                      alignItems: "baseline",
                      gap: 8,
                      padding: "3px 12px",
                      textAlign: "left",
                    }}
                    className="q-hover-row"
                  >
                    <span
                      className="q-mono q-faint"
                      style={{ width: 32, flex: "none", textAlign: "right", fontSize: 12 }}
                    >
                      {formatNumber(match.line_number, { useGrouping: false })}
                    </span>
                    <span
                      className="q-mono"
                      style={{
                        flex: 1,
                        minWidth: 0,
                        overflow: "hidden",
                        textOverflow: "ellipsis",
                        whiteSpace: "nowrap",
                        fontSize: 12,
                        color: "var(--fg-2)",
                      }}
                    >
                      {match.preview}
                    </span>
                  </button>
                </li>
              ))}
            </ul>
          </div>
        ))}
      </div>

      <div
        className="q-statusbar"
        style={{ justifyContent: "space-between", fontFamily: "var(--mono)" }}
      >
        <StatusText text={t("search.matches", { count: search.results.length })} />
        {search.truncated ? (
          <StatusText text={t("search.capped")} style={{ color: "var(--warn)" }} />
        ) : null}
      </div>
    </section>
  );
}
