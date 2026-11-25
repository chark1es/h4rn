/**
 * The machine's phone-alert publishing switch (ADR-049).
 *
 * The daemon owns this setting because the daemon publishes. Off is the
 * default; the UI mirrors the daemon's answer rather than deciding locally.
 */

import { useSyncExternalStore } from "react";

import { toProtoError } from "@/lib/client/errors";
import { getClient, pushToast } from "@/lib/store/appStore";

export type AgentAlertState = {
  enabled: boolean | null;
  loading: boolean;
  saving: boolean;
  error: string | null;
};

const initialState: AgentAlertState = {
  enabled: null,
  loading: false,
  saving: false,
  error: null,
};

let state: AgentAlertState = initialState;
const listeners = new Set<() => void>();

function update(patch: Partial<AgentAlertState>): void {
  state = { ...state, ...patch };
  for (const listener of listeners) listener();
}

function describe(error: unknown): string {
  return toProtoError(error).message;
}

export function getAgentAlertState(): AgentAlertState {
  return state;
}

export function useAgentAlerts(): AgentAlertState {
  return useSyncExternalStore(
    (listener) => {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
    getAgentAlertState,
    getAgentAlertState,
  );
}

export function resetAgentAlertStoreForTests(): void {
  state = initialState;
  listeners.clear();
}

/** Reads the machine's switch once per mount. */
export async function loadAgentAlertSettings(): Promise<void> {
  if (state.loading) return;
  update({ loading: true });
  try {
    const response = await getClient().request({ type: "get_agent_alert_settings" });
    if (response.type === "agent_alert_settings") {
      update({ loading: false, enabled: response.settings.enabled });
    } else {
      update({ loading: false });
    }
  } catch (error) {
    update({ loading: false, error: describe(error) });
  }
}

/** Saves the switch and mirrors the daemon's answer. */
export async function setAgentAlertsEnabled(enabled: boolean): Promise<boolean> {
  update({ saving: true, error: null });
  try {
    const response = await getClient().request({
      type: "set_agent_alert_settings",
      enabled,
    });
    if (response.type === "agent_alert_settings") {
      update({ saving: false, enabled: response.settings.enabled });
      return response.settings.enabled === enabled;
    }
    update({ saving: false });
    return false;
  } catch (error) {
    update({ saving: false, error: describe(error) });
    pushToast({
      variant: "error",
      title: "Agent alerts could not be changed",
      description: describe(error),
    });
    return false;
  }
}
