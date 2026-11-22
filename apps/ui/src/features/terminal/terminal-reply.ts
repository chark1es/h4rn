/**
 * Marks xterm's own answers to terminal queries.
 *
 * A program can ask the terminal for device attributes, cursor position, a
 * mode or a colour. xterm answers through the same `onData` path as the
 * person's keystrokes, so those answers need to be told apart from typed
 * input: viewers share one PTY and the daemon writes only one viewer's answer
 * (ADR-007), while keystrokes and paste must always go through.
 *
 * Guessing from the shape of the bytes is not enough: Shift+F3 is `CSI 1;2R`,
 * exactly a cursor-position report. Instead the parser hooks below note every
 * query xterm is about to answer, and the next data chunk that matches that
 * answer is marked as a reply. A sequence that looks like an answer with no
 * query pending stays input.
 */

// Patterns are strings because Biome rejects control characters in regex
// literals; each is anchored, so only a complete answer matches.
const PATTERNS = {
  // DA1: CSI ? 1 ; 2 c, DA2: CSI > 0 ; 276 ; 0 c
  deviceAttributes: "^\\x1b\\[[?>]?[0-9;]*c$",
  // DSR status: CSI 0 n
  statusReport: "^\\x1b\\[0n$",
  // CPR: CSI row ; col R, DECXCPR: CSI ? row ; col R
  cursorPosition: "^\\x1b\\[\\??[0-9]+;[0-9]+R$",
  // DECRQM: CSI ? mode ; state $ y, ANSI: CSI mode ; state $ y
  modeReport: "^\\x1b\\[\\??[0-9]+;[0-9]+\\$y$",
  // XTWINOPS 14/16/18: CSI 4 ; h ; w t, CSI 6 ; h ; w t, CSI 8 ; rows ; cols t
  windowReport: "^\\x1b\\[[468];[0-9]+;[0-9]+t$",
  // OSC 4;index;rgb:… ST
  indexedColor: "^\\x1b\\]4;[0-9]+;rgb:[0-9a-f]{1,4}/[0-9a-f]{1,4}/[0-9a-f]{1,4}\\x1b\\\\$",
  // OSC 10/11/12;rgb:… ST
  specialColor: "^\\x1b\\](?:10|11|12);rgb:[0-9a-f]{1,4}/[0-9a-f]{1,4}/[0-9a-f]{1,4}\\x1b\\\\$",
  // DECRQSS (DCS $ q): ESC P 1 $ r … ESC \ or ESC P 0 $ r ESC \
  statusString: "^\\x1bP[01]\\$r[^\\x1b]*\\x1b\\\\$",
} as const;

const DEVICE_ATTRIBUTES = new RegExp(PATTERNS.deviceAttributes);
const STATUS_REPORT = new RegExp(PATTERNS.statusReport);
const CURSOR_POSITION = new RegExp(PATTERNS.cursorPosition);
const MODE_REPORT = new RegExp(PATTERNS.modeReport);
const WINDOW_REPORT = new RegExp(PATTERNS.windowReport, "i");
const INDEXED_COLOR = new RegExp(PATTERNS.indexedColor, "i");
const SPECIAL_COLOR = new RegExp(PATTERNS.specialColor, "i");
const STATUS_STRING = new RegExp(PATTERNS.statusString);

/** The parser methods these hooks need; xterm's own parser satisfies it. */
export type TerminalQueryParser = {
  registerCsiHandler(
    id: { final: string; prefix?: string; intermediates?: string },
    callback: (params: (number | number[])[]) => boolean,
  ): { dispose(): void };
  registerOscHandler(ident: number, callback: (data: string) => boolean): { dispose(): void };
  registerDcsHandler(
    id: { final: string; prefix?: string; intermediates?: string },
    callback: (data: string) => boolean,
  ): { dispose(): void };
};

/** How many unanswered queries to remember before forgetting the oldest. */
const MAX_PENDING = 32;

/**
 * Notes the answer shapes xterm owes the program since the last chunk.
 *
 * Matching is by shape rather than a bare count, because one query may go
 * unanswered (xterm only reports window size when the option is enabled) and
 * a stale counter must not swallow a later keystroke.
 */
export class TerminalQueryTracker {
  private readonly expected: RegExp[] = [];

  /** Records that xterm is about to send one `pattern`-shaped answer. */
  expect(pattern: RegExp): void {
    this.expected.push(pattern);
    if (this.expected.length > MAX_PENDING) this.expected.shift();
  }

  /**
   * Whether `data` is xterm's answer to a pending query, consuming it.
   *
   * Returns false for every other chunk, including answer-shaped sequences
   * the person typed or pasted.
   */
  take(data: string): boolean {
    if (this.expected.length === 0) return false;
    const index = this.expected.findIndex((pattern) => pattern.test(data));
    if (index === -1) return false;
    this.expected.splice(index, 1);
    return true;
  }
}

/**
 * Registers parser hooks for every query xterm answers.
 *
 * Each hook returns `false`, so xterm still handles the sequence and writes
 * its answer through `onData`; the hook only records what answer to expect.
 * xterm does not implement XTGETTCAP; its DCS query is DECRQSS, which it does
 * answer and which is tracked here.
 */
export function registerTerminalQueryHooks(
  parser: TerminalQueryParser,
  tracker: TerminalQueryTracker,
): Array<{ dispose: () => void }> {
  const disposables: Array<{ dispose: () => void }> = [];
  const expectCsi = (
    id: { final: string; prefix?: string; intermediates?: string },
    expect: (params: (number | number[])[]) => RegExp | null,
  ) => {
    disposables.push(
      parser.registerCsiHandler(id, (params) => {
        const pattern = expect(params);
        if (pattern) tracker.expect(pattern);
        return false;
      }),
    );
  };

  // DA1 and DA2.
  expectCsi({ final: "c" }, () => DEVICE_ATTRIBUTES);
  expectCsi({ prefix: ">", final: "c" }, () => DEVICE_ATTRIBUTES);

  // DSR 5 (status) and 6 (cursor position), plain and DEC-specific.
  expectCsi({ final: "n" }, (params) => {
    if (params[0] === 5) return STATUS_REPORT;
    if (params[0] === 6) return CURSOR_POSITION;
    return null;
  });
  expectCsi({ prefix: "?", final: "n" }, (params) => (params[0] === 6 ? CURSOR_POSITION : null));

  // DECRQM and ANSI mode report.
  expectCsi({ intermediates: "$", final: "p" }, () => MODE_REPORT);
  expectCsi({ prefix: "?", intermediates: "$", final: "p" }, () => MODE_REPORT);

  // XTWINOPS 14/16/18. xterm only answers these with window options enabled.
  expectCsi({ final: "t" }, (params) =>
    params[0] === 14 || params[0] === 16 || params[0] === 18 ? WINDOW_REPORT : null,
  );

  // DECRQSS: DCS $ q <setting> ST; xterm answers with a status string.
  disposables.push(
    parser.registerDcsHandler({ intermediates: "$", final: "q" }, () => {
      tracker.expect(STATUS_STRING);
      return false;
    }),
  );

  // OSC 4;index;? queries any number of palette entries.
  disposables.push(
    parser.registerOscHandler(4, (data) => {
      // One report per queried index, all in one data event each.
      for (const part of data.split(";")) {
        if (part === "?") tracker.expect(INDEXED_COLOR);
      }
      return false;
    }),
  );
  // OSC 10/11/12;? asks for the default foreground, background or cursor.
  for (const resource of [10, 11, 12]) {
    disposables.push(
      parser.registerOscHandler(resource, (data) => {
        if (data === "?") tracker.expect(SPECIAL_COLOR);
        return false;
      }),
    );
  }

  return disposables;
}
