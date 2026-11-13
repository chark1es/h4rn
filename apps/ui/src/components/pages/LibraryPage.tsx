import { McpPage } from "@/features/mcp/McpPage";
import { SkillsPage } from "@/features/skills/SkillsPage";

/**
 * Settings → Tools.
 *
 * MCP servers and Skills are separate feature surfaces with their own stores;
 * this page only chooses between them from the route.
 */
export function LibraryPage({ path }: { path?: string }) {
  const skillsPath = path?.replace(/\/+$/, "").endsWith("/skills") ?? false;
  return skillsPath ? <SkillsPage /> : <McpPage />;
}
