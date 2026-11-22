import { describe, expect, it } from "vitest";

import { registerTerminalQueryHooks, TerminalQueryTracker } from "./terminal-reply";

type Handler = (value: never) => boolean;

/** Records the parser hooks so a test can act like xterm's parser. */
function fakeParser() {
  const csi = new Map<string, Handler>();
  const osc = new Map<number, Handler>();
  const dcs = new Map<string, Handler>();
  const key = (id: { final: string; prefix?: string; intermediates?: string }) =>
    `${id.prefix ?? ""}|${id.intermediates ?? ""}|${id.final}`;
  return {
    csi,
    osc,
    dcs,
    parser: {
      registerCsiHandler(
        id: { final: string; prefix?: string; intermediates?: string },
        cb: Handler,
      ) {
        csi.set(key(id), cb);
        return { dispose: () => csi.delete(key(id)) };
      },
      registerOscHandler(ident: number, cb: Handler) {
        osc.set(ident, cb);
        return { dispose: () => osc.delete(ident) };
      },
      registerDcsHandler(id: { final: string; intermediates?: string }, cb: Handler) {
        const dcsKey = `${id.intermediates ?? ""}|${id.final}`;
        dcs.set(dcsKey, cb);
        return { dispose: () => dcs.delete(dcsKey) };
      },
    },
    /** Runs the hook for one query, as xterm's parser would. */
    query(id: { final: string; prefix?: string; intermediates?: string }, params: unknown[] = []) {
      return csi.get(key(id))?.(params as never);
    },
    oscQuery(ident: number, data: string) {
      return osc.get(ident)?.(data as never);
    },
    dcsQuery(intermediate: string, final: string, data: string) {
      return dcs.get(`${intermediate}|${final}`)?.(data as never);
    },
  };
}

describe("terminal query replies", () => {
  it("marks a cursor-position report only after a CPR query", () => {
    const fake = fakeParser();
    const tracker = new TerminalQueryTracker();
    registerTerminalQueryHooks(fake.parser, tracker);

    // Shift+F3 is CSI 1;2R and must stay input.
    expect(tracker.take("\x1b[1;2R")).toBe(false);

    expect(fake.query({ final: "n" }, [6])).toBe(false);
    expect(tracker.take("\x1b[12;40R")).toBe(true);
    // The query is used up; an identical chunk is input again.
    expect(tracker.take("\x1b[12;40R")).toBe(false);
  });

  it("marks device attribute, mode, window and colour answers", () => {
    const fake = fakeParser();
    const tracker = new TerminalQueryTracker();
    registerTerminalQueryHooks(fake.parser, tracker);

    fake.query({ final: "c" });
    expect(tracker.take("\x1b[?1;2c")).toBe(true);

    fake.query({ prefix: ">", final: "c" });
    expect(tracker.take("\x1b[>0;276;0c")).toBe(true);

    fake.query({ prefix: "?", intermediates: "$", final: "p" }, [12]);
    expect(tracker.take("\x1b[?12;2$y")).toBe(true);

    fake.query({ intermediates: "$", final: "p" }, [12]);
    expect(tracker.take("\x1b[12;1$y")).toBe(true);

    fake.query({ final: "t" }, [18]);
    expect(tracker.take("\x1b[8;24;80t")).toBe(true);

    fake.query({ final: "n" }, [5]);
    expect(tracker.take("\x1b[0n")).toBe(true);

    fake.oscQuery(10, "?");
    expect(tracker.take("\x1b]10;rgb:cccc/cccc/cccc\x1b\\")).toBe(true);

    fake.oscQuery(4, "1;?");
    expect(tracker.take("\x1b]4;1;rgb:cccc/0000/0000\x1b\\")).toBe(true);

    // DECRQSS asks about a setting and gets a status string back.
    fake.dcsQuery("$", "q", "m");
    expect(tracker.take("\x1bP1$r0m\x1b\\")).toBe(true);
  });

  it("leaves ordinary typing, paste and unsolicited answers as input", () => {
    const fake = fakeParser();
    const tracker = new TerminalQueryTracker();
    registerTerminalQueryHooks(fake.parser, tracker);

    for (const input of [
      "a",
      "\r",
      "\x1b",
      "\x1b[A",
      "\x1b[200~hello\x1b[201~",
      "ls -la\n",
      // An answer to a query this program never asked.
      "\x1b[?1;2c",
    ]) {
      expect(tracker.take(input), JSON.stringify(input)).toBe(false);
    }
  });

  it("does not record a query xterm will not answer", () => {
    const fake = fakeParser();
    const tracker = new TerminalQueryTracker();
    registerTerminalQueryHooks(fake.parser, tracker);

    // OSC 10 without `?` sets a colour rather than querying it.
    fake.oscQuery(10, "#cccccc");
    expect(tracker.take("\x1b]10;rgb:cccc/cccc/cccc\x1b\\")).toBe(false);
  });
});
