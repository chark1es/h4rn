import { describe, expect, it } from "vitest";

import { createDraftStore, DEFAULT_NEW_THREAD_WORKSPACE, type DraftStorage } from "./drafts";
import { NewThreadDraftSession } from "./newThreadDraft";

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

  it("binds text typed before a host resolved into an empty slot", () => {
    const store = makeStore(memoryStorage());
    const session = new NewThreadDraftSession(store);
    session.setText("typed before hello");

    const snapshot = session.adoptHost("host_a");

    expect(snapshot.text).toBe("typed before hello");
    expect(store.readNewThreadDraft("host_a", DEFAULT_NEW_THREAD_WORKSPACE)).toBe(
      "typed before hello",
    );
  });

  it("keeps pre-host text without overwriting a draft already in the slot", () => {
    const store = makeStore(memoryStorage());
    store.writeNewThreadDraft("host_a", DEFAULT_NEW_THREAD_WORKSPACE, "stored earlier");
    store.flush();
    const session = new NewThreadDraftSession(store);
    session.setText("typed before hello");

    const snapshot = session.adoptHost("host_a");

    expect(snapshot.text).toBe("typed before hello");
    expect(store.readNewThreadDraft("host_a", DEFAULT_NEW_THREAD_WORKSPACE)).toBe("stored earlier");
  });

  it("never writes pre-host text over an occupied slot when the folder changes", () => {
    const store = makeStore(memoryStorage());
    store.writeNewThreadDraft("host_a", DEFAULT_NEW_THREAD_WORKSPACE, "stored earlier");
    store.flush();
    const session = new NewThreadDraftSession(store);
    session.setText("typed before hello");
    session.adoptHost("host_a");

    const snapshot = session.commitWorkspace("~/code/api");

    expect(snapshot.text).toBe("typed before hello");
    expect(store.readNewThreadDraft("host_a", DEFAULT_NEW_THREAD_WORKSPACE)).toBe("stored earlier");
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

  it("rebinds an auto-chosen host and takes the typed text along", () => {
    const store = makeStore(memoryStorage());
    const session = new NewThreadDraftSession(store);
    session.adoptHost("host_remote");
    session.setText("typed for the wrong host");

    const snapshot = session.rebindHost("host_local");

    expect(snapshot).toEqual({
      host: "host_local",
      workspace: DEFAULT_NEW_THREAD_WORKSPACE,
      text: "typed for the wrong host",
    });
    expect(store.readNewThreadDraft("host_local", DEFAULT_NEW_THREAD_WORKSPACE)).toBe(
      "typed for the wrong host",
    );
    expect(store.readNewThreadDraft("host_remote", DEFAULT_NEW_THREAD_WORKSPACE)).toBe("");
  });

  it("leaves a saved draft on its host when the host is rebound", () => {
    const store = makeStore(memoryStorage());
    store.writeNewThreadDraft("host_remote", DEFAULT_NEW_THREAD_WORKSPACE, "remote saved");
    store.flush();
    const session = new NewThreadDraftSession(store);
    session.adoptHost("host_remote");

    const snapshot = session.rebindHost("host_local");

    expect(snapshot.text).toBe("");
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

    session.clearSubmitted(session.submission());

    expect(session.snapshot().text).toBe("");
    expect(store.readNewThreadDraft("host_a", DEFAULT_NEW_THREAD_WORKSPACE)).toBe("");
  });

  it("leaves newer text in the submitted folder alone", () => {
    const store = makeStore(memoryStorage());
    const session = new NewThreadDraftSession(store);
    session.adoptHost("host_a");
    session.setText("start me");
    store.flush();
    const submission = session.submission();
    store.writeNewThreadDraft("host_a", DEFAULT_NEW_THREAD_WORKSPACE, "newer from another tab");
    store.flush();

    session.clearSubmitted(submission);

    expect(store.readNewThreadDraft("host_a", DEFAULT_NEW_THREAD_WORKSPACE)).toBe(
      "newer from another tab",
    );
  });

  it("clears only the submitted folder when the folder changed while Start waited", () => {
    const store = makeStore(memoryStorage());
    const session = new NewThreadDraftSession(store);
    session.adoptHost("host_a");
    session.commitWorkspace("~/code/api");
    session.setText("api work");
    store.flush();
    const submission = session.submission();

    session.commitWorkspace("~/code/web");
    session.setText("web work");
    store.flush();
    session.clearSubmitted(submission);

    expect(store.readNewThreadDraft("host_a", "~/code/api")).toBe("");
    expect(store.readNewThreadDraft("host_a", "~/code/web")).toBe("web work");
    expect(session.snapshot().text).toBe("web work");
  });

  it("does not clear a draft the pre-host text never took the slot of", () => {
    const store = makeStore(memoryStorage());
    store.writeNewThreadDraft("host_a", DEFAULT_NEW_THREAD_WORKSPACE, "stored earlier");
    store.flush();
    const session = new NewThreadDraftSession(store);
    session.setText("typed before hello");
    session.adoptHost("host_a");

    session.clearSubmitted(session.submission());

    expect(store.readNewThreadDraft("host_a", DEFAULT_NEW_THREAD_WORKSPACE)).toBe("stored earlier");
  });

  it("writes nothing until it has a host", () => {
    const storage = memoryStorage();
    const store = makeStore(storage);
    const session = new NewThreadDraftSession(store);

    session.setText("waiting");
    store.flush();

    expect(storage.data.size).toBe(0);
    expect(session.snapshot().text).toBe("waiting");
  });
});
