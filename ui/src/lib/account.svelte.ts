// Account + Settings state (M1). Backs SettingsDialog.svelte and reacts to the
// `account://revoked` push (remote sign-out) from the backend heartbeat watcher.

import {
  accountStatus,
  accountSignIn,
  accountSignOut,
  accountListDevices,
  accountStepup,
  accountRevokeDevice,
  accountSetPassword,
  accountBaseUrl,
  accountSetBaseUrl,
  type AccountUser,
  type DeviceInfo
} from '$lib/ipc';
import { toast } from '$lib/toast.svelte';

type Tab = 'account' | 'devices' | 'advanced';

interface AccountUI {
  open: boolean;
  tab: Tab;
  loading: boolean;
  signedIn: boolean;
  user: AccountUser | null;
  devices: DeviceInfo[];
  devicesLoaded: boolean;
  busy: boolean;
  baseUrl: string;
}

export const account = $state<AccountUI>({
  open: false,
  tab: 'account',
  loading: false,
  signedIn: false,
  user: null,
  devices: [],
  devicesLoaded: false,
  busy: false,
  baseUrl: ''
});

export async function refreshStatus(): Promise<void> {
  account.loading = true;
  try {
    const s = await accountStatus();
    account.signedIn = s.signed_in;
    account.user = s.user ?? null;
  } catch {
    account.signedIn = false;
    account.user = null;
  } finally {
    account.loading = false;
  }
}

export async function openSettings(tab: Tab = 'account'): Promise<void> {
  account.open = true;
  account.tab = tab;
  try {
    account.baseUrl = await accountBaseUrl();
  } catch {
    /* ignore */
  }
  await refreshStatus();
  if (account.signedIn && tab === 'devices') await loadDevices();
}

export function closeSettings(): void {
  account.open = false;
}

export async function signIn(provider: string): Promise<void> {
  account.busy = true;
  try {
    const user = await accountSignIn(provider);
    account.signedIn = true;
    account.user = user;
    toast.success('Signed in to Vance.');
  } catch (e) {
    toast.error(`Sign-in didn’t complete.\n${String(e)}`);
  } finally {
    account.busy = false;
  }
}

export async function signOut(): Promise<void> {
  account.busy = true;
  try {
    await accountSignOut();
    resetSignedOut();
    toast.info('Signed out on this device.');
  } catch (e) {
    toast.error(String(e));
  } finally {
    account.busy = false;
  }
}

export async function loadDevices(): Promise<void> {
  try {
    const r = await accountListDevices();
    account.devices = r.devices ?? [];
    account.devicesLoaded = true;
  } catch (e) {
    toast.error(`Couldn’t load your devices.\n${String(e)}`);
  }
}

/** Remotely sign another device out (step-up with the account password). */
export async function removeDevice(id: string, name: string, password: string): Promise<void> {
  account.busy = true;
  try {
    const { stepup_token } = await accountStepup({
      action: 'remove_device',
      targetRef: id,
      password
    });
    await accountRevokeDevice(id, stepup_token);
    toast.success(`Signed “${name}” out.`);
    await loadDevices();
  } catch (e) {
    toast.error(String(e));
    throw e;
  } finally {
    account.busy = false;
  }
}

/** Set (first time) or change (needs current password) the account password. */
export async function setPassword(newPassword: string, currentPassword?: string): Promise<void> {
  account.busy = true;
  try {
    let stepup: string | null = null;
    if (account.user?.password_set) {
      if (!currentPassword) throw new Error('Enter your current password.');
      const r = await accountStepup({ action: 'change_password', password: currentPassword });
      stepup = r.stepup_token;
    }
    await accountSetPassword(newPassword, stepup);
    toast.success(account.user?.password_set ? 'Password changed.' : 'Password set.');
    await refreshStatus();
  } catch (e) {
    toast.error(String(e));
    throw e;
  } finally {
    account.busy = false;
  }
}

export async function saveBaseUrl(url: string): Promise<void> {
  try {
    await accountSetBaseUrl(url);
    account.baseUrl = url.trim().replace(/\/+$/, '');
    toast.success('Accounts server updated.');
    await refreshStatus();
  } catch (e) {
    toast.error(String(e));
  }
}

function resetSignedOut(): void {
  account.signedIn = false;
  account.user = null;
  account.devices = [];
  account.devicesLoaded = false;
}

/** Called when the backend pushes `account://revoked` (remote sign-out). */
export function onRevoked(): void {
  resetSignedOut();
  toast.error('This device was signed out of your Vance account.');
}
