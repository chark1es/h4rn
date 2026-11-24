/**
 * Guest links on the owner's side (ADR-044).
 *
 * The daemon owns the token, the 24-hour expiry, the browser binding and the
 * waiting messages. This store only mirrors those answers so the share dialog,
 * the Request card and Settings → Labs show the truth.
 */

import { useSyncExternalStore } from "react";

import type { GuestLink, GuestSharing, ThreadId } from "@/gen";
import { toProtoError } from "@/lib/client/errors";
import { getClient, pushToast } from "@/lib/store/appStore";

export type GuestState = {
  /** Whether Settings → Labs → Guest links is on; `null` until loaded. */
  settingsEnabled: boolean | null;
  settingsLoading: boolean;
  settingsSaving: boolean;
  /** Per-thread sharing answers, keyed by thread id. */
  sharing: Record<string, GuestSharing>;
  sharingLoading: Record<string, boolean>;
  /** The thread whose share dialog is open. */
  dialogThreadId: ThreadId | null;
  creating: Record<string, boolean>;
  stopping: Record<string, boolean>;
  /** Request ids whose Send or Dismiss is in flight. */
  answering: Record<string, boolean>;
  error: string | null;
};

const initialState: GuestState = {
  settingsEnabled: null,
  settingsLoading: false,
  settingsSaving: false,
  sharing: {},
  sharingLoading: {},
  dialogThreadId: null,
  creating: {},
  stopping: {},
  answering: {},
  error: null,
};

let state: GuestState = initialState;
const listeners = new Set<() => void>();

function update(patch: Partial<GuestState>): void {
  state = { ...state, ...patch };
  for (const listener of listeners) listener();
}

function describe(error: unknown): string {
  return toProtoError(error).message;
}

export function getGuestState(): GuestState {
  return state;
}

export function subscribeGuest(listener: () => void): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

/** Re-renders the caller when any part of the guest state changes. */
export function useGuest(): GuestState {
  return useSyncExternalStore(subscribeGuest, getGuestState, getGuestState);
}

/** Clears the store between tests. */
export function resetGuestStoreForTests(): void {
  state = initialState;
  listeners.clear();
}

/** Reads the machine-wide setting once per mount. */
export async function loadGuestSettings(): Promise<void> {
  if (state.settingsLoading) return;
  update({ settingsLoading: true });
  try {
    const response = await getClient().request({ type: "get_guest_settings" });
    if (response.type === "guest_settings") {
      update({ settingsLoading: false, settingsEnabled: response.settings.enabled });
    } else {
      update({ settingsLoading: false });
    }
  } catch (error) {
    update({ settingsLoading: false, error: describe(error) });
  }
}

/** Turns guest links on or off for this machine. */
export async function setGuestLinksEnabled(enabled: boolean): Promise<boolean> {
  update({ settingsSaving: true, error: null });
  try {
    const response = await getClient().request({ type: "set_guest_settings", enabled });
    if (response.type === "guest_settings") {
      update({ settingsSaving: false, settingsEnabled: response.settings.enabled });
      return response.settings.enabled === enabled;
    }
    update({ settingsSaving: false });
    return false;
  } catch (error) {
    update({ settingsSaving: false, error: describe(error) });
    pushToast({
      variant: "error",
      title: "Guest links could not be changed",
      description: describe(error),
    });
    return false;
  }
}

/** Reads whether one thread already has a link. */
export async function loadGuestSharing(threadId: ThreadId): Promise<void> {
  if (state.sharingLoading[threadId]) return;
  update({ sharingLoading: { ...state.sharingLoading, [threadId]: true } });
  try {
    const response = await getClient().request({
      type: "get_guest_sharing",
      thread_id: threadId,
    });
    if (response.type === "guest_sharing") {
      update({
        sharingLoading: { ...state.sharingLoading, [threadId]: false },
        sharing: { ...state.sharing, [threadId]: response.sharing },
      });
    } else {
      update({ sharingLoading: { ...state.sharingLoading, [threadId]: false } });
    }
  } catch (error) {
    update({
      sharingLoading: { ...state.sharingLoading, [threadId]: false },
      error: describe(error),
    });
  }
}

/** Opens the share dialog for one thread and reads its current link. */
export function openGuestShare(threadId: ThreadId): void {
  update({ dialogThreadId: threadId, error: null });
  void loadGuestSettings();
  void loadGuestSharing(threadId);
}

export function closeGuestShare(): void {
  update({ dialogThreadId: null });
}

function applySharing(sharing: GuestSharing): void {
  update({
    sharing: { ...state.sharing, [sharing.thread_id]: sharing },
    creating: { ...state.creating, [sharing.thread_id]: false },
  });
}

/** Creates a new link, ending the thread's previous one. */
export async function shareThreadWithGuest(threadId: ThreadId): Promise<GuestLink | null> {
  update({ creating: { ...state.creating, [threadId]: true }, error: null });
  try {
    const response = await getClient().request({
      type: "share_thread_with_guest",
      thread_id: threadId,
    });
    if (response.type === "guest_sharing") {
      applySharing(response.sharing);
      return response.sharing.link;
    }
    update({ creating: { ...state.creating, [threadId]: false } });
    return null;
  } catch (error) {
    update({
      creating: { ...state.creating, [threadId]: false },
      error: describe(error),
    });
    return null;
  }
}

/** Ends the thread's link and deletes its waiting files. */
export async function stopGuestSharing(threadId: ThreadId): Promise<boolean> {
  update({ stopping: { ...state.stopping, [threadId]: true }, error: null });
  try {
    await getClient().request({ type: "stop_guest_sharing", thread_id: threadId });
    update({
      stopping: { ...state.stopping, [threadId]: false },
      sharing: {
        ...state.sharing,
        [threadId]: {
          thread_id: threadId,
          links_enabled: state.settingsEnabled ?? true,
          link: null,
        },
      },
    });
    return true;
  } catch (error) {
    update({
      stopping: { ...state.stopping, [threadId]: false },
      error: describe(error),
    });
    return false;
  }
}

/** Runs one waiting guest message as a normal turn. */
export async function sendGuestRequest(threadId: ThreadId, requestId: string): Promise<boolean> {
  update({ answering: { ...state.answering, [requestId]: true } });
  try {
    await getClient().request({
      type: "send_guest_request",
      thread_id: threadId,
      request_id: requestId,
    });
    update({ answering: { ...state.answering, [requestId]: false } });
    return true;
  } catch (error) {
    update({ answering: { ...state.answering, [requestId]: false } });
    pushToast({
      variant: "error",
      title: "That guest message could not be sent.",
      description: describe(error),
    });
    return false;
  }
}

/** Discards one waiting guest message and its files. */
export async function dismissGuestRequest(threadId: ThreadId, requestId: string): Promise<boolean> {
  update({ answering: { ...state.answering, [requestId]: true } });
  try {
    await getClient().request({
      type: "dismiss_guest_request",
      thread_id: threadId,
      request_id: requestId,
    });
    update({ answering: { ...state.answering, [requestId]: false } });
    return true;
  } catch (error) {
    update({ answering: { ...state.answering, [requestId]: false } });
    pushToast({
      variant: "error",
      title: "That guest message could not be dismissed.",
      description: describe(error),
    });
    return false;
  }
}

/** Copies the link when this daemon still holds its token. */
export async function copyGuestLink(link: GuestLink): Promise<boolean> {
  if (!link.url) return false;
  try {
    await navigator.clipboard.writeText(link.url);
    return true;
  } catch {
    return false;
  }
}
