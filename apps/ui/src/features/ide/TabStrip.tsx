import { XIcon } from "lucide-react";
import { useTranslation } from "react-i18next";

import { Tabs, TabsList, TabsTab } from "@/components/ui/tabs";
import { type IdeTab, isDirty } from "./tabs";

type TabStripProps = {
  tabs: IdeTab[];
  activePath: string | null;
  /** Quiet hint after a project switch, e.g. "Restored 3 tabs". */
  note?: string | null;
  onActivate: (path: string) => void;
  onClose: (path: string) => void;
  onReorder: (path: string, index: number) => void;
};

export function TabStrip({
  tabs,
  activePath,
  note = null,
  onActivate,
  onClose,
  onReorder,
}: TabStripProps) {
  const { t } = useTranslation("ide");
  if (tabs.length === 0) return null;

  return (
    <Tabs
      className="flex-row"
      value={activePath ?? null}
      onValueChange={(value) => {
        if (value) onActivate(value);
      }}
    >
      <TabsList variant="bare" aria-label={t("tabs.label")} className="q-etabs min-w-0 flex-1">
        {tabs.map((tab, index) => {
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
              className="q-ttab-wrap"
            >
              <TabsTab
                value={tab.path}
                draggable
                onDragStart={(event) => {
                  event.dataTransfer.setData("text/plain", tab.path);
                  event.dataTransfer.effectAllowed = "move";
                }}
                className="q-etab"
                title={tab.path}
              >
                <span className="q-mono" style={{ fontSize: 12 }}>
                  {tab.name}
                </span>
                {dirty ? (
                  <span
                    role="img"
                    aria-label={t("tabs.unsaved")}
                    title={t("tabs.unsaved")}
                    className="dirty"
                  />
                ) : null}
              </TabsTab>
              <button
                type="button"
                aria-label={t("tabs.close", { name: tab.name })}
                onClick={() => onClose(tab.path)}
                className="q-ttab-x"
              >
                <XIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
              </button>
            </div>
          );
        })}
        <span style={{ flex: 1 }} />
        {note ? <span className="q-faint q-etabs-note">{note}</span> : null}
      </TabsList>
    </Tabs>
  );
}
