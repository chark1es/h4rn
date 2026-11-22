import type { HostId } from "@/gen";

import { DEFAULT_NEW_THREAD_WORKSPACE, type DraftStore, normalizeWorkspace } from "./drafts";

/**
 * The New thread dialog's prompt box remembers text per host and folder.
 *
 * The dialog can be opened before the daemon has named a host, and the user
 * can change the host or the folder at any time. This session keeps the box,
 * the slot it belongs to, and the two changes in step: text typed before a
 * host exists is kept and bound to the slot once the host resolves, and a
 * folder switch moves the old text back under the old folder first.
 */

export type NewThreadDraftSnapshot = {
  host: HostId | "";
  workspace: string;
  text: string;
};

export type NewThreadDraftOptions = {
  /** Folder a preset asked for; the user (or the dialog) chose it explicitly. */
  workspace?: string | undefined;
};

export class NewThreadDraftSession {
  private readonly store: DraftStore;
  private host: HostId | "" = "";
  private workspace: string;
  private text = "";
  /** Text typed before a host was known. */
  private pending: string | null = null;
  private workspaceChosen: boolean;
  private bound = false;

  constructor(store: DraftStore, options: NewThreadDraftOptions = {}) {
    this.store = store;
    const workspace = options.workspace ?? DEFAULT_NEW_THREAD_WORKSPACE;
    this.workspace = normalizeWorkspace(workspace);
    this.workspaceChosen =
      options.workspace !== undefined && normalizeWorkspace(options.workspace).length > 0;
  }

  snapshot(): NewThreadDraftSnapshot {
    return { host: this.host, workspace: this.workspace, text: this.text };
  }

  /**
   * The first host the dialog resolves on open. Later host-list refreshes do
   * not reset the form, and a preset/folder the user chose wins over the
   * remembered one.
   */
  adoptHost(host: HostId): NewThreadDraftSnapshot {
    if (this.bound) return this.snapshot();
    this.bound = true;
    return this.bind(host, true);
  }

  /** The user picked a host in the dropdown. */
  selectHost(host: HostId): NewThreadDraftSnapshot {
    if (host === this.host) return this.snapshot();
    this.saveCurrent();
    this.bound = true;
    return this.bind(host, false);
  }

  /** The user edited the folder field. */
  setWorkspace(workspace: string): NewThreadDraftSnapshot {
    const next = normalizeWorkspace(workspace);
    this.workspaceChosen = true;
    if (next === this.workspace) {
      if (this.host) this.store.writeLastWorkspace(this.host, next);
      return this.snapshot();
    }
    if (this.host) {
      this.saveCurrent();
      this.store.writeLastWorkspace(this.host, next);
    }
    this.workspace = next;
    this.text = this.host ? this.store.readNewThreadDraft(this.host, next) : this.text;
    return this.snapshot();
  }

  /** The user typed in the prompt box. */
  setText(text: string): void {
    this.text = text;
    if (this.host) {
      this.store.writeNewThreadDraft(this.host, this.workspace, text);
    } else {
      this.pending = text.length > 0 ? text : null;
    }
  }

  /** The thread started; its text must not come back next time. */
  clear(): void {
    if (this.host) this.store.clearNewThreadDraft(this.host, this.workspace);
    this.text = "";
    this.pending = null;
  }

  private saveCurrent(): void {
    if (!this.host) return;
    this.store.writeNewThreadDraft(this.host, this.workspace, this.text);
  }

  private bind(host: HostId, rememberWorkspace: boolean): NewThreadDraftSnapshot {
    this.host = host;
    if (rememberWorkspace && !this.workspaceChosen) {
      const last = this.store.readLastWorkspace(host);
      if (last.length > 0) this.workspace = normalizeWorkspace(last);
    }
    if (this.pending !== null) {
      // Keep what the user typed. Only fill an empty slot; never overwrite a
      // stored draft silently with text the user could not see.
      if (this.store.readNewThreadDraft(host, this.workspace).length === 0) {
        this.store.writeNewThreadDraft(host, this.workspace, this.pending);
      }
      this.text = this.pending;
      this.pending = null;
    } else {
      this.text = this.store.readNewThreadDraft(host, this.workspace);
    }
    this.store.writeLastWorkspace(host, this.workspace);
    return this.snapshot();
  }
}
