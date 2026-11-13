import type { HarnessKind, HostId, ThreadId } from "@/gen";

/**
 * Blobatar seed scheme (DESIGN.md):
 * type-prefixed, stable ids only, lowercase from the daemon.
 */
export function machineSeed(hostId: HostId): string {
  return `machine:${hostId}`;
}

export function harnessSeed(hostId: HostId, harness: HarnessKind, instanceId?: string): string {
  return instanceId ? `harness:${hostId}:${harness}:${instanceId}` : `harness:${hostId}:${harness}`;
}

export function subagentSeed(hostId: HostId, parentThreadId: ThreadId, subagentId: string): string {
  return `subagent:${hostId}:${parentThreadId}:${subagentId}`;
}

export function threadSeed(threadId: ThreadId): string {
  return `thread:${threadId}`;
}

export function userSeed(userId: string): string {
  return `user:${userId}`;
}

export const HARNESS_LABELS: Record<HarnessKind, string> = {
  claude_code: "Claude Code",
  codex: "Codex",
  opencode: "OpenCode",
  antigravity: "Antigravity",
};

export const HARNESS_SLUGS: Record<HarnessKind, string> = {
  claude_code: "claude-code",
  codex: "codex",
  opencode: "opencode",
  antigravity: "antigravity",
};

export function harnessLabel(harness: HarnessKind): string {
  return HARNESS_LABELS[harness];
}

export function harnessSlug(harness: HarnessKind): string {
  return HARNESS_SLUGS[harness];
}

export function hostPlatformLabel(platform: string): string {
  if (platform.startsWith("wsl:")) return platform;
  if (platform === "macos") return "macOS";
  if (platform === "linux") return "Linux";
  if (platform === "windows") return "Windows";
  return platform;
}
