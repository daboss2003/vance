<script lang="ts">
  // A DaVinci-style colorist trackball: a circular pad whose puck sets
  // the R/G/B *balance* (chroma direction, luma-neutral), plus a
  // vertical master slider beside it that lifts all three channels
  // together. Red is at top, green lower-left, blue lower-right
  // (120° apart). Drag emits `onInput` live (non-undoable preview);
  // pointer-up emits `onCommit` (single undo step).

  type Rgb = [number, number, number];

  let {
    label,
    rgb,
    master,
    masterMin = -1,
    masterMax = 1,
    showMaster = true,
    onInput,
    onCommit
  }: {
    label: string;
    rgb: Rgb;
    master: number;
    masterMin?: number;
    masterMax?: number;
    showMaster?: boolean;
    onInput: (rgb: Rgb, master: number) => void;
    onCommit: (rgb: Rgb, master: number) => void;
  } = $props();

  const SIZE = 96;
  const R = SIZE / 2 - 6; // puck travel radius
  const CX = SIZE / 2;
  const CY = SIZE / 2;
  const SQ3_2 = Math.sqrt(3) / 2;

  // Forward: puck (x,y in [-1,1]) → per-channel balance offsets.
  function puckToRgb(x: number, y: number): Rgb {
    return [
      -y, // red points up (screen y is down)
      -SQ3_2 * x + 0.5 * y, // green lower-left
      SQ3_2 * x + 0.5 * y // blue lower-right
    ];
  }
  // Inverse: rgb balance → puck position (pseudo-inverse of the 3
  // equiangular 2D directions; the 2/3 factor makes it round-trip).
  function rgbToPuck(c: Rgb): { x: number; y: number } {
    const x = (Math.sqrt(3) / 3) * (c[2] - c[1]);
    const y = (2 / 3) * (-c[0] + 0.5 * c[1] + 0.5 * c[2]);
    return { x, y };
  }

  let puck = $derived(rgbToPuck(rgb));
  // Clamp the *display* position to the ring even when the underlying
  // rgb balance (imported / keyframed) exceeds the wheel's travel; the
  // stored values stay intact.
  let puckClamped = $derived(clampUnit(puck.x, puck.y));
  let puckPx = $derived({ x: CX + puckClamped.x * R, y: CY + puckClamped.y * R });

  let dragging = $state(false);

  function clampUnit(x: number, y: number): { x: number; y: number } {
    const len = Math.hypot(x, y);
    if (len <= 1) return { x, y };
    return { x: x / len, y: y / len };
  }

  function pointerToRgb(ev: PointerEvent, el: SVGSVGElement): Rgb {
    const rect = el.getBoundingClientRect();
    const nx = (ev.clientX - rect.left - CX) / R;
    const ny = (ev.clientY - rect.top - CY) / R;
    const { x, y } = clampUnit(nx, ny);
    return puckToRgb(x, y);
  }

  function onWheelDown(ev: PointerEvent) {
    const el = ev.currentTarget as SVGSVGElement;
    el.setPointerCapture(ev.pointerId);
    dragging = true;
    onInput(pointerToRgb(ev, el), master);
  }
  function onWheelMove(ev: PointerEvent) {
    if (!dragging) return;
    const el = ev.currentTarget as SVGSVGElement;
    onInput(pointerToRgb(ev, el), master);
  }
  function onWheelUp(ev: PointerEvent) {
    if (!dragging) return;
    dragging = false;
    const el = ev.currentTarget as SVGSVGElement;
    onCommit(pointerToRgb(ev, el), master);
  }

  // If the OS/browser cancels the gesture (context menu, capture loss),
  // pointerup never fires. Without this the move handler would stay
  // armed (puck dragging with no button) and the live preview would
  // never be committed. Commit the current rgb and disarm.
  function onWheelCancel() {
    if (!dragging) return;
    dragging = false;
    onCommit(rgb, master);
  }

  // Double-click resets the wheel balance to neutral.
  function resetWheel() {
    onCommit([0, 0, 0], master);
  }

  function onMasterInput(ev: Event) {
    const v = parseFloat((ev.currentTarget as HTMLInputElement).value);
    onInput(rgb, v);
  }
  function onMasterCommit(ev: Event) {
    const v = parseFloat((ev.currentTarget as HTMLInputElement).value);
    onCommit(rgb, v);
  }
</script>

<div class="flex flex-col items-center gap-1">
  <span class="text-vance-muted text-[10px] uppercase tracking-wide">{label}</span>
  <div class="flex items-end gap-1">
    <svg
      width={SIZE}
      height={SIZE}
      class="cursor-crosshair touch-none select-none"
      role="slider"
      tabindex="0"
      aria-label={`${label} balance trackball`}
      aria-valuemin={-1}
      aria-valuemax={1}
      aria-valuenow={0}
      aria-valuetext={`R ${rgb[0].toFixed(2)} G ${rgb[1].toFixed(2)} B ${rgb[2].toFixed(2)}`}
      onpointerdown={onWheelDown}
      onpointermove={onWheelMove}
      onpointerup={onWheelUp}
      onpointercancel={onWheelCancel}
      onlostpointercapture={onWheelCancel}
      ondblclick={resetWheel}
    >
      <!-- Ring background -->
      <circle cx={CX} cy={CY} r={R} fill="#15181d" stroke="#2a2e36" stroke-width="1" />
      <!-- Primary direction hints -->
      <circle cx={CX} cy={CY - R} r="2" fill="#e0524d" />
      <circle cx={CX - SQ3_2 * R} cy={CY + 0.5 * R} r="2" fill="#5fb86b" />
      <circle cx={CX + SQ3_2 * R} cy={CY + 0.5 * R} r="2" fill="#5b8def" />
      <!-- Center crosshair -->
      <line x1={CX - 3} y1={CY} x2={CX + 3} y2={CY} stroke="#3a3f49" stroke-width="1" />
      <line x1={CX} y1={CY - 3} x2={CX} y2={CY + 3} stroke="#3a3f49" stroke-width="1" />
      <!-- Puck -->
      <circle
        cx={puckPx.x}
        cy={puckPx.y}
        r="5"
        fill="#e6ebf5"
        stroke="#003040"
        stroke-width="1.5"
      />
    </svg>
    <!-- Master slider — only for groups that have a master channel
         (primary lift/gamma/gain). Log groups have no master, so the
         slider would be dead UI; hide it. -->
    {#if showMaster}
      <input
        type="range"
        class="vance-vrange h-[96px]"
        min={masterMin}
        max={masterMax}
        step="0.001"
        value={master}
        oninput={onMasterInput}
        onchange={onMasterCommit}
        aria-label={`${label} master`}
        title={`${label} master ${master.toFixed(3)}`}
      />
    {/if}
  </div>
  <span class="font-mono text-[9px] text-vance-muted">
    {rgb[0].toFixed(2)}
    {rgb[1].toFixed(2)}
    {rgb[2].toFixed(2)} · {master.toFixed(2)}
  </span>
</div>

<style>
  /* Vertical range input (master slider). */
  .vance-vrange {
    -webkit-appearance: slider-vertical;
    appearance: slider-vertical;
    width: 14px;
  }
</style>
