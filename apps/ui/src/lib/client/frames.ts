import type { HarnessUpdateEvent, ProxyEvent, UsageStreamEvent } from "@/gen";

/**
 * Ephemeral server frames do not belong to the journal, so they bypass the
 * app store's ordered event queue. Feature stores subscribe here instead of
 * importing each other, which keeps `appStore` free of feature knowledge.
 */
type UsageListener = (event: UsageStreamEvent) => void;
type ProxyListener = (event: ProxyEvent) => void;

const usageListeners = new Set<UsageListener>();
const proxyListeners = new Set<ProxyListener>();

export function onUsageFrame(listener: UsageListener): () => void {
  usageListeners.add(listener);
  return () => {
    usageListeners.delete(listener);
  };
}

export function emitUsageFrame(event: UsageStreamEvent): void {
  for (const listener of usageListeners) listener(event);
}

export function onProxyFrame(listener: ProxyListener): () => void {
  proxyListeners.add(listener);
  return () => {
    proxyListeners.delete(listener);
  };
}

export function emitProxyFrame(event: ProxyEvent): void {
  for (const listener of proxyListeners) listener(event);
}

type HarnessListener = (event: HarnessUpdateEvent) => void;

const harnessListeners = new Set<HarnessListener>();

export function onHarnessFrame(listener: HarnessListener): () => void {
  harnessListeners.add(listener);
  return () => {
    harnessListeners.delete(listener);
  };
}

export function emitHarnessFrame(event: HarnessUpdateEvent): void {
  for (const listener of harnessListeners) listener(event);
}
