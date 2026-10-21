<script lang="ts">
  import { formatDuration, formatSize, formatTime, statusClass } from './format';
  import type { Row } from './capture.svelte';

  interface Props {
    rows: Row[];
    selectedId: number | null;
    onselect: (id: number) => void;
  }
  let { rows, selectedId, onselect }: Props = $props();

  const ROW_H = 22;
  const OVERSCAN = 12;

  let viewport = $state<HTMLDivElement>();
  let scrollTop = $state(0);
  let height = $state(600);
  // Follow the newest row until the user scrolls up.
  let follow = $state(true);

  const start = $derived(Math.max(0, Math.floor(scrollTop / ROW_H) - OVERSCAN));
  const end = $derived(Math.min(rows.length, Math.ceil((scrollTop + height) / ROW_H) + OVERSCAN));
  const visible = $derived(rows.slice(start, end));

  $effect(() => {
    rows.length;
    if (follow && viewport) viewport.scrollTop = viewport.scrollHeight;
  });

  function onscroll() {
    if (!viewport) return;
    scrollTop = viewport.scrollTop;
    follow = viewport.scrollHeight - viewport.scrollTop - viewport.clientHeight < ROW_H * 2;
  }

  function onkeydown(e: KeyboardEvent) {
    if (e.key !== 'ArrowDown' && e.key !== 'ArrowUp') return;
    e.preventDefault();
    if (rows.length === 0 || !viewport) return;
    const cur = rows.findIndex((r) => r.id === selectedId);
    const next = Math.min(rows.length - 1, Math.max(0, cur + (e.key === 'ArrowDown' ? 1 : -1)));
    const row = rows[next];
    if (!row) return;
    onselect(row.id);
    const top = next * ROW_H;
    if (top < viewport.scrollTop + ROW_H) viewport.scrollTop = top - ROW_H;
    else if (top + ROW_H > viewport.scrollTop + viewport.clientHeight - ROW_H)
      viewport.scrollTop = top + 2 * ROW_H - viewport.clientHeight;
  }
</script>

<div
  class="viewport"
  bind:this={viewport}
  bind:clientHeight={height}
  {onscroll}
  {onkeydown}
  role="grid"
  tabindex="0"
  aria-label="Requests"
  aria-rowcount={rows.length}
>
  <div class="head cols" role="row">
    <span>Time</span><span>Method</span><span>Host</span><span>Path</span>
    <span class="r">Status</span><span class="r">Size</span><span class="r">Duration</span>
  </div>
  {#if rows.length === 0}
    <div class="empty">Waiting for traffic…</div>
  {/if}
  <div class="body" style:height="{rows.length * ROW_H}px">
    <div class="window" style:transform="translateY({start * ROW_H}px)">
      {#each visible as r (r.id)}
        <!-- svelte-ignore a11y_click_events_have_key_events -->
        <div
          class="row cols"
          class:sel={r.id === selectedId}
          role="row"
          tabindex="-1"
          aria-selected={r.id === selectedId}
          onclick={() => onselect(r.id)}
        >
          <span class="dim mono">{formatTime(r.ts)}</span>
          <span class="method mono">{r.method}</span>
          <span class="ell" title={r.host}>{r.host}</span>
          <span class="ell dim" title={r.path}>{r.path}</span>
          <span class="r mono {statusClass(r.status, r.error)}">
            {r.error ? 'ERR' : (r.status ?? '…')}
          </span>
          <span class="r dim">{formatSize(r.size)}</span>
          <span class="r dim">{formatDuration(r.duration_ms)}</span>
        </div>
      {/each}
    </div>
  </div>
</div>

<style>
  .viewport {
    height: 100%;
    overflow: auto;
    position: relative;
    outline: none;
  }
  .cols {
    display: grid;
    grid-template-columns: 96px 66px minmax(120px, 220px) minmax(120px, 1fr) 56px 76px 76px;
    column-gap: 10px;
    padding: 0 10px;
    align-items: center;
    min-width: 760px;
  }
  .head {
    position: sticky;
    top: 0;
    z-index: 1;
    height: 24px;
    background: var(--bg-2);
    border-bottom: 1px solid var(--line);
    color: var(--fg-dim);
    font-weight: 600;
  }
  .row {
    height: 22px;
    cursor: default;
    border-bottom: 1px solid color-mix(in srgb, var(--line) 40%, transparent);
  }
  .row:hover {
    background: var(--bg-2);
  }
  .row.sel {
    background: var(--sel);
  }
  .window {
    will-change: transform;
  }
  .ell {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .r {
    text-align: right;
    font-variant-numeric: tabular-nums;
  }
  .dim {
    color: var(--fg-dim);
  }
  .method {
    font-weight: 600;
  }
  .empty {
    position: absolute;
    inset: 40px 0 0;
    display: grid;
    place-items: center;
    color: var(--fg-dim);
    pointer-events: none;
  }
</style>
