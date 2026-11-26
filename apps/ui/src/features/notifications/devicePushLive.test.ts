// @vitest-environment jsdom

import { afterEach, describe, expect, it } from "vitest";

import { endedWorkingTurns } from "./devicePush";

afterEach(() => {
  window.localStorage.clear();
});

describe("ending a running turn clears its Live Update", () => {
  it("names only the threads that were working and stopped", () => {
    expect(
      endedWorkingTurns(
        {
          th_done: { tab: "working" },
          th_still: { tab: "working" },
          th_idle: { tab: "done" },
        },
        {
          th_done: { tab: "done" },
          // A queued turn starts straight away: the thread never stopped
          // working, and its Live Update must stay.
          th_still: { tab: "working" },
          th_idle: { tab: "done" },
        },
      ),
    ).toEqual(["th_done"]);
  });

  it("clears a thread that left the list as well", () => {
    expect(endedWorkingTurns({ th_gone: { tab: "working" } }, {})).toEqual(["th_gone"]);
  });

  it("ignores a subagent's own running turn", () => {
    expect(
      endedWorkingTurns(
        { th_child: { tab: "working", parent_thread_id: "th_parent" } },
        { th_child: { tab: "done", parent_thread_id: "th_parent" } },
      ),
    ).toEqual([]);
  });
});
