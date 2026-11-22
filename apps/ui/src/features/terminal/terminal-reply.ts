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
 *
 * Every answer is named by the output chunk that carried the query's **first**
 * byte and the query's index among the queries that start in that chunk. Both
 * parts are properties of the output stream, so two viewers agree even when
 * one attached with the query's tail already folded into its snapshot: that
 * viewer never sees the query, and the answers it does see keep their names.
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
    callback: (data: string, params: (number | number[])[]) => boolean,
  ): { dispose(): void };
};

/** A CSI, DCS or OSC sequence reduced to what identifies it. */
type QuerySignature =
  | {
      kind: "csi";
      prefix: string;
      intermediates: string;
      final: string;
      params: (number | number[])[];
    }
  | {
      kind: "dcs";
      intermediates: string;
      final: string;
      params: (number | number[])[];
      data: string;
    }
  | { kind: "osc"; ident: number; data: string };

type Expectation = {
  /** Monotonic position, so a finished chunk can settle only its own queries. */
  mark: number;
  pattern: RegExp;
  /** First output chunk whose bytes carried the query. */
  position: number;
  /** The answer's index among the queries that start in that chunk. */
  ordinal: number;
};

/** One output chunk waiting to be parsed. */
type ParsingChunk = {
  seq: number;
  /**
   * The sequence carried in from an earlier chunk that completed while this
   * chunk was parsed, with the chunk that carried its first byte. Only the
   * first hook of the chunk can be it.
   */
  carried: { signature: QuerySignature; position: number } | null;
  /** Position of the hook currently running, or null for the chunk. */
  hookPosition: number | null;
};

/** A sequence whose first byte arrived but whose end is still to come. */
type PendingEscape = {
  kind: "escape" | "csi" | "osc" | "dcs" | "string";
  /** Bytes since the ESC that began it, without the terminator. */
  text: string;
  /** The output chunk that carried that ESC. */
  startPosition: number;
};

/**
 * Longest sequence kept for a boundary comparison.
 *
 * A batched palette query is a few hundred bytes; one this long is not a
 * query xterm answers, and the cap keeps a program that opens an OSC and never
 * closes it from growing the tracker without bound.
 */
const MAX_SEQUENCE_BYTES = 8192;

/** xterm's largest single parameter value (clamped to the positive int32 range). */
const MAX_PARAM_VALUE = 0x7fffffff;

/** xterm's limits on one sequence's parameters and sub-parameters. */
const MAX_PARAMS = 32;
const MAX_SUB_PARAMS = 32;

/**
 * Notes the answer shapes xterm owes the program since the last chunk.
 *
 * Matching is by shape rather than a bare count, because one query may go
 * unanswered (xterm only reports window size when the option is enabled) and
 * a stale counter must not swallow a later keystroke. Expectations also
 * expire with the output chunk that asked the question: a query xterm chose
 * not to answer must not make a later paste look like a reply.
 *
 * There is no cap: one command may query a whole palette (`OSC 4;0;?;1;?;…`)
 * and xterm answers every slot, so a fixed window would forget the tail of a
 * batch and leave those answers looking like typed input. Entries are removed
 * as they are consumed or when their output chunk settles, so the list only
 * grows with queries in the chunk being parsed.
 */
export class TerminalQueryTracker {
  private readonly expected: Expectation[] = [];
  /** Output chunks waiting to be parsed, oldest first. */
  private readonly chunks: ParsingChunk[] = [];
  /** Next ordinal for each chunk that has started a query. */
  private readonly ordinals = new Map<number, number>();
  /** The sequence still open at the end of the last chunk handed in. */
  private pending: PendingEscape | null = null;
  private nextMark = 0;

  /**
   * Records that output chunk `seq` is about to be parsed by xterm.
   *
   * `data` is the chunk's raw bytes, which lets the tracker tell an answer to
   * a query that started in an earlier chunk from an answer to a query that
   * started in this one. A snapshot's bytes pass through the same parser, so
   * they are scanned the same way; [`TerminalQueryTracker.reset`] clears what
   * came before it first.
   */
  beginChunk(seq: number, data?: Uint8Array): void {
    let carried: ParsingChunk["carried"] = null;
    if (data) {
      const scan = scanOutput(seq, data, this.pending);
      this.pending = scan.pending;
      carried = scan.carried;
    }
    this.chunks.push({ seq, carried, hookPosition: null });
  }

  /** Records that xterm finished parsing the oldest outstanding chunk. */
  endChunk(): void {
    this.chunks.shift();
    if (this.ordinals.size > 16) {
      const keep = this.pending?.startPosition;
      for (const position of this.ordinals.keys()) {
        if (this.ordinals.size <= 16) break;
        if (position !== keep) this.ordinals.delete(position);
      }
    }
  }

  /**
   * Forgets every pending sequence, query and ordinal.
   *
   * A resync snapshot replaces the screen and xterm's parser (`term.reset()`),
   * so a sequence that was mid-flight before it can never complete: its tail is
   * gone from the screen, and the parser starts at ground. Keeping it would let
   * the next chunk finish a query that no longer exists and name its answer by
   * a chunk the shell already forgot.
   */
  reset(): void {
    this.expected.length = 0;
    this.chunks.length = 0;
    this.ordinals.clear();
    this.pending = null;
  }

  /**
   * Records that xterm's parser is delivering `signature`.
   *
   * The first hook of a chunk may be the sequence carried in from the
   * previous one; every later hook started in the chunk itself. An OSC query
   * can owe several answers, so the position is fixed once per hook rather
   * than once per expectation.
   */
  noteHook(signature: QuerySignature): void {
    const chunk = this.chunks[0];
    if (!chunk) return;
    const matches = chunk.carried !== null && sameSignature(chunk.carried.signature, signature);
    chunk.hookPosition = matches && chunk.carried ? chunk.carried.position : chunk.seq;
    chunk.carried = null;
  }

  /** Records that xterm is about to send one `pattern`-shaped answer. */
  expect(pattern: RegExp): void {
    const chunk = this.chunks[0];
    const position = chunk?.hookPosition ?? chunk?.seq ?? 0;
    const ordinal = this.ordinals.get(position) ?? 0;
    this.ordinals.set(position, ordinal + 1);
    this.expected.push({ mark: ++this.nextMark, pattern, position, ordinal });
  }

  /** The position to settle from once xterm finishes the chunk being written. */
  mark(): number {
    return this.nextMark;
  }

  /**
   * Forgets unanswered queries recorded since `mark`.
   *
   * Called when xterm has finished parsing an output chunk: any query in it
   * that xterm did not answer stays unanswered, so an answer-shaped string
   * pasted later is input. Later chunks are not affected.
   */
  settle(mark: number): void {
    for (let index = this.expected.length - 1; index >= 0; index -= 1) {
      const entry = this.expected[index];
      if (entry && entry.mark > mark) this.expected.splice(index, 1);
    }
  }

  /**
   * The answer `data` gives, or null for input.
   *
   * Consumes the expectation. Returns null for every other chunk, including
   * answer-shaped sequences the person typed or pasted.
   */
  takeReply(data: string): TerminalQueryReply | null {
    if (this.expected.length === 0) return null;
    const index = this.expected.findIndex((entry) => entry.pattern.test(data));
    if (index === -1) return null;
    const [entry] = this.expected.splice(index, 1);
    if (!entry) return null;
    return { position: entry.position, ordinal: entry.ordinal };
  }

  /**
   * Whether `data` is xterm's answer to a pending query, consuming it.
   *
   * Returns false for every other chunk, including answer-shaped sequences
   * the person typed or pasted.
   */
  take(data: string): boolean {
    return this.takeReply(data) !== null;
  }
}

/** One answer xterm owes: the chunk that started its query and its ordinal. */
export type TerminalQueryReply = {
  position: number;
  ordinal: number;
};

/**
 * Scans one output chunk for the escape sequences in it.
 *
 * Only a sequence that started before this chunk can make the chunk's first
 * hook below its own position, so that is all the scan reports. The rest of
 * the scan finds whether the chunk ends inside a sequence, which the next
 * chunk continues.
 *
 * The transitions mirror xterm's parser rather than a permissive reading of
 * the bytes, because a sequence xterm abandoned must not be matched against a
 * later hook: ESC starts a new escape from every state (cancelling a CSI,
 * ending an OSC, DCS or string first), and CAN or SUB return to ground.
 */
function scanOutput(
  seq: number,
  data: Uint8Array,
  carriedIn: PendingEscape | null,
): { carried: ParsingChunk["carried"]; pending: PendingEscape | null } {
  let kind: PendingEscape["kind"] | "ground" = carriedIn?.kind ?? "ground";
  let text = carriedIn?.text ?? "";
  let startPosition = carriedIn?.startPosition ?? seq;
  let truncated = false;
  let carried: ParsingChunk["carried"] = null;

  const begin = (next: PendingEscape["kind"]) => {
    kind = next;
    text = "\x1b";
    startPosition = seq;
    truncated = false;
  };
  const append = (byte: number) => {
    if (truncated) return;
    if (text.length >= MAX_SEQUENCE_BYTES) {
      truncated = true;
      return;
    }
    text += String.fromCharCode(byte);
  };
  const finish = () => {
    if (startPosition !== seq && !truncated && !carried) {
      const signature = signatureOf(text);
      if (signature) carried = { signature, position: startPosition };
    }
    kind = "ground";
    text = "";
    truncated = false;
  };
  /** Abandons whatever is open without letting it match a later hook. */
  const cancel = () => {
    kind = "ground";
    text = "";
    truncated = false;
  };

  for (const byte of data) {
    // CAN and SUB abort the open sequence from any state (xterm's global
    // EXECUTE-to-ground rule), so nothing in it is a query or its tail.
    if (byte === 0x18 || byte === 0x1a) {
      cancel();
      continue;
    }
    // ESC starts a new escape from any state. Inside an OSC, DCS or string it
    // also ends that sequence first, which is where xterm reports the query;
    // inside a CSI it simply abandons the unfinished sequence.
    if (byte === 0x1b && kind !== "ground") {
      if (kind === "osc" || kind === "dcs" || kind === "string") finish();
      begin("escape");
      continue;
    }
    // A bare ST ends a string; anywhere else it is ignored (xterm returns to
    // ground without dispatching).
    if (byte === 0x9c) {
      if (kind === "osc" || kind === "dcs" || kind === "string") {
        finish();
      } else {
        cancel();
      }
      continue;
    }
    switch (kind) {
      case "ground":
        if (byte === 0x1b) begin("escape");
        break;
      case "escape":
        if (byte === 0x5b) {
          append(byte);
          kind = "csi";
        } else if (byte === 0x5d) {
          append(byte);
          kind = "osc";
        } else if (byte === 0x50) {
          append(byte);
          kind = "dcs";
        } else if (byte === 0x58 || byte === 0x5e || byte === 0x5f) {
          append(byte);
          kind = "string";
        } else {
          append(byte);
          finish();
        }
        break;
      case "csi":
        if (byte >= 0x40 && byte <= 0x7e) {
          append(byte);
          finish();
        } else {
          append(byte);
        }
        break;
      case "osc":
        if (byte === 0x07) {
          finish();
        } else {
          append(byte);
        }
        break;
      case "dcs":
      case "string":
        append(byte);
        break;
    }
  }

  if (kind === "ground") return { carried, pending: null };
  return { carried, pending: { kind, text, startPosition } };
}

/** The signature of a completed escape sequence, or null for other escapes. */
function signatureOf(text: string): QuerySignature | null {
  if (!text.startsWith("\x1b")) return null;
  const body = text.slice(1);
  if (body.startsWith("[")) return csiSignature(body.slice(1));
  if (body.startsWith("]")) return oscSignature(body.slice(1));
  if (body.startsWith("P")) return dcsSignature(body.slice(1));
  return null;
}

function csiSignature(body: string): QuerySignature | null {
  let index = 0;
  let prefix = "";
  const first = body[0];
  if (first && "<=>?".includes(first)) {
    prefix = first;
    index = 1;
  }
  const paramsEnd = scanWhile(body, index, 0x30, 0x3f);
  const intermediatesEnd = scanWhile(body, paramsEnd, 0x20, 0x2f);
  const final = body[intermediatesEnd];
  const finalCode = body.charCodeAt(intermediatesEnd);
  if (!final || finalCode < 0x40 || finalCode > 0x7e) return null;
  const params = parseParams(body.slice(index, paramsEnd));
  if (!params) return null;
  return {
    kind: "csi",
    prefix,
    intermediates: body.slice(paramsEnd, intermediatesEnd),
    final,
    params,
  };
}

function dcsSignature(body: string): QuerySignature | null {
  const paramsEnd = scanWhile(body, 0, 0x30, 0x3f);
  const intermediatesEnd = scanWhile(body, paramsEnd, 0x20, 0x2f);
  const finalCode = body.charCodeAt(intermediatesEnd);
  if (Number.isNaN(finalCode) || finalCode < 0x40 || finalCode > 0x7e) return null;
  const params = parseParams(body.slice(0, paramsEnd));
  if (!params) return null;
  return {
    kind: "dcs",
    intermediates: body.slice(paramsEnd, intermediatesEnd),
    final: body[intermediatesEnd] ?? "",
    params,
    data: body.slice(intermediatesEnd + 1),
  };
}

function oscSignature(body: string): QuerySignature | null {
  const separator = body.indexOf(";");
  const ident = Number.parseInt(separator === -1 ? body : body.slice(0, separator), 10);
  if (Number.isNaN(ident)) return null;
  return { kind: "osc", ident, data: separator === -1 ? "" : body.slice(separator + 1) };
}

/** Index of the first byte of `body` outside `[from, to]`, starting at `start`. */
function scanWhile(body: string, start: number, from: number, to: number): number {
  let index = start;
  while (index < body.length) {
    const code = body.charCodeAt(index);
    if (code < from || code > to) break;
    index += 1;
  }
  return index;
}

/**
 * Parses a CSI or DCS parameter string the way xterm reports it.
 *
 * xterm starts every sequence with one zero parameter: the parser's `CLEAR`
 * action resets the list and adds zero, so `ESC[` | `c` is `[0]`, not `[]`,
 * and a signature built from the bytes can only match the list a hook receives
 * if it does the same. `;` starts another zero parameter, `:` starts a
 * sub-parameter whose empty form stays `-1`, and values clamp at the positive
 * int32 range. Sub-parameters follow their parameter in the flat list xterm's
 * `toArray` hands to a handler.
 *
 * Returns null for bytes xterm sends to its ignore state (a second prefix
 * byte, for instance): no hook can fire for that sequence, so matching one
 * would name a query that never existed.
 */
function parseParams(text: string): (number | number[])[] | null {
  const params: Array<{ value: number; subs: number[] | null }> = [{ value: 0, subs: null }];
  let subParamCount = 0;
  // xterm stops accepting parameters at 32 and sub-parameters at 32, keeping
  // what it already has; further digits are ignored until the next `;`.
  let rejectDigits = false;
  let rejectSubDigits = false;
  for (const char of text) {
    const code = char.charCodeAt(0);
    const current = params[params.length - 1];
    if (!current) return null;
    if (code >= 0x30 && code <= 0x39) {
      if (rejectDigits) continue;
      const digit = code - 0x30;
      const subs = current.subs;
      if (subs) {
        if (rejectSubDigits) continue;
        const last = subs.length - 1;
        const previous = subs[last] ?? -1;
        subs[last] = previous < 0 ? digit : Math.min(previous * 10 + digit, MAX_PARAM_VALUE);
      } else {
        current.value = Math.min(current.value * 10 + digit, MAX_PARAM_VALUE);
      }
    } else if (code === 0x3b) {
      if (params.length >= MAX_PARAMS) {
        rejectDigits = true;
      } else {
        params.push({ value: 0, subs: null });
      }
    } else if (code === 0x3a) {
      if (rejectDigits || subParamCount >= MAX_SUB_PARAMS) {
        rejectSubDigits = true;
      } else {
        current.subs ??= [];
        current.subs.push(-1);
        subParamCount += 1;
      }
    } else {
      return null;
    }
  }
  const values: (number | number[])[] = [];
  for (const param of params) {
    values.push(param.value);
    if (param.subs) values.push(param.subs);
  }
  return values;
}

/** Whether two sequences are the same query. */
function sameSignature(left: QuerySignature, right: QuerySignature): boolean {
  if (left.kind === "csi") {
    return (
      right.kind === "csi" &&
      left.prefix === right.prefix &&
      left.intermediates === right.intermediates &&
      left.final === right.final &&
      sameParams(left.params, right.params)
    );
  }
  if (left.kind === "dcs") {
    return (
      right.kind === "dcs" &&
      left.intermediates === right.intermediates &&
      left.final === right.final &&
      left.data === right.data &&
      sameParams(left.params, right.params)
    );
  }
  return right.kind === "osc" && left.ident === right.ident && left.data === right.data;
}

function sameParams(left: (number | number[])[], right: (number | number[])[]): boolean {
  if (left.length !== right.length) return false;
  return left.every((value, index) => {
    const other = right[index];
    if (Array.isArray(value) || Array.isArray(other)) {
      return (
        Array.isArray(value) &&
        Array.isArray(other) &&
        value.length === other.length &&
        value.every((part, partIndex) => part === other[partIndex])
      );
    }
    return value === other;
  });
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
        if (pattern) {
          tracker.noteHook({
            kind: "csi",
            prefix: id.prefix ?? "",
            intermediates: id.intermediates ?? "",
            final: id.final,
            params,
          });
          tracker.expect(pattern);
        }
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
    parser.registerDcsHandler({ intermediates: "$", final: "q" }, (data, params) => {
      tracker.noteHook({ kind: "dcs", intermediates: "$", final: "q", params, data });
      tracker.expect(STATUS_STRING);
      return false;
    }),
  );

  // OSC 4;index;? queries any number of palette entries.
  disposables.push(
    parser.registerOscHandler(4, (data) => {
      // One report per queried index, each arriving as its own data event.
      const count = data.split(";").filter((part) => part === "?").length;
      if (count > 0) {
        tracker.noteHook({ kind: "osc", ident: 4, data });
        for (let slot = 0; slot < count; slot += 1) tracker.expect(INDEXED_COLOR);
      }
      return false;
    }),
  );
  // OSC 10/11/12;? asks for the default foreground, background or cursor.
  // One command may carry a chain of slots (`10;?;?;?`), and xterm reports
  // every slot, so count them all.
  for (const resource of [10, 11, 12]) {
    disposables.push(
      parser.registerOscHandler(resource, (data) => {
        const count = data.split(";").filter((part) => part === "?").length;
        if (count > 0) {
          tracker.noteHook({ kind: "osc", ident: resource, data });
          for (let slot = 0; slot < count; slot += 1) tracker.expect(SPECIAL_COLOR);
        }
        return false;
      }),
    );
  }

  return disposables;
}
