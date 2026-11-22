// Keyboard focus in the track menu (WAI-ARIA menu pattern).

/** Which items arrow keys move through, and where the menu opens. Every
 *  item is in the order, unavailable ones too: they're `aria-disabled`, so
 *  the keyboard reaches them and their hint ("Unlock the track to delete
 *  it.") is read out. The menu opens on the first available item. */
export function menuFocusPlan(disabled: readonly boolean[]): {
  order: number[];
  initial: number;
} {
  const first = disabled.findIndex((d) => !d);
  // All unavailable (an export pauses them): still open on the first, so the
  // keyboard is in the menu and its hint is read out.
  return {
    order: disabled.map((_, i) => i),
    initial: first >= 0 ? first : disabled.length > 0 ? 0 : -1
  };
}

/** The item `delta` steps from `at` in `order`, wrapping; from outside the
 *  order, Down goes to the first item and Up to the last. */
export function stepFocus(order: readonly number[], at: number, delta: number): number {
  if (order.length === 0) return -1;
  const pos = order.indexOf(at);
  if (pos < 0) return delta > 0 ? order[0] : order[order.length - 1];
  return order[(pos + delta + order.length) % order.length];
}
