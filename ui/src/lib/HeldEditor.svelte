<script lang="ts">
  import { abortHeld, continueHeld } from './api';
  import { headersToText, textToHeaders } from './headerText';
  import type { HeldRequest } from './types';

  let { req }: { req: HeldRequest } = $props();

  // Seeded once, so edits survive other events re-rendering the list.
  // svelte-ignore state_referenced_locally
  let method = $state(req.method);
  // svelte-ignore state_referenced_locally
  let url = $state(req.url);
  // svelte-ignore state_referenced_locally
  let headers = $state(headersToText(req.headers));
  // svelte-ignore state_referenced_locally
  let body = $state(req.body ?? '');
  const editable = $derived(req.body !== null);

  let busy = $state(false);
  let left = $state(0);

  $effect(() => {
    left = req.timeout_secs;
    if (!left) return;
    const t = setInterval(() => (left = Math.max(0, left - 1)), 1000);
    return () => clearInterval(t);
  });

  async function resume() {
    busy = true;
    await continueHeld(req.id, {
      method,
      url,
      headers: textToHeaders(headers),
      ...(editable && body !== (req.body ?? '') ? { body } : {}),
    });
  }

  async function abort() {
    busy = true;
    await abortHeld(req.id);
  }
</script>

<form class="held" onsubmit={(e) => (e.preventDefault(), resume())}>
  <div class="row">
    <input class="method" bind:value={method} aria-label="Method" />
    <input class="url" bind:value={url} aria-label="URL" />
  </div>
  <label>
    Headers
    <textarea rows="6" bind:value={headers} spellcheck="false"></textarea>
  </label>
  <label>
    Body {editable ? `(${req.body_size} bytes)` : `(binary, ${req.body_size} bytes: not editable)`}
    <textarea rows="6" bind:value={body} disabled={!editable} spellcheck="false"></textarea>
  </label>
  <div class="row">
    <button type="submit" class="active" disabled={busy}>Continue</button>
    <button type="button" onclick={abort} disabled={busy}>Abort</button>
    {#if req.timeout_secs}<span class="dim">continues unchanged in {left}s</span>{/if}
  </div>
</form>

<style>
  .held {
    display: flex;
    flex-direction: column;
    gap: 6px;
    padding: 8px;
    border-top: 1px solid var(--line);
  }
  .row {
    display: flex;
    gap: 6px;
    align-items: center;
  }
  .method {
    width: 80px;
  }
  .url {
    flex: 1;
    min-width: 0;
  }
  label {
    display: flex;
    flex-direction: column;
    gap: 2px;
    color: var(--fg-dim);
  }
  textarea {
    font-family: var(--mono);
    background: var(--bg);
    border: 1px solid var(--line);
    border-radius: 4px;
    color: var(--fg);
    padding: 4px 6px;
    resize: vertical;
  }
  .dim {
    color: var(--fg-dim);
  }
</style>
