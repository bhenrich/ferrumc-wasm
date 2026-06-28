<script lang="ts">
  import LiveDot from '$lib/components/LiveDot.svelte';
  import { fmt1, fmtUptime, tpsHeat } from '$lib/format';
  import { telemetry } from '$lib/telemetry.svelte';

  let { title }: { title: string } = $props();

  const snap = $derived(telemetry.snapshot);
  const heat = $derived(tpsHeat(snap.tps));
  const heatClass = $derived(heat === 'ok' ? 'heat-ok' : heat === 'warn' ? 'heat-warn' : 'heat-bad');
</script>

<header class="hdr">
  <div class="left">
    <h1 class="title">{title}</h1>
    <span class="chip">tick <span class="tnum">{snap.tick.toLocaleString('en-US')}</span></span>
  </div>
  <div class="right">
    <div class="stat">
      <span class="k">TPS</span>
      <span class="v tnum {heatClass}">{fmt1(snap.tps)}</span>
    </div>
    <div class="sep"></div>
    <div class="stat">
      <span class="k">UPTIME</span>
      <span class="v tnum">{fmtUptime(snap.uptime_secs)}</span>
    </div>
    <div class="sep"></div>
    <LiveDot state={telemetry.conn} />
  </div>
</header>

<style>
  .hdr {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 16px;
    padding: 18px 26px;
    border-bottom: 1px solid var(--color-iron-700);
    background: rgba(11, 12, 14, 0.6);
    backdrop-filter: blur(6px);
    position: sticky;
    top: 0;
    z-index: 10;
    flex-wrap: wrap;
  }
  .left {
    display: flex;
    align-items: center;
    gap: 14px;
  }
  .title {
    font-family: var(--font-display);
    font-size: 19px;
    font-weight: 600;
    margin: 0;
    color: var(--color-iron-100);
    letter-spacing: 0.01em;
  }
  .right {
    display: flex;
    align-items: center;
    gap: 16px;
  }
  .stat {
    display: flex;
    flex-direction: column;
    align-items: flex-end;
    gap: 2px;
  }
  .stat .k {
    font-size: 9.5px;
    letter-spacing: 0.16em;
    color: var(--color-iron-500);
  }
  .stat .v {
    font-size: 15px;
    font-weight: 700;
    color: var(--color-iron-100);
  }
  .sep {
    width: 1px;
    height: 26px;
    background: var(--color-iron-700);
  }
  @media (max-width: 760px) {
    .hdr {
      padding: 14px 18px;
    }
  }
</style>
