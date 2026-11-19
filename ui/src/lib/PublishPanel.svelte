<script lang="ts">
  /**
   * GOAT-11 — direct publish. Connect the platform via OAuth (using the
   * user's OWN registered app credentials — Vance can't embed platform
   * keys), then upload a rendered file. YouTube upload is fully wired;
   * Vimeo/TikTok OAuth connects (their uploaders are the next step).
   */
  import { publishAuthorized, startOauth, awaitOauth, publishVideo } from '$lib/ipc';

  const PLATFORMS = ['YouTube', 'Vimeo', 'TikTok'];
  let platform = $state('YouTube');
  let clientId = $state('');
  let clientSecret = $state('');
  let redirectUri = $state('http://127.0.0.1:8723/callback');

  let authorized = $state(false);
  let busy = $state(false);
  let status = $state<string | null>(null);
  let error = $state<string | null>(null);
  let authUrl = $state<string | null>(null);

  // Publish fields.
  let filePath = $state('');
  let title = $state('');
  let description = $state('');
  let tags = $state('');
  let privacy = $state('unlisted');

  $effect(() => {
    void publishAuthorized(platform)
      .then((v) => (authorized = v))
      .catch(() => {});
  });

  async function connect() {
    if (!clientId) {
      error = 'Enter your OAuth Client ID (from the platform developer console).';
      return;
    }
    busy = true;
    error = null;
    status = 'Opening the consent page in your browser…';
    authUrl = null;
    try {
      authUrl = await startOauth({
        platform,
        clientId,
        clientSecret: clientSecret || null,
        redirectUri
      });
      status = 'Waiting for you to approve in the browser…';
      authorized = await awaitOauth();
      status = authorized ? `${platform} connected.` : null;
    } catch (e) {
      error = String(e);
      status = null;
    } finally {
      busy = false;
    }
  }

  async function publish() {
    if (!filePath || !title) {
      error = 'Choose a rendered file and a title.';
      return;
    }
    busy = true;
    error = null;
    status = 'Uploading…';
    try {
      const url = await publishVideo({
        platform,
        filePath,
        title,
        description,
        tags: tags
          .split(',')
          .map((t) => t.trim())
          .filter(Boolean),
        privacy
      });
      status = `Published: ${url}`;
    } catch (e) {
      error = String(e);
      status = null;
    } finally {
      busy = false;
    }
  }
</script>

<div class="panel">
  <div class="head">Publish</div>
  {#if error}<div class="err">{error}</div>{/if}
  {#if status}<div class="status">{status}</div>{/if}

  <div class="row">
    <select bind:value={platform} disabled={busy}>
      {#each PLATFORMS as p (p)}
        <option value={p}>{p}</option>
      {/each}
    </select>
    <span class={authorized ? 'ok' : 'muted'}>{authorized ? '● connected' : 'not connected'}</span>
  </div>

  {#if !authorized}
    <div class="setup">
      <div class="hint">
        Register an OAuth app in the {platform} developer console and add this redirect URI.
      </div>
      <input type="text" placeholder="Client ID" bind:value={clientId} disabled={busy} />
      <input
        type="password"
        placeholder="Client Secret (if required)"
        bind:value={clientSecret}
        disabled={busy}
      />
      <input type="text" placeholder="Redirect URI" bind:value={redirectUri} disabled={busy} />
      <button disabled={busy} onclick={connect}>Connect {platform}</button>
      {#if authUrl}
        <div class="hint">
          If the browser didn't open: <a href={authUrl} target="_blank" rel="noreferrer"
            >open consent page</a
          >
        </div>
      {/if}
    </div>
  {:else}
    <div class="upload">
      <input
        type="text"
        placeholder="Rendered file path (.mp4)"
        bind:value={filePath}
        disabled={busy}
      />
      <input type="text" placeholder="Title" bind:value={title} disabled={busy} />
      <input type="text" placeholder="Description" bind:value={description} disabled={busy} />
      <input type="text" placeholder="Tags (comma-separated)" bind:value={tags} disabled={busy} />
      <div class="row">
        <select bind:value={privacy} disabled={busy}>
          <option value="unlisted">Unlisted</option>
          <option value="private">Private</option>
          <option value="public">Public</option>
        </select>
        <button disabled={busy} onclick={publish}>Publish to {platform}</button>
      </div>
    </div>
  {/if}
</div>

<style>
  .panel {
    display: flex;
    flex-direction: column;
    gap: 5px;
    padding: 6px;
    font-size: 12px;
    color: var(--fg, #ddd);
  }
  .head {
    font-weight: 600;
    border-bottom: 1px solid var(--border, #333);
    padding-bottom: 4px;
  }
  .row {
    display: flex;
    align-items: center;
    gap: 5px;
  }
  .setup,
  .upload {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
  input,
  select {
    min-width: 0;
  }
  .row select {
    flex: 1;
  }
  button {
    cursor: pointer;
  }
  .ok {
    color: var(--ok);
  }
  .muted {
    opacity: 0.6;
  }
  .hint {
    opacity: 0.55;
    font-size: 10px;
  }
  .err {
    color: #ff6b6b;
    font-size: 11px;
  }
  .status {
    color: var(--ok);
    font-size: 11px;
    word-break: break-all;
  }
</style>
