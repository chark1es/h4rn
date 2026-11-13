import { CheckIcon, ChevronDownIcon, CircleXIcon, FileIcon } from "lucide-react";
import { type ReactNode, useEffect, useState } from "react";

import { AgentGlyph } from "@/components/agent/AgentGlyph";
import { EntityAvatar } from "@/components/agent/EntityAvatar";
import { MOD } from "@/components/common/Kbd";
import { subagentCount } from "@/features/subagents/SubagentsPanel";
import type { ThreadSummary } from "@/gen";
import { harnessLabel, harnessSeed, machineSeed, subagentSeed } from "@/lib/entities";
import { absoluteTime } from "@/lib/format";
import { openInIde, pushToast, setPanelTab } from "@/lib/store/appStore";
import { FileDiff } from "./FileDiff";
import { usePanelChanges } from "./usePanelChanges";

function splitPath(path: string): { directory: string; name: string } {
  const index = path.lastIndexOf("/");
  return index < 0
    ? { directory: "", name: path }
    : { directory: path.slice(0, index + 1), name: path.slice(index + 1) };
}

/** A flat subagent row: phones do not have room for the desktop tree. */
function SubagentRow({
  glyph,
  title,
  byline,
  status,
  tone,
  muted = false,
  child = false,
  selected = false,
  children,
}: {
  glyph: ReactNode;
  title: string;
  byline: ReactNode;
  status: ReactNode;
  tone?: "accent" | "danger";
  muted?: boolean;
  child?: boolean;
  selected?: boolean;
  children?: ReactNode;
}) {
  return (
    <div className={`q-ph-li q-ph-node${child ? " kid" : ""}${selected ? " sel" : ""}`}>
      <span className="q-ph-node-gl">{glyph}</span>
      <div className="grow">
        <div className="q-ph-node-t" style={muted ? { color: "var(--fg-2)" } : undefined}>
          {title}
        </div>
        <div className="s" style={{ display: "flex", alignItems: "center", gap: 5 }}>
          {byline}
        </div>
        <div className="s" style={tone ? { color: `var(--${tone})`, fontWeight: 500 } : undefined}>
          {status}
        </div>
        {children}
      </div>
    </div>
  );
}

/**
 * On a phone the right panel becomes one sheet: Changes and Subagents share a
 * segmented control. Files and the editor are desktop surfaces, and the sheet
 * says so rather than half-rendering them.
 */
export function PhonePanelSheet({
  thread,
  initialTab,
  onClose,
  onReview,
}: {
  thread: ThreadSummary;
  initialTab: "changes" | "subagents";
  onClose: () => void;
  onReview?: () => void;
}) {
  const [tab, setTab] = useState(initialTab);
  const changes = usePanelChanges(thread);
  const files = changes.changes?.files ?? [];
  const subagents = subagentCount(thread.thread_id);

  useEffect(() => setTab(initialTab), [initialTab]);

  const openFile = (path: string) => {
    // Editing is a desktop task, but a file can still be handed to the IDE for
    // the next time this session runs wide.
    openInIde(path);
    onClose();
  };

  return (
    <>
      <button
        type="button"
        className="q-ph-scrim"
        aria-label="Close changes and subagents"
        onClick={onClose}
      />
      <div
        className="q-ph-sheet large"
        role="dialog"
        aria-modal="true"
        aria-label="Changes and subagents"
      >
        <div className="q-grab" />
        <div className="q-ph-sh-nav">
          <span style={{ width: 64 }} />
          <span className="ttl">{thread.title}</span>
          <button type="button" className="navbtn" onClick={onClose}>
            Done
          </button>
        </div>
        <div className="q-ph-seg" style={{ padding: "0 16px 10px" }}>
          <div className="q-seg" role="tablist" aria-label="Changes and subagents">
            <button
              type="button"
              role="tab"
              aria-selected={tab === "changes"}
              onClick={() => setTab("changes")}
            >
              Changes {files.length > 0 ? <span className="c q-num">{files.length}</span> : null}
            </button>
            <button
              type="button"
              role="tab"
              aria-selected={tab === "subagents"}
              onClick={() => {
                setPanelTab("subagents");
                setTab("subagents");
              }}
            >
              Subagents {subagents > 0 ? <span className="c q-num">{subagents}</span> : null}
              {subagents > 0 && thread.needs_attention ? (
                <span className="q-pip" role="img" aria-label="needs you" />
              ) : null}
            </button>
          </div>
        </div>

        {tab === "subagents" ? (
          subagents > 0 ? (
            <div className="q-ph-body">
              <div className="q-ph-gh">{subagents} subagents · 1 waiting on you</div>
              <div className="q-ph-group">
                <SubagentRow
                  glyph={
                    <AgentGlyph
                      glyph={{ kind: "orb", state: "working", label: "Running" }}
                      size={14}
                    />
                  }
                  title={thread.title}
                  byline={
                    <>
                      <EntityAvatar
                        seed={machineSeed(thread.host)}
                        kind="machine"
                        label={thread.host}
                        size={14}
                      />
                      {thread.host} · {harnessLabel(thread.harness)}
                    </>
                  }
                  status="Running · 18.4k tokens"
                />
                <SubagentRow
                  child
                  selected
                  glyph={
                    <AgentGlyph
                      glyph={{ kind: "orb", state: "breathing", paused: true, label: "Held" }}
                      size={14}
                    />
                  }
                  title="Reproduce CI failure"
                  byline={
                    <>
                      <EntityAvatar
                        seed={subagentSeed("orion", thread.thread_id, "r2")}
                        kind="subagent"
                        label="subagent"
                        size={14}
                      />
                      <EntityAvatar
                        seed={machineSeed("orion")}
                        kind="machine"
                        label="orion"
                        size={14}
                      />
                      orion · Codex
                    </>
                  }
                  status="Needs approval · turn 3 of 20"
                  tone="accent"
                >
                  <div style={{ display: "flex", gap: 10, marginTop: 10 }}>
                    <button
                      type="button"
                      className="q-btn secondary sm"
                      style={{ flex: 1 }}
                      onClick={() =>
                        pushToast({
                          title: "Subagent details arrive with the wire protocol.",
                          variant: "info",
                        })
                      }
                    >
                      Details
                    </button>
                    <button
                      type="button"
                      className="q-btn primary sm"
                      style={{ flex: 1 }}
                      onClick={() => {
                        onClose();
                        onReview?.();
                      }}
                    >
                      Review
                    </button>
                  </div>
                </SubagentRow>
                <SubagentRow
                  child
                  glyph={<CheckIcon className="q-icon s16" strokeWidth={1.5} aria-hidden />}
                  title="Audit test helpers"
                  byline={
                    <>
                      <EntityAvatar
                        seed={subagentSeed("atlas", thread.thread_id, "a1")}
                        kind="subagent"
                        label="subagent"
                        size={14}
                      />
                      atlas · Codex
                    </>
                  }
                  status="Done · 6.2k tokens"
                />
                <SubagentRow
                  child
                  muted
                  glyph={
                    <CircleXIcon className="q-icon s16 q-danger" strokeWidth={1.5} aria-hidden />
                  }
                  title="Check clock skew on buildbox"
                  byline={
                    <>
                      <EntityAvatar
                        seed={subagentSeed("buildbox", thread.thread_id, "c3")}
                        kind="subagent"
                        label="subagent"
                        size={14}
                      />
                      buildbox · Codex
                    </>
                  }
                  status="Couldn’t start: buildbox is offline"
                />
              </div>
              <p className="q-ph-note" style={{ marginTop: -8 }}>
                A subagent’s approval always comes to its parent thread, so it also shows above the
                composer.
              </p>
            </div>
          ) : (
            <div className="q-ph-body">
              <div className="q-empty q-sub-empty">
                <h3>No subagents in this thread</h3>
                <p>
                  Delegated work will appear here with its host, harness, budget and inherited
                  approvals. Delegation is not exposed by the daemon yet.
                </p>
              </div>
            </div>
          )
        ) : (
          <div className="q-ph-body">
            <div className="q-ph-sum">
              <span className="q-btn secondary sm">
                All {Math.max(changes.turns.length, 1)} turns{" "}
                <ChevronDownIcon className="q-icon s12" strokeWidth={1.5} aria-hidden />
              </span>
              <span className="grow" />
              <span className="q-num q-pm">
                <span className="q-add">+{changes.added}</span>{" "}
                <span className="q-del">−{changes.removed}</span>
              </span>
            </div>
            <div className="q-ph-gh" style={{ display: "flex", alignItems: "center", gap: 6 }}>
              <EntityAvatar
                seed={harnessSeed(thread.host, thread.harness)}
                kind="harness"
                label={harnessLabel(thread.harness)}
                size={14}
              />
              {harnessLabel(thread.harness)} on {thread.host} · since{" "}
              {absoluteTime(thread.last_activity)}
            </div>
            {changes.error ? (
              <p className="q-ph-note">{changes.error}</p>
            ) : files.length === 0 ? (
              <p className="q-ph-note">
                {changes.loading ? "Loading changes…" : "No changes in this thread yet."}
              </p>
            ) : (
              <>
                <div className="q-ph-group">
                  {files.map((file) => {
                    const { directory, name } = splitPath(file.path);
                    return (
                      <button
                        key={file.path}
                        type="button"
                        className="q-ph-li"
                        onClick={() => openFile(file.path)}
                      >
                        <span className="q-faint">
                          <FileIcon className="q-icon s18" strokeWidth={1.5} aria-hidden />
                        </span>
                        <span
                          className="grow q-mono"
                          style={{
                            fontSize: 14,
                            overflow: "hidden",
                            textOverflow: "ellipsis",
                            whiteSpace: "nowrap",
                          }}
                        >
                          <span className="q-faint">{directory}</span>
                          {name}
                        </span>
                        <span className="q-num q-pm">
                          <span className="q-add">+{file.added}</span>{" "}
                          <span className="q-del">−{file.removed}</span>
                        </span>
                        <ChevronDownIcon
                          className="q-icon s16 q-faint"
                          strokeWidth={1.5}
                          style={{ transform: "rotate(-90deg)" }}
                          aria-hidden
                        />
                      </button>
                    );
                  })}
                </div>
                {files[0] ? (
                  <>
                    <div className="q-ph-gh q-mono" style={{ fontSize: 13 }}>
                      {files[0].path}
                    </div>
                    <div className="q-ph-diff">
                      <FileDiff
                        bare
                        compact
                        path={files[0].path}
                        diff={changes.diffs[files[0].path] ?? null}
                        added={files[0].added}
                        removed={files[0].removed}
                        loading={changes.diffsLoading}
                        onOpen={() => openFile(files[0]?.path ?? "")}
                      />
                    </div>
                  </>
                ) : null}
              </>
            )}
            <p className="q-ph-note">
              Tap a file for its full diff. Editing files is a desktop task; from here you can hand
              a change back to the agent.
            </p>
            <p className="q-ph-desktop-only">
              Files and the IDE open on desktop. Press <span className="q-kbd">{MOD}E</span> there
              to switch views.
            </p>
          </div>
        )}
      </div>
    </>
  );
}
