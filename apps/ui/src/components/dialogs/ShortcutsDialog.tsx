import { Trans, useTranslation } from "react-i18next";
import {
  Dialog,
  DialogBody,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { ENTER, ESC, KeyCaps, SHIFT, ShortcutCaps } from "@/components/ui/kbd";

import { COMMAND_GROUPS, COMMANDS, commandGroupName, commandName } from "@/lib/keybindings";
import { closeShortcuts, navigate, useApp } from "@/lib/store/appStore";

type TypingKey =
  | "send"
  | "newLine"
  | "recall"
  | "forward"
  | "mention"
  | "command"
  | "moveMenu"
  | "insert"
  | "closeMenu"
  | "closeDialog";

/** Keys that edit text rather than run commands. Keycaps read the same in every language. */
const TYPING: Array<[TypingKey, string[]]> = [
  ["send", [ENTER]],
  ["newLine", [SHIFT, ENTER]],
  ["recall", ["↑"]],
  ["forward", ["↓"]],
  ["mention", ["@"]],
  ["command", ["/"]],
  ["moveMenu", ["↑", "↓"]],
  ["insert", [ENTER, "Tab"]],
  ["closeMenu", [ESC]],
  ["closeDialog", [ESC]],
];

export function ShortcutsDialog() {
  const open = useApp((state) => state.ui.shortcutsOpen);
  const { t } = useTranslation("commands");

  return (
    <Dialog open={open} onOpenChange={(value) => (value ? undefined : closeShortcuts())}>
      <DialogContent size="lg" initialFocus="content">
        <DialogHeader>
          <DialogTitle>{commandName("shortcuts.open")}</DialogTitle>
          <DialogDescription>
            <Trans
              t={t}
              i18nKey="shortcutsDialog.description"
              components={{
                settings: (
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
                  />
                ),
              }}
            />
          </DialogDescription>
        </DialogHeader>

        <DialogBody className="grid gap-x-6 gap-y-5 sm:grid-cols-2">
          {COMMAND_GROUPS.map((group) => (
            <section key={group} className="flex flex-col gap-1">
              <h3 className="m-0 text-xs font-medium text-muted-foreground">
                {commandGroupName(group)}
              </h3>
              <dl className="m-0 divide-y divide-border">
                {COMMANDS.filter((command) => command.group === group).map((command) => (
                  <div key={command.id} className="flex h-8 items-center justify-between gap-2">
                    <dt className="truncate text-sm text-fg-2">{commandName(command.id)}</dt>
                    <dd className="m-0 shrink-0">
                      <ShortcutCaps command={command.id} />
                    </dd>
                  </div>
                ))}
              </dl>
            </section>
          ))}
          <section className="flex flex-col gap-1">
            <h3 className="m-0 text-xs font-medium text-muted-foreground">
              {t("shortcutsDialog.whileTyping")}
            </h3>
            <dl className="m-0 divide-y divide-border">
              {TYPING.map(([key, keys]) => (
                <div key={key} className="flex h-8 items-center justify-between gap-2">
                  <dt className="truncate text-sm text-fg-2">
                    {t(`shortcutsDialog.typing.${key}`)}
                  </dt>
                  <dd className="m-0 shrink-0">
                    <KeyCaps value={keys.join(" ")} />
                  </dd>
                </div>
              ))}
            </dl>
          </section>
        </DialogBody>
      </DialogContent>
    </Dialog>
  );
}
