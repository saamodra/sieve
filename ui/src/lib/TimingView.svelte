<script lang="ts">
  import { formatDuration } from './format';
  import type { FlowDetail } from './types';

  let { detail }: { detail: FlowDetail } = $props();

  const wait = $derived(detail.timing.wait_ms ?? 0);
  const total = $derived(detail.timing.total_ms ?? wait);
  const receive = $derived(Math.max(0, total - wait));
  const scale = $derived(total > 0 ? 100 / total : 0);
</script>

<div class="wrap">
  <table>
    <tbody>
      <tr><th>Started</th><td class="mono">{detail.summary.started_at}</td></tr>
      <tr><th>Client</th><td class="mono">{detail.summary.client ?? 'unknown'}</td></tr>
      <tr><th>Wait (time to first byte)</th><td class="mono">{formatDuration(detail.timing.wait_ms)}</td></tr>
      <tr><th>Receive</th><td class="mono">{detail.timing.total_ms === null ? '' : formatDuration(receive)}</td></tr>
      <tr><th>Total</th><td class="mono">{formatDuration(detail.timing.total_ms)}</td></tr>
    </tbody>
  </table>
  {#if detail.timing.total_ms !== null}
    <div class="bar" aria-hidden="true">
      <span class="w" style:width="{wait * scale}%"></span><span class="r" style:width="{receive * scale}%"></span>
    </div>
    <div class="legend"><i class="w"></i> wait <i class="r"></i> receive</div>
  {/if}
</div>

<style>
  .wrap {
    padding: 10px;
  }
  table {
    border-collapse: collapse;
  }
  th {
    text-align: left;
    font-weight: 500;
    color: var(--fg-dim);
    padding: 2px 24px 2px 0;
  }
  .bar {
    display: flex;
    height: 10px;
    margin-top: 14px;
    background: var(--bg-3);
    border-radius: 3px;
    overflow: hidden;
  }
  .w {
    background: var(--warn);
  }
  .r {
    background: var(--accent);
  }
  .legend {
    margin-top: 6px;
    color: var(--fg-dim);
  }
  .legend i {
    display: inline-block;
    width: 8px;
    height: 8px;
    margin: 0 4px 0 8px;
    border-radius: 2px;
  }
  .legend i:first-child {
    margin-left: 0;
  }
</style>
