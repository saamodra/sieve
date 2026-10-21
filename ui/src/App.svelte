<script lang="ts">
  import { onDestroy, onMount } from 'svelte';
  import { Capture } from './lib/capture.svelte';
  import FlowTable from './lib/FlowTable.svelte';
  import DetailPanel from './lib/DetailPanel.svelte';
  import RulesPanel from './lib/RulesPanel.svelte';
  import BreakpointsPanel from './lib/BreakpointsPanel.svelte';

  const capture = new Capture();
  onMount(() => capture.start());
  onDestroy(() => capture.stop());

  let text = $state('');
  let method = $state('');
  let status = $state('');
  let client = $state('');
  let selectedId = $state<number | null>(null);
  let showRules = $state(false);
  let showHeld = $state(false);
  let seenHeld = 0;

  // Open the breakpoint panel when a request gets held.
  $effect(() => {
    const n = capture.held.length;
    if (n > seenHeld) {
      showHeld = true;
      showRules = false;
    }
    seenHeld = n;
  });
  let detailWidth = $state(Math.round(window.innerWidth * 0.45));

  const METHODS = ['GET', 'POST', 'PUT', 'PATCH', 'DELETE', 'OPTIONS', 'HEAD', 'CONNECT'];
  const STATUSES = [
    ['2xx', '2xx'],
    ['3xx', '3xx'],
    ['4xx', '4xx'],
    ['5xx', '5xx'],
    ['err', 'Failed'],
  ];

  function statusMatches(row: { status: number | null; error: string | null }, want: string) {
    if (want === 'err') return row.error !== null || row.status === null;
    return row.status !== null && Math.floor(row.status / 100) === Number(want[0]);
  }

  const rows = $derived.by(() => {
    capture.version;
    const terms = text.toLowerCase().split(/\s+/).filter(Boolean);
    const any = terms.length || method || status || client;
    // Copy: capture.rows is mutated in place, and an identical reference would not notify FlowTable.
    if (!any) return capture.rows.slice();
    return capture.rows.filter(
      (r) =>
        (!method || r.method === method) &&
        (!client || r.client === client) &&
        (!status || statusMatches(r, status)) &&
        terms.every((t) => (t[0] === '-' && t.length > 1 ? !r.search.includes(t.slice(1)) : r.search.includes(t))),
    );
  });

  const total = $derived.by(() => {
    capture.version;
    return capture.rows.length;
  });

  async function clear() {
    selectedId = null;
    await capture.clear();
  }

  function dragStart(e: PointerEvent) {
    const handle = e.currentTarget as HTMLElement;
    handle.setPointerCapture(e.pointerId);
    const move = (ev: PointerEvent) => {
      detailWidth = Math.min(Math.max(window.innerWidth - ev.clientX, 320), window.innerWidth - 360);
    };
    const up = () => {
      handle.removeEventListener('pointermove', move);
      handle.removeEventListener('pointerup', up);
    };
    handle.addEventListener('pointermove', move);
    handle.addEventListener('pointerup', up);
  }
</script>

<div class="app">
  <header>
    <span class="brand">sieve</span>
    <input
      class="text"
      type="search"
      placeholder="Filter: words match method, URL, status. Prefix - to exclude"
      bind:value={text}
      aria-label="Filter requests"
    />
    <select bind:value={method} aria-label="Method">
      <option value="">All methods</option>
      {#each METHODS as m (m)}<option value={m}>{m}</option>{/each}
    </select>
    <select bind:value={status} aria-label="Status">
      <option value="">All statuses</option>
      {#each STATUSES as [v, label] (v)}<option value={v}>{label}</option>{/each}
    </select>
    <select bind:value={client} aria-label="Source device">
      <option value="">All sources</option>
      {#each capture.clients as c (c)}<option value={c}>{c}</option>{/each}
    </select>
    <span class="spacer"></span>
    <span class="count">
      {rows.length === total ? total : `${rows.length} / ${total}`} requests
    </span>
    <button class:active={capture.paused} onclick={() => capture.setPaused(!capture.paused)}>
      {capture.paused ? `Resume${capture.pendingCount ? ` (${capture.pendingCount})` : ''}` : 'Pause'}
    </button>
    <button class:active={capture.held.length > 0} onclick={() => ((showHeld = !showHeld), (showRules = false))}>
      Breakpoints{capture.held.length ? ` (${capture.held.length})` : ''}
    </button>
    <button onclick={() => ((showRules = !showRules), (showHeld = false))}>Rules</button>
    <button onclick={clear}>Clear</button>
    <span class="dot" class:on={capture.connected} title={capture.connected ? 'Live' : 'Disconnected'}></span>
  </header>

  <main>
    <section class="table">
      <FlowTable {rows} {selectedId} onselect={(id) => (selectedId = id)} />
    </section>
    {#if selectedId !== null}
      <!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
      <div class="handle" role="separator" aria-orientation="vertical" onpointerdown={dragStart}></div>
      <section class="detail" style:width="{detailWidth}px">
        <DetailPanel
          id={selectedId}
          row={capture.rows.find((r) => r.id === selectedId) ?? null}
          version={capture.version}
          onclose={() => (selectedId = null)}
        />
      </section>
    {/if}
  </main>
  {#if showRules}<RulesPanel onclose={() => (showRules = false)} />{/if}
  {#if showHeld}<BreakpointsPanel held={capture.held} onclose={() => (showHeld = false)} />{/if}
</div>

<style>
  .app {
    display: flex;
    flex-direction: column;
    height: 100%;
  }
  header {
    display: flex;
    align-items: center;
    gap: 6px;
    padding: 6px 8px;
    background: var(--bg-2);
    border-bottom: 1px solid var(--line);
  }
  .brand {
    font-weight: 700;
    letter-spacing: 0.02em;
    color: var(--accent);
    margin-right: 4px;
  }
  .text {
    width: 340px;
    max-width: 40vw;
  }
  .spacer {
    flex: 1;
  }
  .count {
    color: var(--fg-dim);
    font-variant-numeric: tabular-nums;
    margin-right: 4px;
  }
  .dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: var(--bad);
  }
  .dot.on {
    background: var(--ok);
  }
  main {
    flex: 1;
    min-height: 0;
    display: flex;
  }
  .table {
    flex: 1;
    min-width: 0;
  }
  .detail {
    min-width: 0;
    border-left: 1px solid var(--line);
  }
  .handle {
    width: 5px;
    margin-left: -3px;
    cursor: col-resize;
    z-index: 2;
  }
  .handle:hover {
    background: var(--accent);
  }
</style>
