// "Waiting for <file>…" notices with a Cancel (engine://import-waiting and
// engine://import-settled), shown as persistent toasts. Rules in
// import-feedback.ts `createImportWaits`.

import { cancelImport } from './ipc';
import { createImportWaits } from './import-feedback';
import { dismissToast, pushToast, toast, toasts } from './toast.svelte';

export const importWaits = createImportWaits<number>({
  show: (fileName) =>
    pushToast(`Waiting for ${fileName}…`, 'info', 0, {
      label: 'Cancel',
      busyLabel: 'Cancelling…',
      run: () => cancelWaiting(fileName)
    }),
  hide: (id) => dismissToast(id),
  isShown: (id) => toasts.some((t) => t.id === id)
});

async function cancelWaiting(fileName: string) {
  const notice = toasts.find((t) => t.message === `Waiting for ${fileName}…` && t.action);
  if (!notice?.action || notice.action.busy) return;
  notice.action.busy = true;
  importWaits.markCancelled(fileName);
  try {
    await cancelImport();
  } catch (e) {
    // Still waiting: offer Cancel again.
    importWaits.markCancelled(fileName, false);
    notice.action.busy = false;
    toast.error(`Couldn’t cancel the import.\n${String(e)}`);
  }
}
