import { describe, expect, it } from "vitest";

import { createDraftStore, type DraftStorage } from "./drafts";
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
    store.writeNewThreadDraft("host_a", "~/code", "saved prompt");
    store.flush();

    const session = new NewThreadDraftSession(store);
    expect(session.adoptHost("host_a")).toEqual({
      host: "host_a",
      workspace: "~/code",
      text: "saved prompt",
    });
  });

  it("keeps the old folder's text when the folder changes, and restores it on return", () => {
    const store = makeStore(memoryStorage());
    const session = new NewThreadDraftSession(store);
    session.adoptHost("host_a");

    session.setWorkspace("~/code/api");
    session.setText("api work");

    session.setWorkspace("~/code/web");
    expect(session.snapshot().text).toBe("");
    session.setText("web work");

    session.setWorkspace("~/code/api");
    expect(session.snapshot().text).toBe("api work");

    session.setWorkspace("~/code/web");
    expect(session.snapshot().text).toBe("web work");
  });

  it("uses the same normalized slot for whitespace-padded folders", () => {
    const store = makeStore(memoryStorage());
    const session = new NewThreadDraftSession(store);
    session.adoptHost("host_a");
    session.setWorkspace("~/code/api");
    session.setText("api work");

    session.setWorkspace("~/code/web");
    const snapshot = session.setWorkspace("  ~/code/api  ");

    expect(snapshot.workspace).toBe("~/code/api");
    expect(snapshot.text).toBe("api work");
  });

  it("remembers the last folder per host across sessions", () => {
    const storage = memoryStorage();
    const store = makeStore(storage);
    const first = new NewThreadDraftSession(store);
    first.adoptHost("host_a");
    first.setWorkspace("~/code/api");
    first.setText("api work");
    store.flush();

    const reopened = new NewThreadDraftSession(makeStore(storage));
    expect(reopened.adoptHost("host_a")).toEqual({
      host: "host_a",
      workspace: "~/code/api",
      text: "api work",
    });

    const otherHost = new NewThreadDraftSession(makeStore(storage));
    expect(otherHost.adoptHost("host_b").workspace).toBe("~/code");
  });

  it("binds text typed before a host resolved into an empty slot", () => {
    const store = makeStore(memoryStorage());
    const session = new NewThreadDraftSession(store);
    session.setText("typed before hello");

    const snapshot = session.adoptHost("host_a");

    expect(snapshot.text).toBe("typed before hello");
    expect(store.readNewThreadDraft("host_a", "~/code")).toBe("typed before hello");
  });

  it("keeps pre-host text without overwriting a draft already in the slot", () => {
    const store = makeStore(memoryStorage());
    store.writeNewThreadDraft("host_a", "~/code", "stored earlier");
    store.flush();
    const session = new NewThreadDraftSession(store);
    session.setText("typed before hello");

    const snapshot = session.adoptHost("host_a");

    expect(snapshot.text).toBe("typed before hello");
    expect(store.readNewThreadDraft("host_a", "~/code")).toBe("stored earlier");
  });

  it("loads the other host's draft when the user switches host", () => {
    const store = makeStore(memoryStorage());
    store.writeNewThreadDraft("host_b", "~/code", "for b");
    store.flush();
    const session = new NewThreadDraftSession(store);
    session.adoptHost("host_a");
    session.setText("for a");

    const snapshot = session.selectHost("host_b");

    expect(snapshot.host).toBe("host_b");
    expect(snapshot.text).toBe("for b");
    expect(store.readNewThreadDraft("host_a", "~/code")).toBe("for a");
  });

  it("does not let a preset folder fall back to the remembered one", () => {
    const storage = memoryStorage();
    const previous = new NewThreadDraftSession(makeStore(storage));
    previous.adoptHost("host_a");
    previous.setWorkspace("~/code/api");
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

    session.clear();

    expect(session.snapshot().text).toBe("");
    expect(store.readNewThreadDraft("host_a", "~/code")).toBe("");
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
