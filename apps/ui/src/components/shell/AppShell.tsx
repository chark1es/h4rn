import { PlusIcon } from "lucide-react";
import type * as React from "react";
import { useEffect, useMemo, useState } from "react";

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
import { HARNESS_LABELS, harnessLabel } from "@/lib/entities";
import { createThread, useApp } from "@/lib/store/appStore";

const START_MODELS: Record<string, string> = {
  claude_code: "claude-sonnet-5-5",
  codex: "gpt-5.1-codex",
  opencode: "claude-sonnet-5-5",
  antigravity: "gemini-4-argon-medium",
};

function InboxDetailEmpty() {
  const hosts = useApp((state) => state.hosts);
  const onlineHosts = useMemo(
    () =>
      Object.values(hosts)
        .filter((host) => host.online)
        .sort((a, b) => a.name.localeCompare(b.name)),
    [hosts],
  );
  const [host, setHost] = useState("");
  const [harness, setHarness] = useState("claude_code");
  const [workspace, setWorkspace] = useState("~/code");
  const [prompt, setPrompt] = useState("");
  const [starting, setStarting] = useState(false);

  useEffect(() => {
    if (host || onlineHosts.length === 0) return;
    setHost(onlineHosts[0]?.host_id ?? "");
  }, [host, onlineHosts]);

  const selectedHost = hosts[host];
  const harnessChoices = selectedHost?.harnesses.length
    ? selectedHost.harnesses
    : (Object.keys(HARNESS_LABELS) as Array<keyof typeof HARNESS_LABELS>);
  const hostName = selectedHost?.name ?? "a host";

  const submit = async () => {
    if (!host || starting || !prompt.trim()) return;
    setStarting(true);
    const ok = await createThread({
      host,
      harness: harness as never,
      workspacePath: workspace.trim() || "~/code",
      model: START_MODELS[harness] ?? "claude-sonnet-5-5",
      prompt: prompt.trim(),
      runtimeMode: "approval_required",
    });
    setStarting(false);
    if (ok) setPrompt("");
  };

  return (
    <main className="q-main">
      <div className="q-center-empty">
        <div className="q-empty" style={{ width: 520, alignItems: "flex-start", padding: 0 }}>
          <h3 style={{ fontSize: 17, lineHeight: "24px" }}>Start a thread</h3>
          <p style={{ margin: 0 }}>
            Pick where it runs. The agent works in a worktree on that host, and you can supervise it
            from any paired device.
          </p>
          <div style={{ display: "flex", gap: 8, margin: "8px 0 10px", flexWrap: "wrap" }}>
            <select
              aria-label="Host"
              className="q-btn secondary"
              style={{ height: 34, borderRadius: 8, fontSize: 13.5 }}
              value={host}
              onChange={(event) => {
                const nextHost = hosts[event.target.value];
                setHost(event.target.value);
                const nextHarness = nextHost?.harnesses[0];
                if (nextHarness) setHarness(nextHarness);
              }}
            >
              {onlineHosts.length === 0 ? <option value="">No online hosts</option> : null}
              {onlineHosts.map((option) => (
                <option key={option.host_id} value={option.host_id}>
                  {option.name}
                </option>
              ))}
            </select>
            <select
              aria-label="Harness"
              className="q-btn secondary"
              style={{ height: 34, borderRadius: 8, fontSize: 13.5 }}
              value={harness}
              onChange={(event) => setHarness(event.target.value)}
            >
              {harnessChoices.map((option) => (
                <option key={option} value={option}>
                  {HARNESS_LABELS[option]}
                </option>
              ))}
            </select>
            <input
              aria-label="Workspace folder"
              className="q-btn secondary"
              style={{
                height: 34,
                borderRadius: 8,
                fontSize: 12,
                width: 190,
                textAlign: "left",
                fontFamily: "var(--mono)",
              }}
              value={workspace}
              onChange={(event) => setWorkspace(event.target.value)}
              placeholder="~/code/api"
            />
          </div>
          <div className="q-composer" style={{ width: "100%" }}>
            <textarea
              aria-label="Describe the task"
              rows={3}
              placeholder={`Describe the task for ${harnessLabel(harness as never)} on ${hostName}`}
              value={prompt}
              onChange={(event) => setPrompt(event.target.value)}
              onKeyDown={(event) => {
                if (event.key === "Enter" && !event.shiftKey && !event.nativeEvent.isComposing) {
                  event.preventDefault();
                  void submit();
                }
              }}
            />
            <div className="q-comp-f">
              <span className="grow" />
              <button
                type="button"
                className={`q-send${prompt.trim() && host ? " ready" : ""}`}
                disabled={!host || !prompt.trim() || starting}
                onClick={() => void submit()}
                aria-label="Start thread"
              >
                <PlusIcon className="q-icon" strokeWidth={2} aria-hidden />
              </button>
            </div>
          </div>
          <p className="q-faint" style={{ fontSize: 12, margin: "12px 0 0" }}>
            {onlineHosts.length === 0 ? (
              <>
                No host yet? Pair a device in Settings or run{" "}
                <span className="q-mono">h4rn host</span> on the machine.
              </>
            ) : (
              <>Everything after Start is visible in the stream.</>
            )}
          </p>
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
      <div className="q-app q-ph" style={{ flexDirection: "column", height: "100%", minHeight: 0 }}>
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
