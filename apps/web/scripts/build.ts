/**
 * Static build for the browser target.
 *
 * The web app is the same React bundle as the Tauri shells; it only differs in
 * how it reaches the daemon. Vite owns the build (`apps/ui`), and this script
 * publishes its output under `apps/web/dist` so a static host can serve one
 * directory without reaching across the workspace.
 */
import { cp, mkdir, rm, stat } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const uiDist = resolve(here, "../../ui/dist");
const webDist = resolve(here, "../dist");

async function exists(path: string): Promise<boolean> {
  try {
    await stat(path);
    return true;
  } catch {
    return false;
  }
}

if (!(await exists(uiDist))) {
  console.error(`apps/ui/dist is missing. Run "bun run --cwd apps/ui build" first.`);
  process.exit(1);
}

await rm(webDist, { recursive: true, force: true });
await mkdir(webDist, { recursive: true });
await cp(uiDist, webDist, { recursive: true });

console.log(`Copied ${uiDist} -> ${webDist}`);
