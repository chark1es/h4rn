/**
 * h4rn-connect client.
 *
 * Every call that authenticates does so with an Ed25519 DPoP proof produced by
 * the Rust wasm module: TypeScript moves opaque strings and never sees a
 * signing key. Session tokens live in the platform secret store, not in
 * `localStorage` (ADR-010, ADR-011).
 */

import type {
  ConnectDevice,
  ConnectDeviceKind,
  ConnectErrorBody,
  ConnectMeResponse,
  ConnectUser,
  DeviceGrantPollResponse,
  DeviceGrantStartResponse,
  DeviceRegistration,
  LoginCompleteResponse,
  LoginStartResponse,
  MachineSummary,
  PairingClaimResponse,
  PairingCreateResponse,
  PairingInfo,
  ProviderInfo,
} from "@/gen";
import { connect_proof, identity_public } from "@/gen/transport-wasm/h4rn_transport_wasm";
import { ensureDeviceIdentity } from "@/lib/client/secure";
import { loadTransportWasm } from "@/lib/client/wasm";

import { deleteSecret, getSecret, setSecret } from "./secretStore";

const CREDENTIALS_KEY = "credentials";

/** Connect's `expires_in` is an i64 on the wire, a number in JSON. */
export type SessionTokens = {
  access_token: string;
  refresh_token: string;
  expires_in: number;
  token_type: string;
};

export type RemoteCredentials = {
  connectUrl: string;
  user: ConnectUser;
  device: ConnectDevice;
  tokens: SessionTokens;
  /** Epoch milliseconds; computed when tokens are stored. */
  expires_at: number;
};

export type ConnectError = Error & { code: string; hint: string | null };

function errorFromBody(status: number, body: ConnectErrorBody | null): ConnectError {
  const error = new Error(body?.message ?? `h4rn-connect answered ${status}.`) as ConnectError;
  error.code = body?.code ?? "connect_error";
  error.hint = body?.hint ?? null;
  return error;
}

async function readError(response: Response): Promise<ConnectError> {
  try {
    const body = (await response.json()) as ConnectErrorBody;
    return errorFromBody(response.status, body);
  } catch {
    return errorFromBody(response.status, null);
  }
}

function join(base: string, path: string): string {
  return `${base.replace(/\/+$/, "")}${path}`;
}

export function normalizeTokens(tokens: {
  access_token: string;
  refresh_token: string;
  expires_in: number | bigint;
  token_type: string;
}): SessionTokens {
  return {
    access_token: tokens.access_token,
    refresh_token: tokens.refresh_token,
    expires_in: Number(tokens.expires_in),
    token_type: tokens.token_type,
  };
}

/** Base64url (unpadded) to hex, for the Noise configuration. */
export function base64UrlToHex(value: string): string {
  const normalised = value.replace(/-/g, "+").replace(/_/g, "/");
  const padded = normalised.padEnd(Math.ceil(normalised.length / 4) * 4, "=");
  const binary = atob(padded);
  let hex = "";
  for (let index = 0; index < binary.length; index += 1) {
    hex += binary.charCodeAt(index).toString(16).padStart(2, "0");
  }
  return hex;
}

/** Public identity of this device, as connect wants it registered. */
export async function deviceRegistration(
  kind: ConnectDeviceKind,
  name: string,
): Promise<DeviceRegistration> {
  await loadTransportWasm();
  const identity = await ensureDeviceIdentity();
  const publicIdentity = JSON.parse(identity_public(identity)) as {
    proofPublicKey: string;
    noisePublic: string;
  };
  return {
    name,
    kind,
    proof_public_key: publicIdentity.proofPublicKey,
    noise_static_public_key: publicIdentity.noisePublic,
    platform: platformLabel(),
    machine_name: kind === "daemon" ? name : null,
  };
}

export function platformLabel(): string {
  if (typeof navigator === "undefined") return "web";
  const ua = navigator.userAgent;
  if (/Android/i.test(ua)) return "android";
  if (/iPhone|iPad|iPod/i.test(ua)) return "ios";
  if (/Macintosh/i.test(ua)) return "macos";
  if (/Windows/i.test(ua)) return "windows";
  if (/Linux/i.test(ua)) return "linux";
  return "web";
}

async function randomNonce(): Promise<string> {
  const bytes = new Uint8Array(16);
  crypto.getRandomValues(bytes);
  return btoa(String.fromCharCode(...bytes))
    .replace(/\+/g, "-")
    .replace(/\//g, "_")
    .replace(/=+$/, "");
}

/** Reads the persisted credentials, if this device is signed in. */
export async function readCredentials(): Promise<RemoteCredentials | null> {
  const raw = await getSecret(CREDENTIALS_KEY);
  if (!raw) return null;
  try {
    const parsed = JSON.parse(raw) as RemoteCredentials;
    if (!parsed.connectUrl || !parsed.tokens?.access_token) return null;
    return parsed;
  } catch {
    return null;
  }
}

export async function writeCredentials(credentials: RemoteCredentials): Promise<void> {
  await setSecret(CREDENTIALS_KEY, JSON.stringify(credentials));
}

export async function clearCredentials(): Promise<void> {
  await deleteSecret(CREDENTIALS_KEY);
}

async function publicJson<T>(
  baseUrl: string,
  path: string,
  init: { method?: string; body?: unknown } = {},
): Promise<T> {
  const response = await fetch(join(baseUrl, path), {
    method: init.method ?? "GET",
    headers: { "content-type": "application/json" },
    ...(init.body === undefined ? {} : { body: JSON.stringify(init.body) }),
  });
  if (!response.ok) throw await readError(response);
  if (response.status === 204) return undefined as T;
  return (await response.json()) as T;
}

// ---------------------------------------------------------------------------
// Unauthenticated auth endpoints
// ---------------------------------------------------------------------------

export function listProviders(connectUrl: string): Promise<ProviderInfo[]> {
  return publicJson<ProviderInfo[]>(connectUrl, "/v1/auth/providers");
}

export async function startLogin(
  connectUrl: string,
  provider: string,
  redirectUri: string,
  device: DeviceRegistration,
): Promise<LoginStartResponse> {
  return publicJson<LoginStartResponse>(connectUrl, `/v1/auth/${provider}/start`, {
    method: "POST",
    body: { redirect_uri: redirectUri, device },
  });
}

export async function completeLogin(
  connectUrl: string,
  provider: string,
  state: string,
  code: string,
  redirectUri: string,
): Promise<LoginCompleteResponse> {
  return publicJson<LoginCompleteResponse>(connectUrl, `/v1/auth/${provider}/complete`, {
    method: "POST",
    body: { state, code, redirect_uri: redirectUri },
  });
}

export async function startDeviceGrant(
  connectUrl: string,
  provider: string,
  device: DeviceRegistration,
): Promise<DeviceGrantStartResponse> {
  return publicJson<DeviceGrantStartResponse>(connectUrl, "/v1/auth/device/start", {
    method: "POST",
    body: { provider, device },
  });
}

export function pollDeviceGrant(
  connectUrl: string,
  grantId: string,
): Promise<DeviceGrantPollResponse> {
  return publicJson<DeviceGrantPollResponse>(connectUrl, "/v1/auth/device/poll", {
    method: "POST",
    body: { grant_id: grantId },
  });
}

// ---------------------------------------------------------------------------
// Authenticated client
// ---------------------------------------------------------------------------

export class ConnectClient {
  private credentials: RemoteCredentials;
  private refreshing: Promise<void> | null = null;

  constructor(credentials: RemoteCredentials) {
    this.credentials = credentials;
  }

  get connectUrl(): string {
    return this.credentials.connectUrl;
  }

  get user(): ConnectUser {
    return this.credentials.user;
  }

  get device(): ConnectDevice {
    return this.credentials.device;
  }

  get tokens(): SessionTokens {
    return this.credentials.tokens;
  }

  async credentialsSnapshot(): Promise<RemoteCredentials> {
    return this.credentials;
  }

  /** Persists the current credentials after a refresh or rotation. */
  private async persist(): Promise<void> {
    this.credentials = {
      ...this.credentials,
      tokens: this.credentials.tokens,
      expires_at: Date.now() + Math.max(0, Number(this.credentials.tokens.expires_in) - 30) * 1_000,
    };
    await writeCredentials(this.credentials);
  }

  private async proof(
    method: string,
    path: string,
  ): Promise<{
    proof: string;
    timestamp: string;
    nonce: string;
  }> {
    await loadTransportWasm();
    const identity = await ensureDeviceIdentity();
    const timestamp = Math.floor(Date.now() / 1_000);
    const nonce = await randomNonce();
    const result = JSON.parse(connect_proof(identity, method, path, BigInt(timestamp), nonce)) as {
      proof: string;
      timestamp: number;
      nonce: string;
    };
    return {
      proof: result.proof,
      timestamp: String(result.timestamp),
      nonce: result.nonce,
    };
  }

  private async refresh(): Promise<void> {
    if (this.refreshing) return this.refreshing;
    this.refreshing = (async () => {
      const tokens = normalizeTokens(
        await publicJson<{
          access_token: string;
          refresh_token: string;
          expires_in: number | bigint;
          token_type: string;
        }>(this.connectUrl, "/v1/auth/refresh", {
          method: "POST",
          body: { refresh_token: this.credentials.tokens.refresh_token },
        }),
      );
      this.credentials = { ...this.credentials, tokens };
      await this.persist();
    })();
    try {
      await this.refreshing;
    } finally {
      this.refreshing = null;
    }
  }

  private async authorized(
    method: string,
    path: string,
    body?: unknown,
    retried = false,
  ): Promise<Response> {
    if (Date.now() >= this.credentials.expires_at) {
      try {
        await this.refresh();
      } catch {
        // The access token may still be valid; let the request decide.
      }
    }
    const proof = await this.proof(method, path);
    const response = await fetch(join(this.connectUrl, path), {
      method,
      headers: {
        "content-type": "application/json",
        authorization: `Bearer ${this.credentials.tokens.access_token}`,
        "x-h4rn-proof": proof.proof,
        "x-h4rn-timestamp": proof.timestamp,
        "x-h4rn-nonce": proof.nonce,
      },
      ...(body === undefined ? {} : { body: JSON.stringify(body) }),
    });
    if (response.status === 401 && !retried) {
      await this.refresh();
      return this.authorized(method, path, body, true);
    }
    if (!response.ok) throw await readError(response);
    return response;
  }

  private async authorizedJson<T>(method: string, path: string, body?: unknown): Promise<T> {
    const response = await this.authorized(method, path, body);
    if (response.status === 204) return undefined as T;
    return (await response.json()) as T;
  }

  me(): Promise<ConnectMeResponse> {
    return this.authorizedJson<ConnectMeResponse>("GET", "/v1/me");
  }

  async machines(): Promise<MachineSummary[]> {
    return this.authorizedJson<MachineSummary[]>("GET", "/v1/machines");
  }

  async devices(): Promise<ConnectDevice[]> {
    return this.authorizedJson<ConnectDevice[]>("GET", "/v1/devices");
  }

  async renameDevice(id: string, name: string): Promise<ConnectDevice> {
    return this.authorizedJson<ConnectDevice>("PATCH", `/v1/devices/${id}`, { name });
  }

  async revokeDevice(id: string): Promise<void> {
    await this.authorized("DELETE", `/v1/devices/${id}`);
  }

  async listPairings(): Promise<PairingInfo[]> {
    return this.authorizedJson<PairingInfo[]>("GET", "/v1/pairings");
  }

  async createPairing(machineId: string): Promise<PairingCreateResponse> {
    return this.authorizedJson<PairingCreateResponse>("POST", "/v1/pairings", {
      machine_id: machineId,
    });
  }

  async revokePairing(id: string): Promise<void> {
    await this.authorized("DELETE", `/v1/pairings/${id}`);
  }

  async claimPairing(payload: string, name: string): Promise<PairingClaimResponse> {
    await loadTransportWasm();
    const identity = await ensureDeviceIdentity();
    const publicIdentity = JSON.parse(identity_public(identity)) as {
      proofPublicKey: string;
      noisePublic: string;
    };
    return this.authorizedJson<PairingClaimResponse>("POST", "/v1/pairings/claim", {
      payload,
      device_public_key: publicIdentity.proofPublicKey,
      noise_static_public_key: publicIdentity.noisePublic,
      device_name: name,
    });
  }

  /** Builds an authenticated relay WebSocket URL for one attempt. */
  async mintRelayUrl(machineId: string): Promise<string> {
    const proof = await this.proof("GET", "/relay");
    const url = new URL(join(this.connectUrl, "/relay"));
    url.protocol = url.protocol === "https:" ? "wss:" : "ws:";
    url.searchParams.set("access_token", this.credentials.tokens.access_token);
    url.searchParams.set("proof", proof.proof);
    url.searchParams.set("timestamp", proof.timestamp);
    url.searchParams.set("nonce", proof.nonce);
    url.searchParams.set("machine_id", machineId);
    return url.toString();
  }

  async logout(): Promise<void> {
    try {
      await this.authorized("POST", "/v1/auth/logout", {
        refresh_token: this.credentials.tokens.refresh_token,
      });
    } catch {
      // Local sign-out must still succeed when the relay is unreachable.
    }
    await clearCredentials();
  }
}
