// Catalog / Browse state (M3). Backs the "Browse" tab of the effects
// marketplace: search + cards + item detail + free install + reviews.

import {
  catalogBrowse,
  catalogDetail,
  catalogReviews,
  catalogInstall,
  catalogReview,
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
  try {
    const r = await catalogBrowse({
      q: catalog.query || undefined,
      category: catalog.category || undefined,
      sort: catalog.sort,
      page: catalog.page
    });
    catalog.effects = reset ? r.effects : [...catalog.effects, ...r.effects];
    catalog.hasMore = r.has_more;
    catalog.loaded = true;
  } catch (e) {
    toast.error(`Couldn’t load the effects catalog.\n${String(e)}`);
  } finally {
    catalog.loading = false;
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
