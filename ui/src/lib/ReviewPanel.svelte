<script lang="ts">
  /**
   * GOAT-gated (collaboration) — timeline review. Add comments anchored to
   * the playhead, resolve them, and reply in a thread. Comments live in the
   * project bundle, so sharing the `.vance` file IS the review package.
   * Real-time cloud sync is a separate (server-gated) layer.
   */
  import {
    reviewComments,
    addReviewComment,
    removeReviewComment,
    resolveReviewComment,
    replyReviewComment,
    currentTimelineSeconds,
    decodeSequenceAt,
    type ReviewComment
  } from '$lib/ipc';
  import { reportableError } from '$lib/toast.svelte';
  import { refreshError } from '$lib/export-busy';
  import { exportRun } from '$lib/export-run.svelte';

  let comments = $state<ReviewComment[]>([]);
  let author = $state('Me');
  let draft = $state('');
  let replyDrafts = $state<Record<string, string>>({});
  let showResolved = $state(false);
  let busy = $state(false);
  let error = $state<string | null>(null);

  let open = $derived(comments.filter((c) => !c.resolved).length);
  let visible = $derived(showResolved ? comments : comments.filter((c) => !c.resolved));

  // Also after an export, which turns the list away while it runs.
  $effect(() => {
    if (!exportRun.busy) void refresh();
  });

  async function refresh() {
    try {
      comments = await reviewComments();
    } catch (e) {
      error = refreshError(e);
    }
  }

  async function act<T>(fn: () => Promise<T>): Promise<T | undefined> {
    busy = true;
    error = null;
    try {
      return await fn();
    } catch (e) {
      error = reportableError(e);
    } finally {
      busy = false;
    }
  }

  async function add() {
    if (!draft.trim()) return;
    const sec = (await currentTimelineSeconds().catch(() => 0)) ?? 0;
    const ms = Math.max(0, Math.round(sec * 1000));
    const ok = await act(() => addReviewComment(ms, author.trim() || 'Me', draft.trim()));
    if (ok !== undefined) {
      draft = '';
      await refresh();
    }
  }

  async function toggleResolved(c: ReviewComment) {
    await act(() => resolveReviewComment(c.id, !c.resolved));
    await refresh();
  }
  async function del(c: ReviewComment) {
    await act(() => removeReviewComment(c.id));
    await refresh();
  }
  async function sendReply(c: ReviewComment) {
    const text = (replyDrafts[c.id] ?? '').trim();
    if (!text) return;
    await act(() => replyReviewComment(c.id, author.trim() || 'Me', text));
    replyDrafts[c.id] = '';
    await refresh();
  }
  async function jump(c: ReviewComment) {
    await act(() => decodeSequenceAt(c.timeline_ms / 1000));
  }

  function fmtTime(ms: number): string {
    const s = Math.floor(ms / 1000);
    const m = Math.floor(s / 60);
    const rem = s % 60;
    const cs = Math.floor((ms % 1000) / 10);
    return `${m}:${String(rem).padStart(2, '0')}.${String(cs).padStart(2, '0')}`;
  }
  function fmtWhen(unix: number): string {
    if (!unix) return '';
    try {
      return new Date(unix * 1000).toLocaleString();
    } catch {
      return '';
    }
  }
</script>

<div class="panel">
  <div class="head">
    <span>Review</span>
    {#if open > 0}<span class="badge">{open} open</span>{/if}
  </div>
  {#if error}<div class="err">{error}</div>{/if}

  <div class="compose">
    <input class="author" type="text" placeholder="Name" bind:value={author} disabled={busy} />
    <textarea placeholder="Comment at the playhead…" rows="2" bind:value={draft} disabled={busy}
    ></textarea>
    <button onclick={add} disabled={busy || !draft.trim()}>＋ Comment</button>
  </div>

  <label class="filter">
    <input type="checkbox" bind:checked={showResolved} /> show resolved
  </label>

  <div class="list">
    {#if visible.length === 0}
      <div class="empty">No {showResolved ? '' : 'open '}comments.</div>
    {/if}
    {#each visible as c (c.id)}
      <div class="comment" class:resolved={c.resolved}>
        <div class="crow">
          <button class="ts" title="Jump to this time" onclick={() => jump(c)}
            >{fmtTime(c.timeline_ms)}</button
          >
          <span class="who">{c.author}</span>
          <span class="when">{fmtWhen(c.created_unix)}</span>
          <span class="spacer"></span>
          <input
            type="checkbox"
            title="Resolved"
            checked={c.resolved}
            disabled={busy}
            onchange={() => toggleResolved(c)}
          />
          <button class="del" title="Delete" onclick={() => del(c)} disabled={busy}>✕</button>
        </div>
        <div class="body">{c.text}</div>
        {#each c.replies as r, i (i)}
          <div class="reply">
            <span class="who">{r.author}</span>
            <span class="body">{r.text}</span>
          </div>
        {/each}
        <div class="replybox">
          <input
            type="text"
            placeholder="Reply…"
            value={replyDrafts[c.id] ?? ''}
            oninput={(e) => (replyDrafts[c.id] = (e.target as HTMLInputElement).value)}
            onkeydown={(e) => e.key === 'Enter' && sendReply(c)}
            disabled={busy}
          />
        </div>
      </div>
    {/each}
  </div>
  <div class="hint">
    Comments travel in the .vance project — share the file to share the review.
  </div>
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
    display: flex;
    align-items: center;
    gap: 6px;
    font-weight: 600;
    border-bottom: 1px solid var(--border, #333);
    padding-bottom: 4px;
  }
  .badge {
    font-size: 10px;
    background: #6b4a2a;
    color: #ffdca8;
    border-radius: 8px;
    padding: 0 6px;
  }
  .compose {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
  .compose .author {
    width: 96px;
  }
  textarea,
  input[type='text'] {
    width: 100%;
    box-sizing: border-box;
    background: var(--panel-bg, #1b1b1b);
    color: var(--fg, #ddd);
    border: 1px solid var(--border, #333);
    border-radius: 3px;
    font: inherit;
  }
  textarea {
    resize: vertical;
  }
  button {
    cursor: pointer;
  }
  .filter {
    display: flex;
    align-items: center;
    gap: 4px;
    opacity: 0.7;
    font-size: 11px;
  }
  .list {
    display: flex;
    flex-direction: column;
    gap: 5px;
    max-height: 40vh;
    overflow: auto;
  }
  .empty {
    opacity: 0.5;
    font-size: 11px;
  }
  .comment {
    border: 1px solid var(--border, #2a2a2a);
    border-radius: 4px;
    padding: 4px 6px;
  }
  .comment.resolved {
    opacity: 0.55;
  }
  .crow {
    display: flex;
    align-items: center;
    gap: 6px;
    font-size: 11px;
  }
  .ts {
    background: #2a2a2a;
    border: none;
    color: #8ec5ff;
    border-radius: 3px;
    padding: 0 5px;
    font-variant-numeric: tabular-nums;
    cursor: pointer;
  }
  .who {
    font-weight: 600;
  }
  .when {
    opacity: 0.5;
    font-size: 10px;
  }
  .spacer {
    flex: 1;
  }
  .del {
    background: transparent;
    border: none;
    color: #ff6b6b;
    opacity: 0.7;
  }
  .body {
    margin-top: 2px;
    line-height: 1.4;
    white-space: pre-wrap;
  }
  .reply {
    margin: 3px 0 0 10px;
    padding-left: 6px;
    border-left: 2px solid var(--border, #333);
    font-size: 11px;
  }
  .reply .who {
    margin-right: 5px;
    opacity: 0.85;
  }
  .replybox {
    margin-top: 4px;
  }
  .hint {
    opacity: 0.5;
    font-size: 10px;
  }
  .err {
    color: #ff6b6b;
    font-size: 11px;
  }
</style>
