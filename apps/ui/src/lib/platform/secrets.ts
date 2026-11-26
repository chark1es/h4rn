/**
 * One platform secret store for every credential the view handles.
 *
 * The UI is a view over the daemon (ADR-001), so the only secrets it ever
 * touches are the values it moves between the daemon, h4rn-connect and the
 * platform. They all go through this module:
 *
 * - Tauri mobile: Android Keystore / iOS Keychain behind `secret_*` (ADR-016).
 * - Tauri desktop: the OS keychain behind `keychain_*` (ADR-016).
 * - Web: AES-GCM envelopes in IndexedDB under a non-extractable CryptoKey.
 *   The browser has no OS keychain, so this is the strongest at-rest store it
 *   can offer; `localStorage` and `sessionStorage` never see a secret
 *   (ADR-038).
 *
 * Reads are asynchronous, but boot needs a few values synchronously (for
 * example `readBootConfig`). `prepareSecrets()` loads them once before React
 * mounts and `peekSecret()` then answers from an in-memory cache. Writes made
 * after boot update the cache immediately and persist in the background.
 */

import { i18n } from "@/lib/i18n";

const SERVICE = "h4rn";
const DB_NAME = "h4rn-secrets";
const DB_VERSION = 1;
const RECORD_STORE = "records";
const META_STORE = "meta";
const WRAP_KEY_ID = "wrap";

/** Keys boot needs synchronously. Dynamic per-machine keys load on demand. */
const KNOWN_KEYS = [
  "h4rn.token",
  "h4rn.pairing",
  "h4rn.remote.credentials",
  "h4rn.secure.identity",
  "h4rn.secure.pin",
  "h4rn.secure.pending",
  "h4rn.secure.session",
  "h4rn.secure.pairing-uri",
] as const;

const LEGACY_KEYCHAIN_SERVICE = "h4rn-connect";
const LEGACY_KEYCHAIN_ACCOUNT = "credentials";

type TauriInternals = {
  invoke<T>(command: string, args?: Record<string, unknown>): Promise<T>;
};

type SecretResponse = { value: string | null } | null;

export type SecretShell = "mobile" | "desktop" | "web";

const memory = new Map<string, string>();
let prepared: Promise<void> | null = null;
let webDb: Promise<IDBDatabase | null> | null = null;
let webKey: Promise<CryptoKey | null> | null = null;

function internals(): TauriInternals | null {
  if (typeof window === "undefined") return null;
  const candidate = (window as unknown as { __TAURI_INTERNALS__?: TauriInternals })
    .__TAURI_INTERNALS__;
  return typeof candidate?.invoke === "function" ? candidate : null;
}

/** Which secret backend this page can use. */
export function secretShell(): SecretShell {
  const shell = internals();
  if (!shell) return "web";
  return /Android|iPhone|iPad|iPod/i.test(window.navigator.userAgent) ? "mobile" : "desktop";
}

/** The cached value, available synchronously after `prepareSecrets()`. */
export function peekSecret(key: string): string | null {
  return memory.get(key) ?? null;
}

/**
 * A synchronous write for callback paths that cannot await.
 *
 * The cache updates now and the backend write is best-effort; every caller
 * that can await should prefer `setSecret`.
 */
export function rememberSecret(key: string, value: string): void {
  memory.set(key, value);
  void persist(key, value);
}

/** A synchronous delete, mirroring `rememberSecret`. */
export function forgetSecret(key: string): void {
  memory.delete(key);
  void erase(key);
}

/** Reads one secret, or `null` when it was never stored. */
export async function getSecret(key: string): Promise<string | null> {
  await prepareSecrets();
  if (memory.has(key)) return memory.get(key) ?? null;
  const value = await readBackend(key);
  if (value !== null) memory.set(key, value);
  return value;
}

/** Stores one secret and makes it visible to later synchronous reads. */
export async function setSecret(key: string, value: string): Promise<void> {
  await prepareSecrets();
  memory.set(key, value);
  await persist(key, value);
}

/** Removes one secret; a missing entry is not an error. */
export async function deleteSecret(key: string): Promise<void> {
  await prepareSecrets();
  memory.delete(key);
  await erase(key);
}

/**
 * Loads every secret boot may need and migrates legacy browser storage.
 *
 * Safe to call more than once; only the first call performs work. Failures are
 * swallowed so a locked-down keychain degrades to this page load instead of
 * blocking the app.
 */
export function prepareSecrets(): Promise<void> {
  prepared ??= loadAll().catch(() => undefined);
  return prepared;
}

/** Clears the in-memory cache between tests. */
export function resetSecretStoreForTests(): void {
  memory.clear();
  prepared = null;
  webDb = null;
  webKey = null;
}

async function loadAll(): Promise<void> {
  const shell = secretShell();
  if (shell === "web") {
    await loadWebSecrets();
  } else {
    for (const key of KNOWN_KEYS) {
      const value = await readBackend(key);
      if (value !== null) memory.set(key, value);
    }
  }
  await migrateLegacyBrowserStorage();
  if (shell === "desktop") await migrateLegacyKeychain();
  await ingestBootSecrets();
}

/**
 * Moves secrets that arrived in the address bar into the store and strips them.
 *
 * `?token=` is the local-daemon bootstrap and `?pair=` carries a one-time
 * pairing URI (PSK included). Both are read once, persisted, and removed from
 * the visible URL so they cannot leak through history, screenshots or a copied
 * link (ADR-038).
 */
async function ingestBootSecrets(): Promise<void> {
  if (typeof window === "undefined") return;
  try {
    const url = new URL(window.location.href);
    let changed = false;
    const token = url.searchParams.get("token");
    if (token) {
      memory.set("h4rn.token", token);
      await persist("h4rn.token", token);
      url.searchParams.delete("token");
      changed = true;
    }
    const pairing = url.searchParams.get("pair");
    if (pairing) {
      memory.set("h4rn.secure.pairing-uri", pairing);
      await persist("h4rn.secure.pairing-uri", pairing);
      url.searchParams.delete("pair");
      changed = true;
    }
    if (changed) {
      window.history.replaceState({}, "", `${url.pathname}${url.search}${url.hash}`);
    }
  } catch {
    // A URL that cannot be rewritten still boots from the cached value.
  }
}

// ---------------------------------------------------------------------------
// Backend selection
// ---------------------------------------------------------------------------

async function readBackend(key: string): Promise<string | null> {
  const shell = secretShell();
  const shellInternals = internals();
  if (shell !== "web" && shellInternals) {
    try {
      if (shell === "mobile") {
        const response = await shellInternals.invoke<SecretResponse>("secret_get", { key });
        return response?.value ?? null;
      }
      return await shellInternals.invoke<string | null>("keychain_get", {
        service: SERVICE,
        account: key,
      });
    } catch {
      return null;
    }
  }
  if (shell === "web") return readWebSecret(key);
  return memory.get(key) ?? null;
}

async function persist(key: string, value: string): Promise<void> {
  const shell = secretShell();
  const shellInternals = internals();
  try {
    if (shell === "mobile" && shellInternals) {
      await shellInternals.invoke("secret_set", { key, value });
      return;
    }
    if (shell === "desktop" && shellInternals) {
      await shellInternals.invoke("keychain_set", {
        service: SERVICE,
        account: key,
        secret: value,
      });
      return;
    }
    if (shell === "web") await writeWebSecret(key, value);
  } catch {
    // The cache still serves this page load; a keychain that rejects a write
    // must not make the caller believe a credential is durable.
  }
}

async function erase(key: string): Promise<void> {
  const shell = secretShell();
  const shellInternals = internals();
  try {
    if (shell === "mobile" && shellInternals) {
      await shellInternals.invoke("secret_delete", { key });
      return;
    }
    if (shell === "desktop" && shellInternals) {
      await shellInternals.invoke("keychain_delete", { service: SERVICE, account: key });
      return;
    }
    if (shell === "web") await deleteWebSecret(key);
  } catch {
    // Ignore: a missing entry is not an error.
  }
}

// ---------------------------------------------------------------------------
// Legacy browser storage
// ---------------------------------------------------------------------------

/**
 * Keys that older builds wrote to `localStorage`. They are moved into the
 * platform store and deleted, so an upgrade never leaves a token behind.
 */
function isLegacySecretKey(key: string): boolean {
  return (
    key === "h4rn.token" ||
    key === "h4rn.pairing" ||
    key === "h4rn.remote.credentials" ||
    key.startsWith("h4rn.secure.")
  );
}

async function migrateLegacyBrowserStorage(): Promise<void> {
  if (typeof window === "undefined") return;
  try {
    const legacy = window.localStorage;
    const keys: string[] = [];
    for (let index = 0; index < legacy.length; index += 1) {
      const key = legacy.key(index);
      if (key !== null && isLegacySecretKey(key)) keys.push(key);
    }
    for (const key of keys) {
      const value = legacy.getItem(key);
      if (value === null) continue;
      memory.set(key, value);
      await persist(key, value);
      legacy.removeItem(key);
    }
  } catch {
    // A storage policy that blocks enumeration leaves nothing to migrate.
  }

  try {
    const session = window.sessionStorage;
    const credentials = session.getItem("h4rn.remote.credentials");
    if (credentials !== null) {
      memory.set("h4rn.remote.credentials", credentials);
      await persist("h4rn.remote.credentials", credentials);
      session.removeItem("h4rn.remote.credentials");
    }
  } catch {
    // Same as above.
  }
}

async function migrateLegacyKeychain(): Promise<void> {
  const shellInternals = internals();
  if (!shellInternals) return;
  if (memory.has("h4rn.remote.credentials")) {
    // A localStorage copy already won; drop the old service entry.
    await deleteLegacyKeychain(shellInternals);
    return;
  }
  try {
    const value = await shellInternals.invoke<string | null>("keychain_get", {
      service: LEGACY_KEYCHAIN_SERVICE,
      account: LEGACY_KEYCHAIN_ACCOUNT,
    });
    if (!value) return;
    memory.set("h4rn.remote.credentials", value);
    await persist("h4rn.remote.credentials", value);
    await deleteLegacyKeychain(shellInternals);
  } catch {
    // No legacy entry, or a locked keychain; both are fine.
  }
}

async function deleteLegacyKeychain(shellInternals: TauriInternals): Promise<void> {
  try {
    await shellInternals.invoke("keychain_delete", {
      service: LEGACY_KEYCHAIN_SERVICE,
      account: LEGACY_KEYCHAIN_ACCOUNT,
    });
  } catch {
    // Ignore.
  }
}

// ---------------------------------------------------------------------------
// Web: AES-GCM envelopes in IndexedDB
// ---------------------------------------------------------------------------

type StoredRecord = {
  key: string;
  iv: Uint8Array;
  data: ArrayBuffer;
};

type WrapRecord = {
  id: string;
  key: CryptoKey;
};

function browserCrypto(): Crypto | null {
  return typeof globalThis.crypto === "undefined" ? null : globalThis.crypto;
}

function openWebDatabase(): Promise<IDBDatabase | null> {
  webDb ??= new Promise((resolve) => {
    if (typeof indexedDB === "undefined") {
      resolve(null);
      return;
    }
    try {
      const request = indexedDB.open(DB_NAME, DB_VERSION);
      request.onupgradeneeded = () => {
        const database = request.result;
        if (!database.objectStoreNames.contains(RECORD_STORE)) {
          database.createObjectStore(RECORD_STORE, { keyPath: "key" });
        }
        if (!database.objectStoreNames.contains(META_STORE)) {
          database.createObjectStore(META_STORE, { keyPath: "id" });
        }
      };
      request.onsuccess = () => resolve(request.result);
      request.onerror = () => resolve(null);
      request.onblocked = () => resolve(null);
    } catch {
      resolve(null);
    }
  });
  return webDb;
}

function requestResult<T>(request: IDBRequest<T>): Promise<T> {
  return new Promise((resolve, reject) => {
    request.onsuccess = () => resolve(request.result);
    request.onerror = () =>
      reject(request.error ?? new Error(i18n.t("shell:secureStorage.requestFailed")));
  });
}

function transactionDone(transaction: IDBTransaction): Promise<void> {
  return new Promise((resolve, reject) => {
    transaction.oncomplete = () => resolve();
    transaction.onerror = () =>
      reject(transaction.error ?? new Error(i18n.t("shell:secureStorage.writeFailed")));
    transaction.onabort = () =>
      reject(transaction.error ?? new Error(i18n.t("shell:secureStorage.writeAborted")));
  });
}

async function wrappingKey(database: IDBDatabase): Promise<CryptoKey | null> {
  const cryptoApi = browserCrypto();
  if (!cryptoApi?.subtle) return null;
  webKey ??= (async () => {
    try {
      const transaction = database.transaction(META_STORE, "readonly");
      const existing = await requestResult<WrapRecord | undefined>(
        transaction.objectStore(META_STORE).get(WRAP_KEY_ID),
      );
      if (existing?.key) return existing.key;
    } catch {
      // Fall through and create a fresh key.
    }

    try {
      const key = await cryptoApi.subtle.generateKey({ name: "AES-GCM", length: 256 }, false, [
        "encrypt",
        "decrypt",
      ]);
      const transaction = database.transaction(META_STORE, "readwrite");
      transaction.objectStore(META_STORE).put({ id: WRAP_KEY_ID, key } satisfies WrapRecord);
      await transactionDone(transaction);
      return key;
    } catch {
      return null;
    }
  })();
  return webKey;
}

async function loadWebSecrets(): Promise<void> {
  const database = await openWebDatabase();
  if (!database) return;
  const key = await wrappingKey(database);
  if (!key) return;
  try {
    const transaction = database.transaction(RECORD_STORE, "readonly");
    const records = await requestResult<StoredRecord[]>(
      transaction.objectStore(RECORD_STORE).getAll(),
    );
    for (const record of records) {
      try {
        const plaintext = await browserCrypto()?.subtle?.decrypt(
          {
            name: "AES-GCM",
            iv: record.iv as BufferSource,
            additionalData: new TextEncoder().encode(record.key),
          },
          key,
          record.data,
        );
        if (plaintext) {
          memory.set(record.key, new TextDecoder().decode(plaintext));
        }
      } catch {
        // A record from another key (for example after a profile reset) is
        // skipped rather than failing the whole store.
      }
    }
  } catch {
    // An unavailable database degrades to memory for this page load.
  }
}

async function readWebSecret(key: string): Promise<string | null> {
  return memory.get(key) ?? null;
}

async function writeWebSecret(key: string, value: string): Promise<void> {
  const database = await openWebDatabase();
  const cryptoApi = browserCrypto();
  if (!database || !cryptoApi?.subtle) return;
  const wrapping = await wrappingKey(database);
  if (!wrapping) return;
  const iv = cryptoApi.getRandomValues(new Uint8Array(12));
  const data = await cryptoApi.subtle.encrypt(
    { name: "AES-GCM", iv, additionalData: new TextEncoder().encode(key) },
    wrapping,
    new TextEncoder().encode(value),
  );
  const transaction = database.transaction(RECORD_STORE, "readwrite");
  transaction.objectStore(RECORD_STORE).put({ key, iv, data } satisfies StoredRecord);
  await transactionDone(transaction);
}

async function deleteWebSecret(key: string): Promise<void> {
  const database = await openWebDatabase();
  if (!database) return;
  const transaction = database.transaction(RECORD_STORE, "readwrite");
  transaction.objectStore(RECORD_STORE).delete(key);
  await transactionDone(transaction);
}
