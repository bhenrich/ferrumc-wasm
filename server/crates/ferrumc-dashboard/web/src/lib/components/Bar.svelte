<script lang="ts">
  import { fmtInt } from '$lib/format';

  let {
    label,
    value,
    max,
    color = 'var(--color-ember)',
    meta = ''
  }: {
    label: string;
    value: number;
    max: number;
    color?: string;
    meta?: string;
  } = $props();

  const pct = $derived(max > 0 ? Math.max(2, Math.min(100, (value / max) * 100)) : 0);
</script>

<div class="row">
  <div class="head">
    <span class="lbl">{label}</span>
    {#if meta}<span class="meta muted">{meta}</span>{/if}
    <span class="val tnum">{fmtInt(value)}</span>
  </div>
  <div class="track">
    <span class="fill" style="width:{pct}%; background:{color}; box-shadow:0 0 8px {color}"></span>
  </div>
</div>

<style>
  .row {
    margin: 10px 0;
  }
  .head {
    display: flex;
    align-items: baseline;
    gap: 8px;
    font-size: 12.5px;
    margin-bottom: 5px;
  }
  .lbl {
    color: var(--color-iron-200);
  }
  .meta {
    font-size: 11px;
  }
  .val {
    margin-left: auto;
    color: var(--color-iron-100);
    font-weight: 500;
  }
  .track {
    height: 8px;
    border-radius: 999px;
    background: var(--color-iron-800);
    border: 1px solid var(--color-iron-700);
    overflow: hidden;
  }
  .fill {
    display: block;
    height: 100%;
    border-radius: 999px;
    transition: width 0.4s ease;
  }
</style>
