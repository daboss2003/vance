// Which keys the focused element handles itself, so the app's global
// shortcuts (Space plays, J/K/L, arrows, Delete...) leave them alone, and
// how the focused element got its focus (keyboard or pointer).

/** Roles whose element is activated by Space / Enter. */
const ACTIVATABLE_ROLES = new Set([
  'button',
  'checkbox',
  'menuitem',
  'menuitemcheckbox',
  'menuitemradio',
  'option',
  'radio',
  'switch',
  'tab'
]);

type KeyTarget = {
  tagName: string;
  isContentEditable?: boolean;
  getAttribute(name: string): string | null;
};

/** True when `target` owns `key`: every key while typing (inputs, text
 *  areas, editable content), and Space / Enter on a button-like control that
 *  has KEYBOARD focus, which they activate. A button focused by a click
 *  (WebView2 and WebKitGTK focus buttons on click; WKWebView doesn't) leaves
 *  Space to the global shortcut, so it plays instead of re-firing the button.
 *  Anything else (the timeline canvas, the page) leaves keys to the globals. */
export function targetOwnsKey(
  target: KeyTarget | null,
  key: string,
  keyboardFocused: boolean
): boolean {
  if (!target) return false;
  const tag = target.tagName;
  if (tag === 'INPUT' || tag === 'TEXTAREA' || target.isContentEditable) return true;
  if (key !== ' ' && key !== 'Enter') return false;
  if (!keyboardFocused) return false;
  if (tag === 'BUTTON' || tag === 'SELECT' || tag === 'SUMMARY') return true;
  const role = target.getAttribute('role');
  return role !== null && ACTIVATABLE_ROLES.has(role);
}

// ---- Input modality ----------------------------------------------------------

export type Modality = 'keyboard' | 'pointer';

export type ModalityState<T> = {
  /** The last kind of input seen. */
  last: Modality;
  /** The element that got focus while the keyboard was the last input. */
  keyboardFocused: T | null;
};

/** A key press or a pointer press happened. Bare modifier presses don't count:
 *  Cmd / Ctrl on their own aren't navigation. A pointer press also ends any
 *  keyboard focus: clicking the button that Tab had reached leaves it
 *  pointer-focused (no focus event fires for it). */
export function noteInput<T>(
  state: ModalityState<T>,
  input: Modality,
  key?: string
): ModalityState<T> {
  if (input === 'pointer') return { last: 'pointer', keyboardFocused: null };
  if (key !== undefined && MODIFIER_KEYS.has(key)) return state;
  return { ...state, last: 'keyboard' };
}

/** `target` received focus: keyboard focus if the last input was a key
 *  (Tab, arrows, a key that made the app move focus). */
export function noteFocus<T>(state: ModalityState<T>, target: T): ModalityState<T> {
  return { ...state, keyboardFocused: state.last === 'keyboard' ? target : null };
}

const MODIFIER_KEYS = new Set(['Shift', 'Control', 'Alt', 'Meta', 'CapsLock']);

let modality: ModalityState<Element> = { last: 'pointer', keyboardFocused: null };

/** Track the input modality app-wide (capture phase, so it runs before any
 *  handler moves focus). Returns the uninstaller. */
export function installInputModality(win: Window): () => void {
  const onKey = (e: KeyboardEvent) => (modality = noteInput(modality, 'keyboard', e.key));
  const onPointer = () => (modality = noteInput(modality, 'pointer'));
  const onFocus = (e: FocusEvent) => {
    if (e.target instanceof Element) modality = noteFocus(modality, e.target);
  };
  win.addEventListener('keydown', onKey, true);
  win.addEventListener('pointerdown', onPointer, true);
  win.addEventListener('focusin', onFocus, true);
  return () => {
    win.removeEventListener('keydown', onKey, true);
    win.removeEventListener('pointerdown', onPointer, true);
    win.removeEventListener('focusin', onFocus, true);
  };
}

/** Whether `el` has focus that came from the keyboard. */
export function hasKeyboardFocus(el: Element | null): boolean {
  return el !== null && el === modality.keyboardFocused;
}

/** Whether the most recent input was a key press (e.g. a click that a key
 *  synthesised rather than the pointer). */
export function lastInputWasKeyboard(): boolean {
  return modality.last === 'keyboard';
}
