<script lang="ts">
  let {
    loaded,
    dirty = 0,
    cap = 320
  }: {
    loaded: number;
    dirty?: number;
    cap?: number;
  } = $props();

  // We have residence *counts*, not per-chunk coordinates, so this renders one
  // cell per resident chunk and marks an even spread of the persist-dirty set —
  // a residence heatmap, not a literal coordinate map.
  const cells = $derived.by(() => {
    const total = Math.min(loaded, cap);
    const dirtyN = Math.min(dirty, total);
    const stride = dirtyN > 0 ? Math.max(1, Math.floor(total / dirtyN)) : 0;
    const out: boolean[] = [];
    for (let i = 0; i < total; i++) out.push(stride > 0 && i % stride === 0 && out.filter(Boolean).length < dirtyN);
    return out;
  });
  const cols = $derived(Math.max(8, Math.ceil(Math.sqrt(Math.min(loaded, cap)))));
  const overflow = $derived(Math.max(0, loaded - cap));
</script>

{#if loaded === 0}
  <div class="empty awaiting">no resident chunks</div>
{:else}
  <div class="grid" style="grid-template-columns: repeat({cols}, 1fr)">
    {#each cells as isDirty, i (i)}
      <span class="cell" class:dirty={isDirty}></span>
    {/each}
  </div>
  <div class="legend muted">
    <span><i class="sw loaded"></i> resident</span>
    <span><i class="sw d"></i> persist-dirty</span>
    {#if overflow > 0}<span>+{overflow} more</span>{/if}
  </div>
{/if}

<style>
  .grid {
    display: grid;
    gap: 3px;
    max-width: 420px;
  }
  .cell {
    aspect-ratio: 1;
    border-radius: 2px;
    background: var(--color-iron-700);
    border: 1px solid rgba(232, 115, 31, 0.04);
  }
  .cell.dirty {
    background: var(--color-ember);
    box-shadow: 0 0 6px rgba(232, 115, 31, 0.55);
    animation: dirty-pulse 2.2s ease-in-out infinite;
  }
  @keyframes dirty-pulse {
    0%,
    100% {
      opacity: 0.7;
    }
    50% {
      opacity: 1;
    }
  }
  .legend {
    display: flex;
    gap: 16px;
    margin-top: 12px;
    font-size: 11px;
  }
  .legend span {
    display: inline-flex;
    align-items: center;
    gap: 6px;
  }
  .sw {
    width: 10px;
    height: 10px;
    border-radius: 2px;
    display: inline-block;
  }
  .sw.loaded {
    background: var(--color-iron-700);
  }
  .sw.d {
    background: var(--color-ember);
  }
  .empty {
    padding: 24px 0;
  }
</style>
