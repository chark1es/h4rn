import { useEffect, useSyncExternalStore } from "react";
import type {
  HarnessKind,
  ProtoError,
  ProxyHarnessOverride,
  ProxyLimits,
  ProxyModel,
  ProxyModelCatalog,
  ProxyProfile,
  ProxyProfileDraft,
  ProxyProfileId,
  ProxyProfilePatch,
  ProxyTestReport,
} from "@/gen";
import { toProtoError } from "@/lib/client/errors";
import { onProxyFrame } from "@/lib/client/frames";
import { getClient, useApp } from "@/lib/store/appStore";

export type ProxyState = {
  profiles: ProxyProfile[];
  /** A profile list has been read, even when the daemon reported none. */
  loaded: boolean;
  loading: boolean;
  error: ProtoError | null;
  selectedId: ProxyProfileId | null;
  catalogs: Record<ProxyProfileId, ProxyModelCatalog>;
  catalogLoading: boolean;
  catalogError: ProtoError | null;
  reports: Record<ProxyProfileId, ProxyTestReport>;
  testing: Record<ProxyProfileId, boolean>;
  limits: Record<ProxyProfileId, ProxyLimits>;
  limitsLoading: boolean;
  saveError: ProtoError | null;
  overridesSaving: Record<string, boolean>;
};

const initialState: ProxyState = {
  profiles: [],
  loaded: false,
  loading: false,
  error: null,
  selectedId: null,
  catalogs: {},
  catalogLoading: false,
  catalogError: null,
  reports: {},
  testing: {},
  limits: {},
  limitsLoading: false,
  saveError: null,
  overridesSaving: {},
};

type Listener = () => void;

let state = initialState;
const listeners = new Set<Listener>();

function setState(next: ProxyState): void {
  state = next;
  for (const listener of listeners) listener();
}

function patch(next: Partial<ProxyState>): void {
  setState({ ...state, ...next });
}

export function getProxyState(): ProxyState {
  return state;
}

export function useProxy<T>(selector: (value: ProxyState) => T): T {
  return useSyncExternalStore(
    (listener) => {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
    () => selector(state),
    () => selector(state),
  );
}

/** Profiles first, then the chosen profile's catalogue and limits. */
export async function loadProxyProfiles(): Promise<void> {
  patch({ loading: true, error: null });
  try {
    const response = await getClient().request({ type: "list_proxy_profiles" });
    if (response.type !== "proxy_profiles") return;
    const selected =
      state.selectedId && response.profiles.some((profile) => profile.id === state.selectedId)
        ? state.selectedId
        : (response.profiles.find((profile) => profile.default)?.id ??
          response.profiles[0]?.id ??
          null);
    setState({
      ...state,
      profiles: response.profiles,
      loaded: true,
      loading: false,
      error: null,
      selectedId: selected,
    });
    if (selected) {
      void loadProxyModels(selected);
      void loadProxyLimits(selected);
    }
  } catch (error) {
    // Mark the attempt so the composer's lazy hook does not hammer a daemon
    // that answered with an error; Settings offers an explicit retry.
    patch({ loaded: true, loading: false, error: toProtoError(error) });
  }
}

export function selectProxyProfile(profileId: ProxyProfileId): void {
  patch({ selectedId: profileId, catalogError: null });
  void loadProxyModels(profileId);
  void loadProxyLimits(profileId);
}

export async function loadProxyModels(profileId: ProxyProfileId): Promise<void> {
  patch({ catalogLoading: true, catalogError: null });
  try {
    const response = await getClient().request({
      type: "list_proxy_models",
      profile_id: profileId,
    });
    if (response.type !== "proxy_models") return;
    patch({
      catalogs: { ...state.catalogs, [profileId]: response.catalog },
      catalogLoading: false,
      catalogError: null,
    });
  } catch (error) {
    patch({ catalogLoading: false, catalogError: toProtoError(error) });
  }
}

export async function loadProxyLimits(profileId: ProxyProfileId): Promise<void> {
  patch({ limitsLoading: true });
  try {
    const response = await getClient().request({
      type: "list_proxy_limits",
      profile_id: profileId,
    });
    if (response.type !== "proxy_limits") return;
    patch({ limits: { ...state.limits, [profileId]: response.limits }, limitsLoading: false });
  } catch {
    patch({ limitsLoading: false });
  }
}

export async function addProxyProfile(draft: ProxyProfileDraft): Promise<ProxyProfile | null> {
  patch({ saveError: null });
  try {
    const response = await getClient().request({ type: "add_proxy_profile", profile: draft });
    if (response.type !== "proxy_profile") return null;
    const profiles = [...state.profiles, response.profile].sort(
      (left, right) =>
        Number(right.default) - Number(left.default) || left.name.localeCompare(right.name),
    );
    patch({ profiles, selectedId: response.profile.id });
    void loadProxyModels(response.profile.id);
    return response.profile;
  } catch (error) {
    patch({ saveError: toProtoError(error) });
    return null;
  }
}

export async function updateProxyProfile(
  profileId: ProxyProfileId,
  values: ProxyProfilePatch,
): Promise<boolean> {
  patch({ saveError: null });
  try {
    const response = await getClient().request({
      type: "update_proxy_profile",
      profile_id: profileId,
      patch: values,
    });
    if (response.type !== "proxy_profile") return false;
    patch({
      profiles: state.profiles.map((profile) =>
        profile.id === profileId ? response.profile : profile,
      ),
    });
    return true;
  } catch (error) {
    patch({ saveError: toProtoError(error) });
    return false;
  }
}

export async function removeProxyProfile(profileId: ProxyProfileId): Promise<void> {
  try {
    await getClient().request({ type: "remove_proxy_profile", profile_id: profileId });
    const profiles = state.profiles.filter((profile) => profile.id !== profileId);
    patch({
      profiles,
      selectedId: state.selectedId === profileId ? (profiles[0]?.id ?? null) : state.selectedId,
    });
  } catch (error) {
    patch({ saveError: toProtoError(error) });
  }
}

export async function testProxyProfile(profileId: ProxyProfileId): Promise<void> {
  patch({ testing: { ...state.testing, [profileId]: true }, saveError: null });
  try {
    const response = await getClient().request({
      type: "test_proxy_profile",
      profile_id: profileId,
    });
    if (response.type !== "proxy_tested") return;
    patch({
      reports: { ...state.reports, [profileId]: response.report },
      profiles: state.profiles.map((profile) =>
        profile.id === profileId
          ? { ...profile, capabilities: response.report.capabilities }
          : profile,
      ),
    });
  } catch (error) {
    patch({ saveError: toProtoError(error) });
  } finally {
    patch({ testing: { ...state.testing, [profileId]: false } });
  }
}

/**
 * Optimistic harness toggle. The UI flips immediately, the daemon is the
 * source of truth, and a failure restores the previous row.
 */
export async function setHarnessOverride(
  profileId: ProxyProfileId,
  harness: HarnessKind,
  enabled: boolean,
  defaultModel: string | null,
): Promise<void> {
  const profile = state.profiles.find((candidate) => candidate.id === profileId);
  if (!profile) return;
  const key = `${profileId}:${harness}`;
  const previous = profile.harnesses;
  const existing = previous.find((override) => override.harness === harness);
  const optimistic: ProxyHarnessOverride = {
    harness,
    enabled,
    default_model: defaultModel,
    verified: existing?.verified ?? false,
    note: existing?.note ?? null,
  };
  patch({
    profiles: state.profiles.map((candidate) =>
      candidate.id === profileId
        ? {
            ...candidate,
            harnesses: existing
              ? candidate.harnesses.map((override) =>
                  override.harness === harness ? optimistic : override,
                )
              : [...candidate.harnesses, optimistic],
          }
        : candidate,
    ),
    overridesSaving: { ...state.overridesSaving, [key]: true },
    saveError: null,
  });
  try {
    const response = await getClient().request({
      type: "set_proxy_harness_override",
      profile_id: profileId,
      harness,
      enabled,
      default_model: defaultModel,
    });
    if (response.type === "proxy_profile") {
      patch({
        profiles: state.profiles.map((candidate) =>
          candidate.id === profileId ? response.profile : candidate,
        ),
      });
    }
  } catch (error) {
    patch({
      profiles: state.profiles.map((candidate) =>
        candidate.id === profileId ? { ...candidate, harnesses: previous } : candidate,
      ),
      saveError: toProtoError(error),
    });
  } finally {
    patch({ overridesSaving: { ...state.overridesSaving, [key]: false } });
  }
}

export function proxyOverrideFor(harness: HarnessKind): {
  profile: ProxyProfile;
  override: ProxyHarnessOverride;
} | null {
  for (const profile of state.profiles) {
    const override = profile.harnesses.find(
      (candidate) => candidate.harness === harness && candidate.enabled,
    );
    if (override) return { profile, override };
  }
  return null;
}

/** Models offered by the enabled profile for a harness, plus its default. */
export function proxyModelsFor(harness: HarnessKind): ProxyModel[] {
  const active = proxyOverrideFor(harness);
  if (!active) return [];
  return state.catalogs[active.profile.id]?.models ?? [];
}

/** The active override's default model, else the first catalogue entry. */
export function proxyDefaultModel(harness: HarnessKind): string | null {
  const active = proxyOverrideFor(harness);
  if (!active) return null;
  return active.override.default_model ?? state.catalogs[active.profile.id]?.models[0]?.id ?? null;
}

/**
 * Loads profiles lazily once the connection is online. Both the settings page
 * and the composer's model picker call this; the store deduplicates.
 */
export function useProxyCatalog(): void {
  const status = useApp((view) => view.connection.status);
  const loading = useProxy((value) => value.loading);
  const loaded = useProxy((value) => value.loaded);
  useEffect(() => {
    if (status !== "online" || loading || loaded) return;
    void loadProxyProfiles();
  }, [status, loading, loaded]);
}

let watching = false;

export function startProxyWatch(): void {
  if (watching) return;
  watching = true;
  onProxyFrame((event) => {
    if (event.type === "profiles_changed") {
      setState({ ...state, profiles: event.profiles });
      return;
    }
    if (event.type === "profile_tested") {
      setState({ ...state, reports: { ...state.reports, [event.profile_id]: event.report } });
      return;
    }
    if (event.type === "models_changed") {
      setState({
        ...state,
        catalogs: { ...state.catalogs, [event.profile_id]: event.catalog },
      });
      return;
    }
    setState({ ...state, limits: { ...state.limits, [event.profile_id]: event.limits } });
  });
}

export function resetProxyStoreForTests(overrides: Partial<ProxyState> = {}): void {
  state = { ...initialState, ...overrides };
  for (const listener of listeners) listener();
}
