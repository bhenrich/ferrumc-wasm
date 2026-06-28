<script lang="ts">
  import { fmtInt } from '$lib/format';
  import { telemetry } from '$lib/telemetry.svelte';

  const snap = $derived(telemetry.snapshot);

  const kinds = [
    { key: 'allow', label: 'allow', color: 'var(--color-heat-ok)' },
    { key: 'replace', label: 'replace', color: 'var(--color-slag)' },
    { key: 'deny', label: 'deny', color: 'var(--color-heat-warn)' },
    { key: 'panic', label: 'panic', color: 'var(--color-heat-bad)' }
  ] as const;
</script>

<div class="legend">
  {#each kinds as k (k.key)}
    <span class="lg"><i style="background:{k.color}"></i>{k.label}</span>
  {/each}
</div>

{#if snap.plugin_decisions.length === 0}
  <section class="panel panel-pad stub">
    <div class="awaiting">awaiting telemetry</div>
    <p class="muted">
      Plugin decision counts arrive from the plugin host. Each plugin's
      <span class="ok">allow</span> / <span class="warn">deny</span> /
      <span class="slag">replace</span> / <span class="bad">panic</span> tally will stream here, and
      a <span class="bad">deny</span> or <span class="bad">panic</span> pulses ember the instant it fires.
    </p>
  </section>
{:else}
  <div class="grid">
    {#each snap.plugin_decisions as plug (plug.plugin_name)}
      {@const d = plug.decisions}
      <section class="panel panel-pad pcard" class:alarm={d.deny > 0 || d.panic > 0}>
        <div class="pname">{plug.plugin_name}</div>
        <div class="dgrid">
          {#each kinds as k (k.key)}
            <div class="d">
              <div class="dv metric tnum" style="color:{k.color}">{fmtInt(d[k.key])}</div>
              <div class="dl">{k.label}</div>
            </div>
          {/each}
        </div>
      </section>
    {/each}
  </div>
{/if}

<style>
  .legend {
    display: flex;
    gap: 18px;
    margin-bottom: 16px;
    font-size: 12px;
    color: var(--color-iron-300);
  }
  .lg {
    display: inline-flex;
    align-items: center;
    gap: 7px;
  }
  .lg i {
    width: 10px;
    height: 10px;
    border-radius: 2px;
    display: inline-block;
  }
  .stub {
    text-align: left;
  }
  .stub p {
    margin: 14px 0 0;
    max-width: 56ch;
    font-size: 13px;
    line-height: 1.7;
  }
  .ok {
    color: var(--color-heat-ok);
  }
  .warn {
    color: var(--color-heat-warn);
  }
  .bad {
    color: var(--color-heat-bad);
  }
  .slag {
    color: var(--color-slag);
  }
  .grid {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(240px, 1fr));
    gap: 16px;
  }
  .pname {
    font-family: var(--font-mono);
    font-size: 14px;
    color: var(--color-iron-100);
    margin-bottom: 16px;
  }
  .dgrid {
    display: grid;
    grid-template-columns: repeat(4, 1fr);
    gap: 12px;
  }
  .dv {
    font-size: 24px;
  }
  .dl {
    font-size: 10px;
    letter-spacing: 0.1em;
    text-transform: uppercase;
    color: var(--color-iron-400);
    margin-top: 6px;
  }
</style>
