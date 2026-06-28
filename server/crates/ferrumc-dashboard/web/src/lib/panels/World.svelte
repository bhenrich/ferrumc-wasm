<script lang="ts">
  import ChunkGrid from '$lib/components/ChunkGrid.svelte';
  import StatCard from '$lib/components/StatCard.svelte';
  import { fmtInt } from '$lib/format';
  import { telemetry } from '$lib/telemetry.svelte';

  const snap = $derived(telemetry.snapshot);
</script>

<div class="cards">
  <StatCard label="chunks resident" value={fmtInt(snap.chunks_loaded)} heat="ember" />
  <StatCard label="persist-dirty" value={fmtInt(snap.chunks_persist_dirty)} sub="awaiting storage flush" />
  <StatCard label="chunks sent" value={fmtInt(snap.chunk_sent_total)} sub="cumulative to clients" />
  <StatCard label="chunks unloaded" value={fmtInt(snap.chunk_unloaded_total)} sub="cumulative evictions" />
  <!-- network-dirty is not yet surfaced by the chunk map; render it as pending. -->
  <StatCard label="network-dirty" pending={snap.chunks_dirty === 0} value={fmtInt(snap.chunks_dirty)} />
</div>

<section class="panel panel-pad map">
  <div class="row-between">
    <div class="eyebrow">chunk residence</div>
    <span class="muted small">cell = resident chunk</span>
  </div>
  <ChunkGrid loaded={snap.chunks_loaded} dirty={snap.chunks_persist_dirty} />
</section>

<style>
  .cards {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(170px, 1fr));
    gap: 16px;
  }
  .map {
    margin-top: 16px;
  }
  .row-between {
    display: flex;
    align-items: center;
    justify-content: space-between;
    margin-bottom: 16px;
  }
  .small {
    font-size: 11px;
  }
</style>
