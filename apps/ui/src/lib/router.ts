export type Route =
  | { kind: "inbox" }
  | { kind: "thread"; threadId: string }
  | { kind: "hosts" }
  | { kind: "ide" }
  | { kind: "placeholder"; surface: string; path: string };

/**
 * Surfaces that are not yet backed by daemon data. They render static,
 * mock-mode-friendly shells while the protocol catches up. The key is the
 * first path segment (or explicit settings section) and the value is the
 * surface name the shell switches on.
 */
const SURFACES: Record<string, string> = {
  library: "library",
  usage: "usage",
  pulls: "pulls",
  issues: "issues",
  settings: "settings",
  pairing: "pairing",
};

export function parseRoute(pathname: string): Route {
  const path = pathname.replace(/\/+$/, "") || "/";
  if (path === "/") return { kind: "inbox" };
  if (path === "/hosts") return { kind: "hosts" };
  if (path === "/ide") return { kind: "ide" };
  const thread = /^\/t\/([^/]+)$/.exec(path);
  if (thread?.[1]) return { kind: "thread", threadId: decodeURIComponent(thread[1]) };
  const key = path.slice(1).split("/")[0] ?? "";
  if (key === "settings") {
    const section = path.slice(1).split("/")[1] ?? "hosts";
    return { kind: "placeholder", surface: "settings", path: `/settings/${section}` };
  }
  return { kind: "placeholder", surface: SURFACES[key] ?? "h4rn", path };
}

export function routePath(route: Route): string {
  switch (route.kind) {
    case "inbox":
      return "/";
    case "thread":
      return `/t/${encodeURIComponent(route.threadId)}`;
    case "hosts":
      return "/hosts";
    case "ide":
      return "/ide";
    case "placeholder":
      return route.path;
  }
}

export function readRoute(): Route {
  if (typeof window === "undefined") return { kind: "inbox" };
  return parseRoute(window.location.pathname);
}

export function pushRoute(route: Route): void {
  if (typeof window === "undefined") return;
  const path = routePath(route);
  if (window.location.pathname === path) return;
  // Keep boot parameters such as `?token=` and `?daemon=` across navigation;
  // the desktop shell and the e2e harness both rely on them.
  window.history.pushState({}, "", `${path}${window.location.search}`);
}

/** Rewrite the current entry, e.g. when a temp thread id resolves to the real one. */
export function replaceRoute(route: Route): void {
  if (typeof window === "undefined") return;
  const path = routePath(route);
  if (window.location.pathname === path) return;
  window.history.replaceState({}, "", `${path}${window.location.search}`);
}
