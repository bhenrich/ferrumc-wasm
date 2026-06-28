<script lang="ts">
  import Gauge from '$lib/components/Gauge.svelte';
  import LineChart from '$lib/components/LineChart.svelte';
  import StatCard from '$lib/components/StatCard.svelte';
  import { fmt2, fmtInt, fmtUptime, heatRatio } from '$lib/format';
  import { telemetry } from '$lib/telemetry.svelte';

  const snap = $derived(telemetry.snapshot);
  // 20 TPS ⇒ a 50 ms tick budget; p99 glow ramps as we approach it.
  const msptGlow = $derived(heatRatio(snap.tick_p99_ms, 50));

  const msptSeries = [
    { label: 'p50', stroke: '#ff8a3d', width: 2 },
    { label: 'p95', stroke: '#d29922', width: 1.6 },
    { label: 'p99', stroke: '#f04438', width: 1.6 }
  ];

  function msptData(): [number[], number[], number[], number[]] {
    const h = telemetry.history;
    return [h.t, h.p50, h.p95, h.p99];
  }
</script>

<div class="grid-top">
  <section class="panel panel-pad gauge-card">
    <div class="eyebrow">tick rate</div>
    <Gauge tps={snap.tps} />
    <div class="budget muted">
      tick <span class="tnum">{fmtInt(snap.tick)}</span> · budget 50.00 ms
    </div>
  </section>

  <section class="panel panel-pad mspt-card">
    <div class="row-between">
      <div class="eyebrow">tick duration · ms</div>
      <div class="legend">
        <span class="k"><i style="background:#ff8a3d"></i>p50 {fmt2(snap.tick_p50_ms)}</span>
        <span class="k"><i style="background:#d29922"></i>p95 {fmt2(snap.tick_p95_ms)}</span>
        <span class="k"><i style="background:#f04438"></i>p99 {fmt2(snap.tick_p99_ms)}</span>
      </div>
    </div>
    <LineChart rev={telemetry.rev} getData={msptData} series={msptSeries} height={200} glow={msptGlow} yMin={0} />
  </section>
</div>

<div class="cards">
  <StatCard label="players online" value={fmtInt(snap.players_online)} heat="ember" />
  <StatCard label="chunks resident" value={fmtInt(snap.chunks_loaded)} />
  <StatCard label="uptime" value={fmtUptime(snap.uptime_secs)} />
  <StatCard label="flush · last" value={fmtInt(snap.storage_flush_ms_last)} unit="ms" />
  <StatCard label="build">
    <div class="build">{snap.build || '—'}</div>
    <div class="sub muted">protocol 772 · MC Java 1.21.8</div>
  </StatCard>
</div>

<style>
  .grid-top {
    display: grid;
    grid-template-columns: 280px 1fr;
    gap: 16px;
  }
  .gauge-card {
    display: flex;
    flex-direction: column;
    gap: 14px;
  }
  .budget {
    text-align: center;
    font-size: 11.5px;
  }
  .mspt-card {
    min-width: 0;
  }
  .row-between {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    margin-bottom: 8px;
    flex-wrap: wrap;
  }
  .legend {
    display: flex;
    gap: 14px;
    font-size: 11.5px;
    color: var(--color-iron-300);
  }
  .legend .k {
    display: inline-flex;
    align-items: center;
    gap: 6px;
  }
  .legend i {
    width: 9px;
    height: 9px;
    border-radius: 2px;
    display: inline-block;
  }
  .cards {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(170px, 1fr));
    gap: 16px;
    margin-top: 16px;
  }
  .build {
    font-family: var(--font-mono);
    font-size: 18px;
    font-weight: 500;
    color: var(--color-iron-100);
    margin-top: 10px;
    word-break: break-word;
  }
  .sub {
    margin-top: 8px;
    font-size: 11px;
  }
  @media (max-width: 760px) {
    .grid-top {
      grid-template-columns: 1fr;
    }
  }
</style>
