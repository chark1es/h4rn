import { ALT, BACKSPACE, ENTER, ESC, Kbd, MOD, SHIFT } from "@/components/common/Kbd";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { closeShortcuts, useApp } from "@/lib/store/appStore";

const GROUPS: Array<{ title: string; rows: Array<[string, string]> }> = [
  {
    title: "Global",
    rows: [
      ["Command palette", `${MOD} K`],
      ["Shortcut sheet", "?"],
      ["New thread", `${MOD} ${SHIFT} N`],
      ["Toggle theme", `${MOD} ${SHIFT} L`],
      ["Settings", `${MOD} ,`],
      ["Close overlay", ESC],
    ],
  },
  {
    title: "Inbox",
    rows: [
      ["Focus search", "/"],
      ["Next / previous row", "J / K"],
      ["Open row", ENTER],
      ["Working / Done / Settled", `${ALT} 1 / 2 / 3`],
      ["Acknowledge", "A"],
      ["Snooze", "S"],
      ["Keep in Working", "P"],
      ["Go to Inbox / Hosts", "G I / G H"],
    ],
  },
  {
    title: "Thread",
    rows: [
      ["Reply / focus composer", "R"],
      ["Approve / Go", `${MOD} ${ENTER}`],
      ["Deny / Hold", `${MOD} ${BACKSPACE}`],
      ["Interrupt turn", `${MOD} .`],
      ["Settle now", `${SHIFT} S`],
      ["Previous / next cue", "[ / ]"],
      ["Terminal / Diff / Files", "T / D / F"],
    ],
  },
  {
    title: "Composer",
    rows: [
      ["Send", ENTER],
      ["Newline", `${SHIFT} ${ENTER}`],
      ["Mention file or host", "@"],
      ["Slash command", "/"],
      ["Attach (Phase 2)", `${MOD} U`],
    ],
  },
];

export function ShortcutsDialog() {
  const open = useApp((state) => state.ui.shortcutsOpen);

  return (
    <Dialog open={open} onOpenChange={(value) => (value ? undefined : closeShortcuts())}>
      <DialogContent className="max-h-[85dvh] overflow-y-auto sm:max-w-[620px]">
        <DialogHeader>
          <DialogTitle>Keyboard shortcuts</DialogTitle>
          <DialogDescription>
            Every pointer action has a key. Sequences use G, then a letter, within 900 ms.
          </DialogDescription>
        </DialogHeader>

        <div className="grid gap-4 sm:grid-cols-2">
          {GROUPS.map((group) => (
            <section key={group.title} className="space-y-1">
              <h3 className="text-[12px] font-medium text-[var(--fg-3)]">{group.title}</h3>
              <dl className="divide-y divide-line/70">
                {group.rows.map(([label, keys]) => (
                  <div key={label} className="flex items-center justify-between gap-2 py-1">
                    <dt className="text-[12.5px] text-[var(--fg-2)]">{label}</dt>
                    <dd className="flex shrink-0 items-center gap-1">
                      {keys.split(" / ").map((part) => (
                        <Kbd key={part}>{part}</Kbd>
                      ))}
                    </dd>
                  </div>
                ))}
              </dl>
            </section>
          ))}
        </div>

        <p className="text-[11.5px] text-[var(--fg-3)]">
          Browser-reserved combinations (Ctrl/Cmd N, T, W) are not intercepted. On the web build,
          use the rail or palette for those routes.
        </p>
      </DialogContent>
    </Dialog>
  );
}
