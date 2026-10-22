<script lang="ts">
  import { copyText, decodeBase64, formatSize, hexDump, prettyJson } from './format';
  import JsonTree from './JsonTree.svelte';
  import type { BodyDetail } from './types';

  let { body }: { body: BodyDetail } = $props();

  type Mode = 'pretty' | 'raw' | 'image' | 'hex';

  const isImage = $derived((body.content_type ?? '').toLowerCase().startsWith('image/'));
  const pretty = $derived(body.text !== null ? prettyJson(body.text) : null);
  const parsed = $derived.by(() => {
    if (pretty === null || body.text === null) return null;
    try {
      return { value: JSON.parse(body.text) as unknown };
    } catch {
      return null;
    }
  });
  let signal = $state({ n: 0, all: true });
  const fold = (all: boolean) => (signal = { n: signal.n + 1, all });
  const modes = $derived.by(() => {
    const m: [Mode, string][] = [];
    if (isImage) m.push(['image', 'Image']);
    if (pretty !== null) m.push(['pretty', 'Pretty']);
    if (body.text !== null) m.push(['raw', 'Raw']);
    m.push(['hex', 'Hex']);
    return m;
  });

  let copied = $state(false);
  async function copyBody() {
    try {
      await copyText(pretty ?? body.text ?? '');
      copied = true;
      setTimeout(() => (copied = false), 1200);
    } catch {
      copied = false;
    }
  }

  let chosen = $state<Mode | null>(null);
  const mode = $derived(modes.find(([k]) => k === chosen) ? chosen! : modes[0]![0]);

  const hex = $derived(mode === 'hex' ? hexDump(decodeBase64(body.base64)) : '');
  const imageSrc = $derived(`data:${body.content_type ?? 'image/*'};base64,${body.base64}`);
</script>

{#if body.size === 0 && body.captured === 0}
  <p class="empty">No body.</p>
{:else}
  <div class="bar">
    <div class="modes">
      {#each modes as [k, label] (k)}
        <button class:on={mode === k} onclick={() => (chosen = k)}>{label}</button>
      {/each}
    </div>
    {#if mode === 'pretty' && parsed}
      <div class="modes">
        <button onclick={() => fold(true)}>Expand all</button>
        <button onclick={() => fold(false)}>Collapse all</button>
      </div>
    {/if}
    {#if body.text !== null}
      <div class="modes"><button onclick={copyBody}>{copied ? 'Copied' : 'Copy'}</button></div>
    {/if}
    <span class="meta">
      {body.content_type ?? 'unknown type'} · {formatSize(body.size)}
      {#if body.content_encoding}· {body.content_encoding}{/if}
      {#if body.truncated}· <b>truncated to {formatSize(body.captured)}</b>{/if}
    </span>
  </div>
  {#if mode === 'image'}
    <div class="img"><img src={imageSrc} alt="Response body preview" /></div>
  {:else if mode === 'pretty' && parsed}
    <div class="tree"><JsonTree value={parsed.value} {signal} /></div>
  {:else if mode === 'pretty'}
    <pre>{pretty}</pre>
  {:else if mode === 'raw'}
    <pre>{body.text}</pre>
  {:else}
    <pre>{hex}</pre>
  {/if}
{/if}

<style>
  .bar {
    position: sticky;
    top: 0;
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 5px 10px;
    background: var(--bg);
    border-bottom: 1px solid var(--line);
  }
  .modes {
    display: flex;
    gap: 4px;
  }
  .modes button {
    padding: 1px 8px;
  }
  .modes button.on {
    background: var(--accent-low);
    border-color: var(--accent);
    color: var(--accent);
  }
  .meta {
    margin-left: auto;
    color: var(--fg-dim);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  pre {
    margin: 0;
    padding: 8px 10px;
    font: 12px/1.45 var(--mono);
    white-space: pre-wrap;
    word-break: break-all;
  }
  .tree {
    padding: 8px 10px 8px calc(10px + 1.2em);
  }
  .img {
    padding: 12px;
  }
  img {
    max-width: 100%;
    image-rendering: auto;
    background: repeating-conic-gradient(var(--bg-3) 0 25%, var(--bg-2) 0 50%) 0 0 / 14px 14px;
    border: 1px solid var(--line);
    min-width: 8px;
    min-height: 8px;
  }
  .empty {
    padding: 12px;
    margin: 0;
    color: var(--fg-dim);
  }
</style>
