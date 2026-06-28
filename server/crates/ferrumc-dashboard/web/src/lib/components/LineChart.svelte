<script lang="ts">
  import { onDestroy, onMount } from 'svelte';
  import uPlot from 'uplot';

  export interface SeriesDef {
    label: string;
    stroke: string;
    fill?: string;
    width?: number;
    dash?: number[];
  }

  let {
    rev,
    getData,
    series,
    height = 180,
    glow = 0,
    yMin = 0,
    yMax
  }: {
    /** Reactive trigger: bump to push fresh data into the chart. */
    rev: number;
    /** Returns aligned data: [xs, ...ys] matching `series` order. */
    getData: () => uPlot.AlignedData;
    series: SeriesDef[];
    height?: number;
    glow?: number;
    yMin?: number | null;
    yMax?: number | null;
  } = $props();

  let host: HTMLDivElement;
  let chart: uPlot | null = null;

  const GRID = 'rgba(53, 59, 69, 0.5)';
  const AXIS = '#6b727e';

  function build(width: number): uPlot.Options {
    return {
      width,
      height,
      padding: [10, 10, 0, 0],
      cursor: { show: true, y: false, points: { size: 6 } },
      legend: { show: false },
      scales: {
        x: { time: false },
        y: {
          range: (_u, dataMin, dataMax) => {
            const lo = yMin ?? dataMin;
            const hi = yMax ?? dataMax;
            return [lo, hi === lo ? lo + 1 : hi];
          }
        }
      },
      axes: [
        {
          stroke: AXIS,
          grid: { stroke: GRID, width: 1 },
          ticks: { stroke: GRID, width: 1 },
          font: '11px "JetBrains Mono", monospace',
          size: 28,
          values: (_u, splits) => splits.map((s) => `${Math.round(s)}s`)
        },
        {
          stroke: AXIS,
          grid: { stroke: GRID, width: 1 },
          ticks: { stroke: GRID, width: 1 },
          font: '11px "JetBrains Mono", monospace',
          size: 40
        }
      ],
      series: [
        {},
        ...series.map((s) => ({
          label: s.label,
          stroke: s.stroke,
          width: s.width ?? 1.6,
          fill: s.fill,
          dash: s.dash,
          points: { show: false }
        }))
      ]
    };
  }

  onMount(() => {
    const width = host.clientWidth || 600;
    chart = new uPlot(build(width), getData(), host);
    const ro = new ResizeObserver(() => {
      if (chart && host.clientWidth) chart.setSize({ width: host.clientWidth, height });
    });
    ro.observe(host);
    return () => ro.disconnect();
  });

  onDestroy(() => chart?.destroy());

  // Push new samples whenever the store revision advances.
  $effect(() => {
    rev;
    if (chart) chart.setData(getData());
  });
</script>

<div
  class="chart"
  bind:this={host}
  style="height:{height}px; filter: drop-shadow(0 0 {6 * glow}px rgba(232,115,31,{0.5 * glow}))"
></div>

<style>
  .chart {
    width: 100%;
    transition: filter 0.4s ease;
  }
  .chart :global(.u-axis) {
    color: var(--color-iron-400);
  }
</style>
