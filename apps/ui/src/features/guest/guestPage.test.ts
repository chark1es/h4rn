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

function plan(entries: string[], turnId = turn) {
  return {
    seq: 3,
    event: {
      type: "plan",
      thread_id: "th_1",
      turn_id: turnId,
      // The protocol's PlanStep is `content`, `status`, `priority`; a plan
      // entry never carries a `title`.
      entries: entries.map((content) => ({
        content,
        status: "pending",
        priority: "medium",
      })),
    },
  };
}

function boot(responses: Array<Snapshot | Error>, servedState = "active") {
  const timers: Array<() => void> = [];
  let calls = 0;
  // The daemon substitutes the link's state into the page it serves.
  const html = pageHtml.replace("__STATE__", servedState);
  const dom = new JSDOM(html, {
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

  it("renders the agent's own plan text and keys a list by its turn", async () => {
    const page = openPage([
      snapshot([plan(["Read the file", "Change it"])], 2),
      snapshot([plan(["Read the file", "Change it", "Test it"])], 3),
    ]);
    await page.domFlush();
    const lists = page.document.querySelectorAll("ol.plan");
    expect(lists).toHaveLength(1);
    // The real protocol payload carries the step in `content`, so a generated
    // "Step 1" would mean the text was dropped.
    expect(lists[0]?.querySelectorAll("li")[0]?.textContent).toBe("Read the file");
    expect(lists[0]?.querySelectorAll("li")).toHaveLength(2);

    await page.nextPoll();
    const updated = page.document.querySelectorAll("ol.plan");
    expect(updated).toHaveLength(1);
    expect(updated[0]?.querySelectorAll("li")).toHaveLength(3);
    expect(updated[0]?.querySelectorAll("li")[2]?.textContent).toBe("Test it");
  });

  it("keeps each turn's plan under that turn", async () => {
    const page = openPage([
      snapshot(
        [
          {
            seq: 1,
            event: {
              type: "turn_started",
              thread_id: "th_1",
              turn_id: "turn_a",
              started_at: "2026-01-01T00:00:01Z",
            },
          },
          plan(["First turn step"], "turn_a"),
          {
            seq: 3,
            event: {
              type: "turn_started",
              thread_id: "th_1",
              turn_id: "turn_b",
              started_at: "2026-01-01T00:00:02Z",
            },
          },
          plan(["Second turn step"], "turn_b"),
        ],
        4,
      ),
    ]);
    await page.domFlush();
    const turns = page.document.querySelectorAll("section.turn");
    expect(turns).toHaveLength(2);
    expect(turns[0]?.querySelector("ol.plan li")?.textContent).toBe("First turn step");
    expect(turns[1]?.querySelector("ol.plan li")?.textContent).toBe("Second turn step");
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

  it("draws the attach control's focus ring on the visible label with the focus token", () => {
    // The file input is invisible (opacity 0, full label overlay), so the
    // browser's default ring on it would never be seen. The ring belongs on the
    // label the person sees, and DESIGN.md reserves the focus token for it.
    const dom = new JSDOM(pageHtml.replace("__STATE__", "active"));
    const { document } = dom.window;
    const label = document.querySelector("label.attach");
    expect(label).not.toBeNull();
    expect(label?.querySelector("input[type=file]")).not.toBeNull();

    const rules: Array<Record<string, unknown>> = [];
    const collect = (list: ArrayLike<Record<string, unknown>>) => {
      for (const rule of Array.from(list)) {
        rules.push(rule);
        if (rule.cssRules) collect(rule.cssRules as ArrayLike<Record<string, unknown>>);
      }
    };
    collect(document.styleSheets[0]?.cssRules ?? []);

    const ring = rules.find((rule) => rule.selectorText === ".attach:has(:focus-visible)") as
      | { style?: { outline?: string } }
      | undefined;
    expect(ring, "the visible label has no focus ring rule").toBeTruthy();
    expect(ring?.style?.outline).toContain("var(--focus)");
    expect(ring?.style?.outline).not.toContain("var(--accent)");

    // Both themes define the token, so the ring has a value in dark mode too.
    const tokens = rules
      .filter((rule) => rule.selectorText === ":root")
      .map(
        (rule) =>
          (rule.style as { getPropertyValue?: (name: string) => string }).getPropertyValue?.(
            "--focus",
          ) ?? "",
      )
      .filter(Boolean);
    expect(tokens.length).toBe(2);
    expect(new Set(tokens).size).toBe(2);
    // A mouse click must not draw the ring: only keyboard focus does.
    expect(pageHtml).not.toContain(".attach:focus-within");
    dom.window.close();
  });

  const servedStates = [
    ["expired", "This link expired"],
    ["revoked", "Sharing stopped"],
    ["disabled", "Guest links are off"],
    ["unbound", "Open this link in the browser that first used it"],
  ] as const;

  for (const [served, title] of servedStates) {
    it(`starts a ${served} link on its notice and hides the composer`, async () => {
      const page = openPage([new Error("offline")], served);
      // The state the server rendered applies synchronously, before any poll.
      const notice = page.document.getElementById("notice");
      expect(notice?.hidden).toBe(false);
      expect(page.document.getElementById("notice-title")?.textContent).toBe(title);
      // The composer never appears for a link that cannot submit.
      expect(page.document.getElementById("composer-wrap")?.hidden).toBe(true);
      await page.domFlush();
      expect(page.document.getElementById("composer-wrap")?.hidden).toBe(true);
    });
  }
});
