export type Route =
  | { kind: "inbox" }
  | { kind: "thread"; threadId: string }
  | { kind: "hosts" }
  | { kind: "placeholder"; surface: string; path: string };

const PLACEHOLDER_TITLES: Record<string, string> = {
  library: "Library",
  usage: "Usage & limits",
  ide: "IDE",
  pulls: "Pull requests",
  settings: "Settings",
};

export function parseRoute(pathname: string): Route {
  const path = pathname.replace(/\/+$/, "") || "/";
  if (path === "/") return { kind: "inbox" };
  if (path === "/hosts") return { kind: "hosts" };
  const thread = /^\/t\/([^/]+)$/.exec(path);
  if (thread?.[1]) return { kind: "thread", threadId: decodeURIComponent(thread[1]) };
  const key = path.slice(1).split("/")[0] ?? "";
  return { kind: "placeholder", surface: PLACEHOLDER_TITLES[key] ?? "h4rn", path };
}

export function routePath(route: Route): string {
  switch (route.kind) {
    case "inbox":
      return "/";
    case "thread":
      return `/t/${encodeURIComponent(route.threadId)}`;
    case "hosts":
      return "/hosts";
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
  window.history.pushState({}, "", path);
}

/** Rewrite the current entry, e.g. when a temp thread id resolves to the real one. */
export function replaceRoute(route: Route): void {
  if (typeof window === "undefined") return;
  const path = routePath(route);
  if (window.location.pathname === path) return;
  window.history.replaceState({}, "", path);
}
