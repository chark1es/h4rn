import {
  ArrowUpIcon,
  AtSignIcon,
  ChevronDownIcon,
  FolderIcon,
  FolderPlusIcon,
  PaperclipIcon,
  TextCursorInputIcon,
} from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Trans, useTranslation } from "react-i18next";
import { EntityAvatar } from "@/components/agent/EntityAvatar";
import { CommandPalette } from "@/components/dialogs/CommandPalette";
import { NewThreadDialog } from "@/components/dialogs/NewThreadDialog";
import { ShortcutsDialog } from "@/components/dialogs/ShortcutsDialog";
import { PhoneInbox } from "@/components/inbox/PhoneInbox";
import { IssuesPage } from "@/components/pages/IssuesPage";
import { LibraryPage } from "@/components/pages/LibraryPage";
import { PairingPage } from "@/components/pages/PairingPage";
import { PullRequestsPage } from "@/components/pages/PullRequestsPage";
import { SettingsPage } from "@/components/pages/SettingsPage";
import { RightPanel } from "@/components/panel/RightPanel";
import { OfflineBanner } from "@/components/shell/OfflineBanner";
import { PhoneNav } from "@/components/shell/PhoneNav";
import { ShellSplit } from "@/components/shell/ShellSplit";
import { Sidebar } from "@/components/shell/Sidebar";
import { HarnessSwitchCommand } from "@/components/thread/HarnessSwitch";
import { ThreadPane } from "@/components/thread/ThreadPane";
import {
  Menu,
  MenuContent,
  MenuGroup,
  MenuGroupLabel,
  MenuItem,
  MenuNote,
  MenuRadioGroup,
  MenuRadioItem,
  MenuSeparator,
  MenuTrigger,
} from "@/components/ui/menu";
import { selectTriggerVariants } from "@/components/ui/select";
import { Toaster } from "@/components/ui/sonner";
import { Tip } from "@/components/ui/tooltip";
import { AttachmentTray } from "@/features/attachments/AttachmentTray";
import { FILE_ACCEPT } from "@/features/attachments/picker";
import { useAttachmentDrafts, useAttachShortcut } from "@/features/attachments/use-attachments";
import { LinkPullRequestDialog } from "@/features/forge/LinkPullRequestDialog";
import { GuestShareDialog } from "@/features/guest/GuestShareDialog";
import { useThreadModels } from "@/features/harnesses/use-thread-models";
import { IdePane } from "@/features/ide/IdePane";
import { McpApprovalCard } from "@/features/mcp/McpApprovalCard";
import { ensureMcpEvents } from "@/features/mcp/store";
import { PreviewPanel } from "@/features/preview/PreviewPanel";
import { AddProjectDialog } from "@/features/projects/AddProjectDialog";
import { shortPath, useProjects } from "@/features/projects/store";
import {
  proxyDefaultModel,
  proxyModelsFor,
  proxyOverrideFor,
  useProxy,
  useProxyCatalog,
} from "@/features/proxy/proxyStore";
import { AppUpdateNotice } from "@/features/updates/AppUpdateNotice";
import { UsagePage } from "@/features/usage/UsagePage";
import type { HarnessKind } from "@/gen";
import { useIsPhone } from "@/hooks/use-media-query";
import { useViewFade, viewKey } from "@/hooks/use-view-fade";
import { useViewportWidth } from "@/hooks/use-viewport-width";
import {
  DEFAULT_NEW_THREAD_WORKSPACE,
  drafts,
  normalizeWorkspace,
  subscribeDraftChanges,
} from "@/lib/drafts";
import { HARNESS_LABELS, harnessLabel, harnessSeed, machineSeed } from "@/lib/entities";
import { projectName } from "@/lib/grouping";
import { HARNESS_MODELS } from "@/lib/models";
import { NewThreadDraftSession } from "@/lib/newThreadDraft";
import {
  closeLinkPullRequest,
  createThread,
  navigate,
  openAddProject,
  panelFits,
  setPanelWidth,
  setSidebarWidth,
  useApp,
} from "@/lib/store/appStore";
import { defaultThreadHostId, localOnlyHostOptions } from "@/lib/thread-hosts";
import { activeThread } from "@/lib/thread-selection";

const HARNESSES: HarnessKind[] = ["claude_code", "codex", "opencode", "antigravity"];

/** The start screen's pickers share the compact Select trigger. */
const START_CHIP = selectTriggerVariants({ size: "chip" });
export function InboxDetailEmpty() {
  const hosts = useApp((state) => state.hosts);
  const localHostId = useApp((state) => state.localHostId);
  const connection = useApp((state) => state.connection.status);
  const { t } = useTranslation(["shell", "composer", "sidebar", "common"]);
  const onlineHosts = useMemo(
    () =>
      Object.values(hosts)
        .filter((host) => host.online)
        .sort((a, b) => a.name.localeCompare(b.name)),
    [hosts],
  );
  const hostOptions = useMemo(
    () => localOnlyHostOptions(onlineHosts, localHostId, t("start.notYetHint")),
    [onlineHosts, localHostId, t],
  );
  const [host, setHost] = useState("");
  const [harness, setHarness] = useState<HarnessKind>("claude_code");
  const [modelChoice, setModel] = useState<string | null>(null);
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
  useProxy((state) => state.catalogs);
  const newThreadOpen = useApp((state) => state.ui.newThreadOpen);

  useEffect(() => {
    if (host || onlineHosts.length === 0) return;
    const first = defaultThreadHostId(onlineHosts, localHostId) ?? "";
    if (!first) return;
    // The dialog's session: no folder option, so a folder the person used on
    // this host before is restored here with its draft.
    const draft = draftSession().adoptHost(first);
    const available = onlineHosts.find((entry) => entry.host_id === first)?.harnesses ?? [];
    setHost(first);
    setHarness(HARNESSES.find((kind) => available.includes(kind)) ?? "claude_code");
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
  const hostName = selectedHost?.name ?? null;
  const offline = connection !== "online";
  const writable = !offline && Boolean(host);
  const proxy = proxyOverrideFor(harness);
  const nativeModels = useThreadModels(host, harness, writable && !proxy);
  const defaultModel =
    proxyDefaultModel(harness) ?? (nativeModels ? "default" : HARNESS_MODELS[harness].default);
  const model = modelChoice ?? defaultModel;

  const attachments = useAttachmentDrafts("start-thread", {
    enabled: writable,
    disabledReason: offline ? t("start.attachOffline") : t("start.attachNoHost"),
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
      setModel(null);
    }
  };

  const pickHarness = (nextHarness: HarnessKind) => {
    setHarness(nextHarness);
    setModel(null);
  };

  const canSend =
    writable &&
    !starting &&
    !attachments.blockedReason &&
    (prompt.trim().length > 0 || attachments.attachments.length > 0);

  const modelChoices = Array.from(
    new Set([
      defaultModel,
      ...(proxy
        ? proxyModelsFor(harness).map((model) => model.id)
        : (nativeModels ?? HARNESS_MODELS[harness].models)),
    ]),
  );

  return (
    <main className="q-main">
      <div className="q-center-empty">
        <div className="q-empty q-box" style={{ alignItems: "flex-start", padding: 0, gap: 0 }}>
          <h2
            style={{
              margin: "0 0 6px",
              fontSize: 16,
              lineHeight: "24px",
              fontWeight: 600,
              color: "var(--fg)",
            }}
          >
            {t("start.title")}
          </h2>
          <p className="q-muted" style={{ margin: "0 0 18px", lineHeight: "19px" }}>
            {t("start.lead")}
          </p>

          <div className="q-chips" style={{ marginBottom: 10 }}>
            <Menu>
              <MenuTrigger
                className={START_CHIP}
                aria-label={
                  selectedHost
                    ? t("start.hostChip", { name: selectedHost.name })
                    : t("start.hostChipNone")
                }
              >
                <EntityAvatar
                  seed={machineSeed(host)}
                  kind="machine"
                  label={selectedHost?.name ?? t("start.host")}
                  size={16}
                />
                <span className="min-w-0 flex-1 truncate">
                  {selectedHost?.name ?? t("start.noOnlineHost")}
                </span>
                <ChevronDownIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
              </MenuTrigger>
              <MenuContent align="start" className="w-60">
                <MenuRadioGroup value={host} onValueChange={(next: string) => pickHost(next)}>
                  <MenuGroupLabel>{t("start.host")}</MenuGroupLabel>
                  {hostOptions.length === 0 ? (
                    <MenuNote>{t("start.noOnlineHosts")}</MenuNote>
                  ) : (
                    hostOptions.map((option) => (
                      <MenuRadioItem
                        key={option.value}
                        value={option.value}
                        label={option.label}
                        disabled={option.disabled}
                        title={option.hint}
                        closeOnSelect
                      >
                        <span className="inline-flex min-w-0 items-center gap-2">
                          <EntityAvatar
                            seed={machineSeed(option.value)}
                            kind="machine"
                            label={option.label}
                            size={14}
                          />
                          <span className="truncate">{option.label}</span>
                          {option.hint ? (
                            <span className="ms-auto shrink-0 text-xs text-muted-foreground">
                              {t("start.notYet")}
                            </span>
                          ) : null}
                        </span>
                      </MenuRadioItem>
                    ))
                  )}
                </MenuRadioGroup>
              </MenuContent>
            </Menu>

            <Menu>
              <MenuTrigger
                className={START_CHIP}
                aria-label={t("start.harnessChip", { name: harnessLabel(harness) })}
              >
                <EntityAvatar
                  seed={harnessSeed(host, harness)}
                  kind="harness"
                  label={harnessLabel(harness)}
                  size={16}
                />
                <span className="min-w-0 flex-1 truncate">{harnessLabel(harness)}</span>
                <ChevronDownIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
              </MenuTrigger>
              <MenuContent align="start" className="w-60">
                <MenuRadioGroup
                  value={harness}
                  onValueChange={(next: HarnessKind) => pickHarness(next)}
                >
                  <MenuGroupLabel>{t("start.harness")}</MenuGroupLabel>
                  {harnessChoices.map((option) => (
                    <MenuRadioItem
                      key={option}
                      value={option}
                      label={HARNESS_LABELS[option]}
                      closeOnSelect
                    >
                      <span className="inline-flex min-w-0 items-center gap-2">
                        <EntityAvatar
                          seed={harnessSeed(host, option)}
                          kind="harness"
                          label={harnessLabel(option)}
                          size={14}
                        />
                        <span className="truncate">{HARNESS_LABELS[option]}</span>
                      </span>
                    </MenuRadioItem>
                  ))}
                </MenuRadioGroup>
              </MenuContent>
            </Menu>

            {customFolder || folderChoices.length === 0 ? (
              <label className="q-chip q-chip-field">
                <span className="sr-only">{t("start.folder")}</span>
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
            <Menu>
              <MenuTrigger
                className={START_CHIP}
                aria-label={t("start.projectChip", { name: selectedFolder?.name ?? workspace })}
                title={workspace}
              >
                <FolderIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
                <span className="min-w-0 flex-1 truncate">
                  {customFolder || folderChoices.length === 0
                    ? t("start.projects")
                    : (selectedFolder?.name ?? shortPath(workspace))}
                </span>
                <ChevronDownIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
              </MenuTrigger>
              <MenuContent align="start" className="w-72">
                <MenuGroup>
                  <MenuGroupLabel>
                    {hostName
                      ? t("start.projectOn", { host: hostName })
                      : t("start.projectOnAnyHost")}
                  </MenuGroupLabel>
                  {folderChoices.length === 0 ? (
                    <MenuNote>{t("start.noProjects")}</MenuNote>
                  ) : (
                    folderChoices.map((choice) => (
                      <MenuItem
                        key={choice.path}
                        icon={<FolderIcon />}
                        label={choice.name}
                        trailing={
                          <span className="max-w-30 truncate font-mono">
                            {shortPath(choice.path)}
                          </span>
                        }
                        onSelect={() => {
                          setCustomFolder(false);
                          commitWorkspace(choice.path);
                        }}
                      >
                        {choice.name}
                      </MenuItem>
                    ))
                  )}
                </MenuGroup>
                <MenuSeparator />
                <MenuItem icon={<TextCursorInputIcon />} onSelect={() => setCustomFolder(true)}>
                  {t("start.typePath")}
                </MenuItem>
                <MenuItem icon={<FolderPlusIcon />} onSelect={openAddProject}>
                  {t("start.addProject")}
                </MenuItem>
              </MenuContent>
            </Menu>
          </div>

          <fieldset
            ref={composerRef}
            aria-label={t("start.composer")}
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
              aria-label={t("start.describe")}
              placeholder={
                hostName
                  ? t("start.describeFor", { harness: harnessLabel(harness), host: hostName })
                  : t("start.describeForAnyHost", { harness: harnessLabel(harness) })
              }
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
              aria-label={t("composer:attach.label")}
              className="sr-only"
              tabIndex={-1}
              onChange={(event) => attachments.onInputChange(event.target.files)}
            />
            <div className="q-comp-f">
              <Tip label={t("composer:attach.label")} command="composer.attach">
                <button
                  type="button"
                  disabled={!writable}
                  onClick={attachments.openPicker}
                  aria-label={t("composer:attach.label")}
                  className="q-ibtn"
                >
                  <PaperclipIcon className="q-icon" strokeWidth={1.5} aria-hidden />
                </button>
              </Tip>
              <Tip label={t("composer:mention")}>
                <button
                  type="button"
                  disabled={!writable}
                  onClick={insertMention}
                  aria-label={t("composer:mention")}
                  className="q-ibtn"
                >
                  <AtSignIcon className="q-icon" strokeWidth={1.5} aria-hidden />
                </button>
              </Tip>
              <span className="grow" />

              <Menu>
                <MenuTrigger
                  className="q-model"
                  aria-label={t("composer:controls.model")}
                  title={`${harnessLabel(harness)} · ${model}`}
                >
                  <span className="truncate" style={{ maxWidth: 210 }}>
                    {model}
                  </span>
                  <ChevronDownIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
                </MenuTrigger>
                <MenuContent align="end" className="w-64">
                  <MenuRadioGroup value={model} onValueChange={(next: string) => setModel(next)}>
                    <MenuGroupLabel>{t("composer:controls.model")}</MenuGroupLabel>
                    {modelChoices.map((option) => (
                      <MenuRadioItem key={option} value={option} closeOnSelect>
                        <span className="font-mono">{option}</span>
                      </MenuRadioItem>
                    ))}
                  </MenuRadioGroup>
                </MenuContent>
              </Menu>

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
                    ? t("start.blocked.offline")
                    : !host
                      ? t("start.blocked.noHost")
                      : !prompt.trim() && attachments.attachments.length === 0
                        ? t("start.blocked.empty")
                        : starting
                          ? t("start.blocked.starting")
                          : t("start.blocked.ready"))
                }
                aria-label={t("start.send")}
              >
                <ArrowUpIcon className="q-icon" strokeWidth={2} aria-hidden />
              </button>
            </div>
          </fieldset>

          <p className="q-faint" style={{ fontSize: 12, margin: "12px 0 0" }}>
            <Trans
              t={t}
              i18nKey="start.noHostYet"
              values={{ command: "h4rn host" }}
              components={{
                pair: (
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
                  />
                ),
                code: <span className="q-mono" />,
              }}
            />
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
    case "issues":
      return <IssuesPage />;
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
  const { t } = useTranslation("shell");
  return (
    <main className="q-main" aria-label="IDE">
      <div className="q-center-empty">
        <div className="q-empty" style={{ width: 340, padding: 0 }}>
          <h3 style={{ fontSize: 16, lineHeight: "24px" }}>{t("phoneIde.title")}</h3>
          <p className="m-0">{t("phoneIde.body")}</p>
          <button
            type="button"
            className="q-btn secondary"
            onClick={() => navigate({ kind: "inbox" })}
          >
            {t("phoneIde.back")}
          </button>
        </div>
      </div>
    </main>
  );
}

export function AppShell() {
  const { t } = useTranslation(["sidebar", "thread"]);
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
  const viewRef = useRef<HTMLDivElement>(null);
  useViewFade(viewRef, viewKey(route, ide ? "ide" : "main"));
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
        <div ref={viewRef} className="q-ph-content flex min-h-0 flex-1 flex-col">
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
        <HarnessSwitchCommand />
        <LinkPullRequestDialog
          open={linkPrOpen}
          thread={thread ?? null}
          onClose={closeLinkPullRequest}
        />
        <ShortcutsDialog />
        <Toaster />
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
      <ShellSplit
        side="start"
        pane={layout.sidebarOpen ? <Sidebar /> : null}
        width={layout.sidebarWidth}
        min={240}
        max={420}
        label={t("sidebar:resize")}
        onWidth={setSidebarWidth}
      >
        {surface ? <OfflineBanner /> : null}
        <AppUpdateNotice />
        <McpApprovalCard />
        <ShellSplit
          side="end"
          pane={showPanel && thread ? <RightPanel thread={thread} ide={ide} /> : null}
          width={layout.panelWidth}
          min={320}
          max={640}
          label={t("thread:panel.resize")}
          onWidth={setPanelWidth}
        >
          <div ref={viewRef} className="flex min-h-0 min-w-0 flex-1">
            {main}
          </div>
        </ShellSplit>
      </ShellSplit>

      <NewThreadDialog />
      <AddProjectDialog />
      <GuestShareDialog />
      <CommandPalette />
      <HarnessSwitchCommand />
      <LinkPullRequestDialog
        open={linkPrOpen}
        thread={thread ?? null}
        onClose={closeLinkPullRequest}
      />
      <ShortcutsDialog />
      <Toaster />
    </div>
  );
}
