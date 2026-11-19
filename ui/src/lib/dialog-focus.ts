/**
 * Svelte action for modal dialog scrims (`<div class="scrim" use:dialogFocus>`).
 *
 * On mount it moves keyboard focus into the dialog, so Esc and other keys
 * reach the dialog immediately instead of the app behind it. On unmount it
 * hands focus back to whatever had it before (usually the button that
 * opened the dialog), if that element is still on the page.
 */
export function dialogFocus(node: HTMLElement) {
  const previous = document.activeElement instanceof HTMLElement ? document.activeElement : null;
  node.focus({ preventScroll: true });
  return {
    destroy() {
      if (previous && previous.isConnected) previous.focus({ preventScroll: true });
    }
  };
}
