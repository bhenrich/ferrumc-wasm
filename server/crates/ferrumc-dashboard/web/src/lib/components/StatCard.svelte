<script lang="ts">
  import type { Snippet } from 'svelte';

  let {
    label,
    value,
    unit = '',
    heat = '',
    sub = '',
    glow = 0,
    pending = false,
    children
  }: {
    label: string;
    value?: string | number;
    unit?: string;
    heat?: '' | 'ok' | 'warn' | 'bad' | 'ember';
    sub?: string;
    glow?: number;
    pending?: boolean;
    children?: Snippet;
  } = $props();

  const heatClass = $derived(
    heat === 'ok'
      ? 'heat-ok'
      : heat === 'warn'
        ? 'heat-warn'
        : heat === 'bad'
          ? 'heat-bad'
          : heat === 'ember'
            ? 'ember'
            : ''
  );
</script>

<div class="panel panel-pad stat">
  <div class="eyebrow">{label}</div>
  {#if pending}
    <div class="metric pend">—</div>
    <div class="awaiting">awaiting telemetry</div>
  {:else if children}
    {@render children()}
  {:else}
    <div class="metric val {heatClass} glow" style="--glow:{glow}">
      <span class="tnum">{value}</span>{#if unit}<span class="unit">{unit}</span>{/if}
    </div>
    {#if sub}<div class="sub muted">{sub}</div>{/if}
  {/if}
</div>

<style>
  .stat {
    min-width: 0;
  }
  .val {
    font-size: clamp(26px, 3.4vw, 38px);
    margin-top: 10px;
  }
  .pend {
    font-size: 34px;
    margin-top: 10px;
    color: var(--color-iron-600);
  }
  .unit {
    font-size: 0.45em;
    margin-left: 4px;
    color: var(--color-iron-400);
    font-weight: 500;
  }
  .sub {
    margin-top: 8px;
    font-size: 11.5px;
  }
</style>
