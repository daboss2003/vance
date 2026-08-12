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
  entitlementsList,
  entitlementInstall,
  entitlementUninstall,
  entitlementsReconcile,
  paymentConnect,
  paymentConnectStatus,
  getAppSettings,
  setAppSettings,
  type AccountUser,
  type DeviceInfo,
  type OwnedEffect
} from '$lib/ipc';
import { toast } from '$lib/toast.svelte';

type Tab = 'account' | 'effects' | 'selling' | 'devices' | 'advanced';

interface AccountUI {
  open: boolean;
  tab: Tab;
  loading: boolean;
  signedIn: boolean;
  user: AccountUser | null;
  devices: DeviceInfo[];
  devicesLoaded: boolean;
  effects: OwnedEffect[];
  effectsLoaded: boolean;
  autoInstall: boolean;
  sellerConnected: boolean;
  sellerChargesEnabled: boolean;
  sellingLoaded: boolean;
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
  effects: [],
  effectsLoaded: false,
  autoInstall: false,
  sellerConnected: false,
  sellerChargesEnabled: false,
  sellingLoaded: false,
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
  if (account.signedIn && tab === 'effects') await loadEffects();
  if (account.signedIn && tab === 'selling') await loadSelling();
}

export async function loadSelling(): Promise<void> {
  try {
    const s = await paymentConnectStatus();
    account.sellerConnected = s.connected;
    account.sellerChargesEnabled = s.charges_enabled;
    account.sellingLoaded = true;
  } catch {
    account.sellingLoaded = true;
  }
}

export async function connectPayouts(): Promise<void> {
  account.busy = true;
  try {
    await paymentConnect();
    toast.info('Finish connecting payouts in the browser, then reopen this tab.');
  } catch (e) {
    toast.error(String(e));
  } finally {
    account.busy = false;
  }
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

// --- My Effects (M2) -------------------------------------------------------

export async function loadEffects(): Promise<void> {
  try {
    const [r, s] = await Promise.all([entitlementsList(), getAppSettings()]);
    account.effects = r.effects ?? [];
    account.autoInstall = s.auto_install_purchases ?? false;
    account.effectsLoaded = true;
  } catch (e) {
    toast.error(`Couldn’t load your effects.\n${String(e)}`);
  }
}

export async function installEffect(sku: string, name: string): Promise<void> {
  account.busy = true;
  try {
    await entitlementInstall(sku);
    toast.success(`Installed “${name}”.`);
    await loadEffects();
  } catch (e) {
    toast.error(`Couldn’t install “${name}”.\n${String(e)}`);
  } finally {
    account.busy = false;
  }
}

export async function uninstallEffect(sku: string, name: string): Promise<void> {
  account.busy = true;
  try {
    await entitlementUninstall(sku);
    toast.info(`Removed “${name}” from this device.`);
    await loadEffects();
  } catch (e) {
    toast.error(String(e));
  } finally {
    account.busy = false;
  }
}

/** Reconcile with the server (called on the entitlements-changed push). */
export async function reconcileEffects(): Promise<void> {
  try {
    await entitlementsReconcile();
    if (account.open && account.tab === 'effects') await loadEffects();
  } catch {
    /* transient; the next open/reconcile will catch up */
  }
}

export async function setAutoInstall(on: boolean): Promise<void> {
  try {
    const s = await getAppSettings();
    await setAppSettings({ ...s, auto_install_purchases: on });
    account.autoInstall = on;
    if (on) await reconcileEffects();
  } catch (e) {
    toast.error(String(e));
  }
}

function resetSignedOut(): void {
  account.signedIn = false;
  account.user = null;
  account.devices = [];
  account.devicesLoaded = false;
  account.effects = [];
  account.effectsLoaded = false;
}

/** Called when the backend pushes `account://revoked` (remote sign-out). */
export function onRevoked(): void {
  resetSignedOut();
  toast.error('This device was signed out of your Vance account.');
}
