<script lang="ts">
  import Bar from '$lib/components/Bar.svelte';
  import { fmtInt } from '$lib/format';
  import { telemetry } from '$lib/telemetry.svelte';
  import type { PacketTraceSummary } from '$lib/types';

  const snap = $derived(telemetry.snapshot);

  const stateColor: Record<string, string> = {
    play: 'var(--color-ember)',
    configuration: 'var(--color-slag)',
    login: 'var(--color-heat-warn)',
    status: 'var(--color-iron-400)',
    handshaking: 'var(--color-iron-500)'
  };

  function top(summary: PacketTraceSummary) {
    const max = Math.max(1, ...summary.top_packets.map((p) => p.count));
    return { rows: summary.top_packets, max };
  }

  const inbound = $derived(top(snap.inbound_trace_summary));
  const outbound = $derived(top(snap.outbound_trace_summary));
  const decodeTotal = $derived(
    snap.decode_errors_recent.reduce((a, e) => a + e.count, 0) + snap.decode_errors_overflow
  );
</script>

<div class="two">
  {#each [{ title: 'inbound', data: inbound }, { title: 'outbound', data: outbound }] as col (col.title)}
    <section class="panel panel-pad">
      <div class="eyebrow">{col.title} · top packets</div>
      {#if col.data.rows.length === 0}
        <div class="stub">
          <div class="awaiting">awaiting telemetry</div>
          <p class="muted">Packet-frequency tracing is wired by the network lane. The view lights up the moment frames flow.</p>
        </div>
      {:else}
        {#each col.data.rows as p (p.packet_name + p.state)}
          <Bar
            label={p.packet_name}
            value={p.count}
            max={col.data.max}
            color={stateColor[p.state] ?? 'var(--color-ember)'}
            meta={p.state}
          />
        {/each}
      {/if}
    </section>
  {/each}
</div>

<section class="panel decode">
  <div class="panel-pad d-head">
    <div>
      <div class="eyebrow">decode errors · live</div>
      <div class="metric tnum d-total" class:bad={decodeTotal > 0}>{fmtInt(decodeTotal)}</div>
    </div>
    <span class="muted small">rejected frames by (state, packet){snap.decode_errors_overflow ? ` · +${snap.decode_errors_overflow} overflow` : ''}</span>
  </div>
  {#if snap.decode_errors_recent.length === 0}
    <div class="panel-pad muted clean">No malformed frames. Hostile-input guards report clean.</div>
  {:else}
    <table>
      <thead><tr><th>State</th><th>Packet</th><th class="r">Count</th></tr></thead>
      <tbody>
        {#each snap.decode_errors_recent as e (e.state + e.packet)}
          <tr><td><span class="chip">{e.state}</span></td><td class="mono">{e.packet}</td><td class="r tnum">{fmtInt(e.count)}</td></tr>
        {/each}
      </tbody>
    </table>
  {/if}
</section>

<style>
  .two {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 16px;
  }
  .stub {
    padding: 22px 0 6px;
  }
  .stub p {
    margin: 12px 0 0;
    font-size: 12px;
    max-width: 38ch;
  }
  .decode {
    margin-top: 16px;
  }
  .d-head {
    display: flex;
    align-items: flex-start;
    justify-content: space-between;
    gap: 12px;
    border-bottom: 1px solid var(--color-iron-700);
  }
  .d-total {
    font-size: 24px;
    margin-top: 8px;
    color: var(--color-heat-ok);
  }
  .d-total.bad {
    color: var(--color-heat-bad);
  }
  .clean {
    padding: 22px 20px;
  }
  table {
    width: 100%;
    border-collapse: collapse;
    font-size: 13px;
  }
  th,
  td {
    text-align: left;
    padding: 10px 18px;
  }
  th {
    font-family: var(--font-mono);
    font-size: 10.5px;
    letter-spacing: 0.12em;
    text-transform: uppercase;
    color: var(--color-iron-400);
    font-weight: 500;
    border-bottom: 1px solid var(--color-iron-700);
  }
  tbody tr {
    border-bottom: 1px solid var(--color-iron-800);
  }
  .mono {
    color: var(--color-iron-200);
  }
  .r {
    text-align: right;
  }
  .small {
    font-size: 11px;
  }
  @media (max-width: 760px) {
    .two {
      grid-template-columns: 1fr;
    }
  }
</style>
