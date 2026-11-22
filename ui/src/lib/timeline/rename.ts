// Inline track rename: what happens when the engine refuses a name.

/** How the rename field was committed. */
export type RenameCommit = 'enter' | 'blur' | 'scroll';

/** After a refused rename, reopen the field with the user's text only when
 *  they pressed Enter: they're still on the field and can fix the name. A
 *  blur or a scroll means they moved on, so reopening would pull focus back
 *  (leaving Esc as the only way out); the error toast says why and the draft
 *  is dropped. Never reopen over another rename or for a track that's gone. */
export function reopenAfterRefusal(
  via: RenameCommit,
  otherRenameOpen: boolean,
  trackExists: boolean
): boolean {
  return via === 'enter' && !otherRenameOpen && trackExists;
}
