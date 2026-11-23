/**
 * The guest page is a single static HTML document (no build step), served by
 * both the daemon and Connect. These tests run its real script in jsdom: the
 * protocol events it receives are folded into DOM, so a streamed reply, a
 * reload replay and a plan update each render exactly once.
 */

import { readFileSync } from "node:fs";
// jsdom ships no type declarations; the test only uses its constructor.
// @ts-expect-error -- jsdom has no bundled types and adding @types/jsdom for one test is not worth it.
import { JSDOM } from "jsdom";
import { afterEach, describe, expect, it } from "vitest";

const pageHtml = readFileSync(
  new URL("../../../../../crates/h4rn-protocol/src/guest-page.html", import.meta.url),
  "utf8",
);

type Snapshot = {
  state: string;
  thread: {
    thread_id: string;
    title: string;
    harness: string;
    model: string;
    updated_at: string;
  };
  events: Array<Record<string, unknown>>;
  next_seq: number;
  notice?: string | null;
};

const thread = {
  thread_id: "th_1",
  title: "Shared thread",
  harness: "claude_code",
  model: "fake-model",
  updated_at: "2026-01-01T00:00:00Z",
};

const turn = "turn_1";

function snapshot(events: Array<Record<string, unknown>>, nextSeq: number): Snapshot {
  return { state: "active", thread, events, next_seq: nextSeq, notice: null };
}

function delta(itemId: string, text: string, at: string) {
  return {
    seq: 1,
    event: {
      type: "message_delta",
      thread_id: "th_1",
      turn_id: turn,
      item_id: itemId,
      text,
    },
    at,
  };
}

function completed(itemId: string, text: string) {
  return {
    seq: 2,
    event: {
      type: "item_completed",
      thread_id: "th_1",
      turn_id: turn,
      item: {
        kind: "message",
        item_id: itemId,
        role: "assistant",
        text,
        attachments: [],
      },
    },
  };
}

function plan(entries: string[]) {
  return {
    seq: 3,
    event: {
      type: "plan",
      thread_id: "th_1",
      turn_id: turn,
      entries: entries.map((title) => ({ title, status: "pending" })),
    },
  };
}

function boot(responses: Array<Snapshot | Error>) {
  const timers: Array<() => void> = [];
  let calls = 0;
  const dom = new JSDOM(pageHtml, {
    runScripts: "dangerously",
    url: "http://localhost/guest/tok",
    beforeParse(window: {
      setTimeout: (handler: () => void) => number;
      fetch: (input: string, init?: unknown) => Promise<unknown>;
    }) {
      window.setTimeout = ((handler: () => void) => {
        timers.push(handler);
        return timers.length;
      }) as typeof window.setTimeout;
      window.fetch = (async () => {
        const next = responses[Math.min(calls, responses.length - 1)];
        calls += 1;
        if (next instanceof Error) throw next;
        return {
          ok: true,
          status: 200,
          json: async () => next,
          text: async () => "",
        };
      }) as typeof window.fetch;
    },
  });
  return {
    window: dom.window,
    document: dom.window.document,
    calls: () => calls,
    async domFlush() {
      await new Promise((resolve) => setTimeout(resolve, 0));
    },
    async nextPoll() {
      const batch = timers.splice(0);
      for (const handler of batch) handler();
      await new Promise((resolve) => setTimeout(resolve, 0));
      await new Promise((resolve) => setTimeout(resolve, 0));
    },
    close: () => dom.window.close(),
  };
}

let opened: Array<{ close: () => void }> = [];

afterEach(() => {
  for (const page of opened) page.close();
  opened = [];
});

function openPage(...args: Parameters<typeof boot>) {
  const page = boot(...args);
  opened.push(page);
  return page;
}

describe("the guest page", () => {
  it("renders a streamed reply once, replacing it when the item completes", async () => {
    const page = openPage([
      snapshot([delta("msg_1", "Hel", "2026-01-01T00:00:01Z")], 2),
      snapshot([delta("msg_1", "lo", "2026-01-01T00:00:02Z"), completed("msg_1", "Hello")], 4),
    ]);
    await page.domFlush();
    expect(page.calls()).toBe(1);
    expect(page.document.querySelectorAll(".prose")).toHaveLength(1);
    expect(page.document.querySelector(".prose")?.textContent).toBe("Hel");

    await page.nextPoll();
    const prose = page.document.querySelectorAll(".prose");
    expect(prose).toHaveLength(1);
    expect(prose[0]?.textContent).toBe("Hello");
  });

  it("renders a reload replay once even when it carries the deltas too", async () => {
    const page = openPage([
      snapshot([delta("msg_1", "Hel", "2026-01-01T00:00:01Z"), completed("msg_1", "Hello")], 4),
    ]);
    await page.domFlush();
    const prose = page.document.querySelectorAll(".prose");
    expect(prose).toHaveLength(1);
    expect(prose[0]?.textContent).toBe("Hello");
  });

  it("does not render a completion that a replay already showed", async () => {
    const page = openPage([
      snapshot([completed("msg_1", "Hello")], 3),
      snapshot([completed("msg_1", "Hello")], 4),
    ]);
    await page.domFlush();
    expect(page.document.querySelectorAll(".prose")).toHaveLength(1);

    await page.nextPoll();
    expect(page.document.querySelectorAll(".prose")).toHaveLength(1);
  });

  it("replaces a plan update instead of appending another list", async () => {
    const page = openPage([
      snapshot([plan(["Read the file", "Change it"])], 2),
      snapshot([plan(["Read the file", "Change it", "Test it"])], 3),
    ]);
    await page.domFlush();
    expect(page.document.querySelectorAll("ol.plan")).toHaveLength(1);
    expect(page.document.querySelectorAll("ol.plan li")).toHaveLength(2);

    await page.nextPoll();
    const lists = page.document.querySelectorAll("ol.plan");
    expect(lists).toHaveLength(1);
    expect(lists[0]?.querySelectorAll("li")).toHaveLength(3);
  });

  it("keeps the waiting notice visible while it retries", async () => {
    const page = openPage([new Error("offline")]);
    await page.domFlush();
    const notice = page.document.getElementById("notice");
    expect(notice?.hidden).toBe(false);
    expect(page.document.getElementById("notice-title")?.textContent).toBe(
      "Waiting for the connection",
    );
  });
});
