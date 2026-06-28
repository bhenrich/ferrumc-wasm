<script lang="ts">
  import Sparkline from '$lib/components/Sparkline.svelte';
  import StatCard from '$lib/components/StatCard.svelte';
  import { fmt2, fmtInt } from '$lib/format';
  import { telemetry } from '$lib/telemetry.svelte';

  const snap = $derived(telemetry.snapshot);
  const flushHist = $derived.by(() => {
    telemetry.rev;
    return [...telemetry.history.flush];
  });

  const muts = $derived([
    { label: 'block breaks', m: snap.block_breaks },
    { label: 'block places', m: snap.block_places }
  ]);
</script>

<div class="cards">
  <StatCard label="flush · last" value={fmtInt(snap.storage_flush_ms_last)} unit="ms" heat="ember" />
  <StatCard label="flush · avg" value={fmt2(snap.storage_flush_ms_avg)} unit="ms" />
  <StatCard label="persist-dirty" value={fmtInt(snap.chunks_persist_dirty)} sub="chunks queued for storage" />
</div>

<section class="panel panel-pad area">
  <div class="row-between">
    <div class="eyebrow">flush latency · rolling</div>
    <span class="muted small">last {flushHist.length} flush samples · ms</span>
  </div>
  <Sparkline values={flushHist} stroke="var(--color-ember)" fill="rgba(232,115,31,0.16)" height={92} />
</section>

<section class="panel panel-pad muts">
  <div class="eyebrow">block mutations · accepted vs rejected</div>
  <div class="mut-grid">
    {#each muts as row (row.label)}
      {@const total = row.m.accepted + row.m.rejected}
      <div class="mut">
        <div class="mlabel">{row.label}</div>
        <div class="mbar">
          <span class="acc" style="flex:{row.m.accepted || (total ? 0 : 0)}"></span>
          <span class="rej" style="flex:{row.m.rejected}"></span>
          {#if total === 0}<span class="none"></span>{/if}
        </div>
        <div class="mnums">
          <span class="ok tnum">{fmtInt(row.m.accepted)} accepted</span>
          <span class="bad tnum" class:zero={row.m.rejected === 0}>{fmtInt(row.m.rejected)} rejected</span>
        </div>
      </div>
    {/each}
  </div>
</section>

<style>
  .cards {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(180px, 1fr));
    gap: 16px;
  }
  .area {
    margin-top: 16px;
  }
  .row-between {
    display: flex;
    align-items: center;
    justify-content: space-between;
    margin-bottom: 14px;
  }
  .small {
    font-size: 11px;
  }
  .muts {
    margin-top: 16px;
  }
  .mut-grid {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 28px;
    margin-top: 16px;
  }
  .mut .mlabel {
    font-size: 12.5px;
    color: var(--color-iron-200);
    margin-bottom: 8px;
  }
  .mbar {
    display: flex;
    height: 12px;
    border-radius: 999px;
    overflow: hidden;
    background: var(--color-iron-800);
    border: 1px solid var(--color-iron-700);
  }
  .mbar .acc {
    background: var(--color-heat-ok);
  }
  .mbar .rej {
    background: var(--color-heat-bad);
  }
  .mbar .none {
    flex: 1;
  }
  .mnums {
    display: flex;
    justify-content: space-between;
    margin-top: 8px;
    font-size: 11.5px;
  }
  .ok {
    color: var(--color-heat-ok);
  }
  .bad {
    color: var(--color-heat-bad);
  }
  .bad.zero {
    color: var(--color-iron-500);
  }
  @media (max-width: 640px) {
    .mut-grid {
      grid-template-columns: 1fr;
    }
  }
</style>
