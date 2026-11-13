# Encrypted multipath transport

This document describes the Phase 3 transport: how a device and a daemon
establish an end-to-end encrypted session, how that session survives packet
loss and path changes, and what the relay can and cannot see. The decisions
behind it are [ADR-009](decisions.md#adr-009-multipath-transport-with-a-path-agnostic-session-layer),
[ADR-010](decisions.md#adr-010-noise-e2e-between-device-and-daemon-the-relay-sees-metadata-only),
[ADR-031](decisions.md#adr-031-one-noise-ik-handshake-per-path-pinned-daemon-key-and-device-allowlist),
[ADR-032](decisions.md#adr-032-the-session-layer-owns-reliability-and-resume-not-the-socket) and
[ADR-033](decisions.md#adr-033-wasm-owns-the-crypto-javascript-owns-sockets-and-timers).

## Goals

- **End-to-end confidentiality and integrity.** The daemon's host, a LAN
  observer and the h4rn-connect relay only see opaque packets and metadata.
- **Path independence.** A LAN socket, a Tailscale socket and a relay socket
  can all carry the same logical session. Switching paths does not drop,
  duplicate or reorder an application frame.
- **Fast failover.** Losing the active path moves the session to a warm
  standby and retransmits unacknowledged records, normally within one
  probe interval (250 ms; the tests assert under 2 s, including a delayed
  probe).
- **Reconnect without an application gap.** The daemon keeps a detached
  session for a grace period and the client resumes with an opaque token, so
  the protocol's `since_seq` replay is not needed for a transport-level
  failure.
- **One implementation, native and wasm.** The core is a synchronous state
  machine with an injected clock; native async code and the browser wrapper
  are thin adapters. TypeScript never sees key material.

## Components

| Crate | Responsibility |
| --- | --- |
| `h4rn-protocol` | `TransportStatus`, `TransportPathKind`, path/session state enums and the `Event::TransportStatus` variant. Generated TypeScript bindings live in `apps/ui/src/gen`. |
| `h4rn-transport` | Wire framing, Noise channels, session layer, client engine, daemon session registry, native client, relay connector trait. |
| `h4rn-transport-wasm` | `wasm-bindgen` facade over the same core for the browser. |
| `h4rn-daemon` | `/secure` WebSocket route, `SecureService`, socket adapter into the existing protocol loop. |
| `apps/ui` | `SecureTransport` (`src/lib/client/secure.ts`): WebSockets, timers, persisted identity/resume token. |

## Wire format

One WebSocket binary message carries exactly one packet. The header is clear
because the daemon must demultiplex a handshake before it has session keys,
and the relay routes by ids:

```text
offset  size  field
0       2     magic "HT"
2       1     version (1)
3       1     kind (1 handshake, 2 record, 3 reject)
4       16    session id (client-chosen, opaque to the relay)
20      ..    body
```

`Reject` carries one byte from `RejectReason` (`not paired`, `revoked`,
`bad handshake`, `unsupported version`) and is sent in the clear before any
session exists. It never contains peer-controlled text.

A record body is a Noise transport message. Its plaintext is:

```text
offset  size  field
0       1     version
1       1     kind (1 data, 2 control, 3 ack)
2       1     flags (bit 0 = binary application frame)
3       1     reserved
4       8     message id (groups fragments of one application frame)
12      2     fragment index
14      1     last-fragment flag
15      1     reserved
16      8     per-direction sequence number
24      8     cumulative ack (highest contiguous sequence received)
32      4     ack bitmap for sequences below the cumulative ack
36      4     payload length
40      ..    payload, then zero padding to a 256-byte bucket
```

Application frames larger than one record are fragmented. A Noise message is
limited to 65 535 bytes, so `MAX_RECORD_PLAINTEXT` is 65 519 and
`MAX_FRAGMENT_BYTES` is 65 479. The daemon's default maximum application frame
is 512 KiB; larger frames are rejected rather than silently truncated.

Padding to 256-byte buckets hides exact prompt and chunk sizes from the relay.
It is not a defence against traffic analysis over time.

## Handshake and identity

The pattern is:

- `Noise_IK_25519_ChaChaPoly_BLAKE2s` when the device is already allowlisted;
- `Noise_IKpsk2_25519_ChaChaPoly_BLAKE2s` when a one-time pairing PSK is in
  use.

The device has two static keys:

- **X25519**, used as the Noise static key and as its identity in the
  daemon's allowlist;
- **Ed25519**, persisted alongside it as the signing identity for future
  device events and pairing proofs.

`IK` is two messages. The client initiates with the daemon's pinned X25519
public key; the daemon completes and both sides verify the handshake hash.
Because the daemon key is pinned from the pairing payload, an active
man-in-the-middle cannot substitute its own key. With a PSK, `IKpsk2` also
authenticates the client, which is what lets the daemon add a new device to
the allowlist on first pairing.

The prologue is `h4rn-transport/1 || session_id`, binding each Noise channel
to one logical session. A channel from another session cannot be replayed onto
this one even if keys were somehow shared.

### Pairing

A pairing payload is a URI:

```text
h4rn://pair?e=<host:port>&d=<daemon x25519 public, hex>&p=<one-time psk, hex>&l=<label>
```

The PSK is single-use. After the first successful handshake the device keeps
the pinned daemon key and drops the PSK; the daemon allowlists the device key
and rotates the payload. A stale or photographed payload fails the handshake
instead of granting access.

### Allowlist and revocation

`PairingPolicy` holds allowed device X25519 keys, an optional PSK, and an
explicit revocation list. Revocation is an allowlist removal plus a `revoked`
marker, so a revoked device gets the specific `revoked` reject reason even if
the PSK is still configured. Revocation is a device-level event; the daemon's
own key does not rotate (ADR-010).

### Rekey and replay

Each path rekeys after a configurable message or byte budget (`RekeyLimits`,
default 1 048 576 messages or 128 MiB, whichever comes first). Rekeying uses
Noise's handshake rekey and never restarts the session sequence space.

The session layer rejects records whose sequence is older than the receive
window and tracks the 64-sequence ack bitmap for out-of-order delivery.
Duplicate ciphertext is **not** replayed: Noise nonces are strict, so a
retransmission is always freshly sealed under the next nonce, with the same
session sequence and message id. The receiver deduplicates by session
sequence, never by ciphertext.

## Session layer

Reliability is owned by the session, not by a socket:

- Each direction has a monotonically increasing 64-bit sequence.
- Every record carries a cumulative ack and a 64-bit bitmap of the sequences
  below it, so selective retransmission is possible.
- Unacknowledged records stay in a bounded retransmit buffer (1024 records /
  16 MiB by default). Acks are coalesced for 40 ms, or piggybacked on the next
  data record.
- Incoming data is reassembled by message id and fragment index. Duplicate
  fragments are discarded.
- Backpressure rejects new frames instead of growing without bound.

Control records are themselves sequenced and retransmitted. The first record
in each direction is `Open`:

```text
Open { token: Option<[u8; 32]>, recv_next: u64, send_next: u64 }
```

`token = None` starts a new logical session. A token resumes an existing one.
`recv_next` tells the peer the next sequence this side can accept; `send_next`
lets both sides reconcile after one endpoint lost its retransmit buffer. The
daemon answers `Welcome { token, recv_next, send_next }`; a client that
reconnects receives the same token and reconciliation positions.

Because the token travels inside the Noise-encrypted `Open` record, a relay
cannot steal or replay it. The session id is visible in the clear, but it is
meaningless without the Noise keys.

## Multipath policy

The client engine owns path choice. The daemon does not choose a path; it
tracks every attached path and sends outbound frames on the most recently
attached one (the path the client just proved alive).

Candidates are ranked `direct < tailscale < relay`, then by smoothed RTT.
Every candidate is probed in parallel:

1. Open a socket and run a fresh Noise handshake.
2. On success, mark the path `Ready` and begin health pings every 250 ms.
3. The best ready path becomes `Active`; all others stay warm standbys.

Rules:

- **Failover is immediate.** If the active path fails, the next ready path is
  promoted and every unacknowledged record is retransmitted on it.
- **Switch-back is hysteretic.** A better-ranked path must beat the current
  path's smoothed RTT by `switch_margin_ms` (40 ms default) for
  `switch_confirmations` probes (2) before it is promoted. A 2 ms wobble
  cannot flap the path.
- **Health pings are authenticated control records.** A path that carries no
  authenticated packet for `path_timeout_ms` (1 s) is declared failed, which
  also removes half-open sockets.
- **The relay is a standby, not on-demand.** Keeping it warm costs one idle
  connection and removes a multi-second provisioning delay at the exact
  moment the direct path died (ADR-009).

The client emits a `TransportStatus` snapshot whenever path state changes:

```json
{
  "type": "transport_status",
  "session_id": "00112233445566778899aabbccddeeff",
  "state": "online",
  "active": { "kind": "direct", "endpoint": "192.168.1.20:47800", "state": "active", "rtt_ms": 3, "encrypted": true },
  "candidates": [ "..." ],
  "switched": true,
  "detail": "failover",
  "updated_at": "2026-11-12T23:00:00Z"
}
```

Like filesystem hints and search batches, `transport_status` is **transient**:
it reuses the current durable sequence number and must not advance the client's
resume cursor.

## Daemon integration

`/secure` is always registered. It answers `404` when no secure service is
configured, so an unconfigured daemon fails closed rather than silently
serving plaintext. The listener wraps each accepted socket in a `SecureSocket`
that implements the same `Sink<Message>` / `Stream<Item = Result<Message, _>>`
interface as the plaintext WebSocket, and the existing protocol loop is
unchanged.

Configuration:

| Variable | Meaning |
| --- | --- |
| `H4RN_SECURE=1` | Enable the Noise service and `/secure`. |
| `H4RN_PAIRING_PSK=<64 hex chars>` | Enable one-time `IKpsk2` pairing with this PSK. |

The daemon's identity is persisted as `transport-identity` in its data
directory. A non-loopback bind rejects plaintext `/ws` with `403`; loopback
keeps `/ws` for local development and the desktop sidecar. An explicit
`DaemonBuilder::with_secure(...)` overrides the environment path.

Detached sessions are kept for a grace period of four path timeouts (8 s with
the default 2 s) so a client that reconnects, or fails over from LAN to relay,
can resume the same sequence space. A resume with an unknown or wrong token is
rejected and the client starts a fresh session.

## Browser client

`apps/ui/src/gen/transport-wasm` is built with:

```sh
wasm-pack build crates/h4rn-transport-wasm --target web \
  --out-name h4rn_transport_wasm --out-dir apps/ui/src/gen/transport-wasm
```

`SecureTransport` (`apps/ui/src/lib/client/secure.ts`) implements the existing
`Transport` interface. It owns:

- one `WebSocket` per candidate path;
- a 50 ms tick that drives probes, retransmits and selection;
- `localStorage` for the identity (generated by wasm), the pinned daemon key,
  and the session id/resume token.

It owns **no** cryptography. Pairing URIs are parsed, identities generated,
handshakes run and frames sealed inside wasm. The PSK is removed from storage
after the first successful handshake. The identity's secret key in
`localStorage` is a deliberate web-platform limitation: the browser has no OS
keychain API; the desktop shell uses the native keychain instead (ADR-016).

Open the UI with `?pair=<h4rn://pair?...>`, or with `?secure=1` after a daemon
has already been pinned. `?relay=<host:port>` adds a standby relay path.
`?daemon=` alone keeps the old plaintext loopback behavior.

## Threat model

**Protected against**

- Passive network observers and the relay reading prompts, outputs, file
  contents or tokens.
- A stranger who photographs the pairing QR code: the PSK is single-use and
  the device key is allowlisted only after a successful handshake.
- An impostor daemon: the device pins the daemon's X25519 key.
- Replay of old records: Noise nonces plus session sequence windows.
- A revoked device: the daemon rejects its static key with `revoked`.
- Silent corruption: ChaCha20-Poly1305 authenticates every record and the
  handshake hash binds both static keys.

**Not protected against**

- A compromised endpoint. Malware on the device or daemon can read plaintext
  before encryption or after decryption.
- Traffic analysis. The relay learns connection ids, timing, packet counts
  and coarse size buckets. It does not learn thread counts (those are inside
  encrypted frames).
- Denial of service. An unauthenticated peer can open sockets and force
  handshake work; rate limiting belongs to the listener and the host firewall.
- Post-quantum adversaries. X25519 and Ed25519 are not PQ-safe. A future
  hybrid handshake is a protocol-version change.
- A stolen browser profile. `localStorage` is readable by same-origin script;
  XSS is equivalent to device compromise. The desktop build uses the OS
  keychain.

## Limits and known behavior

- One Noise message is 65 535 bytes; application frames are fragmented above
  `MAX_FRAGMENT_BYTES`.
- Duplicate ciphertext cannot be decrypted; retransmits are freshly sealed
  with the same session sequence.
- Session reassembly and retransmit buffers are bounded; backpressure is
  surfaced to the caller instead of unbounded memory growth.
- The daemon sends on the newest attached path. A client that wants a
  different path must attach it; the daemon does not run its own selection.
- A resume token is only valid while the daemon keeps the detached session
  (four path timeouts, 8 s by default). After the grace period the client
  starts a new session and the application protocol replays its own gap.
- `TransportStatus` carries a wall-clock `updated_at`; all policy decisions
  use the injected monotonic clock, so a system clock jump cannot cause
  failover.

## Tests

| Test | What it proves |
| --- | --- |
| `h4rn-transport` unit tests | Framing, Noise handshake, pinned key, PSK, revocation, rekey, session acks/retransmit. |
| `crates/h4rn-transport/tests/multipath.rs` | Reorder/duplication, failover with no loss or duplication in under 2 s, hysteretic switch-back, resume without replay. |
| `crates/h4rn-daemon/tests/secure.rs` | Real daemon over Noise WebSocket: `CreateThread` with `FakeHarness`, revoked device, PSK pairing. |
| `h4rn-transport-wasm` `handshake_smoke` | The same core handshake natively and under `wasm-pack test --node`. |
| `apps/ui` typecheck/lint/test/build | `SecureTransport` compiles, the wasm glue bundles, and existing client tests stay green. |
