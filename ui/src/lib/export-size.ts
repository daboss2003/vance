// Export size vs. the sequence frame size (M1, ADR 0007). The export panel
// follows the sequence's size until the user types a different one; plain
// functions so the rules stay testable outside the component.

/** A typed export width or height. `undefined` follows the sequence size;
 *  `null` is a cleared field, which also exports at the sequence size. */
export type SizeEdit = number | null | undefined;

/** Renderers' canvas until the first clip sets the sequence size. */
export const DEFAULT_FRAME_SIZE: readonly [number, number] = [1920, 1080];

/** The sequence size the export follows. */
export function sequenceSize(
  seq: { frame_size: [number, number] | null } | null
): [number, number] {
  const [w, h] = seq?.frame_size ?? DEFAULT_FRAME_SIZE;
  return [w, h];
}

/** What a typed size is relative to: the open project, the current sequence
 *  and its frame size. A change to any of them drops the typed size. The
 *  effective size is used, so the first clip setting 1920×1080 on a sequence
 *  that already showed 1920×1080 keeps a typed size. */
export function exportSizeBasis(
  projectEpoch: number,
  seq: { id: string; frame_size: [number, number] | null } | null
): string {
  const [w, h] = sequenceSize(seq);
  return `${projectEpoch}:${seq?.id ?? ''}:${w}x${h}`;
}

/** The edit to store when the user types `typed` into a field whose
 *  sequence value is `seqValue`: typing the sequence's own value follows the
 *  sequence again (so ↑ then ↓, or retyping 1920, doesn't pin it). */
export function sizeEdit(typed: SizeEdit, seqValue: number): SizeEdit {
  return typed === seqValue ? undefined : typed;
}

/** The size to export for one axis. */
export function exportEdge(edit: SizeEdit, seqValue: number): number {
  return edit ?? seqValue;
}
