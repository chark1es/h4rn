import { ENTER, ESC, KeyCaps, SHIFT, ShortcutCaps } from "@/components/common/Kbd";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { COMMAND_GROUPS, COMMANDS } from "@/lib/keybindings";
import { closeShortcuts, navigate, useApp } from "@/lib/store/appStore";

/** Keys that edit text rather than run commands. */
const TYPING: Array<[string, string[]]> = [
  ["Send", [ENTER]],
  ["New line", [SHIFT, ENTER]],
  ["Mention a file or host", ["@"]],
  ["Close a dialog or go back", [ESC]],
];

export function ShortcutsDialog() {
  const open = useApp((state) => state.ui.shortcutsOpen);

  return (
    <Dialog open={open} onOpenChange={(value) => (value ? undefined : closeShortcuts())}>
      <DialogContent
        className="sm:max-w-[680px]"
        onOpenAutoFocus={(event) => event.preventDefault()}
      >
        <DialogHeader>
          <DialogTitle>Keyboard shortcuts</DialogTitle>
          <DialogDescription>
            Every command is Cmd or Ctrl plus a key, so typing never triggers one. Change any of
            them in{" "}
            <button
              type="button"
              className="q-link"
              onClick={() => {
                closeShortcuts();
                navigate({
                  kind: "placeholder",
                  surface: "settings",
                  path: "/settings/shortcuts",
                });
              }}
            >
              Settings → Shortcuts
            </button>
            .
          </DialogDescription>
        </DialogHeader>

        <div className="grid gap-x-6 gap-y-4 sm:grid-cols-2">
          {COMMAND_GROUPS.map((group) => (
            <section key={group} className="space-y-1">
              <h3 className="text-[12px] font-medium text-[var(--fg-3)]">{group}</h3>
              <dl className="divide-y divide-line/70">
                {COMMANDS.filter((command) => command.group === group).map((command) => (
                  <div key={command.id} className="flex items-center justify-between gap-2 py-1">
                    <dt className="text-[12.5px] text-[var(--fg-2)]">{command.label}</dt>
                    <dd className="q-pal-kbd shrink-0">
                      <ShortcutCaps command={command.id} />
                    </dd>
                  </div>
                ))}
              </dl>
            </section>
          ))}
          <section className="space-y-1">
            <h3 className="text-[12px] font-medium text-[var(--fg-3)]">While typing</h3>
            <dl className="divide-y divide-line/70">
              {TYPING.map(([label, keys]) => (
                <div key={label} className="flex items-center justify-between gap-2 py-1">
                  <dt className="text-[12.5px] text-[var(--fg-2)]">{label}</dt>
                  <dd className="q-pal-kbd shrink-0">
                    <KeyCaps value={keys.join(" ")} />
                  </dd>
                </div>
              ))}
            </dl>
          </section>
        </div>
      </DialogContent>
    </Dialog>
  );
}
