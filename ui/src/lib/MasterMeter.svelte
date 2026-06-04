<script lang="ts">
  /**
   * Phase 7.5 M1 — small master meter rendered next to the master fader.
   * Reads from the shared metersStore so hold + decay tuning matches
   * the per-track meters in the timeline header.
   */
  import { onMount } from 'svelte';
  import { metersStore, dbToFill } from '$lib/meters.svelte';

  let canvas = $state<HTMLCanvasElement | null>(null);
  const W = 80; // total canvas width
  const H = 18; // total canvas height — short horizontal meters

  function draw() {
    if (!canvas) return;
    const ctx = canvas.getContext('2d');
    if (!ctx) return;
    const dpr = window.devicePixelRatio || 1;
    if (canvas.width !== W * dpr) canvas.width = W * dpr;
    if (canvas.height !== H * dpr) canvas.height = H * dpr;
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.clearRect(0, 0, W, H);
    // Background slot.
    ctx.fillStyle = '#1c1f25';
    ctx.fillRect(0, 0, W, H);
    const barH = (H - 2) / 2;
    drawHorizontal(ctx, 0, 1, W, barH, metersStore.master.l);
    drawHorizontal(ctx, 0, 1 + barH, W, barH, metersStore.master.r);
  }

  function drawHorizontal(
    ctx: CanvasRenderingContext2D,
    x: number,
    y: number,
    w: number,
    h: number,
    ch: { rms: number; peak_db: number; clipping: boolean }
  ) {
    const rmsDb = ch.rms > 0 ? 20 * Math.log10(ch.rms) : -Infinity;
    const fill = Math.max(0, Math.min(1, dbToFill(rmsDb)));
    const fillPx = Math.round(fill * w);
    if (fillPx > 0) {
      ctx.fillStyle = '#3ab66c';
      ctx.fillRect(x, y, fillPx, h);
      const sixDbFill = Math.round(dbToFill(-6) * w);
      if (fillPx > sixDbFill) {
        ctx.fillStyle = '#e6c52c';
        ctx.fillRect(x + sixDbFill, y, fillPx - sixDbFill, h);
      }
      const zeroDbFill = Math.round(dbToFill(0) * w);
      if (fillPx > zeroDbFill) {
        ctx.fillStyle = '#e44b4b';
        ctx.fillRect(x + zeroDbFill, y, fillPx - zeroDbFill, h);
      }
    }
    // Held-peak line — only drawn when the peak has crossed the dB
    // floor. dbToFill clamps to 0 at the floor, which would otherwise
    // park the line at x-1 (off the meter) when the channel is silent.
    if (isFinite(ch.peak_db)) {
      const peakFill = Math.max(0, Math.min(1, dbToFill(ch.peak_db)));
      if (peakFill > 0) {
        const peakX = Math.round(peakFill * w);
        ctx.fillStyle = '#e6ebf5';
        ctx.fillRect(x + Math.max(0, peakX - 1), y, 1, h);
      }
    }
    // Clip LED — small red square at the right edge.
    if (ch.clipping) {
      ctx.fillStyle = '#ff3030';
      ctx.fillRect(x + w - 2, y, 2, h);
    }
  }

  onMount(() => {
    const id = window.setInterval(draw, 33);
    return () => window.clearInterval(id);
  });
</script>

<canvas
  bind:this={canvas}
  style="width:{W}px;height:{H}px;display:block;"
  title="Master bus meter (peak hold 1.5s, decay 20 dB/s)"
></canvas>
