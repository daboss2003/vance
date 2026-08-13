<script lang="ts">
  /**
   * Settings dialog (M1). Tabs: Account (sign in/out, password), Devices
   * (manage where you're signed in, remote sign-out), and Advanced (the
   * accounts-server URL). Reuses the app's modal scrim/card pattern.
   */
  import {
    account,
    closeSettings,
    signIn,
    signOut,
    loadDevices,
    removeDevice,
    setPassword,
    saveBaseUrl,
    loadEffects,
    installEffect,
    uninstallEffect,
    setAutoInstall,
    loadSelling,
    connectPayouts
  } from '$lib/account.svelte';
  import type { DeviceInfo } from '$lib/ipc';

  const PROVIDERS = [
    { id: 'google', label: 'Continue with Google' },
    { id: 'apple', label: 'Continue with Apple' },
    { id: 'github', label: 'Continue with GitHub' },
    { id: 'meta', label: 'Continue with Meta' }
  ];

  // Password form
  let newPassword = $state('');
  let currentPassword = $state('');
  // Inline device-removal confirmation
  let confirmingId = $state<string | null>(null);
  let devicePassword = $state('');
  // Advanced
  let baseUrlDraft = $state('');

  // Load devices when the Devices tab is shown while signed in.
  $effect(() => {
    if (account.open && account.tab === 'devices' && account.signedIn && !account.devicesLoaded) {
      void loadDevices();
    }
  });
  // Load owned effects when the My Effects tab is shown while signed in.
  $effect(() => {
    if (account.open && account.tab === 'effects' && account.signedIn && !account.effectsLoaded) {
      void loadEffects();
    }
  });
  // Load seller/payout status when the Selling tab is shown while signed in.
  $effect(() => {
    if (account.open && account.tab === 'selling' && account.signedIn && !account.sellingLoaded) {
      void loadSelling();
    }
  });
  $effect(() => {
    if (account.open) baseUrlDraft = account.baseUrl;
  });

  function relative(seconds: number): string {
    const d = Math.max(0, Math.floor(Date.now() / 1000) - seconds);
    if (d < 60) return 'just now';
    if (d < 3600) return `${Math.floor(d / 60)} min ago`;
    if (d < 86400) return `${Math.floor(d / 3600)} h ago`;
    return `${Math.floor(d / 86400)} d ago`;
  }

  function prettyPlatform(p: string): string {
    return p
      .replace('macos', 'macOS')
      .replace('windows', 'Windows')
      .replace('linux', 'Linux')
      .replace('-aarch64', ' · Apple silicon')
      .replace('-x86_64', ' · Intel/AMD');
  }

  async function submitPassword() {
    if (!newPassword) return;
    try {
      await setPassword(newPassword, currentPassword || undefined);
      newPassword = '';
      currentPassword = '';
    } catch {
      /* toast already shown */
    }
  }

  async function confirmRemove(dev: DeviceInfo) {
    try {
      await removeDevice(dev.id, dev.device_name, devicePassword);
      confirmingId = null;
      devicePassword = '';
    } catch {
      /* toast already shown; keep the field open to retry */
    }
  }
</script>

{#if account.open}
  <div
    class="scrim"
    role="dialog"
    aria-modal="true"
    aria-label="Settings"
    onclick={(e) => {
      if (e.target === e.currentTarget) closeSettings();
    }}
  >
    <div class="card">
      <div class="head">
        <h2>Settings</h2>
        <button class="x" onclick={closeSettings} aria-label="Close">✕</button>
      </div>

      <div class="tabs">
        <button class:active={account.tab === 'account'} onclick={() => (account.tab = 'account')}>
          Account
        </button>
        <button class:active={account.tab === 'effects'} onclick={() => (account.tab = 'effects')}>
          My Effects
        </button>
        <button class:active={account.tab === 'selling'} onclick={() => (account.tab = 'selling')}>
          Selling
        </button>
        <button class:active={account.tab === 'devices'} onclick={() => (account.tab = 'devices')}>
          Devices
        </button>
        <button class:active={account.tab === 'advanced'} onclick={() => (account.tab = 'advanced')}>
          Advanced
        </button>
      </div>

      <!-- ACCOUNT -->
      {#if account.tab === 'account'}
        {#if account.loading}
          <div class="muted">Checking…</div>
        {:else if !account.signedIn}
          <p class="lead">Sign in to buy, sell, and sync add-on effects across your machines.</p>
          <div class="providers">
            {#each PROVIDERS as p (p.id)}
              <button class="provider" disabled={account.busy} onclick={() => signIn(p.id)}>
                {p.label}
              </button>
            {/each}
          </div>
          <button class="dev" disabled={account.busy} onclick={() => signIn('dev')}>
            {account.busy ? 'Opening your browser…' : 'Local dev sign-in (for testing)'}
          </button>
        {:else}
          <div class="who">
            <div class="avatar">{(account.user?.email ?? 'V').slice(0, 1).toUpperCase()}</div>
            <div class="min0">
              <div class="email">{account.user?.email ?? 'Signed in'}</div>
              <div class="muted small">Signed in on this device</div>
            </div>
            <button class="ghost" disabled={account.busy} onclick={signOut}>Sign out</button>
          </div>

          <div class="sec-title">
            {account.user?.password_set ? 'Change password' : 'Set a password'}
          </div>
          <p class="muted small">
            {account.user?.password_set
              ? 'Your password protects purchases and removing devices.'
              : 'Add a password — it protects purchases and removing a device from another machine.'}
          </p>
          {#if account.user?.password_set}
            <input
              class="in"
              type="password"
              placeholder="Current password"
              bind:value={currentPassword}
              autocomplete="current-password"
            />
          {/if}
          <input
            class="in"
            type="password"
            placeholder={account.user?.password_set ? 'New password' : 'Create a password'}
            bind:value={newPassword}
            autocomplete="new-password"
          />
          <button class="primary" disabled={account.busy || !newPassword} onclick={submitPassword}>
            {account.user?.password_set ? 'Change password' : 'Set password'}
          </button>
        {/if}

      <!-- MY EFFECTS -->
      {:else if account.tab === 'effects'}
        {#if !account.signedIn}
          <div class="muted">Sign in to see the effects you own.</div>
        {:else}
          <label class="toggle">
            <input
              type="checkbox"
              checked={account.autoInstall}
              onchange={(e) => setAutoInstall(e.currentTarget.checked)}
            />
            <span>Automatically install effects I own on this device</span>
          </label>
          {#if !account.effectsLoaded}
            <div class="muted">Loading…</div>
          {:else if account.effects.length === 0}
            <div class="muted small">You don’t own any add-on effects yet.</div>
          {:else}
            <ul class="devs">
              {#each account.effects as fx (fx.sku)}
                <li>
                  <div class="min0">
                    <div class="dname">{fx.display_name ?? fx.sku}</div>
                    <div class="muted small">{fx.sku}</div>
                  </div>
                  {#if fx.installed}
                    <button
                      class="ghost"
                      disabled={account.busy}
                      onclick={() => uninstallEffect(fx.sku, fx.display_name ?? fx.sku)}
                    >
                      Remove
                    </button>
                  {:else}
                    <button
                      class="primary"
                      disabled={account.busy}
                      onclick={() => installEffect(fx.sku, fx.display_name ?? fx.sku)}
                    >
                      Install
                    </button>
                  {/if}
                </li>
              {/each}
            </ul>
          {/if}
        {/if}

      <!-- SELLING -->
      {:else if account.tab === 'selling'}
        {#if !account.signedIn}
          <div class="muted">Sign in to sell your effects.</div>
        {:else}
          <p class="muted small">
            Sell effects you make. Buyers pay you directly through Stripe; Vance
            takes a 20% fee and never holds your money.
          </p>
          {#if !account.sellingLoaded}
            <div class="muted">Checking…</div>
          {:else}
            {#if account.sellerFrozen}
              <div class="warn-banner">
                Your listings are paused pending review after buyer reports.
              </div>
            {/if}
            {#if account.sellerChargesEnabled}
              <div class="who">
                <div class="avatar">$</div>
                <div class="min0">
                  <div class="email">
                    Payouts connected
                    <span class="badge">{account.sellerTrusted ? 'Trusted' : 'New seller'}</span>
                  </div>
                  <div class="muted small">
                    {account.sellerTrusted
                      ? "You're ready to sell paid effects."
                      : 'New sellers can list paid effects up to $20 until they build a track record.'}
                  </div>
                </div>
              </div>
            {:else}
              <button class="primary" disabled={account.busy} onclick={connectPayouts}>
                {account.sellerConnected ? 'Finish connecting payouts' : 'Connect payouts with Stripe'}
              </button>
              <p class="muted small" style="margin-top:10px">
                Opens Stripe in your browser to set up how you get paid. Requires the
                marketplace server to have Stripe configured.
              </p>
            {/if}

            {#if account.sellerListings.length > 0}
              <div class="sec-title">Your effects</div>
              <ul class="devs">
                {#each account.sellerListings as l (l.sku)}
                  <li>
                    <div class="min0">
                      <div class="dname">{l.display_name}</div>
                      <div class="muted small">
                        {l.price_cents === 0 ? 'Free' : `$${(l.price_cents / 100).toFixed(2)}`} ·
                        {l.downloads} installs{#if l.reports > 0} ·
                          <span style="color:#f2c14e">{l.reports} reports</span>{/if}
                      </div>
                    </div>
                  </li>
                {/each}
              </ul>
            {/if}
          {/if}
        {/if}

      <!-- DEVICES -->
      {:else if account.tab === 'devices'}
        {#if !account.signedIn}
          <div class="muted">Sign in to manage your devices.</div>
        {:else if !account.devicesLoaded}
          <div class="muted">Loading…</div>
        {:else}
          <p class="muted small">
            The machines signed in to your account. Paid effects work on up to 2 at a time.
          </p>
          <ul class="devs">
            {#each account.devices as d (d.id)}
              <li>
                <div class="min0">
                  <div class="dname">
                    {d.device_name}
                    {#if d.this_device}<span class="badge">This device</span>{/if}
                  </div>
                  <div class="muted small">
                    {prettyPlatform(d.platform)} · active {relative(d.last_seen_at)}
                  </div>
                </div>
                {#if !d.this_device}
                  {#if confirmingId === d.id}
                    <div class="confirm">
                      <input
                        class="in tiny"
                        type="password"
                        placeholder="Your password"
                        bind:value={devicePassword}
                        autocomplete="current-password"
                      />
                      <button
                        class="danger"
                        disabled={account.busy || !devicePassword}
                        onclick={() => confirmRemove(d)}
                      >
                        Sign out
                      </button>
                      <button class="ghost tiny" onclick={() => ((confirmingId = null), (devicePassword = ''))}>
                        Cancel
                      </button>
                    </div>
                  {:else}
                    <button class="ghost" onclick={() => ((confirmingId = d.id), (devicePassword = ''))}>
                      Sign out
                    </button>
                  {/if}
                {/if}
              </li>
            {/each}
          </ul>
        {/if}

      <!-- ADVANCED -->
      {:else}
        <div class="sec-title">Accounts server</div>
        <p class="muted small">
          Where Vance signs in and syncs your account. Leave the default unless you're pointing at
          your own deployment.
        </p>
        <input class="in" type="text" bind:value={baseUrlDraft} placeholder="http://127.0.0.1:8787" />
        <button
          class="primary"
          disabled={!baseUrlDraft || baseUrlDraft === account.baseUrl}
          onclick={() => saveBaseUrl(baseUrlDraft)}
        >
          Save
        </button>
      {/if}
    </div>
  </div>
{/if}

<style>
  .scrim {
    position: fixed;
    inset: 0;
    z-index: 3400;
    background: rgba(0, 0, 0, 0.62);
    display: flex;
    align-items: center;
    justify-content: center;
    padding: 20px;
  }
  .card {
    width: 460px;
    max-width: 94vw;
    max-height: 88vh;
    overflow: auto;
    background: var(--panel-bg, #1e2127);
    color: var(--fg, #dfe3e9);
    border: 1px solid var(--border, #30353d);
    border-radius: 12px;
    padding: 18px 20px;
    box-shadow: 0 24px 64px rgba(0, 0, 0, 0.55);
  }
  .head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    margin-bottom: 12px;
  }
  h2 {
    font-size: 16px;
    font-weight: 700;
    margin: 0;
  }
  .x {
    background: none;
    border: none;
    color: var(--muted, #949bab);
    cursor: pointer;
    font-size: 14px;
  }
  .x:hover {
    color: var(--fg, #dfe3e9);
  }
  .tabs {
    display: flex;
    gap: 4px;
    border-bottom: 1px solid var(--border, #30353d);
    margin-bottom: 14px;
  }
  .tabs button {
    background: none;
    border: none;
    color: var(--muted, #949bab);
    padding: 7px 12px;
    font-size: 13px;
    cursor: pointer;
    border-bottom: 2px solid transparent;
    margin-bottom: -1px;
  }
  .tabs button.active {
    color: var(--fg, #dfe3e9);
    border-bottom-color: var(--accent, #4c9dff);
  }
  .lead {
    font-size: 13px;
    margin: 2px 0 14px;
  }
  .providers {
    display: flex;
    flex-direction: column;
    gap: 8px;
  }
  .provider {
    width: 100%;
    padding: 10px 12px;
    border-radius: 8px;
    border: 1px solid var(--border-strong, #3a4150);
    background: var(--panel-2, #262a31);
    color: var(--fg, #dfe3e9);
    font-size: 13px;
    font-weight: 600;
    cursor: pointer;
  }
  .provider:hover:not(:disabled) {
    filter: brightness(1.15);
  }
  .dev {
    margin-top: 14px;
    width: 100%;
    padding: 8px;
    border-radius: 8px;
    border: 1px dashed var(--border-strong, #3a4150);
    background: transparent;
    color: var(--muted, #949bab);
    font-size: 12px;
    cursor: pointer;
  }
  .who {
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 4px 0 14px;
    border-bottom: 1px solid var(--border, #30353d);
    margin-bottom: 14px;
  }
  .avatar {
    width: 38px;
    height: 38px;
    border-radius: 50%;
    background: var(--accent, #4c9dff);
    color: #071522;
    display: flex;
    align-items: center;
    justify-content: center;
    font-weight: 700;
    flex: none;
  }
  .email {
    font-weight: 600;
    font-size: 13px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .min0 {
    flex: 1;
    min-width: 0;
  }
  .sec-title {
    font-size: 11px;
    text-transform: uppercase;
    letter-spacing: 0.06em;
    color: var(--muted, #949bab);
    font-weight: 600;
    margin: 6px 0 6px;
  }
  .muted {
    color: var(--muted, #949bab);
  }
  .small {
    font-size: 11.5px;
  }
  .in {
    width: 100%;
    box-sizing: border-box;
    padding: 8px 10px;
    margin-bottom: 8px;
    border-radius: 7px;
    border: 1px solid var(--border-strong, #3a4150);
    background: var(--vance-bg, #17191d);
    color: var(--fg, #dfe3e9);
    font-size: 13px;
  }
  .in.tiny {
    width: 130px;
    margin: 0;
    padding: 5px 8px;
  }
  .primary {
    padding: 8px 16px;
    border-radius: 7px;
    border: none;
    background: var(--accent, #4c9dff);
    color: #071522;
    font-weight: 600;
    font-size: 13px;
    cursor: pointer;
  }
  .primary:disabled {
    opacity: 0.5;
    cursor: default;
  }
  .ghost {
    padding: 6px 12px;
    border-radius: 6px;
    border: 1px solid var(--border-strong, #3a4150);
    background: transparent;
    color: var(--muted, #949bab);
    font-size: 12px;
    cursor: pointer;
    flex: none;
  }
  .ghost:hover:not(:disabled) {
    color: var(--fg, #dfe3e9);
  }
  .ghost.tiny {
    padding: 4px 8px;
  }
  .toggle {
    display: flex;
    align-items: center;
    gap: 8px;
    font-size: 12.5px;
    margin-bottom: 12px;
    cursor: pointer;
  }
  .toggle input {
    accent-color: var(--accent, #4c9dff);
  }
  .devs {
    list-style: none;
    margin: 8px 0 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 6px;
  }
  .devs li {
    display: flex;
    align-items: center;
    gap: 10px;
    border: 1px solid var(--border, #30353d);
    border-radius: 8px;
    padding: 8px 10px;
  }
  .dname {
    font-weight: 600;
    font-size: 13px;
    display: flex;
    align-items: center;
    gap: 6px;
  }
  .badge {
    font-size: 10px;
    font-weight: 600;
    color: var(--accent, #4c9dff);
    border: 1px solid var(--accent, #4c9dff);
    border-radius: 999px;
    padding: 0 6px;
  }
  .warn-banner {
    border: 1px solid #f2c14e;
    color: #f2c14e;
    border-radius: 8px;
    padding: 8px 10px;
    font-size: 12px;
    margin-bottom: 12px;
  }
  .confirm {
    display: flex;
    align-items: center;
    gap: 6px;
    flex: none;
  }
  .danger {
    padding: 5px 10px;
    border-radius: 6px;
    border: 1px solid var(--err, #f2686f);
    background: transparent;
    color: var(--err, #f2686f);
    font-size: 12px;
    font-weight: 600;
    cursor: pointer;
  }
  .danger:disabled {
    opacity: 0.5;
    cursor: default;
  }
</style>
