<script lang="ts">
  // Renders one video scope (Phase 6.12) from raw accumulation counts.
  // All presentation lives here — the Rust side ships only counts, so
  // log/linear scaling, channel tints, graticule, 75% target boxes and
  // the skin-tone line are drawn client-side.

  import type { ScopeData, ScopeIntensityGrid } from '$lib/ipc';

  type Kind = 'waveform' | 'parade' | 'vector' | 'histogram';

  let {
    kind,
    data,
    log = true,
    width = 320,
    height = 200
  }: {
    kind: Kind;
    data: ScopeData | null;
    log?: boolean;
    width?: number;
    height?: number;
  } = $props();

  let canvas: HTMLCanvasElement | undefined = $state();

  // Map a normalized [0,1] intensity to a 0..255 brightness, log or
  // linear. Counts span a huge range, so log is the default (matches
  // hardware scopes' trace falloff).
  function norm(count: number, max: number): number {
    if (max <= 0) return 0;
    if (log) return Math.log1p(count) / Math.log1p(max);
    return count / max;
  }

  function arrMax(a: Uint32Array): number {
    let m = 0;
    for (let i = 0; i < a.length; i++) if (a[i] > m) m = a[i];
    return m;
  }

  // Paint a cols×levels grid into an ImageData tinted (tr,tg,tb), with
  // level 0 at the BOTTOM (waveform convention). Returns the ImageData
  // sized cols×levels (caller scales it onto the canvas).
  function gridImage(grid: ScopeIntensityGrid, tint: [number, number, number]): ImageData {
    const { cols, levels, bins } = grid;
    const img = new ImageData(cols, levels);
    const max = arrMax(bins);
    for (let c = 0; c < cols; c++) {
      for (let l = 0; l < levels; l++) {
        const v = norm(bins[c * levels + l], max);
        // Flip y: level 0 → bottom row.
        const px = ((levels - 1 - l) * cols + c) * 4;
        img.data[px] = tint[0] * v;
        img.data[px + 1] = tint[1] * v;
        img.data[px + 2] = tint[2] * v;
        img.data[px + 3] = 255;
      }
    }
    return img;
  }

  // Draw an ImageData scaled to a target rect via an offscreen canvas.
  function blit(ctx: CanvasRenderingContext2D, img: ImageData, dx: number, dy: number, dw: number, dh: number) {
    const off = document.createElement('canvas');
    off.width = img.width;
    off.height = img.height;
    off.getContext('2d')!.putImageData(img, 0, 0);
    ctx.imageSmoothingEnabled = true;
    ctx.drawImage(off, dx, dy, dw, dh);
  }

  function drawWaveform(ctx: CanvasRenderingContext2D, d: ScopeData) {
    blit(ctx, gridImage(d.waveformLuma, [200, 220, 200]), 0, 0, width, height);
  }

  function drawParade(ctx: CanvasRenderingContext2D, d: ScopeData) {
    const w3 = Math.floor(width / 3);
    blit(ctx, gridImage(d.parade.r, [230, 70, 70]), 0, 0, w3, height);
    blit(ctx, gridImage(d.parade.g, [70, 210, 90]), w3, 0, w3, height);
    blit(ctx, gridImage(d.parade.b, [80, 130, 240]), w3 * 2, 0, width - w3 * 2, height);
  }

  function drawVector(ctx: CanvasRenderingContext2D, d: ScopeData) {
    const vs = d.vectorscope;
    const size = vs.size;
    const max = arrMax(vs.bins);
    const img = new ImageData(size, size);
    for (let i = 0; i < vs.bins.length; i++) {
      const v = norm(vs.bins[i], max);
      img.data[i * 4] = 40 * v;
      img.data[i * 4 + 1] = 240 * v; // phosphor green
      img.data[i * 4 + 2] = 60 * v;
      img.data[i * 4 + 3] = 255;
    }
    // Square plot centered in the canvas.
    const s = Math.min(width, height);
    const ox = (width - s) / 2;
    const oy = (height - s) / 2;
    blit(ctx, img, ox, oy, s, s);
    const gx = (vx: number) => ox + (vx / (size - 1)) * s;
    const gy = (vy: number) => oy + (vy / (size - 1)) * s;
    // Graticule circle + center.
    ctx.strokeStyle = 'rgba(120,140,120,0.5)';
    ctx.beginPath();
    ctx.arc(ox + s / 2, oy + s / 2, s / 2 - 1, 0, Math.PI * 2);
    ctx.stroke();
    // 75%-bar target boxes.
    ctx.strokeStyle = 'rgba(220,220,120,0.8)';
    for (let t = 0; t < vs.targets.length; t += 2) {
      const cx = gx(vs.targets[t]);
      const cy = gy(vs.targets[t + 1]);
      ctx.strokeRect(cx - 4, cy - 4, 8, 8);
    }
    // Skin-tone line through the center.
    const rad = (vs.skinLineDeg * Math.PI) / 180;
    ctx.strokeStyle = 'rgba(230,180,150,0.7)';
    ctx.beginPath();
    ctx.moveTo(ox + s / 2, oy + s / 2);
    ctx.lineTo(ox + s / 2 + Math.cos(rad) * (s / 2 - 1), oy + s / 2 - Math.sin(rad) * (s / 2 - 1));
    ctx.stroke();
  }

  function drawHistogram(ctx: CanvasRenderingContext2D, d: ScopeData) {
    const h = d.histogram;
    const bins = h.r.length;
    const max = Math.max(arrMax(h.r), arrMax(h.g), arrMax(h.b));
    const channels: [Uint32Array, string][] = [
      [h.r, 'rgba(230,70,70,0.7)'],
      [h.g, 'rgba(70,210,90,0.7)'],
      [h.b, 'rgba(80,130,240,0.7)']
    ];
    ctx.globalCompositeOperation = 'lighter';
    for (const [arr, color] of channels) {
      ctx.fillStyle = color;
      const bw = width / bins;
      for (let i = 0; i < bins; i++) {
        const v = norm(arr[i], max);
        const bh = v * height;
        ctx.fillRect(i * bw, height - bh, Math.ceil(bw), bh);
      }
    }
    ctx.globalCompositeOperation = 'source-over';
  }

  $effect(() => {
    const c = canvas;
    if (!c) return;
    const ctx = c.getContext('2d');
    if (!ctx) return;
    ctx.clearRect(0, 0, width, height);
    ctx.fillStyle = '#0c0e11';
    ctx.fillRect(0, 0, width, height);
    if (!data) {
      ctx.fillStyle = '#555';
      ctx.font = '11px sans-serif';
      ctx.fillText('no signal', 8, 16);
      return;
    }
    if (kind === 'waveform') drawWaveform(ctx, data);
    else if (kind === 'parade') drawParade(ctx, data);
    else if (kind === 'vector') drawVector(ctx, data);
    else drawHistogram(ctx, data);
  });
</script>

<canvas bind:this={canvas} {width} {height} class="rounded border border-vance-border" style="width:100%;height:auto;"
></canvas>
