import { Terminal } from "@xterm/xterm";
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
      registerDcsHandler(
        id: { final: string; intermediates?: string },
        cb: (data: string, params: (number | number[])[]) => boolean,
      ) {
        const dcsKey = `${id.intermediates ?? ""}|${id.final}`;
        dcs.set(dcsKey, cb as unknown as Handler);
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

  it("expects one report for every colour slot xterm's parser reports", async () => {
    const term = new Terminal({ cols: 80, rows: 24 });
    const tracker = new TerminalQueryTracker();
    const hooks = registerTerminalQueryHooks(term.parser, tracker);
    try {
      // A chained OSC 10 query makes xterm report foreground, background and
      // cursor as three separate data events.
      await parse(term, "\x1b]10;?;?;?\x1b\\");
      expect(tracker.take("\x1b]10;rgb:cccc/cccc/cccc\x1b\\")).toBe(true);
      expect(tracker.take("\x1b]11;rgb:0000/0000/0000\x1b\\")).toBe(true);
      expect(tracker.take("\x1b]12;rgb:cccc/cccc/cccc\x1b\\")).toBe(true);

      // OSC 4 carries one query per `index;?` pair.
      await parse(term, "\x1b]4;1;?;2;?\x1b\\");
      expect(tracker.take("\x1b]4;1;rgb:cccc/0000/0000\x1b\\")).toBe(true);
      expect(tracker.take("\x1b]4;2;rgb:0000/cccc/0000\x1b\\")).toBe(true);
    } finally {
      for (const hook of hooks) hook.dispose();
      term.dispose();
    }
  });

  it("tracks every slot of a large batched palette query", async () => {
    const term = new Terminal({ cols: 80, rows: 24 });
    const tracker = new TerminalQueryTracker();
    const hooks = registerTerminalQueryHooks(term.parser, tracker);
    try {
      // More slots than the old 32-entry window: xterm answers all of them,
      // so every answer must still be marked as a reply.
      const slots = Array.from({ length: 40 }, (_, index) => `${index};?`).join(";");
      await parse(term, `\x1b]4;${slots}\x1b\\`);

      for (let index = 0; index < 40; index += 1) {
        expect(
          tracker.take(`\x1b]4;${index};rgb:cccc/0000/0000\x1b\\`),
          `palette slot ${index}`,
        ).toBe(true);
      }
      // Every slot is used up; an identical report is input again.
      expect(tracker.take("\x1b]4;39;rgb:cccc/0000/0000\x1b\\")).toBe(false);
    } finally {
      for (const hook of hooks) hook.dispose();
      term.dispose();
    }
  });

  it("records the output chunk and ordinal that carried each query", () => {
    const tracker = new TerminalQueryTracker();

    tracker.beginChunk(5);
    tracker.expect(/pattern/);
    expect(tracker.takeReply("pattern")).toEqual({ position: 5, ordinal: 0 });
    tracker.endChunk();

    // Two queued chunks: a query is attributed to the one being parsed.
    tracker.beginChunk(6);
    tracker.beginChunk(7);
    tracker.expect(/first/);
    expect(tracker.takeReply("first")).toEqual({ position: 6, ordinal: 0 });
    tracker.endChunk();
    tracker.expect(/second/);
    expect(tracker.takeReply("second")).toEqual({ position: 7, ordinal: 0 });
    tracker.endChunk();

    // A query seen before any output names no position (old-client shape).
    tracker.expect(/bare/);
    expect(tracker.takeReply("bare")).toEqual({ position: 0, ordinal: 0 });
    expect(tracker.takeReply("nothing")).toBeNull();
  });

  it("numbers every answer in one chunk, including after an earlier answer", () => {
    const tracker = new TerminalQueryTracker();

    tracker.beginChunk(9);
    tracker.expect(/first/);
    tracker.expect(/second/);
    // The first answer arriving before the next query is recorded must not
    // renumber the second: xterm parses and answers in one pass.
    expect(tracker.takeReply("first")).toEqual({ position: 9, ordinal: 0 });
    tracker.expect(/third/);
    expect(tracker.takeReply("second")).toEqual({ position: 9, ordinal: 1 });
    expect(tracker.takeReply("third")).toEqual({ position: 9, ordinal: 2 });
  });

  it("names the chunk where a query split across frames starts", async () => {
    const term = new Terminal({ cols: 80, rows: 24 });
    const tracker = new TerminalQueryTracker();
    const hooks = registerTerminalQueryHooks(term.parser, tracker);
    const answers: string[] = [];
    const onData = term.onData((data) => answers.push(data));
    try {
      // A real xterm parses DECRQM (`CSI ? Ps $ p`) and answers through
      // onData. The query starts in chunk 7 and ends in chunk 8, so every
      // viewer that saw it names it by chunk 7.
      tracker.beginChunk(7, bytes("\x1b[?12"));
      await parse(term, "\x1b[?12");
      tracker.endChunk();

      tracker.beginChunk(8, bytes("$p"));
      await parse(term, "$p");
      tracker.endChunk();

      expect(answers).toHaveLength(1);
      expect(tracker.takeReply(answers[0] ?? "")).toEqual({ position: 7, ordinal: 0 });
    } finally {
      onData.dispose();
      for (const hook of hooks) hook.dispose();
      term.dispose();
    }
  });

  it("names the shared query the same way for a viewer that missed the split", async () => {
    // Chunk 5 ends inside a DECRQM query; chunk 6 finishes it and asks a
    // second one. This is the reproduction from the PR: the early viewer
    // parses both chunks, the late one attached with chunk 5 in its snapshot.
    const chunk5 = "\x1b[?12";
    const chunk6 = ";2$p\x1b[?25$p";

    const early = await repliesFor([
      [5, chunk5],
      [6, chunk6],
    ]);
    const late = await repliesFor([[6, chunk6]]);

    // Both viewers name the second query by chunk 6, its first byte. The
    // query that starts in chunk 5 is only ever seen by the early viewer.
    expect(early).toEqual([
      { position: 5, ordinal: 0 },
      { position: 6, ordinal: 0 },
    ]);
    expect(late).toEqual([{ position: 6, ordinal: 0 }]);
    expect(late[0]).toEqual(early[1]);

    // Delivering the chunks in the other order (a viewer whose snapshot
    // contains chunk 6 instead) cannot make the late viewer's answer collide
    // with the early one either.
    const other = await repliesFor([
      [5, "\x1b[?12;2$p\x1b[?25"],
      [6, "$p"],
    ]);
    const fresh = await repliesFor([[6, "$p"]]);
    expect(other).toEqual([
      { position: 5, ordinal: 0 },
      { position: 5, ordinal: 1 },
    ]);
    expect(fresh).toEqual([]);
  });

  it("does not attribute a later query to an unrelated split escape", async () => {
    // Chunk 5 ends inside a screen-clear sequence, not a query. The mode query
    // in chunk 6 starts there and must not inherit chunk 5's position.
    expect(
      await repliesFor([
        [5, "\x1b[2"],
        [6, "J\x1b[?25$p"],
      ]),
    ).toEqual([{ position: 6, ordinal: 0 }]);

    // A query that really is split still names the chunk it started in.
    expect(
      await repliesFor([
        [5, "\x1b[?25"],
        [6, "$p"],
      ]),
    ).toEqual([{ position: 5, ordinal: 0 }]);
  });

  it("names a split DA1 query by the chunk that carried its ESC", async () => {
    // xterm records `params = [0]` for a sequence with no parameters, so the
    // bytes that arrived before the final `c` carry a matching signature.
    // Split after `ESC[`:
    expect(
      await repliesFor([
        [5, "\x1b["],
        [6, "c\x1b[6n"],
      ]),
    ).toEqual([
      { position: 5, ordinal: 0 },
      { position: 6, ordinal: 0 },
    ]);
    // Split after `ESC`: the same query, named by the same chunk.
    expect(
      await repliesFor([
        [5, "\x1b"],
        [6, "[c\x1b[6n"],
      ]),
    ).toEqual([
      { position: 5, ordinal: 0 },
      { position: 6, ordinal: 0 },
    ]);
    // A viewer whose snapshot already held chunk 5 parses `c` as text and
    // answers only the cursor-position query; that answer keeps its name.
    expect(await repliesFor([[6, "c\x1b[6n"]])).toEqual([{ position: 6, ordinal: 0 }]);

    // DA2 carries a prefix byte and no parameters, and must match too.
    expect(
      await repliesFor([
        [5, "\x1b[>"],
        [6, "c\x1b[6n"],
      ]),
    ).toEqual([
      { position: 5, ordinal: 0 },
      { position: 6, ordinal: 0 },
    ]);
  });

  it("names a split DECRQSS query by the chunk that carried its ESC P", async () => {
    // `ESC P $ q m ESC \` is a DCS with no parameters, so xterm reports `[0]`
    // to the hook that saw it start in chunk 5 and end in chunk 6.
    expect(
      await repliesFor([
        [5, "\x1bP$"],
        [6, "qm\x1b\\\x1b[6n"],
      ]),
    ).toEqual([
      { position: 5, ordinal: 0 },
      { position: 6, ordinal: 0 },
    ]);
    // Delivered whole, both queries start in the same chunk.
    expect(await repliesFor([[5, "\x1bP$qm\x1b\\\x1b[6n"]])).toEqual([
      { position: 5, ordinal: 0 },
      { position: 5, ordinal: 1 },
    ]);
    // The late viewer's snapshot held the DCS, so it only answers the CPR.
    expect(await repliesFor([[6, "qm\x1b\\\x1b[6n"]])).toEqual([{ position: 6, ordinal: 0 }]);
  });

  it("restarts on an ESC inside an open CSI, OSC, DCS or string", async () => {
    // Chunk 6 opens a CSI, an ESC cancels it, and chunk 7 completes the
    // query that ESC began. Naming the answer by chunk 6 is what a viewer
    // that attached after chunk 5 computes; a scanner that kept the ESC as a
    // parameter byte names it by chunk 7 instead.
    const all = await repliesFor([
      [5, "\x1b["],
      [6, "2\x1b"],
      [7, "[?25$p"],
    ]);
    const late = await repliesFor([
      [6, "2\x1b"],
      [7, "[?25$p"],
    ]);
    expect(all).toEqual([{ position: 6, ordinal: 0 }]);
    expect(late).toEqual(all);

    // The same inside an OSC: the ESC ends the colour query in chunk 6 and
    // begins the next escape, so the CPR that follows is not OSC data.
    const term = new Terminal({ cols: 80, rows: 24 });
    const tracker = new TerminalQueryTracker();
    const hooks = registerTerminalQueryHooks(term.parser, tracker);
    const answers: string[] = [];
    const onData = term.onData((data) => answers.push(data));
    try {
      tracker.beginChunk(5, bytes("\x1b]10;?"));
      await parse(term, "\x1b]10;?");
      tracker.endChunk();
      tracker.beginChunk(6, bytes("\x1b"));
      await parse(term, "\x1b");
      tracker.endChunk();
      tracker.beginChunk(7, bytes("[6n"));
      await parse(term, "[6n");
      tracker.endChunk();

      expect(answers).toEqual(["\x1b[1;1R"]);
      expect(tracker.takeReply(answers[0] ?? "")).toEqual({ position: 6, ordinal: 0 });
      // xterm answers colour queries through its renderer, not `onData`, so
      // the report shape is supplied here; it names the chunk the query's
      // own ESC came in.
      expect(tracker.takeReply("\x1b]10;rgb:cccc/cccc/cccc\x1b\\")).toEqual({
        position: 5,
        ordinal: 0,
      });
    } finally {
      onData.dispose();
      for (const hook of hooks) hook.dispose();
      term.dispose();
    }
  });

  it("cancels an open sequence on CAN and SUB", async () => {
    // `ESC[?25` then CAN returns xterm's parser to ground, so the ESC in
    // chunk 6 begins a fresh escape and the query that completes in chunk 7
    // starts there. A scanner that kept the CAN as a parameter byte stays
    // inside the old CSI and names the answer by chunk 7, where the final
    // byte landed.
    expect(
      await repliesFor([
        [5, "\x1b[?25\x18"],
        [6, "\x1b"],
        [7, "[?25$p"],
      ]),
    ).toEqual([{ position: 6, ordinal: 0 }]);
    expect(
      await repliesFor([
        [5, "\x1b[?25\x1a"],
        [6, "\x1b"],
        [7, "[?25$p"],
      ]),
    ).toEqual([{ position: 6, ordinal: 0 }]);
  });

  it("names a query the same way at every two-chunk split", async () => {
    // The query follows a sequence that a split can cut anywhere. Whichever
    // chunk carries the query's ESC names the answer, in every split.
    const before = "\x1b[2J";
    const stream = `${before}\x1b[?25$p`;
    expect(await repliesFor([[5, stream]])).toEqual([{ position: 5, ordinal: 0 }]);
    for (let split = 1; split < stream.length; split += 1) {
      const answers = await repliesFor([
        [5, stream.slice(0, split)],
        [6, stream.slice(split)],
      ]);
      const expected = split > before.length ? 5 : 6;
      expect(answers, `split at ${split}`).toEqual([{ position: expected, ordinal: 0 }]);
    }
  });

  it("names a colour query split across frames by its starting chunk", async () => {
    const term = new Terminal({ cols: 80, rows: 24 });
    const tracker = new TerminalQueryTracker();
    const hooks = registerTerminalQueryHooks(term.parser, tracker);
    const answers: string[] = [];
    const onData = term.onData((data) => answers.push(data));
    try {
      // xterm's parser reports an OSC colour query when its terminator
      // arrives, so the query spans chunks 9 and 10. It does not answer this
      // query through onData, so the report shape is supplied here.
      tracker.beginChunk(9, bytes("\x1b]10;?"));
      await parse(term, "\x1b]10;?");
      tracker.endChunk();
      tracker.beginChunk(10, bytes("\x1b\\"));
      await parse(term, "\x1b\\");
      tracker.endChunk();

      expect(answers).toHaveLength(0);
      expect(tracker.takeReply("\x1b]10;rgb:cccc/cccc/cccc\x1b\\")).toEqual({
        position: 9,
        ordinal: 0,
      });
    } finally {
      onData.dispose();
      for (const hook of hooks) hook.dispose();
      term.dispose();
    }
  });

  it("does not carry an incomplete escape past a chunk without a query", async () => {
    // The chunk before the query ends mid-escape, so the pending sequence
    // completes while the next chunk is parsed. It is not the query, so the
    // query that follows starts in that next chunk.
    expect(
      await repliesFor([
        [5, "\x1b[2"],
        [6, "J\x1b[?25$p"],
      ]),
    ).toEqual([{ position: 6, ordinal: 0 }]);
  });

  it("forgets a query xterm did not answer when its chunk is done", async () => {
    // Enable window reports so xterm's parser lets the window-size query
    // through to the hook.
    const term = new Terminal({ cols: 80, rows: 24, windowOptions: { getWinSizeChars: true } });
    const tracker = new TerminalQueryTracker();
    const hooks = registerTerminalQueryHooks(term.parser, tracker);
    try {
      const mark = tracker.mark();
      await parse(term, "\x1b[18t");
      tracker.settle(mark);

      // A pasted report of the same shape is input, not a reply.
      expect(tracker.take("\x1b[8;24;80t")).toBe(false);

      // A later chunk's query is still tracked normally.
      const next = tracker.mark();
      await parse(term, "\x1b[18t");
      expect(tracker.take("\x1b[8;24;80t")).toBe(true);
      tracker.settle(next);
    } finally {
      for (const hook of hooks) hook.dispose();
      term.dispose();
    }
  });
});

/** Resolves once xterm has parsed `data`, as its write callback does. */
function parse(term: Terminal, data: string): Promise<void> {
  return new Promise((resolve) => term.write(data, resolve));
}

const bytes = (text: string) => new TextEncoder().encode(text);

/**
 * Feeds one viewer's chunks to a real xterm parser and returns the identities
 * of the answers it sent, in order.
 */
async function repliesFor(
  chunks: Array<[seq: number, text: string]>,
): Promise<Array<{ position: number; ordinal: number }>> {
  const term = new Terminal({ cols: 80, rows: 24 });
  const tracker = new TerminalQueryTracker();
  const hooks = registerTerminalQueryHooks(term.parser, tracker);
  const answers: string[] = [];
  const onData = term.onData((data) => answers.push(data));
  try {
    for (const [seq, text] of chunks) {
      tracker.beginChunk(seq, bytes(text));
      await parse(term, text);
      tracker.endChunk();
    }
    return answers
      .map((answer) => tracker.takeReply(answer))
      .filter((reply): reply is { position: number; ordinal: number } => reply !== null);
  } finally {
    onData.dispose();
    for (const hook of hooks) hook.dispose();
    term.dispose();
  }
}
