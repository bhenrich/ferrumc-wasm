<script lang="ts">
  import { NAV } from '$lib/nav';

  let { active = $bindable() }: { active: string } = $props();
</script>

<nav class="rail" aria-label="console sections">
  <div class="mark">
    <div class="word">Ferrum<span class="c">C</span></div>
    <div class="tag">for Minecraft: Java Edition</div>
  </div>
  <ul>
    {#each NAV as item (item.id)}
      <li>
        <button
          class="navbtn"
          class:active={active === item.id}
          aria-current={active === item.id ? 'page' : undefined}
          onclick={() => (active = item.id)}
        >
          <span class="glyph" aria-hidden="true">{item.glyph}</span>
          <span class="label">{item.label}</span>
        </button>
      </li>
    {/each}
  </ul>
  <div class="foot">
    <span class="dot"></span>read-only · localhost
  </div>
</nav>

<style>
  .rail {
    width: 224px;
    flex: 0 0 224px;
    background: linear-gradient(180deg, rgba(19, 21, 25, 0.92), rgba(11, 12, 14, 0.92));
    border-right: 1px solid var(--color-iron-700);
    padding: 22px 14px 16px;
    display: flex;
    flex-direction: column;
    gap: 22px;
    position: sticky;
    top: 0;
    height: 100vh;
  }
  .mark {
    padding: 0 8px;
  }
  .word {
    font-family: var(--font-display);
    font-weight: 700;
    font-size: 23px;
    letter-spacing: -0.01em;
    color: var(--color-iron-100);
  }
  .word .c {
    color: var(--color-ember);
    text-shadow: 0 0 16px rgba(232, 115, 31, 0.55);
  }
  .tag {
    font-size: 10px;
    letter-spacing: 0.06em;
    color: var(--color-iron-400);
    margin-top: 4px;
  }
  ul {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 3px;
  }
  .navbtn {
    width: 100%;
    display: flex;
    align-items: center;
    gap: 12px;
    padding: 10px 12px;
    background: transparent;
    border: 1px solid transparent;
    border-radius: 9px;
    color: var(--color-iron-300);
    font-family: var(--font-mono);
    font-size: 13px;
    cursor: pointer;
    text-align: left;
    transition:
      background 0.15s ease,
      color 0.15s ease;
  }
  .navbtn:hover {
    background: rgba(38, 43, 51, 0.5);
    color: var(--color-iron-100);
  }
  .navbtn.active {
    background: rgba(232, 115, 31, 0.1);
    border-color: rgba(232, 115, 31, 0.32);
    color: var(--color-iron-100);
  }
  .glyph {
    width: 18px;
    text-align: center;
    color: var(--color-iron-500);
    font-size: 14px;
  }
  .navbtn.active .glyph {
    color: var(--color-ember);
  }
  .foot {
    margin-top: auto;
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 0 8px;
    font-size: 10.5px;
    color: var(--color-iron-500);
  }
  .foot .dot {
    width: 6px;
    height: 6px;
    border-radius: 999px;
    background: var(--color-iron-600);
  }
  @media (max-width: 760px) {
    .rail {
      width: 100%;
      flex: none;
      height: auto;
      position: static;
      flex-direction: column;
      gap: 14px;
    }
    ul {
      flex-direction: row;
      flex-wrap: wrap;
    }
    .navbtn {
      width: auto;
    }
    .foot {
      display: none;
    }
  }
</style>
