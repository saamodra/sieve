<script lang="ts">
  import type { FlowDetail, Headers } from './types';

  let { detail }: { detail: FlowDetail } = $props();
</script>

{#snippet block(title: string, line: string, headers: Headers)}
  <section>
    <h3>{title}</h3>
    <div class="line mono">{line}</div>
    <table>
      <tbody>
        {#each headers as [k, v], i (i)}
          <tr><th class="mono">{k}</th><td class="mono">{v}</td></tr>
        {/each}
      </tbody>
    </table>
  </section>
{/snippet}

{@render block(
  'Request',
  `${detail.request.method} ${detail.request.url} ${detail.request.http_version}`,
  detail.request.headers,
)}
{#if detail.response}
  {@render block('Response', `${detail.response.http_version} ${detail.response.status}`, detail.response.headers)}
{:else if detail.error}
  <section>
    <h3>Error</h3>
    <div class="line s-err mono">{detail.error}</div>
  </section>
{/if}

<style>
  section {
    padding: 8px 10px;
    border-bottom: 1px solid var(--line);
  }
  h3 {
    margin: 0 0 4px;
    font-size: 11px;
    text-transform: uppercase;
    letter-spacing: 0.06em;
    color: var(--fg-dim);
  }
  .line {
    margin-bottom: 6px;
    word-break: break-all;
  }
  table {
    border-collapse: collapse;
    width: 100%;
  }
  th,
  td {
    text-align: left;
    vertical-align: top;
    padding: 1px 0;
    word-break: break-all;
  }
  th {
    width: 38%;
    padding-right: 12px;
    font-weight: 500;
    color: var(--accent);
  }
</style>
