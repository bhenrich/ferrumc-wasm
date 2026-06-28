<script lang="ts">
  let {
    values,
    stroke = 'var(--color-ember)',
    fill = 'rgba(232,115,31,0.14)',
    height = 56,
    area = true
  }: {
    values: number[];
    stroke?: string;
    fill?: string;
    height?: number;
    area?: boolean;
  } = $props();

  const W = 240;

  const geom = $derived.by(() => {
    const v = values.length ? values : [0];
    const max = Math.max(1, ...v);
    const n = v.length;
    const dx = n > 1 ? W / (n - 1) : W;
    const pts = v.map((value, i) => {
      const x = i * dx;
      const y = height - (value / max) * (height - 6) - 3;
      return [x, y] as const;
    });
    const line = pts.map(([x, y], i) => `${i ? 'L' : 'M'}${x.toFixed(1)},${y.toFixed(1)}`).join(' ');
    const last = pts[pts.length - 1];
    const areaPath = `${line} L${W},${height} L0,${height} Z`;
    return { line, areaPath, last, max };
  });
</script>

<svg viewBox="0 0 {W} {height}" preserveAspectRatio="none" class="spark" style="height:{height}px">
  {#if area}
    <path d={geom.areaPath} fill={fill} stroke="none" />
  {/if}
  <path d={geom.line} fill="none" stroke={stroke} stroke-width="1.8" vector-effect="non-scaling-stroke" />
  {#if geom.last}
    <circle cx={geom.last[0]} cy={geom.last[1]} r="2.6" fill={stroke} />
  {/if}
</svg>

<style>
  .spark {
    width: 100%;
    display: block;
  }
</style>
