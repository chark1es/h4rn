import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  type BinaryFrame,
  BinaryKind,
  decodeBinaryFrame,
  encodeBinaryFrame,
} from "@/lib/client/binary";
import type { H4rnClient } from "@/lib/client/client";

/**
 * xterm is replaced by a fake that records the callbacks the controller
 * registers, so the test can act like the parser and like typing.
 */
const fakes = vi.hoisted(() => {
  class FakeTerminal {
    static last: FakeTerminal | null = null;
    dataHandler: ((data: string) => void) | null = null;
    csi = new Map<string, (params: unknown[]) => boolean>();
    osc = new Map<number, (data: string) => boolean>();
    cols = 80;
    rows = 24;
    options: Record<string, unknown> = {};

    constructor() {
      FakeTerminal.last = this;
    }

    parser = {
      registerCsiHandler: (
        id: { final: string; prefix?: string; intermediates?: string },
        callback: (params: unknown[]) => boolean,
      ) => {
        const key = `${id.prefix ?? ""}|${id.intermediates ?? ""}|${id.final}`;
        this.csi.set(key, callback);
        return { dispose: () => this.csi.delete(key) };
      },
      registerOscHandler: (ident: number, callback: (data: string) => boolean) => {
        this.osc.set(ident, callback);
        return { dispose: () => this.osc.delete(ident) };
      },
      registerDcsHandler: () => ({ dispose: () => undefined }),
    };

    open(): void {}
    loadAddon(): void {}
    onData(callback: (data: string) => void) {
      this.dataHandler = callback;
      return { dispose: () => undefined };
    }
    onTitleChange() {
      return { dispose: () => undefined };
    }
    onResize() {
      return { dispose: () => undefined };
    }
    attachCustomKeyEventHandler(): void {}
    private writeCallback: (() => void) | null = null;
    write(_data: string | Uint8Array, callback?: () => void): void {
      this.writeCallback = callback ?? null;
    }
    focus(): void {}
    reset(): void {}
    dispose(): void {}

    /** Acts like xterm finishing the last chunk it was given. */
    finishWrite() {
      const callback = this.writeCallback;
      this.writeCallback = null;
      callback?.();
    }

    /** Acts like the program sending a query. */
    query(id: { final: string; prefix?: string; intermediates?: string }, params: unknown[] = []) {
      return this.csi.get(`${id.prefix ?? ""}|${id.intermediates ?? ""}|${id.final}`)?.(params);
    }

    /** Acts like xterm answering through onData, or the person typing. */
    input(data: string) {
      this.dataHandler?.(data);
    }
  }

  return { FakeTerminal };
});

vi.mock("@xterm/xterm", () => ({ Terminal: fakes.FakeTerminal }));
vi.mock("@xterm/addon-fit", () => ({ FitAddon: class {} }));
vi.mock("@xterm/addon-search", () => ({ SearchAddon: class {} }));
vi.mock("@xterm/addon-web-links", () => ({ WebLinksAddon: class {} }));
vi.mock("@xterm/addon-webgl", () => ({ WebglAddon: class {} }));

const { TerminalController } = await import("./terminal-controller");

const TERMINAL_INFO = {
  terminal_id: "term_test",
  stream_id: 7,
  thread_id: null,
  host: "host_test",
  cwd: "/tmp",
  shell: "/bin/sh",
  cols: 80,
  rows: 24,
  alive: true,
  viewers: 1,
  exit: null,
};

type BinarySink = { handler: ((data: ArrayBuffer) => void) | null };

function fakeClient(sent: BinaryFrame[], binary: BinarySink) {
  return {
    getSnapshot: () => ({ status: "online" }),
    request: async (body: { type: string }) => {
      if (body.type === "terminal_open") {
        return { type: "terminal_opened", terminal: TERMINAL_INFO };
      }
      if (body.type === "terminal_attach") {
        return {
          type: "terminal_attached",
          terminal: TERMINAL_INFO,
          snapshot: "",
          start_seq: 0,
        };
      }
      return { type: "ack" };
    },
    onBinary: (handler: (data: ArrayBuffer) => void) => {
      binary.handler = handler;
      return () => undefined;
    },
    onTerminalEvent: () => () => undefined,
    sendBinary: (bytes: Uint8Array) => {
      const frame = decodeBinaryFrame(bytes);
      if (frame) sent.push(frame);
    },
  } as unknown as H4rnClient;
}

async function startController(sent: BinaryFrame[]) {
  const controller = new TerminalController();
  const binary: BinarySink = { handler: null };
  await controller.start({
    client: fakeClient(sent, binary),
    threadId: null,
    host: "host_test",
    cwd: "/tmp",
    element: { clientWidth: 0, clientHeight: 0 } as HTMLElement,
  });
  const term = fakes.FakeTerminal.last;
  if (!term) throw new Error("xterm was not created");
  return { controller, term, binary };
}

const payload = (frame: BinaryFrame) => new TextDecoder().decode(frame.payload);

/** Delivers one PTY output chunk the way the client does. */
function pushOutput(binary: BinarySink, seq: number, text: string): void {
  const bytes = encodeBinaryFrame({
    kind: BinaryKind.PtyOutput,
    streamId: TERMINAL_INFO.stream_id,
    seq,
    payload: new TextEncoder().encode(text),
  });
  const buffer = bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength);
  binary.handler?.(buffer as ArrayBuffer);
}

describe("terminal controller query replies", () => {
  beforeEach(() => {
    Object.defineProperty(globalThis, "window", {
      value: {
        matchMedia: () => ({
          matches: false,
          addEventListener: () => undefined,
          removeEventListener: () => undefined,
        }),
      },
      configurable: true,
    });
    const classList = { contains: () => false };
    Object.defineProperty(globalThis, "document", {
      value: {
        documentElement: {
          getAttribute: () => null,
          classList,
        },
      },
      configurable: true,
    });
    Object.defineProperty(globalThis, "MutationObserver", {
      value: class {
        observe(): void {}
        disconnect(): void {}
      },
      configurable: true,
    });
  });

  afterEach(() => {
    Reflect.deleteProperty(globalThis, "window");
    Reflect.deleteProperty(globalThis, "document");
    Reflect.deleteProperty(globalThis, "MutationObserver");
  });

  it("marks xterm's answer on the wire and forwards the same bytes as input without a query", async () => {
    const sent: BinaryFrame[] = [];
    const { controller, term } = await startController(sent);

    // The program asks for the cursor position; xterm answers.
    expect(term.query({ final: "n" }, [6])).toBe(false);
    term.input("\x1b[12;40R");
    // Shift+F3 has the same shape but no query is pending.
    term.input("\x1b[1;2R");
    // Typing is always input.
    term.input("ls\r");

    expect(sent.map((frame) => [frame.seq, payload(frame)])).toEqual([
      [1, "\x1b[12;40R"],
      [0, "\x1b[1;2R"],
      [0, "ls\r"],
    ]);
    controller.dispose();
  });

  it("marks device attribute and colour answers", async () => {
    const sent: BinaryFrame[] = [];
    const { controller, term } = await startController(sent);

    term.query({ final: "c" });
    term.input("\x1b[?1;2c");
    term.query({ prefix: "?", intermediates: "$", final: "p" }, [12]);
    term.input("\x1b[?12;2$y");

    expect(sent.map((frame) => frame.seq)).toEqual([1, 1]);
    controller.dispose();
  });

  it("stops expecting a reply once the chunk that asked has been parsed", async () => {
    const sent: BinaryFrame[] = [];
    const { controller, term, binary } = await startController(sent);

    // The program asks for the window size; this xterm leaves it unanswered.
    pushOutput(binary, 1, "\x1b[18t");
    // The hook ran while xterm parsed the chunk, then xterm finished it.
    expect(term.query({ final: "t" }, [18])).toBe(false);
    term.finishWrite();

    // A pasted window report is now input, not an answer.
    term.input("\x1b[8;24;80t");
    expect(sent.map((frame) => [frame.seq, payload(frame)])).toEqual([[0, "\x1b[8;24;80t"]]);

    // An answer that arrives while its own chunk is still being parsed is
    // still marked, and the chunk's end does not double-count it.
    pushOutput(binary, 2, "\x1b[18t");
    expect(term.query({ final: "t" }, [18])).toBe(false);
    term.input("\x1b[8;24;80t");
    term.finishWrite();
    expect(sent.map((frame) => frame.seq)).toEqual([0, 1]);

    controller.dispose();
  });
});
