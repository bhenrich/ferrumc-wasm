<script lang="ts">
  import { onDestroy, onMount, type Component } from 'svelte';
  import Header from '$lib/components/Header.svelte';
  import Sidebar from '$lib/components/Sidebar.svelte';
  import { NAV } from '$lib/nav';
  import { telemetry } from '$lib/telemetry.svelte';
  import Overview from '$lib/panels/Overview.svelte';
  import Players from '$lib/panels/Players.svelte';
  import World from '$lib/panels/World.svelte';
  import Packets from '$lib/panels/Packets.svelte';
  import Queues from '$lib/panels/Queues.svelte';
  import Persistence from '$lib/panels/Persistence.svelte';
  import Plugins from '$lib/panels/Plugins.svelte';

  const PANELS: Record<string, Component> = {
    overview: Overview,
    players: Players,
    world: World,
    packets: Packets,
    queues: Queues,
    persistence: Persistence,
    plugins: Plugins
  };

  let active = $state('overview');
  const title = $derived(NAV.find((n) => n.id === active)?.label ?? 'Overview');
  const ActivePanel = $derived(PANELS[active] ?? Overview);

  onMount(() => telemetry.start());
  onDestroy(() => telemetry.stop());
</script>

<div class="app">
  <Sidebar bind:active />
  <div class="main">
    <Header {title} />
    <main class="content">
      {#key active}
        <div class="fade">
          <ActivePanel />
        </div>
      {/key}
    </main>
  </div>
</div>

<style>
  .app {
    display: flex;
    min-height: 100vh;
  }
  .main {
    flex: 1;
    min-width: 0;
    display: flex;
    flex-direction: column;
  }
  .content {
    flex: 1;
    padding: 24px 26px 48px;
    max-width: 1280px;
    width: 100%;
  }
  .fade {
    animation: fade-in 0.28s ease;
  }
  @keyframes fade-in {
    from {
      opacity: 0;
      transform: translateY(4px);
    }
    to {
      opacity: 1;
      transform: none;
    }
  }
  @media (max-width: 760px) {
    .app {
      flex-direction: column;
    }
    .content {
      padding: 18px 16px 40px;
    }
  }
</style>
