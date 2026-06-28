<script lang="ts">
  import Bar from '$lib/components/Bar.svelte';
  import Sparkline from '$lib/components/Sparkline.svelte';
  import StatCard from '$lib/components/StatCard.svelte';
  import { fmtInt } from '$lib/format';
  import { telemetry } from '$lib/telemetry.svelte';

  const snap = $derived(telemetry.snapshot);
  const qmaxHist = $derived.by(() => {
    telemetry.rev;
    return [...telemetry.history.qmax];
  });
  const droppedLive = $derived(snap.players.some((p) => p.packets_dropped_total > 0));
  const queueMax = $derived(Math.max(1, snap.network_outbound_queue_len_max, ...snap.players.map((p) => p.outbound_queue_len)));
</script>

<div class="top">
  <StatCard
    label="outbound queue · max"
    value={fmtInt(snap.network_outbound_queue_len_max)}
    heat={snap.network_outbound_queue_len_max > 0 ? 'warn' : 'ok'}
    sub="deepest session backlog"
  />
  <section class="panel panel-pad spark-card">
    <div class="eyebrow">queue-max · trend</div>
    <Sparkline values={qmaxHist} stroke="var(--color-heat-warn)" fill="rgba(210,153,34,0.12)" height={64} />
  </section>
</div>

<section class="panel">
  <div class="panel-pad q-head">
    <div class="eyebrow">per-player backpressure</div>
    {#if !droppedLive}<span class="awaiting">drop counters · awaiting telemetry</span>{/if}
  </div>
  {#if snap.players.length === 0}
    <div class="panel-pad muted empty">No sessions. Backpressure appears once players connect.</div>
  {:else}
    <div class="panel-pad rows">
      {#each snap.players as p (p.name)}
        <div class="prow">
          <Bar label={p.name} value={p.outbound_queue_len} max={queueMax} color="var(--color-ember)" meta="queued frames" />
          <span class="drop tnum" class:dim={!droppedLive} title="packets dropped (total)">
            {droppedLive ? fmtInt(p.packets_dropped_total) : '—'} dropped
          </span>
        </div>
      {/each}
    </div>
  {/if}
</section>

<style>
  .top {
    display: grid;
    grid-template-columns: 280px 1fr;
    gap: 16px;
  }
  .spark-card {
    display: flex;
    flex-direction: column;
    justify-content: space-between;
    min-width: 0;
  }
  .q-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    border-bottom: 1px solid var(--color-iron-700);
  }
  .rows {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
  .prow {
    display: grid;
    grid-template-columns: 1fr auto;
    align-items: center;
    gap: 16px;
  }
  .drop {
    font-size: 12px;
    color: var(--color-iron-300);
    white-space: nowrap;
  }
  .drop.dim {
    color: var(--color-iron-600);
  }
  .empty {
    padding: 40px 20px;
  }
  @media (max-width: 760px) {
    .top {
      grid-template-columns: 1fr;
    }
  }
</style>
