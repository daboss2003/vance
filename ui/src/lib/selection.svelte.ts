// Timeline selection state. Lives in its own module so the Timeline
// canvas, the Inspector (4.10), and edit-op commands (4.5 / 4.6) can
// all read and mutate the same set without prop-drilling.

class SelectionState {
  /** UUIDs of currently-selected clips. Backed by an array (not a
   *  `Set`) so Svelte 5's reactivity sees mutations. */
  private _ids = $state<string[]>([]);

  /** All currently-selected clip IDs. */
  get ids(): string[] {
    return this._ids;
  }

  /** Whether a given clip is in the selection. */
  has(id: string): boolean {
    return this._ids.includes(id);
  }

  /** Number of selected clips. */
  get count(): number {
    return this._ids.length;
  }

  /** Replace the selection with exactly this one clip. */
  selectOnly(id: string) {
    this._ids = [id];
  }

  /** Add a clip without disturbing the rest of the selection. */
  add(id: string) {
    if (!this._ids.includes(id)) this._ids = [...this._ids, id];
  }

  /** Remove a clip from the selection. */
  remove(id: string) {
    this._ids = this._ids.filter((x) => x !== id);
  }

  /** Toggle a clip's membership. */
  toggle(id: string) {
    if (this.has(id)) this.remove(id);
    else this.add(id);
  }

  /** Replace the selection with this exact set. */
  replace(ids: string[]) {
    // Dedup while preserving order.
    const seen = new Set<string>();
    this._ids = ids.filter((id) => (seen.has(id) ? false : (seen.add(id), true)));
  }

  /** Empty the selection. */
  clear() {
    if (this._ids.length > 0) this._ids = [];
  }
}

export const selectionState = new SelectionState();
