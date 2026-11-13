/* tslint:disable */
/* eslint-disable */

/**
 * The wasm-facing transport. Construct with [`connect`], then register
 * callbacks and drive the socket events from JS.
 */
export class SecureTransport {
    private constructor();
    free(): void;
    [Symbol.dispose](): void;
    /**
     * Closes the session.
     */
    close(now_ms: number): void;
    /**
     * Receives a final close reason.
     */
    on_closed(callback: Function): void;
    /**
     * Receives one application frame: a string for JSON, `Uint8Array` for binary.
     */
    on_frame(callback: Function): void;
    /**
     * Receives `(pathIndex, Uint8Array)` for every packet to write to a socket.
     */
    on_packet(callback: Function): void;
    /**
     * Receives a `TransportStatus` JSON snapshot.
     */
    on_status(callback: Function): void;
    /**
     * Receives the resume token as hex after the session is established.
     */
    on_ticket(callback: Function): void;
    /**
     * Marks a socket as closed.
     */
    path_closed(index: number, reason: string, now_ms: number): void;
    /**
     * Marks a socket as open and starts its handshake.
     */
    path_open(index: number, now_ms: number): void;
    /**
     * Feeds one binary packet received on a socket.
     */
    receive(index: number, bytes: Uint8Array | number[], now_ms: number): void;
    /**
     * The current resume token, when the daemon has issued one.
     */
    resume_token_hex(): string;
    /**
     * Queues one application frame.
     */
    send(bytes: Uint8Array | number[], binary: boolean, now_ms: number): void;
    /**
     * Starts every registered path's handshake.
     */
    start(now_ms: number): void;
    /**
     * The current status as JSON.
     */
    status_json(): string;
    /**
     * Runs probes, retransmits and path selection.
     */
    tick(now_ms: number): void;
}

/**
 * Builds a transport from a JSON config.
 *
 * The config carries `sessionId`, `identity`, `daemonPublic`, optional `psk`
 * and `resumeToken`, and a non-empty `paths` array of `{kind, endpoint}`.
 */
export function connect(config_json: string): SecureTransport;

/**
 * Generates a fresh device identity, encoded for persistence by JS.
 */
export function generate_identity(): string;

/**
 * Parses a pairing URI into JSON (`endpoint`, `daemonPublic`, `psk`, `label`).
 */
export function parse_pairing(uri: string): string;

/**
 * Returns the transport protocol version.
 */
export function transport_version(): number;

export type InitInput = RequestInfo | URL | Response | BufferSource | WebAssembly.Module;

export interface InitOutput {
    readonly memory: WebAssembly.Memory;
    readonly __wbg_securetransport_free: (a: number, b: number) => void;
    readonly connect: (a: number, b: number) => [number, number, number];
    readonly generate_identity: () => [number, number, number, number];
    readonly parse_pairing: (a: number, b: number) => [number, number, number, number];
    readonly securetransport_close: (a: number, b: number) => void;
    readonly securetransport_on_closed: (a: number, b: any) => void;
    readonly securetransport_on_frame: (a: number, b: any) => void;
    readonly securetransport_on_packet: (a: number, b: any) => void;
    readonly securetransport_on_status: (a: number, b: any) => void;
    readonly securetransport_on_ticket: (a: number, b: any) => void;
    readonly securetransport_path_closed: (a: number, b: number, c: number, d: number, e: number) => [number, number];
    readonly securetransport_path_open: (a: number, b: number, c: number) => [number, number];
    readonly securetransport_receive: (a: number, b: number, c: number, d: number, e: number) => [number, number];
    readonly securetransport_resume_token_hex: (a: number) => [number, number];
    readonly securetransport_send: (a: number, b: number, c: number, d: number, e: number) => [number, number];
    readonly securetransport_start: (a: number, b: number) => [number, number];
    readonly securetransport_status_json: (a: number) => [number, number];
    readonly securetransport_tick: (a: number, b: number) => void;
    readonly transport_version: () => number;
    readonly __wbindgen_exn_store: (a: number) => void;
    readonly __externref_table_alloc: () => number;
    readonly __wbindgen_externrefs: WebAssembly.Table;
    readonly __wbindgen_malloc: (a: number, b: number) => number;
    readonly __wbindgen_realloc: (a: number, b: number, c: number, d: number) => number;
    readonly __wbindgen_free: (a: number, b: number, c: number) => void;
    readonly __externref_table_dealloc: (a: number) => void;
    readonly __wbindgen_start: () => void;
}

export type SyncInitInput = BufferSource | WebAssembly.Module;

/**
 * Instantiates the given `module`, which can either be bytes or
 * a precompiled `WebAssembly.Module`.
 *
 * @param {{ module: SyncInitInput }} module - Passing `SyncInitInput` directly is deprecated.
 *
 * @returns {InitOutput}
 */
export function initSync(module: { module: SyncInitInput } | SyncInitInput): InitOutput;

/**
 * If `module_or_path` is {RequestInfo} or {URL}, makes a request and
 * for everything else, calls `WebAssembly.instantiate` directly.
 *
 * @param {{ module_or_path: InitInput | Promise<InitInput> }} module_or_path - Passing `InitInput` directly is deprecated.
 *
 * @returns {Promise<InitOutput>}
 */
export default function __wbg_init (module_or_path?: { module_or_path: InitInput | Promise<InitInput> } | InitInput | Promise<InitInput>): Promise<InitOutput>;
