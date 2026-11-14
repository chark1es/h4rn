import initWasm from "@/gen/transport-wasm/h4rn_transport_wasm";

/**
 * One shared initialization promise for the Rust transport module.
 *
 * Both the Noise transport and the remote connect client need the wasm
 * instance; `initWasm` is not safe to call twice on the same module, so every
 * caller awaits this instead.
 */
let ready: Promise<unknown> | null = null;

export function loadTransportWasm(): Promise<unknown> {
  ready ??= initWasm();
  return ready;
}
