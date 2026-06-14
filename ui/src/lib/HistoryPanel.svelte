<script lang="ts">
  /**
   * GOAT-1 — Edit history + branching ("Git for edits").
   *
   * Renders the current sequence's edit-history tree. Each edit is a
   * node; undoing then editing forks a new branch instead of discarding
   * the old one, so alternate cuts stay reachable. Click any node to jump
   * to it (switch branch / restore version); name a node as a checkpoint.
   */

  import {
    editHistory,
    checkpointHistory,
    gotoHistory,
    type HistoryNode
  } from '$lib/ipc';

  let nodes = $state<HistoryNode[]>([]);
  let busy = $state(false);
  let error = $state<string | null>(null);

  /** Depth of each node from the root, for indentation. */
  let depth = $derived.by(() => {
    const byId = new Map(nodes.map((n) => [n.id, n]));
    const d = new Map<number, number>();
    for (const n of nodes) {
      let k = 0;
      let p = n.parent;
      while (p !== null && p !== undefined) {
        k += 1;
        p = byId.get(p)?.parent ?? null;
      }
      d.set(n.id, k);
    }
    return d;
  });

  export async function refresh() {
    busy = true;
    error = null;
    try {
      nodes = await editHistory();
    } catch (e) {
      error = String(e);
    } finally {
      busy = false;
    }
  }

  async function jump(id: number) {
    busy = true;
    try {
      await gotoHistory(id);
      await refresh();
    } catch (e) {
      error = String(e);
    } finally {
      busy = false;
    }
  }

  async function checkpoint() {
    const name = window.prompt('Checkpoint name (e.g. "v2 — punchier open")');
    if (!name) return;
    try {
      await checkpointHistory(name);
      await refresh();
    } catch (e) {
      error = String(e);
    }
  }

  $effect(() => {
    void refresh();
  });
</script>

<div class="history">
  <div class="head">
    <span>History</span>
    <button onclick={checkpoint} disabled={busy} title="Name the current point">
      ＋ Checkpoint
    </button>
  </div>
  {#if error}<div class="err">{error}</div>{/if}
  <ul>
    {#each nodes as n (n.id)}
      <li
        class:current={n.is_current}
        class:branch={n.children.length > 1}
        style="padding-left: {(depth.get(n.id) ?? 0) * 12 + 6}px"
      >
        <button class="node" onclick={() => jump(n.id)} disabled={busy}>
          <span class="label">{n.id === 0 ? 'Open' : n.label || 'Edit'}</span>
          {#if n.name}<span class="name">★ {n.name}</span>{/if}
          {#if n.children.length === 0}<span class="tip">tip</span>{/if}
          {#if n.is_current}<span class="here">●</span>{/if}
        </button>
      </li>
    {/each}
  </ul>
</div>

<style>
  .history {
    display: flex;
    flex-direction: column;
    font-size: 12px;
    color: var(--fg, #ddd);
  }
  .head {
    display: flex;
    justify-content: space-between;
    align-items: center;
    padding: 4px 6px;
    font-weight: 600;
    border-bottom: 1px solid var(--border, #333);
  }
  .head button {
    font-size: 11px;
    cursor: pointer;
  }
  ul {
    list-style: none;
    margin: 0;
    padding: 0;
    overflow-y: auto;
  }
  li {
    border-left: 2px solid transparent;
  }
  li.current {
    border-left-color: var(--accent, #4a9eff);
    background: rgba(74, 158, 255, 0.08);
  }
  li.branch .label::after {
    content: ' ⑂';
    opacity: 0.6;
  }
  .node {
    display: flex;
    gap: 6px;
    align-items: center;
    width: 100%;
    background: none;
    border: none;
    color: inherit;
    text-align: left;
    cursor: pointer;
    padding: 3px 4px;
  }
  .node:hover {
    background: rgba(255, 255, 255, 0.05);
  }
  .name {
    color: var(--accent, #4a9eff);
  }
  .tip {
    opacity: 0.5;
    font-size: 10px;
  }
  .here {
    margin-left: auto;
    color: var(--accent, #4a9eff);
  }
  .err {
    color: #ff6b6b;
    padding: 4px 6px;
  }
</style>
