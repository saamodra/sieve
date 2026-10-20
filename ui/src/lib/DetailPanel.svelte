<script lang="ts">
  import { fetchCurl, fetchDetail, harUrl } from './api';
  import { copyText, formatSize, statusClass } from './format';
  import BodyView from './BodyView.svelte';
  import HeadersView from './HeadersView.svelte';
  import TimingView from './TimingView.svelte';
  import type { FlowDetail } from './types';
  import type { Row } from './capture.svelte';

  interface Props {
    id: number;
    row: Row | null;
    /** Changes whenever the live list changes, so an in-flight flow is refetched. */
    version: number;
    onclose: () => void;
  }
  let { id, row, version, onclose }: Props = $props();

  type Tab = 'headers' | 'request' | 'response' | 'timing';
  const TABS: [Tab, string][] = [
    ['headers', 'Headers'],
    ['request', 'Request body'],
    ['response', 'Response body'],
    ['timing', 'Timing'],
  ];

  let tab = $state<Tab>('headers');
  let detail = $state<FlowDetail | null>(null);
  let error = $state('');
  let toast = $state('');

  // Refetch when the selection changes or the selected flow itself changed.
  const revision = $derived(`${id}:${row?.status}:${row?.duration_ms}:${row?.error}:${row?.size}`);
  let loadedFor = '';

  $effect(() => {
    version;
    const rev = revision;
    if (rev === loadedFor) return;
    loadedFor = rev;
    const want = id;
    fetchDetail(want)
      .then((d) => {
        if (want === id) {
          detail = d;
          error = '';
        }
      })
      .catch((e: unknown) => {
        if (want === id) error = String(e);
      });
  });

  $effect(() => {
    id;
    detail = null;
    error = '';
  });

  async function copyCurl() {
    try {
      await copyText(await fetchCurl(id));
      flash('Copied curl');
    } catch (e) {
      flash(`Copy failed: ${e}`);
    }
  }

  function flash(msg: string) {
    toast = msg;
    setTimeout(() => (toast = ''), 1600);
  }
</script>

<div class="panel">
  <div class="top">
    <div class="title">
      {#if detail}
        <span class="mono method">{detail.request.method}</span>
        <span class="mono {statusClass(detail.response?.status ?? null, detail.error)}">
          {detail.error ? 'ERR' : (detail.response?.status ?? '…')}
        </span>
        <span class="url mono" title={detail.request.url}>{detail.request.url}</span>
      {:else}
        <span class="dim">#{id}</span>
      {/if}
    </div>
    <div class="actions">
      <button onclick={copyCurl} disabled={!detail}>Copy as curl</button>
      <a class="btn" href={harUrl(id)} download={`sieve-${id}.har`}>HAR</a>
      <button onclick={onclose} aria-label="Close detail">✕</button>
    </div>
  </div>

  <div class="tabs" role="tablist">
    {#each TABS as [key, label] (key)}
      <button role="tab" aria-selected={tab === key} class:on={tab === key} onclick={() => (tab = key)}>
        {label}
        {#if detail && key === 'request' && detail.request.body.size > 0}
          <small>{formatSize(detail.request.body.size)}</small>
        {:else if detail && key === 'response' && detail.response && detail.response.body.size > 0}
          <small>{formatSize(detail.response.body.size)}</small>
        {/if}
      </button>
    {/each}
  </div>

  <div class="content">
    {#if error}
      <p class="msg s-err">{error}</p>
    {:else if !detail}
      <p class="msg dim">Loading…</p>
    {:else if tab === 'headers'}
      <HeadersView {detail} />
    {:else if tab === 'request'}
      <BodyView body={detail.request.body} />
    {:else if tab === 'response'}
      {#if detail.response}
        <BodyView body={detail.response.body} />
      {:else}
        <p class="msg dim">{detail.error ?? 'No response yet.'}</p>
      {/if}
    {:else}
      <TimingView {detail} />
    {/if}
  </div>
  {#if toast}<div class="toast">{toast}</div>{/if}
</div>

<style>
  .panel {
    display: flex;
    flex-direction: column;
    height: 100%;
    position: relative;
  }
  .top {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 6px 8px;
    background: var(--bg-2);
    border-bottom: 1px solid var(--line);
  }
  .title {
    flex: 1;
    min-width: 0;
    display: flex;
    gap: 8px;
    align-items: baseline;
  }
  .url {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    color: var(--fg-dim);
  }
  .method {
    font-weight: 700;
  }
  .actions {
    display: flex;
    gap: 6px;
  }
  .btn {
    background: var(--bg-3);
    border: 1px solid var(--line);
    border-radius: 4px;
    padding: 3px 9px;
    color: inherit;
    text-decoration: none;
  }
  .btn:hover {
    border-color: var(--accent);
  }
  .tabs {
    display: flex;
    border-bottom: 1px solid var(--line);
    background: var(--bg-2);
  }
  .tabs button {
    border: 0;
    border-bottom: 2px solid transparent;
    border-radius: 0;
    background: none;
    padding: 6px 12px;
    color: var(--fg-dim);
  }
  .tabs button.on {
    color: var(--fg);
    border-bottom-color: var(--accent);
  }
  .tabs small {
    margin-left: 4px;
    color: var(--fg-dim);
  }
  .content {
    flex: 1;
    min-height: 0;
    overflow: auto;
  }
  .msg {
    padding: 12px;
    margin: 0;
  }
  .dim {
    color: var(--fg-dim);
  }
  .toast {
    position: absolute;
    right: 10px;
    bottom: 10px;
    background: var(--bg-3);
    border: 1px solid var(--line);
    border-radius: 4px;
    padding: 5px 10px;
  }
</style>
