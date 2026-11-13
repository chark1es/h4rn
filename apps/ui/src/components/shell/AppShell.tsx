import { CommandIcon, InboxIcon, PlusIcon } from "lucide-react";
import type * as React from "react";

import { EmptyState } from "@/components/common/EmptyState";
import { CommandPalette } from "@/components/dialogs/CommandPalette";
import { NewThreadDialog } from "@/components/dialogs/NewThreadDialog";
import { ShortcutsDialog } from "@/components/dialogs/ShortcutsDialog";
import { HostsPane } from "@/components/hosts/HostsPane";
import { InboxPane } from "@/components/inbox/InboxPane";
import { PlaceholderPane } from "@/components/placeholder/PlaceholderPane";
import { OfflineBanner } from "@/components/shell/OfflineBanner";
import { PhoneNav, PhoneTopBar } from "@/components/shell/PhoneNav";
import { Rail } from "@/components/shell/Rail";
import { StatusBar } from "@/components/shell/StatusBar";
import { Toasts } from "@/components/shell/Toasts";
import { ThreadPane } from "@/components/thread/ThreadPane";
import { Button } from "@/components/ui/button";
import { useIsPhone, useIsWide } from "@/hooks/use-media-query";
import { openNewThread, setUi, useApp } from "@/lib/store/appStore";

function InboxDetailEmpty() {
  const count = useApp((state) => Object.keys(state.threads).length);
  return (
    <main className="flex h-full min-h-0 flex-1 flex-col bg-paper">
      <header className="flex h-14 shrink-0 items-center border-b border-line px-4">
        <h2 className="text-title text-ink">Thread</h2>
      </header>
      <EmptyState
        className="flex-1"
        icon={InboxIcon}
        title={count === 0 ? "No thread is open." : "Choose a thread from the list."}
        description="The stream, tool calls and approvals live here. Opening a thread replaces this pane and keeps the list in place."
        action={
          <>
            <Button onClick={openNewThread}>
              <PlusIcon aria-hidden />
              New thread
            </Button>
            <Button variant="secondary" onClick={() => setUi({ paletteOpen: true })}>
              <CommandIcon aria-hidden />
              Commands
            </Button>
          </>
        }
      />
    </main>
  );
}

export function AppShell() {
  const route = useApp((state) => state.ui.route);
  const isPhone = useIsPhone();
  const isWide = useIsWide();
  const isThread = route.kind === "thread";

  const surface =
    route.kind === "hosts" ? (
      <HostsPane />
    ) : route.kind === "placeholder" ? (
      <PlaceholderPane surface={route.surface} />
    ) : null;

  let body: React.ReactNode;
  if (isPhone) {
    body = isThread ? (
      <ThreadPane threadId={route.threadId} />
    ) : (
      <div className="flex min-h-0 flex-1 flex-col">
        {surface ?? <InboxPane />}
        <PhoneNav />
      </div>
    );
  } else if (!isWide) {
    body = isThread ? <ThreadPane threadId={route.threadId} /> : (surface ?? <InboxPane />);
  } else {
    body =
      surface ??
      (isThread ? (
        <div className="flex min-h-0 flex-1">
          <InboxPane />
          <ThreadPane threadId={route.threadId} />
        </div>
      ) : (
        <div className="flex min-h-0 flex-1">
          <InboxPane />
          <InboxDetailEmpty />
        </div>
      ));
  }

  return (
    <div className="flex h-dvh min-h-0 flex-col bg-paper text-ink">
      {isPhone && !isThread ? <PhoneTopBar /> : null}
      <div className="flex min-h-0 flex-1">
        {!isPhone ? <Rail /> : null}
        <div className="flex min-w-0 flex-1 flex-col">
          <OfflineBanner />
          <div className="flex min-h-0 flex-1">{body}</div>
        </div>
      </div>
      {!isPhone ? <StatusBar /> : null}

      <NewThreadDialog />
      <CommandPalette />
      <ShortcutsDialog />
      <Toasts />
    </div>
  );
}
