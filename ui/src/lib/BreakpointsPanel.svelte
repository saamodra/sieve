<script lang="ts">
  import HeldEditor from './HeldEditor.svelte';
  import type { HeldRequest } from './types';

  let { held, onclose }: { held: HeldRequest[]; onclose: () => void } = $props();
</script>

<aside class="panel" aria-label="Held requests">
  <header>
    <strong>Breakpoints</strong>
    <span class="dim">{held.length} held</span>
    <span class="spacer"></span>
    <button onclick={onclose}>Hide</button>
  </header>
  <div class="list">
    {#each held as req (req.id)}
      <HeldEditor {req} />
    {:else}
      <p class="dim empty">No held requests. Add a rule with Breakpoint enabled.</p>
    {/each}
  </div>
</aside>

<style>
  .panel {
    position: fixed;
    right: 0;
    top: 0;
    bottom: 0;
    width: min(520px, 100vw);
    background: var(--bg-2);
    border-left: 1px solid var(--line);
    display: flex;
    flex-direction: column;
    z-index: 10;
  }
  header {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 8px;
  }
  .spacer {
    flex: 1;
  }
  .dim {
    color: var(--fg-dim);
  }
  .list {
    overflow: auto;
    flex: 1;
  }
  .empty {
    padding: 12px;
  }
</style>
