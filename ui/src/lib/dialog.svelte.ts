// App-wide modal dialogs. Anything that needs a real dialog — a choice ("what
// should I do with this file?"), an explainer ("how to get add-on effects") —
// calls openDialog(...) and awaits the chosen action's value, instead of
// scattering panels/popovers around the layout. Rendered by Dialogs.svelte.

export interface DialogAction {
  label: string;
  /** Value resolved when this action is clicked. */
  value: string;
  kind?: 'primary' | 'default' | 'danger';
}

export interface DialogSpec {
  title: string;
  /** Body text (newlines respected). */
  body?: string;
  /** Optional bullet lines under the body. */
  bullets?: string[];
  actions: DialogAction[];
}

interface ActiveDialog extends DialogSpec {
  id: number;
  resolve: (value: string | null) => void;
}

let nextId = 0;

/** The dialog stack (topmost is shown). */
export const dialogs = $state<ActiveDialog[]>([]);

/** Open a dialog; resolves to the clicked action's value, or null if dismissed. */
export function openDialog(spec: DialogSpec): Promise<string | null> {
  return new Promise((resolve) => {
    dialogs.push({ ...spec, id: ++nextId, resolve });
  });
}

export function closeDialog(id: number, value: string | null): void {
  const i = dialogs.findIndex((d) => d.id === id);
  if (i >= 0) {
    const d = dialogs[i];
    dialogs.splice(i, 1);
    d.resolve(value);
  }
}
