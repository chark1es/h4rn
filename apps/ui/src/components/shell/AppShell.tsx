import {
  ArrowUpIcon,
  AtSignIcon,
  ChevronDownIcon,
  FolderIcon,
  FolderPlusIcon,
  PaperclipIcon,
} from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import { EntityAvatar } from "@/components/agent/EntityAvatar";
import { CommandPalette } from "@/components/dialogs/CommandPalette";
import { NewThreadDialog } from "@/components/dialogs/NewThreadDialog";
import { ShortcutsDialog } from "@/components/dialogs/ShortcutsDialog";
import { PhoneInbox } from "@/components/inbox/PhoneInbox";
import { LibraryPage } from "@/components/pages/LibraryPage";
import { PairingPage } from "@/components/pages/PairingPage";
import { PullRequestsPage } from "@/components/pages/PullRequestsPage";
import { SettingsPage } from "@/components/pages/SettingsPage";
import { RightPanel } from "@/components/panel/RightPanel";
import { OfflineBanner } from "@/components/shell/OfflineBanner";
import { PhoneNav } from "@/components/shell/PhoneNav";
import { Sidebar } from "@/components/shell/Sidebar";
import { Toasts } from "@/components/shell/Toasts";
import { ThreadPane } from "@/components/thread/ThreadPane";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { AttachmentTray } from "@/features/attachments/AttachmentTray";
import { FILE_ACCEPT } from "@/features/attachments/picker";
import { useAttachmentDrafts, useAttachShortcut } from "@/features/attachments/use-attachments";
import { LinkPullRequestDialog } from "@/features/forge/LinkPullRequestDialog";
import { GuestShareDialog } from "@/features/guest/GuestShareDialog";
import { IdePane } from "@/features/ide/IdePane";
import { McpApprovalCard } from "@/features/mcp/McpApprovalCard";
import { ensureMcpEvents } from "@/features/mcp/store";
import { PreviewPanel } from "@/features/preview/PreviewPanel";
import { AddProjectDialog } from "@/features/projects/AddProjectDialog";
import { shortPath, useProjects } from "@/features/projects/store";
import {
  proxyDefaultModel,
  proxyModelsFor,
  useProxy,
  useProxyCatalog,
} from "@/features/proxy/proxyStore";
import { AppUpdateNotice } from "@/features/updates/AppUpdateNotice";
import { UsagePage } from "@/features/usage/UsagePage";
import type { HarnessKind } from "@/gen";
import { useIsPhone } from "@/hooks/use-media-query";
import { useViewportWidth } from "@/hooks/use-viewport-width";
import {
  DEFAULT_NEW_THREAD_WORKSPACE,
  drafts,
  normalizeWorkspace,
  subscribeDraftChanges,
} from "@/lib/drafts";
import { HARNESS_LABELS, harnessLabel, harnessSeed, machineSeed } from "@/lib/entities";
import { projectName } from "@/lib/grouping";
import { titleWithShortcut } from "@/lib/keybindings";
import { NewThreadDraftSession } from "@/lib/newThreadDraft";
import {
  closeLinkPullRequest,
  createThread,
  navigate,
  openAddProject,
  panelFits,
  useApp,
} from "@/lib/store/appStore";
import { defaultThreadHostId, localOnlyHostOptions } from "@/lib/thread-hosts";
import { activeThread } from "@/lib/thread-selection";

const START_MODELS: Record<HarnessKind, string> = {
  claude_code: "claude-sonnet-5-5",
  codex: "gpt-5.1-codex",
  opencode: "claude-sonnet-5-5",
  antigravity: "gemini-4-argon-medium",
  acp: "default",
};

const HARNESSES: HarnessKind[] = ["claude_code", "codex", "opencode", "antigravity"];

export function InboxDetailEmpty() {
  const hosts = useApp((state) => state.hosts);
  const localHostId = useApp((state) => state.localHostId);
  const connection = useApp((state) => state.connection.status);
  const onlineHosts = useMemo(
    () =>
      Object.values(hosts)
        .filter((host) => host.online)
        .sort((a, b) => a.name.localeCompare(b.name)),
    [hosts],
  );
  const hostOptions = useMemo(
    () => localOnlyHostOptions(onlineHosts, localHostId, "Not yet: new threads run on this host"),
    [onlineHosts, localHostId],
  );
  const [host, setHost] = useState("");
  const [harness, setHarness] = useState<HarnessKind>("claude_code");
  const [model, setModel] = useState(START_MODELS.claude_code);
  const [workspace, setWorkspace] = useState("~/code");
  const [customFolder, setCustomFolder] = useState(false);
  const projects = useProjects((state) => state.projects);
  const threads = useApp((state) => state.threads);
  const [prompt, setPrompt] = useState("");
  const [starting, setStarting] = useState(false);
  const textareaRef = useRef<HTMLTextAreaElement | null>(null);
  /** The Start composer: focus that stays inside it is still the person's. */
  const composerRef = useRef<HTMLFieldSetElement | null>(null);
  /**
   * The inbox box remembers text per host and folder, exactly like the New
   * thread dialog: it is the same start-a-thread slot, just always on screen.
   */
  const sessionRef = useRef<NewThreadDraftSession | null>(null);
  const draftSession = useCallback(
    () => (sessionRef.current ??= new NewThreadDraftSession(drafts)),
    [],
  );
  /** Start is being pressed: its blur must not swap the text before it submits. */
  const startPressed = useRef(false);
  const startButtonRef = useRef<HTMLButtonElement | null>(null);
  /** The dialog was open on the previous render; closing it re-reads the slot. */
  const dialogWasOpen = useRef(false);
  useProxyCatalog();
  useProxy((state) => state.profiles);
  const newThreadOpen = useApp((state) => state.ui.newThreadOpen);

  useEffect(() => {
    if (host || onlineHosts.length === 0) return;
    const first = defaultThreadHostId(onlineHosts, localHostId) ?? "";
    if (!first) return;
    // The dialog's session: no folder option, so a folder the person used on
    // this host before is restored here with its draft.
    const draft = draftSession().adoptHost(first);
    setHost(first);
    setWorkspace(draft.workspace);
    setPrompt(draft.text);
  }, [host, onlineHosts, localHostId, draftSession]);

  // Another tab (or the New thread dialog in this one) can write the same
  // slot. Show what the slot holds; a queued edit of this box is the newer
  // intent and the store already returns it.
  useEffect(() => {
    return subscribeDraftChanges((change) => {
      if (change.scope !== "new-thread") return;
      const shown = draftSession().snapshot();
      if (change.host !== shown.host || change.target !== shown.workspace) return;
      setPrompt(draftSession().reloadSlot().text);
    });
  }, [draftSession]);

  // The dialog edits the same slot while it sits on top: when it closes, read
  // the slot again so its text (or its clear) shows in the inbox.
  useEffect(() => {
    if (dialogWasOpen.current && !newThreadOpen) {
      setPrompt(draftSession().reloadSlot().text);
    }
    dialogWasOpen.current = newThreadOpen;
  }, [newThreadOpen, draftSession]);

  const commitWorkspace = useCallback(
    (path: string) => {
      const draft = draftSession().commitWorkspace(path);
      setWorkspace(draft.workspace);
      setPrompt(draft.text);
    },
    [draftSession],
  );

  const selectedHost = hosts[host];
  // Registered projects first, then folders that only have threads so far.
  const folderChoices = useMemo(() => {
    const seen = new Set<string>();
    const choices: Array<{ name: string; path: string }> = [];
    for (const project of projects) {
      if (project.host !== host || seen.has(project.path)) continue;
      seen.add(project.path);
      choices.push({ name: project.name, path: project.path });
    }
    for (const thread of Object.values(threads)) {
      if (thread.host !== host || seen.has(thread.workspace_path)) continue;
      seen.add(thread.workspace_path);
      choices.push({ name: projectName(thread.workspace_path), path: thread.workspace_path });
    }
    return choices;
  }, [projects, threads, host]);
  const selectedFolder = folderChoices.find((choice) => choice.path === workspace);
  // Start in the first project once the list arrives, unless a path was typed,
  // the person has a folder remembered on this host, or the box holds unsent
  // text. This is a convenience choice, not one to commit: it shows that
  // folder's draft but does not become the folder the next open remembers.
  useEffect(() => {
    if (customFolder || selectedFolder) return;
    const first = folderChoices[0];
    if (!first || workspace !== DEFAULT_NEW_THREAD_WORKSPACE) return;
    if (prompt.length > 0) return;
    if (host && drafts.readLastWorkspace(host) !== "") return;
    // The choice is view-only: its slot's draft is shown, but the folder is
    // not remembered as the last one used.
    const draft = draftSession().showWorkspace(first.path);
    setWorkspace(draft.workspace);
    setPrompt(draft.text);
  }, [customFolder, selectedFolder, folderChoices, workspace, prompt, host, draftSession]);
  const harnessChoices = (
    selectedHost?.harnesses.length ? selectedHost.harnesses : HARNESSES
  ).filter(
    // A local ACP command needs its instance id; the quick start has no such
    // picker, so it never offers the bare kind.
    (kind) => kind !== "acp",
  );
  const hostName = selectedHost?.name ?? "a host";
  const offline = connection !== "online";
  const writable = !offline && Boolean(host);

  const attachments = useAttachmentDrafts("start-thread", {
    enabled: writable,
    disabledReason: offline ? "Reconnect to attach files." : "Pick an online host first.",
  });
  useAttachShortcut(attachments.openPicker, writable && !newThreadOpen);

  const updatePrompt = (value: string) => {
    const session = draftSession();
    // The shown folder can be one the session has not committed yet: the
    // automatic first project, or a path typed in the field. Text typed while
    // that folder is on screen belongs to it, so the slot moves with the first
    // keystroke — but only while that folder's slot is empty. A typed folder
    // that already holds a saved draft is somebody else's unsent text: the
    // keystroke stays with the slot on screen instead of replacing it. That is
    // still not a folder choice to remember. Clearing the box is not text for
    // the shown folder either: it empties the slot the cleared text came from.
    const target = normalizeWorkspace(workspace) || DEFAULT_NEW_THREAD_WORKSPACE;
    if (
      value.length > 0 &&
      target !== session.snapshot().workspace &&
      session.slotIsEmpty(target)
    ) {
      session.showWorkspace(target);
    }
    session.setText(value);
    setPrompt(value);
  };

  const submit = async () => {
    // A press is over by the time Start runs; its flag must not outlive it.
    startPressed.current = false;
    if (!writable || starting || attachments.blockedReason) return;
    if (!prompt.trim() && attachments.attachments.length === 0) return;
    const draft = draftSession().submission(workspace);
    if (!draft) return;
    setStarting(true);
    const ok = await createThread({
      host: draft.host,
      harness,
      workspacePath: draft.workspace,
      model,
      prompt: draft.text.trim(),
      attachments: attachments.attachments,
      runtimeMode: "approval_required",
    });
    setStarting(false);
    if (ok) {
      // Clear only the slot whose revision was sent; newer text from another
      // tab survives, like a thread composer's draft.
      const cleared = draftSession().clearSubmitted(draft);
      drafts.writeLastWorkspace(draft.host, draft.workspace);
      // The folder can change while Start waits, and the box then shows that
      // folder's draft. Blanking it would lose text this send never saw; show
      // what the slot on screen holds now.
      const shown = draftSession().snapshot();
      const stillOnSentSlot = shown.host === draft.host && shown.workspace === draft.slotWorkspace;
      setPrompt(cleared && stillOnSentSlot ? "" : draftSession().reloadSlot().text);
      attachments.clear();
    }
  };

  const insertMention = () => {
    const element = textareaRef.current;
    if (!element || !writable) return;
    const start = element.selectionStart ?? prompt.length;
    updatePrompt(`${prompt.slice(0, start)}@${prompt.slice(start)}`);
    requestAnimationFrame(() => {
      element.focus();
      element.setSelectionRange(start + 1, start + 1);
    });
  };

  const pickHost = (nextHostId: string) => {
    const draft = draftSession().selectHost(nextHostId);
    setHost(nextHostId);
    setWorkspace(draft.workspace);
    setPrompt(draft.text);
    // Never default to the bare ACP kind: it is not startable without an
    // instance id, which this quick start does not choose.
    const nextHarness = hosts[nextHostId]?.harnesses.find((kind) => kind !== "acp");
    if (nextHarness) {
      setHarness(nextHarness);
      setModel(START_MODELS[nextHarness]);
    }
  };

  const pickHarness = (nextHarness: HarnessKind) => {
    setHarness(nextHarness);
    setModel(proxyDefaultModel(nextHarness) ?? START_MODELS[nextHarness]);
  };

  const canSend =
    writable &&
    !starting &&
    !attachments.blockedReason &&
    (prompt.trim().length > 0 || attachments.attachments.length > 0);

  const modelChoices = Array.from(
    new Set([...Object.values(START_MODELS), ...proxyModelsFor(harness).map((model) => model.id)]),
  );

  return (
    <main className="q-main">
      <div className="q-center-empty">
        <div className="q-empty q-box" style={{ alignItems: "flex-start", padding: 0, gap: 0 }}>
          <h2
            style={{
              margin: "0 0 6px",
              fontSize: 17,
              lineHeight: "24px",
              fontWeight: 600,
              color: "var(--fg)",
            }}
          >
            Start a thread
          </h2>
          <p className="q-muted" style={{ margin: "0 0 18px", lineHeight: "19px" }}>
            Pick where it runs. The agent works in a directory you choose on that host, and you can
            supervise it from any paired device.
          </p>

          <div className="q-chips" style={{ marginBottom: 10 }}>
            <DropdownMenu>
              <DropdownMenuTrigger asChild>
                <button
                  type="button"
                  className="q-chip"
                  aria-label={`Host: ${selectedHost?.name ?? "no online host"}`}
                >
                  <EntityAvatar
                    seed={machineSeed(host)}
                    kind="machine"
                    label={selectedHost?.name ?? "Host"}
                    size={16}
                  />
                  <span className="q-chip-label">{selectedHost?.name ?? "No online host"}</span>
                  <ChevronDownIcon className="q-icon s12" strokeWidth={1.5} aria-hidden />
                </button>
              </DropdownMenuTrigger>
              <DropdownMenuContent align="start" className="w-52">
                <DropdownMenuLabel>Host</DropdownMenuLabel>
                {hostOptions.length === 0 ? (
                  <DropdownMenuItem disabled>No online hosts</DropdownMenuItem>
                ) : (
                  hostOptions.map((option) => (
                    <DropdownMenuItem
                      key={option.value}
                      disabled={option.disabled}
                      title={option.hint}
                      onSelect={() => pickHost(option.value)}
                    >
                      <EntityAvatar
                        seed={machineSeed(option.value)}
                        kind="machine"
                        label={option.label}
                        size={16}
                      />
                      <span className="min-w-0 flex-1 truncate">{option.label}</span>
                      {option.hint ? (
                        <span className="ml-2 shrink-0 text-[11px] text-[var(--fg-3)]">
                          Not yet
                        </span>
                      ) : null}
                    </DropdownMenuItem>
                  ))
                )}
              </DropdownMenuContent>
            </DropdownMenu>

            <DropdownMenu>
              <DropdownMenuTrigger asChild>
                <button
                  type="button"
                  className="q-chip"
                  aria-label={`Harness: ${harnessLabel(harness)}`}
                >
                  <EntityAvatar
                    seed={harnessSeed(host, harness)}
                    kind="harness"
                    label={harnessLabel(harness)}
                    size={16}
                  />
                  <span className="q-chip-label">{harnessLabel(harness)}</span>
                  <ChevronDownIcon className="q-icon s12" strokeWidth={1.5} aria-hidden />
                </button>
              </DropdownMenuTrigger>
              <DropdownMenuContent align="start" className="w-52">
                <DropdownMenuLabel>Harness</DropdownMenuLabel>
                {harnessChoices.map((option) => (
                  <DropdownMenuItem key={option} onSelect={() => pickHarness(option)}>
                    <EntityAvatar
                      seed={harnessSeed(host, option)}
                      kind="harness"
                      label={harnessLabel(option)}
                      size={16}
                    />
                    {HARNESS_LABELS[option]}
                  </DropdownMenuItem>
                ))}
              </DropdownMenuContent>
            </DropdownMenu>

            {customFolder || folderChoices.length === 0 ? (
              <label className="q-chip q-chip-field">
                <span className="sr-only">Workspace folder</span>
                <input
                  value={workspace}
                  onChange={(event) => setWorkspace(event.target.value)}
                  onBlur={(event) => {
                    // Pressing Start blurs this field on the way to the
                    // button, with a mouse or by tabbing to it. Start sends
                    // what is on screen to the folder as typed; the blur must
                    // not swap the box to that folder's draft.
                    if (startPressed.current || event.relatedTarget === startButtonRef.current) {
                      return;
                    }
                    commitWorkspace(workspace);
                  }}
                  onKeyDown={(event) => {
                    if (event.key === "Enter") {
                      event.preventDefault();
                      commitWorkspace(workspace);
                    }
                  }}
                  placeholder="~/code/api"
                  spellCheck={false}
                />
              </label>
            ) : null}
            <DropdownMenu>
              <DropdownMenuTrigger asChild>
                <button
                  type="button"
                  className="q-chip"
                  aria-label={`Project: ${selectedFolder?.name ?? workspace}`}
                  title={workspace}
                >
                  <FolderIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
                  <span className="q-chip-label">
                    {customFolder || folderChoices.length === 0
                      ? "Projects"
                      : (selectedFolder?.name ?? shortPath(workspace))}
                  </span>
                  <ChevronDownIcon className="q-icon s12" strokeWidth={1.5} aria-hidden />
                </button>
              </DropdownMenuTrigger>
              <DropdownMenuContent align="start" className="w-64">
                <DropdownMenuLabel>Project on {hostName}</DropdownMenuLabel>
                {folderChoices.length === 0 ? (
                  <DropdownMenuItem disabled>No projects on this host yet</DropdownMenuItem>
                ) : (
                  folderChoices.map((choice) => (
                    <DropdownMenuItem
                      key={choice.path}
                      onSelect={() => {
                        setCustomFolder(false);
                        commitWorkspace(choice.path);
                      }}
                    >
                      <span className="min-w-0 flex-1 truncate">{choice.name}</span>
                      <span className="ml-2 max-w-[120px] shrink-0 truncate text-[11px] text-[var(--fg-3)]">
                        {shortPath(choice.path)}
                      </span>
                    </DropdownMenuItem>
                  ))
                )}
                <DropdownMenuSeparator />
                <DropdownMenuItem onSelect={() => setCustomFolder(true)}>
                  Type a folder path…
                </DropdownMenuItem>
                <DropdownMenuItem onSelect={openAddProject}>
                  <FolderPlusIcon aria-hidden />
                  Add project…
                </DropdownMenuItem>
              </DropdownMenuContent>
            </DropdownMenu>
          </div>

          <fieldset
            ref={composerRef}
            aria-label="Start a thread composer"
            className="q-composer"
            style={{ margin: 0, minWidth: 0, width: "100%", position: "relative" }}
            onDragOver={(event) => {
              if (writable) event.preventDefault();
            }}
            onDrop={(event) => {
              event.preventDefault();
              if (!writable) return;
              const files = Array.from(event.dataTransfer.files);
              if (files.length > 0) attachments.addFiles(files);
            }}
          >
            <AttachmentTray
              tasks={attachments.tasks}
              onRemove={attachments.remove}
              onRetry={attachments.retry}
            />
            <textarea
              ref={textareaRef}
              aria-label="Describe the task"
              placeholder={`Describe the task for ${harnessLabel(harness)} on ${hostName}`}
              value={prompt}
              disabled={!writable}
              onChange={(event) => updatePrompt(event.target.value)}
              onPaste={(event) => {
                if (!writable) return;
                const files = Array.from(event.clipboardData.files);
                if (files.length > 0) {
                  event.preventDefault();
                  attachments.addFiles(files);
                  return;
                }
                const pasted = event.clipboardData.getData("text/plain");
                if (attachments.addPastedText(pasted)) event.preventDefault();
              }}
              onKeyDown={(event) => {
                if (event.key === "Enter" && !event.shiftKey && !event.nativeEvent.isComposing) {
                  event.preventDefault();
                  void submit();
                }
              }}
            />
            <input
              ref={attachments.inputRef}
              type="file"
              multiple
              accept={FILE_ACCEPT}
              aria-label="Attach files"
              className="sr-only"
              tabIndex={-1}
              onChange={(event) => attachments.onInputChange(event.target.files)}
            />
            <div className="q-comp-f">
              <button
                type="button"
                disabled={!writable}
                onClick={attachments.openPicker}
                aria-label="Attach files"
                title={titleWithShortcut("Attach files", "composer.attach")}
                className="q-ibtn"
              >
                <PaperclipIcon className="q-icon" strokeWidth={1.5} aria-hidden />
              </button>
              <button
                type="button"
                disabled={!writable}
                onClick={insertMention}
                aria-label="Mention a file"
                title="Mention a file"
                className="q-ibtn"
              >
                <AtSignIcon className="q-icon" strokeWidth={1.5} aria-hidden />
              </button>
              <span className="grow" />

              <DropdownMenu>
                <DropdownMenuTrigger asChild>
                  <button
                    type="button"
                    className="q-model"
                    aria-label="Model"
                    title={`${harnessLabel(harness)} · ${model}`}
                  >
                    <span className="truncate" style={{ maxWidth: 210 }}>
                      {model}
                    </span>
                    <ChevronDownIcon className="q-icon s12" strokeWidth={1.5} aria-hidden />
                  </button>
                </DropdownMenuTrigger>
                <DropdownMenuContent align="end" className="w-60">
                  <DropdownMenuLabel>Model</DropdownMenuLabel>
                  {modelChoices.map((option) => (
                    <DropdownMenuItem key={option} onSelect={() => setModel(option)}>
                      <span className="font-mono">{option}</span>
                    </DropdownMenuItem>
                  ))}
                </DropdownMenuContent>
              </DropdownMenu>

              <button
                ref={startButtonRef}
                type="button"
                className={`q-send${canSend ? " ready" : ""}`}
                disabled={!canSend}
                onPointerDown={() => {
                  startPressed.current = true;
                }}
                onPointerUp={() => {
                  startPressed.current = false;
                }}
                onPointerCancel={() => {
                  startPressed.current = false;
                }}
                onPointerLeave={() => {
                  startPressed.current = false;
                }}
                onKeyDown={(event) => {
                  if (event.key === "Enter" || event.key === " ") startPressed.current = true;
                }}
                onKeyUp={() => {
                  startPressed.current = false;
                }}
                onBlur={(event) => {
                  const pressed = startPressed.current;
                  startPressed.current = false;
                  if (pressed) return;
                  // Tabbing from the folder field to Start skips that field's
                  // blur commit so Start sends the folder as typed. That skip
                  // lasts only while the press is happening: when Start loses
                  // focus without being pressed, the typed folder is committed
                  // here, so the session and the box agree again and a later
                  // keystroke cannot land in the wrong folder's slot.
                  const next = event.relatedTarget as Node | null;
                  // Focus that stays inside the composer is still the person
                  // writing here (the box, an attach button, the model menu):
                  // swapping the box under their next keystroke is the loss
                  // this commit exists to prevent.
                  if (next && composerRef.current?.contains(next)) return;
                  if (workspace !== draftSession().snapshot().workspace) {
                    commitWorkspace(workspace);
                  }
                }}
                onClick={() => void submit()}
                title={
                  attachments.blockedReason ??
                  (offline
                    ? "Reconnect to start a thread."
                    : !host
                      ? "No online host is available."
                      : !prompt.trim() && attachments.attachments.length === 0
                        ? "Describe the task first."
                        : starting
                          ? "Starting…"
                          : "Start (Enter)")
                }
                aria-label="Start thread"
              >
                <ArrowUpIcon className="q-icon" strokeWidth={2} aria-hidden />
              </button>
            </div>
          </fieldset>

          <p className="q-faint" style={{ fontSize: 12, margin: "12px 0 0" }}>
            No host yet?{" "}
            <button
              type="button"
              className="q-link"
              onClick={() =>
                navigate({
                  kind: "placeholder",
                  surface: "settings",
                  path: "/settings/devices",
                })
              }
            >
              Pair a device
            </button>{" "}
            or run <span className="q-mono">h4rn host</span> on the machine.
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
      return <LibraryPage path={path} />;
    case "usage":
      return <UsagePage />;
    case "settings":
      return <SettingsPage section={path.split("/")[2] ?? "hosts"} />;
    case "pairing":
      return <PairingPage />;
    default:
      return <InboxDetailEmpty />;
  }
}

function PhoneIdeNotice() {
  return (
    <main className="q-main" aria-label="IDE">
      <div className="q-center-empty">
        <div className="q-empty" style={{ width: 340, padding: 0 }}>
          <h3 style={{ fontSize: 17, lineHeight: "24px" }}>The IDE is a desktop view.</h3>
          <p style={{ margin: 0 }}>
            Open the thread on a larger screen to browse files, read diffs and edit. The thread and
            its changes are available here.
          </p>
          <button
            type="button"
            className="q-btn secondary"
            onClick={() => navigate({ kind: "inbox" })}
          >
            Back to inbox
          </button>
        </div>
      </div>
    </main>
  );
}

export function AppShell() {
  const route = useApp((state) => state.ui.route);
  const layout = useApp((state) => state.ui.layout);
  const threads = useApp((state) => state.threads);
  const selectedThreadId = useApp((state) => state.ui.selectedThreadId);
  const linkPrOpen = useApp((state) => state.ui.linkPrOpen);
  const isPhone = useIsPhone();
  // The relay can hand this daemon a shared call at any time, so the approval
  // stream must run before the owner visits the MCP screen.
  useEffect(() => {
    ensureMcpEvents();
  }, []);
  const isThread = route.kind === "thread";
  const pairing = route.kind === "placeholder" && route.surface === "pairing";
  const surface =
    route.kind === "hosts" ? (
      <SettingsPage section="hosts" />
    ) : route.kind === "placeholder" ? (
      <Surface surface={route.surface} path={route.path} />
    ) : null;
  const ide = layout.view === "ide" && !isPhone && (isThread || route.kind === "ide");
  const thread = activeThread(threads, selectedThreadId);
  const preview = useApp((state) => state.ui.preview);
  const previewExpanded = useApp((state) => state.ui.previewExpanded);
  const viewportWidth = useViewportWidth();
  const showPanel =
    !isPhone &&
    layout.panelOpen &&
    Boolean(thread) &&
    (isThread || ide) &&
    panelFits(layout, viewportWidth);

  if (isPhone) {
    return (
      <div className="q-app q-ph" style={{ flexDirection: "column", height: "100%", minHeight: 0 }}>
        {surface ? <OfflineBanner /> : null}
        <AppUpdateNotice inline />
        <McpApprovalCard />
        <div
          className="q-ph-content"
          style={{ flex: 1, minHeight: 0, display: "flex", flexDirection: "column" }}
        >
          {ide ? (
            <PhoneIdeNotice />
          ) : isThread ? (
            <ThreadPane threadId={route.threadId} />
          ) : (
            (surface ?? <PhoneInbox />)
          )}
        </div>
        {isThread || ide || pairing ? null : <PhoneNav />}

        <NewThreadDialog />
        <AddProjectDialog />
        <GuestShareDialog />
        <CommandPalette />
        <LinkPullRequestDialog
          open={linkPrOpen}
          thread={thread ?? null}
          onClose={closeLinkPullRequest}
        />
        <ShortcutsDialog />
        <Toasts />
      </div>
    );
  }

  const expandedPreview =
    !isPhone &&
    previewExpanded &&
    preview !== null &&
    thread !== null &&
    preview.threadId === thread.thread_id &&
    (isThread || ide);

  const main = expandedPreview ? (
    <PreviewPanel
      key={`${thread.thread_id}:${preview.path}`}
      thread={thread}
      path={preview.path}
      variant="full"
    />
  ) : ide ? (
    <IdePane />
  ) : isThread ? (
    <ThreadPane threadId={route.threadId} />
  ) : surface ? (
    surface
  ) : (
    <InboxDetailEmpty />
  );

  return (
    <div className="q-app">
      {layout.sidebarOpen ? <Sidebar /> : null}
      <div style={{ flex: 1, minWidth: 0, display: "flex", flexDirection: "column", minHeight: 0 }}>
        {surface ? <OfflineBanner /> : null}
        <AppUpdateNotice />
        <McpApprovalCard />
        <div style={{ flex: 1, minHeight: 0, display: "flex" }}>
          {main}
          {showPanel && thread ? <RightPanel thread={thread} ide={ide} /> : null}
        </div>
      </div>

      <NewThreadDialog />
      <AddProjectDialog />
      <GuestShareDialog />
      <CommandPalette />
      <LinkPullRequestDialog
        open={linkPrOpen}
        thread={thread ?? null}
        onClose={closeLinkPullRequest}
      />
      <ShortcutsDialog />
      <Toasts />
    </div>
  );
}
