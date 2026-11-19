// Catalog / Browse state (M3). Backs the "Browse" tab of the effects
// marketplace: search + cards + item detail + free install + reviews.

import {
  catalogBrowse,
  catalogDetail,
  catalogReviews,
  catalogInstall,
  catalogReview,
  catalogReport,
  paymentCheckout,
  type CatalogCard,
  type CatalogDetail,
  type CatalogReview
} from '$lib/ipc';
import { toast } from '$lib/toast.svelte';
import { openDialog } from '$lib/dialog.svelte';
import { refreshInstalled } from '$lib/effectsMarket.svelte';

interface CatalogState {
  query: string;
  category: string;
  sort: string;
  effects: CatalogCard[];
  loading: boolean;
  hasMore: boolean;
  page: number;
  loaded: boolean;
  error: string | null;
  selected: CatalogDetail | null;
  reviews: CatalogReview[];
  reviewsLoading: boolean;
  installedSkus: string[];
  busy: boolean;
}

export const catalog = $state<CatalogState>({
  query: '',
  category: '',
  sort: 'popular',
  effects: [],
  loading: false,
  hasMore: false,
  page: 0,
  loaded: false,
  error: null,
  selected: null,
  reviews: [],
  reviewsLoading: false,
  installedSkus: [],
  busy: false
});

export const CATEGORIES = [
  'Color',
  'Blur/Sharpen',
  'Stylize',
  'Keying',
  'Transitions',
  'Generators',
  'Distortion',
  'Audio-reactive'
];

function prettyCap(cap: string): string {
  switch (cap) {
    case 'network':
      return 'Network access';
    case 'file-read':
      return 'Read files on your computer';
    case 'file-write':
      return 'Write files on your computer';
    default:
      return cap;
  }
}

export async function browse(reset = true): Promise<void> {
  if (reset) {
    catalog.page = 0;
    catalog.effects = [];
  }
  catalog.loading = true;
  catalog.error = null;
  try {
    const r = await catalogBrowse({
      q: catalog.query || undefined,
      category: catalog.category || undefined,
      sort: catalog.sort,
      page: catalog.page
    });
    catalog.effects = reset ? r.effects : [...catalog.effects, ...r.effects];
    catalog.hasMore = r.has_more;
    catalog.error = null;
  } catch (e) {
    // Surface inline (see EffectsMarketplace) — NOT as a toast. The auto-load
    // effect keys off `loaded`, set in `finally`, so one failure can't spiral
    // into a retry loop that buries the app in toasts.
    catalog.error = String(e);
  } finally {
    catalog.loading = false;
    catalog.loaded = true;
  }
}

export async function loadMore(): Promise<void> {
  catalog.page += 1;
  await browse(false);
}

export async function openDetail(sku: string): Promise<void> {
  catalog.selected = null;
  catalog.reviews = [];
  catalog.reviewsLoading = true;
  try {
    catalog.selected = await catalogDetail(sku);
  } catch (e) {
    toast.error(String(e));
    return;
  }
  try {
    const r = await catalogReviews(sku);
    catalog.reviews = r.reviews ?? [];
  } catch {
    /* reviews are best-effort */
  } finally {
    catalog.reviewsLoading = false;
  }
}

export function closeDetail(): void {
  catalog.selected = null;
}

export async function install(d: CatalogDetail): Promise<void> {
  // Consent for any elevated capability (spec §9).
  const elevated = (d.capabilities ?? []).filter((c) => c && c !== 'gpu' && c !== 'cpu-only');
  if (elevated.length > 0) {
    const ok = await openDialog({
      title: `Install “${d.display_name}”?`,
      body: 'This effect is requesting permission to:',
      bullets: elevated.map(prettyCap),
      actions: [
        { label: 'Install', value: 'ok', kind: 'primary' },
        { label: 'Cancel', value: 'cancel' }
      ]
    });
    if (ok !== 'ok') return;
  }
  catalog.busy = true;
  try {
    await catalogInstall(d.sku);
    catalog.installedSkus = [...catalog.installedSkus, d.sku];
    toast.success(`Installed “${d.display_name}”.`);
    await refreshInstalled();
  } catch (e) {
    toast.error(`Couldn’t install “${d.display_name}”.\n${String(e)}`);
  } finally {
    catalog.busy = false;
  }
}

/** Buy a paid effect: opens Stripe checkout in the browser. The webhook grants
 *  it on payment, and it appears in My Effects (spec §8). */
export async function buy(d: CatalogDetail): Promise<void> {
  catalog.busy = true;
  try {
    await paymentCheckout(d.sku);
    toast.info(
      'Finish your purchase in the browser — the effect appears in My Effects once it’s paid.'
    );
  } catch (e) {
    toast.error(String(e));
  } finally {
    catalog.busy = false;
  }
}

export async function submitReview(sku: string, stars: number, body: string): Promise<void> {
  catalog.busy = true;
  try {
    await catalogReview(sku, stars, body || undefined);
    toast.success('Thanks for the review!');
    const r = await catalogReviews(sku);
    catalog.reviews = r.reviews ?? [];
    if (catalog.selected && catalog.selected.sku === sku) {
      catalog.selected = { ...catalog.selected, rating: r.rating, ratings_count: r.count };
    }
  } catch (e) {
    toast.error(String(e));
  } finally {
    catalog.busy = false;
  }
}

export function isInstalled(sku: string): boolean {
  return catalog.installedSkus.includes(sku);
}

/** Report a bad/mis-described/impersonating effect (spec §8a). */
export async function report(d: CatalogDetail): Promise<void> {
  const reason = await openDialog({
    title: `Report “${d.display_name}”?`,
    body: 'What’s wrong with this effect?',
    actions: [
      { label: 'It doesn’t work', value: 'broken' },
      { label: 'Not as described', value: 'misleading' },
      { label: 'Impersonation / stolen', value: 'impersonation', kind: 'danger' },
      { label: 'Cancel', value: 'cancel' }
    ]
  });
  if (!reason || reason === 'cancel') return;
  try {
    await catalogReport(d.sku, reason);
    toast.success('Thanks — we’ll review this effect.');
  } catch (e) {
    toast.error(String(e));
  }
}
