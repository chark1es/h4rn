import { CheckIcon, CircleXIcon, KeyRoundIcon, PlusIcon } from "lucide-react";

import { AgentGlyph } from "@/components/agent/AgentGlyph";
import { EntityAvatar } from "@/components/agent/EntityAvatar";
import { readBootConfig } from "@/lib/client/config";
import { harnessSeed, machineSeed, subagentSeed } from "@/lib/entities";
import { openNewThread, pushToast, useApp } from "@/lib/store/appStore";

/**
 * Static Phase 3 shell. The wire protocol does not expose subagent runs yet, so
 * the populated tree is mock-mode only; a live thread gets the honest empty
 * state below.
 */
export function SubagentsPanel({ threadId }: { threadId: string }) {
  const summary = useApp((state) => state.threads[threadId]);
  const mock = readBootConfig().mock;

  return (
    <>
      <div className="q-rp-bar">
        <span className="q-faint" style={{ fontSize: 12 }}>
          {mock ? "3 subagents · 1 waiting on you" : "No subagents"}
        </span>
        <span className="grow" />
        <button type="button" className="q-btn ghost" onClick={openNewThread}>
          <PlusIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
          Delegate
        </button>
      </div>
      <div className="q-rp-body q-scroll">
        {mock ? (
          <>
            <div className="q-tree">
              <div className="q-node parent">
                <span className="gl">
                  <AgentGlyph
                    glyph={{ kind: "orb", state: "working", label: "Running" }}
                    size={14}
                  />
                </span>
                <div className="bd">
                  <div className="t">
                    <span>{summary?.title ?? "Fix flaky auth test"}</span>
                    <span className="q-faint q-num" style={{ fontSize: 12 }}>
                      4:12
                    </span>
                  </div>
                  <div className="w">
                    <EntityAvatar
                      seed={machineSeed("atlas")}
                      kind="machine"
                      label="atlas"
                      size={14}
                    />
                    atlas ·{" "}
                    <EntityAvatar
                      seed={harnessSeed("atlas", "claude_code")}
                      kind="harness"
                      label="Claude Code"
                      size={12}
                    />{" "}
                    Claude Code
                  </div>
                  <div className="s q-num">Running · 18.4k tokens</div>
                </div>
              </div>
              <div className="q-kids">
                <div className="q-node">
                  <span className="gl" style={{ color: "var(--fg-2)" }}>
                    <CheckIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
                  </span>
                  <div className="bd">
                    <div className="t">
                      <span>Audit test helpers</span>
                      <span className="q-faint q-num" style={{ fontSize: 12 }}>
                        1:48
                      </span>
                    </div>
                    <div className="w">
                      <EntityAvatar
                        seed={subagentSeed("atlas", "thr_a", "a1")}
                        kind="subagent"
                        label="subagent"
                        size={14}
                      />
                      atlas · Codex
                    </div>
                    <div className="s q-num">Done · 6.2k tokens · native resume</div>
                  </div>
                </div>
                <div className="q-node sel">
                  <span className="gl">
                    <AgentGlyph
                      glyph={{ kind: "orb", state: "breathing", paused: true, label: "Held" }}
                      size={14}
                    />
                  </span>
                  <div className="bd">
                    <div className="t">
                      <span>Reproduce CI failure</span>
                      <span className="q-faint q-num" style={{ fontSize: 12 }}>
                        0:31
                      </span>
                    </div>
                    <div className="w">
                      <EntityAvatar
                        seed={subagentSeed("orion", "thr_a", "r2")}
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
                      orion ·{" "}
                      <EntityAvatar
                        seed={harnessSeed("orion", "codex")}
                        kind="harness"
                        label="Codex"
                        size={12}
                      />{" "}
                      Codex
                    </div>
                    <div className="s q-num">
                      <span className="q-accent" style={{ fontWeight: 500 }}>
                        Needs approval
                      </span>{" "}
                      · turn 3 of 20 · 2.1k tokens
                    </div>
                    <div
                      className="prog"
                      role="progressbar"
                      aria-label="Budget used 15%"
                      aria-valuenow={15}
                      aria-valuemin={0}
                      aria-valuemax={100}
                    >
                      <i style={{ width: "15%" }} />
                    </div>
                  </div>
                </div>
                <div className="q-node">
                  <span className="gl q-danger">
                    <CircleXIcon className="q-icon s14" strokeWidth={1.5} aria-hidden />
                  </span>
                  <div className="bd">
                    <div className="t">
                      <span style={{ color: "var(--fg-2)" }}>Check clock skew on buildbox</span>
                      <span className="q-faint q-num" style={{ fontSize: 12 }}>
                        0:04
                      </span>
                    </div>
                    <div className="w">
                      <EntityAvatar
                        seed={subagentSeed("buildbox", "thr_a", "c3")}
                        kind="subagent"
                        label="subagent"
                        size={14}
                      />
                      buildbox · Codex
                    </div>
                    <div className="s">Couldn’t start: buildbox is offline</div>
                  </div>
                </div>
              </div>
            </div>
            <div className="q-insp-sec">
              <h3>Reproduce CI failure</h3>
              <dl className="q-kv">
                <dt>Runs on</dt>
                <dd>orion · Codex 0.98.2</dd>
                <dt>Model</dt>
                <dd>gpt-5.1-codex</dd>
                <dt>Context</dt>
                <dd>Summary handoff · 1.2k tokens</dd>
                <dt>Budget</dt>
                <dd className="q-num">20 turns · 15 min · 50k tokens</dd>
                <dt>Approvals</dt>
                <dd>Inherited from parent</dd>
                <dt>Credential</dt>
                <dd style={{ display: "flex", alignItems: "center", gap: 4 }}>
                  <KeyRoundIcon className="q-icon s12" strokeWidth={1.5} aria-hidden /> Stored on
                  orion
                </dd>
              </dl>
              <div style={{ display: "flex", gap: 6, marginTop: 14 }}>
                <button
                  type="button"
                  className="q-btn secondary"
                  onClick={() =>
                    pushToast({
                      title: "Subagent threads arrive with the wire protocol.",
                      variant: "info",
                    })
                  }
                >
                  Open thread
                </button>
                <button
                  type="button"
                  className="q-btn ghost"
                  onClick={() =>
                    pushToast({
                      title: "Subagent control arrives with the wire protocol.",
                      variant: "info",
                    })
                  }
                >
                  Cancel subagent
                </button>
              </div>
            </div>
          </>
        ) : (
          <div className="q-empty q-sub-empty">
            <h3>No subagents in this thread</h3>
            <p>
              Delegated work will appear here with its host, harness, budget and inherited
              approvals. Delegation is not exposed by the daemon yet.
            </p>
          </div>
        )}
      </div>
    </>
  );
}

/** The panel header badge: only the mock thread has a populated tree today. */
export function subagentCount(threadId: string): number {
  return readBootConfig().mock && threadId === "thr_a" ? 3 : 0;
}
