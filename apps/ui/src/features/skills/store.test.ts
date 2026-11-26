import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { ResponseBody, SkillConflict, SkillInventory, SkillSource, SkillSummary } from "@/gen";
import { H4rnRequestError, protoError } from "@/lib/client/errors";
import { getState, store } from "@/lib/store/appStore";
import {
  addSkill,
  applySkillEvent,
  getSkillsState,
  loadSkills,
  removeSkill,
  resetSkillsStore,
  resolveSkillConflict,
  setSkillEnabled,
  setSkillsRequesterForTests,
} from "./store";

const source: SkillSource = { type: "local_folder", path: "/home/atlas/skills/review" };

function skill(patch: Partial<SkillSummary> = {}): SkillSummary {
  return {
    id: "skl_1",
    name: "review",
    description: "Review a diff before it lands.",
    version: "abc123",
    source,
    total_bytes: 4096,
    updated_at: "2026-11-13T10:00:00.000Z",
    installs: [
      {
        harness: "claude_code",
        installed: true,
        managed: true,
        enabled: true,
        version: "abc123",
        path: "/home/atlas/.claude/skills/review",
      },
      {
        harness: "codex",
        installed: true,
        managed: true,
        enabled: true,
        version: "abc123",
        path: "/home/atlas/.codex/skills/review",
      },
    ],
    conflicts: 0,
    ...patch,
  };
}

function inventory(skills: SkillSummary[] = [skill()]): SkillInventory {
  return {
    skills,
    harnesses: [
      {
        harness: "claude_code",
        detected: true,
        user_root: "/home/atlas/.claude/skills",
        project_root: ".claude/skills",
      },
      {
        harness: "codex",
        detected: true,
        user_root: "/home/atlas/.codex/skills",
        project_root: ".codex/skills",
      },
    ],
  };
}

const conflict: SkillConflict = {
  id: "cnf_1",
  skill_id: "skl_1",
  name: "review",
  local_version: "abc123",
  remote_version: "def456",
  reason: "The shared copy changed while this skill was edited here.",
  detected_at: "2026-11-13T10:05:00.000Z",
};

function ok(response: ResponseBody): Promise<ResponseBody> {
  return Promise.resolve(response);
}

beforeEach(() => {
  resetSkillsStore();
  store.setState({ ...getState(), localHostId: "atlas" });
});

afterEach(() => {
  setSkillsRequesterForTests(null);
  vi.restoreAllMocks();
});

describe("Skills store", () => {
  it("loads the inventory and its conflicts together", async () => {
    setSkillsRequesterForTests((request) =>
      request.type === "skills_list"
        ? ok({ type: "skills", inventory: inventory() })
        : ok({ type: "skill_conflicts", conflicts: [conflict] }),
    );

    await loadSkills();

    expect(getSkillsState().inventory?.skills.map((entry) => entry.name)).toEqual(["review"]);
    expect(getSkillsState().conflicts).toHaveLength(1);
    expect(getSkillsState().checkedAt).not.toBeNull();
  });

  it("refuses to load before the daemon reports its host", async () => {
    store.setState({ ...getState(), localHostId: null });
    setSkillsRequesterForTests(() => ok({ type: "skills", inventory: inventory() }));

    await loadSkills();

    expect(getSkillsState().inventory).toBeNull();
    expect(getSkillsState().error?.message).toBe("The daemon has not reported its host yet.");
    expect(getSkillsState().errorAction).toBeNull();
  });

  it("keeps a failed list's error as sent and names the request as a kind", async () => {
    setSkillsRequesterForTests((request) =>
      request.type === "skills_list"
        ? Promise.reject(new H4rnRequestError(protoError("internal", "registry is locked", null)))
        : ok({ type: "skill_conflicts", conflicts: [] }),
    );

    await loadSkills();

    expect(getSkillsState().error).toEqual({
      code: "internal",
      message: "registry is locked",
      hint: null,
    });
    expect(getSkillsState().errorAction).toBe("listSkills");

    setSkillsRequesterForTests((request) =>
      request.type === "skills_list"
        ? ok({ type: "skills", inventory: inventory() })
        : ok({ type: "skill_conflicts", conflicts: [] }),
    );
    await loadSkills({ force: true });
    expect(getSkillsState().error).toBeNull();
    expect(getSkillsState().errorAction).toBeNull();
  });

  it("flips a harness install immediately and rolls it back on rejection", async () => {
    setSkillsRequesterForTests((request) =>
      request.type === "skills_list"
        ? ok({ type: "skills", inventory: inventory() })
        : ok({ type: "skill_conflicts", conflicts: [] }),
    );
    await loadSkills();

    const gate: { release?: (value: ResponseBody) => void } = {};
    setSkillsRequesterForTests(
      () =>
        new Promise((resolve) => {
          gate.release = resolve;
        }),
    );

    const pending = setSkillEnabled("skl_1", "codex", false);
    const optimistic = getSkillsState().inventory?.skills[0]?.installs.find(
      (entry) => entry.harness === "codex",
    );
    expect(optimistic?.enabled).toBe(false);
    expect(getSkillsState().pending["skl_1:codex"]).toBe(true);

    gate.release?.({ type: "skill_added", skill: skill() });
    await pending;
    expect(
      getSkillsState().inventory?.skills[0]?.installs.find((entry) => entry.harness === "codex")
        ?.enabled,
    ).toBe(true);
  });

  it("restores the row and says the switch moved back", async () => {
    setSkillsRequesterForTests((request) =>
      request.type === "skills_list"
        ? ok({ type: "skills", inventory: inventory() })
        : ok({ type: "skill_conflicts", conflicts: [] }),
    );
    await loadSkills();
    setSkillsRequesterForTests(() =>
      Promise.reject(new H4rnRequestError(protoError("forbidden", "Read-only folder.", null))),
    );

    const result = await setSkillEnabled("skl_1", "codex", false);

    expect(result).toBe(false);
    expect(
      getSkillsState().inventory?.skills[0]?.installs.find((entry) => entry.harness === "codex")
        ?.enabled,
    ).toBe(true);
    const toast = getState().toasts.at(-1);
    expect(toast?.title).toBe("Couldn't disable review for Codex.");
    expect(toast?.description).toContain("The switch is back where it was.");
  });

  it("surfaces a secret-scan refusal without adding a row", async () => {
    setSkillsRequesterForTests((request) =>
      request.type === "skills_list"
        ? ok({ type: "skills", inventory: inventory([]) })
        : ok({ type: "skill_conflicts", conflicts: [] }),
    );
    await loadSkills();
    setSkillsRequesterForTests(() =>
      Promise.reject(
        new H4rnRequestError(
          protoError(
            "invalid_request",
            "This skill contains what looks like a credential.",
            "Remove the secret from every file and add the skill again. h4rn will not install it.",
          ),
        ),
      ),
    );

    const added = await addSkill({ source, harnesses: ["claude_code"] });

    expect(added).toBeNull();
    expect(getSkillsState().inventory?.skills).toHaveLength(0);
    const toast = getState().toasts.at(-1);
    expect(toast?.title).toBe("Couldn't add that skill.");
    expect(toast?.description).toContain("h4rn will not install it");
  });

  it("restores the inventory when removal is refused", async () => {
    setSkillsRequesterForTests((request) =>
      request.type === "skills_list"
        ? ok({ type: "skills", inventory: inventory() })
        : ok({ type: "skill_conflicts", conflicts: [] }),
    );
    await loadSkills();
    setSkillsRequesterForTests(() =>
      Promise.reject(new H4rnRequestError(protoError("conflict", "A harness is using it.", null))),
    );

    const removed = await removeSkill("skl_1");

    expect(removed).toBe(false);
    expect(getSkillsState().inventory?.skills.map((entry) => entry.name)).toEqual(["review"]);
    expect(getState().toasts.at(-1)?.description).toContain("still in the registry");
  });

  it("names the skill whose conflict could not be resolved", async () => {
    setSkillsRequesterForTests((request) =>
      request.type === "skills_list"
        ? ok({ type: "skills", inventory: inventory() })
        : ok({ type: "skill_conflicts", conflicts: [conflict] }),
    );
    await loadSkills();
    setSkillsRequesterForTests(() =>
      Promise.reject(new H4rnRequestError(protoError("conflict", "Changed again.", null))),
    );

    expect(await resolveSkillConflict("cnf_1", "keep_local")).toBe(false);
    expect(getState().toasts.at(-1)?.title).toBe("Couldn't resolve the conflict for review.");
  });

  it("records a sync status event as-is", () => {
    applySkillEvent({
      type: "sync_status",
      state: "offline",
      last_sync: null,
      pushed: 0,
      pulled: 0,
      conflicts: 0,
      etag: null,
      message: "The hub is unreachable.",
      hint: "Skills keep working locally; h4rn will retry.",
    });

    expect(getSkillsState().sync?.state).toBe("offline");
    expect(getSkillsState().sync?.hint).toContain("retry");
  });
});
