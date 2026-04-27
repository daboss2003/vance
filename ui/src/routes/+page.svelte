<script lang="ts">
  import { onMount } from 'svelte';
  import { invoke } from '@tauri-apps/api/core';

  let backendVersion = $state<string>('checking…');

  onMount(async () => {
    try {
      backendVersion = await invoke<string>('backend_version');
    } catch (e) {
      backendVersion = `error: ${e}`;
    }
  });
</script>

<main class="flex h-screen flex-col items-center justify-center gap-4 p-8 text-center">
  <h1 class="text-3xl font-semibold">Vance — Phase 0</h1>
  <p class="text-vance-muted max-w-prose">
    Foundations are in place. Workspace builds. Tauri host runs. Frontend talks to Rust.
  </p>
  <p class="font-mono text-sm">
    backend: <span class="text-vance-accent">{backendVersion}</span>
  </p>
  <p class="text-vance-muted text-xs">
    See <code>PLAN.md</code> for the roadmap.
  </p>
</main>
