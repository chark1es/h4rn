// @vitest-environment jsdom

import { act, cleanup, render } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import type { Attachment } from "@/gen";
import { useAttachmentDrafts } from "./use-attachments";

vi.mock("@/lib/store/appStore", () => ({
  // Restored attachments are already committed, so no upload ever starts.
  getClient: () => ({ onBinary: () => () => undefined }),
}));

let latest: ReturnType<typeof useAttachmentDrafts> | undefined;

function Probe({ threadId }: { threadId: string }) {
  latest = useAttachmentDrafts(threadId, { enabled: true, disabledReason: null });
  return null;
}

function attachment(index: number): Attachment {
  return {
    id: `${index}`.padStart(64, "a"),
    name: `note-${index}.txt`,
    mime: "text/plain",
    size: 12,
    local_path: `/tmp/note-${index}.txt`,
  };
}

afterEach(() => {
  cleanup();
  latest = undefined;
});

describe("restoring a rewound prompt's attachments", () => {
  it("keeps every restored attachment and asks before sending past the limit", () => {
    render(<Probe threadId="thread_restore_limit" />);
    act(() => {
      latest?.restore(Array.from({ length: 10 }, (_, index) => attachment(index)));
    });
    expect(latest?.tasks).toHaveLength(10);
    expect(latest?.blockedReason).toBeNull();

    act(() => {
      latest?.restore([attachment(10)]);
    });
    expect(latest?.tasks).toHaveLength(11);
    expect(latest?.tasks.at(-1)?.restored?.name).toBe("note-10.txt");
    expect(latest?.blockedReason).toBe(
      "A message can carry 10 attachments. Remove 1 before sending.",
    );
  });
});
