<script lang="ts">
  import { untrack } from 'svelte';
  import JsonTree from './JsonTree.svelte';

  type Props = {
    value: unknown;
    name?: string | null;
    depth?: number;
    // Bumps on "Expand all" / "Collapse all"; `all` says which way.
    signal?: { n: number; all: boolean } | null;
    last?: boolean;
  };

  let { value, name = null, depth = 0, signal = null, last = true }: Props = $props();

  const isArr = $derived(Array.isArray(value));
  const container = $derived(typeof value === 'object' && value !== null);
  const entries = $derived(
    container ? (isArr ? (value as unknown[]).map((v, i) => [i, v]) : Object.entries(value as object)) : [],
  );

  let override = $state<boolean | null>(null);
  const open = $derived(override ?? true);

  // Start at the current count so a freshly mounted node ignores an old signal.
  let seen = $state.raw(untrack(() => signal?.n ?? 0));
  $effect(() => {
    if (signal && signal.n !== seen) {
      seen = signal.n;
      override = signal.all;
    }
  });

  const count = $derived(
    `${entries.length} ${isArr ? (entries.length === 1 ? 'item' : 'items') : entries.length === 1 ? 'key' : 'keys'}`,
  );
  const comma = $derived(last ? '' : ',');
</script>

<div class="node" style:padding-left={depth ? '1.2em' : '0'}>
  {#if container && entries.length > 0}
    <div class="tog">
      <button class="arrow" aria-expanded={open} aria-label={open ? 'Collapse' : 'Expand'} onclick={() => (override = !open)}>
        {open ? '▾' : '▸'}
      </button>
      {#if name !== null}<span class="key">{name}</span>: {/if}
      {isArr ? '[' : '{'}{#if !open}<!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_static_element_interactions --><span class="dim peek" onclick={() => (override = true)}>…{isArr ? ']' : '}'} {count}</span>{comma}{/if}
    </div>
    {#if open}
      {#each entries as [k, v], i (k)}
        <JsonTree value={v} name={isArr ? null : String(k)} depth={depth + 1} {signal} last={i === entries.length - 1} />
      {/each}
      <div class="close">{isArr ? ']' : '}'}{comma}</div>
    {/if}
  {:else}
    <span class="leaf">
      {#if name !== null}<span class="key">{name}</span>: {/if}
      {#if container}
        {isArr ? '[]' : '{}'}
      {:else if typeof value === 'string'}
        <span class="str">{JSON.stringify(value)}</span>
      {:else if value === null}
        <span class="null">null</span>
      {:else if typeof value === 'boolean'}
        <span class="bool">{value}</span>
      {:else}
        <span class="num">{value}</span>
      {/if}{comma}
    </span>
  {/if}
</div>

<style>
  .node {
    font: 12px/1.45 var(--mono);
    user-select: text;
  }
  .arrow {
    display: inline-block;
    width: 1.2em;
    margin-left: -1.2em;
    padding: 0;
    border: 0;
    background: none;
    color: var(--fg-dim);
    font: inherit;
    text-align: left;
    cursor: pointer;
    user-select: none;
  }
  .arrow:focus-visible {
    outline: 1px solid var(--accent);
  }
  .peek {
    cursor: pointer;
  }
  .leaf {
    white-space: pre-wrap;
    word-break: break-all;
  }
  .key {
    color: var(--info);
  }
  .str {
    color: var(--ok);
  }
  .num {
    color: var(--warn);
  }
  .bool {
    color: var(--bad);
  }
  .null,
  .dim {
    color: var(--fg-dim);
  }
</style>
