import type { HarnessKind, HostId, ThreadId } from "@/gen";
import { i18n } from "@/lib/i18n";

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

/**
 * Each harness's name. Product names read the same in every language; the
 * generic ACP harness is worded, through a getter, so every reader of this
 * map gets the active language.
 */
export const HARNESS_LABELS: Record<HarnessKind, string> = {
  claude_code: "Claude Code",
  codex: "Codex",
  opencode: "OpenCode",
  antigravity: "Antigravity",
  get acp() {
    return i18n.t("common:acpAgent");
  },
};

export const HARNESS_SLUGS: Record<HarnessKind, string> = {
  claude_code: "claude-code",
  codex: "codex",
  opencode: "opencode",
  antigravity: "antigravity",
  acp: "acp",
};

export function harnessLabel(harness: HarnessKind): string {
  return HARNESS_LABELS[harness];
}

/**
 * A thread's harness in provenance lines. A configured ACP instance names the
 * instance id, because "ACP" alone would hide which command is running.
 */
export function threadHarnessLabel(harness: HarnessKind, agentId?: string | null): string {
  if (harness === "acp") return agentId ? `ACP · ${agentId}` : HARNESS_LABELS.acp;
  return harnessLabel(harness);
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
