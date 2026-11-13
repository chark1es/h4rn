import {
  CheckCircle2Icon,
  CircleXIcon,
  ExternalLinkIcon,
  GitPullRequestIcon,
  HandIcon,
  ListFilterIcon,
  MessageSquareIcon,
  MessageSquareTextIcon,
} from "lucide-react";
import { useState } from "react";
import { AgentGlyph } from "@/components/agent/AgentGlyph";
import { EntityAvatar } from "@/components/agent/EntityAvatar";
import { ENTER, KeyGlyphs, MOD } from "@/components/common/Kbd";
import { glyphForAgentState } from "@/lib/agent";
import { harnessSeed, userSeed } from "@/lib/entities";

const PULLS = [
  { number: "#482", title: "Fix flaky auth test", status: "1 failing check", tone: "danger" },
  { number: "#479", title: "Relay retry with backoff", status: "Review requested", tone: "you" },
  { number: "#476", title: "Audit dependency tree", status: "All checks passed", tone: "" },
  { number: "#471", title: "Pin blobatar to 2.7.0", status: "Merged", tone: "" },
];

const CHECKS = [
  { name: "build", time: "1m 12s", ok: true },
  { name: "unit", time: "2m 04s", ok: true },
  { name: "lint", time: "18s", ok: true },
  { name: "typecheck", time: "41s", ok: true },
  { name: "e2e", time: "42s", ok: false, required: true },
];

/** Static Phase 5 surface: pull request conversation and checks. */
export function PullRequestsPage() {
  const [selected, setSelected] = useState("#482");
  const [popover, setPopover] = useState(true);

  return (
    <>
      <section className="q-pane" aria-label="Pull requests">
        <div className="q-pane-head">
          <h2>Pull requests</h2>
          <button type="button" className="q-ibtn" aria-label="Filter">
            <ListFilterIcon className="q-icon" aria-hidden />
          </button>
        </div>
        <div className="q-pane-sub">
          <div className="q-seg" role="tablist" aria-label="Pull request filters">
            <button type="button" role="tab" aria-selected>
              Open <span className="c">3</span>
            </button>
            <button type="button" role="tab" aria-selected={false}>
              Mine <span className="c">2</span>
            </button>
            <button type="button" role="tab" aria-selected={false}>
              Review <span className="c">1</span>
            </button>
          </div>
        </div>
        <div className="q-scroll">
          <div className="q-rows">
            {PULLS.map((pull) => (
              <button
                key={pull.number}
                type="button"
                className={`q-row ${selected === pull.number ? "sel" : ""}`}
                onClick={() => setSelected(pull.number)}
              >
                <span className="q-row-st" style={{ color: "var(--fg-2)" }}>
                  <GitPullRequestIcon className="q-icon s14" aria-hidden />
                </span>
                <span className="q-row-main">
                  <span className="q-row-l1">
                    <span className="q-row-title">{pull.title}</span>
                    <span className="q-row-time">{pull.number}</span>
                  </span>
                  <span className="q-row-l2">
                    <span className="q-row-meta">dev/api</span>
                    <span className={`q-row-status ${pull.tone}`}>{pull.status}</span>
                  </span>
                </span>
              </button>
            ))}
          </div>
        </div>
      </section>

      <main className="q-main" aria-label="Pull request">
        <header
          className="q-page-head"
          style={{ height: "auto", padding: "14px 16px 14px 32px", alignItems: "flex-start" }}
        >
          <div style={{ flex: 1, minWidth: 0 }}>
            <h1 style={{ fontSize: 17, lineHeight: "22px" }}>
              Fix flaky auth test{" "}
              <span className="q-faint" style={{ fontWeight: 400 }}>
                #482
              </span>
            </h1>
            <div
              className="q-muted"
              style={{ fontSize: 12, marginTop: 4, display: "flex", gap: 6, alignItems: "center" }}
            >
              <span className="q-badge ok" style={{ height: 18 }}>
                Open
              </span>{" "}
              <span className="mono" style={{ fontSize: 11.5 }}>
                h4rn/fix-flaky-auth
              </span>{" "}
              →{" "}
              <span className="mono" style={{ fontSize: 11.5 }}>
                main
              </span>{" "}
              · 3 commits · opened by you 40m ago · dev/api on GitHub
            </div>
          </div>
          <button type="button" className="q-btn secondary">
            <ExternalLinkIcon className="q-icon s14" aria-hidden /> Open on GitHub
          </button>
        </header>

        <div className="q-page">
          <div className="q-pr-cols">
            <div className="q-pr-main">
              <div className="q-tabs" role="tablist">
                <button type="button" role="tab" aria-selected>
                  Conversation <span className="c">4</span>
                </button>
                <button type="button" role="tab" aria-selected={false}>
                  Commits <span className="c">3</span>
                </button>
                <button type="button" role="tab" aria-selected={false}>
                  Checks <span className="c">5</span>
                </button>
                <button type="button" role="tab" aria-selected={false}>
                  Files <span className="c">2</span>
                </button>
              </div>

              <div className="q-cmt" style={{ paddingTop: 0 }}>
                <div className="q-cmt-h">
                  <EntityAvatar seed={userSeed("u_dev")} kind="user" label="you" size={18} />
                  <b>you</b> · 40m ago
                </div>
                <p>
                  Pins the clock in the test setup so <code className="mono">createPool()</code> no
                  longer reads <code className="mono">Date.now()</code> at import time. Adds a
                  regression test at the 30 s session boundary.
                </p>
                <p className="q-muted" style={{ fontSize: 13 }}>
                  Opened from thread{" "}
                  <a href="#thread" className="q-accent">
                    Fix flaky auth test
                  </a>{" "}
                  · Claude Code on atlas
                </p>
              </div>

              <div className="q-cmt">
                <div className="q-cmt-h">
                  <EntityAvatar seed={userSeed("u_mara")} kind="user" label="mara-k" size={18} />
                  <b>mara-k</b> requested changes · 12m ago
                </div>
                <div className="q-ctx-code">
                  <div className="fh">tests/setup.ts · line 10</div>
                  <div className="q-ln a">
                    <span className="n">9</span>
                    <span className="sg">+</span>
                    <span className="q-cx">
                      {
                        'export const clock = vi.useFakeTimers({ now: new Date("2026-01-01T00:00:00Z") });'
                      }
                    </span>
                  </div>
                  <div className="q-ln a">
                    <span className="n">10</span>
                    <span className="sg">+</span>
                    <span className="q-cx">
                      {"beforeAll(() => createPool({ now: () => clock.now() }));"}
                    </span>
                  </div>
                </div>
                <p>
                  This pool is never closed, so the e2e job leaks a connection per file. Close it in{" "}
                  <code className="mono">afterAll</code> and the e2e check should go green.
                </p>
                <div className="q-cmt-a" style={{ position: "relative" }}>
                  <button type="button" className="q-btn secondary">
                    <MessageSquareIcon className="q-icon s14" aria-hidden /> Reply
                  </button>
                  <button
                    type="button"
                    className="q-btn secondary"
                    aria-expanded={popover}
                    onClick={() => setPopover((value) => !value)}
                  >
                    <HandIcon className="q-icon s14" aria-hidden /> Hand to agent
                  </button>
                  {popover ? (
                    <div className="q-pop" style={{ left: 96, top: 36, width: 380 }}>
                      <div style={{ fontWeight: 600, marginBottom: 2 }}>
                        Hand this comment to an agent
                      </div>
                      <div className="q-muted" style={{ fontSize: 12, marginBottom: 12 }}>
                        Sends the comment, file and line, and the PR link.
                      </div>
                      <div
                        className="q-li"
                        style={{ borderTop: "1px solid var(--line)", minHeight: 40 }}
                      >
                        <AgentGlyph glyph={glyphForAgentState("running_tool")} size={14} />
                        <div className="grow">
                          <div className="t">This thread</div>
                          <div className="s">Fix flaky auth test · Claude Code on atlas</div>
                        </div>
                        <span className="q-accent">
                          <CheckCircle2Icon className="q-icon" aria-hidden />
                        </span>
                      </div>
                      <div className="q-li" style={{ minHeight: 40 }}>
                        <span style={{ width: 14 }} />
                        <div className="grow">
                          <div className="t">New thread</div>
                          <div className="s">Choose host and harness</div>
                        </div>
                      </div>
                      <p className="q-faint" style={{ fontSize: 12, margin: "10px 0 12px" }}>
                        The agent won’t push without your approval.
                      </p>
                      <div style={{ display: "flex", justifyContent: "flex-end", gap: 6 }}>
                        <button
                          type="button"
                          className="q-btn ghost"
                          onClick={() => setPopover(false)}
                        >
                          Cancel
                        </button>
                        <button type="button" className="q-btn primary">
                          Send to Claude Code{" "}
                          <span className="q-kbd">
                            <KeyGlyphs value={`${MOD}${ENTER}`} />
                          </span>
                        </button>
                      </div>
                    </div>
                  ) : null}
                </div>
              </div>

              <div className="q-cmt">
                <div className="q-cmt-h">
                  <EntityAvatar
                    seed={harnessSeed("atlas", "claude_code")}
                    kind="harness"
                    label="Claude Code"
                    size={16}
                  />
                  <b>Claude Code</b> <span>on atlas</span> · 9m ago
                </div>
                <p>
                  Added a deterministic clock for the token refresh path too. The e2e failure is the
                  pool leak mara-k pointed out; handing that back now.
                </p>
              </div>

              <div style={{ marginTop: 18 }}>
                <div className="q-composer">
                  <textarea aria-label="Comment" placeholder="Comment on #482" />
                  <div className="q-comp-f">
                    <span className="grow" />
                    <button type="button" className="q-model">
                      Comment
                    </button>
                    <button type="button" className="q-send">
                      <MessageSquareTextIcon className="q-icon s14" aria-hidden />
                    </button>
                  </div>
                </div>
              </div>
            </div>

            <aside className="q-pr-side">
              <div className="q-side-sec">
                <h3>
                  <span>Checks</span>
                  <span>4 of 5 passed</span>
                </h3>
                {CHECKS.map((check) => (
                  <div className="q-chk" key={check.name}>
                    <span className={check.ok ? "q-ok" : "q-danger"}>
                      {check.ok ? (
                        <CheckCircle2Icon className="q-icon s14" aria-hidden />
                      ) : (
                        <CircleXIcon className="q-icon s14" aria-hidden />
                      )}
                    </span>
                    <span className="grow">
                      {check.name}{" "}
                      {check.required ? <span className="q-faint">· required</span> : null}
                    </span>
                    <span className="q-faint num">{check.time}</span>
                  </div>
                ))}
                <div style={{ display: "flex", gap: 6, marginTop: 8 }}>
                  <button type="button" className="q-btn secondary">
                    View log
                  </button>
                  <button type="button" className="q-btn ghost">
                    Re-run failed
                  </button>
                </div>
              </div>
              <div className="q-side-sec">
                <h3>
                  <span>Reviews</span>
                </h3>
                <div className="q-chk">
                  <EntityAvatar seed={userSeed("u_mara")} kind="user" label="mara-k" size={16} />
                  <span className="grow">mara-k</span>
                  <span className="q-warn" style={{ fontSize: 12 }}>
                    Changes requested
                  </span>
                </div>
                <div className="q-chk">
                  <EntityAvatar seed={userSeed("u_ty")} kind="user" label="tyler-o" size={16} />
                  <span className="grow">tyler-o</span>
                  <span className="q-faint" style={{ fontSize: 12 }}>
                    Pending
                  </span>
                </div>
              </div>
              <div className="q-side-sec" style={{ border: 0 }}>
                <h3>
                  <span>Linked thread</span>
                </h3>
                <div className="q-chk">
                  <AgentGlyph glyph={glyphForAgentState("running_tool")} size={14} />
                  <span className="grow">Fix flaky auth test</span>
                  <span className="q-faint" style={{ fontSize: 12 }}>
                    atlas
                  </span>
                </div>
              </div>
            </aside>
          </div>
        </div>
      </main>
    </>
  );
}
