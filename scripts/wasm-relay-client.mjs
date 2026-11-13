#!/usr/bin/env node
// Drives the wasm transport core through an authenticated h4rn-connect relay
// against a real daemon. Node owns the relay socket and the JSON relay
// framing; wasm owns the Noise session and every cryptographic operation
// (ADR-033). `scripts/test-remote.sh` builds the wasm glue into WASM_DIR and
// runs this once, after the Rust e2e has paired the device.
//
// Environment:
//   WASM_DIR       directory produced by `wasm-bindgen --target nodejs`
//   RELAY_URL      single-use authenticated ws(s)://.../relay URL
//   MACHINE_ID     daemon machine id
//   IDENTITY       `DeviceIdentity::encode` of a device already in the allowlist
//   DAEMON_PUBLIC  daemon X25519 public key, hex
//   SESSION_ID     optional 16-byte hex session id (generated otherwise)

import { createRequire } from "node:module";
import { randomBytes } from "node:crypto";
import path from "node:path";

const require = createRequire(import.meta.url);
const env = process.env;
const wasm = require(path.join(env.WASM_DIR, "h4rn_transport_wasm.js"));

const relayUrl = required("RELAY_URL");
const machineId = required("MACHINE_ID");
const identity = required("IDENTITY");
const daemonPublic = required("DAEMON_PUBLIC");
const sessionId = env.SESSION_ID || randomBytes(16).toString("hex");
const connId = sessionId;
const started = Date.now();
const now = () => Date.now() - started;

function required(name) {
  const value = env[name];
  if (!value) {
    fail(`missing ${name}`);
  }
  return value;
}

function fail(message) {
  console.error(`wasm relay client failed: ${message}`);
  process.exit(1);
}

const config = JSON.stringify({
  sessionId,
  identity,
  daemonPublic,
  paths: [{ kind: "relay", endpoint: "relay:node" }],
});

let transport;
try {
  transport = wasm.connect(config);
} catch (error) {
  fail(`connect: ${error}`);
}

const ws = new WebSocket(relayUrl);
let opened = false;
let helloPending = false;
let hostsSeen = false;
let finished = false;

const timer = setTimeout(() => fail(`timed out; status=${transport.status_json()}`), 20_000);

function sendRelay(frame) {
  if (ws.readyState !== WebSocket.OPEN) {
    fail("relay socket is not open");
  }
  ws.send(JSON.stringify(frame));
}

function sendApp(frame) {
  transport.send(new TextEncoder().encode(JSON.stringify(frame)), false, now());
}

transport.on_packet((_index, bytes) => {
  sendRelay({
    conn_id: connId,
    kind: "data",
    payload: Buffer.from(bytes).toString("base64url"),
  });
});

transport.on_ticket(() => {
  if (helloPending) {
    return;
  }
  helloPending = true;
  // Never re-enter wasm from inside a wasm callback.
  setTimeout(() => {
    sendApp({
      id: 1,
      body: {
        type: "hello",
        protocol_version: 1,
        device: {
          device_id: "dev_wasm_relay",
          name: "wasm relay e2e",
          platform: "web",
          app_version: "0.1.0",
        },
      },
    });
  }, 0);
});

transport.on_frame((frame) => {
  if (typeof frame !== "string") {
    return;
  }
  setTimeout(() => handleFrame(frame), 0);
});

function handleFrame(frame) {
  const parsed = JSON.parse(frame);
  if (parsed.kind !== "response") {
    return;
  }
  // `ServerFrame::Response.result` is a Rust `Result`, which serialises as
  // `{"Ok": ...}` or `{"Err": ...}`.
  const result = parsed.result && parsed.result.Ok;
  if (parsed.id === 1) {
    if (result && result.type === "hello") {
      sendApp({ id: 2, body: { type: "list_hosts" } });
    } else {
      fail(`hello rejected: ${JSON.stringify(parsed.result)}`);
    }
  } else if (parsed.id === 2) {
    if (!result || result.type !== "hosts") {
      fail(`hosts rejected: ${JSON.stringify(parsed.result)}`);
    }
    hostsSeen = true;
    finished = true;
    clearTimeout(timer);
    console.log(
      JSON.stringify({
        ok: true,
        sessionId,
        hosts: result.hosts.length,
        state: JSON.parse(transport.status_json()).state,
      }),
    );
    transport.close(now());
    ws.close();
    setTimeout(() => process.exit(0), 50);
  }
}

transport.on_closed((reason) => fail(`transport closed: ${reason}`));

ws.onopen = () => {
  opened = true;
  const open = Buffer.from(
    JSON.stringify({ machine_id: machineId }),
    "utf8",
  ).toString("base64url");
  sendRelay({ conn_id: connId, kind: "open", payload: open });
  transport.start(now());
  setInterval(() => transport.tick(now()), 25);
};

ws.onmessage = (event) => {
  if (typeof event.data !== "string") {
    return;
  }
  const message = JSON.parse(event.data);
  if (message.type === "frame") {
    const frame = message.frame;
    if (!frame || frame.conn_id !== connId) {
      return;
    }
    if (frame.kind === "data") {
      transport.receive(0, Buffer.from(frame.payload, "base64url"), now());
    } else if (frame.kind === "close") {
      if (!hostsSeen) {
        fail("the relay closed the session");
      }
    }
  } else if (message.type === "error") {
    if (!message.conn_id || message.conn_id === connId) {
      fail(`relay error ${message.error?.code}: ${message.error?.message}`);
    }
  }
};

ws.onerror = (event) => {
  if (!finished) {
    fail(`relay socket error: ${event.message || "unknown"}`);
  }
};
ws.onclose = () => {
  if (!finished) {
    fail(`relay socket closed (opened=${opened})`);
  }
};
