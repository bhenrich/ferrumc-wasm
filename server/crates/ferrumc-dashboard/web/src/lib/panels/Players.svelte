<script lang="ts">
  import { fmt1, fmtBytes, fmtInt, pid } from '$lib/format';
  import { telemetry } from '$lib/telemetry.svelte';

  const snap = $derived(telemetry.snapshot);
  // Per-player network counters are fed by a separate lane; flag the column group
  // as awaiting telemetry when nothing has reported yet.
  const netLive = $derived(
    snap.players.some((p) => p.network_in_bytes > 0 || p.network_out_bytes > 0)
  );
</script>

<section class="panel">
  <div class="panel-pad head">
    <div>
      <div class="eyebrow">connected players</div>
      <div class="count metric tnum">{fmtInt(snap.players_online)}</div>
    </div>
    {#if !netLive}<span class="awaiting">per-player network · awaiting telemetry</span>{/if}
  </div>

  {#if snap.players.length === 0}
    <div class="panel-pad empty muted">No players connected. The roster fills as clients join.</div>
  {:else}
    <div class="scroll">
      <table>
        <thead>
          <tr>
            <th>Name</th>
            <th>Position</th>
            <th>Chunk</th>
            <th>Mode</th>
            <th class="r">Out queue</th>
            <th class="r">Net in</th>
            <th class="r">Net out</th>
          </tr>
        </thead>
        <tbody>
          {#each snap.players as p (pid(p.player_id) + p.name)}
            <tr>
              <td class="name">{p.name}</td>
              <td class="tnum mono">{fmt1(p.position.x)} {fmt1(p.position.y)} {fmt1(p.position.z)}</td>
              <td class="tnum mono">{p.chunk.x}, {p.chunk.z}</td>
              <td><span class="chip mode-{p.gamemode}">{p.gamemode || '—'}</span></td>
              <td class="r tnum">{fmtInt(p.outbound_queue_len)}</td>
              <td class="r tnum" class:dim={!netLive}>{netLive ? fmtBytes(p.network_in_bytes) : '—'}</td>
              <td class="r tnum" class:dim={!netLive}>{netLive ? fmtBytes(p.network_out_bytes) : '—'}</td>
            </tr>
          {/each}
        </tbody>
      </table>
    </div>
  {/if}
</section>

<style>
  .head {
    display: flex;
    align-items: flex-start;
    justify-content: space-between;
    gap: 12px;
    border-bottom: 1px solid var(--color-iron-700);
  }
  .count {
    font-size: 26px;
    margin-top: 8px;
  }
  .scroll {
    overflow-x: auto;
  }
  table {
    width: 100%;
    border-collapse: collapse;
    font-size: 13px;
  }
  th,
  td {
    text-align: left;
    padding: 11px 18px;
    white-space: nowrap;
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
  tbody tr:hover {
    background: rgba(232, 115, 31, 0.045);
  }
  .r {
    text-align: right;
  }
  .name {
    color: var(--color-iron-100);
    font-weight: 500;
  }
  .mono {
    color: var(--color-iron-300);
  }
  .dim {
    color: var(--color-iron-600);
  }
  .empty {
    padding: 40px 20px;
  }
  .mode-creative {
    color: var(--color-ember);
    border-color: rgba(232, 115, 31, 0.4);
  }
  .mode-survival {
    color: var(--color-heat-ok);
    border-color: rgba(63, 185, 80, 0.35);
  }
  .mode-spectator {
    color: var(--color-slag);
    border-color: rgba(74, 168, 192, 0.35);
  }
</style>
