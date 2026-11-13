import { CommandIcon, PlusIcon } from "lucide-react";
import type * as React from "react";

import { CommandPalette } from "@/components/dialogs/CommandPalette";
import { NewThreadDialog } from "@/components/dialogs/NewThreadDialog";
import { ShortcutsDialog } from "@/components/dialogs/ShortcutsDialog";
import { InboxPane } from "@/components/inbox/InboxPane";
import { LibraryPage } from "@/components/pages/LibraryPage";
import { PairingPage } from "@/components/pages/PairingPage";
import { PullRequestsPage } from "@/components/pages/PullRequestsPage";
import { SettingsPage } from "@/components/pages/SettingsPage";
import { UsagePage } from "@/components/pages/UsagePage";
import { OfflineBanner } from "@/components/shell/OfflineBanner";
import { PhoneNav } from "@/components/shell/PhoneNav";
import { Sidebar } from "@/components/shell/Sidebar";
import { Toasts } from "@/components/shell/Toasts";
import { ThreadPane } from "@/components/thread/ThreadPane";
import { IdePane } from "@/features/ide/IdePane";
import { useIsPhone, useIsWide } from "@/hooks/use-media-query";
import { openNewThread, setUi, useApp } from "@/lib/store/appStore";

function InboxDetailEmpty() {
  const count = useApp((state) => Object.keys(state.threads).length);
  return (
    <main className="q-main">
      <div className="q-center-empty">
        <div className="q-empty" style={{ width: 420, alignItems: "flex-start", padding: 0 }}>
          <h3 style={{ fontSize: 17, lineHeight: "24px" }}>
            {count === 0 ? "No thread is open" : "Choose a thread"}
          </h3>
          <p style={{ margin: 0 }}>
            The stream, tool calls and approvals live here. Opening a thread replaces this pane and
            keeps the list in place.
          </p>
          <div style={{ display: "flex", gap: 8, marginTop: 8 }}>
            <button type="button" className="q-btn primary" onClick={openNewThread}>
              <PlusIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
              New thread
            </button>
            <button
              type="button"
              className="q-btn secondary"
              onClick={() => setUi({ paletteOpen: true })}
            >
              <CommandIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
              Commands
            </button>
          </div>
        </div>
      </div>
    </main>
  );
}

function Surface({ surface, path }: { surface: string; path: string }) {
  switch (surface) {
    case "pulls":
      return <PullRequestsPage />;
    case "library":
      return <LibraryPage />;
    case "usage":
      return <UsagePage />;
    case "settings":
      return <SettingsPage section={path.split("/")[2] ?? "hosts"} />;
    case "pairing":
      return <PairingPage />;
    default:
      return <InboxPane />;
  }
}

export function AppShell() {
  const route = useApp((state) => state.ui.route);
  const isPhone = useIsPhone();
  const isWide = useIsWide();
  const isThread = route.kind === "thread";
  const pairing = route.kind === "placeholder" && route.surface === "pairing";

  const surface =
    route.kind === "ide" ? (
      <IdePane />
    ) : route.kind === "hosts" ? (
      <SettingsPage section="hosts" />
    ) : route.kind === "placeholder" ? (
      <Surface surface={route.surface} path={route.path} />
    ) : null;

  const page = isThread ? <ThreadPane threadId={route.threadId} /> : (surface ?? <InboxPane />);

  if (isPhone) {
    return (
      <div className="q-app" style={{ flexDirection: "column", height: "100%", minHeight: 0 }}>
        {surface ? <OfflineBanner /> : null}
        <div
          className="q-ph-content"
          style={{ flex: 1, minHeight: 0, display: "flex", flexDirection: "column" }}
        >
          {isThread ? page : (surface ?? <InboxPane />)}
        </div>
        {isThread || pairing ? null : <PhoneNav />}

        <NewThreadDialog />
        <CommandPalette />
        <ShortcutsDialog />
        <Toasts />
      </div>
    );
  }

  let body: React.ReactNode;
  if (surface) {
    body = surface;
  } else if (isThread) {
    body = isWide ? (
      <>
        <InboxPane />
        {page}
      </>
    ) : (
      page
    );
  } else {
    body = isWide ? (
      <>
        <InboxPane />
        <InboxDetailEmpty />
      </>
    ) : (
      <InboxPane />
    );
  }

  return (
    <div className="q-app">
      <Sidebar />
      <div style={{ flex: 1, minWidth: 0, display: "flex", flexDirection: "column", minHeight: 0 }}>
        {surface ? <OfflineBanner /> : null}
        <div style={{ flex: 1, minHeight: 0, display: "flex" }}>{body}</div>
      </div>

      <NewThreadDialog />
      <CommandPalette />
      <ShortcutsDialog />
      <Toasts />
    </div>
  );
}
