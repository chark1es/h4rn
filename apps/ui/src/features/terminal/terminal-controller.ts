import { FitAddon } from "@xterm/addon-fit";
import { SearchAddon } from "@xterm/addon-search";
import { WebLinksAddon } from "@xterm/addon-web-links";
import { WebglAddon } from "@xterm/addon-webgl";
import { Terminal } from "@xterm/xterm";

import type { TerminalExit } from "@/gen";
import {
  AckTracker,
  type BinaryFrame,
  BinaryKind,
  decodeBinaryFrame,
  encodeBinaryFrame,
  PTY_INPUT_REPLY,
  ptyInputReplySeq,
} from "@/lib/client/binary";
import type { H4rnClient } from "@/lib/client/client";
import { H4rnRequestError } from "@/lib/client/errors";
import type { TerminalQueryReply } from "./terminal-reply";
import { registerTerminalQueryHooks, TerminalQueryTracker } from "./terminal-reply";
import { isDarkTheme, terminalTheme } from "./theme";

export type TerminalPhase = "opening" | "attached" | "exited" | "error" | "offline";

export type TerminalStatus = {
  phase: TerminalPhase;
  message?: string;
  hint?: string;
  exit?: TerminalExit;
};

export type TerminalControllerOptions = {
  client: H4rnClient;
  threadId: string | null;
  host: string;
  cwd: string;
  element: HTMLElement;
  reducedMotion?: boolean;
  onStatus?: (status: TerminalStatus) => void;
  onTitle?: (title: string) => void;
  onDimensions?: (dimensions: { cols: number; rows: number }) => void;
};

const RESIZE_DEBOUNCE_MS = 120;

/**
 * Owns one xterm instance and its daemon session (ADR-007).
 *
 * Input and output use binary frames; resize is a JSON request so the daemon
 * can apply the smallest attached viewport. Acks are sent after xterm's write
 * callback fires, which is the only point at which the bytes are really on
 * screen and the daemon may release more credit.
 */
export class TerminalController {
  private client: H4rnClient | null = null;
  private term: Terminal | null = null;
  private fitAddon: FitAddon | null = null;
  private searchAddon: SearchAddon | null = null;
  private element: HTMLElement | null = null;
  private terminalId = "";
  private streamId = 0;
  private tracker: AckTracker | null = null;
  private pending: BinaryFrame[] = [];
  private disposers: Array<() => void> = [];
  private resizeObserver: ResizeObserver | null = null;
  private themeObserver: MutationObserver | null = null;
  private mediaQuery: MediaQueryList | null = null;
  private mediaListener: (() => void) | null = null;
  private resizeTimer: ReturnType<typeof setTimeout> | null = null;
  private ackTimer: ReturnType<typeof setTimeout> | null = null;
  private disposed = false;
  private onStatus: ((status: TerminalStatus) => void) | undefined;
  private onTitle: ((title: string) => void) | undefined;
  private onDimensions: ((dimensions: { cols: number; rows: number }) => void) | undefined;
  private status: TerminalStatus = { phase: "opening" };
  /** Answers xterm owes the program since the last data chunk. */
  private readonly queryTracker = new TerminalQueryTracker();

  async start(options: TerminalControllerOptions): Promise<void> {
    this.client = options.client;
    this.element = options.element;
    this.onStatus = options.onStatus;
    this.onTitle = options.onTitle;
    this.onDimensions = options.onDimensions;

    if (options.client.getSnapshot().status !== "online") {
      this.emit({
        phase: "offline",
        message: "The daemon is offline.",
        hint: "Reconnect in the status bar; this terminal opens when the daemon is back.",
      });
      return;
    }

    // Phones get a slightly larger grid: the prototype's 46-column phone
    // terminal is the smallest view any desktop viewer can be squeezed to.
    const compact = window.matchMedia?.("(max-width: 639px)").matches ?? false;
    const term = new Terminal({
      fontFamily:
        '"Geist Mono Variable", "DejaVu Sans Mono", ui-monospace, SFMono-Regular, monospace',
      fontSize: compact ? 12 : 12.5,
      // xterm multiplies the measured cell height by this. 1.18 lands on the
      // prototype's 18 px row pitch at 12 px on phones; 1.12 keeps desktops at
      // 19 px at 12.5 px.
      lineHeight: compact ? 1.18 : 1.12,
      letterSpacing: 0,
      cursorBlink: !options.reducedMotion,
      cursorStyle: "bar",
      scrollback: 5000,
      scrollOnUserInput: true,
      rightClickSelectsWord: true,
      theme: terminalTheme(isDarkTheme()),
      allowTransparency: true,
    });
    this.term = term;
    term.open(options.element);

    const fit = new FitAddon();
    term.loadAddon(fit);
    this.fitAddon = fit;
    try {
      const webgl = new WebglAddon();
      webgl.onContextLoss(() => webgl.dispose());
      term.loadAddon(webgl);
    } catch {
      // Canvas renderer is the intentional fallback on GPUs and drivers that
      // refuse a WebGL context; nothing else changes.
    }
    term.loadAddon(
      new WebLinksAddon((_event, uri) => {
        window.open(uri, "_blank", "noopener,noreferrer");
      }),
    );
    const search = new SearchAddon();
    term.loadAddon(search);
    this.searchAddon = search;

    this.bindInput(term);
    this.bindBinary();
    this.bindTerminalEvents();
    this.bindTheme();
    this.observeSize();
    this.fitNow();

    try {
      const cols = Math.max(term.cols, 2);
      const rows = Math.max(term.rows, 1);
      const opened = await options.client.request({
        type: "terminal_open",
        thread_id: options.threadId,
        host: options.host,
        cwd: options.cwd,
        shell: null,
        args: [],
        cols,
        rows,
      });
      if (this.disposed) return;
      this.terminalId = opened.terminal.terminal_id;
      this.streamId = opened.terminal.stream_id;
      this.onTitle?.(opened.terminal.shell);

      const attached = await options.client.request({
        type: "terminal_attach",
        terminal_id: this.terminalId,
        cols: Math.max(term.cols, 2),
        rows: Math.max(term.rows, 1),
        reply_positions_v2: true,
      });
      if (this.disposed) return;
      this.streamId = attached.terminal.stream_id;
      this.tracker = new AckTracker(attached.start_seq);
      term.write(attached.snapshot);

      const queued = this.pending.splice(0);
      this.pending = [];
      for (const frame of queued) {
        if (frame) this.applyFrame(frame);
      }

      this.emit({ phase: "attached", message: attached.terminal.shell });
      term.focus();
    } catch (error) {
      if (this.disposed) return;
      this.fail(error);
    }
  }

  /**
   * Sends raw text as PTY input (used by the phone key row and tests).
   *
   * `reply` names the output chunk that carried the query's first byte and
   * the query's index among the queries that start there, or null for typed
   * input. The daemon writes one answer per `(position, ordinal)` and decides
   * whether this viewer may answer it: a reply is never dropped here, because
   * the identity is the same for every viewer that parsed the starting chunk.
   */
  send(data: string, reply: TerminalQueryReply | null = null): void {
    if (!this.client || this.streamId === 0) return;
    const payload = new TextEncoder().encode(data);
    const seq =
      reply === null
        ? 0
        : reply.position > 0
          ? ptyInputReplySeq(reply.position, reply.ordinal)
          : PTY_INPUT_REPLY;
    try {
      this.client.sendBinary(
        encodeBinaryFrame({
          kind: BinaryKind.PtyInput,
          streamId: this.streamId,
          seq,
          payload,
        }),
      );
    } catch {
      // The socket closed between keystrokes; the offline overlay takes over.
    }
  }

  focus(): void {
    this.term?.focus();
  }

  fit(): void {
    this.fitNow();
  }

  findNext(query: string): void {
    if (query) this.searchAddon?.findNext(query);
  }

  findPrevious(query: string): void {
    if (query) this.searchAddon?.findPrevious(query);
  }

  dispose(): void {
    if (this.disposed) return;
    this.disposed = true;
    for (const dispose of this.disposers) dispose();
    this.disposers = [];
    this.resizeObserver?.disconnect();
    this.themeObserver?.disconnect();
    this.mediaQuery?.removeEventListener("change", this.mediaListener ?? (() => undefined));
    if (this.resizeTimer) clearTimeout(this.resizeTimer);
    if (this.ackTimer) clearTimeout(this.ackTimer);
    this.resizeObserver = null;
    this.themeObserver = null;

    const client = this.client;
    const terminalId = this.terminalId;
    this.term?.dispose();
    this.term = null;
    if (client && terminalId) {
      void client
        .request({ type: "terminal_close", terminal_id: terminalId })
        .catch(() => undefined);
    }
  }

  /** Registers an xterm disposable with the same lifetime as the controller. */
  private track(disposable: { dispose: () => void }): void {
    this.disposers.push(() => disposable.dispose());
  }

  private bindInput(term: Terminal): void {
    // Parser hooks note every query xterm is about to answer; the next data
    // chunk of that shape is the answer, and everything else is user input.
    for (const disposable of registerTerminalQueryHooks(term.parser, this.queryTracker)) {
      this.track(disposable);
    }
    this.track(
      term.onData((data) => {
        this.send(data, this.queryTracker.takeReply(data));
      }),
    );
    this.track(
      term.onTitleChange((title) => {
        if (title.trim()) this.onTitle?.(title.trim());
      }),
    );
    term.attachCustomKeyEventHandler((event) => {
      if (event.type !== "keydown") return true;
      const modifier = event.metaKey || event.ctrlKey;
      const key = event.key.toLowerCase();
      if (modifier && key === "c" && term.hasSelection()) {
        const selection = term.getSelection();
        void navigator.clipboard?.writeText(selection);
        return false;
      }
      if (modifier && key === "v") {
        void navigator.clipboard
          ?.readText()
          .then((text) => {
            if (text) term.paste(text);
          })
          .catch(() => undefined);
        return false;
      }
      return true;
    });
  }

  private bindBinary(): void {
    if (!this.client) return;
    this.disposers.push(
      this.client.onBinary((data) => {
        const frame = decodeBinaryFrame(data);
        if (!frame) return;
        if (!this.tracker) {
          this.pending.push(frame);
          return;
        }
        this.applyFrame(frame);
      }),
    );
  }

  private bindTerminalEvents(): void {
    if (!this.client) return;
    this.disposers.push(
      this.client.onTerminalEvent((notice) => {
        if (notice.terminal_id !== this.terminalId) return;
        const exit = notice.exit;
        const suffix = exit.signal ? `signal ${exit.signal}` : `status ${exit.status}`;
        this.term?.write(`\r\n\u001b[38;5;244m[process exited with ${suffix}]\u001b[0m\r\n`);
        this.emit({
          phase: "exited",
          message: exit.signal ? `Exited on ${exit.signal}.` : `Exited with status ${exit.status}.`,
          exit,
        });
      }),
    );
  }

  private bindTheme(): void {
    const refresh = () => {
      if (this.term) this.term.options.theme = terminalTheme(isDarkTheme());
    };
    const observer = new MutationObserver(refresh);
    observer.observe(document.documentElement, {
      attributes: true,
      attributeFilter: ["class", "data-terminal-theme"],
    });
    this.themeObserver = observer;

    this.mediaQuery = window.matchMedia?.("(prefers-color-scheme: dark)") ?? null;
    if (this.mediaQuery) {
      this.mediaListener = refresh;
      this.mediaQuery.addEventListener("change", refresh);
    }
  }

  private observeSize(): void {
    if (!this.element || typeof ResizeObserver === "undefined") return;
    const observer = new ResizeObserver(() => {
      if (this.disposed) return;
      window.requestAnimationFrame(() => this.fitNow());
    });
    observer.observe(this.element);
    this.resizeObserver = observer;
    const resizeDisposable = this.term?.onResize(() => {
      this.reportDimensions();
      this.scheduleResize();
    });
    if (resizeDisposable) this.track(resizeDisposable);
  }

  private fitNow(): void {
    const element = this.element;
    if (!element || element.clientWidth < 24 || element.clientHeight < 24) return;
    try {
      this.fitAddon?.fit();
      this.reportDimensions();
    } catch {
      // A hidden or zero-sized container rejects fit; the next resize retries.
    }
  }

  private reportDimensions(): void {
    const term = this.term;
    if (!term || this.disposed) return;
    this.onDimensions?.({ cols: term.cols, rows: term.rows });
  }

  private scheduleResize(): void {
    if (this.disposed || !this.client || !this.terminalId) return;
    if (this.resizeTimer) clearTimeout(this.resizeTimer);
    this.resizeTimer = setTimeout(() => {
      this.resizeTimer = null;
      const term = this.term;
      if (this.disposed || !this.client || !this.terminalId || !term) return;
      void this.client
        .request({
          type: "terminal_resize",
          terminal_id: this.terminalId,
          cols: Math.max(term.cols, 2),
          rows: Math.max(term.rows, 1),
        })
        .catch(() => undefined);
    }, RESIZE_DEBOUNCE_MS);
  }

  private applyFrame(frame: NonNullable<ReturnType<typeof decodeBinaryFrame>>): void {
    const term = this.term;
    if (!term || !this.tracker || frame.streamId !== this.streamId) return;

    if (frame.kind === BinaryKind.PtyOutput) {
      if (frame.seq <= this.tracker.position) return;
      // Queries in this chunk are only answerable while it is being parsed:
      // once xterm is done, anything it did not answer has missed its chance
      // and a later answer-shaped paste must stay input. The tracker also
      // remembers which chunk is being parsed, so an answer names the output
      // position the daemon uses to keep one answer per query.
      const mark = this.queryTracker.mark();
      this.queryTracker.beginChunk(frame.seq, frame.payload);
      term.write(frame.payload, () => {
        this.queryTracker.settle(mark);
        this.queryTracker.endChunk();
        this.tracker?.applied(frame.seq);
        this.scheduleAck();
      });
      return;
    }

    if (frame.kind === BinaryKind.PtySnapshot) {
      if (frame.seq <= this.tracker.position) return;
      term.reset();
      this.tracker.reset(frame.seq);
      // The snapshot replaces the screen, so a sequence left open before it can
      // never complete. The clear waits for the snapshot chunk to start
      // parsing: xterm parses queued writes later, and an output chunk applied
      // just before this snapshot must still name its answers by its own seq.
      this.queryTracker.resetAt(frame.seq);
      this.queryTracker.beginChunk(frame.seq, frame.payload);
      term.write(new TextDecoder().decode(frame.payload), () => {
        this.queryTracker.endChunk();
        this.scheduleAck();
      });
    }
  }

  private scheduleAck(): void {
    if (this.ackTimer || !this.client) return;
    this.ackTimer = setTimeout(() => {
      this.ackTimer = null;
      const upTo = this.tracker?.takeAck();
      if (upTo == null || !this.client || !this.terminalId) return;
      void this.client
        .request({ type: "terminal_ack", terminal_id: this.terminalId, up_to_chunk: upTo })
        .catch(() => undefined);
    }, 0);
  }

  private emit(status: TerminalStatus): void {
    this.status = status;
    this.onStatus?.(status);
  }

  private fail(error: unknown): void {
    const proto = error instanceof H4rnRequestError ? error.proto : null;
    if (this.streamId === 0 && this.terminalId === "") {
      this.emit({
        phase: "error",
        message:
          proto?.message ?? (error instanceof Error ? error.message : "The terminal failed."),
        hint:
          proto?.hint ?? "Check that the working directory exists and the daemon is still running.",
      });
      return;
    }
    this.emit({
      phase: "error",
      message: proto?.message ?? "The terminal stream failed.",
      hint: proto?.hint ?? "Close this tab and open a new terminal.",
    });
  }

  get currentStatus(): TerminalStatus {
    return this.status;
  }
}
