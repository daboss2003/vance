<script lang="ts">
  /**
   * Add-on effects marketplace. Two tabs:
   *  - Browse (M3): search the hosted catalog, open an effect, install free ones
   *    (with a capability-consent prompt), and read/leave reviews.
   *  - Installed: the current library + Import (drag a .wasm in, or Select a file)
   *    and a Remove action.
   */
  import {
    market,
    closeMarket,
    importEffectPaths,
    uninstallEffect
  } from '$lib/effectsMarket.svelte';
  import {
    catalog,
    browse,
    loadMore,
    openDetail,
    closeDetail,
    install,
    buy,
    submitReview,
    report,
    CATEGORIES
  } from '$lib/catalog.svelte';
  import { openStudio } from '$lib/studio.svelte';
  import { pluginsFolder } from '$lib/ipc';
  import { open as openFileDialog } from '@tauri-apps/plugin-dialog';
  import { dialogFocus } from '$lib/dialog-focus';

  let tab = $state<'browse' | 'installed'>('browse');
  let folder = $state('');
  let reviewStars = $state(5);
  let reviewBody = $state('');

  $effect(() => {
    if (market.open && !folder)
      void pluginsFolder()
        .then((f) => (folder = f))
        .catch(() => {});
  });
  // Load the catalog the first time Browse is shown.
  $effect(() => {
    if (market.open && tab === 'browse' && !catalog.loaded && !catalog.loading) void browse();
  });

  function installedLocally(sku: string): boolean {
    return market.installed.some((p) => p.id === sku);
  }

  async function selectFile() {
    const picked = await openFileDialog({
      multiple: true,
      filters: [{ name: 'Vance effect', extensions: ['wasm'] }]
    });
    if (!picked) return;
    await importEffectPaths(Array.isArray(picked) ? picked : [picked]);
  }

  function ratingLabel(rating: number | null, count: number): string {
    if (!rating || count === 0) return 'No ratings yet';
    return `★ ${rating.toFixed(1)} · ${count}`;
  }
</script>

{#if market.open}
  <div
    class="scrim"
    role="dialog"
    use:dialogFocus
    aria-modal="true"
    aria-label="Add-on effects"
    tabindex="-1"
    onclick={(e) => {
      if (e.target === e.currentTarget) closeMarket();
    }}
    onkeydown={(e) => {
      // Esc closes this dialog only (not the app's global Esc = clear selection).
      if (e.key === 'Escape') {
        e.stopPropagation();
        closeMarket();
      }
    }}
  >
    <div class="card">
      <div class="head">
        <h2>Add-on effects</h2>
        <button class="x" onclick={closeMarket} aria-label="Close">✕</button>
      </div>

      <div class="tabs">
        <button class:active={tab === 'browse'} onclick={() => (tab = 'browse')}>Browse</button>
        <button class:active={tab === 'installed'} onclick={() => (tab = 'installed')}>
          Installed
        </button>
      </div>

      <!-- ============================ BROWSE ============================ -->
      {#if tab === 'browse'}
        {#if catalog.selected}
          {@const d = catalog.selected}
          <button class="back" onclick={closeDetail}>← Back to Browse</button>
          <div class="detail-head">
            <div class="min0">
              <div class="d-name">{d.display_name}</div>
              <div class="muted small">
                by {d.publisher} · {ratingLabel(d.rating, d.ratings_count)}
                {#if d.verified_seller}<span class="badge ok">✓ Verified</span>{/if}
                {#if d.new_seller}<span class="badge warn">New seller</span>{/if}
              </div>
              {#if d.dispute_rate > 0.05}
                <div class="warn-line">⚠ Some buyers have reported issues with this effect.</div>
              {/if}
            </div>
            {#if installedLocally(d.sku)}
              <button class="primary" disabled>Installed ✓</button>
            {:else if d.price_cents === 0 && d.installable}
              <button class="primary" disabled={catalog.busy} onclick={() => install(d)}>
                {catalog.busy ? 'Installing…' : 'Get (Free)'}
              </button>
            {:else if d.price_cents > 0}
              <button class="primary" disabled={catalog.busy} onclick={() => buy(d)}>
                {catalog.busy ? 'Opening…' : `Buy · $${(d.price_cents / 100).toFixed(2)}`}
              </button>
            {:else}
              <button class="primary" disabled>Unavailable</button>
            {/if}
          </div>

          {#if d.description}<p class="desc">{d.description}</p>{/if}

          {#if (d.capabilities ?? []).length > 0}
            <div class="sec-title">This effect can</div>
            <ul class="caps">
              {#each d.capabilities as c (c)}<li>{c}</li>{/each}
            </ul>
          {/if}

          <div class="sec-title">Reviews</div>
          {#if installedLocally(d.sku)}
            <div class="review-form">
              <div class="stars">
                {#each [1, 2, 3, 4, 5] as n (n)}
                  <button
                    class="star"
                    class:on={n <= reviewStars}
                    onclick={() => (reviewStars = n)}
                    aria-label={`${n} stars`}>★</button
                  >
                {/each}
              </div>
              <textarea
                class="rbody"
                bind:value={reviewBody}
                placeholder="Share what you think (optional)"
              ></textarea>
              <button
                class="primary"
                disabled={catalog.busy}
                onclick={async () => {
                  await submitReview(d.sku, reviewStars, reviewBody);
                  reviewBody = '';
                }}
              >
                Post review
              </button>
            </div>
          {:else}
            <div class="muted small">Install this effect to leave a review.</div>
          {/if}

          {#if catalog.reviewsLoading}
            <div class="muted small">Loading reviews…</div>
          {:else if catalog.reviews.length === 0}
            <div class="muted small">No reviews yet — be the first.</div>
          {:else}
            <ul class="reviews">
              {#each catalog.reviews as r, i (i)}
                <li>
                  <div class="rstars">{'★'.repeat(r.stars)}{'☆'.repeat(5 - r.stars)}</div>
                  {#if r.body}<div class="rtext">{r.body}</div>{/if}
                </li>
              {/each}
            </ul>
          {/if}
          <button class="report" onclick={() => report(d)}>Report this effect</button>
        {:else}
          <!-- Search + filters -->
          <div class="filters">
            <input
              class="search"
              type="text"
              placeholder="Search effects…"
              bind:value={catalog.query}
              onkeydown={(e) => e.key === 'Enter' && browse()}
            />
            <select class="sort" bind:value={catalog.sort} onchange={() => browse()}>
              <option value="popular">Popular</option>
              <option value="newest">Newest</option>
              <option value="name">Name</option>
            </select>
          </div>
          <div class="chips">
            <button
              class:on={catalog.category === ''}
              onclick={() => ((catalog.category = ''), browse())}
            >
              All
            </button>
            {#each CATEGORIES as c (c)}
              <button
                class:on={catalog.category === c}
                onclick={() => ((catalog.category = c), browse())}
              >
                {c}
              </button>
            {/each}
          </div>

          {#if catalog.loading && catalog.effects.length === 0}
            <div class="muted">Loading…</div>
          {:else if catalog.error && catalog.effects.length === 0}
            <div class="offline">
              <div class="offline-title">Marketplace unavailable</div>
              <div class="muted small">Couldn’t reach the effects store. It may be offline.</div>
              <div class="muted tiny">{catalog.error}</div>
              <button class="retry" disabled={catalog.loading} onclick={() => browse()}>
                {catalog.loading ? 'Retrying…' : 'Retry'}
              </button>
            </div>
          {:else if catalog.effects.length === 0}
            <div class="muted small">
              No effects found. {catalog.query || catalog.category
                ? 'Try a different search.'
                : 'The catalog is empty on this server.'}
            </div>
          {:else}
            <div class="grid">
              {#each catalog.effects as fx (fx.sku)}
                <button class="tile" onclick={() => openDetail(fx.sku)}>
                  <div class="t-name">{fx.display_name}</div>
                  <div class="muted t-pub">
                    {fx.publisher}
                    {#if fx.verified_seller}<span class="badge ok">✓</span>{/if}
                    {#if fx.new_seller}<span class="badge warn">New</span>{/if}
                  </div>
                  <div class="t-foot">
                    <span class="muted small">{ratingLabel(fx.rating, fx.ratings_count)}</span>
                    <span class="price"
                      >{fx.price_cents === 0
                        ? 'Free'
                        : `$${(fx.price_cents / 100).toFixed(2)}`}</span
                    >
                  </div>
                </button>
              {/each}
            </div>
            {#if catalog.hasMore}
              <button class="more" disabled={catalog.loading} onclick={loadMore}>
                {catalog.loading ? 'Loading…' : 'Load more'}
              </button>
            {/if}
          {/if}
        {/if}

        <!-- ============================ INSTALLED ============================ -->
      {:else}
        <button class="create" onclick={openStudio}>✨ Create your own effect (no code)</button>
        <div class="drop" class:hot={market.dragOver}>
          <div class="drop-title">Add an effect file</div>
          <div class="drop-sub">Drag an effect file (.wasm) here, or</div>
          <button class="select" onclick={selectFile} disabled={market.busy}>
            {market.busy ? 'Adding…' : 'Select a file…'}
          </button>
        </div>

        <div class="sec-title">On this device</div>
        {#if !market.loaded}
          <div class="muted">Loading…</div>
        {:else if market.installed.length === 0}
          <div class="muted small">Nothing yet — browse the catalog or add a file above.</div>
        {:else}
          <ul class="list">
            {#each market.installed as p (p.id)}
              <li>
                <div class="min0">
                  <div class="pname">{p.name}</div>
                  <div class="pmeta">{p.kind} · {p.author} · v{p.version}</div>
                </div>
                <button
                  class="rm"
                  onclick={() => uninstallEffect(p.id, p.name)}
                  disabled={market.busy}
                >
                  Remove
                </button>
              </li>
            {/each}
          </ul>
        {/if}

        {#if folder}
          <div class="folder">Installed effects are kept in <span class="mono">{folder}</span></div>
        {/if}
      {/if}
    </div>
  </div>
{/if}

<style>
  .scrim:focus {
    outline: none;
  }
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
    width: 640px;
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
  .filters {
    display: flex;
    gap: 8px;
    margin-bottom: 10px;
  }
  .search {
    flex: 1;
    padding: 8px 10px;
    border-radius: 7px;
    border: 1px solid var(--border-strong, #3a4150);
    background: var(--vance-bg, #17191d);
    color: var(--fg, #dfe3e9);
    font-size: 13px;
  }
  .sort {
    border-radius: 7px;
    border: 1px solid var(--border-strong, #3a4150);
    background: var(--vance-bg, #17191d);
    color: var(--fg, #dfe3e9);
    font-size: 13px;
    padding: 0 8px;
  }
  .chips {
    display: flex;
    flex-wrap: wrap;
    gap: 6px;
    margin-bottom: 14px;
  }
  .chips button {
    font-size: 11.5px;
    padding: 4px 10px;
    border-radius: 999px;
    border: 1px solid var(--border-strong, #3a4150);
    background: transparent;
    color: var(--muted, #949bab);
    cursor: pointer;
  }
  .chips button.on {
    color: #071522;
    background: var(--accent, #4c9dff);
    border-color: var(--accent, #4c9dff);
    font-weight: 600;
  }
  .grid {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(180px, 1fr));
    gap: 10px;
  }
  .tile {
    text-align: left;
    border: 1px solid var(--border, #30353d);
    border-radius: 10px;
    background: var(--panel-2, #262a31);
    padding: 12px;
    cursor: pointer;
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
  .tile:hover {
    border-color: var(--accent, #4c9dff);
  }
  .t-name {
    font-weight: 600;
    font-size: 13px;
  }
  .t-pub {
    font-size: 11px;
  }
  .t-foot {
    display: flex;
    align-items: center;
    justify-content: space-between;
    margin-top: 6px;
  }
  .price {
    font-size: 11.5px;
    font-weight: 600;
    color: var(--ok, #5ac37d);
  }
  .badge {
    font-size: 9.5px;
    font-weight: 700;
    border-radius: 999px;
    padding: 0 5px;
    margin-left: 4px;
    white-space: nowrap;
  }
  .badge.ok {
    color: var(--ok, #5ac37d);
    border: 1px solid var(--ok, #5ac37d);
  }
  .badge.warn {
    color: #f2c14e;
    border: 1px solid #f2c14e;
  }
  .warn-line {
    font-size: 11.5px;
    color: #f2c14e;
    margin-top: 4px;
  }
  .create {
    width: 100%;
    padding: 10px;
    margin-bottom: 12px;
    border-radius: 8px;
    border: 1px solid var(--accent, #4c9dff);
    background: color-mix(in srgb, var(--accent, #4c9dff) 12%, transparent);
    color: var(--fg, #dfe3e9);
    font-weight: 600;
    font-size: 13px;
    cursor: pointer;
  }
  .create:hover {
    background: color-mix(in srgb, var(--accent, #4c9dff) 20%, transparent);
  }
  .report {
    margin-top: 14px;
    background: none;
    border: none;
    color: var(--muted, #949bab);
    font-size: 11.5px;
    cursor: pointer;
    text-decoration: underline;
    padding: 0;
  }
  .report:hover {
    color: var(--err, #f2686f);
  }
  .more {
    margin: 12px auto 0;
    display: block;
    padding: 7px 16px;
    border-radius: 7px;
    border: 1px solid var(--border-strong, #3a4150);
    background: transparent;
    color: var(--fg, #dfe3e9);
    cursor: pointer;
    font-size: 12.5px;
  }
  .back {
    background: none;
    border: none;
    color: var(--accent, #4c9dff);
    cursor: pointer;
    font-size: 12.5px;
    padding: 0 0 10px;
  }
  .detail-head {
    display: flex;
    align-items: center;
    gap: 10px;
    justify-content: space-between;
  }
  .d-name {
    font-size: 15px;
    font-weight: 700;
  }
  .desc {
    font-size: 13px;
    line-height: 1.5;
    margin: 10px 0;
  }
  .caps {
    margin: 0 0 6px;
    padding-left: 18px;
    font-size: 12.5px;
    color: var(--fg, #dfe3e9);
  }
  .review-form {
    border: 1px solid var(--border, #30353d);
    border-radius: 8px;
    padding: 10px;
    margin-bottom: 12px;
  }
  .stars {
    display: flex;
    gap: 2px;
    margin-bottom: 6px;
  }
  .star {
    background: none;
    border: none;
    color: var(--muted, #949bab);
    font-size: 18px;
    cursor: pointer;
    padding: 0;
  }
  .star.on {
    color: #f2c14e;
  }
  .rbody {
    width: 100%;
    box-sizing: border-box;
    resize: vertical;
    min-height: 46px;
    border-radius: 7px;
    border: 1px solid var(--border-strong, #3a4150);
    background: var(--vance-bg, #17191d);
    color: var(--fg, #dfe3e9);
    font-size: 13px;
    padding: 7px 9px;
    margin-bottom: 8px;
  }
  .reviews {
    list-style: none;
    margin: 8px 0 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 8px;
  }
  .reviews li {
    border-top: 1px solid var(--border, #30353d);
    padding-top: 8px;
  }
  .rstars {
    color: #f2c14e;
    font-size: 12px;
  }
  .rtext {
    font-size: 12.5px;
    margin-top: 2px;
  }
  .drop {
    border: 2px dashed var(--border-strong, #3a4150);
    border-radius: 10px;
    padding: 22px 16px;
    text-align: center;
    transition:
      border-color 0.15s,
      background 0.15s;
  }
  .drop.hot {
    border-color: var(--accent, #4c9dff);
    background: color-mix(in srgb, var(--accent, #4c9dff) 10%, transparent);
  }
  .drop-title {
    font-weight: 600;
    font-size: 14px;
  }
  .drop-sub {
    font-size: 12px;
    color: var(--muted, #949bab);
    margin: 4px 0 10px;
  }
  .select {
    cursor: pointer;
    padding: 7px 16px;
    border-radius: 7px;
    border: none;
    background: var(--accent, #4c9dff);
    color: #071522;
    font-weight: 600;
    font-size: 13px;
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
    flex: none;
  }
  .primary:disabled {
    opacity: 0.55;
    cursor: default;
  }
  .sec-title {
    font-size: 11px;
    text-transform: uppercase;
    letter-spacing: 0.06em;
    color: var(--muted, #949bab);
    font-weight: 600;
    margin: 16px 0 7px;
  }
  .muted {
    font-size: 12.5px;
    color: var(--muted, #949bab);
  }
  .small {
    font-size: 11.5px;
  }
  .tiny {
    font-size: 10.5px;
    opacity: 0.75;
    word-break: break-word;
  }
  .offline {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 6px;
    text-align: center;
    padding: 28px 18px;
    border: 1px solid var(--border, #30353d);
    border-radius: 10px;
    background: var(--vance-bg, #17191d);
  }
  .offline-title {
    font-size: 13px;
    font-weight: 600;
    color: var(--fg, #dfe3e9);
  }
  .retry {
    margin-top: 8px;
    padding: 6px 18px;
    border-radius: 7px;
    border: 1px solid var(--border-strong, #3a4150);
    background: var(--panel-2, #262a31);
    color: var(--fg, #dfe3e9);
    font-size: 12px;
    cursor: pointer;
  }
  .retry:disabled {
    opacity: 0.5;
    cursor: default;
  }
  .min0 {
    min-width: 0;
    flex: 1;
  }
  .list {
    display: flex;
    flex-direction: column;
    gap: 6px;
    margin: 0;
    padding: 0;
    list-style: none;
  }
  .list li {
    display: flex;
    align-items: center;
    gap: 10px;
    border: 1px solid var(--border, #30353d);
    border-radius: 8px;
    padding: 8px 10px;
  }
  .pname {
    font-weight: 600;
    font-size: 13px;
  }
  .pmeta {
    font-size: 11px;
    color: var(--muted, #949bab);
  }
  .rm {
    cursor: pointer;
    padding: 4px 10px;
    border-radius: 6px;
    border: 1px solid var(--border-strong, #3a4150);
    background: transparent;
    color: var(--muted, #949bab);
    font-size: 12px;
    flex: none;
  }
  .rm:hover {
    color: var(--err, #f2686f);
    border-color: var(--err, #f2686f);
  }
  .folder {
    margin-top: 12px;
    font-size: 11px;
    color: var(--muted, #949bab);
  }
  .mono {
    font-family: ui-monospace, 'SF Mono', Menlo, monospace;
    font-size: 0.92em;
    background: var(--panel-2, #262a31);
    padding: 0 4px;
    border-radius: 4px;
  }
</style>
