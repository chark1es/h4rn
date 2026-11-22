import type { HostId } from "@/gen";

import { DEFAULT_NEW_THREAD_WORKSPACE, type DraftStore, normalizeWorkspace } from "./drafts";

/**
 * The New thread dialog's prompt box remembers text per host and folder.
 *
 * The dialog can be opened before the daemon has named a host, and the user
 * can change the host or the folder at any time. This session keeps the box,
 * the slot it belongs to, and the two changes in step: text typed before a
 * host exists is kept and bound to the slot once the host resolves, a folder
 * is only switched when the user commits it, and the dialog's auto-chosen host
 * can be rebound without writing the visible text over a saved draft.
 */

export type NewThreadDraftSnapshot = {
  host: HostId | "";
  workspace: string;
  text: string;
};

/** What a Start press submits: the visible slot and the revision it saw. */
export type NewThreadDraftSubmission = NewThreadDraftSnapshot & {
  revision: number | null;
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
  /** The visible text came from the user here, not from a slot they saved. */
  private carried = false;
  /** The current slot holds the visible text because of this session. */
  private owned = false;
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

  /** The slot and revision a Start press is about to send. */
  submission(): NewThreadDraftSubmission {
    return {
      ...this.snapshot(),
      // Text that never took its slot (a saved draft was already there) has no
      // revision of its own: sending it must not clear that draft.
      revision:
        this.host && this.owned
          ? this.store.readNewThreadDraftRevision(this.host, this.workspace)
          : null,
    };
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

  /**
   * The dialog's default host changed before the user picked one (the daemon
   * naming its own host, or a reconnect to another daemon). Move the box to
   * the new host rather than leave Start pointing at a host this daemon will
   * reject. Text the user typed while the host was auto-chosen travels with
   * it; a draft loaded from the old host's slot stays where it was saved.
   */
  rebindHost(host: HostId): NewThreadDraftSnapshot {
    if (host === this.host) return this.snapshot();
    if (!this.host) {
      this.bound = true;
      return this.bind(host, true);
    }
    const carry = this.carried ? this.text : null;
    if (carry !== null && this.owned) {
      // This session put the text into the auto-chosen host's slot; it was
      // never a draft the user saved there, so take it along.
      this.store.clearNewThreadDraft(this.host, this.workspace);
    }
    this.pending = carry;
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
    // Text that could not be saved over an occupied slot was never bound to
    // this folder; it follows the user instead of disappearing.
    const moveText = this.carried && !this.owned ? this.text : null;
    if (this.host) this.store.writeLastWorkspace(this.host, next);
    this.workspace = next;
    if (this.host) {
      if (moveText !== null) {
        if (this.store.readNewThreadDraft(this.host, next).length === 0) {
          this.store.writeNewThreadDraft(this.host, next, moveText);
          this.owned = true;
        } else {
          this.owned = false;
        }
        this.text = moveText;
      } else {
        this.text = this.store.readNewThreadDraft(this.host, next);
        this.owned = true;
        this.carried = false;
      }
    }
    return this.snapshot();
  }

  /** The user typed in the prompt box. */
  setText(text: string): void {
    this.text = text;
    this.carried = true;
    if (this.host) {
      this.store.writeNewThreadDraft(this.host, this.workspace, text);
      this.owned = true;
    } else {
      this.pending = text.length > 0 ? text : null;
      this.owned = false;
    }
  }

  /**
   * The thread started. Clear the slot only while it still holds the revision
   * that was sent: if the folder changed or another tab saved newer text while
   * Start waited, that text stays.
   */
  clearSubmitted(submission: NewThreadDraftSubmission): void {
    if (!submission.host) return;
    const cleared = this.store.clearNewThreadDraftIfRevision(
      submission.host,
      submission.workspace,
      submission.revision,
    );
    if (!cleared) return;
    if (
      this.host === submission.host &&
      this.workspace === submission.workspace &&
      this.text === submission.text
    ) {
      this.text = "";
      this.carried = false;
      this.owned = false;
    }
  }

  private bind(host: HostId, rememberWorkspace: boolean): NewThreadDraftSnapshot {
    this.host = host;
    if (rememberWorkspace && !this.workspaceChosen) {
      const last = this.store.readLastWorkspace(host);
      if (last.length > 0) this.workspace = normalizeWorkspace(last);
    }
    if (this.pending !== null) {
      // Keep what the user typed. Only fill an empty slot; never write it over
      // a stored draft the user could not see.
      const empty = this.store.readNewThreadDraft(host, this.workspace).length === 0;
      if (empty) this.store.writeNewThreadDraft(host, this.workspace, this.pending);
      this.text = this.pending;
      this.carried = true;
      this.owned = empty;
      this.pending = null;
    } else {
      this.text = this.store.readNewThreadDraft(host, this.workspace);
      this.carried = false;
      this.owned = true;
    }
    this.store.writeLastWorkspace(host, this.workspace);
    return this.snapshot();
  }
}
