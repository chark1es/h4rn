import type { HostId } from "@/gen";

import {
  DEFAULT_NEW_THREAD_WORKSPACE,
  type DraftRevision,
  type DraftStore,
  normalizeWorkspace,
} from "./drafts";

/**
 * The New thread dialog's prompt box remembers text per host and folder.
 *
 * The dialog can be opened before the daemon has named a host, and the user
 * can change the host or the folder at any time. This session keeps the box in
 * step with one slot: the host and folder whose draft it currently shows. The
 * box cannot be typed in until a host is bound, so there is no provisional
 * text to move around; a host change simply shows that host's slot.
 */

export type NewThreadDraftSnapshot = {
  host: HostId | "";
  workspace: string;
  text: string;
};

/**
 * What a Start press sends: the text on screen, the folder the thread will run
 * in, and the slot (with its revision) that text was loaded from or written to.
 */
export type NewThreadDraftSubmission = {
  host: HostId;
  /** Folder for the request; the visible text is sent here even if it came from another slot. */
  workspace: string;
  text: string;
  /** The slot the visible text belongs to. */
  slotWorkspace: string;
  revision: DraftRevision | null;
};

export type NewThreadDraftOptions = {
  /** Folder a preset asked for; the user (or the dialog) chose it explicitly. */
  workspace?: string | undefined;
};

export class NewThreadDraftSession {
  private readonly store: DraftStore;
  private host: HostId | "" = "";
  /** The slot whose draft is on screen. */
  private workspace: string;
  private text = "";
  /** The revision of the text on screen: loaded, or returned by the last write. */
  private revision: DraftRevision | null = null;
  private workspaceChosen: boolean;
  private bound = false;

  constructor(store: DraftStore, options: NewThreadDraftOptions = {}) {
    this.store = store;
    const workspace = options.workspace ?? DEFAULT_NEW_THREAD_WORKSPACE;
    this.workspace = normalizeWorkspace(workspace) || DEFAULT_NEW_THREAD_WORKSPACE;
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
    return this.bind(host, !this.workspaceChosen);
  }

  /**
   * The dialog's default host changed before the user picked one (the daemon
   * naming its own host, or a reconnect to another daemon). Show the new
   * host's slot; the old host's text stays where it was saved.
   */
  rebindHost(host: HostId): NewThreadDraftSnapshot {
    if (host === this.host) return this.snapshot();
    this.bound = true;
    return this.bind(host, !this.workspaceChosen);
  }

  /** The user picked a host in the dropdown. */
  selectHost(host: HostId): NewThreadDraftSnapshot {
    if (host === this.host) return this.snapshot();
    this.bound = true;
    return this.bind(host, false);
  }

  /**
   * The user committed the folder field (blur, Enter or a preset). The prompt
   * box switches to that folder's draft only now, not on every keystroke. An
   * empty field is the default folder, which is the slot it will actually use.
   */
  commitWorkspace(workspace: string): NewThreadDraftSnapshot {
    const next = normalizeWorkspace(workspace) || DEFAULT_NEW_THREAD_WORKSPACE;
    this.workspaceChosen = true;
    if (next === this.workspace) {
      if (this.host) this.store.writeLastWorkspace(this.host, next);
      return this.snapshot();
    }
    this.workspace = next;
    if (this.host) this.store.writeLastWorkspace(this.host, next);
    this.loadSlot();
    return this.snapshot();
  }

  /** The user typed in the prompt box. */
  setText(text: string): void {
    this.text = text;
    if (!this.host) return;
    this.revision = this.store.writeNewThreadDraft(this.host, this.workspace, text);
  }

  /**
   * What Start sends for the folder on screen. The folder is only used for the
   * request: the visible text stays where it is, and the slot it came from is
   * what a successful start clears.
   */
  submission(folder: string): NewThreadDraftSubmission | null {
    if (!this.host) return null;
    return {
      host: this.host,
      workspace: normalizeWorkspace(folder) || DEFAULT_NEW_THREAD_WORKSPACE,
      text: this.text,
      slotWorkspace: this.workspace,
      revision: this.revision,
    };
  }

  /**
   * The thread started. Clear the text's own slot only while it still holds
   * the revision that was sent: if another tab saved newer text while Start
   * waited, that text stays.
   */
  clearSubmitted(submission: NewThreadDraftSubmission): void {
    const cleared = this.store.clearNewThreadDraftIfRevision(
      submission.host,
      submission.slotWorkspace,
      submission.revision,
    );
    if (!cleared) return;
    if (
      this.host === submission.host &&
      this.workspace === submission.slotWorkspace &&
      this.text === submission.text
    ) {
      this.text = "";
      this.revision = null;
    }
  }

  private bind(host: HostId, rememberWorkspace: boolean): NewThreadDraftSnapshot {
    this.host = host;
    if (rememberWorkspace) {
      const last = this.store.readLastWorkspace(host);
      if (last.length > 0) this.workspace = normalizeWorkspace(last);
    }
    this.loadSlot();
    this.store.writeLastWorkspace(host, this.workspace);
    return this.snapshot();
  }

  private loadSlot(): void {
    if (!this.host) return;
    const entry = this.store.readNewThreadDraftEntry(this.host, this.workspace);
    this.text = entry.text;
    this.revision = entry.revision;
  }
}
