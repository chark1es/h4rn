/**
 * The machine-wide Labs switch for "Rewind files with Edit from here".
 *
 * The daemon owns the setting and enforces it: while it is off a file restore
 * is refused, the thread summary reports it unavailable, and the dialog offers
 * only the conversation rewind. This store mirrors the daemon's answer for
 * Settings → Labs.
 */

import { useSyncExternalStore } from "react";

import type { ProtoError } from "@/gen";
import { errorWords, toProtoError } from "@/lib/client/errors";
import { i18n } from "@/lib/i18n";
import { getClient, pushToast } from "@/lib/store/appStore";

export type RewindSettingsState = {
  /** Whether file restore is on for this machine; `null` until loaded. */
  filesEnabled: boolean | null;
  loading: boolean;
  saving: boolean;
  /**
   * The last failure, kept as the error itself so a client-written kind is
   * worded again in the active language where it is shown.
   */
  error: ProtoError | null;
};

const initialState: RewindSettingsState = {
  filesEnabled: null,
  loading: false,
  saving: false,
  error: null,
};

let state: RewindSettingsState = initialState;
const listeners = new Set<() => void>();

function update(patch: Partial<RewindSettingsState>): void {
  state = { ...state, ...patch };
  for (const listener of listeners) listener();
}

export function getRewindSettingsState(): RewindSettingsState {
  return state;
}

export function subscribeRewindSettings(listener: () => void): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

/** Re-renders the caller when the switch changes. */
export function useRewindSettings(): RewindSettingsState {
  return useSyncExternalStore(
    subscribeRewindSettings,
    getRewindSettingsState,
    getRewindSettingsState,
  );
}

/** Clears the store between tests. */
export function resetRewindSettingsForTests(): void {
  state = initialState;
  listeners.clear();
}

/** Reads the machine-wide switch once per mount. */
export async function loadRewindSettings(): Promise<void> {
  if (state.loading) return;
  update({ loading: true });
  try {
    const response = await getClient().request({ type: "get_rewind_settings" });
    if (response.type === "rewind_settings") {
      update({
        loading: false,
        filesEnabled: response.settings.files_enabled,
        error: null,
      });
    } else {
      update({ loading: false });
    }
  } catch (error) {
    update({ loading: false, error: toProtoError(error) });
  }
}

/** Turns file restore on or off for this machine. */
export async function setRewindFilesEnabled(enabled: boolean): Promise<boolean> {
  update({ saving: true, error: null });
  try {
    const response = await getClient().request({
      type: "set_rewind_settings_enabled",
      enabled,
    });
    if (response.type === "rewind_settings") {
      update({
        saving: false,
        filesEnabled: response.settings.files_enabled,
      });
      return response.settings.files_enabled === enabled;
    }
    update({ saving: false });
    return false;
  } catch (error) {
    const proto = toProtoError(error);
    update({ saving: false, error: proto });
    pushToast({
      variant: "error",
      title: i18n.t("settings:labs.rewindFilesFailed"),
      description: errorWords(proto).message,
    });
    return false;
  }
}
