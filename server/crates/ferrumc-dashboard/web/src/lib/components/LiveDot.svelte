<script lang="ts">
  import type { ConnState } from '$lib/types';

  let { state }: { state: ConnState } = $props();

  const meta: Record<ConnState, { label: string; cls: string }> = {
    connecting: { label: 'connecting', cls: 'c-wait' },
    live: { label: 'live', cls: 'c-live' },
    reconnecting: { label: 'reconnecting', cls: 'c-wait' },
    polling: { label: 'polling', cls: 'c-poll' }
  };
</script>

<span class="live {meta[state].cls}" title="telemetry transport: {state}">
  <span class="dot"></span>
  <span class="label">{meta[state].label}</span>
</span>

<style>
  .live {
    display: inline-flex;
    align-items: center;
    gap: 7px;
    font-size: 11px;
    letter-spacing: 0.08em;
    text-transform: uppercase;
    color: var(--color-iron-400);
  }
  .dot {
    width: 8px;
    height: 8px;
    border-radius: 999px;
    background: currentColor;
    box-shadow: 0 0 8px currentColor;
  }
  .c-live {
    color: var(--color-heat-ok);
  }
  .c-live .dot {
    animation: pulse 1.8s ease-in-out infinite;
  }
  .c-wait {
    color: var(--color-heat-warn);
  }
  .c-wait .dot {
    animation: pulse 1s ease-in-out infinite;
  }
  .c-poll {
    color: var(--color-slag);
  }
  .label {
    color: inherit;
  }
  @keyframes pulse {
    0%,
    100% {
      opacity: 1;
    }
    50% {
      opacity: 0.3;
    }
  }
</style>
