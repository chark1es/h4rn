import { describe, expect, it } from "vitest";

import { createDraftStore, DEFAULT_NEW_THREAD_WORKSPACE, type DraftStorage } from "./drafts";
import { NewThreadDraftSession, type NewThreadDraftSubmission } from "./newThreadDraft";

function memoryStorage(): DraftStorage & { data: Map<string, string> } {
  const data = new Map<string, string>();
  return {
    data,
    getItem: (key) => data.get(key) ?? null,
    setItem: (key: string, value: string) => {
      data.set(key, value);
    },
    removeItem: (key: string) => {
      data.delete(key);
    },
    key: (index: number) => [...data.keys()][index] ?? null,
    get length() {
      return data.size;
    },
  };
}

function makeStore(storage: DraftStorage) {
  return createDraftStore(storage);
}

function submit(session: NewThreadDraftSession, folder: string): NewThreadDraftSubmission {
  const submission = session.submission(folder);
  if (!submission) throw new Error("expected a submission once a host is bound");
  return submission;
}

describe("NewThreadDraftSession", () => {
  it("loads the draft for the host once it resolves", () => {
    const store = makeStore(memoryStorage());
    store.writeNewThreadDraft("host_a", DEFAULT_NEW_THREAD_WORKSPACE, "saved prompt");
    store.flush();

    const session = new NewThreadDraftSession(store);
    expect(session.adoptHost("host_a")).toEqual({
      host: "host_a",
      workspace: DEFAULT_NEW_THREAD_WORKSPACE,
      text: "saved prompt",
    });
  });

  it("writes nothing and submits nothing until it has a host", () => {
    const storage = memoryStorage();
    const store = makeStore(storage);
    const session = new NewThreadDraftSession(store);

    session.setText("waiting");
    store.flush();

    expect(storage.data.size).toBe(0);
    expect(session.snapshot().text).toBe("waiting");
    expect(session.submission(DEFAULT_NEW_THREAD_WORKSPACE)).toBeNull();
  });

  it("commits a folder change: the box switches only when the folder is committed", () => {
    const store = makeStore(memoryStorage());
    store.writeNewThreadDraft("host_a", "~/code/api", "api work");
    store.flush();
    const session = new NewThreadDraftSession(store);
    session.adoptHost("host_a");

    // This is what the field does while typing: it changes nothing yet.
    expect(session.snapshot()).toEqual({
      host: "host_a",
      workspace: DEFAULT_NEW_THREAD_WORKSPACE,
      text: "",
    });

    expect(session.commitWorkspace("~/code/api")).toEqual({
      host: "host_a",
      workspace: "~/code/api",
      text: "api work",
    });
  });

  it("maps an empty folder to the default folder slot it will actually use", () => {
    const store = makeStore(memoryStorage());
    store.writeNewThreadDraft("host_a", DEFAULT_NEW_THREAD_WORKSPACE, "default work");
    store.flush();
    const session = new NewThreadDraftSession(store);
    session.adoptHost("host_a");
    session.commitWorkspace("~/code/api");
    session.setText("api work");

    const snapshot = session.commitWorkspace("   ");

    expect(snapshot.workspace).toBe(DEFAULT_NEW_THREAD_WORKSPACE);
    expect(snapshot.text).toBe("default work");
    expect(store.readNewThreadDraft("host_a", "~/code/api")).toBe("api work");
  });

  it("keeps the old folder's text when the folder changes, and restores it on return", () => {
    const store = makeStore(memoryStorage());
    const session = new NewThreadDraftSession(store);
    session.adoptHost("host_a");

    session.commitWorkspace("~/code/api");
    session.setText("api work");

    session.commitWorkspace("~/code/web");
    expect(session.snapshot().text).toBe("");
    session.setText("web work");

    session.commitWorkspace("~/code/api");
    expect(session.snapshot().text).toBe("api work");

    session.commitWorkspace("~/code/web");
    expect(session.snapshot().text).toBe("web work");
  });

  it("uses the same normalized slot for whitespace-padded folders", () => {
    const store = makeStore(memoryStorage());
    const session = new NewThreadDraftSession(store);
    session.adoptHost("host_a");
    session.commitWorkspace("~/code/api");
    session.setText("api work");

    session.commitWorkspace("~/code/web");
    const snapshot = session.commitWorkspace("  ~/code/api  ");

    expect(snapshot.workspace).toBe("~/code/api");
    expect(snapshot.text).toBe("api work");
  });

  it("does not remember a folder nobody picked", () => {
    const store = makeStore(memoryStorage());
    const session = new NewThreadDraftSession(store);

    session.adoptHost("host_a");

    expect(session.snapshot().workspace).toBe(DEFAULT_NEW_THREAD_WORKSPACE);
    expect(store.readLastWorkspace("host_a")).toBe("");
  });

  it("remembers the last folder per host across sessions", () => {
    const storage = memoryStorage();
    const store = makeStore(storage);
    const first = new NewThreadDraftSession(store);
    first.adoptHost("host_a");
    first.commitWorkspace("~/code/api");
    first.setText("api work");
    store.flush();

    const reopened = new NewThreadDraftSession(makeStore(storage));
    expect(reopened.adoptHost("host_a")).toEqual({
      host: "host_a",
      workspace: "~/code/api",
      text: "api work",
    });

    const otherHost = new NewThreadDraftSession(makeStore(storage));
    expect(otherHost.adoptHost("host_b").workspace).toBe(DEFAULT_NEW_THREAD_WORKSPACE);
  });

  it("loads the other host's draft when the user switches host", () => {
    const store = makeStore(memoryStorage());
    store.writeNewThreadDraft("host_b", DEFAULT_NEW_THREAD_WORKSPACE, "for b");
    store.flush();
    const session = new NewThreadDraftSession(store);
    session.adoptHost("host_a");
    session.setText("for a");

    const snapshot = session.selectHost("host_b");

    expect(snapshot.host).toBe("host_b");
    expect(snapshot.text).toBe("for b");
    expect(store.readNewThreadDraft("host_a", DEFAULT_NEW_THREAD_WORKSPACE)).toBe("for a");
  });

  it("shows the new host's slot when an auto-chosen host is rebound", () => {
    const store = makeStore(memoryStorage());
    store.writeNewThreadDraft("host_local", DEFAULT_NEW_THREAD_WORKSPACE, "local draft");
    store.writeNewThreadDraft("host_remote", DEFAULT_NEW_THREAD_WORKSPACE, "remote saved");
    store.flush();
    const session = new NewThreadDraftSession(store);
    session.adoptHost("host_remote");

    const snapshot = session.rebindHost("host_local");

    expect(snapshot).toEqual({
      host: "host_local",
      workspace: DEFAULT_NEW_THREAD_WORKSPACE,
      text: "local draft",
    });
    // Nothing is moved or cleared between the two hosts' slots.
    expect(store.readNewThreadDraft("host_remote", DEFAULT_NEW_THREAD_WORKSPACE)).toBe(
      "remote saved",
    );
  });

  it("does not let a preset folder fall back to the remembered one", () => {
    const storage = memoryStorage();
    const previous = new NewThreadDraftSession(makeStore(storage));
    previous.adoptHost("host_a");
    previous.commitWorkspace("~/code/api");
    previous.setText("api work");

    const preset = new NewThreadDraftSession(makeStore(storage), { workspace: "~/code/web" });
    expect(preset.adoptHost("host_a")).toEqual({
      host: "host_a",
      workspace: "~/code/web",
      text: "",
    });
  });

  it("clears the slot the thread was started from", () => {
    const store = makeStore(memoryStorage());
    const session = new NewThreadDraftSession(store);
    session.adoptHost("host_a");
    session.setText("start me");
    store.flush();

    expect(session.clearSubmitted(submit(session, DEFAULT_NEW_THREAD_WORKSPACE))).toBe(true);

    expect(session.snapshot().text).toBe("");
    expect(store.readNewThreadDraft("host_a", DEFAULT_NEW_THREAD_WORKSPACE)).toBe("");
  });

  it("reports that it cleared nothing when newer text replaced the send", () => {
    const store = makeStore(memoryStorage());
    const session = new NewThreadDraftSession(store);
    session.adoptHost("host_a");
    session.setText("start me");
    store.flush();
    const submission = submit(session, DEFAULT_NEW_THREAD_WORKSPACE);
    store.writeNewThreadDraft("host_a", DEFAULT_NEW_THREAD_WORKSPACE, "newer from another tab");
    store.flush();

    expect(session.clearSubmitted(submission)).toBe(false);
  });

  it("re-reads the visible slot when another writer changed it", () => {
    const store = makeStore(memoryStorage());
    const session = new NewThreadDraftSession(store);
    session.adoptHost("host_a");
    session.setText("in the dialog");
    store.flush();

    const dialog = new NewThreadDraftSession(store);
    dialog.adoptHost("host_a");
    dialog.setText("typed in the dialog");

    expect(session.reloadSlot().text).toBe("typed in the dialog");
  });

  it("sends the visible text to the folder on screen without moving it", () => {
    const store = makeStore(memoryStorage());
    store.writeNewThreadDraft("host_a", DEFAULT_NEW_THREAD_WORKSPACE, "default work");
    store.flush();
    const session = new NewThreadDraftSession(store);
    session.adoptHost("host_a");

    const submission = submit(session, "~/code/api");

    expect(submission).toMatchObject({
      host: "host_a",
      workspace: "~/code/api",
      text: "default work",
      slotWorkspace: DEFAULT_NEW_THREAD_WORKSPACE,
    });
    // Start does not swap the box to the other folder's draft.
    expect(session.snapshot().text).toBe("default work");

    session.clearSubmitted(submission);

    expect(store.readNewThreadDraft("host_a", DEFAULT_NEW_THREAD_WORKSPACE)).toBe("");
    expect(store.readNewThreadDraft("host_a", "~/code/api")).toBe("");
  });

  it("leaves newer text in the submitted slot alone", () => {
    const store = makeStore(memoryStorage());
    const session = new NewThreadDraftSession(store);
    session.adoptHost("host_a");
    session.setText("start me");
    store.flush();
    const submission = submit(session, DEFAULT_NEW_THREAD_WORKSPACE);
    store.writeNewThreadDraft("host_a", DEFAULT_NEW_THREAD_WORKSPACE, "newer from another tab");
    store.flush();

    session.clearSubmitted(submission);

    expect(store.readNewThreadDraft("host_a", DEFAULT_NEW_THREAD_WORKSPACE)).toBe(
      "newer from another tab",
    );
  });

  it("clears only the submitted slot when the folder changed while Start waited", () => {
    const store = makeStore(memoryStorage());
    const session = new NewThreadDraftSession(store);
    session.adoptHost("host_a");
    session.commitWorkspace("~/code/api");
    session.setText("api work");
    store.flush();
    const submission = submit(session, "~/code/api");

    session.commitWorkspace("~/code/web");
    session.setText("web work");
    store.flush();
    session.clearSubmitted(submission);

    expect(store.readNewThreadDraft("host_a", "~/code/api")).toBe("");
    expect(store.readNewThreadDraft("host_a", "~/code/web")).toBe("web work");
    expect(session.snapshot().text).toBe("web work");
  });

  it("captures the revision from the write, so a reload sees the same draft", () => {
    const storage = memoryStorage();
    const store = makeStore(storage);
    const session = new NewThreadDraftSession(store);
    session.adoptHost("host_a");
    session.setText("start me");
    store.flush();

    const reopened = new NewThreadDraftSession(makeStore(storage));
    reopened.adoptHost("host_a");
    expect(reopened.snapshot().text).toBe("start me");
  });
});
