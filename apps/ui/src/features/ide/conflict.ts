import type { FsWriteConflict } from "@/gen/FsWriteConflict";
import type { ConflictState } from "./useIdeSession";

/** Builds the dialog state for a save the daemon refused as a conflict. */
export function conflictFromWrite(
  path: string,
  mine: string,
  conflict: FsWriteConflict,
): ConflictState {
  return {
    path,
    mine,
    theirs: conflict.current_content ?? "",
    expectedEtag: conflict.expected_etag,
    currentEtag: conflict.current_etag,
    deleted: conflict.current_etag === null,
  };
}

/** Keep-mine on a deleted file recreates it (no base etag); otherwise it replaces the on-disk bytes. */
export function keepMineBaseEtag(conflict: ConflictState): string | null {
  return conflict.deleted ? null : conflict.currentEtag;
}
