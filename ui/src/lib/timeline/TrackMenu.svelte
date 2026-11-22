<script lang="ts">
  /**
   * Track header context menu (M1, ADR 0007): rename, add, move, show / hide,
   * lock / unlock and delete tracks. Opened by right-clicking a track header (or the empty
   * header area) or with Shift+F10 on the focused timeline.
   *
   * Video rows are drawn top layer first, so "up" for a video track is a
   * HIGHER index in its kind group while "up" for an audio track is a lower
   * one. Positions passed to the engine are indices within the kind group.
   */

  import { onMount, tick } from 'svelte';
  import type { TrackContents } from '$lib/ipc';
  import { projectState } from '$lib/project.svelte';
  import { selectionState } from '$lib/selection.svelte';
  import { openDialog } from '$lib/dialog.svelte';
  import { canDeleteTrack, lockActionLabel, LOCKED_DELETE_HINT } from '$lib/track-lock';
  import { menuFocusPlan, stepFocus } from './menu-focus';
  import { exportRun } from '$lib/export-run.svelte';
  import { EXPORT_BUSY_HINT } from '$lib/export-busy';

  let {
    trackId,
    x,
    y,
    onClose,
    onRename
  }: {
    /** Track whose header was clicked; `null` for the empty header area. */
    trackId: string | null;
    /** Viewport coordinates to open at. */
    x: number;
    y: number;
    onClose: () => void;
    /** Start the timeline's inline rename of `trackId`. */
    onRename: (trackId: string) => void;
  } = $props();

  const uid = $props.id();

  type Item = {
    id: string;
    label: string;
    run: () => unknown;
    disabled?: boolean;
    /** Shown under a disabled item: why it's unavailable. */
    hint?: string;
    danger?: boolean;
    separatorBefore?: boolean;
  };

  const seq = $derived(projectState.sequence);
  const track = $derived(trackId ? (seq?.tracks.find((t) => t.id === trackId) ?? null) : null);
  const group = $derived(track && seq ? seq.tracks.filter((t) => t.kind === track.kind) : []);
  const gi = $derived(track ? group.findIndex((t) => t.id === track.id) : -1);

  // An export holds the engine: every item is paused (one hint for the menu).
  const paused = $derived(exportRun.busy);

  const items = $derived.by<Item[]>(() => {
    if (!track) {
      return [
        { id: 'add-video', label: 'Add video track', run: () => projectState.addTrack('video') },
        { id: 'add-audio', label: 'Add audio track', run: () => projectState.addTrack('audio') }
      ];
    }
    const t = track;
    const last = group.length - 1;
    const deletable = canDeleteTrack(t);
    const del: Item = {
      id: 'delete',
      label: 'Delete track',
      danger: true,
      separatorBefore: true,
      disabled: !deletable,
      hint: deletable ? undefined : LOCKED_DELETE_HINT,
      run: () => deleteTrack(t)
    };
    const lock: Item = {
      id: 'lock',
      label: lockActionLabel(t.locked),
      run: () => projectState.setTrackLocked(t.id, !t.locked)
    };
    const rename: Item = { id: 'rename', label: 'Rename…', run: () => onRename(t.id) };
    if (t.kind === 'video') {
      return [
        rename,
        {
          id: 'add-above',
          label: 'Add video track above',
          separatorBefore: true,
          run: () => projectState.addTrack('video', gi + 1)
        },
        {
          id: 'add-below',
          label: 'Add video track below',
          run: () => projectState.addTrack('video', gi)
        },
        { id: 'add-audio', label: 'Add audio track', run: () => projectState.addTrack('audio') },
        {
          id: 'up',
          label: 'Move up',
          separatorBefore: true,
          disabled: gi >= last,
          run: () => projectState.moveTrack(t.id, gi + 1)
        },
        {
          id: 'down',
          label: 'Move down',
          disabled: gi <= 0,
          run: () => projectState.moveTrack(t.id, gi - 1)
        },
        {
          id: 'visibility',
          label: t.enabled ? 'Hide track' : 'Show track',
          separatorBefore: true,
          run: () => projectState.setTrackEnabled(t.id, !t.enabled)
        },
        lock,
        del
      ];
    }
    return [
      rename,
      {
        id: 'add-above',
        label: 'Add audio track above',
        separatorBefore: true,
        run: () => projectState.addTrack('audio', gi)
      },
      {
        id: 'add-below',
        label: 'Add audio track below',
        run: () => projectState.addTrack('audio', gi + 1)
      },
      { id: 'add-video', label: 'Add video track', run: () => projectState.addTrack('video') },
      {
        id: 'up',
        label: 'Move up',
        separatorBefore: true,
        disabled: gi <= 0,
        run: () => projectState.moveTrack(t.id, gi - 1)
      },
      {
        id: 'down',
        label: 'Move down',
        disabled: gi >= last,
        run: () => projectState.moveTrack(t.id, gi + 1)
      },
      { ...lock, separatorBefore: true },
      del
    ];
  });

  async function deleteTrack(t: TrackContents) {
    const clipIds = new Set(t.clips.map((c) => c.id));
    if (clipIds.size > 0) {
      const choice = await openDialog({
        title: `Delete track ${t.name}?`,
        body:
          clipIds.size === 1
            ? 'Its clip will be deleted too. You can undo this.'
            : `Its ${clipIds.size} clips will be deleted too. You can undo this.`,
        actions: [
          { label: 'Delete track', value: 'delete', kind: 'danger' },
          { label: 'Cancel', value: 'cancel' }
        ]
      });
      if (choice !== 'delete') return;
    }
    if (await projectState.removeTrack(t.id)) {
      selectionState.replace(selectionState.ids.filter((id) => !clipIds.has(id)));
    }
  }

  function choose(item: Item) {
    if (item.disabled || exportRun.busy) return;
    onClose();
    void item.run();
  }

  // The track went away under the open menu (undo from the menu bar, say).
  $effect(() => {
    if (trackId && !track) onClose();
  });

  let menuEl = $state<HTMLDivElement | null>(null);
  let left = $state(0);
  let top = $state(0);
  let placed = $state(false);
  const previousFocus =
    document.activeElement instanceof HTMLElement ? document.activeElement : null;

  // Unavailable items are `aria-disabled`, not `disabled`, so arrow keys still
  // land on them and their hint is read out (menu-focus.ts); activating one
  // does nothing (see `choose`).
  function menuItems(): HTMLButtonElement[] {
    return menuEl
      ? Array.from(menuEl.querySelectorAll<HTMLButtonElement>('[role="menuitem"]'))
      : [];
  }

  function focusPlan() {
    const list = menuItems();
    return { list, ...menuFocusPlan(list.map((b) => b.getAttribute('aria-disabled') === 'true')) };
  }

  function focusStep(delta: number) {
    const { list, order } = focusPlan();
    const next = stepFocus(order, list.indexOf(document.activeElement as HTMLButtonElement), delta);
    list[next]?.focus();
  }

  function onKeydown(e: KeyboardEvent) {
    // The menu owns the keyboard while open: Space must not start playback
    // and Delete must not remove the selected clips behind it.
    e.stopPropagation();
    switch (e.key) {
      case 'ArrowDown':
        e.preventDefault();
        focusStep(1);
        break;
      case 'ArrowUp':
        e.preventDefault();
        focusStep(-1);
        break;
      case 'Home':
        e.preventDefault();
        menuItems()[0]?.focus();
        break;
      case 'End': {
        e.preventDefault();
        const list = menuItems();
        list[list.length - 1]?.focus();
        break;
      }
      case 'Escape':
      case 'Tab':
        e.preventDefault();
        onClose();
        break;
    }
  }

  onMount(() => {
    if (menuEl) {
      // Intentional: clamped into the window only. The menu opens from the
      // timeline header in the bottom-left of the window, so even when it is
      // pushed up it stays over HTML chrome (timeline, page bar, dock and
      // transport rows) and never under the native video viewports, which
      // paint above the webview (ADR 0005).
      const r = menuEl.getBoundingClientRect();
      left = Math.max(4, Math.min(x, window.innerWidth - r.width - 4));
      top = Math.max(4, Math.min(y, window.innerHeight - r.height - 4));
    }
    placed = true;
    // After `placed` reaches the DOM: a visibility:hidden button can't take focus.
    void tick().then(() => {
      const { list, initial } = focusPlan();
      list[initial]?.focus({ preventScroll: true });
    });
    const close = () => onClose();
    window.addEventListener('resize', close);
    window.addEventListener('blur', close);
    return () => {
      window.removeEventListener('resize', close);
      window.removeEventListener('blur', close);
      if (previousFocus && previousFocus.isConnected) previousFocus.focus({ preventScroll: true });
    };
  });
</script>

<!-- Click-away layer: any press outside the menu closes it. -->
<div
  class="fixed inset-0 z-[3399]"
  role="presentation"
  onpointerdown={onClose}
  oncontextmenu={(e) => {
    e.preventDefault();
    onClose();
  }}
></div>
<div
  bind:this={menuEl}
  role="menu"
  tabindex="-1"
  aria-label={track ? `Track ${track.name}` : 'Tracks'}
  class="bg-vance-panel border-vance-border fixed z-[3400] min-w-[12rem] rounded-md border py-1 text-xs shadow-xl"
  style:left="{left}px"
  style:top="{top}px"
  style:visibility={placed ? 'visible' : 'hidden'}
  onkeydown={onKeydown}
  oncontextmenu={(e) => e.preventDefault()}
>
  {#if track}
    <div
      class="text-vance-muted px-3 pb-1 pt-0.5 text-[10px] uppercase tracking-wider"
      aria-hidden="true"
    >
      {track.name}
    </div>
  {/if}
  {#if paused}
    <!-- One hint for the whole menu: every item is paused during an export. -->
    <div
      id="{uid}-paused"
      class="text-vance-muted px-3 pb-1 text-[10px] leading-snug"
      aria-hidden="true"
    >
      {EXPORT_BUSY_HINT}
    </div>
  {/if}
  {#each items as item (item.id)}
    {#if item.separatorBefore}
      <div role="separator" class="bg-vance-border my-1 h-px"></div>
    {/if}
    <button
      type="button"
      role="menuitem"
      tabindex="-1"
      aria-disabled={item.disabled || paused ? 'true' : undefined}
      class="block w-full px-3 py-1 text-left outline-none hover:bg-vance-panel-2 focus:bg-vance-panel-2 aria-disabled:cursor-default aria-disabled:opacity-40 aria-disabled:hover:bg-transparent {item.danger
        ? 'text-[var(--color-vance-err)]'
        : 'text-vance-fg'}"
      aria-describedby={paused
        ? `${uid}-paused`
        : item.disabled && item.hint
          ? `${uid}-${item.id}-hint`
          : undefined}
      onclick={() => choose(item)}
    >
      {item.label}
    </button>
    {#if item.disabled && item.hint && !paused}
      <!-- Outside the button so the dimmed disabled style doesn't fade it;
           read out as the item's description. -->
      <div
        id="{uid}-{item.id}-hint"
        class="text-vance-muted px-3 pb-1 text-[10px] leading-snug"
        aria-hidden="true"
      >
        {item.hint}
      </div>
    {/if}
  {/each}
</div>
