<script lang="ts">
  import { fmt1, tpsHeat } from '$lib/format';

  let { tps, max = 20 }: { tps: number; max?: number } = $props();

  const R = 78;
  const C = 2 * Math.PI * R;

  const frac = $derived(Math.max(0, Math.min(1, tps / max)));
  const heat = $derived(tpsHeat(tps));
  const color = $derived(
    heat === 'ok'
      ? 'var(--color-heat-ok)'
      : heat === 'warn'
        ? 'var(--color-heat-warn)'
        : 'var(--color-heat-bad)'
  );
  // Calm green when healthy, molten when the server is struggling.
  const glow = $derived(heat === 'bad' ? 1 : heat === 'warn' ? 0.6 : 0.28);
  const offset = $derived(C * (1 - frac));
</script>

<div class="gauge">
  <svg viewBox="0 0 200 200" role="img" aria-label="ticks per second: {fmt1(tps)} of {max}">
    <circle class="track" cx="100" cy="100" r={R} />
    <circle
      class="value"
      cx="100"
      cy="100"
      r={R}
      stroke={color}
      stroke-dasharray={C}
      stroke-dashoffset={offset}
      style="filter: drop-shadow(0 0 {10 * glow}px {color})"
    />
  </svg>
  <div class="center">
    <div class="metric tnum glow" style="--glow:{glow}; color:{color}">{fmt1(tps)}</div>
    <div class="cap">TPS · target {max}</div>
  </div>
</div>

<style>
  .gauge {
    position: relative;
    width: 200px;
    height: 200px;
    margin: 0 auto;
  }
  svg {
    width: 100%;
    height: 100%;
    transform: rotate(-90deg);
  }
  .track {
    fill: none;
    stroke: var(--color-iron-700);
    stroke-width: 12;
  }
  .value {
    fill: none;
    stroke-width: 12;
    stroke-linecap: round;
    transition:
      stroke-dashoffset 0.4s ease,
      stroke 0.4s ease,
      filter 0.4s ease;
  }
  .center {
    position: absolute;
    inset: 0;
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: 6px;
  }
  .center .metric {
    font-size: 46px;
  }
  .cap {
    font-size: 10.5px;
    letter-spacing: 0.14em;
    text-transform: uppercase;
    color: var(--color-iron-400);
  }
</style>
