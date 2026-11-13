import { H4rnClient } from "./client";
import { createMockTransport } from "./mock";
import type { ClientOptions, TransportFactory } from "./types";

export const DEFAULT_WS_URL = "ws://127.0.0.1:47800/ws";
const TOKEN_KEY = "h4rn.token";

export type BootConfig = {
  mock: boolean;
  scenario: string;
  token: string | null;
  theme: "light" | "dark" | "system" | null;
};

export function readBootConfig(): BootConfig {
  if (typeof window === "undefined") {
    return { mock: false, scenario: "default", token: null, theme: null };
  }

  const params = new URLSearchParams(window.location.search);
  const queryTheme = params.get("theme");
  return {
    mock: params.get("mock") === "1",
    scenario: params.get("scenario") ?? "default",
    token: params.get("token") ?? readStoredToken(),
    theme:
      queryTheme === "light" || queryTheme === "dark" || queryTheme === "system"
        ? queryTheme
        : null,
  };
}

function readStoredToken(): string | null {
  try {
    return window.localStorage.getItem(TOKEN_KEY);
  } catch {
    return null;
  }
}

export function createClientFromEnv(): H4rnClient {
  const config = readBootConfig();

  if (config.mock) {
    const transportFactory: TransportFactory = () => createMockTransport(config.scenario);
    const options: ClientOptions = {
      url: DEFAULT_WS_URL,
      token: "mock",
      transportLabel: "mock",
      transportFactory,
      requestTimeoutMs: 10_000,
      reconnect: { baseMs: 300, maxMs: 4_000, jitter: 0.2, maxAttempts: 8 },
    };
    return new H4rnClient(options);
  }

  return new H4rnClient({
    url: DEFAULT_WS_URL,
    token: config.token,
    transportLabel: "direct",
  });
}
