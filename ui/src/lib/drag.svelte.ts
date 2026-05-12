// Simple shared drag state. Tauri 2's WKWebView disables HTML5 native
// drag-and-drop, so we roll our own using plain mousedown/mouseup on
// the source + a global mouseup handler on each drop zone.

class DragState {
  /** ID of the media item currently being dragged, or `null` when idle. */
  mediaId = $state<string | null>(null);
  /** Display name shown on the floating ghost while dragging. */
  mediaName = $state<string>('');
  /** Cursor position (CSS pixels) for the floating ghost. */
  cursorX = $state<number>(0);
  cursorY = $state<number>(0);

  begin(id: string, name: string, x: number, y: number) {
    this.mediaId = id;
    this.mediaName = name;
    this.cursorX = x;
    this.cursorY = y;
  }

  move(x: number, y: number) {
    if (!this.mediaId) return;
    this.cursorX = x;
    this.cursorY = y;
  }

  end() {
    this.mediaId = null;
    this.mediaName = '';
  }
}

export const dragState = new DragState();
